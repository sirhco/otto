//! The [`RouteFactory`] seam — turning a [`ModelRef`] into a runnable
//! [`Route`] + [`Model`] — and its default credential-backed implementation.

use std::collections::HashMap;
use std::sync::Arc;

use otto_agent::ModelRef;
use otto_auth::{AuthMap, Credential};
use otto_llm::{
    Anthropic, Copilot, Google, HttpTransport, Model, OpenAI, OpenAICompatible, Provider, Route,
    Secret, Vertex,
};

use crate::{Error, Result};

/// A resolved custom-provider override (from `config.provider.<id>.options`).
///
/// Populated from [`otto_config::Config::provider_overrides`] — lets a
/// provider id not covered by one of the native arms (e.g. `ollama`) still
/// resolve to a real endpoint via the OpenAI-compatible protocol.
#[derive(Debug, Clone)]
pub struct ProviderOverride {
    /// The provider's base URL, e.g. `http://localhost:11434/v1`.
    pub base_url: String,
    /// An optional API key from config, used as a fallback when no
    /// credential is stored for the provider.
    pub api_key: Option<String>,
    /// Config-declared context/output windows keyed by model id
    /// (`provider.<id>.models.<model>.limits`). Overlaid onto the resolved
    /// [`Model`] so compaction can trigger for models the embedded registry
    /// doesn't know (local ollama models) before the provider silently
    /// truncates the prompt.
    pub model_limits: HashMap<String, ModelLimitsOverride>,
    /// GCP project id (`provider.vertex.options.project`). Only meaningful
    /// for the `vertex` provider id.
    pub project: Option<String>,
    /// GCP region (`provider.vertex.options.location`). Only meaningful for
    /// the `vertex` provider id.
    pub location: Option<String>,
}

/// One config-declared `limits` block (see [`ProviderOverride::model_limits`]).
#[derive(Debug, Clone, Default)]
pub struct ModelLimitsOverride {
    /// Context window, in tokens.
    pub context: Option<u64>,
    /// Max output tokens.
    pub output: Option<u64>,
}

/// The default model used when config pins none (`anthropic/claude-sonnet-4-5`).
/// Must be a real, currently-served model id — a stale/aliased id (e.g. the
/// bare `claude-sonnet-4`) is absent from the models.dev registry and 404s at
/// the Anthropic API, which surfaces as a `ProviderError` on the first prompt.
const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4-5";

/// Resolve the default [`ModelRef`] from an optional `config.model` string,
/// falling back to `anthropic/claude-sonnet-4-5`.
#[must_use]
pub fn default_model(config_model: Option<&str>) -> ModelRef {
    ModelRef::parse(config_model.unwrap_or(DEFAULT_MODEL))
}

/// Maps a [`ModelRef`] to the [`Route`] + [`Model`] a run should generate with.
///
/// This is the injectable seam decoupling the [`crate::Runtime`] from concrete
/// providers/credentials: the default [`AuthRouteFactory`] builds native
/// Anthropic / OpenAI / OpenAI-compatible routes from stored credentials, while
/// tests inject a scripted factory. The same factory is reused by the subagent
/// spawner so child runs resolve their routes identically.
pub trait RouteFactory: Send + Sync {
    /// Build the [`Route`] and resolve the [`Model`] metadata for `m`.
    ///
    /// # Errors
    /// Returns [`Error::Route`] when a route cannot be constructed.
    fn route_for(&self, m: &ModelRef) -> Result<(Arc<dyn Route>, Model)>;
}

/// The default [`RouteFactory`]: builds native provider routes from a snapshot
/// of the credential store, falling back to provider env vars.
///
/// The provider id selects the wire protocol — `anthropic` → native Anthropic
/// Messages, `openai` → native OpenAI Chat, anything else → OpenAI-compatible
/// Chat. The credential (if any) supplies the key material: an [`Credential::Api`]
/// key or the bearer [`Credential::Oauth`] access token becomes a literal
/// [`Secret`]; with no stored credential the provider falls back to its
/// environment variable (e.g. `ANTHROPIC_API_KEY`).
pub struct AuthRouteFactory {
    auth: AuthMap,
    transport: Arc<HttpTransport>,
    providers: HashMap<String, ProviderOverride>,
    vertex_auth: Option<Arc<dyn crate::vertex_auth::VertexAuth>>,
}

impl AuthRouteFactory {
    /// Build a factory over a snapshot of the credential map, a shared HTTP
    /// transport, and a map of config-supplied custom-provider overrides
    /// (base URL / API key), keyed by provider id.
    #[must_use]
    pub fn new(
        auth: AuthMap,
        transport: Arc<HttpTransport>,
        providers: HashMap<String, ProviderOverride>,
    ) -> Self {
        Self {
            auth,
            transport,
            providers,
            vertex_auth: None,
        }
    }

    /// Attach the live GCP token source for Vertex AI routes. Left unset
    /// when `provider.vertex` isn't configured — no `gcloud-auth` cost for
    /// runtimes that never use Vertex.
    #[must_use]
    pub fn with_vertex_auth(
        mut self,
        vertex_auth: Arc<dyn crate::vertex_auth::VertexAuth>,
    ) -> Self {
        self.vertex_auth = Some(vertex_auth);
        self
    }

    /// The key material for `provider`, if stored. `None` lets the provider fall
    /// back to its environment variable.
    fn secret_for(&self, provider: &str) -> Option<Secret> {
        match self.auth.get(provider)? {
            Credential::Api { key, .. } | Credential::WellKnown { key, .. } => {
                Some(Secret::literal(key.clone()))
            }
            Credential::Oauth { access, .. } => Some(Secret::literal(access.clone())),
        }
    }

    /// The stored [`Credential`] for `provider`, if any — lets a provider arm
    /// read metadata beyond the plain key material `secret_for` yields (e.g.
    /// GitHub Copilot's `enterprise_url`).
    fn credential_for(&self, provider: &str) -> Option<&Credential> {
        self.auth.get(provider)
    }
}

impl RouteFactory for AuthRouteFactory {
    fn route_for(&self, m: &ModelRef) -> Result<(Arc<dyn Route>, Model)> {
        let provider = m.provider.0.as_str();
        let model_id = m.model.0.as_str();
        let key = self.secret_for(provider);

        // A config-supplied override lets the NAMED providers point at a
        // gateway (litellm, a corporate proxy) while keeping their native wire
        // protocol — e.g. `provider.anthropic.options.baseURL` →
        // `{baseURL}/messages` with the Anthropic Messages protocol, exactly
        // the litellm endpoint Anthropic-native clients use. Previously the
        // override was honored only for unknown provider ids and silently
        // ignored here.
        let override_base = self
            .providers
            .get(provider)
            .map(|ov| ov.base_url.clone())
            .filter(|u| !u.is_empty());
        let override_key = self
            .providers
            .get(provider)
            .and_then(|ov| ov.api_key.clone().map(Secret::literal));

        let (route, model): (Arc<dyn Route>, Model) = match provider {
            "anthropic" => {
                // A Claude Pro/Max OAuth credential needs Bearer auth, not
                // x-api-key — see `Anthropic::new_oauth`'s doc comment.
                let mut p = if let Some(Credential::Oauth { access, .. }) =
                    self.credential_for("anthropic")
                {
                    Anthropic::new_oauth(Secret::literal(access.clone()), self.transport.clone())
                } else {
                    Anthropic::new(key.or(override_key), self.transport.clone())
                };
                if let Some(base) = override_base {
                    p = p.with_base_url(base);
                }
                (Arc::from(p.route(model_id)), p.model(model_id))
            }
            "openai" => {
                let mut p = OpenAI::new(key.or(override_key), self.transport.clone());
                if let Some(base) = override_base {
                    p = p.with_base_url(base);
                }
                (Arc::from(p.route(model_id)), p.model(model_id))
            }
            "google" | "gemini" => {
                let p = Google::new(key, self.transport.clone());
                (Arc::from(p.route(model_id)), p.model(model_id))
            }
            "vertex" => {
                let ov = self
                    .providers
                    .get("vertex")
                    .and_then(|ov| ov.project.clone());
                let project = ov.ok_or_else(|| {
                    Error::Route(
                        "vertex: no project configured (set provider.vertex.options.project)"
                            .to_string(),
                    )
                })?;
                let location = self
                    .providers
                    .get("vertex")
                    .and_then(|ov| ov.location.clone())
                    .unwrap_or_else(|| "us-central1".to_string());
                let auth = self.vertex_auth.as_ref().ok_or_else(|| {
                    Error::Route("vertex: credentials not initialized".to_string())
                })?;
                let token = auth.current_token()?;
                let p = Vertex::new(
                    project,
                    location,
                    Secret::literal(token),
                    self.transport.clone(),
                );
                (Arc::from(p.route(model_id)), p.model(model_id))
            }
            "github-copilot" => {
                let mut p = Copilot::new(key, self.transport.clone());
                if let Some(Credential::Oauth {
                    enterprise_url: Some(domain),
                    ..
                }) = self.credential_for("github-copilot")
                {
                    p = p.with_enterprise(domain);
                }
                (Arc::from(p.route(model_id)), p.model(model_id))
            }
            other => {
                // No models.dev endpoint registry yet: an unknown provider gets
                // an OpenAI-compatible route. `config.provider.<id>.options`
                // supplies the base URL (e.g. Ollama's local server); with no
                // override configured the base URL is empty and the route
                // still carries the correct protocol/model metadata.
                let (base_url, key) = match self.providers.get(other) {
                    Some(ov) => (
                        ov.base_url.clone(),
                        // A stored credential wins; otherwise fall back to the
                        // config-supplied apiKey.
                        key.or_else(|| ov.api_key.clone().map(Secret::literal)),
                    ),
                    None => (String::new(), key),
                };
                let p = OpenAICompatible::new(other, base_url, key, self.transport.clone());
                (Arc::from(p.route(model_id)), p.model(model_id))
            }
        };
        // Overlay config-declared limits (any provider): the run loop's
        // compaction check reads `model.limits.context`, and local models are
        // unknown to the embedded registry.
        let mut model = model;
        if let Some(ov) = self.providers.get(provider)
            && let Some(l) = ov.model_limits.get(model_id)
        {
            if l.context.is_some() {
                model.limits.context = l.context;
            }
            if l.output.is_some() {
                model.limits.output = l.output;
            }
        }
        Ok((route, model))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shared HTTP transport for tests that build a factory/provider
    /// end-to-end without touching the network.
    fn test_transport() -> Arc<HttpTransport> {
        Arc::new(HttpTransport::new())
    }

    /// Config-declared `limits` for a model land on the resolved [`Model`], so
    /// the run loop's compaction check knows the context window of models the
    /// embedded registry has never heard of (local ollama models).
    #[test]
    fn config_model_limits_overlay_resolved_model() {
        let mut model_limits = HashMap::new();
        model_limits.insert(
            "gemma4:26b-mlx".to_string(),
            ModelLimitsOverride {
                context: Some(32_768),
                output: Some(8192),
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".to_string(),
            ProviderOverride {
                base_url: "http://localhost:11434/v1".to_string(),
                api_key: None,
                model_limits,
                project: None,
                location: None,
            },
        );
        let factory = AuthRouteFactory::new(AuthMap::new(), test_transport(), providers);

        let (_route, model) = factory
            .route_for(&ModelRef::parse("ollama/gemma4:26b-mlx"))
            .expect("route builds");
        assert_eq!(model.limits.context, Some(32_768));
        assert_eq!(model.limits.output, Some(8192));

        // A model without a declared override keeps registry/default limits.
        let (_route, other) = factory
            .route_for(&ModelRef::parse("ollama/llama3.2"))
            .expect("route builds");
        assert_eq!(other.limits.context, None);
    }

    /// A config-supplied provider override (e.g. `ollama`) resolves to an
    /// OpenAI-compatible route using its configured base URL, rather than the
    /// empty-string fallback for a truly unknown provider.
    #[test]
    fn config_provider_supplies_base_url_for_custom_provider() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".to_string(),
            ProviderOverride {
                base_url: "http://localhost:11434/v1".to_string(),
                api_key: None,
                model_limits: HashMap::new(),
                project: None,
                location: None,
            },
        );
        let factory = AuthRouteFactory::new(AuthMap::new(), test_transport(), providers);

        let (_route, model) = factory
            .route_for(&ModelRef::parse("ollama/llama3.2"))
            .expect("route builds for a config-supplied custom provider");
        assert_eq!(model.route_id, "openai-compatible-chat");

        // The override is stored and looked up by the catch-all arm.
        assert_eq!(
            factory.providers.get("ollama").map(|o| o.base_url.as_str()),
            Some("http://localhost:11434/v1")
        );
    }

    /// `provider.anthropic.options.baseURL` must reach the NAMED anthropic
    /// arm — pointing otto's native Anthropic Messages protocol at a gateway
    /// (litellm) the way Anthropic-native clients do. It used to be silently
    /// ignored for named providers. The gateway model name may itself carry a
    /// slash (`github_copilot/claude-opus-4.8`); `ModelRef::parse` splits on
    /// the FIRST slash only.
    #[test]
    fn config_base_url_reaches_named_anthropic_provider() {
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic".to_string(),
            ProviderOverride {
                base_url: "http://litellm:4000/v1".to_string(),
                api_key: Some("sk-litellm".to_string()),
                model_limits: HashMap::new(),
                project: None,
                location: None,
            },
        );
        let factory = AuthRouteFactory::new(AuthMap::new(), test_transport(), providers);

        let mref = ModelRef::parse("anthropic/github_copilot/claude-opus-4.8");
        assert_eq!(mref.provider.0, "anthropic");
        assert_eq!(mref.model.0, "github_copilot/claude-opus-4.8");

        let (_route, model) = factory.route_for(&mref).expect("route builds");
        assert_eq!(
            model.route_id, "anthropic",
            "named provider keeps its native protocol behind the gateway"
        );

        // The endpoint join itself: `{baseURL}/messages`.
        let p = otto_llm::providers::Anthropic::new(None, test_transport())
            .with_base_url("http://litellm:4000/v1");
        assert_eq!(p.endpoint().url(), "http://litellm:4000/v1/messages");
    }

    /// A fake [`VertexAuth`] for tests — returns a fixed token, no ADC, no
    /// network, no background task.
    struct FakeVertexAuth(&'static str);

    impl crate::vertex_auth::VertexAuth for FakeVertexAuth {
        fn current_token(&self) -> Result<String> {
            Ok(self.0.to_string())
        }
    }

    /// `provider.vertex.options.project`/`location` reach the Vertex arm,
    /// which builds the GCP project/location URL and stamps the cached
    /// token as a Bearer header.
    #[test]
    fn vertex_arm_builds_endpoint_from_project_and_location() {
        let mut providers = HashMap::new();
        providers.insert(
            "vertex".to_string(),
            ProviderOverride {
                base_url: String::new(),
                api_key: None,
                model_limits: HashMap::new(),
                project: Some("my-proj".to_string()),
                location: Some("europe-west1".to_string()),
            },
        );
        let factory = AuthRouteFactory::new(AuthMap::new(), test_transport(), providers)
            .with_vertex_auth(Arc::new(FakeVertexAuth("fake-tok")));

        let (route, model) = factory
            .route_for(&ModelRef::parse("vertex/gemini-2.5-pro"))
            .expect("route builds");
        assert_eq!(route.id(), "gemini");
        assert_eq!(model.route_id, "gemini");

        let p = otto_llm::providers::Vertex::new(
            "my-proj",
            "europe-west1",
            otto_llm::Secret::literal("fake-tok"),
            test_transport(),
        );
        assert_eq!(
            p.endpoint("gemini-2.5-pro").url(),
            "https://europe-west1-aiplatform.googleapis.com/v1/projects/my-proj/locations/europe-west1/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
    }

    /// Missing `provider.vertex.options.project` is a clear config error, not
    /// a panic or a silently-broken route.
    #[test]
    fn vertex_arm_errors_when_project_missing() {
        let factory = AuthRouteFactory::new(AuthMap::new(), test_transport(), HashMap::new())
            .with_vertex_auth(Arc::new(FakeVertexAuth("fake-tok")));

        let err = match factory.route_for(&ModelRef::parse("vertex/gemini-2.5-pro")) {
            Ok(_) => panic!("should error without project configured"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("project"), "message was: {err}");
    }

    /// No `VertexTokenCache`/fake installed at all (e.g. `vertex` never
    /// configured) also errors clearly rather than panicking.
    #[test]
    fn vertex_arm_errors_when_no_auth_installed() {
        let mut providers = HashMap::new();
        providers.insert(
            "vertex".to_string(),
            ProviderOverride {
                base_url: String::new(),
                api_key: None,
                model_limits: HashMap::new(),
                project: Some("my-proj".to_string()),
                location: None,
            },
        );
        let factory = AuthRouteFactory::new(AuthMap::new(), test_transport(), providers);

        let err = match factory.route_for(&ModelRef::parse("vertex/gemini-2.5-pro")) {
            Ok(_) => panic!("should error with no VertexAuth installed"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("credentials"),
            "message was: {err}"
        );
    }

    /// Pins that a non-empty base URL actually reaches the resolved request
    /// endpoint (`{baseURL}/chat/completions`), independent of the opaque
    /// `Route` — `OpenAICompatible::endpoint`/`Endpoint::url` are public from
    /// otto-app, so assert directly rather than downcasting the `Route`.
    #[test]
    fn openai_compatible_endpoint_url_uses_base_url() {
        let p = OpenAICompatible::new(
            "ollama",
            "http://localhost:11434/v1".to_string(),
            None,
            test_transport(),
        );
        assert_eq!(
            p.endpoint().url(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    /// A stored `github-copilot` OAuth credential.
    fn copilot_auth() -> AuthMap {
        let mut auth = AuthMap::new();
        auth.insert(
            "github-copilot".into(),
            Credential::Oauth {
                refresh: "r".into(),
                access: "gho_x".into(),
                expires: i64::MAX,
                account_id: None,
                enterprise_url: None,
            },
        );
        auth
    }

    /// `github-copilot/claude-*` routes through the `Copilot` provider's
    /// Anthropic-protocol arm, not the OpenAI-compatible catch-all — the
    /// route id is `"anthropic"`, which the catch-all (`"openai-compatible-chat"`)
    /// never produces.
    #[test]
    fn github_copilot_claude_routes_to_anthropic_messages_not_catchall() {
        let factory = AuthRouteFactory::new(copilot_auth(), test_transport(), HashMap::new());

        let (route, _model) = factory
            .route_for(&ModelRef::parse("github-copilot/claude-sonnet-4.5"))
            .expect("route builds for github-copilot");
        assert_eq!(route.id(), "anthropic");
    }

    /// `github-copilot/gpt-*` routes through the `Copilot` provider's
    /// OpenAI-chat protocol arm.
    #[test]
    fn github_copilot_gpt_routes_to_openai_chat() {
        let factory = AuthRouteFactory::new(copilot_auth(), test_transport(), HashMap::new());

        let (route, _model) = factory
            .route_for(&ModelRef::parse("github-copilot/gpt-4o"))
            .expect("route builds for github-copilot");
        assert_eq!(route.id(), "openai-chat");
    }

    /// A stored `anthropic` Claude Pro/Max OAuth credential routes through
    /// `Anthropic::new_oauth`, not `Anthropic::new` — confirmed indirectly
    /// (the route still builds and resolves to the anthropic route id); the
    /// actual Bearer-vs-x-api-key/anthropic-beta header difference this
    /// selection produces is unit-tested directly on `Anthropic` in
    /// `otto-llm`, since `route_for` returns an opaque `Arc<dyn Route>`.
    #[test]
    fn anthropic_oauth_credential_still_builds_a_route() {
        let mut auth = AuthMap::new();
        auth.insert(
            "anthropic".into(),
            Credential::Oauth {
                refresh: "r".into(),
                access: "sk-ant-oat01-x".into(),
                expires: i64::MAX,
                account_id: None,
                enterprise_url: None,
            },
        );
        let factory = AuthRouteFactory::new(auth, test_transport(), HashMap::new());

        let (route, model) = factory
            .route_for(&ModelRef::parse("anthropic/claude-sonnet-4-5"))
            .expect("route builds for an OAuth-authenticated anthropic credential");
        assert_eq!(route.id(), "anthropic");
        assert_eq!(model.route_id, "anthropic");
    }
}
