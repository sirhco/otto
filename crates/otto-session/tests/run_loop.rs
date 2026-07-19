//! Integration tests for the agent [`run_loop`] and the [`augment_with_tools`]
//! tool-execution runtime.
//!
//! A [`ScriptedRoute`] returns canned [`LLMEvent`] streams (one per `stream()`
//! call) so the whole loop runs headless with no network. An echo tool proves
//! the tool cycle round-trips through storage.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use otto_events::{FinishReason, LLMEvent, ToolResultValue};
use otto_llm::{LLMError, LLMRequest, Model, Route};
use otto_permission::{Permission, Reply, Ruleset, SessionGate};
use otto_session::{RunConfig, augment_with_tools, run_loop};
use otto_storage::model::{
    Info, InfoBody, MessageId, Part, PartKind, SessionId, ToolState, User, UserModel, UserTime,
    new_message_id, new_part_id,
};
use otto_storage::{Session, SessionTokens, Store};
use otto_tools::{
    AllowAll, DenyAllQuestions, ExecuteResult, Tool, ToolContext, ToolError, ToolRegistry,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// -- scripted route ----------------------------------------------------------

/// A [`Route`] that returns a canned event stream per `stream()` call, popping
/// one turn from a queue each time.
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

/// A [`Route`] whose first `stream()` call fails with a retryable HTTP 429
/// and whose second call succeeds with a canned turn — for exercising the
/// run loop's retry arm without a real provider.
struct RetryOnceRoute {
    calls: AtomicUsize,
    success_turn: Mutex<Option<Vec<LLMEvent>>>,
}

impl RetryOnceRoute {
    fn build(success_turn: Vec<LLMEvent>) -> Arc<dyn Route> {
        Arc::new(RetryOnceRoute {
            calls: AtomicUsize::new(0),
            success_turn: Mutex::new(Some(success_turn)),
        })
    }
}

impl Route for RetryOnceRoute {
    fn id(&self) -> &str {
        "retry-once"
    }

    fn stream(&self, _req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            stream::iter(vec![Err(LLMError::Http {
                status: 429,
                message: "rate limit".into(),
                retry_after: None,
            })])
            .boxed()
        } else {
            let events = self.success_turn.lock().unwrap().take().unwrap_or_default();
            stream::iter(events.into_iter().map(Ok)).boxed()
        }
    }
}

// -- tools -------------------------------------------------------------------

/// A tool that echoes its `text` argument back as output.
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo the text argument"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "text": { "type": "string" } } })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(ExecuteResult::new("echo", text))
    }
}

/// A tool that blocks until the context abort token is cancelled.
struct WaitTool;

#[async_trait]
impl Tool for WaitTool {
    fn id(&self) -> &str {
        "wait"
    }
    fn description(&self) -> &str {
        "wait until aborted"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        ctx.abort.cancelled().await;
        Err(ToolError::Aborted)
    }
}

// -- fixtures ----------------------------------------------------------------

const SES: &str = "ses_run";

fn sid(s: &str) -> SessionId {
    s.into()
}

async fn seed(text: &str) -> (Store, MessageId) {
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

    (store, user_id)
}

fn config(
    store: Store,
    route: Arc<dyn Route>,
    tools: ToolRegistry,
    abort: CancellationToken,
) -> RunConfig {
    RunConfig {
        store,
        route,
        tools: Arc::new(tools),
        permission: Arc::new(AllowAll),
        question: Arc::new(DenyAllQuestions),
        model: Model::new("anthropic", "claude-3", "route_scripted"),
        agent: "build".into(),
        agent_prompt: Some("SYSTEM".into()),
        // An isolated, empty working directory. `build_system` now scans `cwd`
        // for `SKILL.md` files (the skills index), so pointing at the shared
        // system temp dir would recursively walk hundreds of unrelated entries
        // and blow the tight abort-race budget in `abort_mid_flight_...`. A
        // dedicated empty dir keeps the scan instant and hermetic.
        directory: {
            let d = std::env::temp_dir().join("otto-run-loop-cwd");
            std::fs::create_dir_all(&d).expect("create isolated test cwd");
            d
        },
        max_steps: None,
        abort,
        subagent: None,
        preserve_recent_tokens: 20_000,
        compaction_reserved: 20_000,
        auto_compact: true,
        prune_protect_tokens: 40_000,
        max_retries: 5,
        max_total_retries: 20,
        event_tx: None,
        system_cache: None,
        tersemode_directive: None,
        hooks: None,
    }
}

fn registry(tool: Arc<dyn Tool>) -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(tool);
    r
}

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

/// A turn that emits an open + streamed text block "…".
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

async fn parts_of(store: &Store, message_id: &MessageId) -> Vec<Part> {
    store.list_parts(message_id).await.expect("parts")
}

// -- tests -------------------------------------------------------------------

#[tokio::test]
async fn stop_deny_injects_synthetic_continuation_and_reruns() {
    let (store, _user_id) = seed("hi").await;

    let mut turn1 = vec![step_start()];
    turn1.extend(text_events("t1", "first response"));
    turn1.push(step_finish(FinishReason::Stop));
    turn1.push(finish(FinishReason::Stop));

    let mut turn2 = vec![step_start()];
    turn2.extend(text_events("t2", "second response"));
    turn2.push(step_finish(FinishReason::Stop));
    turn2.push(finish(FinishReason::Stop));

    let (route, calls) = ScriptedRoute::build(vec![turn1, turn2]);

    // A stateful shell hook: denies the first time it's called (creating a
    // marker file), allows every time after — proving exactly one
    // deny-then-continue cycle.
    let marker = std::env::temp_dir().join(format!("otto-stop-test-marker-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let hooks_cfg = otto_hooks::HooksConfig {
        stop: vec![otto_hooks::HookMatcherGroup {
            matcher: None,
            hooks: vec![otto_hooks::HookCommand {
                command: format!(
                    "if [ -f {p} ]; then echo '{{\"decision\":\"allow\"}}'; else touch {p} && echo '{{\"decision\":\"deny\",\"reason\":\"keep going\"}}'; fi",
                    p = marker.display()
                ),
                timeout_ms: None,
            }],
        }],
        ..Default::default()
    };

    let mut cfg = config(
        store.clone(),
        route,
        ToolRegistry::new(),
        CancellationToken::new(),
    );
    cfg.hooks = Some(Arc::new(otto_hooks::HookRunner::new(hooks_cfg)));

    let info = run_loop(&cfg, &sid(SES)).await.expect("run completes");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "denied once, so two provider turns ran"
    );

    let parts = parts_of(&store, info.id()).await;
    let final_text: String = parts
        .iter()
        .filter_map(|p| match &p.kind {
            PartKind::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(final_text, "second response");

    // A synthetic continuation user message was persisted with the hook's reason.
    let msgs = store.messages_with_parts(&sid(SES)).await.expect("history");
    let synthetic_text: String = msgs
        .iter()
        .flat_map(|m| &m.parts)
        .filter_map(|p| match &p.kind {
            PartKind::Text {
                text,
                synthetic: Some(true),
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(synthetic_text, "keep going");

    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn stop_ask_approved_ends_the_turn_normally() {
    let (store, _user_id) = seed("hi").await;

    let mut turn = vec![step_start()];
    turn.extend(text_events("t1", "final response"));
    turn.push(step_finish(FinishReason::Stop));
    turn.push(finish(FinishReason::Stop));
    let (route, calls) = ScriptedRoute::build(vec![turn]);

    let mut cfg = config(
        store.clone(),
        route,
        ToolRegistry::new(),
        CancellationToken::new(),
    );
    cfg.hooks = Some(Arc::new(otto_hooks::HookRunner::new(
        otto_hooks::HooksConfig {
            stop: vec![otto_hooks::HookMatcherGroup {
                matcher: None,
                hooks: vec![otto_hooks::HookCommand {
                    command: "echo '{\"decision\":\"ask\",\"reason\":\"confirm stop\"}'"
                        .to_string(),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        },
    )));

    let permission = Arc::new(Permission::new(Ruleset::new()));
    cfg.permission = Arc::new(SessionGate::new(permission.clone(), sid(SES)));
    let mut asks = permission.subscribe();

    let cfg_for_run = cfg.clone();
    let handle = tokio::spawn(async move { run_loop(&cfg_for_run, &sid(SES)).await });

    let asked = asks
        .recv()
        .await
        .expect("Stop ask surfaces a Permission ask");
    assert_eq!(asked.permission, "hook");
    assert_eq!(asked.patterns, vec!["stop".to_string()]);
    permission.reply(&asked.request_id, Reply::Once);

    handle.await.unwrap().expect("run completes");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "approval ends the turn after one provider call"
    );

    let msgs = store.messages_with_parts(&sid(SES)).await.expect("history");
    let has_synthetic = msgs.iter().flat_map(|m| &m.parts).any(|p| {
        matches!(
            &p.kind,
            PartKind::Text {
                synthetic: Some(true),
                ..
            }
        )
    });
    assert!(
        !has_synthetic,
        "approval must not synthesize a continuation"
    );
}

#[tokio::test]
async fn stop_ask_rejected_injects_the_human_message_and_reruns() {
    let (store, _user_id) = seed("hi").await;

    let mut turn1 = vec![step_start()];
    turn1.extend(text_events("t1", "first response"));
    turn1.push(step_finish(FinishReason::Stop));
    turn1.push(finish(FinishReason::Stop));

    let mut turn2 = vec![step_start()];
    turn2.extend(text_events("t2", "second response"));
    turn2.push(step_finish(FinishReason::Stop));
    turn2.push(finish(FinishReason::Stop));

    let (route, calls) = ScriptedRoute::build(vec![turn1, turn2]);

    let mut cfg = config(
        store.clone(),
        route,
        ToolRegistry::new(),
        CancellationToken::new(),
    );
    cfg.hooks = Some(Arc::new(otto_hooks::HookRunner::new(
        otto_hooks::HooksConfig {
            stop: vec![otto_hooks::HookMatcherGroup {
                matcher: None,
                hooks: vec![otto_hooks::HookCommand {
                    command: "echo '{\"decision\":\"ask\",\"reason\":\"confirm stop\"}'"
                        .to_string(),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        },
    )));

    let permission = Arc::new(Permission::new(Ruleset::new()));
    cfg.permission = Arc::new(SessionGate::new(permission.clone(), sid(SES)));
    let mut asks = permission.subscribe();

    let cfg_for_run = cfg.clone();
    let handle = tokio::spawn(async move { run_loop(&cfg_for_run, &sid(SES)).await });

    let first = asks.recv().await.expect("first Stop ask");
    permission.reply(
        &first.request_id,
        Reply::Reject {
            message: Some("finish the second part".to_string()),
        },
    );
    let second = asks
        .recv()
        .await
        .expect("second Stop ask, after the synthetic continuation");
    permission.reply(&second.request_id, Reply::Once);

    let info = handle.await.unwrap().expect("run completes");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "rejected once, so two provider turns ran"
    );

    let parts = parts_of(&store, info.id()).await;
    let final_text: String = parts
        .iter()
        .filter_map(|p| match &p.kind {
            PartKind::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(final_text, "second response");

    let msgs = store.messages_with_parts(&sid(SES)).await.expect("history");
    let synthetic_text: String = msgs
        .iter()
        .flat_map(|m| &m.parts)
        .filter_map(|p| match &p.kind {
            PartKind::Text {
                text,
                synthetic: Some(true),
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        synthetic_text, "finish the second part",
        "the human's Reject message wins over the hook's own reason"
    );
}

#[tokio::test]
async fn user_prompt_submit_deny_blocks_before_any_provider_call() {
    let (store, _user_id) = seed("hi").await;

    let mut turn = vec![step_start()];
    turn.extend(text_events("t1", "should never run"));
    turn.push(step_finish(FinishReason::Stop));
    turn.push(finish(FinishReason::Stop));
    let (route, calls) = ScriptedRoute::build(vec![turn]);

    let mut cfg = config(store, route, ToolRegistry::new(), CancellationToken::new());
    cfg.hooks = Some(Arc::new(otto_hooks::HookRunner::new(
        otto_hooks::HooksConfig {
            user_prompt_submit: vec![otto_hooks::HookMatcherGroup {
                matcher: None,
                hooks: vec![otto_hooks::HookCommand {
                    command: "echo '{\"decision\":\"deny\",\"reason\":\"blocked prompt\"}'"
                        .to_string(),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        },
    )));

    let err = run_loop(&cfg, &sid(SES)).await.expect_err("denied");
    assert_eq!(err.to_string(), "blocked prompt");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no provider call made");
}

#[tokio::test]
async fn user_prompt_submit_ask_approved_continues_the_turn() {
    let (store, _user_id) = seed("hi").await;

    let mut turn = vec![step_start()];
    turn.extend(text_events("t1", "approved response"));
    turn.push(step_finish(FinishReason::Stop));
    turn.push(finish(FinishReason::Stop));
    let (route, calls) = ScriptedRoute::build(vec![turn]);

    let mut cfg = config(
        store.clone(),
        route,
        ToolRegistry::new(),
        CancellationToken::new(),
    );
    cfg.hooks = Some(Arc::new(otto_hooks::HookRunner::new(
        otto_hooks::HooksConfig {
            user_prompt_submit: vec![otto_hooks::HookMatcherGroup {
                matcher: None,
                hooks: vec![otto_hooks::HookCommand {
                    command: "echo '{\"decision\":\"ask\",\"reason\":\"needs review\"}'"
                        .to_string(),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        },
    )));

    let permission = Arc::new(Permission::new(Ruleset::new())); // default mode: ApproveEach
    cfg.permission = Arc::new(SessionGate::new(permission.clone(), sid(SES)));
    let mut asks = permission.subscribe();

    let cfg_for_run = cfg.clone();
    let handle = tokio::spawn(async move { run_loop(&cfg_for_run, &sid(SES)).await });

    let asked = asks
        .recv()
        .await
        .expect("UserPromptSubmit ask surfaces a Permission ask");
    assert_eq!(asked.permission, "hook");
    assert_eq!(asked.patterns, vec!["user_prompt_submit".to_string()]);
    permission.reply(&asked.request_id, Reply::Once);

    let info = handle.await.unwrap().expect("run completes");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "provider call made after approval"
    );

    let parts = parts_of(&store, info.id()).await;
    let final_text: String = parts
        .iter()
        .filter_map(|p| match &p.kind {
            PartKind::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(final_text, "approved response");
}

#[tokio::test]
async fn user_prompt_submit_ask_rejected_blocks_with_the_human_message() {
    let (store, _user_id) = seed("hi").await;

    let mut turn = vec![step_start()];
    turn.extend(text_events("t1", "should never run"));
    turn.push(step_finish(FinishReason::Stop));
    turn.push(finish(FinishReason::Stop));
    let (route, calls) = ScriptedRoute::build(vec![turn]);

    let mut cfg = config(store, route, ToolRegistry::new(), CancellationToken::new());
    cfg.hooks = Some(Arc::new(otto_hooks::HookRunner::new(
        otto_hooks::HooksConfig {
            user_prompt_submit: vec![otto_hooks::HookMatcherGroup {
                matcher: None,
                hooks: vec![otto_hooks::HookCommand {
                    command: "echo '{\"decision\":\"ask\",\"reason\":\"needs review\"}'"
                        .to_string(),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        },
    )));

    let permission = Arc::new(Permission::new(Ruleset::new()));
    cfg.permission = Arc::new(SessionGate::new(permission.clone(), sid(SES)));
    let mut asks = permission.subscribe();

    let cfg_for_run = cfg.clone();
    let handle = tokio::spawn(async move { run_loop(&cfg_for_run, &sid(SES)).await });

    let asked = asks.recv().await.expect("ask surfaces");
    permission.reply(
        &asked.request_id,
        Reply::Reject {
            message: Some("try again with more detail".to_string()),
        },
    );

    let err = handle.await.unwrap().expect_err("rejected");
    assert_eq!(err.to_string(), "try again with more detail");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no provider call made");
}

#[tokio::test]
async fn end_to_end_tool_cycle() {
    let (store, _user) = seed("please echo").await;

    // Turn 1: text + echo tool-call, finish=tool-calls.
    let mut turn1 = vec![step_start()];
    turn1.extend(text_events("t1", "let me check"));
    turn1.push(tool_call("call_1", "echo", json!({ "text": "hi" })));
    turn1.push(step_finish(FinishReason::ToolCalls));
    turn1.push(finish(FinishReason::ToolCalls));

    // Turn 2: text "done", finish=stop.
    let mut turn2 = vec![step_start()];
    turn2.extend(text_events("t2", "done"));
    turn2.push(step_finish(FinishReason::Stop));
    turn2.push(finish(FinishReason::Stop));

    let (route, calls) = ScriptedRoute::build(vec![turn1, turn2]);
    let cfg = config(
        store.clone(),
        route,
        registry(Arc::new(EchoTool)),
        CancellationToken::new(),
    );

    let last = run_loop(&cfg, &sid(SES)).await.expect("run_loop");

    // Exactly two provider turns.
    assert_eq!(calls.load(Ordering::SeqCst), 2, "two provider turns");

    // Final assistant text is "done".
    let final_parts = parts_of(&store, last.id()).await;
    let text = final_parts
        .iter()
        .find_map(|p| match &p.kind {
            PartKind::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("final text part");
    assert_eq!(text, "done");

    // A completed echo tool part exists across the session with output "hi".
    let all_messages = store.list_messages(&sid(SES)).await.expect("messages");
    let mut echo_output = None;
    for m in &all_messages {
        for p in parts_of(&store, m.id()).await {
            if let PartKind::Tool {
                tool,
                state: ToolState::Completed { output, .. },
                ..
            } = &p.kind
                && tool == "echo"
            {
                echo_output = Some(output.clone());
            }
        }
    }
    assert_eq!(echo_output.as_deref(), Some("hi"), "completed echo result");
}

#[tokio::test]
async fn exit_without_tools() {
    let (store, _user) = seed("hi").await;

    let mut turn1 = vec![step_start()];
    turn1.extend(text_events("t1", "hello"));
    turn1.push(step_finish(FinishReason::Stop));
    turn1.push(finish(FinishReason::Stop));

    let (route, calls) = ScriptedRoute::build(vec![turn1]);
    let cfg = config(
        store.clone(),
        route,
        registry(Arc::new(EchoTool)),
        CancellationToken::new(),
    );

    let last = run_loop(&cfg, &sid(SES)).await.expect("run_loop");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one provider turn");

    let text = parts_of(&store, last.id())
        .await
        .into_iter()
        .find_map(|p| match p.kind {
            PartKind::Text { text, .. } => Some(text),
            _ => None,
        })
        .expect("text part");
    assert_eq!(text, "hello");
}

#[tokio::test]
async fn abort_mid_flight_marks_tool_interrupted() {
    let (store, _user) = seed("do the thing").await;

    // Turn 1: a wait tool-call that blocks until abort.
    let mut turn1 = vec![step_start()];
    turn1.extend(text_events("t1", "working"));
    turn1.push(tool_call("call_1", "wait", json!({})));
    turn1.push(step_finish(FinishReason::ToolCalls));
    turn1.push(finish(FinishReason::ToolCalls));

    let (route, calls) = ScriptedRoute::build(vec![turn1]);
    let abort = CancellationToken::new();
    let cfg = config(
        store.clone(),
        route,
        registry(Arc::new(WaitTool)),
        abort.clone(),
    );

    // Cancel shortly after the loop starts (during turn 1, while the tool waits).
    let canceller = abort.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        canceller.cancel();
    });

    let last = run_loop(&cfg, &sid(SES))
        .await
        .expect("run_loop returns after abort");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "only one provider turn");

    // The assistant message is finalized (completed stamped).
    let assistant = store
        .get_message(&sid(SES), last.id())
        .await
        .expect("get")
        .expect("some");
    assert!(
        assistant.as_assistant().unwrap().time.completed.is_some(),
        "assistant finalized"
    );

    // The wait tool part is error + interrupted.
    let tool_state = parts_of(&store, last.id())
        .await
        .into_iter()
        .find_map(|p| match p.kind {
            PartKind::Tool { state, .. } => Some(state),
            _ => None,
        })
        .expect("a tool part");
    match tool_state {
        ToolState::Error { metadata, .. } => {
            let meta = metadata.expect("interrupted metadata");
            assert_eq!(meta.get("interrupted"), Some(&json!(true)));
        }
        other => panic!("expected interrupted error, got {other:?}"),
    }
}

#[tokio::test]
async fn has_tool_calls_guard_keeps_looping() {
    let (store, _user) = seed("echo please").await;

    // Turn 1: finish=STOP but WITH a tool-call — must still run the tool + loop.
    let mut turn1 = vec![step_start()];
    turn1.push(tool_call("call_1", "echo", json!({ "text": "again" })));
    turn1.push(step_finish(FinishReason::Stop));
    turn1.push(finish(FinishReason::Stop));

    // Turn 2: plain stop, no tools.
    let mut turn2 = vec![step_start()];
    turn2.extend(text_events("t2", "all done"));
    turn2.push(step_finish(FinishReason::Stop));
    turn2.push(finish(FinishReason::Stop));

    let (route, calls) = ScriptedRoute::build(vec![turn1, turn2]);
    let cfg = config(
        store.clone(),
        route,
        registry(Arc::new(EchoTool)),
        CancellationToken::new(),
    );

    run_loop(&cfg, &sid(SES)).await.expect("run_loop");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "tool call forces a second turn despite finish=stop"
    );
}

#[tokio::test]
async fn augment_appends_tool_result_at_tail() {
    let provider = stream::iter(vec![Ok(tool_call(
        "call_1",
        "echo",
        json!({ "text": "hi" }),
    ))])
    .boxed();
    let tools = Arc::new(registry(Arc::new(EchoTool)));
    let ctx = ToolContext::builder(std::env::temp_dir()).build();

    let out: Vec<LLMEvent> = augment_with_tools(provider, tools, ctx, "claude-3".into())
        .map(|r| r.expect("event"))
        .collect()
        .await;

    // tool-call first, tool-result appended at the tail.
    assert!(matches!(out[0], LLMEvent::ToolCall { .. }), "call first");
    let result = out
        .iter()
        .find_map(|e| match e {
            LLMEvent::ToolResult { result, .. } => Some(result),
            _ => None,
        })
        .expect("a tool-result at the tail");
    match result {
        ToolResultValue::Json { value } => {
            assert_eq!(value.get("output").and_then(Value::as_str), Some("hi"));
        }
        other => panic!("expected json result, got {other:?}"),
    }
}

#[tokio::test]
async fn augment_unknown_tool_emits_error_pair() {
    let provider = stream::iter(vec![Ok(tool_call("call_9", "nope", json!({})))]).boxed();
    let tools = Arc::new(registry(Arc::new(EchoTool)));
    let ctx = ToolContext::builder(std::env::temp_dir()).build();

    let out: Vec<LLMEvent> = augment_with_tools(provider, tools, ctx, "claude-3".into())
        .map(|r| r.expect("event"))
        .collect()
        .await;

    assert!(matches!(out[0], LLMEvent::ToolCall { .. }));
    assert!(
        out.iter().any(|e| matches!(e, LLMEvent::ToolError { .. })),
        "a tool-error is emitted"
    );
    let err_result = out.iter().any(|e| {
        matches!(
            e,
            LLMEvent::ToolResult {
                result: ToolResultValue::Error { .. },
                ..
            }
        )
    });
    assert!(err_result, "an error tool-result is emitted");
}

#[tokio::test]
async fn event_tap_yields_events_for_a_turn() {
    let (store, _user) = seed("hi").await;

    // A single scripted turn: text "hello" then stop.
    let mut turn1 = vec![step_start()];
    turn1.extend(text_events("t1", "hello"));
    turn1.push(step_finish(FinishReason::Stop));
    turn1.push(finish(FinishReason::Stop));

    let (route, _calls) = ScriptedRoute::build(vec![turn1]);
    let mut cfg = config(
        store.clone(),
        route,
        registry(Arc::new(EchoTool)),
        CancellationToken::new(),
    );

    // Install the live tap.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LLMEvent>();
    cfg.event_tx = Some(tx);

    run_loop(&cfg, &sid(SES)).await.expect("run_loop");

    // Drop `cfg` so the tap's sender is released; draining then terminates.
    drop(cfg);
    let mut tapped = Vec::new();
    while let Some(ev) = rx.recv().await {
        tapped.push(ev);
    }

    // The tap forwarded the turn's events without disturbing the run: a
    // text-delta carrying "hello" and the terminal finish both appear.
    assert!(
        tapped.iter().any(|e| matches!(
            e,
            LLMEvent::TextDelta { text, .. } if text == "hello"
        )),
        "text-delta tapped: {tapped:?}"
    );
    assert!(
        tapped.iter().any(|e| matches!(e, LLMEvent::Finish { .. })),
        "finish tapped: {tapped:?}"
    );
}

// NOTE: not `start_paused` — the sqlx in-memory pool's own connection-acquire
// timeout races a paused/auto-advancing clock and spuriously times out, so
// this test eats the real ~4s backoff (`retry::delay(1, None)`).
#[tokio::test]
async fn retry_emits_retry_event_before_backoff() {
    let (store, _user) = seed("hi").await;

    // A single turn succeeding on the route's second call.
    let mut turn1 = vec![step_start()];
    turn1.extend(text_events("t1", "done"));
    turn1.push(step_finish(FinishReason::Stop));
    turn1.push(finish(FinishReason::Stop));

    let route = RetryOnceRoute::build(turn1);
    let mut cfg = config(
        store.clone(),
        route,
        registry(Arc::new(EchoTool)),
        CancellationToken::new(),
    );

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<LLMEvent>();
    cfg.event_tx = Some(event_tx);

    run_loop(&cfg, &sid(SES))
        .await
        .expect("run_loop recovers after one retry");

    drop(cfg);
    let mut saw_retry = false;
    while let Some(ev) = event_rx.recv().await {
        if let LLMEvent::Retry { attempt, max, .. } = ev {
            assert_eq!(attempt, 1);
            assert_eq!(max, 5);
            saw_retry = true;
        }
    }
    assert!(
        saw_retry,
        "a retryable failure must emit a Retry event before backoff"
    );
}

#[tokio::test]
async fn non_retryable_error_emits_no_retry_event() {
    let (store, _user) = seed("hi").await;

    // A route whose only call fails with a non-retryable 400 — the run
    // should fail immediately without ever emitting a Retry event.
    struct AlwaysFailRoute;
    impl Route for AlwaysFailRoute {
        fn id(&self) -> &str {
            "always-fail"
        }
        fn stream(&self, _req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
            stream::iter(vec![Err(LLMError::Http {
                status: 400,
                message: "bad request".into(),
                retry_after: None,
            })])
            .boxed()
        }
    }

    let mut cfg = config(
        store.clone(),
        Arc::new(AlwaysFailRoute),
        registry(Arc::new(EchoTool)),
        CancellationToken::new(),
    );

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<LLMEvent>();
    cfg.event_tx = Some(event_tx);

    let result = run_loop(&cfg, &sid(SES)).await;
    assert!(result.is_err(), "a non-retryable error propagates");

    drop(cfg);
    let mut saw_retry = false;
    while let Some(ev) = event_rx.recv().await {
        if matches!(ev, LLMEvent::Retry { .. }) {
            saw_retry = true;
        }
    }
    assert!(!saw_retry, "a non-retryable failure must not emit Retry");
}

// NOTE: not `start_paused` — see `retry_emits_retry_event_before_backoff`.
// `max_retries: 2` keeps the single real backoff sleep at ~4s.
#[tokio::test]
async fn empty_stream_retries_with_backoff_then_errors() {
    let (store, _user) = seed("hi").await;

    // Every `stream()` call yields zero events: the shape of an
    // OpenAI-compatible gateway emitting only unrecognized frames. Before the
    // EmptyStream fix this looped to the 1000-iteration cap with no backoff,
    // inserting a fresh empty assistant message per pass.
    let (route, calls) = ScriptedRoute::build(vec![]);
    let mut cfg = config(
        store.clone(),
        route,
        registry(Arc::new(EchoTool)),
        CancellationToken::new(),
    );
    cfg.max_retries = 2;

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<LLMEvent>();
    cfg.event_tx = Some(event_tx);

    let result = run_loop(&cfg, &sid(SES)).await;
    assert!(result.is_err(), "an always-empty stream must fail the run");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "initial attempt + 1 retry under max_retries=2, not 1000 iterations"
    );

    drop(cfg);
    let mut saw_retry = false;
    while let Some(ev) = event_rx.recv().await {
        if matches!(ev, LLMEvent::Retry { .. }) {
            saw_retry = true;
        }
    }
    assert!(saw_retry, "empty attempts must surface Retry events");

    // Exactly one assistant message (retries reuse the id — no pile-up), and
    // it is finalized with the provider error.
    let msgs = store.list_messages(&sid(SES)).await.expect("messages");
    let assistants: Vec<_> = msgs.iter().filter(|m| m.is_assistant()).collect();
    assert_eq!(assistants.len(), 1, "no empty-assistant pile-up");
    let a = assistants[0].as_assistant().expect("assistant body");
    assert!(
        a.error.is_some(),
        "exhausted retries must finalize the assistant with an error"
    );
    assert!(
        a.time.completed.is_some(),
        "exhausted retries must stamp time.completed"
    );
}

#[tokio::test]
async fn total_retry_budget_binds_across_generous_per_step_budget() {
    let (store, _user) = seed("hi").await;

    // Always fails retryably with a zero Retry-After (no real sleeps). The
    // per-step budget (10) would allow many more attempts; the run-level
    // total budget (3) must bind first.
    struct AlwaysRateLimitedRoute {
        calls: Arc<AtomicUsize>,
    }
    impl Route for AlwaysRateLimitedRoute {
        fn id(&self) -> &str {
            "always-429"
        }
        fn stream(&self, _req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            stream::iter(vec![Err(LLMError::Http {
                status: 429,
                message: "rate limit".into(),
                retry_after: Some(Duration::from_millis(0)),
            })])
            .boxed()
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let mut cfg = config(
        store.clone(),
        Arc::new(AlwaysRateLimitedRoute {
            calls: calls.clone(),
        }),
        registry(Arc::new(EchoTool)),
        CancellationToken::new(),
    );
    cfg.max_retries = 10;
    cfg.max_total_retries = 3;

    let result = run_loop(&cfg, &sid(SES)).await;
    assert!(result.is_err(), "total retry budget exhausts the run");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "3 attempts total: the run budget binds before the per-step budget"
    );
}

#[tokio::test]
async fn retry_salvages_completed_tool_work_instead_of_replaying() {
    let (store, _user) = seed("please echo").await;

    // Call 0: a tool call streams and EXECUTES, then the stream dies with a
    // retryable failure (zero backoff). Call 1: the loop continues from the
    // salvaged step — history already carries the tool result — and finishes.
    struct ToolThenFailRoute {
        calls: Arc<AtomicUsize>,
    }
    impl Route for ToolThenFailRoute {
        fn id(&self) -> &str {
            "tool-then-fail"
        }
        fn stream(&self, _req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                let items: Vec<Result<LLMEvent, LLMError>> = vec![
                    Ok(step_start()),
                    Ok(tool_call("call_1", "echo", json!({"text":"salvage me"}))),
                    Err(LLMError::Http {
                        status: 429,
                        message: "rate limit".into(),
                        retry_after: Some(Duration::from_millis(0)),
                    }),
                ];
                stream::iter(items).boxed()
            } else {
                let mut turn = vec![step_start()];
                turn.extend(text_events("t2", "done after salvage"));
                turn.push(step_finish(FinishReason::Stop));
                turn.push(finish(FinishReason::Stop));
                stream::iter(turn.into_iter().map(Ok)).boxed()
            }
        }
    }

    /// An echo tool that counts executions — salvage must not re-run it.
    struct CountingEchoTool {
        runs: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Tool for CountingEchoTool {
        fn id(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object", "properties": { "text": { "type": "string" } } })
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolContext,
        ) -> Result<ExecuteResult, ToolError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(ExecuteResult::new("echo", text))
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let runs = Arc::new(AtomicUsize::new(0));
    let mut cfg = config(
        store.clone(),
        Arc::new(ToolThenFailRoute {
            calls: calls.clone(),
        }),
        registry(Arc::new(CountingEchoTool { runs: runs.clone() })),
        CancellationToken::new(),
    );
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<LLMEvent>();
    cfg.event_tx = Some(event_tx);

    run_loop(&cfg, &sid(SES)).await.expect("run completes");

    assert_eq!(runs.load(Ordering::SeqCst), 1, "tool executed exactly once");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "two provider calls");

    drop(cfg);
    let mut salvaged_retry = false;
    while let Some(ev) = event_rx.recv().await {
        if let LLMEvent::Retry { salvaged, .. } = ev {
            salvaged_retry |= salvaged;
        }
    }
    assert!(salvaged_retry, "the retry event is marked salvaged");

    // The failed attempt's assistant is finalized as a tool-calls step and
    // keeps its completed tool part; the follow-up assistant carries the text.
    let msgs = store.list_messages(&sid(SES)).await.expect("messages");
    let assistants: Vec<_> = msgs.iter().filter(|m| m.is_assistant()).collect();
    assert_eq!(assistants.len(), 2, "salvaged step + follow-up step");
    let first = assistants[0].as_assistant().expect("assistant body");
    assert_eq!(first.finish.as_deref(), Some("tool-calls"));
    let first_parts = parts_of(&store, &assistants[0].id).await;
    assert!(
        first_parts.iter().any(|p| matches!(
            &p.kind,
            PartKind::Tool {
                state: ToolState::Completed { .. },
                ..
            }
        )),
        "completed tool part kept on the salvaged step"
    );
}

#[tokio::test]
async fn retry_with_no_tool_work_still_purges_and_replays() {
    // Guard the existing behavior: a failed attempt with NO completed tools
    // must keep using the purge-and-replay path (idempotent in-place retry).
    let (store, _user) = seed("hi").await;
    let mut turn = vec![step_start()];
    turn.extend(text_events("t2", "FINAL"));
    turn.push(step_finish(FinishReason::Stop));
    turn.push(finish(FinishReason::Stop));
    let route = Arc::new(PartialThenCompleteRoute {
        calls: AtomicUsize::new(0),
        success_turn: Mutex::new(Some(turn)),
    });
    let cfg = config(
        store.clone(),
        route,
        registry(Arc::new(EchoTool)),
        CancellationToken::new(),
    );
    run_loop(&cfg, &sid(SES)).await.expect("run completes");
    let msgs = store.list_messages(&sid(SES)).await.expect("messages");
    let assistants: Vec<_> = msgs.iter().filter(|m| m.is_assistant()).collect();
    assert_eq!(assistants.len(), 1, "in-place retry reuses the assistant");
}

// NOTE: not `start_paused` — see `retry_emits_retry_event_before_backoff`.
// `max_retries: 2` keeps each test's real backoff at a single ~4s sleep.
#[tokio::test]
async fn truncated_stream_is_retried_then_succeeds() {
    let (store, _user) = seed("hi").await;

    // Turn 1: text but NO terminal finish (early close / truncation).
    // Turn 2: a complete turn. The truncation must be retried transparently.
    let mut truncated = vec![step_start()];
    truncated.extend(text_events("t1", "PARTIAL"));
    let mut complete = vec![step_start()];
    complete.extend(text_events("t2", "FULL ANSWER"));
    complete.push(step_finish(FinishReason::Stop));
    complete.push(finish(FinishReason::Stop));

    let (route, calls) = ScriptedRoute::build(vec![truncated, complete]);
    let mut cfg = config(
        store.clone(),
        route,
        registry(Arc::new(EchoTool)),
        CancellationToken::new(),
    );
    cfg.max_retries = 2;
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<LLMEvent>();
    cfg.event_tx = Some(event_tx);

    run_loop(&cfg, &sid(SES)).await.expect("run recovers");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    drop(cfg);
    let (mut saw_retry, mut saw_warning) = (false, false);
    while let Some(ev) = event_rx.recv().await {
        match ev {
            LLMEvent::Retry { .. } => saw_retry = true,
            LLMEvent::Warning { .. } => saw_warning = true,
            _ => {}
        }
    }
    assert!(saw_retry, "truncation retried");
    assert!(!saw_warning, "no warning when the retry succeeds");
}

#[tokio::test]
async fn chronic_truncation_accepts_content_with_warning() {
    let (store, _user) = seed("hi").await;

    // EVERY attempt streams content then closes without finish_reason — the
    // shape of a gateway that never sends one. After the budget is spent the
    // run must accept the content with a Warning, not fail the turn.
    struct AlwaysTruncatedRoute;
    impl Route for AlwaysTruncatedRoute {
        fn id(&self) -> &str {
            "always-truncated"
        }
        fn stream(&self, _req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
            let mut events = vec![step_start()];
            events.extend(text_events("t1", "TRUNCATED ANSWER"));
            stream::iter(events.into_iter().map(Ok)).boxed()
        }
    }

    let mut cfg = config(
        store.clone(),
        Arc::new(AlwaysTruncatedRoute),
        registry(Arc::new(EchoTool)),
        CancellationToken::new(),
    );
    cfg.max_retries = 2;
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<LLMEvent>();
    cfg.event_tx = Some(event_tx);

    run_loop(&cfg, &sid(SES))
        .await
        .expect("chronic truncation must not fail the run");

    drop(cfg);
    let mut saw_warning = false;
    while let Some(ev) = event_rx.recv().await {
        if matches!(ev, LLMEvent::Warning { .. }) {
            saw_warning = true;
        }
    }
    assert!(saw_warning, "acceptance is surfaced as a Warning");

    // The accepted assistant carries the streamed text and a terminal finish.
    let msgs = store.list_messages(&sid(SES)).await.expect("messages");
    let assistant = msgs
        .iter()
        .rev()
        .find(|m| m.is_assistant())
        .expect("assistant");
    let a = assistant.as_assistant().expect("assistant body");
    assert_eq!(a.finish.as_deref(), Some("unknown"));
    let parts = parts_of(&store, &assistant.id).await;
    let text: String = parts
        .iter()
        .filter_map(|p| match &p.kind {
            PartKind::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(text.contains("TRUNCATED ANSWER"), "content kept: {text:?}");
}

/// A [`Route`] whose first `stream()` call persists partial text then fails
/// mid-stream with a retryable 429 (zero backoff), and whose second call
/// streams a complete turn — for exercising the retry loop's part-purge (Fix 5).
struct PartialThenCompleteRoute {
    calls: AtomicUsize,
    success_turn: Mutex<Option<Vec<LLMEvent>>>,
}

impl Route for PartialThenCompleteRoute {
    fn id(&self) -> &str {
        "partial-then-complete"
    }

    fn stream(&self, _req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            // Emit real text content (persisted as a part) then fail mid-stream.
            let mut items: Vec<Result<LLMEvent, LLMError>> = vec![
                Ok(step_start()),
                Ok(LLMEvent::TextStart {
                    id: "t1".into(),
                    provider_metadata: None,
                }),
            ];
            items.push(Ok(LLMEvent::TextDelta {
                id: "t1".into(),
                text: "PARTIAL-ATTEMPT-1".into(),
                provider_metadata: None,
            }));
            items.push(Err(LLMError::Http {
                status: 429,
                message: "rate limit".into(),
                // Zero backoff so the test does not eat the real ~4s sleep.
                retry_after: Some(Duration::from_millis(0)),
            }));
            stream::iter(items).boxed()
        } else {
            let events = self.success_turn.lock().unwrap().take().unwrap_or_default();
            stream::iter(events.into_iter().map(Ok)).boxed()
        }
    }
}

#[tokio::test]
async fn retry_purges_partial_parts_no_duplicates() {
    let (store, _user) = seed("hi").await;

    // The successful second attempt: a single complete text turn.
    let mut turn = vec![step_start()];
    turn.extend(text_events("t2", "FINAL-ANSWER"));
    turn.push(step_finish(FinishReason::Stop));
    turn.push(finish(FinishReason::Stop));

    let route: Arc<dyn Route> = Arc::new(PartialThenCompleteRoute {
        calls: AtomicUsize::new(0),
        success_turn: Mutex::new(Some(turn)),
    });
    let cfg = config(
        store.clone(),
        route,
        registry(Arc::new(EchoTool)),
        CancellationToken::new(),
    );

    let last = run_loop(&cfg, &sid(SES))
        .await
        .expect("run_loop recovers after one retry");

    // Exactly one text part survives: the purge erased attempt 1's partial part
    // before attempt 2 re-streamed, so no duplicate.
    let texts: Vec<String> = parts_of(&store, last.id())
        .await
        .into_iter()
        .filter_map(|p| match p.kind {
            PartKind::Text { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec!["FINAL-ANSWER".to_string()],
        "no duplicate parts"
    );
}

#[tokio::test]
async fn abort_under_no_terminal_finish_finalizes_gracefully() {
    let (store, _user) = seed("do the thing").await;

    // A turn that emits content but ends with NO terminal `Finish` — normally
    // Fix 4 surfaces `NoTerminalFinish`. With the turn aborted, Fix 4c must
    // finalize gracefully instead of surfacing the error.
    let mut turn1 = vec![step_start()];
    turn1.extend(text_events("t1", "partial output"));
    // no step_finish / finish → truncated stream.

    let (route, calls) = ScriptedRoute::build(vec![turn1]);
    // Pre-cancel: the retry arm's abort check must break gracefully.
    let abort = CancellationToken::new();
    abort.cancel();
    let cfg = config(store.clone(), route, registry(Arc::new(EchoTool)), abort);

    let last = run_loop(&cfg, &sid(SES))
        .await
        .expect("aborted NoTerminalFinish must finalize gracefully, not error");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "no retry after abort");

    let assistant = store
        .get_message(&sid(SES), last.id())
        .await
        .expect("get")
        .expect("some");
    assert!(
        assistant.as_assistant().unwrap().time.completed.is_some(),
        "assistant finalized on graceful interrupt"
    );
}

// -- live-tap decoupling ------------------------------------------------------

/// The live event tap must deliver events to `event_tx` the moment they arrive
/// from the provider — NOT lazily when the downstream consumer (the processor,
/// which awaits a store write per event) polls the stream. A stalled consumer
/// must not stall client-visible streaming.
#[tokio::test]
async fn tap_events_delivers_without_consumer_polling() {
    let (src_tx, src_rx) = tokio::sync::mpsc::unbounded_channel::<Result<LLMEvent, LLMError>>();
    let src = futures::stream::unfold(src_rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
    .boxed();

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<LLMEvent>();
    let mut tapped = otto_session::tap_events(src, event_tx);

    // Provider produces three deltas; the consumer never polls `tapped`
    // (simulating a processor stuck on a slow store write).
    for i in 0..3 {
        src_tx
            .send(Ok(LLMEvent::TextDelta {
                id: "t1".into(),
                text: format!("chunk-{i}"),
                provider_metadata: None,
            }))
            .expect("send");
    }

    for i in 0..3 {
        let ev = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("event {i} not delivered while consumer idle"))
            .expect("channel open");
        assert!(matches!(ev, LLMEvent::TextDelta { .. }));
    }

    // The consumer still receives every item, in order, once it does poll.
    drop(src_tx);
    let mut texts = Vec::new();
    while let Some(item) = tapped.next().await {
        if let Ok(LLMEvent::TextDelta { text, .. }) = item {
            texts.push(text);
        }
    }
    assert_eq!(texts, vec!["chunk-0", "chunk-1", "chunk-2"]);
}
