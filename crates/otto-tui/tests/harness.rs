#![allow(dead_code)]
//! Shared scripted-server test harness for `otto-tui` integration tests.
//!
//! Copied verbatim (structure + behavior) from `crates/otto-server/tests/server.rs`'s
//! harness section — a scripted [`RouteFactory`] + in-memory [`Runtime`] drive the whole
//! HTTP/SSE surface headless (no network to a provider); the server itself is bound to
//! `127.0.0.1:0` and exercised with `reqwest`.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use otto_agent::ModelRef;
use otto_app::{Result as AppResult, Runtime};
use otto_config::Config;
use otto_events::{FinishReason, LLMEvent};
use otto_llm::{LLMError, LLMRequest, Model, Route};
use otto_server::{ServeOptions, serve};
use otto_tools::{ExecuteResult, PermissionRequest, Tool, ToolContext, ToolError};
use serde_json::{Value, json};

// -- scripted route + factory ------------------------------------------------

pub struct ScriptedRoute {
    turns: Mutex<VecDeque<Vec<LLMEvent>>>,
    calls: Arc<AtomicUsize>,
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

pub struct ScriptedRouteFactory {
    route: Arc<ScriptedRoute>,
    model: Model,
}

impl ScriptedRouteFactory {
    pub fn new(turns: Vec<Vec<LLMEvent>>) -> (Arc<Self>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let route = Arc::new(ScriptedRoute {
            turns: Mutex::new(turns.into_iter().collect()),
            calls: calls.clone(),
        });
        let factory = Arc::new(Self {
            route,
            model: Model::new("anthropic", "claude-3", "route_scripted"),
        });
        (factory, calls)
    }
}

impl otto_app::RouteFactory for ScriptedRouteFactory {
    fn route_for(&self, _m: &ModelRef) -> AppResult<(Arc<dyn Route>, Model)> {
        Ok((self.route.clone(), self.model.clone()))
    }
}

// -- guard tool (asks permission) --------------------------------------------

pub struct GuardTool;

#[async_trait]
impl Tool for GuardTool {
    fn id(&self) -> &str {
        "guard"
    }
    fn description(&self) -> &str {
        "asks for the danger permission"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        ctx.permission
            .ask(PermissionRequest {
                permission: "danger".into(),
                patterns: vec!["x".into()],
                always: vec!["x".into()],
                metadata: json!({}),
            })
            .await?;
        Ok(ExecuteResult::new("guard", "ran"))
    }
}

// -- scripted event helpers --------------------------------------------------

pub fn step_start() -> LLMEvent {
    LLMEvent::StepStart { index: 0 }
}
pub fn step_finish(reason: FinishReason) -> LLMEvent {
    LLMEvent::StepFinish {
        index: 0,
        reason,
        usage: None,
        provider_metadata: None,
    }
}
pub fn finish(reason: FinishReason) -> LLMEvent {
    LLMEvent::Finish {
        reason,
        usage: None,
        provider_metadata: None,
    }
}
pub fn tool_call(id: &str, name: &str, input: Value) -> LLMEvent {
    LLMEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        input,
        provider_executed: None,
        provider_metadata: None,
    }
}
pub fn text_events(id: &str, text: &str) -> Vec<LLMEvent> {
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
pub fn text_turn(id: &str, text: &str) -> Vec<LLMEvent> {
    let mut turn = vec![step_start()];
    turn.extend(text_events(id, text));
    turn.push(step_finish(FinishReason::Stop));
    turn.push(finish(FinishReason::Stop));
    turn
}

// -- harness -----------------------------------------------------------------

/// Bind the server on an ephemeral port and return its base URL.
pub async fn spawn(runtime: Arc<Runtime>, opts: ServeOptions) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    drop(listener); // free the port for `serve` to rebind
    tokio::spawn(async move {
        let _ = serve(runtime, addr, opts).await;
    });
    // Give the server a beat to bind.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    format!("http://{addr}")
}

pub fn no_auth() -> ServeOptions {
    ServeOptions {
        password: None,
        cors: false,
    }
}

pub async fn plain_runtime() -> Arc<Runtime> {
    let (factory, _) = ScriptedRouteFactory::new(vec![]);
    Arc::new(
        Runtime::in_memory(Config::default())
            .await
            .unwrap()
            .with_route_factory(factory),
    )
}
