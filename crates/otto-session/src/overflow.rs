//! Context-overflow detection — a Rust port of opencode
//! `session/overflow.ts`.
//!
//! The auto-compaction pre-check (`session/prompt.ts:1160-1168`) asks whether
//! the last finished assistant's recorded token count has caught up to the
//! usable slice of the model's context window. When it has, the history is
//! summarized before the next turn is generated.

use otto_llm::Model;
use otto_storage::model::Tokens;

/// The context-occupying token count for a set of [`Tokens`] — the input plus
/// both cache halves (`overflow.ts:31-32`, restricted to the tokens that
/// actually consume prompt context: prompt/input + cache read + cache write).
#[must_use]
pub fn context_token_count(tokens: &Tokens) -> f64 {
    tokens.input + tokens.cache.read + tokens.cache.write
}

/// The usable slice of the context window: `context − max_output − reserved`
/// (port of `usable`, `overflow.ts:10-20`).
///
/// Returns `0` when the model declares no context window (models with
/// `context == 0`/unset never overflow). `max_output` and `reserved` are
/// clamped so the result never underflows.
#[must_use]
pub fn usable(model: &Model, reserved: u64) -> u64 {
    let Some(context) = model.limits.context else {
        return 0;
    };
    if context == 0 {
        return 0;
    }
    let max_output = model.limits.output.unwrap_or(0);
    context.saturating_sub(max_output).saturating_sub(reserved)
}

/// Whether `tokens` has reached the usable context slice — port of `isOverflow`
/// (`overflow.ts:22-34`).
///
/// `usable = context − max_output − reserved`; overflow is reported when
/// `count(tokens) >= usable`, where `count` is [`context_token_count`]. A model
/// with no/zero context window never overflows.
#[must_use]
pub fn is_overflow(tokens: &Tokens, model: &Model, reserved: u64) -> bool {
    let Some(context) = model.limits.context else {
        return false;
    };
    if context == 0 {
        return false;
    }
    context_token_count(tokens) >= usable(model, reserved) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_llm::model::ModelLimits;
    use otto_storage::model::TokenCache;

    fn model(context: Option<u64>, output: Option<u64>) -> Model {
        let mut m = Model::new("anthropic", "claude-3", "route");
        m.limits = ModelLimits {
            context,
            input: None,
            output,
        };
        m
    }

    fn tokens(input: f64, read: f64, write: f64) -> Tokens {
        Tokens {
            total: None,
            input,
            output: 999.0, // output is intentionally excluded from the count
            reasoning: 0.0,
            cache: TokenCache { read, write },
        }
    }

    #[test]
    fn usable_is_context_minus_output_minus_reserved() {
        let m = model(Some(1000), Some(100));
        assert_eq!(usable(&m, 0), 900);
        assert_eq!(usable(&m, 50), 850);
        // clamps to zero rather than underflowing
        assert_eq!(usable(&m, 5000), 0);
    }

    #[test]
    fn overflow_around_the_usable_boundary() {
        // usable = 1000 - 100 - 0 = 900.
        let m = model(Some(1000), Some(100));
        assert!(!is_overflow(&tokens(899.0, 0.0, 0.0), &m, 0), "just under");
        assert!(is_overflow(&tokens(900.0, 0.0, 0.0), &m, 0), "at boundary");
        assert!(is_overflow(&tokens(901.0, 0.0, 0.0), &m, 0), "just over");
        // cache read/write count toward context; output does not.
        assert!(
            is_overflow(&tokens(500.0, 300.0, 200.0), &m, 0),
            "500+300+200=1000"
        );
        assert!(
            !is_overflow(&tokens(500.0, 100.0, 100.0), &m, 0),
            "500+100+100=700"
        );
    }

    #[test]
    fn reserved_narrows_the_boundary() {
        // usable = 1000 - 100 - 200 = 700.
        let m = model(Some(1000), Some(100));
        assert!(!is_overflow(&tokens(699.0, 0.0, 0.0), &m, 200));
        assert!(is_overflow(&tokens(700.0, 0.0, 0.0), &m, 200));
    }

    #[test]
    fn no_context_never_overflows() {
        assert!(!is_overflow(
            &tokens(1_000_000.0, 0.0, 0.0),
            &model(None, None),
            0
        ));
        assert!(!is_overflow(
            &tokens(1_000_000.0, 0.0, 0.0),
            &model(Some(0), None),
            0
        ));
    }
}
