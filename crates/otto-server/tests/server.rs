//! Integration tests for `otto-server`.
//!
//! A scripted [`RouteFactory`] + in-memory [`Runtime`] drive the whole HTTP/SSE
//! surface headless (no network to a provider); the server itself is bound to
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
use otto_tools::{
    ExecuteResult, PermissionRequest, QuestionOption, QuestionOutcome, QuestionPrompt, Tool,
    ToolContext, ToolError, ToolRegistry,
};
use serde_json::{Value, json};

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

// -- guard tool (asks permission) --------------------------------------------

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
                always: vec!["x".into()],
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
fn text_turn(id: &str, text: &str) -> Vec<LLMEvent> {
    let mut turn = vec![step_start()];
    turn.extend(text_events(id, text));
    turn.push(step_finish(FinishReason::Stop));
    turn.push(finish(FinishReason::Stop));
    turn
}

// -- harness -----------------------------------------------------------------

/// Bind the server on an ephemeral port and return its base URL.
async fn spawn(runtime: Arc<Runtime>, opts: ServeOptions) -> String {
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

fn no_auth() -> ServeOptions {
    ServeOptions {
        password: None,
        cors: false,
    }
}

async fn plain_runtime() -> Arc<Runtime> {
    let (factory, _) = ScriptedRouteFactory::new(vec![]);
    Arc::new(
        Runtime::in_memory(Config::default())
            .await
            .unwrap()
            .with_route_factory(factory),
    )
}

// -- tests -------------------------------------------------------------------

#[tokio::test]
async fn session_crud() {
    let base = spawn(plain_runtime().await, no_auth()).await;
    let http = reqwest::Client::new();

    // Create.
    let created: Value = http
        .post(format!("{base}/session"))
        .json(&json!({ "title": "Hello" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("ses_"), "session id: {id}");

    // List contains it.
    let list: Value = http
        .get(format!("{base}/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == created["id"]),
        "list has created session"
    );

    // Get by id.
    let got: Value = http
        .get(format!("{base}/session/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["id"], created["id"]);

    // Missing -> 404.
    let missing = http
        .get(format!("{base}/session/ses_does_not_exist"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn prompt_streams_sse_and_persists() {
    let (factory, calls) = ScriptedRouteFactory::new(vec![text_turn("t1", "done")]);
    let runtime = Arc::new(
        Runtime::in_memory(Config::default())
            .await
            .unwrap()
            .with_route_factory(factory),
    );
    let base = spawn(runtime, no_auth()).await;
    let http = reqwest::Client::new();

    let created: Value = http
        .post(format!("{base}/session"))
        .json(&json!({ "title": "Chat" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let resp = http
        .post(format!("{base}/session/{id}/message"))
        .json(&json!({ "parts": [{ "type": "text", "text": "hi" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
        "text/event-stream"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("text-delta"), "sse body: {body}");
    assert!(body.contains("\"text\":\"done\""), "sse body: {body}");
    assert!(body.contains("\"type\":\"finish\""), "sse body: {body}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Assistant text persisted.
    let msgs: Value = http
        .get(format!("{base}/session/{id}/message"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dump = msgs.to_string();
    assert!(dump.contains("done"), "messages: {dump}");
}

#[tokio::test]
async fn agent_and_config() {
    let base = spawn(plain_runtime().await, no_auth()).await;
    let http = reqwest::Client::new();

    let agents: Value = http
        .get(format!("{base}/agent"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        agents
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "build"),
        "agents: {agents}"
    );

    let cfg = http.get(format!("{base}/config")).send().await.unwrap();
    assert_eq!(cfg.status(), 200);
    let cfg: Value = cfg.json().await.unwrap();
    assert!(cfg.is_object(), "config: {cfg}");
}

#[tokio::test]
async fn provider_and_doc() {
    let base = spawn(plain_runtime().await, no_auth()).await;
    let http = reqwest::Client::new();

    let prov = http.get(format!("{base}/provider")).send().await.unwrap();
    assert_eq!(prov.status(), 200);

    let doc = http.get(format!("{base}/doc")).send().await.unwrap();
    assert_eq!(doc.status(), 200);
    let doc: Value = doc.json().await.unwrap();
    assert!(doc.get("paths").is_some(), "doc: {doc}");
}

#[tokio::test]
async fn permission_flow() {
    // Turn 1 calls the guard tool (which asks for "danger" -> blocks on Ask);
    // turn 2 wraps up once the permission is granted.
    let mut turn1 = vec![step_start()];
    turn1.push(tool_call("call_1", "guard", json!({})));
    turn1.push(step_finish(FinishReason::ToolCalls));
    turn1.push(finish(FinishReason::ToolCalls));
    let (factory, _calls) = ScriptedRouteFactory::new(vec![turn1, text_turn("t2", "done")]);

    let config = Config {
        permission: Some(json!({ "danger": "ask" })),
        ..Config::default()
    };
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(GuardTool));
    let runtime = Arc::new(
        Runtime::in_memory(config)
            .await
            .unwrap()
            .with_route_factory(factory)
            .with_tools(Arc::new(registry)),
    );
    let base = spawn(runtime, no_auth()).await;
    let http = reqwest::Client::new();

    let created: Value = http
        .post(format!("{base}/session"))
        .json(&json!({ "title": "Perm" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // Initially no pending permissions.
    let pending: Value = http
        .get(format!("{base}/permission"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(pending.as_array().unwrap().is_empty());

    // Fire the prompt in the background; it will block awaiting the permission.
    let prompt = {
        let http = http.clone();
        let base = base.clone();
        let id = id.clone();
        tokio::spawn(async move {
            http.post(format!("{base}/session/{id}/message"))
                .json(&json!({ "prompt": "do danger" }))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        })
    };

    // Poll until the request shows up as pending.
    let mut request_id = None;
    for _ in 0..100 {
        let pending: Value = http
            .get(format!("{base}/permission"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(first) = pending.as_array().and_then(|a| a.first()) {
            request_id = Some(first["id"].as_str().unwrap().to_string());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let request_id = request_id.expect("a permission request should be pending");

    // Reply once -> unblocks the run.
    let replied = http
        .post(format!("{base}/permission/{request_id}/reply"))
        .json(&json!({ "reply": "once" }))
        .send()
        .await
        .unwrap();
    assert_eq!(replied.status(), 200);

    let body = prompt.await.unwrap();
    assert!(body.contains("done"), "stream body: {body}");
}

#[tokio::test]
async fn session_busy_reflects_an_in_flight_turn() {
    let mut turn1 = vec![step_start()];
    turn1.push(tool_call("call_1", "guard", json!({})));
    turn1.push(step_finish(FinishReason::ToolCalls));
    turn1.push(finish(FinishReason::ToolCalls));
    let (factory, _calls) = ScriptedRouteFactory::new(vec![turn1, text_turn("t2", "done")]);

    let config = Config {
        permission: Some(json!({ "danger": "ask" })),
        ..Config::default()
    };
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(GuardTool));
    let runtime = Arc::new(
        Runtime::in_memory(config)
            .await
            .unwrap()
            .with_route_factory(factory)
            .with_tools(Arc::new(registry)),
    );
    let base = spawn(runtime, no_auth()).await;
    let http = reqwest::Client::new();

    let created: Value = http
        .post(format!("{base}/session"))
        .json(&json!({ "title": "Busy" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    fn busy_of(sessions: &Value, id: &str) -> bool {
        sessions
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .and_then(|s| s["busy"].as_bool())
            .unwrap_or(false)
    }

    let sessions: Value = http
        .get(format!("{base}/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!busy_of(&sessions, &id), "not busy before any turn starts");

    let prompt = {
        let http = http.clone();
        let base = base.clone();
        let id = id.clone();
        tokio::spawn(async move {
            http.post(format!("{base}/session/{id}/message"))
                .json(&json!({ "prompt": "do danger" }))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        })
    };

    let mut saw_busy = false;
    for _ in 0..100 {
        let sessions: Value = http
            .get(format!("{base}/session"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if busy_of(&sessions, &id) {
            saw_busy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw_busy,
        "session should report busy while the turn is in flight"
    );

    let pending: Value = http
        .get(format!("{base}/permission"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let request_id = pending.as_array().unwrap().first().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    http.post(format!("{base}/permission/{request_id}/reply"))
        .json(&json!({ "reply": "once" }))
        .send()
        .await
        .unwrap();
    prompt.await.unwrap();

    let mut cleared = false;
    for _ in 0..100 {
        let sessions: Value = http
            .get(format!("{base}/session"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if !busy_of(&sessions, &id) {
            cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        cleared,
        "session should report idle again once the turn ends"
    );
}

#[tokio::test]
async fn permission_mode_route_sets_mode_and_rejects_unknown() {
    let runtime = plain_runtime().await;
    let base = spawn(runtime.clone(), no_auth()).await;
    let http = reqwest::Client::new();

    let created: Value = http
        .post(format!("{base}/session"))
        .json(&json!({ "title": "Mode" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // Valid mode -> 200, and the runtime's permission service reflects it.
    let resp = http
        .post(format!("{base}/session/{id}/permission-mode"))
        .json(&json!({ "mode": "full-auto" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        runtime.permission().mode(&id),
        otto_permission::PermissionMode::FullAuto
    );

    // Unknown mode -> 400, and the previously-set mode is unchanged.
    let resp = http
        .post(format!("{base}/session/{id}/permission-mode"))
        .json(&json!({ "mode": "nope" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(
        runtime.permission().mode(&id),
        otto_permission::PermissionMode::FullAuto
    );
}

#[tokio::test]
async fn session_cancel_route_reports_no_live_turn() {
    let runtime = plain_runtime().await;
    let base = spawn(runtime, no_auth()).await;
    let http = reqwest::Client::new();

    let created: Value = http
        .post(format!("{base}/session"))
        .json(&json!({ "title": "Cancel" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // No turn in flight for this session -> 200 with cancelled:false.
    let resp = http
        .post(format!("{base}/session/{id}/cancel"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["cancelled"], json!(false));
}

#[tokio::test]
async fn permission_mode_change_emits_sse_frame() {
    let runtime = plain_runtime().await;
    let base = spawn(runtime, no_auth()).await;
    let http = reqwest::Client::new();

    let created: Value = http
        .post(format!("{base}/session"))
        .json(&json!({ "title": "ModeSSE" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // Open the /event stream, then flip the mode and confirm a
    // `permission.mode_changed` frame with the session id + mode arrives.
    let event_resp = http.get(format!("{base}/event")).send().await.unwrap();
    let mut body = event_resp.bytes_stream();
    // Drain the initial `server.connected` frame.
    let _ = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .expect("initial frame within 2s");

    let set = http
        .post(format!("{base}/session/{id}/permission-mode"))
        .json(&json!({ "mode": "accept-edits" }))
        .send()
        .await
        .unwrap();
    assert_eq!(set.status(), 200);

    let frame = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .expect("a mode_changed frame within 2s")
        .expect("a chunk")
        .expect("chunk ok");
    let text = String::from_utf8_lossy(&frame);
    assert!(text.contains("permission.mode_changed"), "frame: {text}");
    assert!(text.contains(&id), "frame: {text}");
    assert!(text.contains("accept-edits"), "frame: {text}");
}

// -- question ------------------------------------------------------------

fn one_question() -> QuestionPrompt {
    QuestionPrompt {
        question: "Which color?".into(),
        header: "color".into(),
        options: vec![
            QuestionOption {
                label: "Red".into(),
                description: "the color red".into(),
            },
            QuestionOption {
                label: "Blue".into(),
                description: "the color blue".into(),
            },
        ],
        multiple: false,
    }
}

#[tokio::test]
async fn question_list_and_reply_answered() {
    let runtime = plain_runtime().await;
    let base = spawn(runtime.clone(), no_auth()).await;
    let http = reqwest::Client::new();

    // Initially no pending questions.
    let pending: Value = http
        .get(format!("{base}/question"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(pending.as_array().unwrap().is_empty());

    // Fire the ask in the background; it blocks awaiting a reply.
    let ask = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.question().ask("ses_1", vec![one_question()]).await })
    };

    // Poll until the request shows up as pending.
    let mut found = None;
    for _ in 0..100 {
        let pending: Value = http
            .get(format!("{base}/question"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(first) = pending.as_array().and_then(|a| a.first()) {
            found = Some(first.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let found = found.expect("a question request should be pending");
    assert_eq!(found["sessionID"], "ses_1");
    let questions = found["questions"].as_array().unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0]["question"], "Which color?");
    assert_eq!(questions[0]["header"], "color");
    assert_eq!(questions[0]["multiple"], false);
    let options = questions[0]["options"].as_array().unwrap();
    assert_eq!(options.len(), 2);
    assert_eq!(options[0]["label"], "Red");
    assert_eq!(options[0]["description"], "the color red");
    assert_eq!(options[1]["label"], "Blue");
    let request_id = found["id"].as_str().unwrap().to_string();

    // Reply -> unblocks the ask with the chosen index.
    let replied = http
        .post(format!("{base}/question/{request_id}/reply"))
        .json(&json!({ "reply": "answered", "answers": [[0]] }))
        .send()
        .await
        .unwrap();
    assert_eq!(replied.status(), 200);
    assert_eq!(replied.json::<Value>().await.unwrap(), json!(true));

    let outcome = ask.await.unwrap();
    assert_eq!(outcome, QuestionOutcome::Answered(vec![vec![0]]));

    // No longer pending.
    let pending: Value = http
        .get(format!("{base}/question"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(pending.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn question_reply_cancelled_resolves_the_ask() {
    let runtime = plain_runtime().await;
    let base = spawn(runtime.clone(), no_auth()).await;
    let http = reqwest::Client::new();

    let ask = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.question().ask("ses_2", vec![one_question()]).await })
    };

    let mut request_id = None;
    for _ in 0..100 {
        let pending: Value = http
            .get(format!("{base}/question"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(first) = pending.as_array().and_then(|a| a.first()) {
            request_id = Some(first["id"].as_str().unwrap().to_string());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let request_id = request_id.expect("a question request should be pending");

    let replied = http
        .post(format!("{base}/question/{request_id}/reply"))
        .json(&json!({ "reply": "cancelled" }))
        .send()
        .await
        .unwrap();
    assert_eq!(replied.status(), 200);

    let outcome = ask.await.unwrap();
    assert_eq!(outcome, QuestionOutcome::Cancelled);
}

#[tokio::test]
async fn question_reply_unknown_id_returns_404() {
    let base = spawn(plain_runtime().await, no_auth()).await;
    let http = reqwest::Client::new();

    let resp = http
        .post(format!("{base}/question/que_does_not_exist/reply"))
        .json(&json!({ "reply": "cancelled" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn question_reply_bad_reply_string_returns_400() {
    let base = spawn(plain_runtime().await, no_auth()).await;
    let http = reqwest::Client::new();

    let resp = http
        .post(format!("{base}/question/que_whatever/reply"))
        .json(&json!({ "reply": "bogus" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn question_asked_emits_sse_frame() {
    let runtime = plain_runtime().await;
    let base = spawn(runtime.clone(), no_auth()).await;
    let http = reqwest::Client::new();

    // Open the /event stream, then ask a question and confirm a
    // `question.asked` frame with the full questions[] payload arrives.
    let event_resp = http.get(format!("{base}/event")).send().await.unwrap();
    let mut body = event_resp.bytes_stream();
    // Drain the initial `server.connected` frame.
    let _ = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .expect("initial frame within 2s");

    let ask = {
        let runtime = runtime.clone();
        tokio::spawn(async move { runtime.question().ask("ses_9", vec![one_question()]).await })
    };

    let frame = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .expect("a question.asked frame within 2s")
        .expect("a chunk")
        .expect("chunk ok");
    let text = String::from_utf8_lossy(&frame).to_string();
    let payload = text.strip_prefix("data: ").unwrap_or(&text).trim();
    let parsed: Value = serde_json::from_str(payload).expect("json frame");
    assert_eq!(parsed["type"], "question.asked");
    assert_eq!(parsed["properties"]["sessionID"], "ses_9");
    let questions = parsed["properties"]["questions"].as_array().unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0]["question"], "Which color?");
    assert_eq!(questions[0]["header"], "color");
    assert_eq!(questions[0]["multiple"], false);
    let options = questions[0]["options"].as_array().unwrap();
    assert_eq!(options.len(), 2);
    assert_eq!(options[0]["label"], "Red");
    assert_eq!(options[1]["label"], "Blue");

    // Resolve it so the spawned ask completes.
    let request_id = parsed["properties"]["id"].as_str().unwrap().to_string();
    let replied = http
        .post(format!("{base}/question/{request_id}/reply"))
        .json(&json!({ "reply": "cancelled" }))
        .send()
        .await
        .unwrap();
    assert_eq!(replied.status(), 200);
    assert_eq!(ask.await.unwrap(), QuestionOutcome::Cancelled);
}

#[tokio::test]
async fn basic_auth_gate() {
    let opts = ServeOptions {
        password: Some("secret".into()),
        cors: false,
    };
    let base = spawn(plain_runtime().await, opts).await;
    let http = reqwest::Client::new();

    // No credentials -> 401.
    let unauth = http.get(format!("{base}/agent")).send().await.unwrap();
    assert_eq!(unauth.status(), 401);

    // Correct credentials -> 200.
    let ok = http
        .get(format!("{base}/agent"))
        .basic_auth("otto", Some("secret"))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);

    // /doc stays unauthenticated.
    let doc = http.get(format!("{base}/doc")).send().await.unwrap();
    assert_eq!(doc.status(), 200);
}

#[tokio::test]
async fn event_stream_opens() {
    let base = spawn(plain_runtime().await, no_auth()).await;
    let http = reqwest::Client::new();

    let resp = http.get(format!("{base}/event")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
        "text/event-stream"
    );

    // The stream stays open and emits at least an initial frame.
    let mut body = resp.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .expect("a first SSE frame within 2s")
        .expect("a chunk")
        .expect("chunk ok");
    let text = String::from_utf8_lossy(&first);
    assert!(
        text.contains("data:") || text.starts_with(':'),
        "frame: {text}"
    );
}

#[tokio::test]
async fn lsp_status_route_returns_array() {
    let base = spawn(plain_runtime().await, no_auth()).await;
    let http = reqwest::Client::new();

    let resp = http.get(format!("{base}/lsp")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body.is_array(), "lsp statuses: {body}");
    assert!(body.as_array().unwrap().is_empty(), "lsp statuses: {body}");
}

#[tokio::test]
async fn file_list_route_returns_workspace_files() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("one.rs"), "x").unwrap();
    std::fs::write(tmp.path().join("two.txt"), "y").unwrap();
    std::fs::create_dir_all(tmp.path().join("nested")).unwrap();
    std::fs::write(tmp.path().join("nested/three.rs"), "z").unwrap();

    let (factory, _) = ScriptedRouteFactory::new(vec![]);
    let runtime = Arc::new(
        Runtime::in_memory(Config::default())
            .await
            .unwrap()
            .with_route_factory(factory)
            .with_directory(tmp.path().to_path_buf()),
    );
    let base = spawn(runtime, no_auth()).await;
    let http = reqwest::Client::new();

    let resp = http
        .get(format!("{base}/file/list?limit=100"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let files = body["files"].as_array().unwrap();
    let names: Vec<&str> = files.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        names.contains(&"one.rs")
            && names.contains(&"two.txt")
            && names.contains(&"nested/three.rs"),
        "files: {body}"
    );
    let dirs = body["dirs"].as_array().unwrap();
    let dir_names: Vec<&str> = dirs.iter().filter_map(|v| v.as_str()).collect();
    assert!(dir_names.contains(&"nested"), "dirs: {body}");
    assert_eq!(body["truncated"], false, "truncated: {body}");
}

// -- workflow ------------------------------------------------------------

/// `POST /workflow/{kind}` must stamp the workflow's root session with
/// `metadata.kind == "workflow_root"` and `metadata.workflowKind == <kind>`
/// (otto extension, no opencode analog — feeds the multi-agent dashboard)
/// synchronously, before the workflow itself runs. The `arg` here points at a
/// nonexistent plan file, so the engine fails once it runs on its detached
/// background task; that's fine — the assertion only needs the session to
/// exist with the stamped metadata, not the workflow to complete.
#[tokio::test]
async fn workflow_run_stamps_root_session_metadata() {
    // `workflow_run` discovers a git root (for its worktree context) before
    // returning, so the runtime's directory must sit inside a real repo —
    // unlike `plain_runtime()`, which points at a bare temp dir. Point it at
    // this crate's own directory, which lives inside the otto repo/worktree.
    let (factory, _) = ScriptedRouteFactory::new(vec![]);
    let runtime = Arc::new(
        Runtime::in_memory(Config::default())
            .await
            .unwrap()
            .with_route_factory(factory)
            .with_directory(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
    );
    let base = spawn(runtime, no_auth()).await;
    let http = reqwest::Client::new();

    let resp = http
        .post(format!("{base}/workflow/plan"))
        .json(&json!({ "arg": "/nonexistent/plan.md" }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let created: Value = resp.json().await.unwrap();
    assert!(status.is_success(), "status {status}: {created}");
    let id = created["session"].as_str().unwrap().to_string();

    let got: Value = http
        .get(format!("{base}/session/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["metadata"]["kind"], "workflow_root", "session: {got}");
    assert_eq!(got["metadata"]["workflowKind"], "plan", "session: {got}");
}

// -- run-failure surfacing ---------------------------------------------------

/// A route that fails mid-stream — models a bad model id / auth / transport
/// error, so the run's join returns `Err`.
struct ErrorRoute;

impl Route for ErrorRoute {
    fn id(&self) -> &str {
        "error"
    }
    fn stream(&self, _req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
        stream::iter(vec![Err(LLMError::Transport("boom".into()))]).boxed()
    }
}

struct ErrorRouteFactory {
    model: Model,
}

impl otto_app::RouteFactory for ErrorRouteFactory {
    fn route_for(&self, _m: &ModelRef) -> AppResult<(Arc<dyn Route>, Model)> {
        Ok((Arc::new(ErrorRoute), self.model.clone()))
    }
}

/// When the run fails, the prompt SSE must still emit a terminal
/// `provider-error` frame — otherwise a client keyed off `finish` hangs.
#[tokio::test]
async fn prompt_run_failure_emits_provider_error() {
    let factory = Arc::new(ErrorRouteFactory {
        model: Model::new("anthropic", "claude-3", "route_err"),
    });
    let runtime = Arc::new(
        Runtime::in_memory(Config::default())
            .await
            .unwrap()
            .with_route_factory(factory),
    );
    let base = spawn(runtime, no_auth()).await;
    let http = reqwest::Client::new();

    let created: Value = http
        .post(format!("{base}/session"))
        .json(&json!({ "title": "Err" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();

    let body = http
        .post(format!("{base}/session/{id}/message"))
        .json(&json!({ "prompt": "hi" }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("provider-error"),
        "run failure must surface as a provider-error frame: {body}"
    );
}

// -- prompt file attachments --------------------------------------------------

#[tokio::test]
async fn prompt_with_text_attachment_inlines_file() {
    let tmp = std::env::temp_dir().join(format!("otto-pa-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("note.txt"), "hello from file\n").unwrap();

    let (factory, _calls) = ScriptedRouteFactory::new(vec![text_turn("t1", "done")]);
    let runtime = Arc::new(
        Runtime::in_memory(Config::default())
            .await
            .unwrap()
            .with_route_factory(factory)
            .with_directory(tmp.clone()),
    );
    let base = spawn(runtime, no_auth()).await;
    let http = reqwest::Client::new();

    let created: Value = http
        .post(format!("{base}/session"))
        .json(&json!({ "title": "Attach" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let resp = http
        .post(format!("{base}/session/{id}/message"))
        .json(&json!({ "prompt": "summarize", "files": [ { "path": "note.txt" } ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap(); // drain the SSE stream so the run settles.

    // The persisted user message parts should include the inlined envelope.
    let msgs: Value = http
        .get(format!("{base}/session/{id}/message"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dump = msgs.to_string();
    assert!(
        dump.contains("hello from file") && dump.contains("<content>"),
        "messages: {dump}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn prompt_with_binary_attachment_returns_400() {
    let tmp = std::env::temp_dir().join(format!("otto-pab-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("x.bin"), [0u8, 0, 0, 1, 2]).unwrap();

    let (factory, calls) = ScriptedRouteFactory::new(vec![text_turn("t1", "done")]);
    let runtime = Arc::new(
        Runtime::in_memory(Config::default())
            .await
            .unwrap()
            .with_route_factory(factory)
            .with_directory(tmp.clone()),
    );
    let base = spawn(runtime, no_auth()).await;
    let http = reqwest::Client::new();

    let created: Value = http
        .post(format!("{base}/session"))
        .json(&json!({ "title": "Bin" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let resp = http
        .post(format!("{base}/session/{id}/message"))
        .json(&json!({ "prompt": "hi", "files": [ { "path": "x.bin" } ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.unwrap();
    assert!(
        err["error"].as_str().is_some(),
        "expected a top-level error string: {err}"
    );
    // No run must have been started.
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn prompt_with_too_many_attachments_returns_400() {
    let tmp = std::env::temp_dir().join(format!("otto-pamax-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let (factory, calls) = ScriptedRouteFactory::new(vec![text_turn("t1", "done")]);
    let runtime = Arc::new(
        Runtime::in_memory(Config::default())
            .await
            .unwrap()
            .with_route_factory(factory)
            .with_directory(tmp.clone()),
    );
    let base = spawn(runtime, no_auth()).await;
    let http = reqwest::Client::new();

    let created: Value = http
        .post(format!("{base}/session"))
        .json(&json!({ "title": "TooMany" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // The count cap is enforced before any path is resolved, so these
    // needn't exist on disk.
    let files: Vec<Value> = (0..21)
        .map(|i| json!({ "path": format!("nonexistent-{i}.txt") }))
        .collect();

    let resp = http
        .post(format!("{base}/session/{id}/message"))
        .json(&json!({ "prompt": "hi", "files": files }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.unwrap();
    assert!(
        err["error"]
            .as_str()
            .is_some_and(|m| m.contains("too many attachments")),
        "expected a too-many-attachments error: {err}"
    );
    // No run must have been started.
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let _ = std::fs::remove_dir_all(&tmp);
}
