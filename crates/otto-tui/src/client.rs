//! Typed HTTP client for the otto server surface.

use anyhow::{Context, Result};
use futures::StreamExt;
use otto_events::LLMEvent;
use serde::Deserialize;

/// A otto HTTP client bound to one server base URL.
#[derive(Clone)]
pub struct Client {
    base: String,
    http: reqwest::Client,
    password: Option<String>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// A session summary row from `GET /session`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    /// Last-activity timestamp (server `Session.time_updated`, epoch millis).
    /// Absent/old servers → 0. Used to reopen the last-active session on start.
    #[serde(default)]
    pub time_updated: i64,
    /// Creation timestamp (server `Session.time_created`, epoch millis). Tie
    /// breaker for [`most_recent_session`].
    #[serde(default)]
    pub time_created: i64,
}

/// The session to reopen on startup: the most-recently-updated one (ties broken
/// by most-recently-created). `GET /session` returns sessions oldest-created
/// first, so the naive `.first()` reopens the OLDEST — reopen the last-active
/// session instead so a fresh launch lands where the user left off.
#[must_use]
pub fn most_recent_session(sessions: &[SessionInfo]) -> Option<&SessionInfo> {
    // `max_by_key` returns the last maximal element, so an all-equal (e.g.
    // all-zero-timestamp) list yields the last entry = newest-created, never
    // the oldest.
    sessions
        .iter()
        .max_by_key(|s| (s.time_updated, s.time_created))
}

/// An agent row from `GET /agent`.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub name: String,
}

/// A selectable `provider/model` pair derived from `GET /provider`.
#[derive(Debug, Clone)]
pub struct ModelChoice {
    pub provider: String,
    pub model: String,
}

impl ModelChoice {
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// Connect timeout, in seconds, for the built [`reqwest::Client`].
const CONNECT_TIMEOUT_SECS: u64 = 30;

/// Read [`otto_llm::transport::IDLE_TIMEOUT_ENV`]'s value and resolve the
/// effective per-read idle timeout, in seconds. Shares both the env var name
/// and the parsing rule with `otto-llm`'s `HttpTransport` (same default) via
/// [`otto_llm::transport::parse_idle_secs`] so a hung local server or a hung
/// upstream provider are both bounded the same way.
fn idle_timeout_secs() -> u64 {
    otto_llm::transport::parse_idle_secs(std::env::var(otto_llm::transport::IDLE_TIMEOUT_ENV).ok())
}

impl Client {
    #[must_use]
    pub fn new(base: impl Into<String>, password: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .read_timeout(std::time::Duration::from_secs(idle_timeout_secs()))
            .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            base: base.into(),
            http,
            password,
        }
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let rb = self.http.get(format!("{}{path}", self.base));
        self.auth(rb)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        let rb = self.http.post(format!("{}{path}", self.base));
        self.auth(rb)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.password {
            Some(pw) => rb.basic_auth("opencode", Some(pw)),
            None => rb,
        }
    }

    /// `GET /session`.
    ///
    /// # Errors
    /// Returns an error if the request fails, the server responds with a
    /// non-success status, or the body cannot be decoded.
    pub async fn sessions(&self) -> Result<Vec<SessionInfo>> {
        self.get("/session")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("decoding /session")
    }

    /// `POST /session` with `{title}`.
    ///
    /// # Errors
    /// Returns an error if the request fails, the server responds with a
    /// non-success status, or the body cannot be decoded.
    pub async fn create_session(&self, title: &str) -> Result<SessionInfo> {
        self.post("/session")
            .json(&serde_json::json!({ "title": title }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("decoding created session")
    }

    /// Start a workflow run server-side; returns the new session id. Progress
    /// arrives asynchronously on the /event stream (see `events()`).
    ///
    /// # Errors
    /// Returns an error if the request fails, the server responds with a
    /// non-success status, or the body cannot be decoded.
    pub async fn workflow(&self, kind: &str, arg: &str) -> Result<String> {
        let body = serde_json::json!({ "arg": arg });
        let v: serde_json::Value = self
            .post(&format!("/workflow/{kind}"))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v.get("session")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// Cancel a running workflow by session id. Returns whether a run was
    /// actually cancelled (`false` if none was in flight for that session).
    ///
    /// # Errors
    /// Returns an error if the request fails, the server responds with a
    /// non-success status, or the body cannot be decoded.
    pub async fn cancel_workflow(&self, session: &str) -> Result<bool> {
        let v: serde_json::Value = self
            .post(&format!("/workflow/{session}/cancel"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v.get("cancelled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false))
    }

    /// Interrupt a running prompt turn by session id (without ending the
    /// session). Returns whether a turn was actually cancelled (`false` if none
    /// was in flight).
    ///
    /// # Errors
    /// Returns an error if the request fails, the server responds with a
    /// non-success status, or the body cannot be decoded.
    pub async fn cancel_run(&self, session: &str) -> Result<bool> {
        let v: serde_json::Value = self
            .post(&format!("/session/{session}/cancel"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v.get("cancelled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false))
    }

    /// `GET /session/{id}/message` — raw message rows (folded by the caller).
    ///
    /// # Errors
    /// Returns an error if the request fails, the server responds with a
    /// non-success status, or the body cannot be decoded.
    pub async fn history(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        self.get(&format!("/session/{id}/message"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("decoding history")
    }

    /// `GET /agent`.
    ///
    /// # Errors
    /// Returns an error if the request fails, the server responds with a
    /// non-success status, or the body cannot be decoded.
    pub async fn agents(&self) -> Result<Vec<AgentInfo>> {
        self.get("/agent")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("decoding /agent")
    }

    /// `GET /provider`, flattened to `provider/model` choices.
    ///
    /// # Errors
    /// Returns an error if the request fails, the server responds with a
    /// non-success status, or the body cannot be decoded as JSON.
    pub async fn models(&self) -> Result<Vec<ModelChoice>> {
        let v: serde_json::Value = self
            .get("/provider")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("decoding /provider")?;
        Ok(flatten_models(&v))
    }

    /// `POST /session/{id}/message` — stream the response as `LLMEvent`s.
    ///
    /// # Errors
    /// Returns an error if the request fails or the server responds with a
    /// non-success status.
    pub async fn prompt(
        &self,
        id: &str,
        text: &str,
        agent: Option<&str>,
        model: Option<&str>,
        files: &[String],
    ) -> Result<impl futures::Stream<Item = LLMEvent>> {
        let body = build_prompt_body(text, agent, model, files);
        let resp = self
            .post(&format!("/session/{id}/message"))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        // A mid-turn connection loss becomes a visible `provider-error` event
        // (rendered as an error row + header) rather than a silent stream end.
        Ok(sse_stream(resp, crate::sse::decode_llm, |msg| {
            Some(LLMEvent::ProviderError {
                message: format!("lost connection to otto server: {msg}"),
                classification: None,
                retryable: None,
                provider_metadata: None,
            })
        }))
    }

    /// `GET /file/list?limit=` — candidate paths for the attachment picker.
    ///
    /// # Errors
    /// Returns an error if the request fails, the server responds with a
    /// non-success status, or the body cannot be decoded as JSON.
    pub async fn list_files(&self, limit: usize) -> Result<(Vec<String>, bool)> {
        let json: serde_json::Value = self
            .get(&format!("/file/list?limit={limit}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("decoding /file/list")?;
        let files = json["files"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let truncated = json["truncated"].as_bool().unwrap_or(false);
        Ok((files, truncated))
    }

    /// `GET /event` — stream decoded envelopes.
    ///
    /// # Errors
    /// Returns an error if the request fails or the server responds with a
    /// non-success status.
    pub async fn events(&self) -> Result<impl futures::Stream<Item = crate::sse::ServerEvent>> {
        let resp = self.get("/event").send().await?.error_for_status()?;
        // The global event bus is best-effort; drop a transport error quietly.
        Ok(sse_stream(
            resp,
            |f| Some(crate::sse::decode_event(f)),
            |_| None,
        ))
    }

    /// `POST /permission/{request_id}/reply`.
    ///
    /// # Errors
    /// Returns an error if the request fails or the server responds with a
    /// non-success status.
    pub async fn reply_permission(
        &self,
        request_id: &str,
        reply: &str,
        message: Option<&str>,
    ) -> Result<()> {
        let mut body = serde_json::json!({ "reply": reply });
        if let Some(m) = message {
            body["message"] = m.into();
        }
        self.post(&format!("/permission/{request_id}/reply"))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// `POST /session/{session_id}/permission-mode`.
    ///
    /// # Errors
    /// Returns an error if the request fails or the server responds non-success.
    pub async fn set_permission_mode(&self, session_id: &str, mode: &str) -> Result<()> {
        self.post(&format!("/session/{session_id}/permission-mode"))
            .json(&serde_json::json!({ "mode": mode }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Build the JSON body for `POST /session/{id}/message`. `files` is included
/// as `[{"path": p}, ...]` only when non-empty, so a bare-text prompt keeps
/// the same shape it had before attachments existed.
pub(crate) fn build_prompt_body(
    text: &str,
    agent: Option<&str>,
    model: Option<&str>,
    files: &[String],
) -> serde_json::Value {
    let mut body = serde_json::json!({ "prompt": text });
    if let Some(a) = agent {
        body["agent"] = a.into();
    }
    if let Some(m) = model {
        body["model"] = m.into();
    }
    if !files.is_empty() {
        body["files"] = serde_json::Value::Array(
            files
                .iter()
                .map(|p| serde_json::json!({ "path": p }))
                .collect(),
        );
    }
    body
}

/// Turn a streaming SSE `reqwest::Response` into a stream of `T`, decoding each
/// frame with `decode` and dropping frames that decode to `None`.
///
/// A mid-stream transport failure (e.g. the idle read-timeout tripping on a
/// dead connection) is NOT silently swallowed: `on_error` maps it to a final
/// item (or `None` to drop) and then the stream ends. This is why a lost
/// connection surfaces as a visible error instead of a silent hang.
fn sse_stream<T: 'static>(
    resp: reqwest::Response,
    decode: impl Fn(&str) -> Option<T> + Send + 'static,
    on_error: impl Fn(String) -> Option<T> + Send + 'static,
) -> impl futures::Stream<Item = T> {
    let mut decoder = crate::sse::FrameDecoder::new();
    resp.bytes_stream()
        // EOF sentinel so the decoder can flush a trailing frame the server
        // never terminated with \n\n before the connection closed.
        .map(Some)
        .chain(futures::stream::iter([None]))
        .scan(false, move |ended, r| {
            let items: Vec<T> = if *ended {
                Vec::new()
            } else {
                match r {
                    Some(Ok(chunk)) => decoder
                        .push(chunk.as_ref())
                        .into_iter()
                        .filter_map(|f| decode(&f))
                        .collect(),
                    Some(Err(e)) => {
                        *ended = true;
                        // Flush frames that arrived before the failure, then
                        // append the mapped error item.
                        let mut items: Vec<T> = decoder
                            .flush()
                            .into_iter()
                            .filter_map(|f| decode(&f))
                            .collect();
                        items.extend(on_error(e.to_string()));
                        items
                    }
                    None => {
                        *ended = true;
                        decoder
                            .flush()
                            .into_iter()
                            .filter_map(|f| decode(&f))
                            .collect()
                    }
                }
            };
            // Stop once the stream has settled and there is nothing left to
            // emit; otherwise keep forwarding decoded items.
            futures::future::ready((!*ended || !items.is_empty()).then_some(items))
        })
        .flat_map(futures::stream::iter)
}

/// Extract `provider/model` pairs from a `/provider` payload.
///
/// The real `otto-server` handler (`provider_list`) emits:
/// ```json
/// {
///   "providers": [
///     { "id": "anthropic", "name": "anthropic",
///       "models": [ { "id": "claude-3", "name": "claude-3" } ] }
///   ],
///   "default": { "providerID": "anthropic", "modelID": "claude-3" }
/// }
/// ```
/// i.e. `models` is an *array* of `{id, name}` objects, not a map. This stays
/// tolerant of a `models` object-map shape too (keys as model ids), in case an
/// alternate/older server emits that instead, and of a bare top-level array in
/// place of `{"providers": [...]}`.
fn flatten_models(v: &serde_json::Value) -> Vec<ModelChoice> {
    let arr = v
        .get("providers")
        .and_then(|p| p.as_array())
        .or_else(|| v.as_array());
    let Some(arr) = arr else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for p in arr {
        let provider = p
            .get("id")
            .or_else(|| p.get("name"))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string();
        if provider.is_empty() {
            continue;
        }
        let Some(models) = p.get("models") else {
            continue;
        };
        if let Some(list) = models.as_array() {
            for m in list {
                if let Some(model) = m
                    .get("id")
                    .or_else(|| m.get("name"))
                    .and_then(|s| s.as_str())
                {
                    out.push(ModelChoice {
                        provider: provider.clone(),
                        model: model.to_string(),
                    });
                }
            }
        } else if let Some(map) = models.as_object() {
            for key in map.keys() {
                out.push(ModelChoice {
                    provider: provider.clone(),
                    model: key.clone(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(id: &str, updated: i64, created: i64) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            title: None,
            time_updated: updated,
            time_created: created,
        }
    }

    #[test]
    fn most_recent_session_picks_latest_updated() {
        // `GET /session` returns sessions oldest-created first. The one to
        // reopen is the most-recently-UPDATED (last active), even when it isn't
        // the newest-created — so a fresh launch lands where the user left off.
        let sessions = vec![
            sess("old", 100, 1),
            sess("newest_created", 150, 300),
            sess("last_active", 500, 50),
        ];
        assert_eq!(most_recent_session(&sessions).unwrap().id, "last_active");
    }

    #[test]
    fn most_recent_session_empty_is_none() {
        assert!(most_recent_session(&[]).is_none());
    }

    #[test]
    fn most_recent_session_falls_back_to_newest_created_when_no_timestamps() {
        // If the server omitted timestamps (all zero), reopen the newest-created
        // — the last element, since the list is oldest-first. Still never the
        // oldest (the pre-fix `.first()` bug).
        let z = vec![sess("a", 0, 0), sess("b", 0, 0), sess("c", 0, 0)];
        assert_eq!(most_recent_session(&z).unwrap().id, "c");
    }

    #[test]
    fn session_info_reads_snake_case_time_fields() {
        // Guards the field-name contract with the server (`Session` serializes
        // flat snake_case, no rename_all) — if this drifts, the timestamps
        // silently read as 0 and reopen degrades to newest-created.
        let j = r#"{"id":"ses_1","title":"t","time_created":10,"time_updated":20}"#;
        let s: SessionInfo = serde_json::from_str(j).unwrap();
        assert_eq!(s.time_created, 10);
        assert_eq!(s.time_updated, 20);
    }

    /// Matches the exact shape emitted by `otto_server::provider_list`
    /// (`crates/otto-server/src/lib.rs`): `providers[].models` is an array of
    /// `{id, name}` objects, not an object map.
    #[test]
    fn flatten_models_reads_real_provider_list_shape() {
        let v = serde_json::json!({
            "providers": [
                {
                    "id": "anthropic",
                    "name": "anthropic",
                    "models": [
                        { "id": "claude-3", "name": "claude-3" },
                        { "id": "claude-3-5", "name": "claude-3-5" },
                    ],
                },
                {
                    "id": "openai",
                    "name": "openai",
                    "models": [
                        { "id": "gpt-4o", "name": "gpt-4o" },
                    ],
                },
            ],
            "default": { "providerID": "anthropic", "modelID": "claude-3" },
        });

        let mut ids: Vec<String> = flatten_models(&v).iter().map(ModelChoice::id).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "anthropic/claude-3".to_string(),
                "anthropic/claude-3-5".to_string(),
                "openai/gpt-4o".to_string(),
            ]
        );
    }

    #[test]
    fn flatten_models_empty_on_shape_mismatch() {
        let v = serde_json::json!({ "unexpected": true });
        assert!(flatten_models(&v).is_empty());
    }

    #[test]
    fn prompt_body_includes_files_when_present() {
        let body = build_prompt_body("hi", None, None, &["a.rs".to_string(), "b.txt".to_string()]);
        assert_eq!(body["prompt"], "hi");
        let files = body["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["path"], "a.rs");
    }

    #[test]
    fn prompt_body_omits_files_when_empty() {
        let body = build_prompt_body("hi", None, None, &[]);
        assert!(body.get("files").is_none(), "no files key when empty");
    }

    #[test]
    fn client_new_builds_without_panic() {
        let _client = Client::new("http://x", None);
    }

    #[test]
    fn debug_redacts_password() {
        let client = Client::new("http://x", Some("secret".into()));
        let debug = format!("{client:?}");
        assert!(!debug.contains("secret"), "password leaked: {debug}");
        assert!(
            debug.contains("redacted"),
            "missing redaction marker: {debug}"
        );
    }
}
