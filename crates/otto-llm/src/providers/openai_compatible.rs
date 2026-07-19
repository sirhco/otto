//! Generic OpenAI-compatible provider — the catch-all wire shape for local
//! and gateway tooling (litellm, ollama, vllm, or any other endpoint that
//! speaks the OpenAI Chat Completions shape).
//!
//! Port of opencode `packages/llm/src/providers/openai-compatible.ts`. Reuses
//! the [`OpenAICompatibleChat`] protocol at a caller-supplied `baseURL`. Auth
//! is `Bearer` with **no** env-var fallback list (`AuthOptions.bearer(input,
//! [])`): pass an explicit [`Secret`] (a literal, or [`Secret::config`] to
//! read a provider-specific env var) or `None` for no auth header.

use std::sync::Arc;

use crate::auth::{AuthDef, Secret};
use crate::model::Model;
use crate::protocols::openai_compatible::OpenAICompatibleChat;
use crate::registry;
use crate::route::{Endpoint, GenericRoute, Route};
use crate::transport::Transport;

use super::Provider;

/// The Chat Completions path appended to the base URL.
const PATH: &str = "/chat/completions";
/// The route id served by this provider (`OpenAICompatibleChat::id`).
const ROUTE_ID: &str = "openai-compatible-chat";

/// A generic OpenAI-compatible provider, generic over the [`Transport`].
///
/// Port of the `configure` facade in `openai-compatible.ts`. Construct one
/// with [`OpenAICompatible::new`].
pub struct OpenAICompatible<T> {
    provider: String,
    base_url: String,
    api_key: Option<Secret>,
    transport: Arc<T>,
}

impl<T> OpenAICompatible<T>
where
    T: Transport + 'static,
{
    /// Configure a compatible provider with a `provider` name, a `base_url`, an
    /// optional API key, and a transport.
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<Secret>,
        transport: Arc<T>,
    ) -> Self {
        OpenAICompatible {
            provider: provider.into(),
            base_url: base_url.into(),
            api_key,
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

    /// The auth strategy: `Bearer` from the explicit key (no env fallback), or
    /// no auth header when no key is configured.
    #[must_use]
    pub fn auth(&self) -> AuthDef {
        match &self.api_key {
            Some(secret) => AuthDef::bearer(secret.clone()),
            None => AuthDef::none(),
        }
    }
}

impl<T> Provider for OpenAICompatible<T>
where
    T: Transport + 'static,
{
    fn id(&self) -> &str {
        &self.provider
    }

    fn route(&self, _model_id: &str) -> Box<dyn Route> {
        Box::new(GenericRoute::new(
            Arc::new(OpenAICompatibleChat),
            self.endpoint(),
            self.auth(),
            self.transport.clone(),
        ))
    }

    fn model(&self, model_id: &str) -> Model {
        registry::model_or_default(&self.provider, model_id, ROUTE_ID)
    }
}
