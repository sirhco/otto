//! The universal LLM streaming event union.
//!
//! Port of the `LLMEvent` tagged union from opencode
//! `packages/llm/src/schema/events.ts` (lines ~78-295), together with the
//! supporting literal unions it references from `ids.ts`, `errors.ts`, and
//! `messages.ts`.

use serde::{Deserialize, Serialize};

use crate::usage::{ProviderMetadata, Usage};

/// Arbitrary JSON payload (`Schema.Unknown` / `Schema.Defect` in opencode).
pub type Json = serde_json::Value;

/// Reason a generation step or the whole response finished.
///
/// Port of `FinishReason` from opencode `ids.ts`:
/// `["stop", "length", "tool-calls", "content-filter", "error", "unknown"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FinishReason {
    /// Natural stop.
    Stop,
    /// Hit the max output-length limit.
    Length,
    /// Stopped to emit tool calls.
    ToolCalls,
    /// Blocked by a content filter.
    ContentFilter,
    /// Provider error terminated the stream.
    Error,
    /// Reason not reported by the provider.
    Unknown,
}

/// Classification of a provider failure.
///
/// Port of `ProviderFailureClassification` from opencode `errors.ts`:
/// `Schema.Literal("context-overflow")`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderFailureClassification {
    /// The request exceeded the model's context window.
    ContextOverflow,
}

/// A tool result value.
///
/// Port of `ToolResultValue` from opencode `messages.ts`: a union tagged by
/// `type` where `json` / `text` / `error` carry an arbitrary `value` and
/// `content` carries an array of tool-content blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolResultValue {
    /// Structured JSON result.
    Json {
        /// The result payload.
        value: Json,
    },
    /// Plain-text result.
    Text {
        /// The result payload.
        value: Json,
    },
    /// Error result the model can self-correct from.
    Error {
        /// The result payload.
        value: Json,
    },
    /// Rich tool-content blocks.
    Content {
        /// The tool-content blocks (`ToolContent[]` in opencode).
        value: Vec<Json>,
    },
}

/// Normalized tool output.
///
/// Port of `ToolOutput` from opencode `messages.ts`:
/// `{ structured: unknown, content: ToolContent[] }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Structured (JSON) portion of the output.
    pub structured: Json,
    /// Rich content blocks accompanying the output.
    pub content: Vec<Json>,
}

/// A single provider-neutral streaming event.
///
/// Port of the `LLMEvent` tagged union from opencode `events.ts`. The JSON
/// discriminator is the `type` field with kebab-case variant tags (e.g.
/// `"tool-input-delta"`), exactly matching the strings opencode emits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LLMEvent {
    /// `step-start` — a new generation step began. Port of `StepStart`.
    StepStart {
        /// Zero-based index of the step.
        index: u64,
    },

    /// `text-start` — a text content block opened. Port of `TextStart`.
    TextStart {
        /// Content-block id (`ContentBlockID`).
        id: String,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `text-delta` — incremental text for an open block. Port of `TextDelta`.
    TextDelta {
        /// Content-block id (`ContentBlockID`).
        id: String,
        /// The text fragment.
        text: String,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `text-end` — a text content block closed. Port of `TextEnd`.
    TextEnd {
        /// Content-block id (`ContentBlockID`).
        id: String,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `reasoning-start` — a reasoning block opened. Port of `ReasoningStart`.
    ReasoningStart {
        /// Content-block id (`ContentBlockID`).
        id: String,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `reasoning-delta` — incremental reasoning text. Port of `ReasoningDelta`.
    ReasoningDelta {
        /// Content-block id (`ContentBlockID`).
        id: String,
        /// The reasoning text fragment.
        text: String,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `reasoning-end` — a reasoning block closed. Port of `ReasoningEnd`.
    ReasoningEnd {
        /// Content-block id (`ContentBlockID`).
        id: String,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `tool-input-start` — tool-call input began streaming. Port of
    /// `ToolInputStart`.
    ToolInputStart {
        /// Tool-call id (`ToolCallID`).
        id: String,
        /// Tool name.
        name: String,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `tool-input-delta` — incremental tool-call input. Port of
    /// `ToolInputDelta`. Note: opencode does *not* carry `providerMetadata`
    /// on this variant.
    ToolInputDelta {
        /// Tool-call id (`ToolCallID`).
        id: String,
        /// Tool name.
        name: String,
        /// The input fragment (typically partial JSON text).
        text: String,
    },

    /// `tool-input-end` — tool-call input finished streaming. Port of
    /// `ToolInputEnd`.
    ToolInputEnd {
        /// Tool-call id (`ToolCallID`).
        id: String,
        /// Tool name.
        name: String,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `tool-call` — a complete tool call. Port of `ToolCall`.
    ToolCall {
        /// Tool-call id (`ToolCallID`).
        id: String,
        /// Tool name.
        name: String,
        /// Parsed tool input (`Schema.Unknown`).
        input: Json,
        /// Whether the provider executed the tool itself.
        #[serde(
            rename = "providerExecuted",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_executed: Option<bool>,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `tool-result` — result of a tool call. Port of `ToolResult`.
    ToolResult {
        /// Tool-call id (`ToolCallID`).
        id: String,
        /// Tool name.
        name: String,
        /// The tool result value.
        result: ToolResultValue,
        /// Optional normalized output.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<ToolOutput>,
        /// Whether the provider executed the tool itself.
        #[serde(
            rename = "providerExecuted",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_executed: Option<bool>,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `tool-error` — a tool call failed. Port of `ToolError`.
    ToolError {
        /// Tool-call id (`ToolCallID`).
        id: String,
        /// Tool name.
        name: String,
        /// Human-readable failure message.
        message: String,
        /// Optional structured error payload (`Schema.Defect`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<Json>,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `step-finish` — a generation step finished. Port of `StepFinish`.
    StepFinish {
        /// Zero-based index of the step.
        index: u64,
        /// Why the step finished.
        reason: FinishReason,
        /// Token usage for the step.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `finish` — the whole response finished. Port of `Finish`.
    Finish {
        /// Why the response finished.
        reason: FinishReason,
        /// Aggregate token usage.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `provider-error` — the provider reported an error. Port of
    /// `ProviderErrorEvent`.
    ProviderError {
        /// Human-readable error message.
        message: String,
        /// Optional failure classification.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        classification: Option<ProviderFailureClassification>,
        /// Whether the failure is retryable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retryable: Option<bool>,
        /// Provider metadata escape hatch.
        #[serde(
            rename = "providerMetadata",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_metadata: Option<ProviderMetadata>,
    },

    /// `retry` — the run loop is backing off before retrying a retryable failure.
    /// Purely informational — the turn has not failed yet.
    Retry {
        /// 1-based attempt number that just failed.
        attempt: u32,
        /// Configured max attempts (`RunConfig.max_retries`).
        max: u32,
        /// Backoff wait before the next attempt, in milliseconds.
        delay_ms: u64,
        /// The failing error's message (the TUI classifies rate-limit from it).
        message: String,
        /// `true` when the failed attempt's completed tool work was kept and
        /// the retry continues from it as a new step (salvage), instead of
        /// purging and re-streaming the attempt from scratch. A UI must NOT
        /// roll its transcript back for a salvaged retry.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        salvaged: bool,
    },

    /// `warning` — the run continued despite a quality concern the user should
    /// see (e.g. a provider that never sends `finish_reason`, whose response
    /// was accepted as-is after the retry budget ran out). Informational; the
    /// turn did not fail.
    Warning {
        /// Human-readable description of the concern.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_event_roundtrips_as_retry_tag() {
        let ev = LLMEvent::Retry {
            attempt: 2,
            max: 5,
            delay_ms: 8000,
            salvaged: false,
            message: "http error: status 429: rate limit".to_string(),
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "retry", "serde tag is kebab 'retry'");
        assert_eq!(json["attempt"], 2);
        assert_eq!(json["max"], 5);
        // Deserialize back and confirm the variant round-trips.
        let back: LLMEvent = serde_json::from_value(json).unwrap();
        assert!(
            matches!(
                back,
                LLMEvent::Retry {
                    attempt: 2,
                    max: 5,
                    delay_ms: 8000,
            salvaged: false,
                    ..
                }
            ),
            "round-trips back to Retry"
        );
    }
}
