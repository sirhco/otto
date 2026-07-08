//! The high-level LLM client.
//!
//! Port of `LLMClient` in opencode `packages/llm/src/route/client.ts`:
//! [`LLMClient::stream`] surfaces the raw event stream, and
//! [`LLMClient::generate`] folds it into an [`LLMResponse`] — erroring if the
//! stream ends without a terminal `finish` (client.ts lines ~382-391).

use std::sync::Arc;

use futures::stream::{BoxStream, StreamExt};
use otto_events::LLMEvent;

use crate::error::LLMError;
use crate::request::LLMRequest;
use crate::response::LLMResponse;
use crate::route::Route;

/// A client bound to a single [`Route`].
///
/// Port of `LLMClient` in `route/client.ts`.
#[derive(Clone)]
pub struct LLMClient {
    route: Arc<dyn Route>,
}

impl LLMClient {
    /// Build a client over `route`.
    #[must_use]
    pub fn new(route: Arc<dyn Route>) -> Self {
        LLMClient { route }
    }

    /// The id of the underlying route.
    #[must_use]
    pub fn route_id(&self) -> &str {
        self.route.id()
    }

    /// Stream provider-neutral [`LLMEvent`]s for `req`.
    #[must_use]
    pub fn stream(&self, req: LLMRequest) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
        self.route.stream(req)
    }

    /// Run `req` to completion, folding the event stream into an
    /// [`LLMResponse`].
    ///
    /// Port of `LLMClient.generate` (client.ts ~382-391).
    ///
    /// # Errors
    /// Propagates any stream error, and returns [`LLMError::NoTerminalFinish`]
    /// if the stream ends without a terminal `finish` event.
    pub async fn generate(&self, req: LLMRequest) -> Result<LLMResponse, LLMError> {
        let mut stream = self.route.stream(req);
        let mut response = LLMResponse::empty();
        while let Some(event) = stream.next().await {
            response = response.reduce(event?);
        }
        if response.complete().is_none() {
            return Err(LLMError::NoTerminalFinish);
        }
        Ok(response)
    }
}
