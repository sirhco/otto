//! Request-shaping types.
//!
//! Port of the request/option types in opencode
//! `packages/llm/src/schema/messages.ts` and the request assembly in
//! `packages/llm/src/route/client.ts`.

use std::collections::BTreeMap;

use otto_events::Json;
use serde::{Deserialize, Serialize};

use crate::message::{Message, SystemPart, ToolChoice, ToolDefinition};
use crate::model::Model;

/// Provider-neutral generation knobs.
///
/// Port of `GenerationOptions` in `messages.ts`. All fields are optional; a
/// protocol maps the ones its provider supports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerationOptions {
    /// Maximum output tokens.
    #[serde(rename = "maxTokens", default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus-sampling probability mass.
    #[serde(rename = "topP", default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Top-k sampling cutoff.
    #[serde(rename = "topK", default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u64>,
    /// Frequency penalty.
    #[serde(
        rename = "frequencyPenalty",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub frequency_penalty: Option<f64>,
    /// Presence penalty.
    #[serde(
        rename = "presencePenalty",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub presence_penalty: Option<f64>,
    /// Deterministic sampling seed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// Stop sequences.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
}

/// Per-request HTTP escape hatches.
///
/// Port of `HttpOptions` in `messages.ts`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HttpOptions {
    /// Extra request headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// Extra query-string parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<BTreeMap<String, String>>,
    /// Extra body fields merged into the built body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Json>,
}

/// The kind of prompt caching a [`CacheHint`] requests.
///
/// Port of the `cache.type` literal in `messages.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheKind {
    /// Short-lived (e.g. Anthropic `ephemeral`) cache.
    Ephemeral,
    /// Long-lived cache.
    Persistent,
}

/// A hint that a content block should be cached.
///
/// Port of the `CacheHint` shape in `messages.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheHint {
    /// The cache kind.
    #[serde(rename = "type")]
    pub kind: CacheKind,
    /// Optional time-to-live in seconds.
    #[serde(
        rename = "ttlSeconds",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub ttl_seconds: Option<u64>,
}

/// Which messages an automatic cache policy applies its breakpoints to.
///
/// Port of the `messages` sub-policy in `messages.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessagesPolicy {
    /// A named strategy.
    Named(MessagesPolicyNamed),
    /// Cache the last `tail` messages.
    Tail {
        /// Number of trailing messages to mark.
        tail: u32,
    },
}

/// Named message cache strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessagesPolicyNamed {
    /// Cache up to the latest user message.
    LatestUserMessage,
    /// Cache up to the latest assistant message.
    LatestAssistant,
}

/// The object form of a [`CachePolicy`].
///
/// Port of the structured `cache` policy object in `messages.ts`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CachePolicyObject {
    /// Whether to cache the tool definitions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    /// Whether to cache the system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<bool>,
    /// Message caching sub-policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<MessagesPolicy>,
    /// Default TTL for created breakpoints.
    #[serde(
        rename = "ttlSeconds",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub ttl_seconds: Option<u64>,
}

/// Named cache presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CachePreset {
    /// Let the client place cache breakpoints automatically.
    Auto,
    /// Disable caching.
    None,
}

/// Top-level caching policy.
///
/// Port of the `cache` union in `messages.ts`: either the string presets
/// `"auto"` / `"none"`, or a structured [`CachePolicyObject`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CachePolicy {
    /// A string preset (`"auto"` / `"none"`).
    Preset(CachePreset),
    /// A structured policy object.
    Object(CachePolicyObject),
}

/// Free-form, provider-scoped options.
///
/// Port of `providerOptions` in `messages.ts`. Kept as a map keyed by provider
/// name so protocols can reach `providerOptions.anthropic.thinking` /
/// `providerOptions.openai.reasoning_effort`.
pub type ProviderOptions = BTreeMap<String, Json>;

/// A fully-specified generation request.
///
/// Port of `LLMRequest` in `messages.ts` / the request assembled by
/// `route/client.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LLMRequest {
    /// Optional caller-supplied request id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The target model.
    pub model: Model,
    /// System-prompt segments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<SystemPart>,
    /// The conversation messages.
    pub messages: Vec<Message>,
    /// Available tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    /// Tool-choice policy.
    #[serde(
        rename = "toolChoice",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_choice: Option<ToolChoice>,
    /// Generation knobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationOptions>,
    /// Provider-scoped options.
    #[serde(
        rename = "providerOptions",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_options: Option<ProviderOptions>,
    /// HTTP escape hatches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpOptions>,
    /// Caching policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CachePolicy>,
    /// Free-form request metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Json>,
}

impl LLMRequest {
    /// Build a minimal request for `model` with `messages` and defaults
    /// elsewhere.
    #[must_use]
    pub fn new(model: Model, messages: Vec<Message>) -> Self {
        LLMRequest {
            id: None,
            model,
            system: Vec::new(),
            messages,
            tools: Vec::new(),
            tool_choice: None,
            generation: None,
            provider_options: None,
            http: None,
            cache: None,
            metadata: None,
        }
    }
}
