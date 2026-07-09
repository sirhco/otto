//! Retry policy for transient provider failures — a Rust port of opencode
//! `session/retry.ts`.
//!
//! [`retryable`] classifies an [`LLMError`] as worth retrying (rate limits /
//! 429 / 5xx / transient transport failures) versus terminal (auth, invalid
//! request, context overflow). [`delay`] computes the backoff (`retry.ts:35-66`)
//! and [`with_retry`] reruns a fallible future with that backoff
//! (`retry.ts:176-199`, `policy`).

use std::future::Future;
use std::time::Duration;

use otto_llm::LLMError;
use tokio_util::sync::CancellationToken;

/// Base backoff delay (`RETRY_INITIAL_DELAY`, `retry.ts:26`).
pub const RETRY_INITIAL_DELAY_MS: u64 = 2000;
/// Backoff cap when no `Retry-After` header is present
/// (`RETRY_MAX_DELAY_NO_HEADERS`, `retry.ts:28`).
pub const RETRY_MAX_DELAY_MS: u64 = 30_000;
/// Ceiling for a provider-supplied Retry-After, so a hostile/broken header
/// can't stall a turn indefinitely. Kept short (60s) because the whole backoff
/// is a silent pause from the client's perspective — a longer honored header
/// reads as a hang, and a still-limited provider just 429s again on the next
/// attempt with a fresh header.
pub const RETRY_AFTER_MAX_MS: u64 = 60_000;

/// Whether `msg` carries a provider rate-limit signal (port of the plain-text
/// pattern checks, `retry.ts:126-150`). `pub(crate)` so the processor can reuse
/// it when classifying a `provider-error` event with no explicit `retryable`.
pub(crate) fn has_rate_limit_pattern(msg: &str) -> bool {
    let l = msg.to_ascii_lowercase();
    l.contains("rate limit")
        || l.contains("rate increased too quickly")
        || l.contains("too many requests")
        || l.contains("overloaded")
        || l.contains("exhausted")
        || l.contains("unavailable")
}

/// Classify whether `err` is worth retrying — port of `retryable`
/// (`retry.ts:68-152`).
///
/// Retryable: HTTP `429`, any HTTP `5xx` (transient server failures), a
/// transport-level failure (connection/TLS/I/O), any error whose message
/// carries a rate-limit / overloaded signal, a provider-error the provider
/// flagged transient ([`LLMError::ProviderRetryable`]), or a stream that ended
/// without a terminal finish ([`LLMError::NoTerminalFinish`] — a truncated /
/// halted response that a fresh attempt can complete). Non-retryable:
/// authentication, request validation / body build failures, and event-decode
/// failures (all deterministic; retrying cannot help). Context overflow is
/// surfaced as a distinct outcome upstream and never reaches here.
#[must_use]
pub fn retryable(err: &LLMError, _provider: &str) -> bool {
    match err {
        // 5xx are always transient; 429 is a rate limit; otherwise fall back to
        // the text patterns for providers that embed the signal in the body.
        LLMError::Http {
            status, message, ..
        } => *status == 429 || *status >= 500 || has_rate_limit_pattern(message),
        // Connection/TLS/I/O failures are transient.
        LLMError::Transport(_) => true,
        // A mid-stream failure is retryable only when it reads as a rate limit.
        LLMError::Stream(msg) => has_rate_limit_pattern(msg),
        // A provider-flagged transient error always retries.
        LLMError::ProviderRetryable(_) => true,
        // A truncated / halted stream (clean EOF, no terminal finish) is worth
        // a fresh attempt.
        LLMError::NoTerminalFinish => true,
        // A stream that produced zero recognized events (gateway emitted
        // nothing decodable) — retry with backoff rather than hammering the
        // provider with immediate re-requests.
        LLMError::EmptyStream => true,
        // Deterministic failures — retrying cannot change the outcome.
        LLMError::Authentication { .. }
        | LLMError::Validation(_)
        | LLMError::Body(_)
        | LLMError::EventDecode(_) => false,
    }
}

/// The delay before retry attempt `attempt` (0-based) — port of `delay`
/// (`retry.ts:35-66`).
///
/// A `retry_after` duration (from a provider `Retry-After` header) always wins;
/// otherwise the delay is `min(RETRY_INITIAL_DELAY * 2^attempt, 30s)`.
#[must_use]
pub fn delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(after) = retry_after {
        return after.min(Duration::from_millis(RETRY_AFTER_MAX_MS));
    }
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let ms = RETRY_INITIAL_DELAY_MS
        .saturating_mul(factor)
        .min(RETRY_MAX_DELAY_MS);
    Duration::from_millis(ms)
}

/// Run `f`, retrying on a [`retryable`] [`LLMError`] with [`delay`] backoff up
/// to `max_attempts` total tries — the imperative analog of the effect
/// `Schedule` built by `policy` (`retry.ts:176-199`).
///
/// The `abort` token cancels the wait and aborts the retry loop: a cancelled
/// token makes the current error propagate immediately rather than sleeping.
/// A non-retryable error propagates on the first failure.
///
/// # Errors
/// Returns the last [`LLMError`] once retries are exhausted, the error is not
/// retryable, or `abort` is cancelled.
pub async fn with_retry<F, Fut, T>(
    max_attempts: u32,
    provider: &str,
    abort: &CancellationToken,
    mut f: F,
) -> Result<T, LLMError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, LLMError>>,
{
    let mut attempt: u32 = 0;
    loop {
        match f().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                attempt += 1;
                if attempt >= max_attempts || abort.is_cancelled() || !retryable(&err, provider) {
                    return Err(err);
                }
                let wait = delay(attempt, None);
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = abort.cancelled() => return Err(err),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn classification_table() {
        let p = "anthropic";
        assert!(retryable(
            &LLMError::Http {
                status: 429,
                message: String::new(),
                retry_after: None,
            },
            p
        ));
        assert!(retryable(
            &LLMError::Http {
                status: 500,
                message: String::new(),
                retry_after: None,
            },
            p
        ));
        assert!(retryable(
            &LLMError::Http {
                status: 503,
                message: String::new(),
                retry_after: None,
            },
            p
        ));
        assert!(retryable(
            &LLMError::Transport("connection reset".into()),
            p
        ));
        assert!(retryable(
            &LLMError::Http {
                status: 400,
                message: "rate limit exceeded".into(),
                retry_after: None,
            },
            p
        ));
        assert!(retryable(
            &LLMError::Stream("provider is overloaded".into()),
            p
        ));
        assert!(retryable(
            &LLMError::ProviderRetryable("transient".into()),
            p
        ));
        // A truncated / halted stream is retryable (Fix 4).
        assert!(retryable(&LLMError::NoTerminalFinish, p));
        // An empty stream (zero recognized events) is retryable — the
        // alternative is an immediate, backoff-free re-request loop.
        assert!(retryable(&LLMError::EmptyStream, p));

        assert!(!retryable(
            &LLMError::Http {
                status: 400,
                message: "bad request".into(),
                retry_after: None,
            },
            p
        ));
        assert!(!retryable(
            &LLMError::Http {
                status: 401,
                message: String::new(),
                retry_after: None,
            },
            p
        ));
        assert!(!retryable(
            &LLMError::Http {
                status: 404,
                message: String::new(),
                retry_after: None,
            },
            p
        ));
        assert!(!retryable(&LLMError::auth_missing(), p));
        assert!(!retryable(&LLMError::Validation("bad".into()), p));
        assert!(!retryable(&LLMError::Body("bad".into()), p));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(delay(0, None), Duration::from_millis(2000));
        assert_eq!(delay(1, None), Duration::from_millis(4000));
        assert_eq!(delay(2, None), Duration::from_millis(8000));
        assert_eq!(delay(3, None), Duration::from_millis(16_000));
        assert_eq!(delay(4, None), Duration::from_millis(30_000), "capped");
        assert_eq!(
            delay(50, None),
            Duration::from_millis(30_000),
            "still capped"
        );
    }

    #[test]
    fn retry_after_wins() {
        assert_eq!(
            delay(3, Some(Duration::from_millis(1234))),
            Duration::from_millis(1234)
        );
    }

    #[test]
    fn retry_after_is_capped_at_ceiling() {
        // A hostile/broken provider sending Retry-After: 86400 (24h) must not
        // stall a turn indefinitely — capped to RETRY_AFTER_MAX_MS.
        assert_eq!(
            delay(0, Some(Duration::from_secs(86_400))),
            Duration::from_millis(RETRY_AFTER_MAX_MS)
        );
        // The ceiling itself stays short enough that a turn never looks hung:
        // a 2-minute Retry-After clamps to at most 60s.
        assert!(
            delay(0, Some(Duration::from_secs(120))) <= Duration::from_secs(60),
            "Retry-After ceiling must not exceed 60s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn with_retry_reruns_until_success() {
        let calls = AtomicU32::new(0);
        let abort = CancellationToken::new();
        let out: Result<&str, LLMError> = with_retry(5, "anthropic", &abort, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(LLMError::Transport("reset".into()))
                } else {
                    Ok("ok")
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), "ok");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "failed once then succeeded"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn with_retry_non_retryable_returns_immediately() {
        let calls = AtomicU32::new(0);
        let abort = CancellationToken::new();
        let out: Result<&str, LLMError> = with_retry(5, "anthropic", &abort, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async move { Err(LLMError::Validation("nope".into())) }
        })
        .await;
        assert!(matches!(out, Err(LLMError::Validation(_))));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no retry on terminal error"
        );
    }
}
