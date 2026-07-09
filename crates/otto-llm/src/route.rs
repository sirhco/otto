//! The route: Protocol + Endpoint + Auth + Transport, and the streaming
//! pipeline that binds them.
//!
//! Port of opencode `packages/llm/src/route/client.ts` — specifically the
//! `streamPrepared` pipeline (lines ~279-295): build body → encode JSON → POST
//! via the transport → for each frame `decode_event`, apply the inclusive
//! `terminal` take-until, `mapAccum` via `initial`/`step`, then flush `on_halt`.
//!
//! [`Route`] is the object-safe erasure of a concrete [`Protocol`] +
//! [`Transport`], so providers can hand back a `Box<dyn Route>`.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::stream::{BoxStream, Stream, StreamExt};
use otto_events::LLMEvent;

use crate::auth::AuthDef;
use crate::error::LLMError;
use crate::protocol::Protocol;
use crate::request::LLMRequest;
use crate::transport::{PreparedHttp, Transport};

/// A resolved HTTP endpoint for a route.
///
/// Port of the endpoint config assembled in `route/client.ts`.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Base URL, e.g. `https://api.anthropic.com`.
    pub base_url: String,
    /// Path appended to the base, e.g. `/v1/messages`.
    pub path: String,
    /// Optional query-string parameters.
    pub query: Option<BTreeMap<String, String>>,
}

impl Endpoint {
    /// Build an endpoint from a base URL and path with no query.
    #[must_use]
    pub fn new(base_url: impl Into<String>, path: impl Into<String>) -> Self {
        Endpoint {
            base_url: base_url.into(),
            path: path.into(),
            query: None,
        }
    }

    /// The fully-resolved request URL (base + path + query).
    #[must_use]
    pub fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = self.path.trim_start_matches('/');
        let mut url = if path.is_empty() {
            base.to_string()
        } else {
            format!("{base}/{path}")
        };
        if let Some(query) = &self.query
            && !query.is_empty()
        {
            let qs = query
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&qs);
        }
        url
    }
}

/// The core streaming pipeline (`route/client.ts` `streamPrepared`).
///
/// Builds the request body, encodes it, prepares headers (content-type +
/// auth), POSTs through `transport`, and folds each decoded provider event
/// through `protocol.step`. The event that satisfies [`Protocol::terminal`] is
/// processed *then* ends the stream (inclusive take-until); [`Protocol::on_halt`]
/// is flushed afterwards.
pub fn run_stream<P, T>(
    protocol: Arc<P>,
    endpoint: Endpoint,
    auth: AuthDef,
    transport: Arc<T>,
    route_headers: BTreeMap<String, String>,
    req: LLMRequest,
) -> impl Stream<Item = Result<LLMEvent, LLMError>> + Send
where
    P: Protocol + 'static,
    T: Transport + 'static,
{
    async_stream::try_stream! {
        // body.from → encode JSON.
        let body = protocol.build_body(&req)?;
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| LLMError::Body(e.to_string()))?;

        // Prepare headers: content-type + static route headers + per-request
        // headers + auth. Static route headers (e.g. Anthropic's
        // `anthropic-version`) are applied by the provider before auth; a
        // per-request header of the same name may still override them.
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        for (k, v) in &route_headers {
            headers.insert(k.clone(), v.clone());
        }
        if let Some(http) = &req.http
            && let Some(extra) = &http.headers
        {
            for (k, v) in extra {
                headers.insert(k.clone(), v.clone());
            }
        }
        auth.apply(&mut headers)?;

        // Merge per-request query params into the endpoint.
        let mut endpoint = endpoint;
        if let Some(http) = &req.http
            && let Some(extra) = &http.query
        {
            let query = endpoint.query.get_or_insert_with(BTreeMap::new);
            for (k, v) in extra {
                query.insert(k.clone(), v.clone());
            }
        }

        let prepared = PreparedHttp {
            url: endpoint.url(),
            headers,
            body: body_bytes,
        };

        let mut state = protocol.initial(&req);
        let frames = transport.frames(prepared);
        futures::pin_mut!(frames);

        // Gateways (litellm, proxies, HTML error pages) sometimes interleave
        // frames the protocol cannot decode. One bad frame used to abort the
        // whole turn with a non-retryable `EventDecode`; instead skip it with
        // a warning, and only fail — retryably — when the garbage dominates:
        // too many skips, or a stream that ends with skips and zero decoded
        // frames (which would otherwise surface as an empty attempt).
        const MAX_SKIPPED_FRAMES: u32 = 20;
        let mut skipped: u32 = 0;
        let mut decoded: u64 = 0;

        while let Some(frame) = frames.next().await {
            let frame = frame?;
            let event = match protocol.decode_event(&frame) {
                Ok(event) => event,
                Err(e) => {
                    skipped += 1;
                    let preview: String = frame.chars().take(120).collect();
                    tracing::warn!(skipped, error = %e, frame = %preview, "skipping undecodable stream frame");
                    if skipped >= MAX_SKIPPED_FRAMES {
                        Err(LLMError::ProviderRetryable(format!(
                            "{skipped} undecodable stream frames (last: {e})"
                        )))?;
                    }
                    continue;
                }
            };
            decoded += 1;
            let is_terminal = protocol.terminal(&event);
            for out in protocol.step(&mut state, event)? {
                yield out;
            }
            if is_terminal {
                break;
            }
        }

        if skipped > 0 && decoded == 0 {
            Err(LLMError::ProviderRetryable(format!(
                "stream carried only undecodable frames ({skipped} skipped)"
            )))?;
        }

        // Flush any dangling state (stream.onHalt).
        for out in protocol.on_halt(&mut state) {
            yield out;
        }
    }
}

/// An object-safe route: a bound Protocol + Endpoint + Auth + Transport.
///
/// Port of the `Route` abstraction in `route/client.ts`. Erases the protocol's
/// associated types so a provider can return `Box<dyn Route>` /
/// `Arc<dyn Route>`.
pub trait Route: Send + Sync {
    /// The route id (the protocol id).
    fn id(&self) -> &str;

    /// Stream provider-neutral events for `req`.
    fn stream(&self, req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>>;
}

/// A concrete route wrapping a [`Protocol`] + [`Endpoint`] + [`AuthDef`] +
/// [`Transport`].
///
/// Port of the generic route constructed in `route/client.ts`; implements the
/// object-safe [`Route`] by delegating to [`run_stream`].
pub struct GenericRoute<P, T> {
    protocol: Arc<P>,
    endpoint: Endpoint,
    auth: AuthDef,
    transport: Arc<T>,
    headers: BTreeMap<String, String>,
}

impl<P, T> GenericRoute<P, T>
where
    P: Protocol + 'static,
    T: Transport + 'static,
{
    /// Bind a protocol, endpoint, auth strategy, and transport into a route.
    pub fn new(protocol: Arc<P>, endpoint: Endpoint, auth: AuthDef, transport: Arc<T>) -> Self {
        GenericRoute {
            protocol,
            endpoint,
            auth,
            transport,
            headers: BTreeMap::new(),
        }
    }

    /// Attach static headers sent on every request through this route (e.g.
    /// Anthropic's `anthropic-version`). Applied before auth; a matching
    /// per-request header still overrides them.
    #[must_use]
    pub fn with_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        self.headers = headers;
        self
    }
}

impl<P, T> Route for GenericRoute<P, T>
where
    P: Protocol + 'static,
    T: Transport + 'static,
{
    fn id(&self) -> &str {
        self.protocol.id()
    }

    fn stream(&self, req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
        run_stream(
            self.protocol.clone(),
            self.endpoint.clone(),
            self.auth.clone(),
            self.transport.clone(),
            self.headers.clone(),
            req,
        )
        .boxed()
    }
}
