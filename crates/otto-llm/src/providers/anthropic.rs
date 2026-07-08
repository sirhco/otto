//! The native Anthropic provider.
//!
//! Port of opencode `packages/llm/src/providers/anthropic.ts`. Serves the
//! [`AnthropicMessages`] protocol over `POST {baseURL}/messages`, authenticates
//! with the `x-api-key` header (NOT bearer), and stamps the required
//! `anthropic-version` header on every request.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::auth::{AuthDef, Secret};
use crate::model::Model;
use crate::protocols::anthropic_messages::{ANTHROPIC_VERSION, AnthropicMessages};
use crate::registry;
use crate::route::{Endpoint, GenericRoute, Route};
use crate::transport::Transport;

use super::Provider;

/// The default Anthropic API base URL (`anthropic.ts` endpoint baseURL).
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
/// The Messages path appended to the base URL.
const PATH: &str = "/messages";
/// The `ANTHROPIC_API_KEY` env var read when no explicit key is given
/// (`Auth.config("ANTHROPIC_API_KEY")`).
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
/// The provider id (`ProviderID.make("anthropic")`).
const PROVIDER_ID: &str = "anthropic";
/// The route id served by this provider (`AnthropicMessages::id`).
const ROUTE_ID: &str = "anthropic";

/// The native Anthropic provider, generic over the [`Transport`].
///
/// Port of the `configure`/`model` facade in `anthropic.ts`.
pub struct Anthropic<T> {
    api_key: Option<Secret>,
    base_url: String,
    transport: Arc<T>,
}

impl<T> Anthropic<T>
where
    T: Transport + 'static,
{
    /// Configure the provider with an optional API key (an explicit
    /// [`Secret`], or `None` to fall back to `ANTHROPIC_API_KEY`) and a
    /// transport.
    #[must_use]
    pub fn new(api_key: Option<Secret>, transport: Arc<T>) -> Self {
        Anthropic {
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            transport,
        }
    }

    /// Override the base URL (e.g. a gateway or a mock server in tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// The resolved endpoint (`{baseURL}/messages`).
    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::new(self.base_url.clone(), PATH)
    }

    /// The auth strategy: `x-api-key` from the explicit key, falling back to
    /// `ANTHROPIC_API_KEY` (port of the `optional(apiKey).orElse(config(ENV))`
    /// chain piped through `Auth.header("x-api-key")`).
    #[must_use]
    pub fn auth(&self) -> AuthDef {
        let env = AuthDef::header("x-api-key", Secret::config(API_KEY_ENV));
        match &self.api_key {
            Some(secret) => AuthDef::header("x-api-key", secret.clone()).or_else(env),
            None => env,
        }
    }

    /// The static headers stamped on every request (`anthropic-version`).
    #[must_use]
    pub fn headers(&self) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        headers.insert(
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string(),
        );
        headers
    }
}

impl<T> Provider for Anthropic<T>
where
    T: Transport + 'static,
{
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn route(&self, _model_id: &str) -> Box<dyn Route> {
        Box::new(
            GenericRoute::new(
                Arc::new(AnthropicMessages),
                self.endpoint(),
                self.auth(),
                self.transport.clone(),
            )
            .with_headers(self.headers()),
        )
    }

    fn model(&self, model_id: &str) -> Model {
        registry::model_or_default(PROVIDER_ID, model_id, ROUTE_ID)
    }
}
