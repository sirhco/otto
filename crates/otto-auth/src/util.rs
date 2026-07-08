//! Small internal helpers shared across provider flows.

/// Current Unix time in **milliseconds**, matching JavaScript's `Date.now()`
/// used throughout the opencode auth plugins to compute token `expires`.
#[must_use]
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
