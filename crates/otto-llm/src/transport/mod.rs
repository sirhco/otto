//! HTTP transport + SSE framing.
//!
//! Port of opencode `packages/llm/src/route/transport/` (`index.ts` +
//! `http.ts`). A [`Transport`] takes a fully-prepared HTTP request and yields
//! the SSE `data:` payload frames; [`HttpTransport`] is the `reqwest`-backed
//! implementation, and [`sse`] provides the reusable framing.

pub mod aws_event_stream;
pub mod bedrock;
pub mod sse;

use std::collections::BTreeMap;

use futures::stream::{BoxStream, StreamExt, TryStreamExt};

use crate::error::LLMError;

/// A prepared HTTP request ready to send.
///
/// Port of the request the transport receives in `route/transport/http.ts`:
/// the URL, headers (including auth), and the serialized JSON body.
#[derive(Debug, Clone)]
pub struct PreparedHttp {
    /// Fully-resolved request URL (base + path + query).
    pub url: String,
    /// Request headers, including content-type and any auth headers.
    pub headers: BTreeMap<String, String>,
    /// Serialized request body bytes.
    pub body: Vec<u8>,
}

/// A transport that turns a prepared request into a stream of SSE frames.
///
/// Port of the `Transport` interface in `route/transport/index.ts`. Each
/// yielded [`String`] is one SSE `data:` payload (empty payloads and `[DONE]`
/// are already filtered out).
pub trait Transport: Send + Sync {
    /// POST `req` and stream its SSE `data:` payload frames.
    fn frames(&self, req: PreparedHttp) -> BoxStream<'static, Result<String, LLMError>>;
}

/// A `reqwest`-backed HTTP transport that POSTs JSON and streams SSE.
///
/// Port of `httpJson` / `sse` in `route/transport/http.ts`.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
}

impl Default for HttpTransport {
    /// Delegates to [`HttpTransport::new`] so the derived and explicit
    /// construction paths cannot diverge (a derived `Default` would build an
    /// unconfigured `reqwest::Client` with no idle/connect timeout).
    fn default() -> Self {
        Self::new()
    }
}

/// Env var overriding the per-read (idle) stream timeout, in seconds.
///
/// Applies to `.read_timeout` on the built [`reqwest::Client`]: reqwest 0.12
/// resets this timer on every chunk received, so it is an *idle* timeout, not
/// a total-request timeout. A stalled SSE stream (provider goes silent
/// mid-response) will error out after this many seconds instead of hanging
/// forever.
///
/// `pub` so other crates building their own `reqwest::Client` (e.g.
/// `otto-tui`'s local HTTP client) can read the same env var instead of
/// duplicating the name as a literal.
pub const IDLE_TIMEOUT_ENV: &str = "otto_STREAM_IDLE_TIMEOUT_SECS";

/// Default idle timeout, in seconds, when the env var is unset or unparseable.
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 120;

/// Connect timeout, in seconds, for the built [`reqwest::Client`].
const CONNECT_TIMEOUT_SECS: u64 = 30;

/// Parse the raw `otto_STREAM_IDLE_TIMEOUT_SECS` value into a timeout in
/// seconds. Missing or unparseable input falls back to
/// [`DEFAULT_IDLE_TIMEOUT_SECS`].
///
/// Pure function so it can be unit-tested without mutating process env vars.
/// Exported so other crates (e.g. `otto-tui`'s local HTTP client) can share
/// the same parsing rule without duplicating it.
#[must_use]
pub fn parse_idle_secs(raw: Option<String>) -> u64 {
    raw.and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS)
}

/// Read [`IDLE_TIMEOUT_ENV`] and resolve the effective idle timeout, in
/// seconds.
fn idle_timeout_secs() -> u64 {
    parse_idle_secs(std::env::var(IDLE_TIMEOUT_ENV).ok())
}

/// Build a [`reqwest::Client`] with a per-read idle timeout (see
/// [`idle_timeout_secs`]) and a fixed connect timeout, falling back to an
/// unconfigured client if building the configured one fails.
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .read_timeout(std::time::Duration::from_secs(idle_timeout_secs()))
        .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

impl HttpTransport {
    /// A transport with a [`reqwest::Client`] configured with a per-read idle
    /// timeout (see [`idle_timeout_secs`]) and a 30s connect timeout.
    #[must_use]
    pub fn new() -> Self {
        HttpTransport {
            client: build_client(),
        }
    }

    /// A transport reusing an existing [`reqwest::Client`].
    #[must_use]
    pub fn with_client(client: reqwest::Client) -> Self {
        HttpTransport { client }
    }
}

impl Transport for HttpTransport {
    fn frames(&self, req: PreparedHttp) -> BoxStream<'static, Result<String, LLMError>> {
        let client = self.client.clone();
        let stream = async_stream::try_stream! {
            let mut builder = client.post(&req.url).body(req.body);
            for (name, value) in &req.headers {
                builder = builder.header(name, value);
            }
            let resp = builder
                .send()
                .await
                .map_err(|e| LLMError::Transport(e.to_string()))?;

            let status = resp.status();
            if !status.is_success() {
                let retry_after = parse_retry_after(resp.headers());
                let message = resp.text().await.unwrap_or_default();
                Err(LLMError::Http {
                    status: status.as_u16(),
                    message,
                    retry_after,
                })?;
            } else {
                let byte_stream = resp
                    .bytes_stream()
                    .map_err(|e| LLMError::Transport(e.to_string()));
                let frames = sse::decode_sse(byte_stream);
                futures::pin_mut!(frames);
                while let Some(frame) = frames.next().await {
                    yield frame?;
                }
            }
        };
        stream.boxed()
    }
}

/// Parse a `Retry-After` header value in its integer-seconds form
/// (e.g. `Retry-After: 3`). The HTTP-date form is not supported (no date
/// crate in this workspace); such values yield `None` and fall back to the
/// default backoff. Anthropic/OpenAI send integer seconds.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(std::time::Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_idle_secs_unset_defaults_to_120() {
        assert_eq!(parse_idle_secs(None), DEFAULT_IDLE_TIMEOUT_SECS);
    }

    #[test]
    fn parse_idle_secs_valid_number() {
        assert_eq!(parse_idle_secs(Some("45".to_string())), 45);
    }

    #[test]
    fn parse_idle_secs_garbage_defaults_to_120() {
        assert_eq!(
            parse_idle_secs(Some("notanumber".to_string())),
            DEFAULT_IDLE_TIMEOUT_SECS
        );
    }

    #[test]
    fn parse_idle_secs_empty_string_defaults_to_120() {
        assert_eq!(
            parse_idle_secs(Some(String::new())),
            DEFAULT_IDLE_TIMEOUT_SECS
        );
    }

    #[test]
    fn parse_idle_secs_negative_defaults_to_120() {
        // u64 parse rejects a leading '-', so this exercises the unparseable path.
        assert_eq!(
            parse_idle_secs(Some("-5".to_string())),
            DEFAULT_IDLE_TIMEOUT_SECS
        );
    }

    #[test]
    fn http_transport_new_builds_without_panic() {
        let _transport = HttpTransport::new();
    }

    #[test]
    fn http_transport_default_builds_without_panic() {
        // Guards against `Default` drifting from `new()` (M-2): both must
        // route through `build_client()` so neither construction path
        // silently loses the idle/connect timeouts.
        let _transport = HttpTransport::default();
    }

    #[test]
    fn http_transport_with_client_builds_without_panic() {
        let _transport = HttpTransport::with_client(reqwest::Client::new());
    }

    #[test]
    fn parse_retry_after_integer_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3".parse().unwrap());
        assert_eq!(
            parse_retry_after(&headers),
            Some(std::time::Duration::from_secs(3))
        );
    }

    #[test]
    fn parse_retry_after_garbage_value_is_none() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "soon".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn parse_retry_after_http_date_is_none() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn parse_retry_after_missing_header_is_none() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
    }
}
