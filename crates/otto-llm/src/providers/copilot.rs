//! The GitHub Copilot provider.
//!
//! Routes `claude-*` models through the [`AnthropicMessages`] protocol
//! (`POST {baseURL}/v1/messages`), gpt-5-class models (per
//! [`should_use_responses`]) through the [`OpenAIResponses`] protocol
//! (`POST {baseURL}/responses`), and everything else through the
//! [`OpenAIChat`] protocol (`POST {baseURL}/chat/completions`) — all
//! authenticated with a GitHub token as a `Bearer` header and stamped with the
//! Copilot-required static headers. Each inner protocol is wrapped in
//! [`CopilotCache`] so the `copilot_cache_control` markers (and, for `gpt*`
//! models, the `max_tokens` strip) are applied regardless of which wire shape
//! is in play.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::auth::{AuthDef, Secret};
use crate::model::Model;
use crate::protocols::anthropic_messages::{ANTHROPIC_VERSION, AnthropicMessages};
use crate::protocols::copilot_cache::{BodyShape, CopilotCache};
use crate::protocols::openai_chat::OpenAIChat;
use crate::protocols::openai_responses::{OpenAIResponses, should_use_responses};
use crate::registry;
use crate::route::{Endpoint, GenericRoute, Route};
use crate::transport::Transport;

use super::Provider;

/// The default (public) Copilot API base URL.
const PUBLIC_BASE: &str = "https://api.githubcopilot.com";
/// The provider id (matches the models.dev provider key).
const PROVIDER_ID: &str = "github-copilot";
/// The route id served by this provider (advisory — see [`Provider::route`]).
const ROUTE_ID: &str = "openai-compatible-chat";
/// The `X-GitHub-Api-Version` header value.
const API_VERSION: &str = "2026-06-01";
/// The `anthropic-beta` header value stamped on claude-model requests.
const ANTHROPIC_BETA: &str = "interleaved-thinking-2025-05-14";

/// The GitHub Copilot provider, generic over the [`Transport`].
///
/// Dispatches to one of two wire protocols depending on `model_id`: Anthropic
/// Messages for `claude-*` models, OpenAI Chat Completions for everything
/// else. Both are wrapped in [`CopilotCache`] before being handed to the
/// route.
pub struct Copilot<T> {
    token: Option<Secret>,
    base: String,
    transport: Arc<T>,
}

impl<T> Copilot<T>
where
    T: Transport + 'static,
{
    /// Configure the provider with an optional GitHub token and a transport.
    /// Defaults to the public Copilot API base URL.
    #[must_use]
    pub fn new(token: Option<Secret>, transport: Arc<T>) -> Self {
        Self {
            token,
            base: PUBLIC_BASE.to_string(),
            transport,
        }
    }

    /// Point the provider at an explicit base URL, from
    /// `config.provider.github-copilot.options.baseURL`.
    ///
    /// Overrides both the public default and any enterprise host derived from
    /// the credential — an explicit config value is the user speaking, and it
    /// is the only escape hatch when the derived host is wrong or unreachable
    /// from their network. Apply it *after* [`Self::with_enterprise`].
    #[must_use]
    pub fn with_base_url(mut self, base: impl AsRef<str>) -> Self {
        self.base = base.as_ref().trim().trim_end_matches('/').to_string();
        self
    }

    /// Switch the base URL to a GitHub Enterprise Copilot API host
    /// (`https://copilot-api.<domain>`).
    #[must_use]
    pub fn with_enterprise(mut self, domain: &str) -> Self {
        let d = domain
            .trim()
            .trim_start_matches("https://")
            .trim_end_matches('/');
        self.base = format!("https://copilot-api.{d}");
        self
    }

    /// Whether `model_id` should be routed through Anthropic Messages.
    fn is_claude(model_id: &str) -> bool {
        model_id.starts_with("claude")
    }

    /// The resolved base URL for `model_id` (`{base}/v1` for claude models,
    /// `{base}` otherwise).
    #[must_use]
    pub(crate) fn base_for(&self, model_id: &str) -> String {
        if Self::is_claude(model_id) {
            format!("{}/v1", self.base)
        } else {
            self.base.clone()
        }
    }

    /// The resolved path for `model_id` (`/messages` for claude models,
    /// `/chat/completions` otherwise).
    #[must_use]
    pub(crate) fn path_for(&self, model_id: &str) -> &'static str {
        if Self::is_claude(model_id) {
            "/messages"
        } else {
            "/chat/completions"
        }
    }

    /// The fully-resolved [`Endpoint`] for `model_id`.
    ///
    /// The single source of truth for endpoint selection — [`Self::route`]
    /// calls this, so the URL a caller can inspect is always the URL the
    /// route will actually target.
    #[must_use]
    pub fn endpoint(&self, model_id: &str) -> Endpoint {
        if !Self::is_claude(model_id) && should_use_responses(model_id) {
            Endpoint::new(self.base_for(model_id), "/responses")
        } else {
            Endpoint::new(self.base_for(model_id), self.path_for(model_id))
        }
    }

    /// The auth strategy: `Bearer` from the explicit GitHub token, or none.
    #[must_use]
    pub(crate) fn auth(&self) -> AuthDef {
        match &self.token {
            Some(secret) => AuthDef::bearer(secret.clone()),
            None => AuthDef::none(),
        }
    }

    /// The static headers stamped on every request for `model_id`: the
    /// Copilot-required headers, plus (for claude models) the Anthropic
    /// version/beta headers.
    #[must_use]
    pub(crate) fn headers(&self, model_id: &str) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        headers.insert(
            "User-Agent".to_string(),
            format!("otto/{}", env!("CARGO_PKG_VERSION")),
        );
        headers.insert(
            "Openai-Intent".to_string(),
            "conversation-edits".to_string(),
        );
        headers.insert("X-GitHub-Api-Version".to_string(), API_VERSION.to_string());
        headers.insert("x-initiator".to_string(), "user".to_string());
        if Self::is_claude(model_id) {
            headers.insert(
                "anthropic-version".to_string(),
                ANTHROPIC_VERSION.to_string(),
            );
            headers.insert("anthropic-beta".to_string(), ANTHROPIC_BETA.to_string());
        }
        headers
    }
}

impl<T> Provider for Copilot<T>
where
    T: Transport + 'static,
{
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn route(&self, model_id: &str) -> Box<dyn Route> {
        let headers = self.headers(model_id);
        let endpoint = self.endpoint(model_id);
        if Self::is_claude(model_id) {
            let protocol = CopilotCache::new(AnthropicMessages, BodyShape::Anthropic, model_id);
            return Box::new(
                GenericRoute::new(
                    Arc::new(protocol),
                    endpoint,
                    self.auth(),
                    self.transport.clone(),
                )
                .with_headers(headers),
            );
        }
        if should_use_responses(model_id) {
            let protocol = CopilotCache::new(OpenAIResponses, BodyShape::OpenAi, model_id);
            return Box::new(
                GenericRoute::new(
                    Arc::new(protocol),
                    endpoint,
                    self.auth(),
                    self.transport.clone(),
                )
                .with_headers(headers),
            );
        }
        let protocol = CopilotCache::new(OpenAIChat, BodyShape::OpenAi, model_id);
        Box::new(
            GenericRoute::new(
                Arc::new(protocol),
                endpoint,
                self.auth(),
                self.transport.clone(),
            )
            .with_headers(headers),
        )
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

    /// A real (but never-driven) [`HttpTransport`], matching the convention
    /// used by `tests/providers.rs` for the other built-in providers: these
    /// tests only assert on the provider's `base_for`/`path_for`/`headers`
    /// helpers, so the transport is never actually invoked.
    fn test_transport() -> Arc<HttpTransport> {
        Arc::new(HttpTransport::new())
    }

    #[test]
    fn claude_model_targets_v1_messages_with_beta_header() {
        let p = Copilot::new(Some(Secret::literal("gho_x")), test_transport());
        assert_eq!(
            p.base_for("claude-sonnet-4.5"),
            "https://api.githubcopilot.com/v1"
        );
        assert_eq!(p.path_for("claude-sonnet-4.5"), "/messages");
        let h = p.headers("claude-sonnet-4.5");
        assert_eq!(
            h.get("anthropic-beta").map(String::as_str),
            Some("interleaved-thinking-2025-05-14")
        );
        assert!(h.contains_key("anthropic-version"));
        assert_eq!(
            h.get("Openai-Intent").map(String::as_str),
            Some("conversation-edits")
        );
        assert_eq!(
            h.get("X-GitHub-Api-Version").map(String::as_str),
            Some("2026-06-01")
        );
    }

    #[test]
    fn gpt_model_targets_chat_completions_no_anthropic_headers() {
        let p = Copilot::new(Some(Secret::literal("gho_x")), test_transport());
        assert_eq!(p.base_for("gpt-4o"), "https://api.githubcopilot.com");
        assert_eq!(p.path_for("gpt-4o"), "/chat/completions");
        let h = p.headers("gpt-4o");
        assert!(!h.contains_key("anthropic-beta"));
        assert_eq!(
            h.get("X-GitHub-Api-Version").map(String::as_str),
            Some("2026-06-01")
        );
    }

    #[test]
    fn enterprise_base_uses_copilot_api_host() {
        let p = Copilot::new(Some(Secret::literal("gho_x")), test_transport())
            .with_enterprise("acme.ghe.com");
        assert_eq!(p.base_for("gpt-4o"), "https://copilot-api.acme.ghe.com");
        assert_eq!(
            p.base_for("claude-sonnet-4.5"),
            "https://copilot-api.acme.ghe.com/v1"
        );
    }

    /// `config.provider.github-copilot.options.baseURL` overrides both the
    /// public default and any credential-derived enterprise host. Config is
    /// the user speaking explicitly, and it is the only escape hatch when the
    /// derived enterprise host is wrong or unreachable from their network.
    #[test]
    fn config_base_url_overrides_default_and_enterprise() {
        let p = Copilot::new(Some(Secret::literal("gho_x")), test_transport())
            .with_base_url("https://copilot.internal.acme/api/");
        // trailing slash trimmed so path joins don't double up
        assert_eq!(p.base_for("gpt-4o"), "https://copilot.internal.acme/api");
        assert_eq!(
            p.base_for("claude-sonnet-4.5"),
            "https://copilot.internal.acme/api/v1"
        );

        // applied after with_enterprise, config still wins
        let p = Copilot::new(Some(Secret::literal("gho_x")), test_transport())
            .with_enterprise("acme.ghe.com")
            .with_base_url("https://copilot.internal.acme");
        assert_eq!(p.base_for("gpt-4o"), "https://copilot.internal.acme");
    }

    #[test]
    fn id_and_model_use_provider_id() {
        let p = Copilot::new(Some(Secret::literal("gho_x")), test_transport());
        assert_eq!(p.id(), "github-copilot");
        let model = p.model("gpt-4o");
        assert_eq!(model.provider.0, "github-copilot");
    }

    /// `route()` must actually pick the Anthropic Messages protocol for
    /// `claude-*` models — `CopilotCache::id()` delegates to the inner
    /// protocol's `id()` (see `protocols::copilot_cache`), so this would fail
    /// if the wrong protocol (or shape) were ever swapped in.
    #[test]
    fn route_claude_uses_anthropic_protocol() {
        let p = Copilot::new(Some(Secret::literal("gho_x")), test_transport());
        assert_eq!(p.route("claude-sonnet-4.5").id(), "anthropic");
    }

    /// `route()` must pick the OpenAI Chat protocol for everything else.
    #[test]
    fn route_gpt_uses_openai_protocol() {
        let p = Copilot::new(Some(Secret::literal("gho_x")), test_transport());
        assert_eq!(p.route("gpt-4o").id(), "openai-chat");
    }

    /// `route()` must pick the OpenAI Responses protocol for gpt-5-class
    /// models (excluding the `gpt-5-mini` family), while claude and
    /// non-gpt-5-class models are unaffected.
    #[test]
    fn copilot_gpt5_uses_responses_protocol() {
        let p = Copilot::new(Some(Secret::literal("ghtok")), test_transport());
        assert_eq!(p.route("gpt-5").id(), "openai-responses");
        assert_eq!(p.route("gpt-4o").id(), "openai-chat");
        assert_eq!(p.route("claude-sonnet-4.5").id(), "anthropic");
        assert_eq!(p.route("gpt-5-mini").id(), "openai-chat");
    }

    /// `base_for`/`path_for` a gpt-5-class model must resolve to the same
    /// non-`/v1` base the chat path uses, with a `/responses` path — never
    /// the claude `/v1` base.
    #[test]
    fn responses_model_uses_non_v1_base_and_responses_path() {
        let p = Copilot::new(Some(Secret::literal("gho_x")), test_transport());
        assert_eq!(p.base_for("gpt-5"), "https://api.githubcopilot.com");
    }

    /// The gpt `max_tokens` strip in `CopilotCache` is keyed on the literal
    /// `"max_tokens"`; the Responses wire body field is `max_output_tokens`,
    /// so building a Responses body through `CopilotCache` for a gpt-5 id
    /// must leave `max_output_tokens` intact (the strip is a no-op for this
    /// shape).
    #[test]
    fn copilot_cache_leaves_max_output_tokens_intact_for_responses_body() {
        use crate::message::Message;
        use crate::model::Model;
        use crate::protocol::Protocol;
        use crate::protocols::copilot_cache::{BodyShape, CopilotCache};
        use crate::protocols::openai_responses::OpenAIResponses;
        use crate::request::LLMRequest;

        let model = Model::new("github-copilot", "gpt-5", "openai-responses");
        let mut req = LLMRequest::new(model, vec![Message::user(vec![])]);
        req.generation = Some(crate::request::GenerationOptions {
            max_tokens: Some(1234),
            ..Default::default()
        });
        let wrapped = CopilotCache::new(OpenAIResponses, BodyShape::OpenAi, "gpt-5");
        let body = wrapped.build_body(&req).unwrap();
        assert_eq!(body["max_output_tokens"], 1234);
        assert!(body.get("max_tokens").is_none());
    }
}
