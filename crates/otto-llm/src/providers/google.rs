//! The Google Gemini provider.
//!
//! Port of opencode `packages/llm/src/providers/google.ts`. Serves the
//! [`protocols::gemini::Gemini`] protocol over
//! `POST {baseURL}/models/{model}:streamGenerateContent?alt=sse`, authenticating
//! with the `x-goog-api-key` header (NOT bearer).
//!
//! Named `Google` (rather than `Gemini`) to avoid colliding with the protocol
//! type `protocols::gemini::Gemini`.

use std::sync::Arc;

use crate::auth::{AuthDef, Secret};
use crate::model::Model;
use crate::protocols::gemini::Gemini;
use crate::registry;
use crate::route::{Endpoint, GenericRoute, Route};
use crate::transport::Transport;

use super::Provider;

/// The default Gemini API base URL (`google.ts` endpoint baseURL).
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
/// The `GOOGLE_GENERATIVE_AI_API_KEY` env var read when no explicit key is
/// given (`Auth.config("GOOGLE_GENERATIVE_AI_API_KEY")`).
const API_KEY_ENV: &str = "GOOGLE_GENERATIVE_AI_API_KEY";
/// The provider id (`ProviderID.make("google")`).
const PROVIDER_ID: &str = "google";
/// The route id served by this provider (`Gemini::id`).
const ROUTE_ID: &str = "gemini";

/// The native Google Gemini provider, generic over the [`Transport`].
///
/// Port of the `configure`/`model` facade in `google.ts`.
pub struct Google<T> {
    api_key: Option<Secret>,
    base_url: String,
    transport: Arc<T>,
}

impl<T> Google<T>
where
    T: Transport + 'static,
{
    /// Configure the provider with an optional API key (an explicit
    /// [`Secret`], or `None` to fall back to `GOOGLE_GENERATIVE_AI_API_KEY`)
    /// and a transport.
    #[must_use]
    pub fn new(api_key: Option<Secret>, transport: Arc<T>) -> Self {
        Google {
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

    /// The resolved endpoint for `model_id`
    /// (`{baseURL}/models/{model_id}:streamGenerateContent?alt=sse`). Unlike
    /// Anthropic/OpenAI (whose paths are static), Gemini embeds the model id
    /// in the path, so the endpoint must be rebuilt per model.
    #[must_use]
    pub fn endpoint(&self, model_id: &str) -> Endpoint {
        let mut query = std::collections::BTreeMap::new();
        query.insert("alt".to_string(), "sse".to_string());
        Endpoint {
            base_url: self.base_url.clone(),
            path: format!("/models/{model_id}:streamGenerateContent"),
            query: Some(query),
        }
    }

    /// The auth strategy: `x-goog-api-key` from the explicit key, falling
    /// back to `GOOGLE_GENERATIVE_AI_API_KEY`. Never sets `authorization`.
    #[must_use]
    pub fn auth(&self) -> AuthDef {
        let env = AuthDef::header("x-goog-api-key", Secret::config(API_KEY_ENV));
        match &self.api_key {
            Some(secret) => AuthDef::header("x-goog-api-key", secret.clone()).or_else(env),
            None => env,
        }
    }
}

impl<T> Provider for Google<T>
where
    T: Transport + 'static,
{
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn route(&self, model_id: &str) -> Box<dyn Route> {
        Box::new(GenericRoute::new(
            Arc::new(Gemini),
            self.endpoint(model_id),
            self.auth(),
            self.transport.clone(),
        ))
    }

    fn model(&self, model_id: &str) -> Model {
        registry::model_or_default(PROVIDER_ID, model_id, ROUTE_ID)
    }
}
