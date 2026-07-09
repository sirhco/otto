//! Error type for the LLM client seam.
//!
//! Port of the error surface referenced across opencode
//! `packages/llm/src/route/client.ts` and `packages/llm/src/route/auth.ts`.
//! The variants stay faithful to the error *kinds* those modules raise while
//! remaining pragmatic for Rust.

use thiserror::Error;

/// Errors produced anywhere in the compile → prepare → stream → generate
/// pipeline.
///
/// Port of the error kinds referenced in opencode `route/client.ts` and
/// `route/auth.ts`.
#[derive(Debug, Error)]
pub enum LLMError {
    /// Authentication could not be satisfied.
    ///
    /// Mirrors the `AuthenticationReason` raised by `route/auth.ts` when a
    /// required secret is missing (`kind = "missing"`) or otherwise invalid.
    #[error("authentication failed ({kind})")]
    Authentication {
        /// Machine-readable reason, e.g. `"missing"`.
        kind: String,
    },

    /// A streamed protocol event could not be decoded from its wire frame.
    ///
    /// Mirrors `decode_event` failures in `route/client.ts`.
    #[error("failed to decode event: {0}")]
    EventDecode(String),

    /// The request body could not be built or is invalid.
    ///
    /// Mirrors `body.from` build/validation failures in `route/protocol.ts` /
    /// `route/client.ts`.
    #[error("failed to build request body: {0}")]
    Body(String),

    /// The request failed validation before being sent.
    #[error("request validation error: {0}")]
    Validation(String),

    /// A transport-level failure (connection, TLS, I/O).
    #[error("transport error: {0}")]
    Transport(String),

    /// The provider returned a non-success HTTP status.
    #[error("http error: status {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Response body (best effort).
        message: String,
        /// Parsed `Retry-After` header (integer seconds), if the provider sent one.
        retry_after: Option<std::time::Duration>,
    },

    /// A failure while consuming the SSE frame stream.
    #[error("stream error: {0}")]
    Stream(String),

    /// The provider stream ended without emitting a terminal `finish` event.
    ///
    /// Mirrors the guard in `route/client.ts` `generate` (lines ~382-391).
    #[error("provider stream ended without terminal finish")]
    NoTerminalFinish,

    /// The provider stream completed without producing a single recognized
    /// event — no content, no finish, no error. Typical of OpenAI-compatible
    /// gateways emitting frames in a shape the protocol decoder ignores.
    /// Retryable: distinct from [`LLMError::NoTerminalFinish`] (which implies
    /// content was seen) so the two show up separately in logs and metrics.
    #[error("provider stream produced no recognized events")]
    EmptyStream,

    /// A mid-stream provider error the provider (or its rate-limit signal)
    /// marked as transient — surfaced from a `provider-error` event and always
    /// retried, unlike a plain [`LLMError`] with no retry signal.
    #[error("provider error (retryable): {0}")]
    ProviderRetryable(String),
}

impl LLMError {
    /// Convenience constructor for [`LLMError::Authentication`] with
    /// `kind = "missing"`.
    #[must_use]
    pub fn auth_missing() -> Self {
        LLMError::Authentication {
            kind: "missing".to_string(),
        }
    }

    /// The provider-dictated retry delay from a `Retry-After` header, if any.
    /// Returns `None` for every non-`Http` variant.
    #[must_use]
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            LLMError::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_returns_parsed_value_on_http_variant() {
        let err = LLMError::Http {
            status: 429,
            message: String::new(),
            retry_after: Some(std::time::Duration::from_secs(7)),
        };
        assert_eq!(err.retry_after(), Some(std::time::Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_returns_none_for_non_http_variant() {
        let err = LLMError::auth_missing();
        assert_eq!(err.retry_after(), None);
    }
}
