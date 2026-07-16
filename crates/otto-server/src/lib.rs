//! HTTP + SSE server — an opencode-compatible route surface over the shared
//! [`otto_app::Runtime`].
//!
//! The server mirrors the shapes of opencode's experimental HttpApi groups
//! (`packages/opencode/src/server/routes/instance/httpapi/groups/*.ts`): the
//! `/session/*` CRUD + prompt surface (`session.ts`), the global `/event` SSE
//! stream (`event.ts`), the `/permission` list/reply pair (`permission.ts`),
//! `/provider` (`provider.ts`), the `/agent` + `/path` instance routes
//! (`instance.ts`), `/config` (`config.ts`), and the `/find` + `/file` file
//! routes (`file.ts`).
//!
//! [`serve`] binds a listener; [`router`] builds the [`axum::Router`] so tests
//! can drive it over an ephemeral port (or `tower::ServiceExt::oneshot`) with no
//! network to a provider.

#![forbid(unsafe_code)]

mod attach;
mod find;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use async_stream::stream;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, extract::Request};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use otto_agent::ModelRef;
use otto_app::{RunHandle, Runtime};
use otto_permission::Reply;
use otto_storage::Session;
use otto_storage::model::SessionId;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;

/// The server version reported by `/app` and `/path`.
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Username accepted by the Basic-auth gate.
const AUTH_USERS: [&str; 1] = ["otto"];
/// Maximum number of `files[]` attachments accepted per prompt. Guards against
/// an authenticated client attaching an unbounded list and forcing the server
/// to sequentially resolve (and hold in memory) an unbounded number of files.
const MAX_ATTACHMENTS: usize = 20;

/// Options controlling the server surface.
///
/// * `password` — when `Some`, every route except `/doc` is gated behind HTTP
///   Basic auth (`Authorization: Basic base64("<user>:<password>")`, user in
///   [`AUTH_USERS`]).
/// * `cors` — when `true`, a permissive `tower-http` CORS layer is installed.
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// Optional Basic-auth password gate.
    pub password: Option<String>,
    /// Enable permissive CORS.
    pub cors: bool,
}

/// Errors raised while binding or serving.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// The listener could not bind, or the accept loop failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Per-session registry of running workflows' [`CancellationToken`]s.
///
/// A workflow run registers its `abort` token here on start and removes it when
/// the run ends; `POST /workflow/{session}/cancel` looks up the token and
/// cancels it (aborting the engine's in-flight subagent spawns). A cancel on a
/// session with no live run (finished or never started) returns `false`.
///
/// Entries are generation-tagged: `insert` cancels any token it replaces (a new
/// prompt on a busy session interrupts the prior turn instead of racing it),
/// and `remove` only drops the entry when the generation still matches, so a
/// replaced turn settling late cannot remove the newer turn's token.
#[derive(Clone, Default)]
struct TokenRegistry(
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, (u64, CancellationToken)>>>,
);

impl TokenRegistry {
    /// Register `token` for `session`, cancelling any token it replaces.
    /// Returns the entry's generation, which the caller passes to [`remove`].
    ///
    /// [`remove`]: TokenRegistry::remove
    fn insert(&self, session: &str, token: CancellationToken) -> u64 {
        let mut map = self.0.lock().unwrap();
        let generation = map.get(session).map_or(0, |(g, _)| g + 1);
        if let Some((_, prev)) = map.insert(session.to_string(), (generation, token)) {
            prev.cancel();
        }
        generation
    }
    /// Drop `session`'s entry (called when the run ends), but only if it is
    /// still the `generation` this caller registered — a stale remove from a
    /// replaced turn must not evict the newer turn's token.
    fn remove(&self, session: &str, generation: u64) {
        let mut map = self.0.lock().unwrap();
        if map.get(session).is_some_and(|(g, _)| *g == generation) {
            map.remove(session);
        }
    }
    /// Cancel the run for `session`; returns `true` if one was registered.
    fn cancel(&self, session: &str) -> bool {
        match self.0.lock().unwrap().get(session) {
            Some((_, t)) => {
                t.cancel();
                true
            }
            None => false,
        }
    }
}

/// Shared handler state: the runtime plus a process-wide event bus that `/event`
/// fans out (permission asks + streamed run events).
#[derive(Clone)]
struct AppState {
    runtime: Arc<Runtime>,
    password: Option<Arc<str>>,
    /// Pre-serialized SSE `data` payloads broadcast to `/event` subscribers.
    events: broadcast::Sender<String>,
    /// Cancellation tokens of currently-running workflows, keyed by session id.
    workflows: TokenRegistry,
    /// Cancellation tokens of currently-running prompt turns, keyed by session
    /// id. A turn registers its `abort` token on start and removes it when the
    /// stream settles; `POST /session/{id}/cancel` cancels it to interrupt a
    /// turn without ending the session.
    runs: TokenRegistry,
}

/// Bind `addr` and serve the [`router`] until the process exits.
///
/// The CLI wires this entrypoint; its signature is a stable contract.
///
/// # Errors
/// Returns [`ServerError::Io`] if the listener cannot bind or the accept loop
/// fails.
pub async fn serve(
    runtime: Arc<Runtime>,
    addr: SocketAddr,
    opts: ServeOptions,
) -> Result<(), ServerError> {
    let app = router(runtime, opts);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the full [`axum::Router`] over `runtime`.
///
/// Applies the Basic-auth layer (when `opts.password` is set) to every route
/// except `/doc`, then a permissive CORS layer when `opts.cors` is set.
pub fn router(runtime: Arc<Runtime>, opts: ServeOptions) -> Router {
    let (events, _) = broadcast::channel::<String>(1024);
    let state = AppState {
        runtime,
        password: opts.password.map(Arc::from),
        events,
        workflows: TokenRegistry::default(),
        runs: TokenRegistry::default(),
    };

    // Every route except `/doc` (which stays unauthenticated for discovery).
    let protected = Router::new()
        .route("/app", get(app_info))
        .route("/path", get(app_info))
        .route("/config", get(config_get).patch(config_patch))
        .route("/agent", get(agent_list))
        .route("/provider", get(provider_list))
        .route("/session", get(session_list).post(session_create))
        .route("/session/{id}", get(session_get).delete(session_delete))
        .route(
            "/session/{id}/message",
            get(session_messages).post(session_prompt),
        )
        .route("/event", get(event_stream))
        .route("/permission", get(permission_list))
        .route("/permission/{request_id}/reply", post(permission_reply))
        .route("/session/{id}/permission-mode", post(set_permission_mode))
        .route("/session/{id}/cancel", post(session_cancel))
        .route("/find", get(find_text))
        .route("/find/file", get(find_file))
        .route("/file/content", get(file_content))
        .route("/file/list", get(file_list))
        .route("/lsp", get(lsp_status))
        .route(
            "/experimental/worktree",
            get(worktree_list)
                .post(worktree_create)
                .delete(worktree_remove),
        )
        .route("/experimental/worktree/reset", post(worktree_reset))
        .route("/workflow/{kind}", post(workflow_run))
        .route("/workflow/{session}/cancel", post(workflow_cancel))
        .layer(middleware::from_fn_with_state(state.clone(), auth_gate));

    let mut app = protected.merge(Router::new().route("/doc", get(doc)));
    if opts.cors {
        app = app.layer(CorsLayer::permissive());
    }
    app.with_state(state)
}

// -- middleware --------------------------------------------------------------

/// Basic-auth gate — port of the `Authorization` middleware referenced by every
/// opencode HttpApi group. A `None` password disables the gate.
async fn auth_gate(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.password.as_deref() else {
        return next.run(req).await;
    };
    if credentials_ok(&req, expected) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"otto\"")],
            Json(json!({ "error": { "message": "unauthorized" } })),
        )
            .into_response()
    }
}

/// Validate an `Authorization: Basic …` header against `expected`.
fn credentials_ok(req: &Request, expected: &str) -> bool {
    let Some(value) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(b64) = value.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = BASE64.decode(b64.trim()) else {
        return false;
    };
    let Ok(pair) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((user, pass)) = pair.split_once(':') else {
        return false;
    };
    AUTH_USERS.contains(&user) && pass == expected
}

// -- error type --------------------------------------------------------------

/// A JSON API error rendered as `{ error: { message } }` with a status code.
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": { "message": self.message } })),
        )
            .into_response()
    }
}

impl From<otto_storage::StorageError> for ApiError {
    fn from(e: otto_storage::StorageError) -> Self {
        ApiError::internal(e.to_string())
    }
}
impl From<otto_app::Error> for ApiError {
    fn from(e: otto_app::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}
impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}
impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}

/// Handler result alias.
type ApiResult<T> = Result<T, ApiError>;

// -- instance / config / agent / provider ------------------------------------

/// `GET /app` + `GET /path` — instance info (cwd/root/version). Shape mirrors
/// opencode `instance.ts` `PathInfo`.
async fn app_info(State(state): State<AppState>) -> Json<Value> {
    let dir = state.runtime.directory().display().to_string();
    Json(json!({
        "version": VERSION,
        "directory": dir,
        "path": { "directory": dir, "cwd": dir, "root": dir },
    }))
}

/// `GET /lsp` — spawned LSP client statuses (opencode `lsp.ts` `LSP.Status`).
/// Empty array when no servers have been spawned.
async fn lsp_status(State(state): State<AppState>) -> Json<Value> {
    let statuses = state.runtime.lsp().statuses();
    Json(serde_json::to_value(statuses).unwrap_or_else(|_| json!([])))
}

/// `GET /config` — the loaded config JSON (opencode `config.ts`).
async fn config_get(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(serde_json::to_value(state.runtime.config())?))
}

/// `PATCH /config` — best-effort deep-merge of the incoming patch over the
/// loaded config, returning the merged result (opencode `config.ts`). The
/// runtime's live config is not mutated.
async fn config_patch(
    State(state): State<AppState>,
    Json(patch): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut base = serde_json::to_value(state.runtime.config())?;
    merge_json(&mut base, patch);
    Ok(Json(base))
}

/// `GET /agent` — the resolved agent set (opencode `instance.ts` `/agent`).
async fn agent_list(State(state): State<AppState>) -> Json<Value> {
    Json(json!(state.runtime.agents()))
}

/// `GET /provider` — providers + models derived from the default model and any
/// agent-pinned models (opencode `provider.ts`).
async fn provider_list(State(state): State<AppState>) -> Json<Value> {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;

    let default = state.runtime.default_model();
    let mut providers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    providers
        .entry(default.provider.0.clone())
        .or_default()
        .insert(default.model.0.clone());
    for agent in state.runtime.agents() {
        if let Some(m) = &agent.model {
            providers
                .entry(m.provider.0.clone())
                .or_default()
                .insert(m.model.0.clone());
        }
    }
    let list: Vec<Value> = providers
        .into_iter()
        .map(|(id, models)| {
            let models: Vec<Value> = models
                .into_iter()
                .map(|m| json!({ "id": m, "name": m }))
                .collect();
            json!({ "id": id, "name": id, "models": models })
        })
        .collect();
    Json(json!({
        "providers": list,
        "default": { "providerID": default.provider.0, "modelID": default.model.0 },
    }))
}

// -- sessions ----------------------------------------------------------------

/// Body for `POST /session`.
#[derive(Debug, Default, Deserialize)]
struct CreateSessionBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default, rename = "parentID")]
    parent: Option<String>,
}

/// `GET /session` — list sessions (opencode `session.ts`).
async fn session_list(State(state): State<AppState>) -> ApiResult<Json<Vec<Session>>> {
    Ok(Json(state.runtime.store().list_sessions().await?))
}

/// `POST /session` — create a session and return it (opencode `session.ts`).
async fn session_create(
    State(state): State<AppState>,
    body: Option<Json<CreateSessionBody>>,
) -> ApiResult<Json<Session>> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let title = body.title.unwrap_or_else(|| "New Session".to_string());
    let agent = resolve_agent(&state.runtime, body.agent.as_deref());
    let id = state
        .runtime
        .create_session(title, &agent, body.parent.map(SessionId::from))
        .await?;
    let session = state
        .runtime
        .store()
        .get_session(&id)
        .await?
        .ok_or_else(|| ApiError::internal("created session vanished"))?;
    Ok(Json(session))
}

/// `GET /session/{id}` — fetch a session or 404 (opencode `session.ts`).
async fn session_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Session>> {
    state
        .runtime
        .store()
        .get_session(&SessionId::from(&id))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("session {id} not found")))
}

/// `GET /session/{id}/message` — messages with their parts (opencode
/// `session.ts` `WithParts`).
async fn session_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let with_parts = state
        .runtime
        .store()
        .messages_with_parts(&SessionId::from(&id))
        .await?;
    Ok(Json(json!(with_parts)))
}

/// `DELETE /session/{id}` — delete a session (FK-cascades its messages/parts).
async fn session_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<bool>> {
    sqlx::query("DELETE FROM session WHERE id = ?")
        .bind(&id)
        .execute(state.runtime.store().pool())
        .await?;
    Ok(Json(true))
}

/// Body for `POST /session/{id}/message`: either `{ parts: [{ type, text }] }`
/// or `{ prompt }`, with optional `agent` / `model` overrides and optional
/// `files: [{ path }]` attachments resolved relative to the runtime directory.
#[derive(Debug, Default, Deserialize)]
struct PromptBody {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    parts: Option<Vec<PartInput>>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    files: Option<Vec<FileInput>>,
}

/// One `parts[]` element of a [`PromptBody`].
#[derive(Debug, Deserialize)]
struct PartInput {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

/// One `files[]` element of a [`PromptBody`] — a workspace-relative path
/// resolved via [`crate::attach::resolve_attachment`].
#[derive(Debug, Clone, Deserialize)]
struct FileInput {
    path: String,
}

impl PromptBody {
    /// Flatten to the prompt text (`prompt`, else concatenated text parts).
    fn text(&self) -> String {
        if let Some(p) = &self.prompt
            && !p.is_empty()
        {
            return p.clone();
        }
        self.parts
            .iter()
            .flatten()
            .filter(|p| p.kind.as_deref() != Some("file"))
            .filter_map(|p| p.text.clone())
            .collect::<Vec<_>>()
            .join("")
    }
}

/// `POST /session/{id}/message` — the streaming prompt endpoint (opencode
/// `session.ts` prompt route).
///
/// Persists the user prompt, kicks off [`Runtime::run`], and streams each
/// [`LLMEvent`](otto_events::LLMEvent) back as `data: {json}\n\n` over an SSE
/// response. The stream ends when the run's join completes. Each event is also
/// fanned out to the process-wide `/event` bus.
async fn session_prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<PromptBody>>,
) -> ApiResult<Response> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let prompt = body.text();
    if prompt.is_empty() {
        return Err(ApiError::bad_request("empty prompt"));
    }
    // Session must exist before we spend a turn on it.
    if state
        .runtime
        .store()
        .get_session(&SessionId::from(&id))
        .await?
        .is_none()
    {
        return Err(ApiError::not_found(format!("session {id} not found")));
    }

    let agent = resolve_agent(&state.runtime, body.agent.as_deref());
    let model = body
        .model
        .as_deref()
        .map_or_else(|| state.runtime.default_model(), ModelRef::parse);

    // Resolve any file attachments before spending a turn: any `AttachError`
    // is a 400 with no run started (opencode surfaces bad attachments the
    // same way — as a client-facing rejection, not a wasted provider call).
    let root = state.runtime.directory().to_path_buf();
    let mut extra_parts: Vec<otto_storage::PartKind> = Vec::new();
    if let Some(files) = &body.files {
        if files.len() > MAX_ATTACHMENTS {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("too many attachments (max {MAX_ATTACHMENTS})") })),
            )
                .into_response());
        }
        // Attachment reads are blocking `std::fs` work; resolve the whole
        // batch off the async runtime in one `spawn_blocking` call.
        let files = files.clone();
        let root = root.clone();
        let resolved = tokio::task::spawn_blocking(move || {
            let mut parts = Vec::new();
            for f in &files {
                match attach::resolve_attachment(&root, &f.path) {
                    Ok(attach::ResolvedAttachment::Text(envelope)) => {
                        parts.push(otto_storage::PartKind::Text {
                            text: envelope,
                            synthetic: Some(true),
                            ignored: None,
                            time: None,
                            metadata: None,
                        });
                    }
                    Ok(attach::ResolvedAttachment::Image {
                        mime,
                        filename,
                        data_url,
                    }) => {
                        parts.push(otto_storage::PartKind::File {
                            mime,
                            filename: Some(filename),
                            url: data_url,
                            source: None,
                        });
                    }
                    Err(e) => return Err(e.message(&f.path)),
                }
            }
            Ok(parts)
        })
        .await
        .expect("attachment resolution task panicked");
        match resolved {
            Ok(parts) => extra_parts.extend(parts),
            Err(message) => {
                return Ok(
                    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
                );
            }
        }
    }

    // Register the turn's abort token so `POST /session/{id}/cancel` can
    // interrupt it. Removed once the stream settles (below), so a later cancel
    // on a finished turn returns `false`. Registering also cancels any prior
    // still-running turn on this session — two concurrent runs would race
    // writes on the same rows.
    let abort = CancellationToken::new();
    let run_generation = state.runs.insert(&id, abort.clone());

    let RunHandle { mut events, join } =
        state
            .runtime
            .run_with_parts(&id, prompt, extra_parts, &agent, &model, abort);

    let bus = state.events.clone();
    let runs = state.runs.clone();
    let run_session = id.clone();
    let sse = stream! {
        while let Some(event) = events.recv().await {
            match serde_json::to_string(&event) {
                Ok(data) => {
                    // Fan the run event out to /event subscribers, too.
                    let envelope = json!({
                        "type": "message.part.updated",
                        "properties": event,
                    })
                    .to_string();
                    let _ = bus.send(envelope);
                    yield Ok::<_, Infallible>(Event::default().data(data));
                }
                Err(_) => continue,
            }
        }
        // Ensure the run has fully settled before closing the stream. If it
        // failed (bad model, auth, provider error, …) the run's event channel
        // closes without a `finish` frame, so emit a terminal `provider-error`
        // event — otherwise a client that keyed off `finish` waits forever.
        // `LLMEvent::ProviderError` only requires `message`, so this decodes
        // into the same event clients already render.
        if let Ok(Err(e)) = join.await {
            let err = json!({ "type": "provider-error", "message": e.to_string() });
            let _ = bus.send(
                json!({ "type": "message.part.updated", "properties": err }).to_string(),
            );
            yield Ok::<_, Infallible>(Event::default().data(err.to_string()));
        }
        // The turn has settled — drop its cancel token so a later cancel on this
        // finished turn returns `false`. Generation-checked: if a newer turn
        // already replaced this one, its token stays registered.
        runs.remove(&run_session, run_generation);
    };
    // Keep-alive comments so a legitimately slow turn (tools running, or the
    // provider mid-generation) keeps bytes flowing to the client during quiet
    // stretches — otherwise the client's idle read-timeout kills the socket
    // mid-turn. The `/event` stream already does this; the prompt stream must
    // too, since a single turn can be silent for far longer than that timeout.
    Ok(Sse::new(sse)
        .keep_alive(KeepAlive::default())
        .into_response())
}

// -- event bus ---------------------------------------------------------------

/// `GET /event` — the global SSE stream (opencode `event.ts`).
///
/// Emits an initial `server.connected` frame, then forwards permission
/// [`Asked`](otto_permission::Asked) events and any fanned-out run events,
/// each framed as `data: { type, properties }`. `tower-http`/axum keep-alive
/// comments hold the connection open between events.
async fn event_stream(
    State(state): State<AppState>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let mut perm_rx = state.runtime.permission().subscribe();
    let mut bus_rx = state.events.subscribe();
    let connected = json!({ "type": "server.connected", "properties": {} }).to_string();

    let sse = stream! {
        yield Ok(Event::default().data(connected));
        loop {
            tokio::select! {
                asked = perm_rx.recv() => match asked {
                    Ok(a) => {
                        let data = json!({
                            "type": "permission.asked",
                            "properties": {
                                "id": a.request_id,
                                "sessionID": a.session_id,
                                "permission": a.permission,
                                "patterns": a.patterns,
                                "metadata": a.metadata,
                            }
                        })
                        .to_string();
                        yield Ok(Event::default().data(data));
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                },
                fanned = bus_rx.recv() => match fanned {
                    Ok(data) => yield Ok(Event::default().data(data)),
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                },
            }
        }
    };
    Sse::new(sse).keep_alive(KeepAlive::default())
}

// -- permission --------------------------------------------------------------

/// `GET /permission` — pending requests (opencode `permission.ts` `list`).
async fn permission_list(State(state): State<AppState>) -> Json<Value> {
    let pending: Vec<Value> = state
        .runtime
        .permission()
        .list_pending()
        .into_iter()
        .map(|p| {
            json!({
                "id": p.request_id,
                "sessionID": p.session_id,
                "permission": p.permission,
                "patterns": p.patterns,
                "metadata": p.metadata,
            })
        })
        .collect();
    Json(Value::Array(pending))
}

/// Body for `POST /permission/{request_id}/reply`.
#[derive(Debug, Deserialize)]
struct ReplyBody {
    reply: String,
    #[serde(default)]
    message: Option<String>,
}

/// `POST /permission/{request_id}/reply` — resolve a request (opencode
/// `permission.ts` `reply`).
async fn permission_reply(
    State(state): State<AppState>,
    Path(request_id): Path<String>,
    Json(body): Json<ReplyBody>,
) -> ApiResult<Json<bool>> {
    let reply = match body.reply.as_str() {
        "once" => Reply::Once,
        "always" => Reply::Always,
        "reject" => Reply::Reject {
            message: body.message,
        },
        other => return Err(ApiError::bad_request(format!("unknown reply {other:?}"))),
    };
    if state.runtime.permission().reply(&request_id, reply) {
        Ok(Json(true))
    } else {
        Err(ApiError::not_found(format!(
            "permission {request_id} not found"
        )))
    }
}

/// Body for `POST /session/{id}/permission-mode`.
#[derive(Debug, Deserialize)]
struct SetModeBody {
    mode: String,
}

/// `POST /session/{id}/permission-mode` — set a session's live permission mode
/// (drives the TUI's mode-cycle keybind; no direct opencode analog).
///
/// A recognized wire string (`PermissionMode::from_str_opt`) sets the mode via
/// [`otto_permission::Permission::set_mode`] and fans a `permission.mode_changed`
/// frame `{ sessionID, mode }` out over the `/event` bus, mirroring the
/// `permission.asked` envelope shape built in [`event_stream`]. An
/// unrecognized string is a 400, matching `permission_reply`'s handling of an
/// unknown `reply`.
async fn set_permission_mode(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<SetModeBody>,
) -> ApiResult<Json<bool>> {
    let Some(mode) = otto_permission::PermissionMode::from_str_opt(&body.mode) else {
        return Err(ApiError::bad_request(format!(
            "unknown permission mode: {}",
            body.mode
        )));
    };
    state
        .runtime
        .permission()
        .set_mode(session_id.clone(), mode);

    let envelope = json!({
        "type": "permission.mode_changed",
        "properties": {
            "sessionID": session_id,
            "mode": mode.as_str(),
        }
    })
    .to_string();
    let _ = state.events.send(envelope);

    Ok(Json(true))
}

// -- find / file -------------------------------------------------------------

/// Query for `GET /find`.
#[derive(Debug, Deserialize)]
struct FindQuery {
    #[serde(default)]
    pattern: String,
}
/// Query for `GET /find/file`.
#[derive(Debug, Deserialize)]
struct FileQuery {
    #[serde(default)]
    query: String,
}
/// Query for `GET /file/content`.
#[derive(Debug, Deserialize)]
struct ContentQuery {
    path: String,
}

/// `GET /find?pattern=` — content grep over the instance directory
/// (opencode `file.ts`). The walk + per-file reads are blocking `std::fs`
/// work, so they run off the async runtime via `spawn_blocking`.
async fn find_text(State(state): State<AppState>, Query(q): Query<FindQuery>) -> Json<Value> {
    let root = state.runtime.directory().to_path_buf();
    Json(
        tokio::task::spawn_blocking(move || find::grep(&root, &q.pattern))
            .await
            .expect("find::grep task panicked"),
    )
}

/// `GET /find/file?query=` — filename search (opencode `file.ts`).
async fn find_file(State(state): State<AppState>, Query(q): Query<FileQuery>) -> Json<Value> {
    let root = state.runtime.directory().to_path_buf();
    Json(
        tokio::task::spawn_blocking(move || find::find_files(&root, &q.query))
            .await
            .expect("find::find_files task panicked"),
    )
}

/// `GET /file/content?path=` — read a file (opencode `file.ts`).
async fn file_content(
    State(state): State<AppState>,
    Query(q): Query<ContentQuery>,
) -> ApiResult<Json<Value>> {
    let root = state.runtime.directory().to_path_buf();
    tokio::task::spawn_blocking(move || find::read(&root, &q.path))
        .await
        .expect("find::read task panicked")
        .map(Json)
        .map_err(|e| ApiError::not_found(e.to_string()))
}

/// Query for `GET /file/list`.
#[derive(Debug, Deserialize)]
struct FileListQuery {
    limit: Option<usize>,
}

/// `GET /file/list?limit=` — enumerate workspace files and directories
/// (repo-relative, capped at `limit`, clamped to `1..=5000`, default `1000`)
/// for the TUI's `@`-mention attachment picker.
///
/// `dirs` is a separate key from `files` for backward compatibility (old
/// clients that only read `files` are unaffected). The TUI client merges
/// `dirs` into its candidate list as trailing-`/` strings, i.e.
/// `is_dir == ends_with('/')`.
async fn file_list(State(state): State<AppState>, Query(q): Query<FileListQuery>) -> Json<Value> {
    let limit = q.limit.unwrap_or(1000).clamp(1, 5000);
    let root = state.runtime.directory().to_path_buf();
    let (files, dirs) = tokio::task::spawn_blocking(move || otto_vcs::find_entries(&root, limit))
        .await
        .unwrap_or_default();
    let truncated = files.len() >= limit || dirs.len() >= limit;
    Json(json!({ "files": files, "dirs": dirs, "truncated": truncated }))
}

// -- worktree ------------------------------------------------------------

/// Build a worktree manager rooted at the runtime's directory.
///
/// A non-git working directory is a client-side condition → 400; any other
/// git failure is a 500.
async fn worktree_for(state: &AppState) -> ApiResult<otto_vcs::worktree::Worktree> {
    let data_base = otto_config::paths::global_data_dir().join("worktree");
    otto_vcs::worktree::Worktree::discover(state.runtime.directory(), &data_base)
        .await
        .map_err(|e| match e {
            otto_vcs::VcsError::NotGit => {
                ApiError::bad_request("worktree requires a git repository")
            }
            other => ApiError::internal(other.to_string()),
        })
}

/// `GET /experimental/worktree` — directory paths of managed worktrees.
async fn worktree_list(State(state): State<AppState>) -> ApiResult<Json<Vec<String>>> {
    let wt = worktree_for(&state).await?;
    let dirs = wt
        .list()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .into_iter()
        .map(|w| w.directory)
        .collect();
    Ok(Json(dirs))
}

/// `POST /experimental/worktree` — create a worktree (empty body allowed).
async fn worktree_create(
    State(state): State<AppState>,
    body: Option<Json<otto_vcs::worktree::CreateInput>>,
) -> ApiResult<Json<otto_vcs::worktree::WorktreeInfo>> {
    let wt = worktree_for(&state).await?;
    let input = body.map(|Json(b)| b).unwrap_or_default();
    let info = wt
        .create(input)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(info))
}

/// `DELETE /experimental/worktree` — remove a worktree.
async fn worktree_remove(
    State(state): State<AppState>,
    Json(input): Json<otto_vcs::worktree::RemoveInput>,
) -> ApiResult<Json<bool>> {
    let wt = worktree_for(&state).await?;
    let ok = wt
        .remove(input)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(ok))
}

/// `POST /experimental/worktree/reset` — hard-reset a worktree to origin.
async fn worktree_reset(
    State(state): State<AppState>,
    Json(input): Json<otto_vcs::worktree::ResetInput>,
) -> ApiResult<Json<bool>> {
    let wt = worktree_for(&state).await?;
    let ok = wt
        .reset(input)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(ok))
}

// -- workflow ----------------------------------------------------------------

/// Build a `/event` envelope string: `{"type": kind_type, "properties": props}`.
fn workflow_envelope(kind_type: &str, props: Value) -> String {
    json!({ "type": kind_type, "properties": props }).to_string()
}

/// Body for `POST /workflow/{kind}` — the workflow argument (a plan path for
/// `sdd`/`plan`, a feature description for `tdd`).
#[derive(Debug, Deserialize)]
struct WorkflowBody {
    arg: String,
    /// Optional parent session (e.g. the TUI chat session that launched the
    /// workflow). Parenting links the workflow session into the permission
    /// service's chain so it — and every subagent under it — inherits the
    /// parent's permission mode (full-auto, accept-edits, …) live.
    #[serde(default)]
    parent: Option<String>,
}

/// `POST /workflow/{kind}` — run a native dev-loop workflow (`tdd`/`sdd`/`plan`)
/// on a background task. Builds the [`WfCtx`](otto_workflow::WfCtx) from the
/// runtime (fallible pieces run here so their errors reach the client), spawns
/// the engine detached, and returns `{ "session": "<id>" }` immediately. The
/// engine emits `workflow.started` then `workflow.done` onto the `/event` bus.
async fn workflow_run(
    State(state): State<AppState>,
    Path(kind): Path<String>,
    Json(body): Json<WorkflowBody>,
) -> ApiResult<Response> {
    if !matches!(kind.as_str(), "tdd" | "sdd" | "plan") {
        return Err(ApiError::bad_request(format!(
            "unknown workflow kind: {kind}"
        )));
    }
    let rt = state.runtime.clone();
    let agent = rt.default_agent().clone();
    let model = rt.default_model();
    let session_id = rt
        .create_session(
            format!("workflow {kind}"),
            &agent,
            body.parent.clone().map(SessionId::from),
        )
        .await?;
    let spawner = rt
        .subagent_spawner(&agent, &model)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let worktree = Arc::new(
        otto_vcs::worktree::Worktree::discover(
            rt.directory(),
            &otto_config::paths::global_data_dir().join("worktree"),
        )
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?,
    );
    let events = state.events.clone();
    let abort = CancellationToken::new();
    // Register the run's abort token BEFORE spawning so a cancel that races the
    // spawn still finds it; `spawn_workflow` removes it when the run ends.
    let generation = state.workflows.insert(&session_id, abort.clone());

    let cx = otto_workflow::WfCtx {
        spawner,
        worktree,
        runner: Arc::new(otto_workflow::AutoRunner::new(rt.directory().to_path_buf())),
        store: rt.store().clone(),
        directory: rt.directory().to_path_buf(),
        parent_session_id: session_id.to_string(),
        permission: Arc::new(otto_permission::Ruleset::default()),
        progress: None,
        subagent: None,
        abort: abort.clone(),
    };
    spawn_workflow(
        kind,
        body.arg,
        session_id.to_string(),
        cx,
        events,
        abort,
        state.workflows.clone(),
        generation,
    );

    Ok(Json(json!({ "session": session_id })).into_response())
}

/// `POST /workflow/{session}/cancel` — cancel a running workflow by session id.
///
/// Cancels the run's [`CancellationToken`] (aborting the engine's in-flight
/// subagent spawns) if one is registered. Returns `{"cancelled": bool}` —
/// `false` when the session has no live run (already finished, or unknown).
async fn workflow_cancel(
    State(state): State<AppState>,
    Path(session): Path<String>,
) -> ApiResult<Response> {
    let cancelled = state.workflows.cancel(&session);
    Ok(Json(json!({ "cancelled": cancelled })).into_response())
}

/// `POST /session/{id}/cancel` — interrupt a running prompt turn by session id.
///
/// Cancels the turn's [`CancellationToken`] (aborting the in-flight run) if one
/// is registered, without ending the session. Returns `{"cancelled": bool}` —
/// `false` when the session has no live turn (already finished, or unknown).
async fn session_cancel(
    State(state): State<AppState>,
    Path(session): Path<String>,
) -> ApiResult<Response> {
    let cancelled = state.runs.cancel(&session);
    Ok(Json(json!({ "cancelled": cancelled })).into_response())
}

/// Spawn the workflow engine on a detached background task, framing
/// `workflow.started` before the run and `workflow.done` after it onto `events`.
///
/// While the engine runs it emits live [`WfProgress`](otto_workflow::WfProgress)
/// through the sink placed on [`WfCtx::progress`](otto_workflow::WfCtx); a
/// companion bridge task forwards each item onto `events` as a
/// `workflow.progress` envelope. The bridge ends when the sender drops: after
/// [`run_engine`] returns, the engine's internal sink clones are gone, so
/// clearing `cx.progress` and dropping `cx` closes the channel and `rx.recv()`
/// returns `None`.
///
/// `abort` is the caller-owned [`CancellationToken`] handed to the engine's
/// `drive`; the caller registers it in `registry` so `POST
/// /workflow/{session}/cancel` can cancel the run. This task removes the entry
/// once the run ends (before emitting `workflow.done`), so a later cancel on a
/// finished session returns `false`.
#[allow(clippy::too_many_arguments)]
fn spawn_workflow(
    kind: String,
    arg: String,
    session: String,
    mut cx: otto_workflow::WfCtx,
    events: broadcast::Sender<String>,
    abort: CancellationToken,
    registry: TokenRegistry,
    generation: u64,
) {
    tokio::spawn(async move {
        let _ = events.send(workflow_envelope(
            "workflow.started",
            json!({ "session": session, "kind": kind, "arg": arg }),
        ));

        // Live progress: hand the sender to the engine via `cx.progress`, and
        // forward every `WfProgress` onto `/event` from a companion bridge.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<otto_workflow::WfProgress>();
        cx.progress = Some(tx);

        let bridge_session = session.clone();
        let bridge_kind = kind.clone();
        let bridge_events = events.clone();
        let bridge = tokio::spawn(async move {
            while let Some(p) = rx.recv().await {
                let _ = bridge_events.send(workflow_envelope(
                    "workflow.progress",
                    json!({
                        "session": bridge_session, "kind": bridge_kind,
                        "task_index": p.task_index, "status": p.status, "notes": p.detail,
                    }),
                ));
            }
        });

        // Live subagent activity: same pattern as the progress bridge above.
        // Hand the sender to the engine via `cx.subagent`, and forward every
        // filtered `SubagentActivity` onto `/event` as a `workflow.subagent`
        // envelope. The engine's `tap_subagent` forwarders hold clones of this
        // sink only until each child `event_tx` closes (when that subagent's
        // `run_loop` finishes), so all clones are released by the time
        // `run_engine` returns.
        let (sub_tx, mut sub_rx) =
            tokio::sync::mpsc::unbounded_channel::<otto_workflow::SubagentActivity>();
        cx.subagent = Some(sub_tx);

        let sub_session = session.clone();
        let sub_kind = kind.clone();
        let sub_events = events.clone();
        let sub_bridge = tokio::spawn(async move {
            while let Some(a) = sub_rx.recv().await {
                let _ = sub_events.send(workflow_envelope(
                    "workflow.subagent",
                    json!({
                        "session": sub_session, "kind": sub_kind,
                        "task_index": a.task_index, "verb": a.verb, "detail": a.detail,
                    }),
                ));
            }
        });

        let result: Result<String, String> = run_engine(&kind, &arg, &cx, abort).await;

        // Drop the only remaining senders so each bridge's `rx.recv()` returns
        // `None` and the task ends. The engine cloned the sinks into `drive`, but
        // those clones are gone once `drive` returned; clearing `cx.progress` /
        // `cx.subagent` (belt-and-suspenders) and dropping `cx` releases the
        // originals so both bridges finish.
        cx.progress = None;
        cx.subagent = None;
        drop(cx);
        let _ = bridge.await;
        let _ = sub_bridge.await;

        // The run has ended (Ok or Err); deregister its abort token before the
        // terminal `workflow.done` so a cancel arriving now returns `false`.
        registry.remove(&session, generation);

        let props = match &result {
            Ok(summary) => json!({
                "session": session, "kind": kind, "ok": true, "summary": summary, "error": null
            }),
            Err(e) => json!({
                "session": session, "kind": kind, "ok": false, "summary": null, "error": e
            }),
        };
        let _ = events.send(workflow_envelope("workflow.done", props));
    });
}

/// Dispatch to the engine for `kind`, returning a human summary or an error
/// string. Drives the engine directly so the caller's `abort` token and the
/// live progress sink on [`WfCtx::progress`](otto_workflow::WfCtx) are threaded
/// through.
async fn run_engine(
    kind: &str,
    arg: &str,
    cx: &otto_workflow::WfCtx,
    abort: CancellationToken,
) -> Result<String, String> {
    match kind {
        "sdd" => {
            let tasks = read_plan_tasks(arg).await?;
            let n = tasks.len();
            otto_workflow::SddWorkflow::new(tasks)
                .drive(
                    &cx.spawner,
                    cx.store.clone(),
                    &cx.parent_session_id,
                    abort,
                    cx.progress.clone(),
                    cx.subagent.clone(),
                    &cx.worktree,
                )
                .await
                .map(|_| format!("{n} task(s) processed"))
                .map_err(|e| e.to_string())
        }
        "plan" => {
            let tasks = read_plan_tasks(arg).await?;
            let n = tasks.len();
            otto_workflow::PlanWorkflow::new(tasks)
                .drive(
                    &cx.spawner,
                    cx.store.clone(),
                    &cx.directory,
                    &cx.parent_session_id,
                    abort,
                    cx.progress.clone(),
                    cx.subagent.clone(),
                )
                .await
                .map(|_| format!("{n} task(s) executed"))
                .map_err(|e| e.to_string())
        }
        "tdd" => otto_workflow::TddWorkflow::new(arg.to_string())
            .drive(
                &cx.spawner,
                cx.runner.as_ref(),
                &cx.directory,
                &cx.parent_session_id,
                abort,
                cx.progress.clone(),
                cx.subagent.clone(),
            )
            .await
            .map(|r| format!("TDD complete: {:?}", r.regression))
            .map_err(|e| e.to_string()),
        other => Err(format!("unknown workflow kind: {other}")),
    }
}

/// Read + parse the `### Task N` sections of the plan markdown at `path`.
async fn read_plan_tasks(path: &str) -> Result<Vec<otto_workflow::PlanTask>, String> {
    let md = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("read {path}: {e}"))?;
    let tasks = otto_workflow::parse_plan_tasks(&md);
    if tasks.is_empty() {
        return Err(format!("no `### Task N` sections in {path}"));
    }
    Ok(tasks)
}

// -- doc ---------------------------------------------------------------------

/// `GET /doc` — a minimal hand-written OpenAPI-ish route map (unauthenticated).
async fn doc() -> Json<Value> {
    Json(json!({
        "openapi": "3.0.0",
        "info": { "title": "otto-server", "version": VERSION },
        "paths": {
            "/app": { "get": { "summary": "Instance info" } },
            "/path": { "get": { "summary": "Instance paths" } },
            "/config": {
                "get": { "summary": "Get config" },
                "patch": { "summary": "Merge config" }
            },
            "/agent": { "get": { "summary": "List agents" } },
            "/provider": { "get": { "summary": "List providers" } },
            "/session": {
                "get": { "summary": "List sessions" },
                "post": { "summary": "Create session" }
            },
            "/session/{id}": {
                "get": { "summary": "Get session" },
                "delete": { "summary": "Delete session" }
            },
            "/session/{id}/message": {
                "get": { "summary": "List messages" },
                "post": { "summary": "Prompt (SSE stream)" }
            },
            "/event": { "get": { "summary": "Global event stream (SSE)" } },
            "/permission": { "get": { "summary": "List pending permissions" } },
            "/permission/{request_id}/reply": { "post": { "summary": "Reply to a permission" } },
            "/session/{id}/permission-mode": { "post": { "summary": "Set a session's permission mode" } },
            "/find": { "get": { "summary": "Content search" } },
            "/find/file": { "get": { "summary": "Filename search" } },
            "/file/content": { "get": { "summary": "Read a file" } },
            "/file/list": { "get": { "summary": "Enumerate workspace files and directories" } },
            "/doc": { "get": { "summary": "This document" } }
        }
    }))
}

// -- helpers -----------------------------------------------------------------

/// Resolve the agent to run: the named agent if present, else the runtime
/// default. Cloned so it can be handed to [`Runtime::run`] / `create_session`.
fn resolve_agent(runtime: &Runtime, name: Option<&str>) -> otto_agent::AgentInfo {
    match name {
        Some(n) => runtime
            .agents()
            .iter()
            .find(|a| a.name == n)
            .unwrap_or_else(|| runtime.default_agent())
            .clone(),
        None => runtime.default_agent().clone(),
    }
}

/// Recursively deep-merge `patch` into `base` (objects merge key-wise; any other
/// value replaces). Mirrors opencode's sparse `mergeDeep`.
fn merge_json(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(base), Value::Object(patch)) => {
            for (k, v) in patch {
                merge_into(base, k, v);
            }
        }
        (base, patch) => *base = patch,
    }
}

/// Merge a single key/value into an object map.
fn merge_into(base: &mut Map<String, Value>, key: String, value: Value) {
    match base.get_mut(&key) {
        Some(slot) => merge_json(slot, value),
        None => {
            base.insert(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_envelope_shape() {
        let s = workflow_envelope(
            "workflow.started",
            serde_json::json!({"session":"ses_1","kind":"sdd","arg":"p.md"}),
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "workflow.started");
        assert_eq!(v["properties"]["kind"], "sdd");
        assert_eq!(v["properties"]["session"], "ses_1");
    }

    #[test]
    fn workflow_subagent_envelope_shape() {
        let s = workflow_envelope(
            "workflow.subagent",
            serde_json::json!({
                "session":"ses_1","kind":"sdd",
                "task_index":2,"verb":"editing","detail":"src/lib.rs",
            }),
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "workflow.subagent");
        assert_eq!(v["properties"]["session"], "ses_1");
        assert_eq!(v["properties"]["task_index"], 2);
        assert_eq!(v["properties"]["verb"], "editing");
        assert_eq!(v["properties"]["detail"], "src/lib.rs");
    }

    #[tokio::test]
    async fn workflow_registry_cancels_by_session() {
        use tokio_util::sync::CancellationToken;
        let reg: TokenRegistry = Default::default();
        let tok = CancellationToken::new();
        let generation = reg.insert("ses_1", tok.clone());
        assert!(!tok.is_cancelled());
        assert!(reg.cancel("ses_1")); // found + cancelled
        assert!(tok.is_cancelled());
        assert!(!reg.cancel("ses_missing")); // absent
        reg.remove("ses_1", generation);
        assert!(!reg.cancel("ses_1")); // gone after remove
    }

    /// A second prompt on the same session must interrupt the first turn:
    /// registering a new token cancels the one it replaces.
    #[tokio::test]
    async fn registry_insert_cancels_previous_token() {
        use tokio_util::sync::CancellationToken;
        let reg: TokenRegistry = Default::default();
        let first = CancellationToken::new();
        let second = CancellationToken::new();
        reg.insert("ses_1", first.clone());
        reg.insert("ses_1", second.clone());
        assert!(first.is_cancelled(), "prior turn's token must be cancelled");
        assert!(!second.is_cancelled(), "new turn's token must stay live");
    }

    /// When a replaced (stale) turn settles after a newer turn registered, its
    /// remove must not drop the newer turn's token.
    #[tokio::test]
    async fn registry_stale_remove_keeps_newer_token() {
        use tokio_util::sync::CancellationToken;
        let reg: TokenRegistry = Default::default();
        let first = CancellationToken::new();
        let second = CancellationToken::new();
        let gen1 = reg.insert("ses_1", first.clone());
        reg.insert("ses_1", second.clone());
        reg.remove("ses_1", gen1); // stale remove from the replaced turn
        assert!(
            reg.cancel("ses_1"),
            "newer turn must still be cancellable after a stale remove"
        );
        assert!(second.is_cancelled());
    }
}
