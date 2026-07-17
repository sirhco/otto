//! Headless integration test for the `run` flow.
//!
//! A scripted [`Route`] + factory drives [`Runtime::in_memory`] with no network,
//! and a scripted [`PermissionResponder`] answers any permission asks. The
//! rendered output is captured through a shared buffer and asserted on.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use otto_agent::ModelRef;
use otto_app::{Result as AppResult, Runtime};
use otto_cli::run::{PermissionResponder, QuestionResponder, RunRequest, run_session};
use otto_config::Config;
use otto_events::{FinishReason, LLMEvent};
use otto_llm::{LLMError, LLMRequest, Model, Route};
use otto_permission::{Asked, Reply};
use otto_tools::{ExecuteResult, Tool, ToolContext, ToolError, ToolRegistry};
use serde_json::{Value, json};
use std::io::{self, Write};
use tokio_util::sync::CancellationToken;

// -- scripted route + factory ------------------------------------------------

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

// -- shared-buffer writer ----------------------------------------------------

/// A [`Write`] that appends into a shared buffer we can read after the run.
#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// -- scripted permission responder -------------------------------------------

/// Records asks and answers each with a fixed [`Reply`].
struct ScriptedResponder {
    reply: Reply,
    seen: Arc<AtomicUsize>,
}

impl PermissionResponder for ScriptedResponder {
    fn respond(&self, _asked: &Asked) -> Reply {
        self.seen.fetch_add(1, Ordering::SeqCst);
        self.reply.clone()
    }
}

// -- always-cancel question responder ----------------------------------------

/// A [`QuestionResponder`] test double for tests that don't exercise the
/// question tool: cancels every ask.
struct AlwaysCancelResponder;

impl QuestionResponder for AlwaysCancelResponder {
    fn respond(&self, _asked: &otto_question::Asked) -> otto_tools::QuestionOutcome {
        otto_tools::QuestionOutcome::Cancelled
    }
}

// -- a tool that asks for permission -----------------------------------------

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
            .ask(otto_tools::PermissionRequest {
                permission: "danger".into(),
                patterns: vec!["x".into()],
                always: vec![],
                metadata: json!({}),
            })
            .await?;
        Ok(ExecuteResult::new("guard", "ran"))
    }
}

// -- event helpers -----------------------------------------------------------

fn text_turn(id: &str, text: &str) -> Vec<LLMEvent> {
    vec![
        LLMEvent::StepStart { index: 0 },
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
        LLMEvent::StepFinish {
            index: 0,
            reason: FinishReason::Stop,
            usage: None,
            provider_metadata: None,
        },
        LLMEvent::Finish {
            reason: FinishReason::Stop,
            usage: None,
            provider_metadata: None,
        },
    ]
}

// -- tests -------------------------------------------------------------------

#[tokio::test]
async fn run_session_renders_scripted_assistant_text() {
    let (factory, calls) = ScriptedRouteFactory::new(vec![text_turn("t1", "the answer is 42")]);
    let runtime = Runtime::in_memory(Config::default())
        .await
        .expect("runtime")
        .with_route_factory(factory);

    let agent = runtime.default_agent().clone();
    let model = runtime.default_model();

    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let seen = Arc::new(AtomicUsize::new(0));
    let responder = Arc::new(ScriptedResponder {
        reply: Reply::Once,
        seen: seen.clone(),
    });

    run_session(
        &runtime,
        RunRequest {
            prompt: "what is the answer".into(),
            agent,
            model,
            session_id: None,
        },
        buf.clone(),
        false,
        responder,
        Arc::new(AlwaysCancelResponder),
        CancellationToken::new(),
    )
    .await
    .expect("run ok");

    assert_eq!(calls.load(Ordering::SeqCst), 1, "one provider turn");
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains("the answer is 42"), "rendered text: {out:?}");
}

#[tokio::test]
async fn run_session_answers_permission_via_responder() {
    // Turn 1 calls the guarded tool; turn 2 wraps up.
    let mut turn1 = vec![LLMEvent::StepStart { index: 0 }];
    turn1.push(LLMEvent::ToolCall {
        id: "c1".into(),
        name: "guard".into(),
        input: json!({}),
        provider_executed: None,
        provider_metadata: None,
    });
    turn1.push(LLMEvent::StepFinish {
        index: 0,
        reason: FinishReason::ToolCalls,
        usage: None,
        provider_metadata: None,
    });
    turn1.push(LLMEvent::Finish {
        reason: FinishReason::ToolCalls,
        usage: None,
        provider_metadata: None,
    });

    let (factory, _calls) = ScriptedRouteFactory::new(vec![turn1, text_turn("t2", "done")]);

    // The `danger` permission requires an ask (not pre-allowed / pre-denied).
    let config = Config {
        permission: Some(json!({ "danger": "ask" })),
        ..Config::default()
    };

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(GuardTool));

    let runtime = Runtime::in_memory(config)
        .await
        .expect("runtime")
        .with_route_factory(factory)
        .with_tools(Arc::new(registry));

    let agent = runtime.default_agent().clone();
    let model = runtime.default_model();

    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let seen = Arc::new(AtomicUsize::new(0));
    let responder = Arc::new(ScriptedResponder {
        reply: Reply::Once,
        seen: seen.clone(),
    });

    run_session(
        &runtime,
        RunRequest {
            prompt: "do danger".into(),
            agent,
            model,
            session_id: None,
        },
        buf.clone(),
        false,
        responder,
        Arc::new(AlwaysCancelResponder),
        CancellationToken::new(),
    )
    .await
    .expect("run ok");

    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "the responder answered one ask"
    );
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains("done"), "final text rendered: {out:?}");
    assert!(out.contains("⏺ guard"), "tool marker rendered: {out:?}");
}
