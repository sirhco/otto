//! Generic API-key flow for any provider (openai, openai-compatible, etc.).
//!
//! The trivial `type: "api"` method that every opencode auth plugin exposes as
//! its "Manually enter API Key" method (see the `{ type: "api" }` entry in
//! `plugin/openai/codex.ts` `methods`). Storing the key *is* the whole flow.

use std::collections::BTreeMap;

use crate::credential::Credential;

/// Build an [`Credential::Api`] from a raw key.
///
/// Port of the callback that returns `{ type: "success", key }` for an `api`
/// method in the opencode plugins.
#[must_use]
pub fn credential(key: impl Into<String>) -> Credential {
    Credential::Api {
        key: key.into(),
        metadata: None,
    }
}

/// Build an [`Credential::Api`] with attached string metadata.
#[must_use]
pub fn credential_with_metadata(
    key: impl Into<String>,
    metadata: BTreeMap<String, String>,
) -> Credential {
    Credential::Api {
        key: key.into(),
        metadata: Some(metadata),
    }
}

/// Read an API key from the given environment variable, if present and
/// non-empty.
#[must_use]
pub fn from_env(var: &str) -> Option<Credential> {
    std::env::var(var)
        .ok()
        .filter(|k| !k.is_empty())
        .map(credential)
}
