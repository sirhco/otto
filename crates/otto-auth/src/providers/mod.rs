//! Provider auth flows + load-time credential resolution.
//!
//! Concrete, hardcoded ports of the opencode auth plugins (otto has no JS
//! plugin runtime):
//! - [`anthropic`] — OAuth (PKCE S256) + API key.
//! - [`api_key`] — the generic `type: "api"` flow used by every provider.
//! - [`copilot`] — the GitHub Copilot device-code flow.
//!
//! [`Resolver::resolve`] reproduces the token-refresh-at-load behaviour from
//! the plugin `loader`s (e.g. `plugin/openai/codex.ts`): an OAuth credential
//! whose `expires` is at/near the current time is refreshed via the provider's
//! refresh flow and the new tokens are persisted before use.

pub mod anthropic;
pub mod api_key;
pub mod copilot;

use crate::credential::Credential;
use crate::error::Result;
use crate::store::AuthStore;
use crate::util::now_ms;

/// Default refresh margin: refresh an OAuth credential that expires within the
/// next 60 seconds.
pub const DEFAULT_EXPIRY_MARGIN_MS: i64 = 60_000;

/// A credential resolved for use, tagged with whether a refresh happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCredential {
    /// The (possibly refreshed) credential.
    pub credential: Credential,
    /// `true` if a token refresh was performed and persisted during resolve.
    pub refreshed: bool,
}

/// Resolves stored credentials at load time, refreshing expired OAuth tokens.
///
/// The token endpoint overrides make the refresh path testable against a mock
/// server without touching real endpoints.
pub struct Resolver {
    /// Override for the Anthropic token endpoint (tests point this at a mock).
    pub anthropic_token_url: Option<String>,
    /// How far before actual expiry to trigger a refresh, in milliseconds.
    pub expiry_margin_ms: i64,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

impl Resolver {
    /// Resolver pointed at real endpoints with the default expiry margin.
    #[must_use]
    pub fn new() -> Self {
        Self {
            anthropic_token_url: None,
            expiry_margin_ms: DEFAULT_EXPIRY_MARGIN_MS,
        }
    }

    /// Resolve the credential for `provider` from `store`.
    ///
    /// Returns `None` if no credential is stored. For an [`Credential::Oauth`]
    /// whose `expires` is at/near now (see [`Credential::is_expired`]) and
    /// whose provider has a refresh flow, the token is refreshed and the new
    /// credential persisted to `store` before being returned.
    ///
    /// Providers without a refresh flow (e.g. `github-copilot`, whose token is
    /// long-lived with `expires: 0`) are returned unchanged.
    ///
    /// # Errors
    /// Propagates store I/O errors and any error from the provider refresh.
    pub async fn resolve(
        &self,
        provider: &str,
        store: &AuthStore,
    ) -> Result<Option<ResolvedCredential>> {
        let Some(credential) = store.get(provider)? else {
            return Ok(None);
        };

        if !credential.is_expired(now_ms(), self.expiry_margin_ms) {
            return Ok(Some(ResolvedCredential {
                credential,
                refreshed: false,
            }));
        }

        // Expired OAuth — refresh if the provider supports it.
        if let Credential::Oauth { refresh, .. } = &credential
            && let Some(refreshed) = self.refresh(provider, refresh).await?
        {
            store.set(provider, refreshed.clone())?;
            return Ok(Some(ResolvedCredential {
                credential: refreshed,
                refreshed: true,
            }));
        }

        // No refresh flow for this provider — hand back what we have.
        Ok(Some(ResolvedCredential {
            credential,
            refreshed: false,
        }))
    }

    /// Dispatch a refresh to the provider-specific flow. Returns `None` when
    /// the provider has no refresh flow.
    async fn refresh(&self, provider: &str, refresh_token: &str) -> Result<Option<Credential>> {
        match provider {
            "anthropic" => {
                let client = match &self.anthropic_token_url {
                    Some(url) => anthropic::AnthropicOAuth::with_token_url(url),
                    None => anthropic::AnthropicOAuth::new(),
                };
                Ok(Some(client.refresh(refresh_token).await?))
            }
            // github-copilot tokens are long-lived and used directly; no
            // refresh flow (matches the copilot plugin, which never refreshes).
            _ => Ok(None),
        }
    }
}

/// Convenience: resolve with a default [`Resolver`] (real endpoints).
///
/// # Errors
/// See [`Resolver::resolve`].
pub async fn resolve(provider: &str, store: &AuthStore) -> Result<Option<ResolvedCredential>> {
    Resolver::new().resolve(provider, store).await
}
