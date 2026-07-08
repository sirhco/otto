//! The stored credential model.
//!
//! Port of the `Oauth | Api | WellKnown` union in opencode
//! `packages/opencode/src/auth/index.ts` (the `Info` schema). The serde tag is
//! `type`, matching the on-disk `auth.json` shape exactly so an opencode
//! `auth.json` deserializes unchanged.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A single stored credential, keyed by provider id in `auth.json`.
///
/// Serde tag is `type` with values `"oauth" | "api" | "wellknown"` — a direct
/// port of the `Info` union in opencode `auth/index.ts` (`Oauth`, `Api`,
/// `WellKnown` classes). Field names use the same camelCase wire names
/// (`accountId`, `enterpriseUrl`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Credential {
    /// OAuth credential — port of the `Oauth` class.
    ///
    /// `expires` is a Unix timestamp in **milliseconds** (matching opencode's
    /// `Date.now() + expires_in * 1000`).
    Oauth {
        /// Refresh token.
        refresh: String,
        /// Access token (bearer).
        access: String,
        /// Absolute expiry, Unix epoch milliseconds.
        expires: i64,
        /// Optional provider account id (e.g. ChatGPT account id).
        #[serde(rename = "accountId", skip_serializing_if = "Option::is_none", default)]
        account_id: Option<String>,
        /// Optional GitHub Enterprise base domain (copilot).
        #[serde(
            rename = "enterpriseUrl",
            skip_serializing_if = "Option::is_none",
            default
        )]
        enterprise_url: Option<String>,
    },

    /// Plain API-key credential — port of the `Api` class.
    Api {
        /// The API key.
        key: String,
        /// Optional free-form string metadata.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        metadata: Option<BTreeMap<String, String>>,
    },

    /// Well-known key/token pair — port of the `WellKnown` class.
    WellKnown {
        /// The well-known key.
        key: String,
        /// The associated token.
        token: String,
    },
}

impl Credential {
    /// Returns `true` when this is an [`Credential::Oauth`] whose `expires`
    /// timestamp is at or before `now_ms + margin_ms`.
    ///
    /// Drives the token-refresh-at-load behaviour in
    /// [`crate::providers::resolve`]. Non-OAuth credentials never expire.
    #[must_use]
    pub fn is_expired(&self, now_ms: i64, margin_ms: i64) -> bool {
        match self {
            Credential::Oauth { expires, .. } => *expires <= now_ms + margin_ms,
            _ => false,
        }
    }
}
