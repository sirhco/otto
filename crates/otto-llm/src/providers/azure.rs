//! The Azure OpenAI provider.
//!
//! Azure serves a byte-identical `openai-chat` request/stream shape to native
//! OpenAI — only the endpoint (a per-resource base URL plus an `api-version`
//! query parameter) and auth (an `api-key` header, NOT bearer) differ. This
//! provider therefore reuses the [`OpenAIChat`] protocol directly rather than
//! defining a new one.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::auth::{AuthDef, Secret};
use crate::model::Model;
use crate::protocols::openai_chat::OpenAIChat;
use crate::registry;
use crate::route::{Endpoint, GenericRoute, Route};
use crate::transport::Transport;

use super::Provider;

/// The Chat Completions path appended to the base URL.
const PATH: &str = "/chat/completions";
/// The `AZURE_OPENAI_API_KEY` env var read when no explicit key is given.
const API_KEY_ENV: &str = "AZURE_OPENAI_API_KEY";
/// The provider id (`ProviderID.make("azure")`).
const PROVIDER_ID: &str = "azure";
/// The route id served by this provider.
const ROUTE_ID: &str = "azure-openai-chat";
/// The default `api-version` query value.
const DEFAULT_API_VERSION: &str = "v1";

/// The Azure OpenAI provider, generic over the [`Transport`].
pub struct Azure<T> {
    api_key: Option<Secret>,
    base_url: String,
    api_version: String,
    transport: Arc<T>,
}

impl<T> Azure<T>
where
    T: Transport + 'static,
{
    /// Configure the provider for an Azure `resource_name` (the base URL is
    /// derived as `https://{resource_name}.openai.azure.com/openai/v1`), an
    /// optional API key (falling back to `AZURE_OPENAI_API_KEY`), and a
    /// transport.
    #[must_use]
    pub fn new(resource_name: String, api_key: Option<Secret>, transport: Arc<T>) -> Self {
        Azure {
            api_key,
            base_url: format!("https://{resource_name}.openai.azure.com/openai/v1"),
            api_version: DEFAULT_API_VERSION.to_string(),
            transport,
        }
    }

    /// Override the base URL (e.g. a gateway or a mock server in tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Override the `api-version` query parameter (default `"v1"`).
    #[must_use]
    pub fn with_api_version(mut self, api_version: String) -> Self {
        self.api_version = api_version;
        self
    }

    /// The resolved endpoint (`{baseURL}/chat/completions?api-version=...`).
    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        let mut query = BTreeMap::new();
        query.insert("api-version".to_string(), self.api_version.clone());
        Endpoint {
            base_url: self.base_url.clone(),
            path: PATH.to_string(),
            query: Some(query),
        }
    }

    /// The auth strategy: `api-key` from the explicit key, falling back to
    /// `AZURE_OPENAI_API_KEY`. Never sets `authorization`.
    #[must_use]
    pub fn auth(&self) -> AuthDef {
        let env = AuthDef::header("api-key", Secret::config(API_KEY_ENV));
        match &self.api_key {
            Some(secret) => AuthDef::header("api-key", secret.clone()).or_else(env),
            None => env,
        }
    }
}

impl<T> Provider for Azure<T>
where
    T: Transport + 'static,
{
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn route(&self, _model_id: &str) -> Box<dyn Route> {
        Box::new(GenericRoute::new(
            Arc::new(OpenAIChat),
            self.endpoint(),
            self.auth(),
            self.transport.clone(),
        ))
    }

    fn model(&self, model_id: &str) -> Model {
        registry::model_or_default(PROVIDER_ID, model_id, ROUTE_ID)
    }
}
