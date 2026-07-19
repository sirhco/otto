//! Integration tests for the `otto-app` [`Runtime`].
//!
//! A [`ScriptedRoute`] + [`ScriptedRouteFactory`] drive the whole assembly
//! headless (no network); [`Runtime::in_memory`] + [`Runtime::with_route_factory`]
//! / [`Runtime::with_tools`] inject the scripted seams.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use otto_agent::ModelRef;
use otto_app::{AuthRouteFactory, Result as AppResult, Runtime};
use otto_auth::{AuthMap, Credential};
use otto_config::Config;
use otto_events::{FinishReason, LLMEvent};
use otto_llm::{HttpTransport, LLMError, LLMRequest, Model, Route};
use otto_storage::model::{PartKind, SessionId, ToolState};
use otto_tools::{ExecuteResult, PermissionRequest, Tool, ToolContext, ToolError, ToolRegistry};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// -- scripted route + factory ------------------------------------------------

/// A [`Route`] that returns a canned event stream per `stream()` call.
struct ScriptedRoute {
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

/// A [`RouteFactory`] handing back one shared [`ScriptedRoute`] for every model.
struct ScriptedRouteFactory {
    route: Arc<ScriptedRoute>,
    model: Model,
}

impl ScriptedRouteFactory {
    fn new(turns: Vec<Vec<LLMEvent>>) -> (Arc<Self>, Arc<AtomicUsize>) {
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

/// A route that captures every request's assembled system-prompt text, for
/// asserting hook-injected context reaches the real LLM request. Kept
/// separate from `ScriptedRoute` (used by many other tests in this file) so
/// this addition can't affect them.
struct CapturingRoute {
    system_texts: Mutex<Vec<String>>,
    turns: Mutex<VecDeque<Vec<LLMEvent>>>,
}

impl Route for CapturingRoute {
    fn id(&self) -> &str {
        "capturing"
    }
    fn stream(&self, req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
        let joined = req
            .system
            .iter()
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        self.system_texts.lock().unwrap().push(joined);
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        stream::iter(events.into_iter().map(Ok)).boxed()
    }
}

struct CapturingRouteFactory {
    route: Arc<CapturingRoute>,
    model: Model,
}

impl otto_app::RouteFactory for CapturingRouteFactory {
    fn route_for(&self, _m: &ModelRef) -> AppResult<(Arc<dyn Route>, Model)> {
        Ok((self.route.clone(), self.model.clone()))
    }
}

// -- tools -------------------------------------------------------------------

/// Echoes its `text` argument back as output.
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

/// Asks the permission gate for the `danger` permission; denied → tool error.
struct GuardTool;

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
                always: vec![],
                metadata: json!({}),
            })
            .await?;
        Ok(ExecuteResult::new("guard", "ran"))
    }
}

// -- scripted event helpers --------------------------------------------------

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

/// A one-turn "say `text`, then stop" script.
fn text_turn(id: &str, text: &str) -> Vec<LLMEvent> {
    let mut turn = vec![step_start()];
    turn.extend(text_events(id, text));
    turn.push(step_finish(FinishReason::Stop));
    turn.push(finish(FinishReason::Stop));
    turn
}

async fn assistant_text(rt: &Runtime, session: &SessionId) -> Option<String> {
    let messages = rt.store().list_messages(session).await.expect("messages");
    for m in messages.iter().rev() {
        if !m.is_assistant() {
            continue;
        }
        for p in rt.store().list_parts(m.id()).await.expect("parts") {
            if let PartKind::Text { text, .. } = p.kind {
                return Some(text);
            }
        }
    }
    None
}

async fn tool_state(rt: &Runtime, session: &SessionId, tool_id: &str) -> Option<ToolState> {
    for m in rt.store().list_messages(session).await.expect("messages") {
        for p in rt.store().list_parts(m.id()).await.expect("parts") {
            if let PartKind::Tool { tool, state, .. } = &p.kind
                && tool == tool_id
            {
                return Some(state.clone());
            }
        }
    }
    None
}

// -- tests -------------------------------------------------------------------

/// `Runtime::in_memory` installs the embedded models.dev snapshot (no
/// network) so provider/model lookups resolve immediately after boot.
#[tokio::test]
async fn in_memory_installs_models_dev_registry() {
    let _rt = Runtime::in_memory(Config::default())
        .await
        .expect("runtime");

    assert!(
        !otto_llm::registry::current().is_empty(),
        "registry populated after Runtime::in_memory"
    );
    assert!(
        otto_llm::registry::current()
            .all_models()
            .any(|m| m.provider.0 == "anthropic"),
        "some anthropic model resolves"
    );
}

#[tokio::test]
async fn assembles_in_memory() {
    let rt = Runtime::in_memory(Config::default())
        .await
        .expect("runtime");

    // Agents resolved, builtins registered, default model + agent available.
    assert!(rt.agents().iter().any(|a| a.name == "build"));
    assert!(rt.tools().get("read").is_some());
    assert_eq!(
        rt.default_model(),
        ModelRef::parse("anthropic/claude-sonnet-4-5")
    );
    assert_eq!(rt.default_agent().name, "build");
    // The permission service is present (a fresh session has no pending asks).
    assert!(rt.permission().list_pending().is_empty());
}

#[tokio::test]
async fn config_hooks_reach_the_tool_registry_and_block_a_matching_call() {
    // `bash` is a real built-in `Runtime::in_memory` registers by default (no
    // `.with_tools()` override — that would bypass the very Config -> Runtime
    // -> registry hook wiring this test exists to prove). Denying it via
    // `PreToolUse` is safe: the command is never actually executed.
    let config = Config {
        hooks: Some(otto_hooks::HooksConfig {
            pre_tool_use: vec![otto_hooks::HookMatcherGroup {
                matcher: Some("bash".to_string()),
                hooks: vec![otto_hooks::HookCommand {
                    command: "echo '{\"decision\":\"deny\",\"reason\":\"blocked in test\"}'"
                        .to_string(),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    let runtime = Runtime::in_memory(config).await.unwrap();

    let ctx = ToolContext::builder(std::env::temp_dir()).build();
    let err = runtime
        .tools()
        .execute("bash", json!({ "command": "echo hi" }), &ctx)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "blocked in test");
}

#[tokio::test]
async fn config_without_hooks_leaves_tool_calls_unaffected() {
    let config = Config::default();
    let runtime = Runtime::in_memory(config).await.unwrap();

    let ctx = ToolContext::builder(std::env::temp_dir()).build();
    let res = runtime
        .tools()
        .execute("bash", json!({ "command": "true" }), &ctx)
        .await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn run_yields_events_and_persists_info() {
    let (factory, calls) = ScriptedRouteFactory::new(vec![text_turn("t1", "done")]);
    let rt = Runtime::in_memory(Config::default())
        .await
        .expect("runtime")
        .with_route_factory(factory);

    let agent = rt.default_agent().clone();
    let model = rt.default_model();
    let session = rt
        .create_session("Test", &agent, None)
        .await
        .expect("session");

    let mut handle = rt.run(&session, "hi", &agent, &model, CancellationToken::new());
    let info = handle.join.await.expect("join").expect("run ok");

    assert_eq!(calls.load(Ordering::SeqCst), 1, "one provider turn");

    // The tap surfaced the streamed events.
    let mut events = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        events.push(ev);
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LLMEvent::TextDelta { text, .. } if text == "done")),
        "text-delta tapped: {events:?}"
    );

    // The final Info is the newest assistant message, and its text persisted.
    assert!(info.is_assistant());
    assert_eq!(assistant_text(&rt, &session).await.as_deref(), Some("done"));
}

#[tokio::test]
async fn run_drives_a_tool_cycle() {
    // Turn 1: call echo; turn 2: wrap up.
    let mut turn1 = vec![step_start()];
    turn1.extend(text_events("t1", "calling"));
    turn1.push(tool_call("call_1", "echo", json!({ "text": "hi" })));
    turn1.push(step_finish(FinishReason::ToolCalls));
    turn1.push(finish(FinishReason::ToolCalls));

    let (factory, calls) = ScriptedRouteFactory::new(vec![turn1, text_turn("t2", "done")]);

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));

    let rt = Runtime::in_memory(Config::default())
        .await
        .expect("runtime")
        .with_route_factory(factory)
        .with_tools(Arc::new(registry));

    let agent = rt.default_agent().clone();
    let model = rt.default_model();
    let session = rt
        .create_session("Tool", &agent, None)
        .await
        .expect("session");

    let handle = rt.run(
        &session,
        "echo hi",
        &agent,
        &model,
        CancellationToken::new(),
    );
    handle.join.await.expect("join").expect("run ok");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "tool call forced a 2nd turn"
    );

    // The echo tool ran and its result persisted.
    match tool_state(&rt, &session, "echo").await {
        Some(ToolState::Completed { output, .. }) => assert_eq!(output, "hi"),
        other => panic!("expected completed echo, got {other:?}"),
    }
    assert_eq!(assistant_text(&rt, &session).await.as_deref(), Some("done"));
}

#[tokio::test]
async fn auth_route_factory_selects_provider() {
    let mut auth = AuthMap::new();
    auth.insert(
        "anthropic".into(),
        Credential::Api {
            key: "sk-test".into(),
            metadata: None,
        },
    );
    let factory = AuthRouteFactory::new(auth, Arc::new(HttpTransport::new()), Default::default());

    // Anthropic + a stored Api key → native Anthropic route.
    let (route, model) =
        otto_app::RouteFactory::route_for(&factory, &ModelRef::parse("anthropic/claude-sonnet-4"))
            .expect("route");
    assert_eq!(route.id(), "anthropic");
    assert_eq!(model.route_id, "anthropic");

    // Unknown provider → OpenAI-compatible route.
    let (route, model) =
        otto_app::RouteFactory::route_for(&factory, &ModelRef::parse("weirdprov/some-model"))
            .expect("route");
    assert_eq!(route.id(), "openai-compatible-chat");
    assert_eq!(model.route_id, "openai-compatible-chat");
}

/// `Runtime::subagent_spawner` wires the same private services
/// (store/tools/permission/route_factory/project_id/version) `run_with_parts`
/// builds inline — proving the extraction reaches everything it needs.
#[tokio::test]
async fn subagent_spawner_builds_from_runtime() {
    let rt = Runtime::in_memory(Config::default())
        .await
        .expect("runtime");

    let agent = rt.default_agent().clone();
    let model = rt.default_model();
    let spawner = rt.subagent_spawner(&agent, &model);
    assert!(spawner.is_ok());
}

#[tokio::test]
async fn deny_ruleset_blocks_a_tool() {
    // Turn 1: call the guarded tool; turn 2: wrap up.
    let mut turn1 = vec![step_start()];
    turn1.push(tool_call("call_1", "guard", json!({})));
    turn1.push(step_finish(FinishReason::ToolCalls));
    turn1.push(finish(FinishReason::ToolCalls));

    let (factory, _calls) = ScriptedRouteFactory::new(vec![turn1, text_turn("t2", "done")]);

    // Config denies the `danger` permission the guard tool asks for.
    let config = Config {
        permission: Some(json!({ "danger": "deny" })),
        ..Config::default()
    };

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(GuardTool));

    let rt = Runtime::in_memory(config)
        .await
        .expect("runtime")
        .with_route_factory(factory)
        .with_tools(Arc::new(registry));

    let agent = rt.default_agent().clone();
    let model = rt.default_model();
    let session = rt
        .create_session("Deny", &agent, None)
        .await
        .expect("session");

    let handle = rt.run(
        &session,
        "do danger",
        &agent,
        &model,
        CancellationToken::new(),
    );
    handle.join.await.expect("join").expect("run ok");

    // The denied tool was recorded as an error, never completed.
    match tool_state(&rt, &session, "guard").await {
        Some(ToolState::Error { .. }) => {}
        other => panic!("expected denied guard error, got {other:?}"),
    }
}

// -- turn timeout --------------------------------------------------------------

/// A [`Route`] whose stream never yields — the shape of a provider that
/// accepts the request and then hangs forever.
struct HangingRoute;
impl Route for HangingRoute {
    fn id(&self) -> &str {
        "hanging"
    }
    fn stream(&self, _req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
        stream::pending().boxed()
    }
}

struct HangingRouteFactory;
impl otto_app::RouteFactory for HangingRouteFactory {
    fn route_for(&self, _m: &ModelRef) -> AppResult<(Arc<dyn Route>, Model)> {
        Ok((
            Arc::new(HangingRoute),
            Model::new("anthropic", "claude-3", "route_hanging"),
        ))
    }
}

#[tokio::test]
async fn turn_timeout_aborts_a_hung_run() {
    // `retry.turn_timeout_seconds = 1`: the watchdog must cancel the run's
    // abort token, ending the turn through the graceful-interrupt path
    // instead of hanging forever on a silent provider.
    let config = Config {
        retry: Some(otto_config::Retry {
            turn_timeout_seconds: Some(1),
            ..otto_config::Retry::default()
        }),
        ..Config::default()
    };
    let rt = Runtime::in_memory(config)
        .await
        .expect("runtime")
        .with_route_factory(Arc::new(HangingRouteFactory));

    let agent = rt.default_agent().clone();
    let model = rt.default_model();
    let session = rt
        .create_session("Timeout", &agent, None)
        .await
        .expect("session");

    let handle = rt.run(&session, "hang", &agent, &model, CancellationToken::new());
    let joined = tokio::time::timeout(std::time::Duration::from_secs(10), handle.join)
        .await
        .expect("run must end well before 10s — the 1s turn timeout fires");
    let info = joined.expect("join").expect("graceful interrupt, not an error");
    let a = info.as_assistant().expect("assistant");
    assert!(
        a.time.completed.is_some(),
        "aborted assistant is finalized"
    );
}

// -- hook wiring end-to-end ---------------------------------------------------

#[tokio::test]
async fn session_start_hook_context_reaches_the_system_prompt() {
    let config = Config {
        hooks: Some(otto_hooks::HooksConfig {
            session_start: vec![otto_hooks::HookMatcherGroup {
                matcher: None,
                hooks: vec![otto_hooks::HookCommand {
                    command: "echo '{\"additional_context\":\"REMEMBER: be terse\"}'".to_string(),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    let route = Arc::new(CapturingRoute {
        system_texts: Mutex::new(Vec::new()),
        turns: Mutex::new(vec![text_turn("t1", "done")].into_iter().collect()),
    });
    let factory = Arc::new(CapturingRouteFactory {
        route: route.clone(),
        model: Model::new("anthropic", "claude-3", "route_scripted"),
    });
    let rt = Runtime::in_memory(config)
        .await
        .expect("runtime")
        .with_route_factory(factory);

    let agent = rt.default_agent().clone();
    let model = rt.default_model();
    let session = rt
        .create_session("Test", &agent, None)
        .await
        .expect("session");

    let handle = rt.run(&session, "hi", &agent, &model, CancellationToken::new());
    handle.join.await.expect("join").expect("run ok");

    let texts = route.system_texts.lock().unwrap();
    assert!(
        texts.iter().any(|t| t.contains("REMEMBER: be terse")),
        "SessionStart's additional_context reached the system prompt: {texts:?}"
    );
}

#[tokio::test]
async fn user_prompt_submit_deny_blocks_before_any_provider_call() {
    let config = Config {
        hooks: Some(otto_hooks::HooksConfig {
            user_prompt_submit: vec![otto_hooks::HookMatcherGroup {
                matcher: None,
                hooks: vec![otto_hooks::HookCommand {
                    command: "echo '{\"decision\":\"deny\",\"reason\":\"blocked prompt\"}'"
                        .to_string(),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    let (factory, calls) = ScriptedRouteFactory::new(vec![text_turn("t1", "done")]);
    let rt = Runtime::in_memory(config)
        .await
        .expect("runtime")
        .with_route_factory(factory);

    let agent = rt.default_agent().clone();
    let model = rt.default_model();
    let session = rt
        .create_session("Test", &agent, None)
        .await
        .expect("session");

    let handle = rt.run(&session, "hi", &agent, &model, CancellationToken::new());
    let err = handle.join.await.expect("join").expect_err("denied");

    assert_eq!(err.to_string(), "blocked prompt");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no provider call made");
}
