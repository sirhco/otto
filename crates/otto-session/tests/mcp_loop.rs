//! Integration test proving MCP tools are first-class in the agent [`run_loop`].
//!
//! An in-process MCP `TestServer` (an `echo` tool) is served over one end of a
//! `tokio::io::duplex` in-memory pipe; an [`McpClient`] connects to the other
//! end via [`McpClient::connect_transport`] (the same seam otto-mcp's own
//! `in_process` test uses). Its namespaced tool (`testserver_echo`) is
//! registered into a [`ToolRegistry`] alongside the builtins, then a
//! [`ScriptedRoute`] drives a two-turn loop: turn 1 calls the MCP tool, turn 2
//! wraps up. We assert the MCP tool executed, its result was persisted, and the
//! loop terminated — i.e. MCP tools flow through the loop exactly like builtins.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use otto_events::{FinishReason, LLMEvent};
use otto_llm::{LLMError, LLMRequest, Model, Route};
use otto_mcp::McpClient;
use otto_session::{RunConfig, run_loop};
use otto_storage::model::{
    Info, InfoBody, Part, PartKind, ToolState, User, UserModel, UserTime, new_message_id,
    new_part_id,
};
use otto_storage::{Session, SessionTokens, Store};
use otto_tools::{AllowAll, ToolRegistry};
use rmcp::ErrorData as McpError;
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParam, CallToolResult, Content, ListToolsResult, PaginatedRequestParam,
    ServerCapabilities, ServerInfo, Tool as McpToolDef,
};
use rmcp::service::{RequestContext, RoleServer, RunningService};
use serde_json::{Value, json};
use tokio::io::DuplexStream;
use tokio_util::sync::CancellationToken;

// -- in-process MCP echo server ----------------------------------------------

/// A minimal MCP server exposing a single `echo` tool. Replicated (kept tiny)
/// from otto-mcp's `in_process` test since that `TestServer` is private to its
/// test module.
#[derive(Clone)]
struct TestServer;

impl TestServer {
    fn echo_schema() -> Arc<serde_json::Map<String, Value>> {
        let schema = json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        });
        Arc::new(schema.as_object().cloned().unwrap())
    }
}

impl rmcp::ServerHandler for TestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let echo = McpToolDef::new("echo", "Echo the text back", TestServer::echo_schema());
        Ok(ListToolsResult::with_all_items(vec![echo]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        match request.name.as_ref() {
            "echo" => {
                let text = request
                    .arguments
                    .as_ref()
                    .and_then(|m| m.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            other => Err(McpError::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}

fn serve_test_server(
    server_io: DuplexStream,
) -> tokio::task::JoinHandle<RunningService<RoleServer, TestServer>> {
    tokio::spawn(async move { TestServer.serve(server_io).await.expect("server serve") })
}

// -- scripted route ----------------------------------------------------------

/// A [`Route`] that returns a canned event stream per `stream()` call, popping
/// one turn from a queue each time (mirrors the pattern in `run_loop.rs`).
struct ScriptedRoute {
    turns: Mutex<VecDeque<Vec<LLMEvent>>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedRoute {
    fn build(turns: Vec<Vec<LLMEvent>>) -> (Arc<dyn Route>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let route = Arc::new(ScriptedRoute {
            turns: Mutex::new(turns.into_iter().collect()),
            calls: calls.clone(),
        });
        (route, calls)
    }
}

impl Route for ScriptedRoute {
    fn id(&self) -> &str {
        "scripted"
    }

    fn stream(&self, _req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        stream::iter(events.into_iter().map(Ok)).boxed()
    }
}

// -- event helpers -----------------------------------------------------------

fn step_start() -> LLMEvent {
    LLMEvent::StepStart { index: 0 }
}
fn step_finish(reason: FinishReason) -> LLMEvent {
    LLMEvent::StepFinish {
        index: 0,
        reason,
        usage: None,
        provider_metadata: None,
    }
}
fn finish(reason: FinishReason) -> LLMEvent {
    LLMEvent::Finish {
        reason,
        usage: None,
        provider_metadata: None,
    }
}
fn tool_call(id: &str, name: &str, input: Value) -> LLMEvent {
    LLMEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        input,
        provider_executed: None,
        provider_metadata: None,
    }
}
fn text_events(id: &str, text: &str) -> Vec<LLMEvent> {
    vec![
        LLMEvent::TextStart {
            id: id.into(),
            provider_metadata: None,
        },
        LLMEvent::TextDelta {
            id: id.into(),
            text: text.into(),
            provider_metadata: None,
        },
        LLMEvent::TextEnd {
            id: id.into(),
            provider_metadata: None,
        },
    ]
}

// -- fixtures ----------------------------------------------------------------

const SES: &str = "ses_mcp";

async fn seed(text: &str) -> Store {
    let store = Store::open_in_memory().await.expect("open");
    store
        .create_session(&Session {
            id: SES.into(),
            project_id: "prj_1".into(),
            parent_id: None,
            directory: "/work".into(),
            title: "Test".into(),
            version: "1.0.0".into(),
            cost: 0.0,
            tokens: SessionTokens::default(),
            metadata: None,
            time_created: 1,
            time_updated: 1,
        })
        .await
        .expect("session");

    let user_id = new_message_id();
    let user = Info {
        id: user_id.clone(),
        session_id: SES.into(),
        body: InfoBody::User(User {
            time: UserTime { created: 1 },
            format: None,
            summary: None,
            agent: "build".into(),
            model: UserModel {
                provider_id: "anthropic".into(),
                model_id: "claude-3".into(),
                variant: None,
            },
            system: None,
            tools: None,
        }),
    };
    store.insert_message(&user).await.expect("user msg");
    store
        .insert_part(&Part {
            id: new_part_id(),
            session_id: SES.into(),
            message_id: user_id,
            kind: PartKind::Text {
                text: text.into(),
                synthetic: None,
                ignored: None,
                time: None,
                metadata: None,
            },
        })
        .await
        .expect("user text part");

    store
}

fn config(store: Store, route: Arc<dyn Route>, tools: ToolRegistry) -> RunConfig {
    RunConfig {
        store,
        route,
        tools: Arc::new(tools),
        permission: Arc::new(AllowAll),
        model: Model::new("anthropic", "claude-3", "route_scripted"),
        agent: "build".into(),
        agent_prompt: Some("SYSTEM".into()),
        directory: std::env::temp_dir(),
        max_steps: None,
        abort: CancellationToken::new(),
        subagent: None,
        preserve_recent_tokens: 20_000,
        compaction_reserved: 20_000,
        auto_compact: true,
        max_retries: 5,
        max_total_retries: 20,
        event_tx: None,
        system_cache: None,
        tersemode_directive: None,
    }
}

async fn parts_of(store: &Store, message_id: &str) -> Vec<Part> {
    store.list_parts(message_id).await.expect("parts")
}

// -- test --------------------------------------------------------------------

#[tokio::test]
async fn mcp_tool_executes_through_run_loop() {
    // 1. Stand up the in-process MCP server + client over a duplex pipe.
    let (server_io, client_io) = tokio::io::duplex(8192);
    let server = serve_test_server(server_io);
    let client = McpClient::new("test");
    client
        .connect_transport("testserver", client_io)
        .await
        .expect("client connect");
    let _server = server.await.expect("server join");

    // 2. Registry = builtins + the namespaced MCP tools (`testserver_echo`).
    let mut registry = ToolRegistry::with_builtins(None);
    let mcp_tools = client.tools();
    assert!(
        mcp_tools.iter().any(|t| t.id() == "testserver_echo"),
        "namespaced MCP tool present: {:?}",
        mcp_tools
            .iter()
            .map(|t| t.id().to_string())
            .collect::<Vec<_>>()
    );
    for tool in mcp_tools {
        registry.register(tool);
    }

    // 3. Seed a session + user message.
    let store = seed("please echo via mcp").await;

    // 4. Turn 1: call the MCP tool, finish=tool-calls. Turn 2: final text, stop.
    let mut turn1 = vec![step_start()];
    turn1.extend(text_events("t1", "calling mcp"));
    turn1.push(tool_call(
        "call_1",
        "testserver_echo",
        json!({ "text": "hi" }),
    ));
    turn1.push(step_finish(FinishReason::ToolCalls));
    turn1.push(finish(FinishReason::ToolCalls));

    let mut turn2 = vec![step_start()];
    turn2.extend(text_events("t2", "done"));
    turn2.push(step_finish(FinishReason::Stop));
    turn2.push(finish(FinishReason::Stop));

    let (route, calls) = ScriptedRoute::build(vec![turn1, turn2]);
    let cfg = config(store.clone(), route, registry);

    // 5. Run the loop.
    let last = run_loop(&cfg, SES).await.expect("run_loop");

    // Exactly two provider turns: the MCP tool call forced the second.
    assert_eq!(calls.load(Ordering::SeqCst), 2, "two provider turns");

    // Final assistant text is "done" (loop terminated normally).
    let final_text = parts_of(&store, last.id())
        .await
        .into_iter()
        .find_map(|p| match p.kind {
            PartKind::Text { text, .. } => Some(text),
            _ => None,
        })
        .expect("final text part");
    assert_eq!(final_text, "done");

    // A completed `testserver_echo` tool part exists with output "hi" — the MCP
    // tool actually executed and its result was fed back through storage.
    let all_messages = store.list_messages(SES).await.expect("messages");
    let mut mcp_output = None;
    for m in &all_messages {
        for p in parts_of(&store, m.id()).await {
            if let PartKind::Tool {
                tool,
                state: ToolState::Completed { output, .. },
                ..
            } = &p.kind
                && tool == "testserver_echo"
            {
                mcp_output = Some(output.clone());
            }
        }
    }
    assert_eq!(
        mcp_output.as_deref(),
        Some("hi"),
        "completed MCP tool result fed back"
    );
}
