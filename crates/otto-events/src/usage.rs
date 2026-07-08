//! Token usage accounting.
//!
//! Port of the `Usage` class from opencode
//! `packages/llm/src/schema/events.ts` (lines ~51-74).

use serde::{Deserialize, Serialize};

/// Provider-supplied metadata escape hatch.
///
/// Mirrors `ProviderMetadata` from opencode's schema (`@opencode-ai/schema/llm`),
/// which is a nested record keyed by provider name. Represented here as an
/// arbitrary JSON value so that un-normalized provider fields survive round
/// trips untouched.
pub type ProviderMetadata = serde_json::Value;

/// Token usage reported by an LLM provider.
///
/// Port of the `Usage` class in opencode `events.ts`. Two views of the same
/// data are kept simultaneously:
///
/// **Inclusive totals** (AI SDK / OpenAI / LangChain convention):
/// - [`Usage::input_tokens`] — total prompt tokens, *including* cached
///   reads/writes.
/// - [`Usage::output_tokens`] — total output tokens, *including* reasoning.
/// - [`Usage::total_tokens`] — provider total, or `input + output`.
///
/// **Non-overlapping breakdown** (each field independently meaningful; no
/// consumer ever has to subtract):
/// - [`Usage::non_cached_input_tokens`] — the "fresh" portion of the prompt.
/// - [`Usage::cache_read_input_tokens`] — input tokens served from cache.
/// - [`Usage::cache_write_input_tokens`] — input tokens written to cache.
/// - [`Usage::reasoning_tokens`] — subset of `output_tokens` spent on hidden
///   reasoning.
///
/// **Invariant** (see [`Usage::invariant_holds`]):
/// `non_cached + cache_read + cache_write == input_tokens`, and
/// `reasoning_tokens <= output_tokens`.
///
/// JSON field names are camelCase to match the wire format opencode emits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Total prompt tokens, *including* cached reads/writes. `inputTokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Total output tokens, *including* reasoning. `outputTokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// The "fresh" (non-cached) portion of the prompt. `nonCachedInputTokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_cached_input_tokens: Option<u64>,
    /// Input tokens served from cache. `cacheReadInputTokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    /// Input tokens written to cache. `cacheWriteInputTokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u64>,
    /// Subset of `output_tokens` spent on hidden reasoning. `reasoningTokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Provider-supplied total, or `input + output`. `totalTokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Raw provider usage payload, keyed by provider name. `providerMetadata`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
}

impl Usage {
    /// Visible output tokens — `output_tokens` minus `reasoning_tokens`,
    /// clamped to zero.
    ///
    /// Port of the `visibleOutputTokens` getter in opencode `events.ts`. The
    /// saturating subtraction means a provider reporting
    /// `reasoning_tokens > output_tokens` produces a harmless zero rather than
    /// underflowing.
    #[must_use]
    pub fn visible_output_tokens(&self) -> u64 {
        self.output_tokens
            .unwrap_or(0)
            .saturating_sub(self.reasoning_tokens.unwrap_or(0))
    }

    /// Whether the documented inclusive-vs-breakdown invariant holds.
    ///
    /// Encodes the invariant documented on the `Usage` class in opencode
    /// `events.ts`:
    /// `non_cached + cache_read + cache_write == input_tokens` (checked only
    /// when all four fields are present) and
    /// `reasoning_tokens <= output_tokens`.
    #[must_use]
    pub fn invariant_holds(&self) -> bool {
        let input_ok = match (
            self.non_cached_input_tokens,
            self.cache_read_input_tokens,
            self.cache_write_input_tokens,
            self.input_tokens,
        ) {
            (Some(n), Some(r), Some(w), Some(total)) => n + r + w == total,
            _ => true,
        };
        let reasoning_ok = match (self.reasoning_tokens, self.output_tokens) {
            (Some(reasoning), Some(output)) => reasoning <= output,
            // reasoning reported without any output tokens violates the subset rule.
            (Some(_), None) => false,
            _ => true,
        };
        input_ok && reasoning_ok
    }
}
