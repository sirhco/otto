//! GitHub Copilot device-code OAuth flow.
//!
//! Port of the device flow in opencode
//! `packages/opencode/src/plugin/github-copilot/copilot.ts`: request a device
//! code, show the user the code + verification URI, then poll the access-token
//! endpoint until the user authorises. On success the GitHub OAuth token is
//! stored as *both* `access` and `refresh` with `expires: 0` (copilot uses the
//! token directly and does not refresh it — matching the plugin's
//! `expires: 0`). The endpoint base is injectable for tests.

use serde::Deserialize;

use crate::credential::Credential;
use crate::error::{AuthError, Result};

// otto's own GitHub OAuth App (device flow enabled), registered separately
// from opencode's `copilot.ts` client id this flow was ported from.
const CLIENT_ID: &str = "Ov23liNvyZyqKthwNsRN";
const GITHUB_BASE: &str = "https://github.com";
const SCOPE: &str = "read:user";

/// The device-code start response — port of the `deviceData` shape in
/// `copilot.ts`.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceStart {
    /// The opaque device code used when polling.
    pub device_code: String,
    /// The short code the user types into the verification page.
    pub user_code: String,
    /// The URL the user visits to enter `user_code`.
    pub verification_uri: String,
    /// Minimum seconds to wait between polls.
    pub interval: u64,
}

/// The outcome of a single poll of the access-token endpoint.
///
/// Port of the three-way branch in the `copilot.ts` `callback` loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePoll {
    /// Authorisation complete — the resulting credential.
    Complete(Box<Credential>),
    /// User has not authorised yet (`error: "authorization_pending"`); wait
    /// `interval` and poll again.
    Pending,
    /// Server asked us to back off (`error: "slow_down"`); increase the
    /// interval before polling again.
    SlowDown,
}

/// Strip a URL scheme and any trailing slash, leaving a bare domain.
///
/// Port of `normalizeDomain` in `copilot.ts`
/// (`url.replace(/^https?:\/\//, "").replace(/\/$/, "")`) — users paste either
/// `company.ghe.com` or `https://company.ghe.com`, and both must resolve to
/// the same host.
#[must_use]
pub fn normalize_domain(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

/// The GitHub Copilot device-flow client.
pub struct CopilotOAuth {
    device_code_url: String,
    access_token_url: String,
    enterprise_domain: Option<String>,
    client: reqwest::Client,
}

impl Default for CopilotOAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl CopilotOAuth {
    /// Client pointed at public `github.com`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url(GITHUB_BASE)
    }

    /// Client for a GitHub Enterprise deployment.
    ///
    /// Points the device flow at the enterprise host **and** records the
    /// domain for the resulting credential — both halves are required. Port
    /// of `copilot.ts`'s enterprise branch, where
    /// `domain = normalizeDomain(enterpriseUrl)` feeds `getUrls(domain)`.
    ///
    /// Authenticating against `github.com` while claiming an enterprise
    /// deployment yields a token the enterprise Copilot API will not honor.
    #[must_use]
    pub fn enterprise(domain: &str) -> Self {
        let domain = normalize_domain(domain);
        Self::with_base_url(format!("https://{domain}")).with_enterprise_domain(domain)
    }

    /// Client pointed at an explicit base URL (test mock, or a GitHub
    /// Enterprise host). Endpoints are derived as `<base>/login/device/code`
    /// and `<base>/login/oauth/access_token`, matching `getUrls(domain)` in
    /// `copilot.ts`.
    #[must_use]
    pub fn with_base_url(base: impl AsRef<str>) -> Self {
        let base = base.as_ref().trim_end_matches('/');
        Self {
            device_code_url: format!("{base}/login/device/code"),
            access_token_url: format!("{base}/login/oauth/access_token"),
            enterprise_domain: None,
            client: reqwest::Client::new(),
        }
    }

    /// Record a GitHub Enterprise domain to stamp onto the resulting
    /// credential's `enterprise_url` (port of the `result.enterpriseUrl`
    /// branch).
    #[must_use]
    pub fn with_enterprise_domain(mut self, domain: impl Into<String>) -> Self {
        self.enterprise_domain = Some(domain.into());
        self
    }

    /// Start the device flow: request a device + user code.
    ///
    /// # Errors
    /// [`AuthError::Transport`] on network failure, [`AuthError::Http`] on a
    /// non-2xx response, [`AuthError::Parse`] on a malformed body.
    pub async fn start_device(&self) -> Result<DeviceStart> {
        let resp = self
            .client
            .post(&self.device_code_url)
            .header("Accept", "application/json")
            .json(&serde_json::json!({ "client_id": CLIENT_ID, "scope": SCOPE }))
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
        serde_json::from_str(&text).map_err(|e| AuthError::Parse(e.to_string()))
    }

    /// Poll the access-token endpoint once for the given `device_code`.
    ///
    /// Returns [`DevicePoll::Complete`] with the credential on success, or
    /// [`DevicePoll::Pending`] / [`DevicePoll::SlowDown`] to signal the caller
    /// to keep waiting. Port of one iteration of the `copilot.ts` `callback`
    /// loop.
    ///
    /// # Errors
    /// [`AuthError::Transport`] on network failure, [`AuthError::Http`] on a
    /// non-2xx response, [`AuthError::Oauth`] on a terminal OAuth error
    /// (`access_denied`, `expired_token`, ...).
    pub async fn poll(&self, device_code: &str) -> Result<DevicePoll> {
        let resp = self
            .client
            .post(&self.access_token_url)
            .header("Accept", "application/json")
            .json(&serde_json::json!({
                "client_id": CLIENT_ID,
                "device_code": device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }))
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

        let data: TokenResponse =
            serde_json::from_str(&text).map_err(|e| AuthError::Parse(e.to_string()))?;

        if let Some(token) = data.access_token {
            // Copilot stores the token as both access and refresh with
            // expires: 0 (it is used directly and never refreshed).
            let credential = Credential::Oauth {
                refresh: token.clone(),
                access: token,
                expires: 0,
                account_id: None,
                enterprise_url: self.enterprise_domain.clone(),
            };
            return Ok(DevicePoll::Complete(Box::new(credential)));
        }

        match data.error.as_deref() {
            Some("authorization_pending") => Ok(DevicePoll::Pending),
            Some("slow_down") => Ok(DevicePoll::SlowDown),
            Some(other) => Err(AuthError::Oauth(other.to_string())),
            None => Err(AuthError::Oauth("empty device token response".to_string())),
        }
    }
}

/// Access-token endpoint response — port of the `data` shape in `copilot.ts`.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_domain_strips_scheme_and_trailing_slash() {
        for raw in [
            "company.ghe.com",
            "https://company.ghe.com",
            "http://company.ghe.com",
            "https://company.ghe.com/",
            "  https://company.ghe.com/  ",
        ] {
            assert_eq!(normalize_domain(raw), "company.ghe.com", "input: {raw:?}");
        }
    }

    /// The public client authenticates against github.com.
    #[test]
    fn default_client_targets_github_dot_com() {
        let f = CopilotOAuth::new();
        assert_eq!(f.device_code_url, "https://github.com/login/device/code");
        assert_eq!(
            f.access_token_url,
            "https://github.com/login/oauth/access_token"
        );
        assert_eq!(f.enterprise_domain, None);
    }

    /// Regression: `--enterprise` recorded the domain on the credential but
    /// left the device flow pointed at github.com, so an enterprise
    /// deployment authenticated against the wrong server and received a token
    /// its Copilot API would not honor. Upstream derives BOTH from the
    /// domain (`getUrls(domain)` + `result.enterpriseUrl = domain`).
    #[test]
    fn enterprise_client_targets_the_enterprise_host_and_records_the_domain() {
        let f = CopilotOAuth::enterprise("https://company.ghe.com/");
        assert_eq!(
            f.device_code_url,
            "https://company.ghe.com/login/device/code"
        );
        assert_eq!(
            f.access_token_url,
            "https://company.ghe.com/login/oauth/access_token"
        );
        assert_eq!(f.enterprise_domain.as_deref(), Some("company.ghe.com"));
    }
}
