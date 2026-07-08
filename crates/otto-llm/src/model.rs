//! Model + provider identity and capability metadata.
//!
//! Port of the model/provider shapes in opencode `packages/llm/src/schema`
//! (the models.dev-derived `Model` record). This is a *minimal* carrier: the
//! full models.dev registry is Phase 1 later — providers populate these fields.

use serde::{Deserialize, Serialize};

/// A provider identifier, e.g. `anthropic` or `openai`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl ProviderId {
    /// Wrap a provider id string.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        ProviderId(id.into())
    }
}

/// A model identifier, e.g. `claude-sonnet-4` or `gpt-4o`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

impl ModelId {
    /// Wrap a model id string.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        ModelId(id.into())
    }
}

/// Context/token limits for a model.
///
/// Port of the `limit` block of the models.dev record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLimits {
    /// Total context window in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// Maximum input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    /// Maximum output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
}

/// Boolean/modality capabilities of a model.
///
/// Port of the capability flags on the models.dev record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Supports a `temperature` knob.
    pub temperature: bool,
    /// Supports reasoning / thinking output.
    pub reasoning: bool,
    /// Supports file/image attachments.
    pub attachment: bool,
    /// Supports tool calling.
    #[serde(rename = "toolCall")]
    pub tool_call: bool,
    /// Supports interleaved reasoning + tool calls.
    pub interleaved: bool,
    /// Accepted input modalities (e.g. `text`, `image`).
    #[serde(rename = "inputModalities", default)]
    pub input_modalities: Vec<String>,
    /// Produced output modalities.
    #[serde(rename = "outputModalities", default)]
    pub output_modalities: Vec<String>,
}

/// Per-token cost of a model (USD per million tokens, provider-defined units).
///
/// Port of the `cost` block of the models.dev record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    /// Input token cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    /// Output token cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    /// Cache-read token cost.
    #[serde(rename = "cacheRead", default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cache-write token cost.
    #[serde(
        rename = "cacheWrite",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_write: Option<f64>,
}

/// A model plus the metadata protocols need to build requests.
///
/// Port of the models.dev-derived `Model` record. `route_id` is the id of the
/// [`crate::route::Route`] that serves this model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    /// The model id.
    pub id: ModelId,
    /// The owning provider id.
    pub provider: ProviderId,
    /// The id of the route that serves this model.
    #[serde(rename = "routeId")]
    pub route_id: String,
    /// Context/token limits.
    #[serde(default)]
    pub limits: ModelLimits,
    /// Capability flags.
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    /// Optional cost metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
}

impl Model {
    /// Build a minimal [`Model`] with default limits/capabilities and no cost.
    ///
    /// Providers fill in the richer fields; the full models.dev registry is a
    /// later phase.
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        id: impl Into<String>,
        route_id: impl Into<String>,
    ) -> Self {
        Model {
            id: ModelId::new(id),
            provider: ProviderId::new(provider),
            route_id: route_id.into(),
            limits: ModelLimits::default(),
            capabilities: ModelCapabilities::default(),
            cost: None,
        }
    }
}
