//! Integration tests for real subagent spawning ([`SessionSubagentSpawner`])
//! and the live permission gate wired into [`run_loop`].
//!
//! A [`ScriptedRoute`] returns canned [`LLMEvent`] streams so the whole loop
//! (parent + child) runs headless with no network — the same pattern as
//! `run_loop.rs`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use otto_agent::AgentInfo;
use otto_events::{FinishReason, LLMEvent};
use otto_llm::{LLMError, LLMRequest, Model, Route};
use otto_permission::{Permission, Ruleset, SessionGate};
use otto_session::{RouteFor, RunConfig, SessionSubagentSpawner, run_loop};
use otto_storage::model::{
    Info, InfoBody, Part, PartKind, ToolState, User, UserModel, UserTime, new_message_id,
    new_part_id,
};
use otto_storage::{Session, SessionTokens, Store};
use otto_tools::{
    ExecuteResult, PermissionRequest, SubagentSpawner, Tool, ToolContext, ToolError, ToolRegistry,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// -- scripted route ----------------------------------------------------------

/// A [`Route`] that returns a canned event stream per `stream()` call.
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

/// A finished text-only turn: text then finish(stop).
fn text_turn(id: &str, text: &str) -> Vec<LLMEvent> {
    let mut t = vec![step_start()];
    t.extend(text_events(id, text));
    t.push(step_finish(FinishReason::Stop));
    t.push(finish(FinishReason::Stop));
    t
}

// -- fixtures ----------------------------------------------------------------

async fn seed_session(store: &Store, id: &str, text: &str) -> String {
    store
        .create_session(&Session {
            id: id.into(),
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
    store
        .insert_message(&Info {
            id: user_id.clone(),
            session_id: id.into(),
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
        })
        .await
        .expect("user msg");
    store
        .insert_part(&Part {
            id: new_part_id(),
            session_id: id.into(),
            message_id: user_id.clone(),
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
    user_id
}

fn registry(tools: Vec<Arc<dyn Tool>>) -> Arc<ToolRegistry> {
    let mut r = ToolRegistry::new();
    for t in tools {
        r.register(t);
    }
    Arc::new(r)
}

fn model() -> Model {
    Model::new("anthropic", "claude-3", "route_scripted")
}

/// Find the first tool part with the given tool name across a session.
async fn tool_state(store: &Store, session_id: &str, tool_name: &str) -> Option<ToolState> {
    for m in store.list_messages(session_id).await.expect("messages") {
        for p in store.list_parts(m.id()).await.expect("parts") {
            if let PartKind::Tool { tool, state, .. } = &p.kind
                && tool == tool_name
            {
                return Some(state.clone());
            }
        }
    }
    None
}

// -- 1. subagent spawn end-to-end -------------------------------------------

#[tokio::test]
async fn subagent_spawn_end_to_end() {
    let store = Store::open_in_memory().await.expect("store");
    let parent_id = "ses_parent";
    seed_session(&store, parent_id, "please delegate").await;

    // Child route: one text turn returning the answer.
    let (child_route, child_calls) = ScriptedRoute::build(vec![text_turn("c1", "child answer")]);
    let route_for: RouteFor = {
        let child_route = child_route.clone();
        Arc::new(move |_agent: &AgentInfo| (child_route.clone(), model()))
    };

    let permission = Arc::new(Permission::new(Ruleset::from_config(
        &json!({ "*": "allow" }),
    )));
    let tools = registry(vec![Arc::new(otto_tools::TaskTool)]);

    let spawner = Arc::new(SessionSubagentSpawner::new(
        store.clone(),
        tools.clone(),
        permission.clone(),
        Ruleset::new(),
        json!({}),
        route_for,
        std::env::temp_dir(),
        "prj_1",
        "1.0.0",
        None,
    ));

    // Parent turn 1: a task tool-call; turn 2: text done.
    let mut parent_turn1 = vec![step_start()];
    parent_turn1.push(tool_call(
        "call_1",
        "task",
        json!({
            "subagent_type": "general",
            "description": "d",
            "prompt": "do X"
        }),
    ));
    parent_turn1.push(step_finish(FinishReason::ToolCalls));
    parent_turn1.push(finish(FinishReason::ToolCalls));
    let (parent_route, _) =
        ScriptedRoute::build(vec![parent_turn1, text_turn("p2", "parent done")]);

    let cfg = RunConfig {
        store: store.clone(),
        route: parent_route,
        tools: tools.clone(),
        permission: Arc::new(SessionGate::new(permission.clone(), parent_id)),
        model: model(),
        agent: "build".into(),
        agent_prompt: Some("SYSTEM".into()),
        directory: std::env::temp_dir(),
        max_steps: None,
        abort: CancellationToken::new(),
        subagent: Some(spawner as Arc<dyn SubagentSpawner>),
        preserve_recent_tokens: 20_000,
        compaction_reserved: 20_000,
        auto_compact: true,
        max_retries: 5,
        event_tx: None,
        system_cache: None,
        tersemode_directive: None,
    };

    run_loop(&cfg, parent_id).await.expect("parent run_loop");

    // The child ran exactly once.
    assert_eq!(child_calls.load(Ordering::SeqCst), 1, "child ran once");

    // The parent's task tool result carries the wrapped child text.
    let state = tool_state(&store, parent_id, "task")
        .await
        .expect("a task tool part");
    let output = match state {
        ToolState::Completed { output, .. } => output,
        other => panic!("expected completed task, got {other:?}"),
    };
    assert!(
        output.contains("<task_result>"),
        "wrapper present: {output}"
    );
    assert!(
        output.contains("child answer"),
        "child text present: {output}"
    );

    // A child session exists with parentID = parent session.
    let sessions = store.list_sessions().await.expect("sessions");
    let child = sessions
        .iter()
        .find(|s| s.parent_id.as_deref() == Some(parent_id))
        .expect("a child session");
    assert_eq!(child.parent_id.as_deref(), Some(parent_id));
    // The child's derived permission was persisted in metadata.
    assert!(
        child
            .metadata
            .as_ref()
            .is_some_and(|m| m.get("permission").is_some())
    );
}

// -- 1b. spawn_many dispatches a batch, preserving order + isolation --------

#[tokio::test]
async fn spawn_many_delegates_in_order() {
    let store = Store::open_in_memory().await.expect("store");
    let permission = Arc::new(Permission::new(Ruleset::from_config(
        &json!({ "*": "allow" }),
    )));
    let tools = registry(vec![Arc::new(otto_tools::TaskTool)]);
    // Each spawned child gets its own fresh single-turn route (route_for is
    // invoked once per `spawn` call), so both children resolve independently.
    let route_for: RouteFor = Arc::new(move |_agent: &AgentInfo| {
        let (route, _) = ScriptedRoute::build(vec![text_turn("c1", "child answer")]);
        (route, model())
    });

    let spawner = SessionSubagentSpawner::new(
        store.clone(),
        tools,
        permission,
        Ruleset::new(),
        json!({}),
        route_for,
        std::env::temp_dir(),
        "prj_1",
        "1.0.0",
        None,
    );

    let make_req = |description: &str| otto_tools::SubagentRequest {
        subagent_type: "general".into(),
        description: description.into(),
        prompt: "p".into(),
        parent_session_id: "ses_x".into(),
        parent_message_id: "msg_x".into(),
        task_id: None,
        command: None,
        abort: CancellationToken::new(),
        event_tx: None,
    };
    let reqs = vec![make_req("one"), make_req("two")];

    let out = spawner.spawn_many(reqs).await;

    assert_eq!(out.len(), 2);
    assert!(out[0].is_ok(), "first: {:?}", out[0]);
    assert!(out[1].is_ok(), "second: {:?}", out[1]);
}

// -- 2. unknown subagent_type ------------------------------------------------

#[tokio::test]
async fn unknown_subagent_type_errors() {
    let store = Store::open_in_memory().await.expect("store");
    let permission = Arc::new(Permission::new(Ruleset::from_config(
        &json!({ "*": "allow" }),
    )));
    let tools = registry(vec![Arc::new(otto_tools::TaskTool)]);
    let (child_route, _) = ScriptedRoute::build(vec![text_turn("c1", "x")]);
    let route_for: RouteFor = Arc::new(move |_a| (child_route.clone(), model()));

    let spawner = SessionSubagentSpawner::new(
        store.clone(),
        tools,
        permission,
        Ruleset::new(),
        json!({}),
        route_for,
        std::env::temp_dir(),
        "prj_1",
        "1.0.0",
        None,
    );

    let err = spawner
        .spawn(otto_tools::SubagentRequest {
            subagent_type: "does_not_exist".into(),
            description: "d".into(),
            prompt: "p".into(),
            parent_session_id: "ses_x".into(),
            parent_message_id: "msg_x".into(),
            task_id: None,
            command: None,
            abort: CancellationToken::new(),
            event_tx: None,
        })
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("does_not_exist"), "names the bad type: {msg}");
    assert!(msg.contains("general"), "lists available agents: {msg}");
}

// -- 2b. event_tx forwards the child run's stream ----------------------------

/// A `SubagentRequest` carrying `event_tx: Some(tx)` taps the child run: at
/// least one `LLMEvent` from the child's scripted turn arrives on the receiver.
#[tokio::test]
async fn event_tx_forwards_child_run_events() {
    let store = Store::open_in_memory().await.expect("store");
    let permission = Arc::new(Permission::new(Ruleset::from_config(
        &json!({ "*": "allow" }),
    )));
    let tools = registry(vec![Arc::new(otto_tools::TaskTool)]);
    let (child_route, _) = ScriptedRoute::build(vec![text_turn("c1", "child answer")]);
    let route_for: RouteFor = Arc::new(move |_a| (child_route.clone(), model()));

    let spawner = SessionSubagentSpawner::new(
        store.clone(),
        tools,
        permission,
        Ruleset::new(),
        json!({}),
        route_for,
        std::env::temp_dir(),
        "prj_1",
        "1.0.0",
        None,
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LLMEvent>();
    let out = spawner
        .spawn(otto_tools::SubagentRequest {
            subagent_type: "general".into(),
            description: "d".into(),
            prompt: "p".into(),
            parent_session_id: "ses_x".into(),
            parent_message_id: "msg_x".into(),
            task_id: None,
            command: None,
            abort: CancellationToken::new(),
            event_tx: Some(tx),
        })
        .await
        .expect("child run");
    assert!(out.contains("child answer"), "child text returned: {out}");

    // The child run forwarded at least one event onto the tap.
    let mut got = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        got.push(ev);
    }
    assert!(
        !got.is_empty(),
        "expected the child run to forward at least one LLMEvent onto event_tx"
    );
}

// -- 3. nested subagent ------------------------------------------------------

#[tokio::test]
async fn nested_subagent_spawn() {
    let store = Store::open_in_memory().await.expect("store");
    let parent_id = "ses_parent";
    seed_session(&store, parent_id, "delegate deeply").await;

    // Grandchild route: plain text answer.
    let (grandchild_route, grandchild_calls) =
        ScriptedRoute::build(vec![text_turn("g1", "grandchild answer")]);
    // Child route turn 1: spawn another task; turn 2: text done.
    let mut child_turn1 = vec![step_start()];
    child_turn1.push(tool_call(
        "call_c",
        "task",
        json!({ "subagent_type": "general", "description": "d2", "prompt": "go deeper" }),
    ));
    child_turn1.push(step_finish(FinishReason::ToolCalls));
    child_turn1.push(finish(FinishReason::ToolCalls));
    let (child_route, _) = ScriptedRoute::build(vec![child_turn1, text_turn("c2", "child done")]);

    // The route factory hands out child route first, then grandchild route.
    let routes: Arc<Mutex<VecDeque<Arc<dyn Route>>>> =
        Arc::new(Mutex::new(VecDeque::from([child_route, grandchild_route])));
    let route_for: RouteFor = Arc::new(move |_a| {
        let r = routes.lock().unwrap().pop_front().expect("a route");
        (r, model())
    });

    // `general` allows `task` by default? No — derive denies it. Use an
    // allow-all Permission service and a custom agent that permits `task` so the
    // nested spawn is not gated out. Build config.agent granting task.
    let config_agents = json!({ "general": { "permission": { "task": "allow" } } });
    let permission = Arc::new(Permission::new(Ruleset::from_config(
        &json!({ "*": "allow" }),
    )));
    let tools = registry(vec![Arc::new(otto_tools::TaskTool)]);

    let spawner = Arc::new(SessionSubagentSpawner::new(
        store.clone(),
        tools.clone(),
        permission.clone(),
        Ruleset::new(),
        config_agents,
        route_for,
        std::env::temp_dir(),
        "prj_1",
        "1.0.0",
        None,
    ));

    let mut parent_turn1 = vec![step_start()];
    parent_turn1.push(tool_call(
        "call_p",
        "task",
        json!({ "subagent_type": "general", "description": "d1", "prompt": "delegate" }),
    ));
    parent_turn1.push(step_finish(FinishReason::ToolCalls));
    parent_turn1.push(finish(FinishReason::ToolCalls));
    let (parent_route, _) =
        ScriptedRoute::build(vec![parent_turn1, text_turn("p2", "parent done")]);

    let cfg = RunConfig {
        store: store.clone(),
        route: parent_route,
        tools,
        permission: Arc::new(SessionGate::new(permission.clone(), parent_id)),
        model: model(),
        agent: "build".into(),
        agent_prompt: Some("SYSTEM".into()),
        directory: std::env::temp_dir(),
        max_steps: None,
        abort: CancellationToken::new(),
        subagent: Some(spawner as Arc<dyn SubagentSpawner>),
        preserve_recent_tokens: 20_000,
        compaction_reserved: 20_000,
        auto_compact: true,
        max_retries: 5,
        event_tx: None,
        system_cache: None,
        tersemode_directive: None,
    };

    run_loop(&cfg, parent_id).await.expect("parent run_loop");

    // The grandchild ran — nested spawner re-injection worked.
    assert_eq!(grandchild_calls.load(Ordering::SeqCst), 1, "grandchild ran");

    // Two nested sessions exist below the parent (child + grandchild).
    let sessions = store.list_sessions().await.expect("sessions");
    let child = sessions
        .iter()
        .find(|s| s.parent_id.as_deref() == Some(parent_id))
        .expect("child session");
    assert!(
        sessions
            .iter()
            .any(|s| s.parent_id.as_deref() == Some(child.id.as_str())),
        "grandchild session parented to child"
    );
}

// -- 4. permission gate wiring ----------------------------------------------

/// A tool that asks the gate for the `edit` permission; succeeds only if the
/// gate grants it.
struct AskEditTool;

#[async_trait]
impl Tool for AskEditTool {
    fn id(&self) -> &str {
        "askedit"
    }
    fn description(&self) -> &str {
        "asks for edit permission"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        ctx.permission
            .ask(PermissionRequest {
                permission: "edit".into(),
                patterns: vec!["*".into()],
                always: vec!["*".into()],
                metadata: json!({}),
            })
            .await?;
        Ok(ExecuteResult::new("askedit", "edited"))
    }
}

async fn run_askedit_with_ruleset(ruleset: Value) -> ToolState {
    let store = Store::open_in_memory().await.expect("store");
    let ses = "ses_perm";
    seed_session(&store, ses, "edit please").await;

    let mut turn1 = vec![step_start()];
    turn1.push(tool_call("call_1", "askedit", json!({})));
    turn1.push(step_finish(FinishReason::ToolCalls));
    turn1.push(finish(FinishReason::ToolCalls));
    let (route, _) = ScriptedRoute::build(vec![turn1, text_turn("t2", "ok")]);

    let permission = Arc::new(Permission::new(Ruleset::from_config(&ruleset)));
    let cfg = RunConfig {
        store: store.clone(),
        route,
        tools: registry(vec![Arc::new(AskEditTool)]),
        permission: Arc::new(SessionGate::new(permission, ses)),
        model: model(),
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
        event_tx: None,
        system_cache: None,
        tersemode_directive: None,
    };
    run_loop(&cfg, ses).await.expect("run_loop");
    tool_state(&store, ses, "askedit")
        .await
        .expect("askedit tool part")
}

#[tokio::test]
async fn permission_deny_blocks_tool() {
    let state = run_askedit_with_ruleset(json!({ "edit": "deny" })).await;
    match state {
        ToolState::Error { error, .. } => {
            assert!(
                error.to_lowercase().contains("denied") || error.contains("edit"),
                "denial recorded: {error}"
            );
        }
        other => panic!("expected error (denied) tool state, got {other:?}"),
    }
}

#[tokio::test]
async fn permission_allow_admits_tool() {
    let state = run_askedit_with_ruleset(json!({ "edit": "allow" })).await;
    match state {
        ToolState::Completed { output, .. } => assert_eq!(output, "edited"),
        other => panic!("expected completed tool state, got {other:?}"),
    }
}

#[tokio::test]
async fn permission_ask_admits_on_reply_once() {
    // Default ruleset → `edit` resolves to Ask, so the gate blocks until a
    // reply. A spawned task subscribes and replies `Once`.
    let store = Store::open_in_memory().await.expect("store");
    let ses = "ses_ask";
    seed_session(&store, ses, "edit please").await;

    let mut turn1 = vec![step_start()];
    turn1.push(tool_call("call_1", "askedit", json!({})));
    turn1.push(step_finish(FinishReason::ToolCalls));
    turn1.push(finish(FinishReason::ToolCalls));
    let (route, _) = ScriptedRoute::build(vec![turn1, text_turn("t2", "ok")]);

    let permission = Arc::new(Permission::new(Ruleset::new()));
    let mut asked = permission.subscribe();
    let replier = permission.clone();
    tokio::spawn(async move {
        if let Ok(evt) = asked.recv().await {
            replier.reply(&evt.request_id, otto_permission::Reply::Once);
        }
    });

    let cfg = RunConfig {
        store: store.clone(),
        route,
        tools: registry(vec![Arc::new(AskEditTool)]),
        permission: Arc::new(SessionGate::new(permission, ses)),
        model: model(),
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
        event_tx: None,
        system_cache: None,
        tersemode_directive: None,
    };
    run_loop(&cfg, ses).await.expect("run_loop");

    match tool_state(&store, ses, "askedit").await.expect("part") {
        ToolState::Completed { output, .. } => assert_eq!(output, "edited"),
        other => panic!("expected completed after reply Once, got {other:?}"),
    }
}
