//! The Anthropic Messages wire protocol.
//!
//! Faithful port of opencode
//! `packages/llm/src/protocols/anthropic-messages.ts`. Covers request-body
//! construction ([`AnthropicMessages::build_body`], porting `fromRequest` +
//! `lowerMessages`) and the streaming-event reducer
//! ([`AnthropicMessages::step`], porting the `step` dispatch at
//! anthropic-messages.ts:814-822).
//!
//! Line references throughout point at the TypeScript source of truth.

use std::collections::HashSet;

use otto_events::{FinishReason, Json, LLMEvent, ProviderFailureClassification, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::LLMError;
use crate::message::{ContentPart, Role, ToolChoice};
use crate::protocol::Protocol;
use crate::protocols::utils::{lifecycle, tool_stream};
use crate::request::{CacheHint, LLMRequest};

/// The `anthropic-version` header the route sends (anthropic-messages.ts:852).
/// The route/provider layer is responsible for actually attaching it; it is
/// exposed here so callers that build requests by hand can reuse the value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic accepts at most 4 explicit `cache_control` breakpoints per request
/// across `tools`, `system`, and `messages` (anthropic-messages.ts:238).
const ANTHROPIC_BREAKPOINT_CAP: u32 = 4;

/// Image MIME types Anthropic accepts (`ProviderShared.IMAGE_MIMES`).
const IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

// =============================================================================
// Request Body Schema (anthropic-messages.ts:35-171)
// =============================================================================

/// A `cache_control` marker: `{ "type": "ephemeral", "ttl"?: "5m" | "1h" }`
/// (anthropic-messages.ts:35-38).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheControl {
    /// Always `"ephemeral"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Optional TTL bucket; omitted for the default 5-minute cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<&'static str>,
}

impl CacheControl {
    /// `EPHEMERAL_5M` (anthropic-messages.ts:240).
    fn ephemeral_5m() -> Self {
        CacheControl {
            kind: "ephemeral",
            ttl: None,
        }
    }

    /// `EPHEMERAL_1H` (anthropic-messages.ts:241).
    fn ephemeral_1h() -> Self {
        CacheControl {
            kind: "ephemeral",
            ttl: Some("1h"),
        }
    }
}

/// The `source` of an image block (anthropic-messages.ts:49-53).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageSource {
    /// Always `"base64"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The MIME type, e.g. `image/png`.
    pub media_type: String,
    /// The base64-encoded payload.
    pub data: String,
}

/// A content block on an Anthropic message.
///
/// Union of the block schemas in anthropic-messages.ts:40-129, tagged by
/// `type`. `server_tool_use` is ported; the `*_tool_result` server blocks are
/// TODO-stubbed (see [`AnthropicMessages::build_body`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// `{ "type": "text", "text", "cache_control"? }`
    /// (anthropic-messages.ts:40-45).
    Text {
        /// The text content.
        text: String,
        /// Optional cache breakpoint.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// `{ "type": "image", "source", "cache_control"? }`
    /// (anthropic-messages.ts:47-55).
    Image {
        /// The base64 image source.
        source: ImageSource,
        /// Optional cache breakpoint.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// `{ "type": "thinking", "thinking", "signature"? }`
    /// (anthropic-messages.ts:58-63).
    Thinking {
        /// The reasoning text.
        thinking: String,
        /// Provider signature for the redacted reasoning, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// `{ "type": "tool_use", "id", "name", "input" }`
    /// (anthropic-messages.ts:65-71).
    ToolUse {
        /// Tool-call id.
        id: String,
        /// Tool name.
        name: String,
        /// Parsed tool input.
        input: Json,
    },
    /// `{ "type": "server_tool_use", "id", "name", "input" }`
    /// (anthropic-messages.ts:74-80).
    ServerToolUse {
        /// Tool-call id.
        id: String,
        /// Tool name.
        name: String,
        /// Parsed tool input.
        input: Json,
    },
    /// `{ "type": "tool_result", "tool_use_id", "content", "is_error"? }`
    /// (anthropic-messages.ts:111-117).
    ToolResult {
        /// The originating `tool_use` id.
        tool_use_id: String,
        /// String or structured block content.
        content: ToolResultContent,
        /// Whether the result is an error.
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        /// Optional cache breakpoint.
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

impl ContentBlock {
    /// Build a text block (anthropic-messages.ts:40-45).
    fn text(text: String, cache_control: Option<CacheControl>) -> Self {
        ContentBlock::Text {
            text,
            cache_control,
        }
    }
}

/// The `content` of a `tool_result` block: either a plain string or an ordered
/// array of text/image blocks (anthropic-messages.ts:103-114).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    /// A plain string result.
    Text(String),
    /// An ordered array of text/image blocks.
    Blocks(Vec<ContentBlock>),
}

/// A message in the Anthropic request, tagged by `role`
/// (anthropic-messages.ts:131-136). Only the `user` and `assistant` shapes are
/// produced: system-role messages are wrapped into visible user text in the
/// common path (the native `system` role for `claude-opus-4-8` is a TODO).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum AnthropicMessage {
    /// A user turn.
    User {
        /// The ordered content blocks.
        content: Vec<ContentBlock>,
    },
    /// An assistant turn.
    Assistant {
        /// The ordered content blocks.
        content: Vec<ContentBlock>,
    },
}

/// A tool definition (anthropic-messages.ts:138-143).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicTool {
    /// Tool name.
    pub name: String,
    /// Human-readable description (required by Anthropic; empty if absent).
    pub description: String,
    /// JSON Schema for the tool input.
    pub input_schema: Json,
    /// Optional cache breakpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// The `tool_choice` field (anthropic-messages.ts:146-149).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AnthropicToolChoice {
    /// The model decides.
    Auto,
    /// The model must call some tool (`required` → `any`).
    Any,
    /// The model must call the named tool.
    Tool {
        /// The required tool name.
        name: String,
    },
}

/// Extended-thinking configuration (anthropic-messages.ts:151-154).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnthropicThinking {
    /// Always `"enabled"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The reasoning token budget.
    pub budget_tokens: u64,
}

/// The Anthropic Messages request body (anthropic-messages.ts:156-171).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicMessagesBody {
    /// The model id.
    pub model: String,
    /// Top-level system prompt blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<ContentBlock>>,
    /// The conversation messages.
    pub messages: Vec<AnthropicMessage>,
    /// The available tools (omitted entirely when `tool_choice` is `none`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    /// The tool-choice policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<AnthropicToolChoice>,
    /// Always `true` — this protocol only streams.
    pub stream: bool,
    /// Maximum output tokens.
    pub max_tokens: u64,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus-sampling probability mass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Top-k sampling cutoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u64>,
    /// Stop sequences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// Extended-thinking configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<AnthropicThinking>,
}

// =============================================================================
// Streaming Event Schema (anthropic-messages.ts:173-221)
// =============================================================================

/// The provider usage breakdown (anthropic-messages.ts:173-178). Anthropic
/// reports a *non-overlapping* breakdown: `input_tokens` is the non-cached
/// count only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicUsage {
    /// Non-cached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens (includes reasoning; not broken out by Anthropic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Cache-write tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    /// Cache-read tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
}

/// `message.usage` on `message_start` (anthropic-messages.ts:209).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MessageStart {
    /// Usage reported at the start of the message.
    #[serde(default)]
    pub usage: Option<AnthropicUsage>,
}

/// A streamed content block descriptor (anthropic-messages.ts:181-194). All
/// fields are optional so partial/gateway payloads still parse.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StreamBlock {
    /// Block type (`text`, `thinking`, `tool_use`, …).
    #[serde(rename = "type", default)]
    pub block_type: Option<String>,
    /// Tool-call id.
    #[serde(default)]
    pub id: Option<String>,
    /// Tool name.
    #[serde(default)]
    pub name: Option<String>,
    /// Initial text (text blocks).
    #[serde(default)]
    pub text: Option<String>,
    /// Initial thinking text (thinking blocks).
    #[serde(default)]
    pub thinking: Option<String>,
}

/// A streamed delta (anthropic-messages.ts:196-204). All fields optional.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StreamDelta {
    /// Delta type (`text_delta`, `thinking_delta`, `signature_delta`,
    /// `input_json_delta`).
    #[serde(rename = "type", default)]
    pub delta_type: Option<String>,
    /// Text fragment.
    #[serde(default)]
    pub text: Option<String>,
    /// Thinking fragment.
    #[serde(default)]
    pub thinking: Option<String>,
    /// Partial tool-input JSON.
    #[serde(default)]
    pub partial_json: Option<String>,
    /// Reasoning signature.
    #[serde(default)]
    pub signature: Option<String>,
    /// Terminal stop reason (on `message_delta`).
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Matched stop sequence (on `message_delta`).
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

/// A stream `error` payload (anthropic-messages.ts:217-219).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ErrorInfo {
    /// Error type, e.g. `overloaded_error`.
    #[serde(rename = "type", default)]
    pub error_type: Option<String>,
    /// Human-readable error message.
    #[serde(default)]
    pub message: Option<String>,
}

/// A single decoded SSE event (anthropic-messages.ts:206-221). Deserialized
/// permissively so any partial frame still parses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AnthropicEvent {
    /// The event type, e.g. `content_block_delta`.
    #[serde(rename = "type", default)]
    pub event_type: String,
    /// Content-block index.
    #[serde(default)]
    pub index: Option<u64>,
    /// `message.usage` (on `message_start`).
    #[serde(default)]
    pub message: Option<MessageStart>,
    /// The content-block descriptor (on `content_block_start`).
    #[serde(default)]
    pub content_block: Option<StreamBlock>,
    /// The delta (on `content_block_delta` / `message_delta`).
    #[serde(default)]
    pub delta: Option<StreamDelta>,
    /// Top-level usage (on `message_delta`).
    #[serde(default)]
    pub usage: Option<AnthropicUsage>,
    /// The error payload (on `error`).
    #[serde(default)]
    pub error: Option<ErrorInfo>,
}

/// Per-stream reducer state.
///
/// Port of `ParserState` (anthropic-messages.ts:223-227). `tool_indices`
/// records which content-block indices belong to tool calls: otto's
/// [`tool_stream::State::finish`] errors on unknown keys (unlike opencode's
/// no-op finish), so `content_block_stop` consults this set before deciding
/// whether to finish a tool or close a text/reasoning block.
#[derive(Default)]
pub struct ParserState {
    /// The streaming tool-call accumulator, keyed by content-block index.
    pub tools: tool_stream::State<usize>,
    /// Merged usage seen so far.
    pub usage: Option<Usage>,
    /// The step/text/reasoning lifecycle machine.
    pub lifecycle: lifecycle::State,
    /// Content-block indices that are tool calls (otto-specific bookkeeping).
    tool_indices: HashSet<usize>,
}

/// The Anthropic Messages protocol (anthropic-messages.ts:832-843).
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicMessages;

// =============================================================================
// Cache-breakpoint budget (utils/cache.ts)
// =============================================================================

/// The 4-breakpoint budget tracker (`Cache.Breakpoints`).
struct Breakpoints {
    remaining: u32,
    dropped: u32,
}

impl Breakpoints {
    fn new(cap: u32) -> Self {
        Breakpoints {
            remaining: cap,
            dropped: 0,
        }
    }
}

/// `Cache.ttlBucket` (utils/cache.ts): `>= 3600s` → `1h`, else the 5m default.
fn ttl_is_1h(ttl_seconds: Option<u64>) -> bool {
    ttl_seconds.is_some_and(|s| s >= 3600)
}

/// Allocate one breakpoint from the budget for `cache`, dropping it past the
/// cap. Port of `cacheControl` (anthropic-messages.ts:243-251).
fn cache_control(breakpoints: &mut Breakpoints, cache: Option<&CacheHint>) -> Option<CacheControl> {
    let cache = cache?;
    if breakpoints.remaining == 0 {
        breakpoints.dropped += 1;
        return None;
    }
    breakpoints.remaining -= 1;
    if ttl_is_1h(cache.ttl_seconds) {
        Some(CacheControl::ephemeral_1h())
    } else {
        Some(CacheControl::ephemeral_5m())
    }
}

// =============================================================================
// Request Lowering (anthropic-messages.ts:261-553)
// =============================================================================

/// Port of `lowerToolChoice` (anthropic-messages.ts:268-274). `none` returns
/// `None` (and the caller omits `tools`).
fn lower_tool_choice(choice: &ToolChoice) -> Option<AnthropicToolChoice> {
    match choice {
        ToolChoice::Auto => Some(AnthropicToolChoice::Auto),
        ToolChoice::None => None,
        ToolChoice::Required => Some(AnthropicToolChoice::Any),
        ToolChoice::Tool { name } => Some(AnthropicToolChoice::Tool { name: name.clone() }),
    }
}

/// Build an image block from base64 media. Port of `lowerImage`
/// (anthropic-messages.ts:307-321) with lighter base64 validation.
fn lower_image(media_type: &str, data: &str) -> Result<ContentBlock, LLMError> {
    let mime = media_type.to_lowercase();
    if !IMAGE_MIMES.contains(&mime.as_str()) {
        return Err(LLMError::Validation(format!(
            "Anthropic Messages does not support media type {media_type}"
        )));
    }
    // Strip an optional `data:...;base64,` prefix.
    let base64 = match data.find(";base64,") {
        Some(idx) => &data[idx + ";base64,".len()..],
        None => data,
    };
    Ok(ContentBlock::Image {
        source: ImageSource {
            kind: "base64",
            media_type: mime,
            data: base64.to_string(),
        },
        cache_control: None,
    })
}

/// Stringify a JSON value the way `String(value)` / `encodeJson(value)` would
/// (anthropic-messages.ts:213-223 in shared.ts). Strings are returned verbatim;
/// everything else is JSON-encoded.
fn loose_string(value: &Json) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Port of `toolResultText` (shared.ts:213-223).
fn tool_result_text(result: &otto_events::ToolResultValue) -> String {
    use otto_events::ToolResultValue as R;
    match result {
        R::Text { value } | R::Error { value } => loose_string(value),
        R::Json { value } => serde_json::to_string(value).unwrap_or_default(),
        R::Content { .. } => String::new(),
    }
}

/// Lower one structured tool-result content item. Port of
/// `lowerToolResultContentItem` (anthropic-messages.ts:325-342), simplified:
/// text passes through; anything carrying a `mime`/`mediaType` + `data`/`uri`
/// becomes an image; otherwise it errors.
fn lower_tool_content_item(item: &Json) -> Result<ContentBlock, LLMError> {
    if item.get("type").and_then(Value::as_str) == Some("text") {
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Ok(ContentBlock::text(text, None));
    }
    let mime = item
        .get("mime")
        .or_else(|| item.get("mediaType"))
        .and_then(Value::as_str);
    let data = item
        .get("uri")
        .or_else(|| item.get("data"))
        .and_then(Value::as_str);
    match (mime, data) {
        (Some(mime), Some(data)) => lower_image(mime, data),
        _ => Err(LLMError::Validation(
            "Anthropic Messages tool result content item must be text or image".to_string(),
        )),
    }
}

/// Port of `lowerToolResultContent` (anthropic-messages.ts:344-351).
fn lower_tool_result_content(
    result: &otto_events::ToolResultValue,
) -> Result<ToolResultContent, LLMError> {
    if let otto_events::ToolResultValue::Content { value } = result {
        let mut blocks = Vec::with_capacity(value.len());
        for item in value {
            blocks.push(lower_tool_content_item(item)?);
        }
        Ok(ToolResultContent::Blocks(blocks))
    } else {
        Ok(ToolResultContent::Text(tool_result_text(result)))
    }
}

/// XML-escape system-update text so it cannot close the wrapper
/// (shared.ts:111-112).
fn escape_system_update(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Wrap chronological system text into visible lower-authority user text. Port
/// of `wrapSystemUpdate` (shared.ts:120-121).
fn wrap_system_update(joined: &str) -> String {
    format!(
        "<system-update>\n{}\n</system-update>",
        escape_system_update(joined)
    )
}

/// Lower all messages. Port of `lowerMessages` (anthropic-messages.ts:402-489).
///
/// TODO: the `claude-opus-4-8` native mid-conversation `system` role
/// (`lowerNativeSystemUpdate`) and the `splitsLocalToolResults` guard are not
/// ported — every system-role message takes the common wrapped-user path.
fn lower_messages(
    req: &LLMRequest,
    breakpoints: &mut Breakpoints,
) -> Result<Vec<AnthropicMessage>, LLMError> {
    let mut messages: Vec<AnthropicMessage> = Vec::new();

    for message in &req.messages {
        match message.role {
            Role::System => {
                // Collect text-only content, then wrap it as visible user text.
                let mut joined = String::new();
                let mut last_cache: Option<CacheHint> = None;
                for part in &message.content {
                    match part {
                        ContentPart::Text { text, cache } => {
                            if !joined.is_empty() {
                                joined.push('\n');
                            }
                            joined.push_str(text);
                            last_cache = cache.clone();
                        }
                        _ => {
                            return Err(LLMError::Validation(
                                "Anthropic Messages system messages only support text content"
                                    .to_string(),
                            ));
                        }
                    }
                }
                let block = ContentBlock::text(
                    wrap_system_update(&joined),
                    cache_control(breakpoints, last_cache.as_ref()),
                );
                // Append to a trailing user turn if present, else push a new one.
                if let Some(AnthropicMessage::User { content }) = messages.last_mut() {
                    content.push(block);
                } else {
                    messages.push(AnthropicMessage::User {
                        content: vec![block],
                    });
                }
            }
            Role::User => {
                let mut content = Vec::with_capacity(message.content.len());
                for part in &message.content {
                    match part {
                        ContentPart::Text { text, cache } => content.push(ContentBlock::text(
                            text.clone(),
                            cache_control(breakpoints, cache.as_ref()),
                        )),
                        ContentPart::Media {
                            media_type, data, ..
                        } => content.push(lower_image(media_type, data)?),
                        _ => {
                            return Err(LLMError::Validation(
                                "Anthropic Messages user messages only support text and media \
                                 content"
                                    .to_string(),
                            ));
                        }
                    }
                }
                messages.push(AnthropicMessage::User { content });
            }
            Role::Assistant => {
                let mut content = Vec::with_capacity(message.content.len());
                for part in &message.content {
                    match part {
                        ContentPart::Text { text, cache } => content.push(ContentBlock::text(
                            text.clone(),
                            cache_control(breakpoints, cache.as_ref()),
                        )),
                        ContentPart::Reasoning { text, encrypted } => {
                            content.push(ContentBlock::Thinking {
                                thinking: text.clone(),
                                signature: encrypted.clone(),
                            });
                        }
                        ContentPart::ToolCall {
                            id,
                            name,
                            input,
                            provider_executed,
                        } => {
                            let block = if *provider_executed == Some(true) {
                                ContentBlock::ServerToolUse {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                }
                            } else {
                                ContentBlock::ToolUse {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                }
                            };
                            content.push(block);
                        }
                        ContentPart::ToolResult {
                            provider_executed, ..
                        } if *provider_executed == Some(true) => {
                            // TODO: round-trip server_tool_result blocks
                            // (web_search / code_execution / web_fetch); see
                            // anthropic-messages.ts:300-305.
                            return Err(LLMError::Validation(
                                "Anthropic Messages server tool results are not yet supported"
                                    .to_string(),
                            ));
                        }
                        _ => {
                            return Err(LLMError::Validation(
                                "Anthropic Messages assistant messages only support text, \
                                 reasoning, and tool-call content"
                                    .to_string(),
                            ));
                        }
                    }
                }
                messages.push(AnthropicMessage::Assistant { content });
            }
            Role::Tool => {
                let mut content = Vec::with_capacity(message.content.len());
                for part in &message.content {
                    match part {
                        ContentPart::ToolResult {
                            id, result, cache, ..
                        } => content.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: lower_tool_result_content(result)?,
                            is_error: matches!(result, otto_events::ToolResultValue::Error { .. })
                                .then_some(true),
                            cache_control: cache_control(breakpoints, cache.as_ref()),
                        }),
                        _ => {
                            return Err(LLMError::Validation(
                                "Anthropic Messages tool messages only support tool-result content"
                                    .to_string(),
                            ));
                        }
                    }
                }
                messages.push(AnthropicMessage::User { content });
            }
        }
    }

    Ok(messages)
}

/// Read `providerOptions.anthropic.thinking`. Port of `lowerThinking`
/// (anthropic-messages.ts:493-504).
fn lower_thinking(req: &LLMRequest) -> Result<Option<AnthropicThinking>, LLMError> {
    let Some(thinking) = req
        .provider_options
        .as_ref()
        .and_then(|po| po.get("anthropic"))
        .and_then(|a| a.get("thinking"))
    else {
        return Ok(None);
    };
    if thinking.get("type").and_then(Value::as_str) != Some("enabled") {
        return Ok(None);
    }
    let budget = thinking
        .get("budgetTokens")
        .and_then(Value::as_u64)
        .or_else(|| thinking.get("budget_tokens").and_then(Value::as_u64));
    match budget {
        Some(budget_tokens) => Ok(Some(AnthropicThinking {
            kind: "enabled",
            budget_tokens,
        })),
        None => Err(LLMError::Validation(
            "Anthropic thinking provider option requires budgetTokens".to_string(),
        )),
    }
}

// =============================================================================
// Usage math (anthropic-messages.ts:558-617, shared.ts token helpers)
// =============================================================================

/// Port of `ProviderShared.sumTokens` (shared.ts:85-88): `None` only if every
/// value is `None`, else `None` counts as `0`.
fn sum_tokens(values: &[Option<u64>]) -> Option<u64> {
    if values.iter().all(Option::is_none) {
        return None;
    }
    Some(values.iter().map(|v| v.unwrap_or(0)).sum())
}

/// Port of `ProviderShared.totalTokens` (shared.ts:51-59).
fn total_tokens(input: Option<u64>, output: Option<u64>, total: Option<u64>) -> Option<u64> {
    if let Some(total) = total {
        return Some(total);
    }
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(input.unwrap_or(0) + output.unwrap_or(0))
}

/// Wrap provider metadata under the `anthropic` key. Port of
/// `anthropicMetadata` (anthropic-messages.ts:253).
fn anthropic_metadata(inner: Json) -> Json {
    json!({ "anthropic": inner })
}

/// Map a provider usage breakdown into the neutral [`Usage`]. Port of `mapUsage`
/// (anthropic-messages.ts:573-588): Anthropic's `input_tokens` is the
/// *non-cached* count, so the inclusive `inputTokens` is the sum of the
/// non-overlapping breakdown.
fn map_usage(usage: &AnthropicUsage) -> Usage {
    let non_cached = usage.input_tokens;
    let cache_read = usage.cache_read_input_tokens;
    let cache_write = usage.cache_creation_input_tokens;
    let input_tokens = sum_tokens(&[non_cached, cache_read, cache_write]);
    Usage {
        input_tokens,
        output_tokens: usage.output_tokens,
        non_cached_input_tokens: non_cached,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        reasoning_tokens: None,
        total_tokens: total_tokens(input_tokens, usage.output_tokens, None),
        provider_metadata: Some(anthropic_metadata(
            serde_json::to_value(usage).unwrap_or(Value::Null),
        )),
    }
}

/// Merge the `anthropic` sub-objects of two provider-metadata blobs (right-biased).
fn merge_anthropic_meta(left: &Option<Json>, right: &Option<Json>) -> Option<Json> {
    let extract = |m: &Option<Json>| -> Option<Map<String, Value>> {
        m.as_ref()
            .and_then(|v| v.get("anthropic"))
            .and_then(Value::as_object)
            .cloned()
    };
    if left.is_none() && right.is_none() {
        return None;
    }
    let mut map = extract(left).unwrap_or_default();
    if let Some(rhs) = extract(right) {
        for (k, v) in rhs {
            map.insert(k, v);
        }
    }
    Some(json!({ "anthropic": map }))
}

/// Right-biased usage merge with a recomputed inclusive `inputTokens`. Port of
/// `mergeUsage` (anthropic-messages.ts:595-617).
fn merge_usage(left: Option<Usage>, right: Option<Usage>) -> Option<Usage> {
    match (left, right) {
        (None, right) => right,
        (left, None) => left,
        (Some(left), Some(right)) => {
            let non_cached = right
                .non_cached_input_tokens
                .or(left.non_cached_input_tokens);
            let cache_read = right
                .cache_read_input_tokens
                .or(left.cache_read_input_tokens);
            let cache_write = right
                .cache_write_input_tokens
                .or(left.cache_write_input_tokens);
            let input_tokens = sum_tokens(&[non_cached, cache_read, cache_write]);
            let output_tokens = right.output_tokens.or(left.output_tokens);
            Some(Usage {
                input_tokens,
                output_tokens,
                non_cached_input_tokens: non_cached,
                cache_read_input_tokens: cache_read,
                cache_write_input_tokens: cache_write,
                reasoning_tokens: None,
                total_tokens: total_tokens(input_tokens, output_tokens, None),
                provider_metadata: merge_anthropic_meta(
                    &left.provider_metadata,
                    &right.provider_metadata,
                ),
            })
        }
    }
}

/// Map an Anthropic stop reason to a neutral [`FinishReason`]. Port of
/// `mapFinishReason` (anthropic-messages.ts:558-564).
fn map_finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("end_turn" | "stop_sequence" | "pause_turn") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        Some("refusal") => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

/// Anthropic context-overflow detection. Port of `isContextOverflow`
/// (provider-error.ts:26-27), implemented as case-insensitive substring checks
/// (no regex dependency).
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
// Stream reducer (anthropic-messages.ts:652-822)
// =============================================================================

impl ParserState {
    /// `message_start` → merge usage (anthropic-messages.ts:652-655).
    fn on_message_start(&mut self, event: &AnthropicEvent) -> Vec<LLMEvent> {
        if let Some(usage) = event
            .message
            .as_ref()
            .and_then(|m| m.usage.as_ref())
            .map(map_usage)
        {
            self.usage = merge_usage(self.usage.take(), Some(usage));
        }
        Vec::new()
    }

    /// `content_block_start` (anthropic-messages.ts:657-701).
    fn on_content_block_start(&mut self, event: &AnthropicEvent) -> Vec<LLMEvent> {
        let Some(block) = event.content_block.as_ref() else {
            return Vec::new();
        };
        let block_type = block.block_type.as_deref().unwrap_or_default();
        let idx0 = event.index.unwrap_or(0);

        // tool_use / server_tool_use: start the tool (needs an index).
        if (block_type == "tool_use" || block_type == "server_tool_use") && event.index.is_some() {
            let idx = idx0 as usize;
            let id = block.id.clone().unwrap_or_else(|| idx.to_string());
            let name = block.name.clone().unwrap_or_default();
            let provider_executed = (block_type == "server_tool_use").then_some(true);
            let mut out = self.lifecycle.step_start(0);
            out.extend(self.tools.start(idx, id, name, provider_executed, None));
            self.tool_indices.insert(idx);
            return out;
        }

        // text block with initial text → text delta. otto's lifecycle
        // `text_delta` does not auto-emit `step-start` (the TS version does), so
        // the protocol opens the step explicitly.
        if block_type == "text"
            && let Some(text) = block.text.as_ref().filter(|t| !t.is_empty())
        {
            let mut out = self.lifecycle.step_start(0);
            out.extend(
                self.lifecycle
                    .text_delta(&format!("text-{idx0}"), text.clone()),
            );
            return out;
        }

        // thinking block with initial text → reasoning delta.
        if block_type == "thinking"
            && let Some(thinking) = block.thinking.as_ref().filter(|t| !t.is_empty())
        {
            let mut out = self.lifecycle.step_start(0);
            out.extend(
                self.lifecycle
                    .reasoning_delta(&format!("reasoning-{idx0}"), thinking.clone()),
            );
            return out;
        }

        // server_tool_result blocks are TODO-stubbed (no LLMEvent emitted); see
        // anthropic-messages.ts:632-701.
        Vec::new()
    }

    /// `content_block_delta` (anthropic-messages.ts:703-761).
    fn on_content_block_delta(
        &mut self,
        event: &AnthropicEvent,
    ) -> Result<Vec<LLMEvent>, LLMError> {
        let Some(delta) = event.delta.as_ref() else {
            return Ok(Vec::new());
        };
        let idx0 = event.index.unwrap_or(0);
        match delta.delta_type.as_deref() {
            Some("text_delta") => {
                if let Some(text) = delta.text.as_ref().filter(|t| !t.is_empty()) {
                    let mut out = self.lifecycle.step_start(0);
                    out.extend(
                        self.lifecycle
                            .text_delta(&format!("text-{idx0}"), text.clone()),
                    );
                    return Ok(out);
                }
                Ok(Vec::new())
            }
            Some("thinking_delta") => {
                if let Some(thinking) = delta.thinking.as_ref().filter(|t| !t.is_empty()) {
                    let mut out = self.lifecycle.step_start(0);
                    out.extend(
                        self.lifecycle
                            .reasoning_delta(&format!("reasoning-{idx0}"), thinking.clone()),
                    );
                    return Ok(out);
                }
                Ok(Vec::new())
            }
            Some("signature_delta") => {
                let Some(signature) = delta.signature.as_ref().filter(|s| !s.is_empty()) else {
                    return Ok(Vec::new());
                };
                // Close the reasoning block, attaching the signature to the
                // emitted reasoning-end (otto's `reasoning_end` takes no
                // metadata, so we patch it here). Port of
                // anthropic-messages.ts:728-742.
                let mut events = self.lifecycle.reasoning_end(&format!("reasoning-{idx0}"));
                let meta = anthropic_metadata(json!({ "signature": signature }));
                for event in &mut events {
                    if let LLMEvent::ReasoningEnd {
                        provider_metadata, ..
                    } = event
                    {
                        *provider_metadata = Some(meta.clone());
                    }
                }
                Ok(events)
            }
            Some("input_json_delta") if event.index.is_some() => {
                let Some(partial) = delta.partial_json.as_ref().filter(|p| !p.is_empty()) else {
                    return Ok(Vec::new());
                };
                let idx = idx0 as usize;
                // `step_start` is a no-op once started (the tool's start event
                // already opened the step). Port of anthropic-messages.ts:755.
                let mut out = self.lifecycle.step_start(0);
                out.extend(self.tools.append_existing(&idx, partial)?);
                Ok(out)
            }
            _ => Ok(Vec::new()),
        }
    }

    /// `content_block_stop` (anthropic-messages.ts:763-780).
    fn on_content_block_stop(&mut self, event: &AnthropicEvent) -> Result<Vec<LLMEvent>, LLMError> {
        let Some(index) = event.index else {
            return Ok(Vec::new());
        };
        let idx = index as usize;
        if self.tool_indices.remove(&idx) {
            let mut out = self.lifecycle.step_start(0);
            out.extend(self.tools.finish(&idx)?);
            Ok(out)
        } else {
            // Not a tool: close the text then reasoning block at this index.
            let mut out = self.lifecycle.text_end(&format!("text-{idx}"));
            out.extend(self.lifecycle.reasoning_end(&format!("reasoning-{idx}")));
            Ok(out)
        }
    }

    /// `message_delta` → finish (anthropic-messages.ts:782-793).
    fn on_message_delta(&mut self, event: &AnthropicEvent) -> Vec<LLMEvent> {
        let mapped = event.usage.as_ref().map(map_usage);
        let usage = merge_usage(self.usage.take(), mapped);
        let reason = map_finish_reason(event.delta.as_ref().and_then(|d| d.stop_reason.as_deref()));
        let stop_sequence = event.delta.as_ref().and_then(|d| d.stop_sequence.clone());

        let mut out = self.lifecycle.finish(reason, usage.clone(), 0);
        if let Some(stop_sequence) = stop_sequence {
            let meta = anthropic_metadata(json!({ "stopSequence": stop_sequence }));
            for event in &mut out {
                match event {
                    LLMEvent::StepFinish {
                        provider_metadata, ..
                    }
                    | LLMEvent::Finish {
                        provider_metadata, ..
                    } => *provider_metadata = Some(meta.clone()),
                    _ => {}
                }
            }
        }
        self.usage = usage;
        out
    }

    /// `error` → `provider-error` (anthropic-messages.ts:797-812).
    fn on_error(&self, event: &AnthropicEvent) -> Vec<LLMEvent> {
        let error_type = event.error.as_ref().and_then(|e| e.error_type.as_deref());
        let error_message = event.error.as_ref().and_then(|e| e.message.as_deref());
        let message = match (error_type, error_message) {
            (Some(t), Some(m)) => format!("{t}: {m}"),
            _ => error_message
                .or(error_type)
                .unwrap_or("Anthropic Messages stream error")
                .to_string(),
        };
        let classification = is_context_overflow(error_message.unwrap_or_default())
            .then_some(ProviderFailureClassification::ContextOverflow);
        vec![LLMEvent::ProviderError {
            message,
            classification,
            retryable: None,
            provider_metadata: None,
        }]
    }
}

impl Protocol for AnthropicMessages {
    type Body = AnthropicMessagesBody;
    type Event = AnthropicEvent;
    type State = ParserState;

    fn id(&self) -> &'static str {
        "anthropic"
    }

    /// Build the request body. Port of `fromRequest` (anthropic-messages.ts:506-553).
    fn build_body(&self, req: &LLMRequest) -> Result<Self::Body, LLMError> {
        let mut breakpoints = Breakpoints::new(ANTHROPIC_BREAKPOINT_CAP);

        let tool_choice = req.tool_choice.as_ref().and_then(lower_tool_choice);

        // tools: omitted when empty or when tool_choice is `none`.
        let tools = if req.tools.is_empty() || matches!(req.tool_choice, Some(ToolChoice::None)) {
            None
        } else {
            Some(
                req.tools
                    .iter()
                    .map(|tool| AnthropicTool {
                        name: tool.name.clone(),
                        description: tool.description.clone().unwrap_or_default(),
                        input_schema: tool.input_schema.clone(),
                        cache_control: cache_control(&mut breakpoints, tool.cache.as_ref()),
                    })
                    .collect(),
            )
        };

        let system = if req.system.is_empty() {
            None
        } else {
            Some(
                req.system
                    .iter()
                    .map(|part| {
                        ContentBlock::text(
                            part.text.clone(),
                            cache_control(&mut breakpoints, part.cache.as_ref()),
                        )
                    })
                    .collect(),
            )
        };

        let messages = lower_messages(req, &mut breakpoints)?;

        let generation = req.generation.as_ref();
        let output_limit = req.model.limits.output.unwrap_or(4096);
        let max_tokens = generation
            .and_then(|g| g.max_tokens)
            .unwrap_or(output_limit);
        let stop_sequences = generation
            .map(|g| g.stop.clone())
            .filter(|stop| !stop.is_empty());

        Ok(AnthropicMessagesBody {
            model: req.model.id.0.clone(),
            system,
            messages,
            tools,
            tool_choice,
            stream: true,
            max_tokens,
            temperature: if req.model.capabilities.temperature {
                generation.and_then(|g| g.temperature)
            } else {
                None
            },
            top_p: generation.and_then(|g| g.top_p),
            top_k: generation.and_then(|g| g.top_k),
            stop_sequences,
            thinking: lower_thinking(req)?,
        })
    }

    fn decode_event(&self, frame: &str) -> Result<Self::Event, LLMError> {
        serde_json::from_str(frame)
            .map_err(|e| LLMError::EventDecode(format!("invalid Anthropic event: {e}")))
    }

    fn initial(&self, _req: &LLMRequest) -> Self::State {
        ParserState::default()
    }

    /// Fold one event into the neutral event stream. Port of `step`
    /// (anthropic-messages.ts:814-822). `message_stop` / `ping` / unknown emit
    /// nothing.
    fn step(&self, state: &mut Self::State, event: Self::Event) -> Result<Vec<LLMEvent>, LLMError> {
        match event.event_type.as_str() {
            "message_start" => Ok(state.on_message_start(&event)),
            "content_block_start" => Ok(state.on_content_block_start(&event)),
            "content_block_delta" => state.on_content_block_delta(&event),
            "content_block_stop" => state.on_content_block_stop(&event),
            "message_delta" => Ok(state.on_message_delta(&event)),
            "error" => Ok(state.on_error(&event)),
            _ => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, SystemPart, ToolDefinition};
    use crate::model::Model;
    use crate::request::GenerationOptions;
    use otto_events::ToolResultValue;

    /// Flatten an event slice into its kebab-case type tags.
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

    /// Feed a scripted sequence of SSE `data:` payloads through
    /// `decode_event` + `step`, returning the flattened event list.
    fn run(frames: &[&str]) -> Vec<LLMEvent> {
        let proto = AnthropicMessages;
        let req = LLMRequest::new(
            Model::new("anthropic", "claude-sonnet-4", "anthropic"),
            vec![Message::user(vec![ContentPart::text("hi")])],
        );
        let mut state = proto.initial(&req);
        let mut out = Vec::new();
        for frame in frames {
            let event = proto.decode_event(frame).expect("decode");
            out.extend(proto.step(&mut state, event).expect("step"));
        }
        out
    }

    #[test]
    fn text_golden_sequence() {
        let frames = [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            r#"{"type":"message_stop"}"#,
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
        // Usage is merged: input from message_start, output from message_delta.
        match events.last().unwrap() {
            LLMEvent::Finish {
                reason,
                usage: Some(usage),
                ..
            } => {
                assert_eq!(*reason, FinishReason::Stop);
                assert_eq!(usage.input_tokens, Some(10));
                assert_eq!(usage.output_tokens, Some(5));
                assert!(usage.invariant_holds());
            }
            other => panic!("expected finish with usage, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_golden_sequence() {
        let frames = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"get_weather"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"paris\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
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
                LLMEvent::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .expect("tool-call");
        assert_eq!(tool_call.0, "get_weather");
        assert_eq!(tool_call.1["city"], "paris");
        match events.last().unwrap() {
            LLMEvent::Finish { reason, .. } => assert_eq!(*reason, FinishReason::ToolCalls),
            other => panic!("expected finish, got {other:?}"),
        }
    }

    #[test]
    fn thinking_golden_sequence_with_signature() {
        let frames = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"Let me think"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" more"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig123"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
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
        // The reasoning-end carries the signature metadata.
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
        assert_eq!(signature["anthropic"]["signature"], "sig123");
    }

    #[test]
    fn error_event_flags_context_overflow() {
        let frames = [
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens"}}"#,
        ];
        let events = run(&frames);
        match events.as_slice() {
            [
                LLMEvent::ProviderError {
                    message,
                    classification,
                    ..
                },
            ] => {
                assert!(message.contains("prompt is too long"));
                assert_eq!(
                    *classification,
                    Some(ProviderFailureClassification::ContextOverflow)
                );
            }
            other => panic!("expected single provider-error, got {other:?}"),
        }
    }

    fn body_for(req: &LLMRequest) -> Value {
        let proto = AnthropicMessages;
        serde_json::to_value(proto.build_body(req).expect("build_body")).expect("serialize")
    }

    #[test]
    fn tool_choice_mapping() {
        let tool = ToolDefinition {
            name: "t".into(),
            description: Some("d".into()),
            input_schema: json!({"type":"object"}),
            output_schema: None,
            cache: None,
        };
        let base = || {
            let mut req = LLMRequest::new(
                Model::new("anthropic", "claude-sonnet-4", "anthropic"),
                vec![Message::user(vec![ContentPart::text("hi")])],
            );
            req.tools = vec![tool.clone()];
            req
        };

        let mut auto = base();
        auto.tool_choice = Some(ToolChoice::Auto);
        assert_eq!(body_for(&auto)["tool_choice"], json!({"type":"auto"}));

        let mut required = base();
        required.tool_choice = Some(ToolChoice::Required);
        assert_eq!(body_for(&required)["tool_choice"], json!({"type":"any"}));

        let mut named = base();
        named.tool_choice = Some(ToolChoice::Tool { name: "t".into() });
        assert_eq!(
            body_for(&named)["tool_choice"],
            json!({"type":"tool","name":"t"})
        );

        // `none` omits both tool_choice and tools.
        let mut none = base();
        none.tool_choice = Some(ToolChoice::None);
        let body = body_for(&none);
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn max_tokens_fallback() {
        // No generation, no model output limit → 4096.
        let req = LLMRequest::new(
            Model::new("anthropic", "claude-sonnet-4", "anthropic"),
            vec![Message::user(vec![ContentPart::text("hi")])],
        );
        assert_eq!(body_for(&req)["max_tokens"], 4096);

        // Model output limit is used when generation is absent.
        let mut with_limit = req.clone();
        with_limit.model.limits.output = Some(8192);
        assert_eq!(body_for(&with_limit)["max_tokens"], 8192);

        // Explicit generation.max_tokens wins.
        let mut with_gen = with_limit;
        with_gen.generation = Some(GenerationOptions {
            max_tokens: Some(1024),
            ..GenerationOptions::default()
        });
        assert_eq!(body_for(&with_gen)["max_tokens"], 1024);
    }

    #[test]
    fn temperature_gated_on_model_capability() {
        // Capability off (e.g. a reasoning model) → temperature must be
        // omitted even when the caller asked for one.
        let mut req = LLMRequest::new(
            Model::new("anthropic", "claude-sonnet-4", "anthropic"),
            vec![Message::user(vec![ContentPart::text("hi")])],
        );
        req.generation = Some(GenerationOptions {
            temperature: Some(0.7),
            ..GenerationOptions::default()
        });
        req.model.capabilities.temperature = false;
        let body = body_for(&req);
        assert!(body.get("temperature").is_none());

        // Capability on → temperature passes through.
        req.model.capabilities.temperature = true;
        let body = body_for(&req);
        assert_eq!(body["temperature"], json!(0.7));
    }

    #[test]
    fn message_lowering_round_trip() {
        let mut req = LLMRequest::new(
            Model::new("anthropic", "claude-sonnet-4", "anthropic"),
            vec![
                Message::user(vec![ContentPart::text("what is the weather?")]),
                Message::tool(vec![ContentPart::ToolResult {
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    result: ToolResultValue::Text {
                        value: Value::String("sunny".into()),
                    },
                    provider_executed: None,
                    cache: None,
                }]),
            ],
        );
        req.system = vec![SystemPart::new("be terse")];

        let body = body_for(&req);
        assert_eq!(body["system"], json!([{"type":"text","text":"be terse"}]));
        assert_eq!(
            body["messages"],
            json!([
                {"role":"user","content":[{"type":"text","text":"what is the weather?"}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"sunny"}]}
            ])
        );
    }

    #[test]
    fn error_tool_result_sets_is_error() {
        let req = LLMRequest::new(
            Model::new("anthropic", "claude-sonnet-4", "anthropic"),
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
        assert_eq!(body["messages"][0]["content"][0]["is_error"], true);
        assert_eq!(body["messages"][0]["content"][0]["content"], "kaboom");
    }

    #[test]
    fn cache_control_breakpoint_allocation() {
        let cached_tool = |name: &str| ToolDefinition {
            name: name.into(),
            description: Some("d".into()),
            input_schema: json!({"type":"object"}),
            output_schema: None,
            cache: Some(CacheHint {
                kind: crate::request::CacheKind::Ephemeral,
                ttl_seconds: None,
            }),
        };
        let cached_text = || ContentPart::Text {
            text: "x".into(),
            cache: Some(CacheHint {
                kind: crate::request::CacheKind::Ephemeral,
                ttl_seconds: Some(3600),
            }),
        };

        let mut req = LLMRequest::new(
            Model::new("anthropic", "claude-sonnet-4", "anthropic"),
            vec![
                Message::user(vec![cached_text(), cached_text()]),
                Message::user(vec![cached_text(), cached_text()]),
            ],
        );
        // 2 tools + 2 system + 4 messages = 8 cache hints, capped at 4.
        req.tools = vec![cached_tool("a"), cached_tool("b")];
        req.system = vec![
            SystemPart {
                text: "s1".into(),
                cache: Some(CacheHint {
                    kind: crate::request::CacheKind::Ephemeral,
                    ttl_seconds: None,
                }),
            },
            SystemPart {
                text: "s2".into(),
                cache: Some(CacheHint {
                    kind: crate::request::CacheKind::Ephemeral,
                    ttl_seconds: None,
                }),
            },
        ];

        let body = body_for(&req);
        let text = serde_json::to_string(&body).unwrap();
        let emitted = text.matches("\"cache_control\"").count();
        assert_eq!(emitted, ANTHROPIC_BREAKPOINT_CAP as usize);

        // The budget is allocated tools → system → messages, so both tools and
        // both system parts get markers and the message tail is dropped.
        assert!(body["tools"][0].get("cache_control").is_some());
        assert!(body["tools"][1].get("cache_control").is_some());
        assert!(body["system"][0].get("cache_control").is_some());
        assert!(body["system"][1].get("cache_control").is_some());
        // ttl bucket: 1h for >= 3600s, omitted (5m) otherwise.
        assert!(body["tools"][0]["cache_control"].get("ttl").is_none());
    }
}
