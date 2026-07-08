//! The Google Gemini wire protocol.
//!
//! Faithful port of opencode `packages/llm/src/protocols/gemini.ts`. This
//! module covers both request-body construction ([`Gemini::build_body`],
//! porting `fromRequest` + `lowerMessages`, gemini.ts:108-136 / 205-318) and
//! the streaming-event reducer ([`Gemini::step`] / [`Gemini::on_halt`],
//! porting `step` / `finish`, gemini.ts:399-476). Unlike Anthropic
//! (`message_delta`) and OpenAI Chat (a terminal `finish_reason` in-band),
//! Gemini's `step` never itself emits `step-finish`/`finish`: it only
//! accumulates `finishReason`/usage into [`ParserState`], and
//! [`Gemini::on_halt`] (the reducer's `onHalt`, called once the frame stream
//! ends) turns that into the actual finish events — matching gemini.ts's
//! `onHalt: finish` wiring exactly.
//!
//! Line references throughout point at the TypeScript source of truth.

use otto_events::{FinishReason, Json, LLMEvent, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::LLMError;
use crate::message::{ContentPart, Role, ToolChoice, ToolDefinition};
use crate::protocol::Protocol;
use crate::protocols::utils::gemini_tool_schema;
use crate::protocols::utils::{lifecycle, tool_stream};
use crate::request::LLMRequest;

/// Protocol id (`ADAPTER` in gemini.ts:25).
const ADAPTER: &str = "gemini";

/// MIME types Gemini accepts as inline media (`ProviderShared.MEDIA_MIMES`
/// restricted to the image subset otto currently ports; see
/// `anthropic_messages::IMAGE_MIMES` / `openai_chat::IMAGE_MIMES` for the same
/// simplification in the sibling protocols).
const MEDIA_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

// =============================================================================
// Request Body Schema (gemini.ts:32-116)
// =============================================================================

/// `{ mimeType, data }` — inline base64 media (gemini.ts:38-43).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeminiInlineData {
    /// The MIME type, e.g. `image/png`.
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    /// The base64-encoded payload.
    pub data: String,
}

/// `{ name, args }` — a model-issued function call (gemini.ts:45-51).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeminiFunctionCall {
    /// The called function's name.
    pub name: String,
    /// The call arguments.
    pub args: Json,
}

/// `{ name, response }` — the result of a function call (gemini.ts:53-58).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeminiFunctionResponse {
    /// The responding function's name.
    pub name: String,
    /// The structured response payload.
    pub response: Json,
}

/// A `Content.parts[]` entry (`GeminiContentPart`, gemini.ts:32-65). Untagged:
/// Gemini's dialect distinguishes variants by which key is present, not by an
/// explicit `type` discriminator. `Deserialize` is needed alongside
/// `Serialize` because this same type is reused to parse the streamed
/// response's `candidates[].content.parts[]` (gemini.ts:399-476).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GeminiPart {
    /// `{ text, thought?, thoughtSignature? }` (gemini.ts:32-36).
    Text {
        /// The text content.
        text: String,
        /// Whether this text is a "thought" (reasoning) segment.
        #[serde(skip_serializing_if = "Option::is_none")]
        thought: Option<bool>,
        /// Provider signature for the thought, if any.
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    /// `{ inlineData: { mimeType, data } }` (gemini.ts:38-43).
    InlineData {
        /// The inline media payload.
        #[serde(rename = "inlineData")]
        inline_data: GeminiInlineData,
    },
    /// `{ functionCall: { name, args }, thoughtSignature? }` (gemini.ts:45-51).
    FunctionCall {
        /// The called function.
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall,
        /// Provider signature carried alongside the call, if any.
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    /// `{ functionResponse: { name, response } }` (gemini.ts:53-58).
    FunctionResponse {
        /// The function-call result.
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponse,
    },
}

impl GeminiPart {
    /// Build a plain text part (no `thought`/`thoughtSignature`).
    fn text(text: impl Into<String>) -> Self {
        GeminiPart::Text {
            text: text.into(),
            thought: None,
            thought_signature: None,
        }
    }
}

/// A `Content` entry (`GeminiContent`, gemini.ts:67-71). `Deserialize` is
/// needed alongside `Serialize` because this same type is reused to parse the
/// streamed response's `candidates[].content` (gemini.ts:399-476). `role` is
/// an owned `String` (rather than `&'static str`) so the type can round-trip
/// through `#[derive(Deserialize)]` without a `'static` borrow; the reducer
/// never reads it back (Gemini responses only ever carry `"model"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeminiContent {
    /// `"user"` or `"model"`.
    pub role: String,
    /// The ordered content parts.
    pub parts: Vec<GeminiPart>,
}

/// `systemInstruction` (`GeminiSystemInstruction`, gemini.ts:73-75).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GeminiSystemInstruction {
    /// Text-only parts (no `thought`/media on system instructions).
    pub parts: Vec<GeminiTextOnlyPart>,
}

/// A bare `{ text }` part used only by `systemInstruction`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GeminiTextOnlyPart {
    /// The text content.
    pub text: String,
}

/// One tool's function declaration (`GeminiFunctionDeclaration`, gemini.ts:77-81).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GeminiFunctionDeclaration {
    /// Tool name.
    pub name: String,
    /// Human-readable description (empty string if absent; the wire field is
    /// required).
    pub description: String,
    /// The Gemini-projected JSON Schema for the tool input, omitted for
    /// degenerate (empty-object) schemas.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Json>,
}

/// `{ functionDeclarations }` (`GeminiTool`, gemini.ts:83-85).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GeminiToolDecls {
    /// The declared functions.
    #[serde(rename = "functionDeclarations")]
    pub function_declarations: Vec<GeminiFunctionDeclaration>,
}

/// `functionCallingConfig` (part of `GeminiToolConfig`, gemini.ts:87-92).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GeminiFunctionCallingConfig {
    /// `"AUTO"` | `"NONE"` | `"ANY"`.
    pub mode: &'static str,
    /// Restrict `ANY` mode to a named subset of tools.
    #[serde(
        rename = "allowedFunctionNames",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_function_names: Option<Vec<String>>,
}

/// `toolConfig` (`GeminiToolConfig`, gemini.ts:87-92).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GeminiToolConfig {
    /// The function-calling policy.
    #[serde(rename = "functionCallingConfig")]
    pub function_calling_config: GeminiFunctionCallingConfig,
}

/// `thinkingConfig` (`GeminiThinkingConfig`, gemini.ts:94-97).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GeminiThinkingConfig {
    /// The reasoning token budget.
    #[serde(rename = "thinkingBudget", skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<f64>,
    /// Whether to include thought summaries in the stream.
    #[serde(rename = "includeThoughts", skip_serializing_if = "Option::is_none")]
    pub include_thoughts: Option<bool>,
}

/// `generationConfig` (`GeminiGenerationConfig`, gemini.ts:99-106).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GeminiGenerationConfig {
    /// Maximum output tokens.
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus-sampling probability mass.
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Top-k sampling cutoff.
    #[serde(rename = "topK", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u64>,
    /// Stop sequences.
    #[serde(rename = "stopSequences", skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// Extended-thinking configuration.
    #[serde(rename = "thinkingConfig", skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<GeminiThinkingConfig>,
}

impl GeminiGenerationConfig {
    /// Whether every field is `None` (`Object.values(..).some(v => v !==
    /// undefined)` in gemini.ts:329, negated).
    fn is_empty(&self) -> bool {
        self.max_output_tokens.is_none()
            && self.temperature.is_none()
            && self.top_p.is_none()
            && self.top_k.is_none()
            && self.stop_sequences.is_none()
            && self.thinking_config.is_none()
    }
}

/// The Gemini `generateContent` request body (`GeminiBody`, gemini.ts:108-116).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GeminiBody {
    /// The lowered conversation contents.
    pub contents: Vec<GeminiContent>,
    /// The top-level system instruction.
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiSystemInstruction>,
    /// The available tools (omitted when there are none, or `tool_choice` is
    /// `none`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GeminiToolDecls>>,
    /// The tool-calling policy.
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<GeminiToolConfig>,
    /// Sampling / generation knobs.
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GeminiGenerationConfig>,
}

// =============================================================================
// Request Lowering (gemini.ts:171-333)
// =============================================================================

/// Lower a tool definition. Port of `lowerTool` (gemini.ts:171-175).
fn lower_tool(tool: &ToolDefinition) -> GeminiFunctionDeclaration {
    let parameters = gemini_tool_schema::convert(&tool.input_schema);
    GeminiFunctionDeclaration {
        name: tool.name.clone(),
        description: tool.description.clone().unwrap_or_default(),
        parameters: (!parameters.is_null()).then_some(parameters),
    }
}

/// Lower the tool-choice policy. Port of `lowerToolConfig` (gemini.ts:177-183).
fn lower_tool_config(choice: &ToolChoice) -> GeminiToolConfig {
    let function_calling_config = match choice {
        ToolChoice::Auto => GeminiFunctionCallingConfig {
            mode: "AUTO",
            allowed_function_names: None,
        },
        ToolChoice::None => GeminiFunctionCallingConfig {
            mode: "NONE",
            allowed_function_names: None,
        },
        ToolChoice::Required => GeminiFunctionCallingConfig {
            mode: "ANY",
            allowed_function_names: None,
        },
        ToolChoice::Tool { name } => GeminiFunctionCallingConfig {
            mode: "ANY",
            allowed_function_names: Some(vec![name.clone()]),
        },
    };
    GeminiToolConfig {
        function_calling_config,
    }
}

/// Lower one media part into `inlineData`. Port of `lowerUserPart`'s media
/// branch (gemini.ts:185-189), with lighter base64/media validation (mirrors
/// `anthropic_messages::lower_image` / `openai_chat::lower_media`).
fn lower_inline_data(media_type: &str, data: &str) -> Result<GeminiPart, LLMError> {
    let mime = media_type.to_lowercase();
    if !MEDIA_MIMES.contains(&mime.as_str()) {
        return Err(LLMError::Validation(format!(
            "Gemini does not support media type {media_type}"
        )));
    }
    let base64 = match data.find(";base64,") {
        Some(idx) => &data[idx + ";base64,".len()..],
        None => data,
    };
    Ok(GeminiPart::InlineData {
        inline_data: GeminiInlineData {
            mime_type: mime,
            data: base64.to_string(),
        },
    })
}

/// `String(value)`-style rendering of a tool-result value's JSON payload.
/// Port of `ProviderShared.toolResultText`'s text/error handling
/// (shared.ts:213-223), reused verbatim from `anthropic_messages::loose_string`.
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

/// Lower one non-text `content` tool-result item into an `inlineData` part.
/// Port of the `ProviderShared.validateToolFile` call inside `lowerMessages`'s
/// tool-result branch (gemini.ts:278-282).
fn lower_tool_file_part(item: &Json) -> Result<GeminiPart, LLMError> {
    let mime = item.get("mime").and_then(Value::as_str);
    let uri = item.get("uri").and_then(Value::as_str);
    match (mime, uri) {
        (Some(mime), Some(uri)) => lower_inline_data(mime, uri),
        _ => Err(LLMError::Validation(
            "Gemini tool result file content requires mime and uri".to_string(),
        )),
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

/// Lower all messages. Port of `lowerMessages` (gemini.ts:205-288).
fn lower_messages(req: &LLMRequest) -> Result<Vec<GeminiContent>, LLMError> {
    let mut contents: Vec<GeminiContent> = Vec::new();

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
                                "Gemini system messages only support text content".to_string(),
                            ));
                        }
                    }
                }
                let text = wrap_system_update(&joined);
                match contents.last_mut() {
                    Some(GeminiContent { role, parts }) if role == "user" => {
                        parts.push(GeminiPart::text(text));
                    }
                    _ => contents.push(GeminiContent {
                        role: "user".to_string(),
                        parts: vec![GeminiPart::text(text)],
                    }),
                }
            }
            Role::User => {
                let mut parts = Vec::with_capacity(message.content.len());
                for part in &message.content {
                    match part {
                        ContentPart::Text { text, .. } => {
                            parts.push(GeminiPart::text(text.clone()))
                        }
                        ContentPart::Media {
                            media_type, data, ..
                        } => parts.push(lower_inline_data(media_type, data)?),
                        _ => {
                            return Err(LLMError::Validation(
                                "Gemini user messages only support text and media content"
                                    .to_string(),
                            ));
                        }
                    }
                }
                contents.push(GeminiContent {
                    role: "user".to_string(),
                    parts,
                });
            }
            Role::Assistant => {
                let mut parts = Vec::with_capacity(message.content.len());
                for part in &message.content {
                    match part {
                        ContentPart::Text { text, .. } => {
                            parts.push(GeminiPart::text(text.clone()))
                        }
                        ContentPart::Reasoning { text, encrypted } => {
                            parts.push(GeminiPart::Text {
                                text: text.clone(),
                                thought: Some(true),
                                thought_signature: encrypted.clone(),
                            })
                        }
                        ContentPart::ToolCall { name, input, .. } => {
                            parts.push(GeminiPart::FunctionCall {
                                function_call: GeminiFunctionCall {
                                    name: name.clone(),
                                    args: input.clone(),
                                },
                                thought_signature: None,
                            });
                        }
                        _ => {
                            return Err(LLMError::Validation(
                                "Gemini assistant messages only support text, reasoning, and \
                                 tool-call content"
                                    .to_string(),
                            ));
                        }
                    }
                }
                contents.push(GeminiContent {
                    role: "model".to_string(),
                    parts,
                });
            }
            Role::Tool => {
                let mut parts = Vec::new();
                for part in &message.content {
                    let ContentPart::ToolResult { name, result, .. } = part else {
                        return Err(LLMError::Validation(
                            "Gemini tool messages only support tool-result content".to_string(),
                        ));
                    };
                    if let otto_events::ToolResultValue::Content { value } = result {
                        let text: Vec<&str> = value
                            .iter()
                            .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                            .filter_map(|item| item.get("text").and_then(Value::as_str))
                            .collect();
                        parts.push(GeminiPart::FunctionResponse {
                            function_response: GeminiFunctionResponse {
                                name: name.clone(),
                                response: json!({ "name": name, "content": text.join("\n") }),
                            },
                        });
                        for item in value {
                            if item.get("type").and_then(Value::as_str) == Some("text") {
                                continue;
                            }
                            parts.push(lower_tool_file_part(item)?);
                        }
                    } else {
                        parts.push(GeminiPart::FunctionResponse {
                            function_response: GeminiFunctionResponse {
                                name: name.clone(),
                                response: json!({ "name": name, "content": tool_result_text(result) }),
                            },
                        });
                    }
                }
                contents.push(GeminiContent {
                    role: "user".to_string(),
                    parts,
                });
            }
        }
    }

    Ok(contents)
}

/// Read `providerOptions.gemini.thinkingConfig`. Port of `thinkingConfig`
/// (gemini.ts:292-300).
fn lower_thinking_config(req: &LLMRequest) -> Option<GeminiThinkingConfig> {
    let value = req
        .provider_options
        .as_ref()?
        .get("gemini")?
        .get("thinkingConfig")?;
    let thinking_budget = value.get("thinkingBudget").and_then(Value::as_f64);
    let include_thoughts = value.get("includeThoughts").and_then(Value::as_bool);
    if thinking_budget.is_none() && include_thoughts.is_none() {
        return None;
    }
    Some(GeminiThinkingConfig {
        thinking_budget,
        include_thoughts,
    })
}

// =============================================================================
// Streaming Event Schema (gemini.ts:118-136)
// =============================================================================

/// `usageMetadata` on a streamed chunk (`GeminiUsage`, gemini.ts:118-125).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiUsage {
    /// Total prompt tokens (inclusive of any cached subset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_token_count: Option<u64>,
    /// Visible (non-thought) output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidates_token_count: Option<u64>,
    /// Reasoning ("thought") output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thoughts_token_count: Option<u64>,
    /// The cached subset of `prompt_token_count`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_content_token_count: Option<u64>,
    /// Provider-reported total, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_token_count: Option<u64>,
}

/// One streamed candidate (`GeminiCandidate`, gemini.ts:127-130).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiCandidate {
    /// The candidate's content delta, if any.
    #[serde(default)]
    pub content: Option<GeminiContent>,
    /// The terminal finish reason, present on the final chunk.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// One decoded SSE `data:` payload (`GeminiEvent`, gemini.ts:132-136).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiEvent {
    /// The streamed candidates (only the first is consulted, matching
    /// gemini.ts:404 `event.candidates?.[0]`).
    #[serde(default)]
    pub candidates: Option<Vec<GeminiCandidate>>,
    /// Usage accompanying this chunk, if any.
    #[serde(default)]
    pub usage_metadata: Option<GeminiUsage>,
}

// =============================================================================
// Usage / finish-reason mapping (gemini.ts:338-377)
// =============================================================================

/// Wrap provider metadata under the `google` key (`googleMetadata`,
/// gemini.ts:191). Note this is the *provider* id (matching the `route:
/// provider: "google"` registration), distinct from the `gemini` protocol
/// [`ADAPTER`] id.
fn google_metadata(inner: Json) -> Json {
    json!({ "google": inner })
}

/// `Math.max(0, total - subtrahend)` token subtraction
/// (`ProviderShared.subtractTokens`, shared.ts:72-76).
fn subtract_tokens(total: Option<u64>, subtrahend: Option<u64>) -> Option<u64> {
    match (total, subtrahend) {
        (None, _) => None,
        (Some(total), None) => Some(total),
        (Some(total), Some(sub)) => Some(total.saturating_sub(sub)),
    }
}

/// Provider total, else `input + output` when at least one is present
/// (`ProviderShared.totalTokens`, shared.ts:51-59).
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
/// `mapUsage` (gemini.ts:342-361). Gemini's `candidatesTokenCount` is
/// *visible-only* (excludes `thoughtsTokenCount`), so the inclusive
/// `outputTokens` sums the two.
fn map_usage(usage: &GeminiUsage) -> Usage {
    let cached = usage.cached_content_token_count;
    let non_cached = subtract_tokens(usage.prompt_token_count, cached);
    let output_tokens = usage
        .candidates_token_count
        .map(|visible| visible + usage.thoughts_token_count.unwrap_or(0));
    Usage {
        input_tokens: usage.prompt_token_count,
        output_tokens,
        non_cached_input_tokens: non_cached,
        cache_read_input_tokens: cached,
        cache_write_input_tokens: None,
        reasoning_tokens: usage.thoughts_token_count,
        total_tokens: total_tokens(
            usage.prompt_token_count,
            output_tokens,
            usage.total_token_count,
        ),
        provider_metadata: Some(google_metadata(
            serde_json::to_value(usage).unwrap_or(Value::Null),
        )),
    }
}

/// Map a Gemini `finishReason` to a neutral [`FinishReason`]. Port of
/// `mapFinishReason` (gemini.ts:363-377).
fn map_finish_reason(reason: Option<&str>, has_tool_calls: bool) -> FinishReason {
    match reason {
        Some("STOP") => {
            if has_tool_calls {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            }
        }
        Some("MAX_TOKENS") => FinishReason::Length,
        Some(
            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY",
        ) => FinishReason::ContentFilter,
        Some("MALFORMED_FUNCTION_CALL") => FinishReason::Error,
        _ => FinishReason::Unknown,
    }
}

// =============================================================================
// Stream reducer (gemini.ts:138-145, 379-476)
// =============================================================================

/// Per-stream reducer state. Port of `ParserState` (gemini.ts:138-145).
///
/// Unlike Anthropic/OpenAI Chat, Gemini's tool calls arrive whole in a single
/// `functionCall` part — there is no incremental `tool-input-delta` phase —
/// so [`ParserState::tools`] is registered and finished back-to-back within
/// the same [`Gemini::step`] call rather than across several.
#[derive(Default)]
pub struct ParserState {
    /// The tool-call accumulator, keyed by a synthetic per-stream counter.
    tools: tool_stream::State<u64>,
    /// Merged usage seen so far (Gemini reports a running total per chunk, so
    /// this is a replace, not a merge; see [`Gemini::step`]).
    usage: Option<Usage>,
    /// The step/text/reasoning lifecycle machine.
    lifecycle: lifecycle::State,
    /// The most recently reported `finishReason`, carried until `on_halt`.
    finish_reason: Option<String>,
    /// Whether any `functionCall` part has been seen (`hasToolCalls`).
    has_tool_calls: bool,
    /// Monotonic counter used to synthesize `tool_{n}` ids.
    next_tool_call_id: u64,
    /// The most recently seen reasoning `thoughtSignature`, attached when the
    /// reasoning block is eventually closed (`reasoningSignature`).
    reasoning_signature: Option<String>,
}

impl ParserState {
    /// Close the (single) `reasoning-0` block, attaching the tracked
    /// `thoughtSignature` (if any) to the `reasoning-end` event. Port of the
    /// repeated `Lifecycle.reasoningEnd(..., reasoningSignature ?
    /// googleMetadata(...) : undefined)` call (gemini.ts:431-436, 444-449,
    /// 384-389). A no-op if no reasoning block is open.
    fn close_reasoning(&mut self) -> Vec<LLMEvent> {
        let mut events = self.lifecycle.reasoning_end("reasoning-0");
        if let Some(sig) = &self.reasoning_signature {
            let meta = google_metadata(json!({ "thoughtSignature": sig }));
            for event in &mut events {
                if let LLMEvent::ReasoningEnd {
                    provider_metadata, ..
                } = event
                {
                    *provider_metadata = Some(meta.clone());
                }
            }
        }
        events
    }
}

// =============================================================================
// Protocol
// =============================================================================

/// The Gemini protocol — request body construction and the streaming
/// reducer.
#[derive(Debug, Clone, Copy, Default)]
pub struct Gemini;

impl Protocol for Gemini {
    type Body = GeminiBody;
    type Event = GeminiEvent;
    type State = ParserState;

    fn id(&self) -> &'static str {
        ADAPTER
    }

    /// Build the request body. Port of `fromRequest` (gemini.ts:302-333).
    fn build_body(&self, req: &LLMRequest) -> Result<Self::Body, LLMError> {
        let tools_enabled =
            !req.tools.is_empty() && !matches!(req.tool_choice, Some(ToolChoice::None));

        let tools = tools_enabled.then(|| {
            vec![GeminiToolDecls {
                function_declarations: req.tools.iter().map(lower_tool).collect(),
            }]
        });

        let tool_config = if tools_enabled {
            req.tool_choice.as_ref().map(lower_tool_config)
        } else {
            None
        };

        let system_instruction = if req.system.is_empty() {
            None
        } else {
            let joined = req
                .system
                .iter()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            Some(GeminiSystemInstruction {
                parts: vec![GeminiTextOnlyPart { text: joined }],
            })
        };

        let contents = lower_messages(req)?;

        let generation = req.generation.as_ref();
        let generation_config = GeminiGenerationConfig {
            max_output_tokens: generation.and_then(|g| g.max_tokens),
            temperature: if req.model.capabilities.temperature {
                generation.and_then(|g| g.temperature)
            } else {
                None
            },
            top_p: generation.and_then(|g| g.top_p),
            top_k: generation.and_then(|g| g.top_k),
            stop_sequences: generation
                .map(|g| g.stop.clone())
                .filter(|stop| !stop.is_empty()),
            thinking_config: lower_thinking_config(req),
        };

        Ok(GeminiBody {
            contents,
            system_instruction,
            tools,
            tool_config,
            generation_config: (!generation_config.is_empty()).then_some(generation_config),
        })
    }

    fn decode_event(&self, frame: &str) -> Result<Self::Event, LLMError> {
        serde_json::from_str(frame)
            .map_err(|e| LLMError::EventDecode(format!("invalid Gemini event: {e}")))
    }

    fn initial(&self, _req: &LLMRequest) -> Self::State {
        ParserState::default()
    }

    /// Fold one streamed chunk into the neutral event stream. Port of `step`
    /// (gemini.ts:399-476). Note this never itself emits `step-finish`/
    /// `finish` — like the TS source, it only records `finishReason`/usage;
    /// [`Gemini::on_halt`] is what turns that into the actual finish events.
    fn step(&self, state: &mut Self::State, event: Self::Event) -> Result<Vec<LLMEvent>, LLMError> {
        // usage: replace with the latest reported usage, not merge
        // (gemini.ts:402 — `mapUsage` never returns `undefined` for a
        // present `usageMetadata`, so this always overwrites).
        if let Some(usage) = event.usage_metadata.as_ref().map(map_usage) {
            state.usage = Some(usage);
        }

        let Some(candidate) = event.candidates.and_then(|c| c.into_iter().next()) else {
            return Ok(Vec::new());
        };

        let mut events = Vec::new();

        if let Some(content) = candidate.content {
            for part in content.parts {
                // Track the latest reasoning signature regardless of text
                // length (gemini.ts:418-419).
                if let GeminiPart::Text {
                    thought: Some(true),
                    thought_signature: Some(sig),
                    ..
                } = &part
                {
                    state.reasoning_signature = Some(sig.clone());
                }

                match part {
                    GeminiPart::Text {
                        text,
                        thought,
                        thought_signature,
                    } if !text.is_empty() => {
                        if thought == Some(true) {
                            events.extend(state.lifecycle.step_start(0));
                            let mut delta_events =
                                state.lifecycle.reasoning_delta("reasoning-0", text);
                            if let Some(sig) = &thought_signature {
                                let meta = google_metadata(json!({ "thoughtSignature": sig }));
                                for event in &mut delta_events {
                                    if let LLMEvent::ReasoningStart {
                                        provider_metadata, ..
                                    } = event
                                    {
                                        *provider_metadata = Some(meta.clone());
                                    }
                                }
                            }
                            events.extend(delta_events);
                        } else {
                            events.extend(state.close_reasoning());
                            events.extend(state.lifecycle.step_start(0));
                            events.extend(state.lifecycle.text_delta("text-0", text));
                        }
                    }
                    GeminiPart::FunctionCall {
                        function_call,
                        thought_signature,
                    } => {
                        events.extend(state.close_reasoning());
                        events.extend(state.lifecycle.step_start(0));
                        let key = state.next_tool_call_id;
                        state.next_tool_call_id += 1;
                        let id = format!("tool_{key}");
                        let meta = thought_signature
                            .map(|sig| google_metadata(json!({ "thoughtSignature": sig })));
                        events.extend(state.tools.start(key, id, function_call.name, None, meta));
                        events.extend(state.tools.finish_with_input(&key, function_call.args)?);
                        state.has_tool_calls = true;
                    }
                    _ => {}
                }
            }
        }

        if let Some(reason) = candidate.finish_reason {
            state.finish_reason = Some(reason);
        }

        Ok(events)
    }

    /// Flush any dangling reasoning block and emit the finish events. Port of
    /// `finish` (gemini.ts:379-397), which is wired as `onHalt`.
    fn on_halt(&self, state: &mut Self::State) -> Vec<LLMEvent> {
        if state.finish_reason.is_none() && state.usage.is_none() {
            return Vec::new();
        }
        let mut events = state.close_reasoning();
        // Open the step if no content part did. `lifecycle::State::finish`
        // (unlike the TS `Lifecycle.finish`) does not fold in `step-start`, so
        // a content-less finish (e.g. an immediate `SAFETY` block with no
        // parts) would otherwise emit `[step-finish, finish]` with no matching
        // `step-start`. `step_start` is idempotent, so this is a no-op when a
        // content part already opened the step (mirrors the compensation in
        // `openai_chat.rs`'s `on_halt`).
        events.extend(state.lifecycle.step_start(0));
        let reason = map_finish_reason(state.finish_reason.as_deref(), state.has_tool_calls);
        events.extend(state.lifecycle.finish(reason, state.usage.clone(), 0));
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
        let proto = Gemini;
        serde_json::to_value(proto.build_body(req).expect("build_body")).expect("serialize")
    }

    fn base_request() -> LLMRequest {
        LLMRequest::new(
            Model::new("google", "gemini-2.5-pro", "gemini"),
            vec![Message::user(vec![ContentPart::text("hi")])],
        )
    }

    #[test]
    fn system_instruction_from_top_level_system() {
        let mut req = base_request();
        req.system = vec![SystemPart::new("be terse"), SystemPart::new("be kind")];
        let body = body_for(&req);
        assert_eq!(
            body["systemInstruction"],
            json!({ "parts": [{ "text": "be terse\nbe kind" }] })
        );
    }

    #[test]
    fn no_system_omits_system_instruction() {
        let body = body_for(&base_request());
        assert!(body.get("systemInstruction").is_none());
    }

    #[test]
    fn assistant_tool_call_lowers_to_model_role_function_call() {
        let req = LLMRequest::new(
            Model::new("google", "gemini-2.5-pro", "gemini"),
            vec![Message::assistant(vec![ContentPart::ToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: json!({"city": "paris"}),
                provider_executed: None,
            }])],
        );
        let body = body_for(&req);
        assert_eq!(body["contents"][0]["role"], "model");
        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"],
            json!({"name": "get_weather", "args": {"city": "paris"}})
        );
    }

    #[test]
    fn assistant_reasoning_lowers_to_thought_text() {
        let req = LLMRequest::new(
            Model::new("google", "gemini-2.5-pro", "gemini"),
            vec![Message::assistant(vec![ContentPart::reasoning(
                "thinking...",
            )])],
        );
        let body = body_for(&req);
        assert_eq!(
            body["contents"][0]["parts"][0],
            json!({"text": "thinking...", "thought": true})
        );
    }

    #[test]
    fn tool_result_lowers_to_user_role_function_response() {
        let req = LLMRequest::new(
            Model::new("google", "gemini-2.5-pro", "gemini"),
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
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(
            body["contents"][0]["parts"][0]["functionResponse"],
            json!({"name": "get_weather", "response": {"name": "get_weather", "content": "sunny"}})
        );
    }

    #[test]
    fn tools_lower_to_function_declarations() {
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
            body["tools"][0]["functionDeclarations"][0]["name"],
            "get_weather"
        );
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["description"],
            "Get the weather"
        );
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parameters"]["properties"]["city"]["type"],
            "string"
        );
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
    }

    #[test]
    fn tool_choice_none_omits_tools_and_config() {
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
        assert!(body.get("tools").is_none());
        assert!(body.get("toolConfig").is_none());
    }

    #[test]
    fn tool_choice_named_maps_to_any_with_allowed_names() {
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
            body["toolConfig"]["functionCallingConfig"],
            json!({"mode": "ANY", "allowedFunctionNames": ["t"]})
        );
    }

    #[test]
    fn temperature_gated_on_model_capability() {
        let mut req = base_request();
        req.generation = Some(GenerationOptions {
            temperature: Some(0.7),
            ..GenerationOptions::default()
        });
        req.model.capabilities.temperature = false;
        let body = body_for(&req);
        assert!(body.get("generationConfig").is_none());

        req.model.capabilities.temperature = true;
        let body = body_for(&req);
        assert_eq!(body["generationConfig"]["temperature"], json!(0.7));
    }

    #[test]
    fn mid_conversation_system_message_wraps_into_user_text() {
        let req = LLMRequest::new(
            Model::new("google", "gemini-2.5-pro", "gemini"),
            vec![
                Message::user(vec![ContentPart::text("hi")]),
                Message::system(vec![ContentPart::text("session refreshed")]),
            ],
        );
        let body = body_for(&req);
        // The system update is appended to the *same* trailing user content
        // (Gemini has no dedicated mid-conversation system role).
        assert_eq!(body["contents"].as_array().unwrap().len(), 1);
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert!(
            parts[1]["text"]
                .as_str()
                .unwrap()
                .contains("session refreshed")
        );
    }

    #[test]
    fn thinking_config_from_provider_options() {
        let mut req = base_request();
        req.provider_options = Some(
            [(
                "gemini".to_string(),
                json!({"thinkingConfig": {"thinkingBudget": 1024, "includeThoughts": true}}),
            )]
            .into_iter()
            .collect(),
        );
        let body = body_for(&req);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"thinkingBudget": 1024.0, "includeThoughts": true})
        );
    }

    #[test]
    fn assistant_reasoning_lowers_thought_signature_from_encrypted() {
        let req = LLMRequest::new(
            Model::new("google", "gemini-2.5-pro", "gemini"),
            vec![Message::assistant(vec![ContentPart::Reasoning {
                text: "thinking...".into(),
                encrypted: Some("sig".into()),
            }])],
        );
        let body = body_for(&req);
        assert_eq!(
            body["contents"][0]["parts"][0],
            json!({"text": "thinking...", "thought": true, "thoughtSignature": "sig"})
        );
    }

    // -- streaming reducer (gemini.ts:399-476) -------------------------------

    /// Flatten an event slice into its kebab-case type tags (mirrors
    /// `anthropic_messages::tests::types` / `openai_chat::tests::types`).
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
            })
            .collect()
    }

    /// Feed a scripted sequence of SSE `data:` payloads through
    /// `decode_event` + `step`, then `on_halt` (Gemini's finish events are
    /// only emitted at halt, not inline in `step`; see the module docs),
    /// returning the flattened event list.
    fn run(frames: &[&str]) -> Vec<LLMEvent> {
        let proto = Gemini;
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
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]}}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":" world"}]}}]}"#,
            r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
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
                assert!(usage.invariant_holds());
            }
            other => panic!("expected finish with usage, got {other:?}"),
        }
    }

    #[test]
    fn function_call_golden_sequence() {
        let frames = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city":"paris"}}}]},"finishReason":"STOP"}]}"#,
        ];
        let events = run(&frames);
        assert_eq!(
            types(&events),
            [
                "step-start",
                "tool-input-start",
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
        assert_eq!(tool_call.0, "tool_0");
        assert_eq!(tool_call.1, "get_weather");
        assert_eq!(tool_call.2["city"], "paris");
        // A tool call while finishReason is STOP is coerced to tool-calls.
        match events.last().unwrap() {
            LLMEvent::Finish { reason, .. } => assert_eq!(*reason, FinishReason::ToolCalls),
            other => panic!("expected finish, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_then_text_golden_sequence() {
        let frames = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Let me think","thought":true}]}}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"The answer is 4"}]},"finishReason":"STOP"}]}"#,
        ];
        let events = run(&frames);
        assert_eq!(
            types(&events),
            [
                "step-start",
                "reasoning-start",
                "reasoning-delta",
                "reasoning-end",
                "text-start",
                "text-delta",
                "text-end",
                "step-finish",
                "finish",
            ]
        );
    }

    #[test]
    fn content_less_finish_still_emits_step_start() {
        // A stream that carries only a terminal `finishReason` with no content
        // parts (e.g. an immediate SAFETY block) must still open the step, so
        // `step-finish` has a matching `step-start`.
        let events = run(&[r#"{"candidates":[{"finishReason":"SAFETY"}]}"#]);
        assert_eq!(types(&events), ["step-start", "step-finish", "finish"]);
        // Step boundaries are balanced: one step-start per step-finish.
        let starts = types(&events)
            .iter()
            .filter(|t| **t == "step-start")
            .count();
        let finishes = types(&events)
            .iter()
            .filter(|t| **t == "step-finish")
            .count();
        assert_eq!(starts, finishes);
        match events.last().unwrap() {
            LLMEvent::Finish { reason, .. } => {
                assert_eq!(*reason, FinishReason::ContentFilter)
            }
            other => panic!("expected finish, got {other:?}"),
        }
    }

    #[test]
    fn usage_sums_thoughts_and_subtracts_cached() {
        // All four token fields non-zero so the `+ thoughts` term and the
        // cached subtraction are both observable (a regression dropping either
        // would fail here).
        let frames = [
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]}}]}"#,
            r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":30,"thoughtsTokenCount":20,"cachedContentTokenCount":40,"totalTokenCount":150}}"#,
        ];
        let events = run(&frames);
        match events.last().unwrap() {
            LLMEvent::Finish {
                usage: Some(usage), ..
            } => {
                // inclusive input passes through untouched.
                assert_eq!(usage.input_tokens, Some(100));
                // output = candidates + thoughts = 30 + 20.
                assert_eq!(usage.output_tokens, Some(50));
                // non-cached input = prompt - cached = 100 - 40.
                assert_eq!(usage.non_cached_input_tokens, Some(60));
                assert_eq!(usage.cache_read_input_tokens, Some(40));
                assert_eq!(usage.reasoning_tokens, Some(20));
                assert_eq!(usage.total_tokens, Some(150));
                assert!(usage.invariant_holds());
            }
            other => panic!("expected finish with usage, got {other:?}"),
        }
    }

    #[test]
    fn usage_serialization_omits_absent_fields() {
        // Matches opencode: absent counts must not appear as explicit `null`s
        // in the `provider_metadata.google` blob.
        let usage = GeminiUsage {
            prompt_token_count: Some(10),
            candidates_token_count: Some(5),
            thoughts_token_count: None,
            cached_content_token_count: None,
            total_token_count: None,
        };
        let value = serde_json::to_value(&usage).expect("serialize");
        assert!(value.get("thoughtsTokenCount").is_none());
        assert!(value.get("cachedContentTokenCount").is_none());
        assert!(value.get("totalTokenCount").is_none());
        assert_eq!(value["promptTokenCount"], json!(10));
        assert_eq!(value["candidatesTokenCount"], json!(5));
    }
}
