//! The native OpenAI provider.
//!
//! Port of opencode `packages/llm/src/providers/openai.ts` (the Chat
//! Completions facade). Serves the [`OpenAIChat`] protocol over
//! `POST {baseURL}/chat/completions` with `Bearer` auth, falling back to the
//! `OPENAI_API_KEY` env var (`AuthOptions.bearer(options, "OPENAI_API_KEY")`).

use std::sync::Arc;

use crate::auth::{AuthDef, Secret};
use crate::model::Model;
use crate::protocols::openai_chat::OpenAIChat;
use crate::protocols::openai_responses::{OpenAIResponses, should_use_responses};
use crate::registry;
use crate::route::{Endpoint, GenericRoute, Route};
use crate::transport::Transport;

use super::Provider;

/// The default OpenAI API base URL (`openai.ts` endpoint baseURL).
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// The Chat Completions path appended to the base URL.
const PATH: &str = "/chat/completions";
/// The `OPENAI_API_KEY` env var read when no explicit key is given.
const API_KEY_ENV: &str = "OPENAI_API_KEY";
/// The provider id (`ProviderID.make("openai")`).
const PROVIDER_ID: &str = "openai";
/// The route id served by this provider (`OpenAIChat::id`).
const ROUTE_ID: &str = "openai-chat";

/// The native OpenAI provider, generic over the [`Transport`].
///
/// Port of the `configure`/`chat` facade in `openai.ts`.
pub struct OpenAI<T> {
    api_key: Option<Secret>,
    base_url: String,
    transport: Arc<T>,
}

impl<T> OpenAI<T>
where
    T: Transport + 'static,
{
    /// Configure the provider with an optional API key (falling back to
    /// `OPENAI_API_KEY`) and a transport.
    #[must_use]
    pub fn new(api_key: Option<Secret>, transport: Arc<T>) -> Self {
        OpenAI {
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            transport,
        }
    }

    /// Override the base URL (e.g. a mock server in tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// The resolved endpoint (`{baseURL}/chat/completions`).
    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::new(self.base_url.clone(), PATH)
    }

    /// The auth strategy: `Bearer` from the explicit key, falling back to
    /// `OPENAI_API_KEY`.
    #[must_use]
    pub fn auth(&self) -> AuthDef {
        let env = AuthDef::bearer(Secret::config(API_KEY_ENV));
        match &self.api_key {
            Some(secret) => AuthDef::bearer(secret.clone()).or_else(env),
            None => env,
        }
    }
}

impl<T> Provider for OpenAI<T>
where
    T: Transport + 'static,
{
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn route(&self, model_id: &str) -> Box<dyn Route> {
        if should_use_responses(model_id) {
            let endpoint = Endpoint::new(self.base_url.clone(), "/responses");
            Box::new(GenericRoute::new(
                Arc::new(OpenAIResponses),
                endpoint,
                self.auth(),
                self.transport.clone(),
            ))
        } else {
            Box::new(GenericRoute::new(
                Arc::new(OpenAIChat),
                self.endpoint(),
                self.auth(),
                self.transport.clone(),
            ))
        }
    }

    fn model(&self, model_id: &str) -> Model {
        registry::model_or_default(PROVIDER_ID, model_id, ROUTE_ID)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::transport::HttpTransport;

    /// `route()` must pick the OpenAI Responses protocol for gpt-5-class
    /// models and the OpenAI Chat protocol for everything else.
    #[test]
    fn gpt5_uses_responses_route_others_chat() {
        let p = OpenAI::new(
            Some(Secret::literal("sk-test")),
            Arc::new(HttpTransport::new()),
        );
        assert_eq!(p.route("gpt-5").id(), "openai-responses");
        assert_eq!(p.route("gpt-4o").id(), "openai-chat");
    }
}
