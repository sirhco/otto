//! The Google Vertex AI provider (Gemini models only).
//!
//! Reuses the [`protocols::gemini::Gemini`] wire protocol unchanged — Vertex's
//! `:streamGenerateContent` request/response body shape matches the public
//! Gemini API used by [`super::google::Google`]. Differs only in endpoint
//! (GCP project/location URL) and auth (`Authorization: Bearer`, resolved
//! upstream from Application Default Credentials — see
//! `otto-app`'s `vertex_auth` module, not present in this crate).
//!
//! otto extension: no opencode analog (opencode has no native Vertex AI provider).

use std::sync::Arc;

use crate::auth::{AuthDef, Secret};
use crate::model::Model;
use crate::protocols::gemini::Gemini;
use crate::registry;
use crate::route::{Endpoint, GenericRoute, Route};
use crate::transport::Transport;

use super::Provider;

/// The provider id (`ProviderID.make("vertex")`).
const PROVIDER_ID: &str = "vertex";
/// The route id served by this provider. Shares the Gemini protocol's model
/// metadata lookup — Vertex-flavored model ids typically aren't in the
/// embedded models.dev registry at all, so `provider.vertex.models.<id>.limits`
/// in config is usually required (same situation as local Ollama models).
const ROUTE_ID: &str = "gemini";

/// The native Google Vertex AI provider (Gemini models), generic over the
/// [`Transport`].
pub struct Vertex<T> {
    project: String,
    location: String,
    bearer_token: Secret,
    transport: Arc<T>,
}

impl<T> Vertex<T>
where
    T: Transport + 'static,
{
    /// Configure the provider with a GCP project, region, an already-resolved
    /// Bearer token (from ADC — see `otto-app`'s `VertexTokenCache`), and a
    /// transport.
    #[must_use]
    pub fn new(
        project: impl Into<String>,
        location: impl Into<String>,
        bearer_token: Secret,
        transport: Arc<T>,
    ) -> Self {
        Vertex {
            project: project.into(),
            location: location.into(),
            bearer_token,
            transport,
        }
    }

    /// The resolved endpoint for `model_id`
    /// (`https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models/{model_id}:streamGenerateContent?alt=sse`).
    /// Unlike Anthropic/OpenAI (static paths), the project/location/model id
    /// are all embedded in the path, so the endpoint is rebuilt per model.
    #[must_use]
    pub fn endpoint(&self, model_id: &str) -> Endpoint {
        let mut query = std::collections::BTreeMap::new();
        query.insert("alt".to_string(), "sse".to_string());
        Endpoint {
            base_url: format!("https://{}-aiplatform.googleapis.com/v1", self.location),
            path: format!(
                "/projects/{}/locations/{}/publishers/google/models/{}:streamGenerateContent",
                self.project, self.location, model_id
            ),
            query: Some(query),
        }
    }

    /// The auth strategy: `Authorization: Bearer <token>`. Never falls back to
    /// an env var — the token is always resolved dynamically via ADC upstream,
    /// not read from a static secret.
    #[must_use]
    pub fn auth(&self) -> AuthDef {
        AuthDef::bearer(self.bearer_token.clone())
    }
}

impl<T> Provider for Vertex<T>
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
