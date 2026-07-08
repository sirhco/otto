//! The [`RouteFactory`] seam — turning a [`ModelRef`] into a runnable
//! [`Route`] + [`Model`] — and its default credential-backed implementation.

use std::collections::HashMap;
use std::sync::Arc;

use otto_agent::ModelRef;
use otto_auth::{AuthMap, Credential};
use otto_llm::{
    Anthropic, AwsCredentials, Azure, Bedrock, Copilot, Google, HttpTransport, Model, OpenAI,
    OpenAICompatible, Provider, Route, Secret,
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
        }
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
    /// Azure's `resourceName`).
    fn credential_for(&self, provider: &str) -> Option<&Credential> {
        self.auth.get(provider)
    }
}

/// Resolve the Azure resource name: the stored credential's `resourceName`
/// metadata takes priority, falling back to `env` (the caller passes
/// `AZURE_RESOURCE_NAME`, if set). A pure function so the missing-resource
/// error path is unit-testable without mutating process env.
fn resolve_azure_resource(cred: Option<&Credential>, env: Option<String>) -> Result<String> {
    cred.and_then(|c| match c {
        Credential::Api {
            metadata: Some(m), ..
        } => m.get("resourceName").cloned(),
        _ => None,
    })
    .or(env)
    .ok_or_else(|| {
        Error::Route(
            "azure: no resourceName (set auth metadata resourceName or AZURE_RESOURCE_NAME)"
                .to_string(),
        )
    })
}

impl RouteFactory for AuthRouteFactory {
    fn route_for(&self, m: &ModelRef) -> Result<(Arc<dyn Route>, Model)> {
        let provider = m.provider.0.as_str();
        let model_id = m.model.0.as_str();
        let key = self.secret_for(provider);

        let (route, model): (Arc<dyn Route>, Model) = match provider {
            "anthropic" => {
                let p = Anthropic::new(key, self.transport.clone());
                (Arc::from(p.route(model_id)), p.model(model_id))
            }
            "openai" => {
                let p = OpenAI::new(key, self.transport.clone());
                (Arc::from(p.route(model_id)), p.model(model_id))
            }
            "xai" => {
                let p = OpenAICompatible::xai(key, self.transport.clone());
                (Arc::from(p.route(model_id)), p.model(model_id))
            }
            "azure" => {
                let resource = resolve_azure_resource(
                    self.credential_for("azure"),
                    std::env::var("AZURE_RESOURCE_NAME").ok(),
                )?;
                let p = Azure::new(resource, key, self.transport.clone());
                (Arc::from(p.route(model_id)), p.model(model_id))
            }
            "google" | "gemini" => {
                let p = Google::new(key, self.transport.clone());
                (Arc::from(p.route(model_id)), p.model(model_id))
            }
            "amazon-bedrock" | "bedrock" => {
                let creds = AwsCredentials::from_env().ok_or_else(|| {
                    Error::Route(
                        "bedrock: AWS credentials not found (set AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY)"
                            .to_string(),
                    )
                })?;
                let p = Bedrock::new(creds);
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
    use std::collections::BTreeMap;

    /// A shared HTTP transport for tests that build a factory/provider
    /// end-to-end without touching the network.
    fn test_transport() -> Arc<HttpTransport> {
        Arc::new(HttpTransport::new())
    }

    /// A stored `resourceName` metadata entry wins even when the env fallback
    /// is also present.
    #[test]
    fn resolve_azure_resource_prefers_credential_metadata() {
        let mut metadata = BTreeMap::new();
        metadata.insert("resourceName".to_string(), "myres".to_string());
        let cred = Credential::Api {
            key: "ak".into(),
            metadata: Some(metadata),
        };
        let resource =
            resolve_azure_resource(Some(&cred), Some("envres".to_string())).expect("resolved");
        assert_eq!(resource, "myres");
    }

    /// No credential metadata → falls back to the env value.
    #[test]
    fn resolve_azure_resource_falls_back_to_env() {
        let cred = Credential::Api {
            key: "ak".into(),
            metadata: None,
        };
        let resource =
            resolve_azure_resource(Some(&cred), Some("envres".to_string())).expect("resolved");
        assert_eq!(resource, "envres");
    }

    /// Neither credential metadata nor env → a clear error mentioning the
    /// resource name.
    #[test]
    fn resolve_azure_resource_errors_when_neither_present() {
        let err = resolve_azure_resource(None, None).expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("resourceName"), "message was: {msg}");
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
}
