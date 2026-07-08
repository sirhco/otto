//! Error type for the credential store + provider OAuth flows.
//!
//! Port of the `AuthError` tagged error raised across opencode
//! `packages/opencode/src/auth/index.ts` and the provider auth plugins
//! (`plugin/openai/codex.ts`, `plugin/github-copilot/copilot.ts`). The
//! variants stay faithful to the failure *kinds* those modules raise while
//! remaining pragmatic for Rust.

use thiserror::Error;

/// Errors produced by the credential store and the provider OAuth flows.
///
/// Port of the `AuthError` class in opencode `auth/index.ts` plus the ad-hoc
/// `throw new Error(...)` sites in the auth plugins.
#[derive(Debug, Error)]
pub enum AuthError {
    /// A filesystem failure while reading or writing `auth.json`.
    ///
    /// Mirrors the `"Failed to write auth data"` failures in `auth/index.ts`.
    #[error("io error: {0}")]
    Io(String),

    /// The stored auth data (or `OPENCODE_AUTH_CONTENT`) could not be parsed.
    ///
    /// Mirrors the `JSON.parse` / schema-decode paths in `auth/index.ts`.
    #[error("failed to parse auth data: {0}")]
    Parse(String),

    /// A transport-level failure (connection, TLS, I/O) talking to a token
    /// endpoint.
    #[error("transport error: {0}")]
    Transport(String),

    /// A token endpoint returned a non-success HTTP status.
    ///
    /// Mirrors the `Token exchange failed: ${status}` / `Token refresh failed`
    /// throws in `plugin/openai/codex.ts`.
    #[error("http error: status {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Response body (best effort).
        message: String,
    },

    /// An OAuth flow failed for a reason that is not a plain HTTP status
    /// (e.g. the device flow returned `error: "access_denied"`).
    #[error("oauth flow error: {0}")]
    Oauth(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, AuthError>;
