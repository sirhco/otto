//! Credential store — port of opencode `packages/opencode/src/auth/index.ts`.
//!
//! Persists a map of `provider id -> `[`Credential`]` as pretty JSON at
//! `<data_dir>/otto/auth.json`, written with mode `0600` on unix. Honours
//! the `OTTO_AUTH_CONTENT` environment override (read-only short-circuit)
//! and normalises trailing slashes on `set`/`remove`, exactly like the
//! TypeScript `Auth` service.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::credential::Credential;
use crate::error::{AuthError, Result};

/// Dummy API key handed to SDKs that require *some* key when the real auth is
/// OAuth-based. Port of `OAUTH_DUMMY_KEY` in opencode `auth/index.ts`.
pub const OAUTH_DUMMY_KEY: &str = "otto-oauth-dummy-key";

/// Environment variable that, when set, replaces the entire contents of
/// `auth.json` (read-only). Port of the `OPENCODE_AUTH_CONTENT` branch in
/// `auth/index.ts` `all()`, namespaced to otto's own env var.
pub const AUTH_CONTENT_ENV: &str = "OTTO_AUTH_CONTENT";

/// The decoded `auth.json` map: provider id -> credential.
pub type AuthMap = BTreeMap<String, Credential>;

/// Where the store reads/writes credentials from.
enum Source {
    /// A real file on disk (default and `with_path`).
    File(PathBuf),
    /// Explicit in-memory content — read-only. Used for tests and as the
    /// programmatic equivalent of `OPENCODE_AUTH_CONTENT`.
    Content(String),
}

/// The credential store.
///
/// Port of the `Auth` service in opencode `auth/index.ts`. Methods are
/// synchronous file operations; provider OAuth flows (which are async) call
/// [`AuthStore::set`] to persist refreshed tokens.
pub struct AuthStore {
    source: Source,
}

impl AuthStore {
    /// Store backed by the default `auth.json` path
    /// (`<data_dir>/otto/auth.json`).
    ///
    /// Port of `const file = path.join(Global.Path.data, "auth.json")` in
    /// `auth/index.ts`. `Global.Path.data` is the XDG data dir under
    /// `otto`.
    ///
    /// # Errors
    /// Returns [`AuthError::Io`] if the platform data directory cannot be
    /// resolved.
    pub fn new() -> Result<Self> {
        Ok(Self {
            source: Source::File(default_path()?),
        })
    }

    /// Store backed by an explicit file path. Primarily for tests.
    #[must_use]
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            source: Source::File(path.into()),
        }
    }

    /// Read-only store backed by explicit JSON content — the programmatic
    /// equivalent of the `OTTO_AUTH_CONTENT` override, without racing on a
    /// process-global env var.
    #[must_use]
    pub fn with_content(content: impl Into<String>) -> Self {
        Self {
            source: Source::Content(content.into()),
        }
    }

    /// Returns the full credential map.
    ///
    /// Port of `Auth.all`. Precedence:
    /// 1. explicit [`AuthStore::with_content`] content, else
    /// 2. the `OTTO_AUTH_CONTENT` env var (if set and valid JSON), else
    /// 3. the on-disk `auth.json` (missing file -> empty map).
    ///
    /// Entries that fail to decode as a [`Credential`] are silently dropped,
    /// mirroring the `Record.filterMap` in `auth/index.ts`.
    ///
    /// # Errors
    /// Returns [`AuthError::Parse`] only for explicit-content sources whose
    /// JSON is malformed; disk/env errors fall back to an empty map.
    pub fn all(&self) -> Result<AuthMap> {
        match &self.source {
            Source::Content(content) => {
                parse_map(content).map_err(|e| AuthError::Parse(e.to_string()))
            }
            Source::File(path) => {
                // OTTO_AUTH_CONTENT env override (read-only short-circuit).
                if let Ok(content) = std::env::var(AUTH_CONTENT_ENV)
                    && let Ok(map) = parse_map(&content)
                {
                    return Ok(map);
                }
                match std::fs::read_to_string(path) {
                    Ok(content) => Ok(parse_map(&content).unwrap_or_default()),
                    // A missing (or unreadable) file is an empty map, matching
                    // `Effect.orElseSucceed(() => ({}))`.
                    Err(_) => Ok(AuthMap::new()),
                }
            }
        }
    }

    /// Returns the credential for `provider`, if any. Port of `Auth.get`.
    ///
    /// # Errors
    /// Propagates errors from [`AuthStore::all`].
    pub fn get(&self, provider: &str) -> Result<Option<Credential>> {
        Ok(self.all()?.get(provider).cloned())
    }

    /// Stores `credential` under `provider`, normalising trailing slashes.
    ///
    /// Port of `Auth.set`: the key is stripped of trailing slashes; any
    /// pre-existing raw or `norm + "/"` entry is removed before inserting.
    ///
    /// # Errors
    /// Returns [`AuthError::Io`] on a read-only source or on write failure.
    pub fn set(&self, provider: &str, credential: Credential) -> Result<()> {
        let norm = provider.trim_end_matches('/');
        let mut data = self.all()?;
        if norm != provider {
            data.remove(provider);
        }
        data.remove(&format!("{norm}/"));
        data.insert(norm.to_string(), credential);
        self.write(&data)
    }

    /// Removes the credential for `provider`, deleting both the raw and the
    /// slash-normalised key. Port of `Auth.remove`.
    ///
    /// # Errors
    /// Returns [`AuthError::Io`] on a read-only source or on write failure.
    pub fn remove(&self, provider: &str) -> Result<()> {
        let norm = provider.trim_end_matches('/');
        let mut data = self.all()?;
        data.remove(provider);
        data.remove(norm);
        self.write(&data)
    }

    /// Writes `data` back to disk as pretty JSON with mode `0600`.
    fn write(&self, data: &AuthMap) -> Result<()> {
        let path = match &self.source {
            Source::File(path) => path,
            Source::Content(_) => {
                return Err(AuthError::Io(
                    "cannot write to a read-only content-backed store".to_string(),
                ));
            }
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AuthError::Io(e.to_string()))?;
        }
        let json =
            serde_json::to_string_pretty(data).map_err(|e| AuthError::Parse(e.to_string()))?;
        std::fs::write(path, json).map_err(|e| AuthError::Io(e.to_string()))?;
        set_mode_0600(path)?;
        Ok(())
    }

    /// The path this store writes to, if it is file-backed.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match &self.source {
            Source::File(path) => Some(path.as_path()),
            Source::Content(_) => None,
        }
    }
}

/// Default `auth.json` path: `<data_dir>/otto/auth.json`.
fn default_path() -> Result<PathBuf> {
    let data = dirs::data_dir()
        .ok_or_else(|| AuthError::Io("could not resolve platform data directory".to_string()))?;
    Ok(data.join("otto").join("auth.json"))
}

/// Parse a JSON object into an [`AuthMap`], dropping entries that do not decode
/// as a [`Credential`] (mirrors opencode's `Record.filterMap`).
fn parse_map(content: &str) -> std::result::Result<AuthMap, serde_json::Error> {
    let raw: serde_json::Map<String, serde_json::Value> = serde_json::from_str(content)?;
    let mut map = AuthMap::new();
    for (key, value) in raw {
        if let Ok(cred) = serde_json::from_value::<Credential>(value) {
            map.insert(key, cred);
        }
    }
    Ok(map)
}

/// Set file permissions to `0600` on unix; best-effort no-op elsewhere.
#[cfg(unix)]
fn set_mode_0600(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms).map_err(|e| AuthError::Io(e.to_string()))
}

/// Non-unix best-effort: file permissions are left at platform defaults.
#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) -> Result<()> {
    Ok(())
}
