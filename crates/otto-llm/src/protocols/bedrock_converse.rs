//! The Amazon Bedrock Converse wire protocol.
//!
//! Faithful (scoped) port of opencode
//! `packages/llm/src/protocols/bedrock-converse.ts`. This module covers
//! request-body construction ([`BedrockConverse::build_body`], porting
//! `fromRequest` / `lowerMessages`, bedrock-converse.ts:302-379), the body
//! schema (bedrock-converse.ts:36-143), and the streaming-event reducer
//! ([`BedrockConverse::step`] / [`BedrockConverse::on_halt`], porting `step` /
//! `onHalt`, bedrock-converse.ts:469-629).
//!
//! **Wire framing.** The `BedrockTransport` (a separate task) emits each
//! decoded AWS event-stream chunk as a single-key wrapper JSON string, e.g.
//! `{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"hi"}}}` —
//! reconstructing the `:event-type` header the raw AWS framing carries
//! out-of-band (bedrock-converse.ts:154-157). [`BedrockEvent`] is a plain
//! externally-tagged enum (`#[serde(rename_all = "camelCase")]`) so each
//! variant's own tag matches that wrapper key directly — no `Schema.Struct`
//! translation layer is needed, unlike the TS source's `BedrockEvent`
//! (bedrock-converse.ts:158-206), which encodes the same "exactly one of
//! these keys is present" shape as a struct of all-optional fields.
//!
//! **Split finish.** Bedrock reports the terminal `stop_reason` on
//! `messageStop` and usage on a separate `metadata` chunk, in either order.
//! [`ParserState::pending_finish`] holds whichever has arrived so far;
//! [`BedrockConverse::on_halt`] (not `step`) emits the combined
//! `step-finish`/`finish` once the frame stream ends, mirroring `onHalt`
//! (bedrock-converse.ts:618-629) and the Gemini reducer's `onHalt`-emits-
//! finish shape. Unlike the TS source (which emits nothing at halt if
//! `pendingFinish` never got set), [`BedrockConverse::on_halt`] still emits an
//! `unknown`-reason finish if the lifecycle ever opened a step but the stream
//! ended without a `messageStop`/`metadata` chunk — matching the defensive
//! posture of [`crate::protocols::gemini::Gemini::on_halt`]'s content-less-
//! finish handling, so a truncated stream never leaves a step open forever.
//!
//! Two simplifications versus the TypeScript source, both flagged inline
//! where they bite:
//! - No media (`image`/`document`) content blocks and no `cachePoint`
//!   breakpoints — [`BedrockContentBlock`] only carries the four variants the
//!   task brief specifies (`text`, `toolUse`, `toolResult`,
//!   `reasoningContent`). Media/cache support is a TODO for a later task,
//!   same posture as `anthropic_messages`'s deferred `server_tool_result`
//!   round-trip.
//! - `BedrockMessage`'s `content` is a single [`BedrockContentBlock`] enum
//!   shared by both roles (rather than the TS source's separate
//!   `BedrockUserBlock` / `BedrockAssistantBlock` unions), since Rust doesn't
//!   need the extra split to keep each role's content well-typed here.
//!
//! Line references throughout point at the TypeScript source of truth.

use std::collections::{HashMap, HashSet};

use otto_events::{FinishReason, Json, LLMEvent, ProviderFailureClassification, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::LLMError;
use crate::message::{ContentPart, Role, ToolChoice, ToolDefinition};
use crate::protocol::Protocol;
use crate::protocols::utils::{lifecycle, tool_stream};
use crate::request::LLMRequest;

/// Protocol id (`ADAPTER` in bedrock-converse.ts:29).
const ADAPTER: &str = "bedrock-converse";

// =============================================================================
// Request Body Schema (bedrock-converse.ts:36-143)
// =============================================================================

/// `{ text }` — a plain text block, shared by content and system blocks
/// (bedrock-converse.ts:36-39).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BedrockTextBlock {
    /// The text content.
    pub text: String,
}

/// A top-level `system[]` entry (`BedrockSystemBlock`, bedrock-converse.ts:99).
/// Distinct type from [`BedrockTextBlock`] only to match the brief's naming;
/// same shape.
pub type BedrockSystemBlock = BedrockTextBlock;

/// `{ toolUseId, name, input }` — the inner payload of a `toolUse` block
/// (bedrock-converse.ts:41-47).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BedrockToolUse {
    /// Tool-call id.
    #[serde(rename = "toolUseId")]
    pub tool_use_id: String,
    /// Tool name.
    pub name: String,
    /// Parsed tool input.
    pub input: Json,
}

/// One item of a `toolResult` block's `content` array. The TS source also
/// accepts an image block here (bedrock-converse.ts:50-54); that variant is
/// not ported (see module docs).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum BedrockToolResultContentItem {
    /// `{ text }` (bedrock-converse.ts:51).
    Text {
        /// The text content.
        text: String,
    },
    /// `{ json }` (bedrock-converse.ts:52).
    Json {
        /// The structured payload.
        json: Json,
    },
}

/// `{ toolUseId, content, status? }` — the inner payload of a `toolResult`
/// block (bedrock-converse.ts:56-63).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BedrockToolResult {
    /// The originating `toolUse` id.
    #[serde(rename = "toolUseId")]
    pub tool_use_id: String,
    /// The result content items.
    pub content: Vec<BedrockToolResultContentItem>,
    /// `"success"` or `"error"`.
    pub status: &'static str,
}

/// `{ text, signature? }` — the inner `reasoningText` of a reasoning block
/// (bedrock-converse.ts:65-74).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BedrockReasoningText {
    /// The reasoning text.
    pub text: String,
    /// Provider signature for the reasoning block, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// `{ reasoningText }` — the inner payload of a `reasoningContent` block
/// (bedrock-converse.ts:65-74).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BedrockReasoningContent {
    /// The reasoning text + signature.
    #[serde(rename = "reasoningText")]
    pub reasoning_text: BedrockReasoningText,
}

/// A content block on a [`BedrockMessage`]. Union of the block schemas in
/// bedrock-converse.ts:36-91, distinguished by which key is present (untagged
/// — Bedrock's dialect has no `type` discriminator). Shared by both `user`
/// and `assistant` roles (see module docs); the lowering functions only ever
/// construct the variant valid for a given role.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum BedrockContentBlock {
    /// `{ text }` (bedrock-converse.ts:36-39).
    Text {
        /// The text content.
        text: String,
    },
    /// `{ toolUse: { toolUseId, name, input } }` (bedrock-converse.ts:41-48,
    /// `BedrockToolUseBlock`).
    ToolUse {
        /// The tool-call payload.
        #[serde(rename = "toolUse")]
        tool_use: BedrockToolUse,
    },
    /// `{ toolResult: { toolUseId, content, status? } }`
    /// (bedrock-converse.ts:56-63, `BedrockToolResultBlock`).
    ToolResult {
        /// The tool-result payload.
        #[serde(rename = "toolResult")]
        tool_result: BedrockToolResult,
    },
    /// `{ reasoningContent: { reasoningText: { text, signature? } } }`
    /// (bedrock-converse.ts:65-74, `BedrockReasoningBlock`).
    ReasoningContent {
        /// The reasoning payload.
        #[serde(rename = "reasoningContent")]
        reasoning_content: BedrockReasoningContent,
    },
}

/// A message in the Converse request, tagged by `role`
/// (bedrock-converse.ts:93-97, `BedrockMessage`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum BedrockMessage {
    /// A user turn.
    User {
        /// The ordered content blocks.
        content: Vec<BedrockContentBlock>,
    },
    /// An assistant turn.
    Assistant {
        /// The ordered content blocks.
        content: Vec<BedrockContentBlock>,
    },
}

/// `{}` — an empty JSON object, used by the unit variants of
/// [`BedrockToolChoice`] (bedrock-converse.ts:117-118).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct BedrockEmpty {}

/// `{ name }` — the inner payload of a named `tool` choice
/// (bedrock-converse.ts:119).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BedrockToolChoiceName {
    /// The required tool name.
    pub name: String,
}

/// The `toolConfig.toolChoice` field (bedrock-converse.ts:116-120).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum BedrockToolChoice {
    /// The model decides (`{ auto: {} }`).
    Auto {
        /// Always `{}`.
        auto: BedrockEmpty,
    },
    /// The model must call some tool (`{ any: {} }`, `required` → `any`).
    Any {
        /// Always `{}`.
        any: BedrockEmpty,
    },
    /// The model must call the named tool (`{ tool: { name } }`).
    Tool {
        /// The required tool payload.
        tool: BedrockToolChoiceName,
    },
}

/// `{ json }` — the inner payload of `toolSpec.inputSchema`
/// (bedrock-converse.ts:106-109).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BedrockInputSchema {
    /// The JSON Schema document.
    pub json: Json,
}

/// `{ name, description, inputSchema }` — the inner payload of a `toolSpec`
/// block (bedrock-converse.ts:102-110).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BedrockToolSpecInner {
    /// Tool name.
    pub name: String,
    /// Human-readable description (required by Converse; empty if absent).
    pub description: String,
    /// The tool's input schema.
    #[serde(rename = "inputSchema")]
    pub input_schema: BedrockInputSchema,
}

/// One `toolConfig.tools[]` entry (`BedrockToolSpec`, bedrock-converse.ts:102-114).
/// The TS source also allows a positional `cachePoint` block here
/// (bedrock-converse.ts:113); not ported (see module docs).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BedrockToolSpec {
    /// The tool specification.
    #[serde(rename = "toolSpec")]
    pub tool_spec: BedrockToolSpecInner,
}

/// `toolConfig` (bedrock-converse.ts:134-139).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BedrockToolConfig {
    /// The declared tools.
    pub tools: Vec<BedrockToolSpec>,
    /// The tool-calling policy.
    #[serde(rename = "toolChoice", skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<BedrockToolChoice>,
}

/// `inferenceConfig` (bedrock-converse.ts:126-133). Converse's base schema has
/// no `topK` field — it goes through
/// [`BedrockConverseBody::additional_model_request_fields`] instead
/// (bedrock-converse.ts:139-142, 422-424).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BedrockInferenceConfig {
    /// Maximum output tokens.
    #[serde(rename = "maxTokens", skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus-sampling probability mass.
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Stop sequences.
    #[serde(rename = "stopSequences", skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
}

impl BedrockInferenceConfig {
    /// Whether every field is `None` (bedrock-converse.ts:409-414, negated).
    fn is_empty(&self) -> bool {
        self.max_tokens.is_none()
            && self.temperature.is_none()
            && self.top_p.is_none()
            && self.stop_sequences.is_none()
    }
}

/// The Bedrock Converse request body (`BedrockConverseBody`,
/// bedrock-converse.ts:122-143).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BedrockConverseBody {
    /// The model id.
    #[serde(rename = "modelId")]
    pub model_id: String,
    /// The lowered conversation messages.
    pub messages: Vec<BedrockMessage>,
    /// The top-level system prompt blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<BedrockSystemBlock>>,
    /// Sampling / generation knobs.
    #[serde(rename = "inferenceConfig", skip_serializing_if = "Option::is_none")]
    pub inference_config: Option<BedrockInferenceConfig>,
    /// The available tools and tool-calling policy.
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<BedrockToolConfig>,
    /// Model-specific fields with no native Converse field (e.g. `top_k`).
    #[serde(
        rename = "additionalModelRequestFields",
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_model_request_fields: Option<Value>,
}

// =============================================================================
// Request Lowering (bedrock-converse.ts:211-426)
// =============================================================================

/// Lower a tool definition. Port of `lowerToolSpec` (bedrock-converse.ts:211-217),
/// without the `ToolSchemaProjection.modelCompatibility` step (not yet ported;
/// `tool.input_schema` is passed through verbatim, matching
/// `anthropic_messages`'s treatment).
fn lower_tool_spec(tool: &ToolDefinition) -> BedrockToolSpec {
    BedrockToolSpec {
        tool_spec: BedrockToolSpecInner {
            name: tool.name.clone(),
            description: tool.description.clone().unwrap_or_default(),
            input_schema: BedrockInputSchema {
                json: tool.input_schema.clone(),
            },
        },
    }
}

/// Lower the tool-choice policy. Port of `lowerToolChoice`
/// (bedrock-converse.ts:242-248). `none` returns `None` (and the caller omits
/// `toolConfig` entirely).
fn lower_tool_choice(choice: &ToolChoice) -> Option<BedrockToolChoice> {
    match choice {
        ToolChoice::Auto => Some(BedrockToolChoice::Auto {
            auto: BedrockEmpty::default(),
        }),
        ToolChoice::None => None,
        ToolChoice::Required => Some(BedrockToolChoice::Any {
            any: BedrockEmpty::default(),
        }),
        ToolChoice::Tool { name } => Some(BedrockToolChoice::Tool {
            tool: BedrockToolChoiceName { name: name.clone() },
        }),
    }
}

/// Stringify a JSON value the way `String(value)` / `encodeJson(value)` would
/// (shared.ts:213-223). Strings are returned verbatim; everything else is
/// JSON-encoded. Reused verbatim from `anthropic_messages::loose_string` /
/// `gemini::loose_string`.
fn loose_string(value: &Json) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Port of `ProviderShared.toolResultText` (shared.ts:213-223).
fn tool_result_text(result: &otto_events::ToolResultValue) -> String {
    use otto_events::ToolResultValue as R;
    match result {
        R::Text { value } | R::Error { value } => loose_string(value),
        R::Json { value } => serde_json::to_string(value).unwrap_or_default(),
        R::Content { .. } => String::new(),
    }
}

/// Lower a tool-result's content into `toolResult.content[]`. Port of
/// `lowerToolResultContent` (bedrock-converse.ts:268-290), without the
/// structured-content image branch (`BedrockMedia.lower`; not yet ported —
/// non-text items in a `content`-typed result error out, matching the
/// module's media-support TODO).
fn lower_tool_result_content(
    result: &otto_events::ToolResultValue,
) -> Result<Vec<BedrockToolResultContentItem>, LLMError> {
    use otto_events::ToolResultValue as R;
    match result {
        R::Text { .. } | R::Error { .. } => Ok(vec![BedrockToolResultContentItem::Text {
            text: tool_result_text(result),
        }]),
        R::Json { value } => Ok(vec![BedrockToolResultContentItem::Json {
            json: value.clone(),
        }]),
        R::Content { value } => {
            let mut content = Vec::with_capacity(value.len());
            for item in value {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    let text = item
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    content.push(BedrockToolResultContentItem::Text { text });
                } else {
                    return Err(LLMError::Validation(
                        "Bedrock Converse only supports text content in tool results (media \
                         not yet ported)"
                            .to_string(),
                    ));
                }
            }
            Ok(content)
        }
    }
}

/// XML-escape system-update text so it cannot close the wrapper
/// (shared.ts:111-112), reused verbatim from the sibling protocols.
fn escape_system_update(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Wrap chronological system text into visible lower-authority user text.
/// Port of `wrapSystemUpdate` (shared.ts:120-121).
fn wrap_system_update(joined: &str) -> String {
    format!(
        "<system-update>\n{}\n</system-update>",
        escape_system_update(joined)
    )
}

/// Lower all messages. Port of `lowerMessages` (bedrock-converse.ts:302-379),
/// without cache-point emission (`BedrockCache.block`; not yet ported — see
/// module docs) or media content (`BedrockMedia.lower`; likewise deferred).
fn lower_messages(req: &LLMRequest) -> Result<Vec<BedrockMessage>, LLMError> {
    let mut messages: Vec<BedrockMessage> = Vec::new();

    for message in &req.messages {
        match message.role {
            Role::System => {
                // Collect text-only content, then wrap it as visible user
                // text, appending to a trailing user turn if present.
                let mut joined = String::new();
                for part in &message.content {
                    match part {
                        ContentPart::Text { text, .. } => {
                            if !joined.is_empty() {
                                joined.push('\n');
                            }
                            joined.push_str(text);
                        }
                        _ => {
                            return Err(LLMError::Validation(
                                "Bedrock Converse system messages only support text content"
                                    .to_string(),
                            ));
                        }
                    }
                }
                let block = BedrockContentBlock::Text {
                    text: wrap_system_update(&joined),
                };
                match messages.last_mut() {
                    Some(BedrockMessage::User { content }) => content.push(block),
                    _ => messages.push(BedrockMessage::User {
                        content: vec![block],
                    }),
                }
            }
            Role::User => {
                let mut content = Vec::with_capacity(message.content.len());
                for part in &message.content {
                    match part {
                        ContentPart::Text { text, .. } => {
                            content.push(BedrockContentBlock::Text { text: text.clone() });
                        }
                        _ => {
                            return Err(LLMError::Validation(
                                "Bedrock Converse user messages only support text content \
                                 (media not yet ported)"
                                    .to_string(),
                            ));
                        }
                    }
                }
                messages.push(BedrockMessage::User { content });
            }
            Role::Assistant => {
                let mut content = Vec::with_capacity(message.content.len());
                for part in &message.content {
                    match part {
                        ContentPart::Text { text, .. } => {
                            content.push(BedrockContentBlock::Text { text: text.clone() });
                        }
                        ContentPart::Reasoning { text, encrypted } => {
                            content.push(BedrockContentBlock::ReasoningContent {
                                reasoning_content: BedrockReasoningContent {
                                    reasoning_text: BedrockReasoningText {
                                        text: text.clone(),
                                        signature: encrypted.clone(),
                                    },
                                },
                            });
                        }
                        ContentPart::ToolCall {
                            id, name, input, ..
                        } => {
                            content.push(BedrockContentBlock::ToolUse {
                                tool_use: BedrockToolUse {
                                    tool_use_id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                },
                            });
                        }
                        _ => {
                            return Err(LLMError::Validation(
                                "Bedrock Converse assistant messages only support text, \
                                 reasoning, and tool-call content"
                                    .to_string(),
                            ));
                        }
                    }
                }
                messages.push(BedrockMessage::Assistant { content });
            }
            Role::Tool => {
                let mut content = Vec::with_capacity(message.content.len());
                for part in &message.content {
                    match part {
                        ContentPart::ToolResult { id, result, .. } => {
                            let status =
                                if matches!(result, otto_events::ToolResultValue::Error { .. }) {
                                    "error"
                                } else {
                                    "success"
                                };
                            content.push(BedrockContentBlock::ToolResult {
                                tool_result: BedrockToolResult {
                                    tool_use_id: id.clone(),
                                    content: lower_tool_result_content(result)?,
                                    status,
                                },
                            });
                        }
                        _ => {
                            return Err(LLMError::Validation(
                                "Bedrock Converse tool messages only support tool-result content"
                                    .to_string(),
                            ));
                        }
                    }
                }
                messages.push(BedrockMessage::User { content });
            }
        }
    }

    Ok(messages)
}

/// Lower the top-level system prompt. Port of `lowerSystem`
/// (bedrock-converse.ts:383-386), without cache-point emission (see module
/// docs) — one block per [`crate::message::SystemPart`], not joined.
fn lower_system(req: &LLMRequest) -> Option<Vec<BedrockSystemBlock>> {
    if req.system.is_empty() {
        return None;
    }
    Some(
        req.system
            .iter()
            .map(|part| BedrockSystemBlock {
                text: part.text.clone(),
            })
            .collect(),
    )
}

// =============================================================================
// Streaming Event Schema (bedrock-converse.ts:158-206)
// =============================================================================

/// `{ toolUseId, name }` — the inner `toolUse` payload of a
/// `contentBlockStart.start` (bedrock-converse.ts:165).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockStartToolUse {
    /// Tool-call id.
    pub tool_use_id: String,
    /// Tool name.
    pub name: String,
}

/// `contentBlockStart.start` (bedrock-converse.ts:163-167).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockBlockStart {
    /// Present when the opened block is a tool call.
    #[serde(default)]
    pub tool_use: Option<BedrockStartToolUse>,
}

/// `{ input }` — the inner `toolUse` payload of a `contentBlockDelta.delta`
/// (bedrock-converse.ts:176).
#[derive(Debug, Clone, Deserialize)]
pub struct BedrockDeltaToolUse {
    /// The next chunk of the tool's raw (partial) JSON input.
    pub input: String,
}

/// `contentBlockDelta.delta.reasoningContent` (bedrock-converse.ts:177-182).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BedrockReasoningDelta {
    /// The next chunk of reasoning text, if any.
    #[serde(default)]
    pub text: Option<String>,
    /// A provider signature for the reasoning block, if any.
    #[serde(default)]
    pub signature: Option<String>,
}

/// `contentBlockDelta.delta` (bedrock-converse.ts:173-184). Exactly one of
/// `text` / `tool_use` / `reasoning_content` is present on any given delta.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockBlockDelta {
    /// A text chunk.
    #[serde(default)]
    pub text: Option<String>,
    /// A tool-input chunk.
    #[serde(default)]
    pub tool_use: Option<BedrockDeltaToolUse>,
    /// A reasoning chunk.
    #[serde(default)]
    pub reasoning_content: Option<BedrockReasoningDelta>,
}

/// The provider usage breakdown on a `metadata` event
/// (`BedrockUsageSchema`, bedrock-converse.ts:145-152).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockEventUsage {
    /// Total prompt tokens, inclusive of any cached subset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Provider-reported total, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// The cached-read subset of `input_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    /// The cache-write subset of `input_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u64>,
}

/// One decoded streamed event (`BedrockEvent`, bedrock-converse.ts:158-206).
///
/// Externally tagged by the wrapper's single key
/// (`#[serde(rename_all = "camelCase")]` applied to the variant names), e.g.
/// `{"contentBlockDelta": {...}}` — see the module docs' "Wire framing" note
/// for why this differs in shape from the TS source's all-optional-fields
/// struct while covering the same cases.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BedrockEvent {
    /// `{ role }` (bedrock-converse.ts:159). Not consulted by the reducer.
    MessageStart {
        /// The message role, always `"assistant"`.
        #[allow(dead_code)]
        role: String,
    },
    /// `{ contentBlockIndex, start? }` (bedrock-converse.ts:160-169).
    ///
    /// Field-level `rename_all` is needed here (and on the other
    /// multi-field variants below) because an enum-level `rename_all`
    /// attribute only renames variant *tags*, not the fields nested inside
    /// struct variants.
    #[serde(rename_all = "camelCase")]
    ContentBlockStart {
        /// The content-block index this event applies to.
        content_block_index: u64,
        /// Present when the opened block is a tool call.
        #[serde(default)]
        start: Option<BedrockBlockStart>,
    },
    /// `{ contentBlockIndex, delta? }` (bedrock-converse.ts:170-186).
    #[serde(rename_all = "camelCase")]
    ContentBlockDelta {
        /// The content-block index this event applies to.
        content_block_index: u64,
        /// The delta payload.
        #[serde(default)]
        delta: Option<BedrockBlockDelta>,
    },
    /// `{ contentBlockIndex }` (bedrock-converse.ts:187).
    #[serde(rename_all = "camelCase")]
    ContentBlockStop {
        /// The content-block index that closed.
        content_block_index: u64,
    },
    /// `{ stopReason, additionalModelResponseFields? }`
    /// (bedrock-converse.ts:188-193).
    #[serde(rename_all = "camelCase")]
    MessageStop {
        /// The provider's terminal stop reason.
        stop_reason: String,
        /// Model-specific fields, unused by the reducer.
        #[serde(default)]
        #[allow(dead_code)]
        additional_model_response_fields: Option<Value>,
    },
    /// `{ usage?, metrics? }` (bedrock-converse.ts:194-199).
    Metadata {
        /// The usage breakdown, if reported on this chunk.
        #[serde(default)]
        usage: Option<BedrockEventUsage>,
        /// Provider metrics, unused by the reducer.
        #[serde(default)]
        #[allow(dead_code)]
        metrics: Option<Value>,
    },
    /// `{ message }` (bedrock-converse.ts:200). Maps to `provider-error`,
    /// `retryable: true`.
    InternalServerException {
        /// The error message.
        #[serde(default)]
        message: Option<String>,
    },
    /// `{ message }` (bedrock-converse.ts:203). Maps to `provider-error`,
    /// `retryable: true`.
    ThrottlingException {
        /// The error message.
        #[serde(default)]
        message: Option<String>,
    },
    /// `{ message }` (bedrock-converse.ts:202). Maps to `provider-error`,
    /// `retryable: false`, with context-overflow classification.
    ValidationException {
        /// The error message.
        #[serde(default)]
        message: Option<String>,
    },
    /// `{ message }` (bedrock-converse.ts:201). Maps to `provider-error`,
    /// `retryable: true`.
    ModelStreamErrorException {
        /// The error message.
        #[serde(default)]
        message: Option<String>,
    },
    /// `{ message }` (bedrock-converse.ts:204). Maps to `provider-error`,
    /// `retryable: true`.
    ServiceUnavailableException {
        /// The error message.
        #[serde(default)]
        message: Option<String>,
    },
}

// =============================================================================
// Usage / finish-reason mapping (bedrock-converse.ts:431-456)
// =============================================================================

/// Wrap provider metadata under the `bedrock` key (`bedrockMetadata`,
/// bedrock-converse.ts:250).
fn bedrock_metadata(inner: Json) -> Json {
    json!({ "bedrock": inner })
}

/// `Math.max(0, total - subtrahend)` token subtraction
/// (`ProviderShared.subtractTokens`, shared.ts:72-76), reused verbatim from
/// `gemini::subtract_tokens`.
fn subtract_tokens(total: Option<u64>, subtrahend: Option<u64>) -> Option<u64> {
    match (total, subtrahend) {
        (None, _) => None,
        (Some(total), None) => Some(total),
        (Some(total), Some(sub)) => Some(total.saturating_sub(sub)),
    }
}

/// Provider total, else `input + output` when at least one is present
/// (`ProviderShared.totalTokens`, shared.ts:51-59), reused verbatim from
/// `gemini::total_tokens`.
fn total_tokens(input: Option<u64>, output: Option<u64>, total: Option<u64>) -> Option<u64> {
    if let Some(total) = total {
        return Some(total);
    }
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(input.unwrap_or(0) + output.unwrap_or(0))
}

/// Map a provider usage breakdown into the neutral [`Usage`]. Port of
/// `mapUsage` (bedrock-converse.ts:443-456). Bedrock's `inputTokens` is
/// *inclusive* of any cached subset, so the non-cached breakdown is derived
/// by subtraction (mirroring Gemini, not Anthropic's non-overlapping report).
fn map_usage(usage: &BedrockEventUsage) -> Usage {
    let cache_total =
        usage.cache_read_input_tokens.unwrap_or(0) + usage.cache_write_input_tokens.unwrap_or(0);
    let non_cached = subtract_tokens(usage.input_tokens, Some(cache_total));
    Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        non_cached_input_tokens: non_cached,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        reasoning_tokens: None,
        total_tokens: total_tokens(usage.input_tokens, usage.output_tokens, usage.total_tokens),
        provider_metadata: Some(bedrock_metadata(
            serde_json::to_value(usage).unwrap_or(Value::Null),
        )),
    }
}

/// Map a Bedrock `stopReason` to a neutral [`FinishReason`]. Port of
/// `mapFinishReason` (bedrock-converse.ts:431-437).
fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "content_filtered" | "guardrail_intervened" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

/// Bedrock/Anthropic-style context-overflow detection. Port of
/// `isContextOverflow` (provider-error.ts:26-27), reused verbatim from
/// `anthropic_messages::is_context_overflow` (Bedrock's Anthropic-family
/// models surface the same substrings).
fn is_context_overflow(message: &str) -> bool {
    let lower = message.to_lowercase();
    const NEEDLES: [&str; 17] = [
        "prompt is too long",
        "input is too long for requested model",
        "exceeds the context window",
        "exceeds the maximum",
        "maximum prompt length is",
        "reduce the length of the messages",
        "maximum context length is",
        "exceeds the limit of",
        "exceeds the available context size",
        "greater than the context length",
        "context window exceeds limit",
        "exceeded model token limit",
        "context_length_exceeded",
        "context length exceeded",
        "request entity too large",
        "context length is only",
        "model_context_window_exceeded",
    ];
    if NEEDLES.iter().any(|n| lower.contains(n)) {
        return true;
    }
    // `4(00|13) ... (no body)` heuristic.
    (lower.starts_with("400") || lower.starts_with("413")) && lower.contains("no body")
}

// =============================================================================
// Stream reducer (bedrock-converse.ts:458-629)
// =============================================================================

/// The terminal event, held until both halves have arrived. Port of the
/// `pendingFinish` slot of `ParserState` (bedrock-converse.ts:461-463).
#[derive(Debug, Clone)]
struct PendingFinish {
    /// The mapped stop reason (from `messageStop`, or `stop` if `metadata`
    /// arrived first).
    reason: FinishReason,
    /// The mapped usage (from `metadata`), if it has arrived yet.
    usage: Option<Usage>,
}

/// Per-stream reducer state. Port of `ParserState` (bedrock-converse.ts:458-467).
#[derive(Default)]
pub struct ParserState {
    /// The streaming tool-call accumulator, keyed by content-block index.
    tools: tool_stream::State<u64>,
    /// The step/text/reasoning lifecycle machine.
    lifecycle: lifecycle::State,
    /// Content-block indices that are tool calls (otto-specific bookkeeping,
    /// same role as `anthropic_messages::ParserState::tool_indices`: otto's
    /// [`tool_stream::State::finish`] errors on unknown keys, unlike
    /// opencode's no-op `ToolStream.finish`, so `content_block_stop` must
    /// know up front whether a given index is a tool block).
    tool_indices: HashSet<u64>,
    /// The most recently seen `reasoningContent.signature` per content-block
    /// index, attached to the `reasoning-end` event when that block closes.
    reasoning_signatures: HashMap<u64, String>,
    /// Whether any tool call has completed (`hasToolCalls`).
    has_tool_calls: bool,
    /// The held terminal event; see the module docs' "Split finish" note.
    pending_finish: Option<PendingFinish>,
}

impl ParserState {
    /// Close the block at `index`: `contentBlockStop` for a non-tool block.
    /// Closes any open text block then any open reasoning block, attaching
    /// the tracked signature (if any) to the `reasoning-end` event. Port of
    /// the non-tool branch of `contentBlockStop` (bedrock-converse.ts:544-572).
    fn close_content_block(&mut self, index: u64) -> Vec<LLMEvent> {
        let mut events = self.lifecycle.text_end(&format!("text-{index}"));
        let mut reasoning_end = self.lifecycle.reasoning_end(&format!("reasoning-{index}"));
        if let Some(signature) = self.reasoning_signatures.remove(&index) {
            let meta = bedrock_metadata(json!({ "signature": signature }));
            for event in &mut reasoning_end {
                if let LLMEvent::ReasoningEnd {
                    provider_metadata, ..
                } = event
                {
                    *provider_metadata = Some(meta.clone());
                }
            }
        }
        events.extend(reasoning_end);
        events
    }
}

// =============================================================================
// Protocol
// =============================================================================

/// The Bedrock Converse protocol — request body construction and the
/// streaming reducer.
#[derive(Debug, Clone, Copy, Default)]
pub struct BedrockConverse;

impl Protocol for BedrockConverse {
    type Body = BedrockConverseBody;
    type Event = BedrockEvent;
    type State = ParserState;

    fn id(&self) -> &'static str {
        ADAPTER
    }

    /// Build the request body. Port of `fromRequest` (bedrock-converse.ts:388-426).
    fn build_body(&self, req: &LLMRequest) -> Result<Self::Body, LLMError> {
        let tools_enabled =
            !req.tools.is_empty() && !matches!(req.tool_choice, Some(ToolChoice::None));

        let tool_config = tools_enabled.then(|| BedrockToolConfig {
            tools: req.tools.iter().map(lower_tool_spec).collect(),
            tool_choice: req.tool_choice.as_ref().and_then(lower_tool_choice),
        });

        let system = lower_system(req);
        let messages = lower_messages(req)?;

        let generation = req.generation.as_ref();
        let inference_config = BedrockInferenceConfig {
            max_tokens: generation.and_then(|g| g.max_tokens),
            temperature: if req.model.capabilities.temperature {
                generation.and_then(|g| g.temperature)
            } else {
                None
            },
            top_p: generation.and_then(|g| g.top_p),
            stop_sequences: generation
                .map(|g| g.stop.clone())
                .filter(|stop| !stop.is_empty()),
        };

        // Converse has no native topK field; it rides along in
        // additionalModelRequestFields (bedrock-converse.ts:139-142, 422-424).
        let additional_model_request_fields = generation
            .and_then(|g| g.top_k)
            .map(|top_k| json!({ "top_k": top_k }));

        Ok(BedrockConverseBody {
            model_id: req.model.id.0.clone(),
            messages,
            system,
            inference_config: (!inference_config.is_empty()).then_some(inference_config),
            tool_config,
            additional_model_request_fields,
        })
    }

    /// Decode one wrapper-JSON frame emitted by `BedrockTransport` (see the
    /// module docs' "Wire framing" note). Real AWS event-stream binary
    /// framing (`BedrockEventStream.framing`, bedrock-converse.ts:616) is the
    /// transport's concern, not this protocol's.
    fn decode_event(&self, frame: &str) -> Result<Self::Event, LLMError> {
        serde_json::from_str(frame)
            .map_err(|e| LLMError::EventDecode(format!("invalid Bedrock Converse event: {e}")))
    }

    fn initial(&self, _req: &LLMRequest) -> Self::State {
        ParserState::default()
    }

    /// Fold one streamed event into the neutral event stream. Port of `step`
    /// (bedrock-converse.ts:469-614). `messageStart` and unmatched delta
    /// shapes emit nothing. Neither `messageStop` nor `metadata` ever emits
    /// `step-finish`/`finish` directly — like the TS source, they only record
    /// the pending finish; [`BedrockConverse::on_halt`] is what turns that
    /// into the actual finish events (see the module docs' "Split finish"
    /// note).
    fn step(&self, state: &mut Self::State, event: Self::Event) -> Result<Vec<LLMEvent>, LLMError> {
        match event {
            BedrockEvent::MessageStart { .. } => Ok(Vec::new()),

            // contentBlockStart.start.toolUse (bedrock-converse.ts:471-492).
            BedrockEvent::ContentBlockStart {
                content_block_index,
                start,
            } => {
                let Some(tool_use) = start.and_then(|s| s.tool_use) else {
                    return Ok(Vec::new());
                };
                let mut events = state.lifecycle.step_start(0);
                events.extend(state.tools.start(
                    content_block_index,
                    tool_use.tool_use_id,
                    tool_use.name,
                    None,
                    None,
                ));
                state.tool_indices.insert(content_block_index);
                Ok(events)
            }

            // contentBlockDelta.delta.{text,reasoningContent,toolUse}
            // (bedrock-converse.ts:494-542). Exactly one branch fires per
            // delta; text takes priority (matching the TS `if`-chain order),
            // then reasoning, then tool input.
            BedrockEvent::ContentBlockDelta {
                content_block_index,
                delta,
            } => {
                let Some(delta) = delta else {
                    return Ok(Vec::new());
                };
                let idx = content_block_index;

                if let Some(text) = delta.text.filter(|t| !t.is_empty()) {
                    let mut events = state.lifecycle.step_start(0);
                    events.extend(state.lifecycle.text_delta(&format!("text-{idx}"), text));
                    return Ok(events);
                }

                if let Some(reasoning) = delta.reasoning_content {
                    let mut events = Vec::new();
                    if let Some(text) = reasoning.text.filter(|t| !t.is_empty()) {
                        events.extend(state.lifecycle.step_start(0));
                        events.extend(
                            state
                                .lifecycle
                                .reasoning_delta(&format!("reasoning-{idx}"), text),
                        );
                    }
                    if let Some(signature) = reasoning.signature {
                        state.reasoning_signatures.insert(idx, signature);
                    }
                    return Ok(events);
                }

                if let Some(tool_use) = delta.tool_use {
                    let delta_events = state.tools.append_existing(&idx, &tool_use.input)?;
                    let mut events = Vec::new();
                    if !delta_events.is_empty() {
                        events.extend(state.lifecycle.step_start(0));
                    }
                    events.extend(delta_events);
                    return Ok(events);
                }

                Ok(Vec::new())
            }

            // contentBlockStop (bedrock-converse.ts:544-572).
            BedrockEvent::ContentBlockStop {
                content_block_index,
            } => {
                let idx = content_block_index;
                if state.tool_indices.remove(&idx) {
                    let mut events = state.lifecycle.step_start(0);
                    let finish_events = state.tools.finish(&idx)?;
                    if finish_events
                        .iter()
                        .any(|e| matches!(e, LLMEvent::ToolCall { .. }))
                    {
                        state.has_tool_calls = true;
                    }
                    events.extend(finish_events);
                    Ok(events)
                } else {
                    Ok(state.close_content_block(idx))
                }
            }

            // messageStop → hold the mapped reason (bedrock-converse.ts:574-582).
            BedrockEvent::MessageStop { stop_reason, .. } => {
                let reason = map_finish_reason(&stop_reason);
                let usage = state.pending_finish.as_ref().and_then(|p| p.usage.clone());
                state.pending_finish = Some(PendingFinish { reason, usage });
                Ok(Vec::new())
            }

            // metadata → hold the mapped usage (bedrock-converse.ts:584-587).
            BedrockEvent::Metadata { usage, .. } => {
                let mapped_usage = usage.as_ref().map(map_usage);
                let reason = state
                    .pending_finish
                    .as_ref()
                    .map(|p| p.reason)
                    .unwrap_or(FinishReason::Stop);
                state.pending_finish = Some(PendingFinish {
                    reason,
                    usage: mapped_usage,
                });
                Ok(Vec::new())
            }

            // internalServerException / modelStreamErrorException /
            // serviceUnavailableException → retryable provider-error
            // (bedrock-converse.ts:589-596).
            BedrockEvent::InternalServerException { message }
            | BedrockEvent::ModelStreamErrorException { message }
            | BedrockEvent::ServiceUnavailableException { message } => {
                Ok(vec![LLMEvent::ProviderError {
                    message: message.unwrap_or_else(|| "Bedrock Converse stream error".to_string()),
                    classification: None,
                    retryable: Some(true),
                    provider_metadata: None,
                }])
            }

            // validationException → non-retryable provider-error, flagged as
            // context-overflow when the message matches (bedrock-converse.ts:598-610).
            BedrockEvent::ValidationException { message } => {
                let message = message.unwrap_or_else(|| "Bedrock Converse error".to_string());
                let classification = is_context_overflow(&message)
                    .then_some(ProviderFailureClassification::ContextOverflow);
                Ok(vec![LLMEvent::ProviderError {
                    message,
                    classification,
                    retryable: Some(false),
                    provider_metadata: None,
                }])
            }

            // throttlingException → retryable provider-error
            // (bedrock-converse.ts:598-610).
            BedrockEvent::ThrottlingException { message } => Ok(vec![LLMEvent::ProviderError {
                message: message.unwrap_or_else(|| "Bedrock Converse error".to_string()),
                classification: None,
                retryable: Some(true),
                provider_metadata: None,
            }]),
        }
    }

    /// Emit the held finish (+ usage) and flush any still-open text/
    /// reasoning/tool block. Port of `onHalt` (bedrock-converse.ts:618-629),
    /// extended per the module docs' "Split finish" note to still emit an
    /// `unknown`-reason finish if the lifecycle ever opened but the stream
    /// ended without a `messageStop`/`metadata` chunk.
    fn on_halt(&self, state: &mut Self::State) -> Vec<LLMEvent> {
        if state.pending_finish.is_none() && !state.lifecycle.is_started() {
            return Vec::new();
        }

        let mut events = state.lifecycle.step_start(0);

        // Flush any tool block that never saw a matching contentBlockStop.
        let tool_flush = state.tools.finish_all().unwrap_or_default();
        if tool_flush
            .iter()
            .any(|e| matches!(e, LLMEvent::ToolCall { .. }))
        {
            state.has_tool_calls = true;
        }
        events.extend(tool_flush);

        let (reason, usage) = match state.pending_finish.take() {
            Some(pending) => {
                let reason = if pending.reason == FinishReason::Stop && state.has_tool_calls {
                    FinishReason::ToolCalls
                } else {
                    pending.reason
                };
                (reason, pending.usage)
            }
            None => (FinishReason::Unknown, None),
        };

        events.extend(state.lifecycle.finish(reason, usage, 0));
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, SystemPart};
    use crate::model::Model;
    use crate::request::GenerationOptions;
    use otto_events::ToolResultValue;

    fn body_for(req: &LLMRequest) -> Value {
        let proto = BedrockConverse;
        serde_json::to_value(proto.build_body(req).expect("build_body")).expect("serialize")
    }

    fn base_request() -> LLMRequest {
        LLMRequest::new(
            Model::new("bedrock", "anthropic.claude-sonnet-4", "bedrock-converse"),
            vec![Message::user(vec![ContentPart::text("hi")])],
        )
    }

    #[test]
    fn system_lowers_to_top_level_system_not_a_role() {
        let mut req = base_request();
        req.system = vec![SystemPart::new("be terse"), SystemPart::new("be kind")];
        let body = body_for(&req);
        assert_eq!(
            body["system"],
            json!([{"text": "be terse"}, {"text": "be kind"}])
        );
        // Not folded into `messages` as a role.
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn no_system_omits_system_field() {
        let body = body_for(&base_request());
        assert!(body.get("system").is_none());
    }

    #[test]
    fn assistant_tool_call_lowers_to_tool_use() {
        let req = LLMRequest::new(
            Model::new("bedrock", "anthropic.claude-sonnet-4", "bedrock-converse"),
            vec![Message::assistant(vec![ContentPart::ToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: json!({"city": "paris"}),
                provider_executed: None,
            }])],
        );
        let body = body_for(&req);
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert_eq!(
            body["messages"][0]["content"][0]["toolUse"],
            json!({"toolUseId": "call_1", "name": "get_weather", "input": {"city": "paris"}})
        );
    }

    #[test]
    fn assistant_reasoning_lowers_with_signature_from_encrypted() {
        let req = LLMRequest::new(
            Model::new("bedrock", "anthropic.claude-sonnet-4", "bedrock-converse"),
            vec![Message::assistant(vec![ContentPart::Reasoning {
                text: "thinking...".into(),
                encrypted: Some("sig".into()),
            }])],
        );
        let body = body_for(&req);
        assert_eq!(
            body["messages"][0]["content"][0]["reasoningContent"],
            json!({"reasoningText": {"text": "thinking...", "signature": "sig"}})
        );
    }

    #[test]
    fn tool_result_lowers_to_user_role_tool_result() {
        let req = LLMRequest::new(
            Model::new("bedrock", "anthropic.claude-sonnet-4", "bedrock-converse"),
            vec![Message::tool(vec![ContentPart::ToolResult {
                id: "call_1".into(),
                name: "get_weather".into(),
                result: ToolResultValue::Text {
                    value: Value::String("sunny".into()),
                },
                provider_executed: None,
                cache: None,
            }])],
        );
        let body = body_for(&req);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(
            body["messages"][0]["content"][0]["toolResult"],
            json!({"toolUseId": "call_1", "content": [{"text": "sunny"}], "status": "success"})
        );
    }

    #[test]
    fn error_tool_result_sets_status_error() {
        let req = LLMRequest::new(
            Model::new("bedrock", "anthropic.claude-sonnet-4", "bedrock-converse"),
            vec![Message::tool(vec![ContentPart::ToolResult {
                id: "call_1".into(),
                name: "boom".into(),
                result: ToolResultValue::Error {
                    value: Value::String("kaboom".into()),
                },
                provider_executed: None,
                cache: None,
            }])],
        );
        let body = body_for(&req);
        assert_eq!(
            body["messages"][0]["content"][0]["toolResult"]["status"],
            "error"
        );
    }

    #[test]
    fn tools_lower_to_tool_spec_with_input_schema_json() {
        let mut req = base_request();
        req.tools = vec![ToolDefinition {
            name: "get_weather".into(),
            description: Some("Get the weather".into()),
            input_schema: json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }),
            output_schema: None,
            cache: None,
        }];
        req.tool_choice = Some(ToolChoice::Auto);
        let body = body_for(&req);
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["name"],
            "get_weather"
        );
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["description"],
            "Get the weather"
        );
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["properties"]["city"]
                ["type"],
            "string"
        );
        assert_eq!(body["toolConfig"]["toolChoice"], json!({"auto": {}}));
    }

    #[test]
    fn tool_choice_none_omits_tool_config() {
        let mut req = base_request();
        req.tools = vec![ToolDefinition {
            name: "t".into(),
            description: Some("d".into()),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            cache: None,
        }];
        req.tool_choice = Some(ToolChoice::None);
        let body = body_for(&req);
        assert!(body.get("toolConfig").is_none());
    }

    #[test]
    fn tool_choice_required_maps_to_any() {
        let mut req = base_request();
        req.tools = vec![ToolDefinition {
            name: "t".into(),
            description: Some("d".into()),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            cache: None,
        }];
        req.tool_choice = Some(ToolChoice::Required);
        let body = body_for(&req);
        assert_eq!(body["toolConfig"]["toolChoice"], json!({"any": {}}));
    }

    #[test]
    fn tool_choice_named_maps_to_tool_with_name() {
        let mut req = base_request();
        req.tools = vec![ToolDefinition {
            name: "t".into(),
            description: Some("d".into()),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            cache: None,
        }];
        req.tool_choice = Some(ToolChoice::Tool { name: "t".into() });
        let body = body_for(&req);
        assert_eq!(
            body["toolConfig"]["toolChoice"],
            json!({"tool": {"name": "t"}})
        );
    }

    #[test]
    fn temperature_gated_on_model_capability() {
        let mut req = base_request();
        req.generation = Some(GenerationOptions {
            temperature: Some(0.7),
            max_tokens: Some(1024),
            ..GenerationOptions::default()
        });
        req.model.capabilities.temperature = false;
        let body = body_for(&req);
        assert!(body["inferenceConfig"].get("temperature").is_none());
        // maxTokens is never gated on the temperature capability.
        assert_eq!(body["inferenceConfig"]["maxTokens"], json!(1024));

        req.model.capabilities.temperature = true;
        let body = body_for(&req);
        assert_eq!(body["inferenceConfig"]["temperature"], json!(0.7));
    }

    #[test]
    fn no_generation_omits_inference_config() {
        let body = body_for(&base_request());
        assert!(body.get("inferenceConfig").is_none());
    }

    #[test]
    fn top_k_rides_additional_model_request_fields() {
        let mut req = base_request();
        req.generation = Some(GenerationOptions {
            top_k: Some(40),
            ..GenerationOptions::default()
        });
        let body = body_for(&req);
        // top_k does not appear in inferenceConfig (Converse has no native field).
        assert!(body["inferenceConfig"].get("topK").is_none());
        assert_eq!(body["additionalModelRequestFields"], json!({"top_k": 40}));
    }

    #[test]
    fn mid_conversation_system_message_wraps_into_user_text() {
        let req = LLMRequest::new(
            Model::new("bedrock", "anthropic.claude-sonnet-4", "bedrock-converse"),
            vec![
                Message::user(vec![ContentPart::text("hi")]),
                Message::system(vec![ContentPart::text("session refreshed")]),
            ],
        );
        let body = body_for(&req);
        // The system update is appended to the same trailing user message
        // (Bedrock Converse has no dedicated mid-conversation system role).
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert!(
            content[1]["text"]
                .as_str()
                .unwrap()
                .contains("session refreshed")
        );
    }

    #[test]
    fn model_id_passes_through() {
        let body = body_for(&base_request());
        assert_eq!(body["modelId"], "anthropic.claude-sonnet-4");
    }

    // -- streaming reducer (bedrock-converse.ts:469-629) -------------------

    /// Flatten an event slice into its kebab-case type tags (mirrors
    /// `anthropic_messages::tests::types` / `gemini::tests::types`).
    fn types(events: &[LLMEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                LLMEvent::StepStart { .. } => "step-start",
                LLMEvent::TextStart { .. } => "text-start",
                LLMEvent::TextDelta { .. } => "text-delta",
                LLMEvent::TextEnd { .. } => "text-end",
                LLMEvent::ReasoningStart { .. } => "reasoning-start",
                LLMEvent::ReasoningDelta { .. } => "reasoning-delta",
                LLMEvent::ReasoningEnd { .. } => "reasoning-end",
                LLMEvent::ToolInputStart { .. } => "tool-input-start",
                LLMEvent::ToolInputDelta { .. } => "tool-input-delta",
                LLMEvent::ToolInputEnd { .. } => "tool-input-end",
                LLMEvent::ToolCall { .. } => "tool-call",
                LLMEvent::ToolResult { .. } => "tool-result",
                LLMEvent::ToolError { .. } => "tool-error",
                LLMEvent::StepFinish { .. } => "step-finish",
                LLMEvent::Finish { .. } => "finish",
                LLMEvent::ProviderError { .. } => "provider-error",
                LLMEvent::Retry { .. } => "retry",
                LLMEvent::Warning { .. } => "warning",
            })
            .collect()
    }

    /// Feed a scripted sequence of wrapper-JSON frames (the shape
    /// `BedrockTransport` emits; see the module docs) through `decode_event`
    /// and `step`, then `on_halt` (Bedrock's held finish is only emitted at
    /// halt, mirroring `gemini::tests::run`), returning the flattened event
    /// list.
    fn run(frames: &[&str]) -> Vec<LLMEvent> {
        let proto = BedrockConverse;
        let mut state = proto.initial(&base_request());
        let mut out = Vec::new();
        for frame in frames {
            let event = proto.decode_event(frame).expect("decode");
            out.extend(proto.step(&mut state, event).expect("step"));
        }
        out.extend(proto.on_halt(&mut state));
        out
    }

    #[test]
    fn text_golden_sequence() {
        let frames = [
            r#"{"messageStart":{"role":"assistant"}}"#,
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"Hello"}}}"#,
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":" world"}}}"#,
            r#"{"contentBlockStop":{"contentBlockIndex":0}}"#,
            r#"{"messageStop":{"stopReason":"end_turn"}}"#,
            r#"{"metadata":{"usage":{"inputTokens":10,"outputTokens":5,"totalTokens":15}}}"#,
        ];
        let events = run(&frames);
        assert_eq!(
            types(&events),
            [
                "step-start",
                "text-start",
                "text-delta",
                "text-delta",
                "text-end",
                "step-finish",
                "finish",
            ]
        );
        match events.last().unwrap() {
            LLMEvent::Finish {
                reason,
                usage: Some(usage),
                ..
            } => {
                assert_eq!(*reason, FinishReason::Stop);
                assert_eq!(usage.input_tokens, Some(10));
                assert_eq!(usage.output_tokens, Some(5));
                assert_eq!(usage.total_tokens, Some(15));
                assert!(usage.invariant_holds());
            }
            other => panic!("expected finish with usage, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_golden_sequence() {
        let frames = [
            r#"{"contentBlockStart":{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"call_1","name":"get_weather"}}}}"#,
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"toolUse":{"input":"{\"city\":"}}}}"#,
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"toolUse":{"input":"\"paris\"}"}}}}"#,
            r#"{"contentBlockStop":{"contentBlockIndex":0}}"#,
            r#"{"messageStop":{"stopReason":"tool_use"}}"#,
        ];
        let events = run(&frames);
        assert_eq!(
            types(&events),
            [
                "step-start",
                "tool-input-start",
                "tool-input-delta",
                "tool-input-delta",
                "tool-input-end",
                "tool-call",
                "step-finish",
                "finish",
            ]
        );
        let tool_call = events
            .iter()
            .find_map(|e| match e {
                LLMEvent::ToolCall {
                    id, name, input, ..
                } => Some((id.clone(), name.clone(), input.clone())),
                _ => None,
            })
            .expect("tool-call");
        assert_eq!(tool_call.0, "call_1");
        assert_eq!(tool_call.1, "get_weather");
        assert_eq!(tool_call.2["city"], "paris");
        match events.last().unwrap() {
            LLMEvent::Finish { reason, .. } => assert_eq!(*reason, FinishReason::ToolCalls),
            other => panic!("expected finish, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_golden_sequence_with_signature() {
        let frames = [
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"Let me think"}}}}"#,
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":" more","signature":"sig123"}}}}"#,
            r#"{"contentBlockStop":{"contentBlockIndex":0}}"#,
            r#"{"messageStop":{"stopReason":"end_turn"}}"#,
        ];
        let events = run(&frames);
        assert_eq!(
            types(&events),
            [
                "step-start",
                "reasoning-start",
                "reasoning-delta",
                "reasoning-delta",
                "reasoning-end",
                "step-finish",
                "finish",
            ]
        );
        let signature = events
            .iter()
            .find_map(|e| match e {
                LLMEvent::ReasoningEnd {
                    provider_metadata: Some(meta),
                    ..
                } => Some(meta.clone()),
                _ => None,
            })
            .expect("reasoning-end with metadata");
        assert_eq!(signature["bedrock"]["signature"], "sig123");
        match events.last().unwrap() {
            LLMEvent::Finish { reason, .. } => assert_eq!(*reason, FinishReason::Stop),
            other => panic!("expected finish, got {other:?}"),
        }
    }

    #[test]
    fn split_finish_metadata_before_message_stop_still_yields_one_finish() {
        // metadata (usage) arrives *before* messageStop (reason) — the
        // opposite order from the golden text/tool-use sequences above.
        // Exactly one finish must still be emitted, carrying both.
        let frames = [
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"hi"}}}"#,
            r#"{"contentBlockStop":{"contentBlockIndex":0}}"#,
            r#"{"metadata":{"usage":{"inputTokens":7,"outputTokens":3}}}"#,
            r#"{"messageStop":{"stopReason":"end_turn"}}"#,
        ];
        let events = run(&frames);
        let finishes = events
            .iter()
            .filter(|e| matches!(e, LLMEvent::Finish { .. }))
            .count();
        assert_eq!(finishes, 1, "expected exactly one finish, got {events:?}");
        match events.last().unwrap() {
            LLMEvent::Finish {
                reason,
                usage: Some(usage),
                ..
            } => {
                assert_eq!(*reason, FinishReason::Stop);
                assert_eq!(usage.input_tokens, Some(7));
                assert_eq!(usage.output_tokens, Some(3));
            }
            other => panic!("expected finish with usage, got {other:?}"),
        }
    }

    #[test]
    fn split_finish_tool_use_reason_survives_metadata_arriving_first() {
        // Same split-finish ordering as above, but the stop reason is
        // tool_use — the earlier-arriving metadata must not clobber it with
        // the "stop" default.
        let frames = [
            r#"{"contentBlockStart":{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"call_1","name":"get_weather"}}}}"#,
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"toolUse":{"input":"{}"}}}}"#,
            r#"{"contentBlockStop":{"contentBlockIndex":0}}"#,
            r#"{"metadata":{"usage":{"inputTokens":7,"outputTokens":3}}}"#,
            r#"{"messageStop":{"stopReason":"tool_use"}}"#,
        ];
        let events = run(&frames);
        match events.last().unwrap() {
            LLMEvent::Finish {
                reason,
                usage: Some(usage),
                ..
            } => {
                assert_eq!(*reason, FinishReason::ToolCalls);
                assert_eq!(usage.input_tokens, Some(7));
            }
            other => panic!("expected finish with usage, got {other:?}"),
        }
    }

    #[test]
    fn stream_end_without_message_stop_still_finishes_unknown() {
        // A truncated stream (no messageStop, no metadata) must still close
        // out any open block rather than leaving the step open forever.
        let events =
            run(&[r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"partial"}}}"#]);
        assert_eq!(
            types(&events),
            [
                "step-start",
                "text-start",
                "text-delta",
                "text-end",
                "step-finish",
                "finish"
            ]
        );
        match events.last().unwrap() {
            LLMEvent::Finish { reason, usage, .. } => {
                assert_eq!(*reason, FinishReason::Unknown);
                assert!(usage.is_none());
            }
            other => panic!("expected finish, got {other:?}"),
        }
    }

    #[test]
    fn untouched_stream_emits_nothing_at_halt() {
        // No frames at all: on_halt must not synthesize a phantom finish.
        assert!(run(&[]).is_empty());
    }

    #[test]
    fn exception_events_map_to_provider_error() {
        let throttled = run(&[r#"{"throttlingException":{"message":"slow down"}}"#]);
        match throttled.as_slice() {
            [
                LLMEvent::ProviderError {
                    message, retryable, ..
                },
            ] => {
                assert_eq!(message, "slow down");
                assert_eq!(*retryable, Some(true));
            }
            other => panic!("expected provider-error, got {other:?}"),
        }

        let validation =
            run(&[r#"{"validationException":{"message":"prompt is too long: 250000 tokens"}}"#]);
        match validation.as_slice() {
            [
                LLMEvent::ProviderError {
                    classification,
                    retryable,
                    ..
                },
            ] => {
                assert_eq!(
                    *classification,
                    Some(ProviderFailureClassification::ContextOverflow)
                );
                assert_eq!(*retryable, Some(false));
            }
            other => panic!("expected provider-error, got {other:?}"),
        }
    }
}
