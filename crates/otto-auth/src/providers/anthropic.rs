//! Anthropic (Claude Pro/Max) OAuth flow with PKCE, plus the plain API-key
//! path.
//!
//! The exact Anthropic OAuth flow is **not** present in the opencode repo used
//! as the source of truth (see `packages/core/src/plugin/provider/anthropic.ts`,
//! which only wires the AI SDK and beta headers — no OAuth). This is therefore
//! the standard, well-known Claude Pro/Max console OAuth flow (PKCE S256).
//! Every endpoint and the client id are marked `TODO(confirm)` below.
//!
//! Shape mirrors the OAuth methods in the ported plugins
//! (`plugin/openai/codex.ts`): `authorize_url` builds the authorize URL from a
//! PKCE pair, `exchange` swaps an authorization code for tokens, and `refresh`
//! renews an access token. The token endpoint base URL is injectable so tests
//! can point it at a mock server.

use serde::Deserialize;

use crate::credential::Credential;
use crate::error::{AuthError, Result};
use crate::pkce::Pkce;
use crate::util::now_ms;

// --- Well-known Anthropic OAuth constants (not verifiable from the repo) -----
// TODO(confirm): Anthropic OAuth client id for the Claude Pro/Max console flow.
// This is the widely-used public client id shared by Claude Code / opencode.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
// TODO(confirm): browser authorize endpoint (claude.ai variant).
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
// TODO(confirm): token endpoint (console.anthropic.com).
const DEFAULT_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
// TODO(confirm): OAuth redirect URI registered for the above client id.
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
// TODO(confirm): requested scopes.
const SCOPES: &str = "org:create_api_key user:profile user:inference";

/// The Anthropic OAuth client.
///
/// Holds the token endpoint (injectable for tests) and a reusable HTTP client.
pub struct AnthropicOAuth {
    token_url: String,
    client: reqwest::Client,
}

impl Default for AnthropicOAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicOAuth {
    /// Client pointed at the real Anthropic token endpoint.
    #[must_use]
    pub fn new() -> Self {
        Self {
            token_url: DEFAULT_TOKEN_URL.to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Client pointed at an explicit token endpoint — used by tests to target
    /// a mock server instead of the real Anthropic endpoint.
    #[must_use]
    pub fn with_token_url(token_url: impl Into<String>) -> Self {
        Self {
            token_url: token_url.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Build the browser authorize URL for a PKCE pair.
    ///
    /// Returns `(url, verifier)`. The caller opens `url`, and later feeds the
    /// returned `verifier` (plus the callback `code`) to [`Self::exchange`].
    /// The URL carries `code_challenge`, `code_challenge_method=S256` and the
    /// client id. `state` is set to the verifier so the callback (which
    /// appends `#state`) round-trips it back.
    #[must_use]
    pub fn authorize_url(&self, pkce: &Pkce) -> (String, String) {
        let mut url = url::Url::parse(AUTHORIZE_URL).expect("valid authorize url const");
        url.query_pairs_mut()
            .append_pair("code", "true")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", SCOPES)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &pkce.verifier);
        (url.to_string(), pkce.verifier.clone())
    }

    /// Exchange an authorization `code` for tokens.
    ///
    /// The Claude console callback returns the code as `code#state`; this
    /// splits on `#` so either form works. Produces a
    /// [`Credential::Oauth`] with `expires` set to `now + expires_in` (ms).
    ///
    /// # Errors
    /// [`AuthError::Transport`] on network failure, [`AuthError::Http`] on a
    /// non-2xx response, [`AuthError::Parse`] on a malformed body.
    pub async fn exchange(&self, code: &str, verifier: &str) -> Result<Credential> {
        let (code, state) = split_code(code);
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
            "state": state,
            "client_id": CLIENT_ID,
            "redirect_uri": REDIRECT_URI,
            "code_verifier": verifier,
        });
        self.post_token(body).await
    }

    /// Refresh an access token from a refresh token.
    ///
    /// # Errors
    /// As [`Self::exchange`].
    pub async fn refresh(&self, refresh_token: &str) -> Result<Credential> {
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
        });
        self.post_token(body).await
    }

    /// POST a token request and map the response to a [`Credential::Oauth`].
    async fn post_token(&self, body: serde_json::Value) -> Result<Credential> {
        let resp = self
            .client
            .post(&self.token_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AuthError::Transport(e.to_string()))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| AuthError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(AuthError::Http {
                status: status.as_u16(),
                message: text,
            });
        }

        let token: TokenResponse =
            serde_json::from_str(&text).map_err(|e| AuthError::Parse(e.to_string()))?;
        Ok(Credential::Oauth {
            refresh: token.refresh_token,
            access: token.access_token,
            expires: now_ms() + token.expires_in.unwrap_or(3600) * 1000,
            account_id: None,
            enterprise_url: None,
        })
    }
}

/// Token endpoint response body.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    /// Lifetime in seconds; defaults to 3600 when absent.
    expires_in: Option<i64>,
}

/// Split a `code#state` callback value into `(code, state)`.
fn split_code(raw: &str) -> (&str, &str) {
    match raw.split_once('#') {
        Some((code, state)) => (code, state),
        None => (raw, ""),
    }
}

/// Build an [`Credential::Api`] for a plain Anthropic API key.
///
/// The direct-key path (user-supplied key or `ANTHROPIC_API_KEY`), independent
/// of the OAuth flow.
#[must_use]
pub fn api_key_credential(key: impl Into<String>) -> Credential {
    Credential::Api {
        key: key.into(),
        metadata: None,
    }
}

/// Read `ANTHROPIC_API_KEY` from the environment, if present, as a credential.
#[must_use]
pub fn api_key_from_env() -> Option<Credential> {
    std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .map(api_key_credential)
}
