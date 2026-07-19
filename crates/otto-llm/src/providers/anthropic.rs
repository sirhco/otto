//! The native Anthropic provider.
//!
//! Port of opencode `packages/llm/src/providers/anthropic.ts`. Serves the
//! [`AnthropicMessages`] protocol over `POST {baseURL}/messages`, authenticates
//! with the `x-api-key` header (NOT bearer) for a plain API key, and stamps
//! the required `anthropic-version` header on every request.
//!
//! otto extension, no opencode analog: a Claude Pro/Max OAuth access token
//! (from `otto-auth`'s `AnthropicOAuth`, see [`Self::new_oauth`]) is NOT a
//! valid `x-api-key` value — sending it as one 401s with `invalid x-api-key`
//! (confirmed against the live API). It authenticates instead as
//! `Authorization: Bearer <token>` plus the `anthropic-beta: oauth-2025-04-20`
//! header, which is what enables Bearer-token auth on `/v1/messages` for the
//! Pro/Max grant. Not documented in Anthropic's public API reference; this is
//! the same header opencode/Claude Code and other third-party Pro/Max clients
//! send — reproduced here, not invented.

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
/// The `anthropic-beta` value that enables `Authorization: Bearer` auth on
/// `/v1/messages` for a Claude Pro/Max OAuth access token. See [`Anthropic::new_oauth`].
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// The native Anthropic provider, generic over the [`Transport`].
///
/// Port of the `configure`/`model` facade in `anthropic.ts`.
pub struct Anthropic<T> {
    api_key: Option<Secret>,
    /// Set by [`Self::new_oauth`] — switches auth from `x-api-key` to
    /// `Authorization: Bearer` and adds the `anthropic-beta` header the
    /// Pro/Max OAuth grant requires. otto extension, no opencode analog.
    oauth: bool,
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
            oauth: false,
            base_url: DEFAULT_BASE_URL.to_string(),
            transport,
        }
    }

    /// Configure the provider with a Claude Pro/Max OAuth access token
    /// instead of a plain API key. otto extension, no opencode analog — see
    /// this module's doc comment for why this needs a distinct auth strategy.
    #[must_use]
    pub fn new_oauth(access_token: Secret, transport: Arc<T>) -> Self {
        Anthropic {
            api_key: Some(access_token),
            oauth: true,
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

    /// The auth strategy: `Authorization: Bearer` for an OAuth access token
    /// ([`Self::new_oauth`]); otherwise `x-api-key` from the explicit key,
    /// falling back to `ANTHROPIC_API_KEY` (port of the
    /// `optional(apiKey).orElse(config(ENV))` chain piped through
    /// `Auth.header("x-api-key")`).
    #[must_use]
    pub fn auth(&self) -> AuthDef {
        if self.oauth {
            let secret = self.api_key.clone().expect("new_oauth always sets api_key");
            return AuthDef::bearer(secret);
        }
        let env = AuthDef::header("x-api-key", Secret::config(API_KEY_ENV));
        match &self.api_key {
            Some(secret) => AuthDef::header("x-api-key", secret.clone()).or_else(env),
            None => env,
        }
    }

    /// The static headers stamped on every request (`anthropic-version`,
    /// plus `anthropic-beta` when authenticating with an OAuth access token —
    /// see [`Self::new_oauth`]).
    #[must_use]
    pub fn headers(&self) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        headers.insert(
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string(),
        );
        if self.oauth {
            headers.insert("anthropic-beta".to_string(), OAUTH_BETA.to_string());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::HttpTransport;

    fn test_transport() -> Arc<HttpTransport> {
        Arc::new(HttpTransport::new())
    }

    #[test]
    fn api_key_auth_uses_x_api_key_not_bearer() {
        let p = Anthropic::new(Some(Secret::literal("sk-ant-x")), test_transport());
        assert_eq!(
            p.auth(),
            AuthDef::header("x-api-key", Secret::literal("sk-ant-x"))
                .or_else(AuthDef::header("x-api-key", Secret::config(API_KEY_ENV))),
        );
        assert!(
            !p.headers().contains_key("anthropic-beta"),
            "a plain API key must not stamp the OAuth-only beta header"
        );
    }

    /// Regression test: an OAuth access token sent as `x-api-key` 401s with
    /// `invalid x-api-key` against the live API (confirmed manually) — it
    /// must authenticate as `Authorization: Bearer` with the
    /// `anthropic-beta: oauth-2025-04-20` header instead.
    #[test]
    fn oauth_auth_uses_bearer_and_beta_header() {
        let p = Anthropic::new_oauth(Secret::literal("sk-ant-oat01-x"), test_transport());
        assert_eq!(
            p.auth(),
            AuthDef::bearer(Secret::literal("sk-ant-oat01-x")),
            "OAuth must authenticate as Bearer, never x-api-key"
        );
        assert_eq!(
            p.headers().get("anthropic-beta").map(String::as_str),
            Some(OAUTH_BETA)
        );
        assert!(p.headers().contains_key("anthropic-version"));
    }
}
