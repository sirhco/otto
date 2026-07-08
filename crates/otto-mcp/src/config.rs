//! MCP server configuration — a Rust port of opencode's `ConfigMCPV1`
//! (`packages/core/src/v1/config/mcp.ts`).
//!
//! opencode models an MCP entry as a discriminated union tagged by `type`
//! (`local` / `remote`). [`McpServerConfig`] reproduces that union, and
//! [`OAuthSetting`] reproduces the `OAuth | false` union used to either
//! configure or explicitly disable OAuth on a remote server
//! (`mcp.ts:53-55`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Default request timeout in milliseconds when a server config omits one.
///
/// Mirrors the documented default in `mcp.ts:21`/`mcp.ts:57` ("Defaults to
/// 5000 (5 seconds) if not specified"). Note opencode's runtime `index.ts`
/// uses a larger `DEFAULT_TIMEOUT` for the wire library itself; the config
/// default surfaced to users is 5000.
pub const DEFAULT_TIMEOUT: u64 = 5000;

/// Default port for the local OAuth callback server (`mcp.ts:34-36`).
pub const DEFAULT_CALLBACK_PORT: u16 = 19876;

/// Default OAuth redirect URI (`mcp.ts:38-39`).
pub const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:19876/mcp/oauth/callback";

/// A single MCP server entry. Ported from opencode `ConfigMCPV1.Info`
/// (`mcp.ts:62`), the `Schema.Union([Local, Remote])` discriminated on `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    /// A local server launched as a child process over stdio
    /// (`mcp.ts:6-23`, `McpLocalConfig`).
    Local {
        /// Command and arguments to run the MCP server (`command[0]` is the
        /// executable, the rest are args).
        command: Vec<String>,
        /// Working directory for the server process. Relative paths resolve
        /// from the workspace directory.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        cwd: Option<String>,
        /// Environment variables to set for the server process.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        environment: Option<BTreeMap<String, String>>,
        /// Enable/disable the server on startup (default enabled).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        enabled: Option<bool>,
        /// Per-request timeout in ms (see [`DEFAULT_TIMEOUT`]).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        timeout: Option<u64>,
    },
    /// A remote server dialed over streamable-HTTP (with SSE fallback)
    /// (`mcp.ts:44-60`, `McpRemoteConfig`).
    Remote {
        /// URL of the remote MCP server.
        url: String,
        /// Enable/disable the server on startup (default enabled).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        enabled: Option<bool>,
        /// Extra headers to send with every request.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        headers: Option<BTreeMap<String, String>>,
        /// OAuth configuration, or `false` to disable OAuth auto-detection
        /// (`mcp.ts:53-55`).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        oauth: Option<OAuthSetting>,
        /// Per-request timeout in ms (see [`DEFAULT_TIMEOUT`]).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        timeout: Option<u64>,
    },
}

impl McpServerConfig {
    /// Whether this server should be started. Ports the `enabled === false`
    /// gate in `index.ts:366`/`index.ts:506`; a missing value defaults to
    /// enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        match self {
            McpServerConfig::Local { enabled, .. } | McpServerConfig::Remote { enabled, .. } => {
                enabled.unwrap_or(true)
            }
        }
    }

    /// The effective request timeout in ms, falling back to [`DEFAULT_TIMEOUT`].
    #[must_use]
    pub fn timeout_ms(&self) -> u64 {
        match self {
            McpServerConfig::Local { timeout, .. } | McpServerConfig::Remote { timeout, .. } => {
                timeout.unwrap_or(DEFAULT_TIMEOUT)
            }
        }
    }
}

/// The `oauth` field of a remote server: either a configuration object or the
/// literal `false` to disable OAuth. Ports `Schema.Union([OAuth, Literal(false)])`
/// (`mcp.ts:53`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OAuthSetting {
    /// An explicit OAuth configuration object.
    Config(OAuthConfig),
    /// `false` — disable OAuth auto-detection.
    Disabled(bool),
}

impl OAuthSetting {
    /// True when OAuth is explicitly disabled (`oauth === false` in
    /// `index.ts:232`).
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        matches!(self, OAuthSetting::Disabled(_))
    }

    /// The OAuth config object, if present (returns `None` when disabled).
    #[must_use]
    pub fn config(&self) -> Option<&OAuthConfig> {
        match self {
            OAuthSetting::Config(c) => Some(c),
            OAuthSetting::Disabled(_) => None,
        }
    }
}

/// OAuth authentication configuration for a remote MCP server. Ports
/// `McpOAuthConfig` (`mcp.ts:26-42`). Field names are camelCase on the wire to
/// match opencode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OAuthConfig {
    /// OAuth client ID. If absent, dynamic client registration (RFC 7591) is
    /// attempted.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_id: Option<String>,
    /// OAuth client secret, if required by the authorization server.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_secret: Option<String>,
    /// OAuth scopes to request during authorization.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub scope: Option<String>,
    /// Port for the local OAuth callback server (see [`DEFAULT_CALLBACK_PORT`]).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub callback_port: Option<u16>,
    /// OAuth redirect URI (see [`DEFAULT_REDIRECT_URI`]).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub redirect_uri: Option<String>,
}

impl OAuthConfig {
    /// Effective callback port, defaulting to [`DEFAULT_CALLBACK_PORT`].
    #[must_use]
    pub fn callback_port(&self) -> u16 {
        self.callback_port.unwrap_or(DEFAULT_CALLBACK_PORT)
    }

    /// Effective redirect URI. Mirrors opencode's resolution order
    /// (`index.ts:810-812`): explicit `redirectUri` > `callbackPort` shorthand
    /// > [`DEFAULT_REDIRECT_URI`].
    #[must_use]
    pub fn redirect_uri(&self) -> String {
        if let Some(uri) = &self.redirect_uri {
            return uri.clone();
        }
        if let Some(port) = self.callback_port {
            return format!("http://127.0.0.1:{port}/mcp/oauth/callback");
        }
        DEFAULT_REDIRECT_URI.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn local_round_trip() {
        let cfg = McpServerConfig::Local {
            command: vec!["node".into(), "server.js".into()],
            cwd: Some("./sub".into()),
            environment: Some(BTreeMap::from([("FOO".to_string(), "bar".to_string())])),
            enabled: Some(true),
            timeout: Some(1234),
        };
        let value = serde_json::to_value(&cfg).unwrap();
        assert_eq!(value["type"], "local");
        assert_eq!(value["command"][0], "node");
        assert_eq!(value["environment"]["FOO"], "bar");
        let back: McpServerConfig = serde_json::from_value(value).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn local_minimal_deserializes() {
        let cfg: McpServerConfig =
            serde_json::from_value(json!({ "type": "local", "command": ["uvx", "mcp"] })).unwrap();
        match &cfg {
            McpServerConfig::Local { command, cwd, .. } => {
                assert_eq!(command, &vec!["uvx".to_string(), "mcp".to_string()]);
                assert!(cwd.is_none());
            }
            other => panic!("expected local, got {other:?}"),
        }
        assert!(cfg.is_enabled());
        assert_eq!(cfg.timeout_ms(), DEFAULT_TIMEOUT);
    }

    #[test]
    fn remote_round_trip_with_oauth_object() {
        let cfg = McpServerConfig::Remote {
            url: "https://example.com/mcp".into(),
            enabled: None,
            headers: Some(BTreeMap::from([(
                "Authorization".to_string(),
                "Bearer x".to_string(),
            )])),
            oauth: Some(OAuthSetting::Config(OAuthConfig {
                client_id: Some("cid".into()),
                ..Default::default()
            })),
            timeout: None,
        };
        let value = serde_json::to_value(&cfg).unwrap();
        assert_eq!(value["type"], "remote");
        assert_eq!(value["oauth"]["clientId"], "cid");
        let back: McpServerConfig = serde_json::from_value(value).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn remote_oauth_false_disables() {
        let cfg: McpServerConfig = serde_json::from_value(json!({
            "type": "remote",
            "url": "https://example.com/mcp",
            "oauth": false
        }))
        .unwrap();
        match &cfg {
            McpServerConfig::Remote { oauth, .. } => {
                let oauth = oauth.as_ref().expect("oauth present");
                assert!(oauth.is_disabled());
                assert!(oauth.config().is_none());
            }
            other => panic!("expected remote, got {other:?}"),
        }
        // `false` round-trips back to a JSON boolean, not an object.
        assert_eq!(serde_json::to_value(&cfg).unwrap()["oauth"], json!(false));
    }

    #[test]
    fn enabled_false_is_disabled() {
        let cfg = McpServerConfig::Local {
            command: vec!["x".into()],
            cwd: None,
            environment: None,
            enabled: Some(false),
            timeout: None,
        };
        assert!(!cfg.is_enabled());
    }

    #[test]
    fn oauth_defaults() {
        let oauth = OAuthConfig::default();
        assert_eq!(oauth.callback_port(), DEFAULT_CALLBACK_PORT);
        assert_eq!(oauth.redirect_uri(), DEFAULT_REDIRECT_URI);

        let with_port = OAuthConfig {
            callback_port: Some(2000),
            ..Default::default()
        };
        assert_eq!(with_port.callback_port(), 2000);
        assert_eq!(
            with_port.redirect_uri(),
            "http://127.0.0.1:2000/mcp/oauth/callback"
        );

        let with_uri = OAuthConfig {
            callback_port: Some(2000),
            redirect_uri: Some("https://app/cb".into()),
            ..Default::default()
        };
        assert_eq!(with_uri.redirect_uri(), "https://app/cb");
    }
}
