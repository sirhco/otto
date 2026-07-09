//! The OpenAI Chat Completions wire protocol.
//!
//! Faithful port of opencode `packages/llm/src/protocols/openai-chat.ts`.
//! Builds the provider-native `/chat/completions` request body from a neutral
//! [`LLMRequest`] and folds streamed SSE events into provider-neutral
//! [`LLMEvent`]s via the shared [`lifecycle`] / [`tool_stream`] state machines.
//!
//! Reused verbatim by every route that speaks OpenAI Chat over HTTP+SSE
//! (native OpenAI, DeepSeek, TogetherAI, Cerebras, Fireworks, …) — see
//! [`crate::protocols::openai_compatible`], which only overrides the route id.

use otto_events::{FinishReason, Json, LLMEvent, Usage};
use serde::{Deserialize, Serialize};

use crate::error::LLMError;
use crate::message::{ContentPart, Role, ToolChoice, ToolDefinition};
use crate::protocols::utils::{lifecycle, tool_stream};
use crate::request::LLMRequest;

/// Protocol id (`ADAPTER` in `openai-chat.ts:26`).
const ADAPTER: &str = "openai-chat";

/// MIME types OpenAI Chat accepts as inline image content
/// (`ProviderShared.IMAGE_MIMES`).
const IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Every reasoning-effort literal (`ReasoningEfforts` in opencode `ids.ts:29`).
const ALL_REASONING_EFFORTS: [&str; 7] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The subset OpenAI Chat accepts — everything except `"max"`
/// (`OpenAIReasoningEfforts` in `openai-options.ts:5`).
const OPENAI_REASONING_EFFORTS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

// =============================================================================
// Request Body Schema (openai-chat.ts:37-109)
// =============================================================================

/// One image reference inside a user content part (`image_url` field,
/// `openai-chat.ts:62-64`).
#[derive(Debug, Clone, Serialize)]
pub struct ImageUrl {
    /// The image data URL (`data:<mime>;base64,<data>`).
    pub url: String,
}

/// A structured user content block (`OpenAIChatUserContent`,
/// `openai-chat.ts:59-65`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContentPart {
    /// A text segment.
    Text {
        /// The text.
        text: String,
    },
    /// An inline image reference.
    ImageUrl {
        /// The image URL wrapper.
        image_url: ImageUrl,
    },
}

/// User message content — either a collapsed string or a content-part array
/// (`content: String | Array` in `openai-chat.ts:71`).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum UserContent {
    /// Collapsed text-only content.
    Text(String),
    /// Mixed / media content parts.
    Parts(Vec<UserContentPart>),
}

/// One `function` tool-call the assistant made
/// (`OpenAIChatAssistantToolCall.function`, `openai-chat.ts:52-55`).
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallFunction {
    /// Tool name.
    pub name: String,
    /// JSON-encoded arguments string.
    pub arguments: String,
}

/// An assistant tool call (`OpenAIChatAssistantToolCall`,
/// `openai-chat.ts:49-57`).
#[derive(Debug, Clone, Serialize)]
pub struct AssistantToolCall {
    /// Tool-call id.
    pub id: String,
    /// Always `"function"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The called function.
    pub function: ToolCallFunction,
}

/// A wire message (`OpenAIChatMessage`, tagged by `role`,
/// `openai-chat.ts:67-81`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum OpenAIChatMessage {
    /// A system instruction.
    System {
        /// The joined system text.
        content: String,
    },
    /// A user message.
    User {
        /// Collapsed string or content array.
        content: UserContent,
    },
    /// An assistant message. `content` is nullable (`Schema.NullOr`,
    /// `openai-chat.ts:75`).
    Assistant {
        /// The assistant text, or `null` when only tool calls / reasoning.
        content: Option<String>,
        /// Tool calls made this turn.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<AssistantToolCall>>,
        /// DeepSeek-style reasoning echo.
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    /// A tool result.
    Tool {
        /// The tool-call id this result answers.
        tool_call_id: String,
        /// The textual tool result.
        content: String,
    },
}

/// A `function` tool definition wrapper (`OpenAIChatFunction`,
/// `openai-chat.ts:37-41`).
#[derive(Debug, Clone, Serialize)]
pub struct OpenAIChatFunction {
    /// Tool name.
    pub name: String,
    /// Tool description (defaults to empty; the wire field is required).
    pub description: String,
    /// JSON Schema for the tool input.
    pub parameters: Json,
}

/// A tool the model may call (`OpenAIChatTool`, `openai-chat.ts:43-46`).
#[derive(Debug, Clone, Serialize)]
pub struct OpenAIChatTool {
    /// Always `"function"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The function definition.
    pub function: OpenAIChatFunction,
}

/// The `tool_choice` field (`OpenAIChatToolChoice`, `openai-chat.ts:83-89`).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum OpenAIChatToolChoice {
    /// One of `"auto"` / `"none"` / `"required"`.
    Mode(&'static str),
    /// Force a specific named function.
    Function {
        /// Always `"function"`.
        #[serde(rename = "type")]
        kind: &'static str,
        /// The forced function name wrapper.
        function: ToolChoiceFunction,
    },
}

/// The forced-function selector (`openai-chat.ts:86-88`).
#[derive(Debug, Clone, Serialize)]
pub struct ToolChoiceFunction {
    /// Tool name to force.
    pub name: String,
}

/// `stream_options` — always `{ include_usage: true }` (`openai-chat.ts:360`).
#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    /// Request a final usage-only chunk.
    pub include_usage: bool,
}

/// The provider-native request body (`OpenAIChatBody`, `bodyFields`
/// `openai-chat.ts:91-108`).
#[derive(Debug, Clone, Serialize)]
pub struct OpenAIChatBody {
    /// Target model id.
    pub model: String,
    /// Lowered conversation messages.
    pub messages: Vec<OpenAIChatMessage>,
    /// Tool definitions, omitted when there are none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAIChatTool>>,
    /// Tool-choice policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<OpenAIChatToolChoice>,
    /// Always `true`.
    pub stream: bool,
    /// Always `{ include_usage: true }`.
    pub stream_options: StreamOptions,
    /// OpenAI `store` flag (`provider_options.openai.store`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    /// OpenAI reasoning effort (`provider_options.openai.reasoningEffort`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Max output tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus-sampling mass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Frequency penalty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Presence penalty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Deterministic seed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// Stop sequences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
}

// =============================================================================
// Streaming Event Schema (openai-chat.ts:117-160)
// =============================================================================

/// `prompt_tokens_details` (`openai-chat.ts:121-125`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    /// Cached prompt-token subset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
}

/// `completion_tokens_details` (`openai-chat.ts:126-130`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompletionTokensDetails {
    /// Reasoning completion-token subset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

/// The `usage` block (`OpenAIChatUsage`, `openai-chat.ts:117-131`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenAIChatUsage {
    /// Inclusive prompt tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    /// Inclusive completion tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    /// Provider total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Cached-token breakdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// Reasoning-token breakdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

/// A streamed tool-call delta's `function` field
/// (`OpenAIChatToolCallDeltaFunction`, `openai-chat.ts:133-136`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolCallDeltaFunction {
    /// Tool name (usually only on the first delta for an index).
    #[serde(default)]
    pub name: Option<String>,
    /// Incremental JSON argument text.
    #[serde(default)]
    pub arguments: Option<String>,
}

/// A streamed tool-call delta (`OpenAIChatToolCallDelta`,
/// `openai-chat.ts:138-142`).
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallDelta {
    /// Stream-local index (the accumulation key).
    pub index: u32,
    /// Tool-call id (usually only on the first delta).
    #[serde(default)]
    pub id: Option<String>,
    /// The delta function payload.
    #[serde(default)]
    pub function: Option<ToolCallDeltaFunction>,
}

/// A choice `delta` (`OpenAIChatDelta`, `openai-chat.ts:145-149`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Delta {
    /// Visible text delta.
    #[serde(default)]
    pub content: Option<String>,
    /// Reasoning text delta (DeepSeek-style).
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Reasoning text delta in the OpenRouter/vLLM encoding (`delta.reasoning`)
    /// — mapped identically to `reasoning_content` so those gateways' thinking
    /// output isn't silently dropped.
    #[serde(default)]
    pub reasoning: Option<String>,
    /// Tool-call deltas.
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// A streamed choice (`OpenAIChatChoice`, `openai-chat.ts:151-154`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Choice {
    /// The incremental delta.
    #[serde(default)]
    pub delta: Option<Delta>,
    /// The terminal finish reason, when present.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// A provider error delivered as a frame over an already-open stream. OpenAI
/// sends `data: {"error": {...}}` when a request is rejected after streaming has
/// begun (e.g. context-length / tool-schema / continuation validation). Without
/// this the frame decodes into an empty `OpenAIChatEvent` and the error is lost.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenAIChatError {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default, rename = "type")]
    pub error_type: Option<String>,
    #[serde(default)]
    pub code: Option<ErrorCode>,
}

/// An error `code` that gateways send as either a string ("`rate_limited`",
/// "`429`") or a bare number (`429`). litellm/OpenRouter commonly use the
/// numeric form, which used to fail strict decoding and turn the whole frame
/// into a fatal `EventDecode`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ErrorCode {
    /// Numeric code (often the upstream HTTP status).
    Num(i64),
    /// String code.
    Str(String),
}

impl ErrorCode {
    /// The code as an HTTP status when it plausibly is one (400..=599).
    #[must_use]
    pub fn as_http_status(&self) -> Option<u16> {
        let n = match self {
            ErrorCode::Num(n) => *n,
            ErrorCode::Str(s) => s.parse().ok()?,
        };
        u16::try_from(n).ok().filter(|s| (400..=599).contains(s))
    }

    fn display(&self) -> String {
        match self {
            ErrorCode::Num(n) => n.to_string(),
            ErrorCode::Str(s) => s.clone(),
        }
    }
}

/// The `error` field of a frame: OpenAI's `{"error": {...}}` object, or the
/// bare-string form (`{"error": "boom"}`) some gateways emit.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ErrorField {
    /// Structured error object.
    Struct(OpenAIChatError),
    /// Bare string message.
    Text(String),
}

/// One decoded SSE event (`OpenAIChatEvent`, `openai-chat.ts:156-160`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenAIChatEvent {
    /// The choices array (`choices[0]` is used).
    #[serde(default)]
    pub choices: Vec<Choice>,
    /// The optional usage block.
    #[serde(default)]
    pub usage: Option<OpenAIChatUsage>,
    /// An inline provider error frame, surfaced rather than swallowed.
    #[serde(default)]
    pub error: Option<ErrorField>,
}

// =============================================================================
// Parser State (`ParserState`, openai-chat.ts:163-169)
// =============================================================================

/// Per-stream accumulator (`ParserState`).
///
/// OpenAI Chat does not emit per-tool stop events, so accumulated tool calls
/// are finalized eagerly when a terminal `finish_reason` first arrives and the
/// resulting `tool-input-end` + `tool-call` events are buffered in
/// [`ParserState::tool_call_events`] until [`OpenAIChat::on_halt`].
pub struct ParserState {
    /// Index-keyed tool accumulator.
    tools: tool_stream::State<u32>,
    /// Buffered `tool-input-end` + `tool-call` events, emitted at halt.
    tool_call_events: Vec<LLMEvent>,
    /// Latest mapped usage.
    usage: Option<Usage>,
    /// First-seen finish reason.
    finish_reason: Option<FinishReason>,
    /// Text/reasoning/step lifecycle machine.
    lifecycle: lifecycle::State,
}

// =============================================================================
// Request Lowering (openai-chat.ts:179-370)
// =============================================================================

/// Format a content-type list (`ProviderShared.formatContentTypes`).
fn format_content_types(types: &[&str]) -> String {
    match types {
        [] => String::new(),
        [only] => (*only).to_string(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// Build an unsupported-content error (`ProviderShared.unsupportedContent`).
fn unsupported(role: &str, types: &[&str]) -> LLMError {
    LLMError::Validation(format!(
        "OpenAI Chat {role} messages only support {} content for now",
        format_content_types(types)
    ))
}

/// Lower one media part into an `image_url` content part
/// (`lowerMedia`, `openai-chat.ts:205-208`).
fn lower_media(media_type: &str, data: &str) -> Result<UserContentPart, LLMError> {
    let mime = media_type.to_lowercase();
    if !IMAGE_MIMES.contains(&mime.as_str()) {
        return Err(LLMError::Validation(format!(
            "OpenAI Chat does not support media type {media_type}"
        )));
    }
    let url = if data.starts_with("data:") {
        data.to_string()
    } else {
        format!("data:{mime};base64,{data}")
    };
    Ok(UserContentPart::ImageUrl {
        image_url: ImageUrl { url },
    })
}

/// Lower a tool definition (`lowerTool`, `openai-chat.ts:179-186`).
fn lower_tool(tool: &ToolDefinition) -> OpenAIChatTool {
    OpenAIChatTool {
        kind: "function",
        function: OpenAIChatFunction {
            name: tool.name.clone(),
            description: tool.description.clone().unwrap_or_default(),
            parameters: tool.input_schema.clone(),
        },
    }
}

/// Lower the tool-choice policy (`lowerToolChoice`, `openai-chat.ts:188-194`).
fn lower_tool_choice(choice: &ToolChoice) -> OpenAIChatToolChoice {
    match choice {
        ToolChoice::Auto => OpenAIChatToolChoice::Mode("auto"),
        ToolChoice::None => OpenAIChatToolChoice::Mode("none"),
        ToolChoice::Required => OpenAIChatToolChoice::Mode("required"),
        ToolChoice::Tool { name } => OpenAIChatToolChoice::Function {
            kind: "function",
            function: ToolChoiceFunction { name: name.clone() },
        },
    }
}

/// Lower one assistant tool call (`lowerToolCall`, `openai-chat.ts:196-203`).
fn lower_tool_call(id: &str, name: &str, input: &Json) -> AssistantToolCall {
    AssistantToolCall {
        id: id.to_string(),
        kind: "function",
        function: ToolCallFunction {
            name: name.to_string(),
            arguments: serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string()),
        },
    }
}

/// Read `native.openaiCompatible.reasoning_content`
/// (`openAICompatibleReasoningContent`, `openai-chat.ts:210-211`).
fn openai_compatible_reasoning_content(native: Option<&Json>) -> Option<String> {
    native
        .and_then(|n| n.get("openaiCompatible"))
        .and_then(|c| c.get("reasoning_content"))
        .and_then(Json::as_str)
        .map(str::to_string)
}

/// Render a non-`content` tool result as text (`ProviderShared.toolResultText`,
/// `shared.ts:213-223`).
fn tool_result_text(result: &otto_events::ToolResultValue) -> String {
    use otto_events::ToolResultValue as V;
    match result {
        V::Text { value } => json_to_plain_string(value),
        V::Error { value } => {
            if value.is_object() || value.is_array() {
                serde_json::to_string(value).unwrap_or_default()
            } else {
                json_to_plain_string(value)
            }
        }
        V::Json { value } => serde_json::to_string(value).unwrap_or_default(),
        // `content` results are handled by the caller; never reach here.
        V::Content { .. } => String::new(),
    }
}

/// `String(value)`-style rendering for a scalar JSON value.
fn json_to_plain_string(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        Json::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Lower a user message (`lowerUserMessage`, `openai-chat.ts:213-229`).
fn lower_user_message(content: &[ContentPart]) -> Result<OpenAIChatMessage, LLMError> {
    let mut parts: Vec<UserContentPart> = Vec::new();
    for part in content {
        match part {
            ContentPart::Text { text, .. } => {
                parts.push(UserContentPart::Text { text: text.clone() })
            }
            ContentPart::Media {
                media_type, data, ..
            } => parts.push(lower_media(media_type, data)?),
            _ => return Err(unsupported("user", &["text", "media"])),
        }
    }
    // Collapse to a single string when every part is text (join with "",
    // `openai-chat.ts:226-227`).
    if parts
        .iter()
        .all(|p| matches!(p, UserContentPart::Text { .. }))
    {
        let text: String = parts
            .iter()
            .map(|p| match p {
                UserContentPart::Text { text } => text.as_str(),
                UserContentPart::ImageUrl { .. } => "",
            })
            .collect();
        return Ok(OpenAIChatMessage::User {
            content: UserContent::Text(text),
        });
    }
    Ok(OpenAIChatMessage::User {
        content: UserContent::Parts(parts),
    })
}

/// Lower an assistant message (`lowerAssistantMessage`,
/// `openai-chat.ts:231-262`).
fn lower_assistant_message(
    content: &[ContentPart],
    native: Option<&Json>,
) -> Result<OpenAIChatMessage, LLMError> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<AssistantToolCall> = Vec::new();
    for part in content {
        match part {
            ContentPart::Text { text, .. } => text_parts.push(text.clone()),
            ContentPart::Reasoning { text, .. } => reasoning_parts.push(text.clone()),
            ContentPart::ToolCall {
                id, name, input, ..
            } => tool_calls.push(lower_tool_call(id, name, input)),
            _ => {
                return Err(unsupported(
                    "assistant",
                    &["text", "reasoning", "tool-call"],
                ));
            }
        }
    }
    // Text parts join with "\n" (`ProviderShared.joinText`); reasoning parts
    // join with "" (`openai-chat.ts:259`).
    let content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n"))
    };
    let reasoning_content = if reasoning_parts.is_empty() {
        openai_compatible_reasoning_content(native)
    } else {
        Some(reasoning_parts.concat())
    };
    Ok(OpenAIChatMessage::Assistant {
        content,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        reasoning_content,
    })
}

/// Lower a tool message into `tool` messages + trailing images
/// (`lowerToolMessages`, `openai-chat.ts:264-285`).
fn lower_tool_messages(
    content: &[ContentPart],
) -> Result<(Vec<OpenAIChatMessage>, Vec<UserContentPart>), LLMError> {
    use otto_events::ToolResultValue as V;
    let mut messages: Vec<OpenAIChatMessage> = Vec::new();
    let mut images: Vec<UserContentPart> = Vec::new();
    for part in content {
        let ContentPart::ToolResult { id, result, .. } = part else {
            return Err(unsupported("tool", &["tool-result"]));
        };
        match result {
            V::Content { value } => {
                let text: Vec<String> = value
                    .iter()
                    .filter(|item| item.get("type").and_then(Json::as_str) == Some("text"))
                    .filter_map(|item| item.get("text").and_then(Json::as_str).map(str::to_string))
                    .collect();
                messages.push(OpenAIChatMessage::Tool {
                    tool_call_id: id.clone(),
                    content: text.join("\n"),
                });
                for item in value
                    .iter()
                    .filter(|item| item.get("type").and_then(Json::as_str) == Some("file"))
                {
                    let mime = item.get("mime").and_then(Json::as_str).unwrap_or_default();
                    let uri = item.get("uri").and_then(Json::as_str).unwrap_or_default();
                    images.push(lower_media(mime, uri)?);
                }
            }
            other => messages.push(OpenAIChatMessage::Tool {
                tool_call_id: id.clone(),
                content: tool_result_text(other),
            }),
        }
    }
    Ok((messages, images))
}

/// Wrap a chronological system update as visible user text
/// (`ProviderShared.wrappedSystemUpdate`, `shared.ts:111-147`).
fn wrapped_system_update(content: &[ContentPart]) -> Result<String, LLMError> {
    let mut texts: Vec<String> = Vec::new();
    for part in content {
        match part {
            ContentPart::Text { text, .. } => texts.push(text.clone()),
            _ => return Err(unsupported("system", &["text"])),
        }
    }
    let escaped = texts
        .join("\n")
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    Ok(format!("<system-update>\n{escaped}\n</system-update>"))
}

/// Flush pending images into a trailing user message (`flushImages`,
/// `openai-chat.ts:298-301`).
fn flush_images(messages: &mut Vec<OpenAIChatMessage>, pending: &mut Vec<UserContentPart>) {
    if pending.is_empty() {
        return;
    }
    messages.push(OpenAIChatMessage::User {
        content: UserContent::Parts(std::mem::take(pending)),
    });
}

/// Lower the full message list (`lowerMessages`, `openai-chat.ts:293-331`).
fn lower_messages(req: &LLMRequest) -> Result<Vec<OpenAIChatMessage>, LLMError> {
    let mut messages: Vec<OpenAIChatMessage> = Vec::new();
    if !req.system.is_empty() {
        let content = req
            .system
            .iter()
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        messages.push(OpenAIChatMessage::System { content });
    }
    let mut pending_images: Vec<UserContentPart> = Vec::new();
    for message in &req.messages {
        match message.role {
            Role::System => {
                let text = wrapped_system_update(&message.content)?;
                if !pending_images.is_empty() {
                    let mut content: Vec<UserContentPart> = std::mem::take(&mut pending_images);
                    content.push(UserContentPart::Text { text });
                    messages.push(OpenAIChatMessage::User {
                        content: UserContent::Parts(content),
                    });
                    continue;
                }
                match messages.last_mut() {
                    Some(OpenAIChatMessage::User {
                        content: UserContent::Text(prev),
                    }) => {
                        *prev = format!("{prev}\n{text}");
                    }
                    Some(OpenAIChatMessage::User {
                        content: UserContent::Parts(prev),
                    }) => {
                        prev.push(UserContentPart::Text { text });
                    }
                    _ => messages.push(OpenAIChatMessage::User {
                        content: UserContent::Text(text),
                    }),
                }
            }
            Role::Tool => {
                let (lowered, images) = lower_tool_messages(&message.content)?;
                messages.extend(lowered);
                pending_images.extend(images);
            }
            Role::User => {
                flush_images(&mut messages, &mut pending_images);
                messages.push(lower_user_message(&message.content)?);
            }
            Role::Assistant => {
                flush_images(&mut messages, &mut pending_images);
                messages.push(lower_assistant_message(
                    &message.content,
                    message.native.as_ref(),
                )?);
            }
        }
    }
    flush_images(&mut messages, &mut pending_images);
    Ok(messages)
}

/// Resolve `store` + validated `reasoning_effort` from provider options
/// (`lowerOptions` + `OpenAIOptions`, `openai-chat.ts:333-342`,
/// `openai-options.ts:46-56`).
fn lower_options(req: &LLMRequest) -> Result<(Option<bool>, Option<String>), LLMError> {
    let openai = req
        .provider_options
        .as_ref()
        .and_then(|opts| opts.get("openai"));
    let store = openai.and_then(|o| o.get("store")).and_then(Json::as_bool);
    // `reasoningEffort` is only surfaced when it is a known effort at all
    // (`isAnyReasoningEffort`); an unknown value is silently dropped.
    let reasoning_effort = openai
        .and_then(|o| o.get("reasoningEffort"))
        .and_then(Json::as_str)
        .filter(|s| ALL_REASONING_EFFORTS.contains(s))
        .map(str::to_string);
    if let Some(effort) = &reasoning_effort
        && !OPENAI_REASONING_EFFORTS.contains(&effort.as_str())
    {
        return Err(LLMError::Validation(format!(
            "OpenAI Chat does not support reasoning effort {effort}"
        )));
    }
    Ok((store, reasoning_effort))
}

/// Build the full request body (`fromRequest`, `openai-chat.ts:344-370`).
fn build_body(req: &LLMRequest) -> Result<OpenAIChatBody, LLMError> {
    let messages = lower_messages(req)?;
    let tools = if req.tools.is_empty() {
        None
    } else {
        Some(req.tools.iter().map(lower_tool).collect())
    };
    let tool_choice = req.tool_choice.as_ref().map(lower_tool_choice);
    let (store, reasoning_effort) = lower_options(req)?;
    let generation = req.generation.as_ref();
    Ok(OpenAIChatBody {
        model: req.model.id.0.clone(),
        messages,
        tools,
        tool_choice,
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        store,
        reasoning_effort,
        max_tokens: generation.and_then(|g| g.max_tokens),
        temperature: if req.model.capabilities.temperature {
            generation.and_then(|g| g.temperature)
        } else {
            None
        },
        top_p: generation.and_then(|g| g.top_p),
        frequency_penalty: generation.and_then(|g| g.frequency_penalty),
        presence_penalty: generation.and_then(|g| g.presence_penalty),
        seed: generation.and_then(|g| g.seed),
        stop: generation.and_then(|g| {
            if g.stop.is_empty() {
                None
            } else {
                Some(g.stop.clone())
            }
        }),
    })
}

// =============================================================================
// Stream Parsing (openai-chat.ts:378-470)
// =============================================================================

/// Map a provider finish reason (`mapFinishReason`, `openai-chat.ts:378-384`).
fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        "function_call" | "tool_calls" => FinishReason::ToolCalls,
        _ => FinishReason::Unknown,
    }
}

/// `Math.max(0, total - subtrahend)` token subtraction
/// (`ProviderShared.subtractTokens`, `shared.ts:72-76`).
fn subtract_tokens(total: Option<u64>, subtrahend: Option<u64>) -> Option<u64> {
    match (total, subtrahend) {
        (None, _) => None,
        (Some(total), None) => Some(total),
        (Some(total), Some(sub)) => Some(total.saturating_sub(sub)),
    }
}

/// Provider total, else `input + output` when at least one is present
/// (`ProviderShared.totalTokens`, `shared.ts:51-59`).
fn total_tokens(input: Option<u64>, output: Option<u64>, total: Option<u64>) -> Option<u64> {
    if let Some(total) = total {
        return Some(total);
    }
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(input.unwrap_or(0) + output.unwrap_or(0))
}

/// Map inclusive OpenAI usage into the additive [`Usage`] contract by
/// subtracting the cached subset (`mapUsage`, `openai-chat.ts:391-405`).
fn map_usage(usage: Option<&OpenAIChatUsage>) -> Option<Usage> {
    let usage = usage?;
    let cached = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens);
    let reasoning = usage
        .completion_tokens_details
        .as_ref()
        .and_then(|d| d.reasoning_tokens);
    let non_cached = subtract_tokens(usage.prompt_tokens, cached);
    let metadata = serde_json::json!({
        "openai": serde_json::to_value(usage).unwrap_or(Json::Null),
    });
    Some(Usage {
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
        non_cached_input_tokens: non_cached,
        cache_read_input_tokens: cached,
        cache_write_input_tokens: None,
        reasoning_tokens: reasoning,
        total_tokens: total_tokens(
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens,
        ),
        provider_metadata: Some(metadata),
    })
}

/// The OpenAI Chat protocol.
///
/// Port of the `protocol` export in `openai-chat.ts:481-493`.
pub struct OpenAIChat;

impl crate::protocol::Protocol for OpenAIChat {
    type Body = OpenAIChatBody;
    type Event = OpenAIChatEvent;
    type State = ParserState;

    fn id(&self) -> &'static str {
        ADAPTER
    }

    fn build_body(&self, req: &LLMRequest) -> Result<Self::Body, LLMError> {
        build_body(req)
    }

    fn decode_event(&self, frame: &str) -> Result<Self::Event, LLMError> {
        serde_json::from_str(frame).map_err(|e| LLMError::EventDecode(e.to_string()))
    }

    fn initial(&self, _req: &LLMRequest) -> Self::State {
        ParserState {
            tools: tool_stream::State::initial(),
            tool_call_events: Vec::new(),
            usage: None,
            finish_reason: None,
            lifecycle: lifecycle::State::initial(),
        }
    }

    /// Fold one streamed event (`step`, `openai-chat.ts:407-460`).
    fn step(&self, state: &mut Self::State, event: Self::Event) -> Result<Vec<LLMEvent>, LLMError> {
        let mut events: Vec<LLMEvent> = Vec::new();

        // A mid-stream provider error frame must surface, not be swallowed into an
        // empty event (which would leave the turn with no terminal finish).
        if let Some(err) = event.error {
            let err = match err {
                ErrorField::Struct(e) => e,
                ErrorField::Text(msg) => OpenAIChatError {
                    message: Some(msg),
                    ..OpenAIChatError::default()
                },
            };
            let msg = err.message.unwrap_or_else(|| "unknown error".to_string());
            // When the code reads as an HTTP status (429/5xx from litellm and
            // similar gateways), surface it as `Http` so the retry policy's
            // status-based classification applies instead of relying on
            // rate-limit phrase matching in the message text.
            if let Some(status) = err.code.as_ref().and_then(ErrorCode::as_http_status) {
                return Err(LLMError::Http {
                    status,
                    message: format!("openai: {msg}"),
                    retry_after: None,
                });
            }
            let detail = match (err.error_type, err.code.map(|c| c.display())) {
                (Some(t), Some(c)) => format!(" ({t}/{c})"),
                (Some(t), None) => format!(" ({t})"),
                (None, Some(c)) => format!(" ({c})"),
                (None, None) => String::new(),
            };
            return Err(LLMError::Stream(format!("openai: {msg}{detail}")));
        }

        // usage = mapUsage(event.usage) ?? state.usage.
        if let Some(usage) = map_usage(event.usage.as_ref()) {
            state.usage = Some(usage);
        }

        let choice = event.choices.into_iter().next();
        let previous_finish = state.finish_reason;
        // finishReason = choice?.finish_reason ? map(...) : state.finishReason.
        let finish_reason = choice
            .as_ref()
            .and_then(|c| c.finish_reason.as_deref())
            .map(map_finish_reason)
            .or(previous_finish);

        let delta = choice.and_then(|c| c.delta);
        if let Some(delta) = &delta {
            // reasoning_content → reasoning-0 (truthy: skip empty strings).
            // opencode's `reasoningDelta` starts the step internally; otto's
            // lifecycle keeps that explicit, so emit `step-start` first.
            if let Some(reasoning) = delta
                .reasoning_content
                .as_deref()
                .or(delta.reasoning.as_deref())
                .filter(|s| !s.is_empty())
            {
                events.extend(state.lifecycle.step_start(0));
                events.extend(state.lifecycle.reasoning_delta("reasoning-0", reasoning));
            }
            // content → close reasoning-0, then text-0.
            if let Some(content) = delta.content.as_deref().filter(|s| !s.is_empty()) {
                events.extend(state.lifecycle.step_start(0));
                events.extend(state.lifecycle.reasoning_end("reasoning-0"));
                events.extend(state.lifecycle.text_delta("text-0", content));
            }
        }

        let tool_deltas = delta.and_then(|d| d.tool_calls).unwrap_or_default();
        if !tool_deltas.is_empty() {
            events.extend(state.lifecycle.reasoning_end("reasoning-0"));
        }
        for tool in tool_deltas {
            let (name, arguments) = tool
                .function
                .map(|f| (f.name, f.arguments.unwrap_or_default()))
                .unwrap_or((None, String::new()));
            let produced = state
                .tools
                .append_or_start(tool.index, tool.id, name, &arguments)?;
            // Emit step-start before the first tool event (openai-chat.ts:439).
            if !produced.is_empty() {
                let step = state.lifecycle.step_start(0);
                events.extend(step);
            }
            events.extend(produced);
        }

        // Eager finalize: finalize accumulated tool inputs when finish_reason
        // first arrives (openai-chat.ts:445-448).
        if finish_reason.is_some() && previous_finish.is_none() && !state.tools.is_empty() {
            state.tool_call_events = state.tools.finish_all()?;
        }
        state.finish_reason = finish_reason;

        Ok(events)
    }

    /// Flush buffered tool calls, then the step/finish events (`finishEvents`,
    /// `openai-chat.ts:462-470`).
    fn on_halt(&self, state: &mut Self::State) -> Vec<LLMEvent> {
        let has_tool_calls = !state.tool_call_events.is_empty();
        // A stream that produced nothing and carried no finish_reason: don't
        // fabricate a terminal event (mirrors `bedrock_converse::on_halt`'s
        // `is_started()` guard).
        if state.finish_reason.is_none() && !has_tool_calls && !state.lifecycle.is_started() {
            return Vec::new();
        }
        // A STARTED stream that ended without a finish_reason and without tool
        // calls is a truncated response (early close / gateway hiccup).
        // Fabricating `Finish(Unknown)` here silently accepted the partial
        // answer; instead close the open blocks and emit NO finish, so the
        // processor surfaces the retryable `NoTerminalFinish` and the run loop
        // retries (accept-with-warning happens there once the budget runs out).
        if state.finish_reason.is_none() && !has_tool_calls {
            let mut events = state.lifecycle.reasoning_end("reasoning-0");
            events.extend(state.lifecycle.text_end("text-0"));
            return events;
        }
        let mut events: Vec<LLMEvent> = Vec::new();
        // Coerce stop → tool-calls when the model actually emitted tool calls;
        // fall back to `Unknown` when tool calls were buffered but the stream
        // closed before the finish frame arrived.
        let reason = match state.finish_reason {
            Some(FinishReason::Stop) if has_tool_calls => FinishReason::ToolCalls,
            Some(other) => other,
            None => FinishReason::Unknown,
        };
        if has_tool_calls {
            events.extend(state.lifecycle.step_start(0));
        }
        events.append(&mut state.tool_call_events);
        events.extend(state.lifecycle.finish(reason, state.usage.clone(), 0));
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, SystemPart};
    use crate::model::Model;
    use crate::protocol::Protocol;
    use serde_json::json;

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
                LLMEvent::StepFinish { .. } => "step-finish",
                LLMEvent::Finish { .. } => "finish",
                _ => "other",
            })
            .collect()
    }

    fn request() -> LLMRequest {
        LLMRequest::new(
            Model::new("openai", "gpt-4o", "openai-chat"),
            vec![Message::user(vec![ContentPart::text("hi")])],
        )
    }

    /// Drive frames through decode_event + step, then on_halt, collecting all
    /// emitted events (mirrors the route pipeline for text/tool streams that
    /// terminate on `[DONE]`).
    fn drive(frames: &[serde_json::Value]) -> Vec<LLMEvent> {
        let protocol = OpenAIChat;
        let req = request();
        let mut state = protocol.initial(&req);
        let mut out = Vec::new();
        for frame in frames {
            let event: OpenAIChatEvent = serde_json::from_value(frame.clone()).unwrap();
            out.extend(protocol.step(&mut state, event).unwrap());
        }
        out.extend(protocol.on_halt(&mut state));
        out
    }

    // ---- streaming reducer -------------------------------------------------

    #[test]
    fn golden_text_stream() {
        let frames = vec![
            json!({"choices":[{"delta":{"content":"Hello"}}]}),
            json!({"choices":[{"delta":{"content":" world"}}]}),
            json!({
                "choices":[{"delta":{},"finish_reason":"stop"}],
                "usage":{
                    "prompt_tokens":10,
                    "completion_tokens":5,
                    "total_tokens":15,
                    "prompt_tokens_details":{"cached_tokens":4},
                    "completion_tokens_details":{"reasoning_tokens":2}
                }
            }),
        ];
        let events = drive(&frames);
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
        // finish carries usage with SUBTRACT-direction cache math.
        let usage = match events.last().unwrap() {
            LLMEvent::Finish {
                usage: Some(u),
                reason,
                ..
            } => {
                assert_eq!(*reason, FinishReason::Stop);
                u
            }
            _ => panic!("expected finish with usage"),
        };
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.cache_read_input_tokens, Some(4));
        // non-cached = prompt_tokens - cached = 10 - 4.
        assert_eq!(usage.non_cached_input_tokens, Some(6));
        assert_eq!(usage.reasoning_tokens, Some(2));
        assert_eq!(usage.total_tokens, Some(15));
    }

    #[test]
    fn stream_without_finish_reason_closes_blocks_without_finish() {
        // A stream that opens a step (streams content) but whose HTTP frames end
        // WITHOUT any `finish_reason` chunk — an early connection close or a
        // mid-stream provider hiccup, i.e. a TRUNCATED response. Fabricating a
        // terminal `Finish(Unknown)` here silently accepted the partial answer;
        // instead the reducer closes the open blocks and emits NO finish, so
        // the processor's truncation gate surfaces retryable NoTerminalFinish
        // and the run loop retries (accepting-with-warning only once the retry
        // budget runs out).
        let frames = vec![json!({"choices":[{"delta":{"content":"partial"}}]})];
        let events = drive(&frames);
        let tys = types(&events);
        assert!(
            !tys.contains(&"finish"),
            "a truncated stream must not fabricate a terminal finish; got {tys:?}"
        );
        assert!(
            tys.contains(&"text-end"),
            "open text block must still be closed; got {tys:?}"
        );
    }

    #[test]
    fn stream_with_buffered_tool_calls_but_no_finish_still_terminates() {
        // Tool calls were fully streamed but the finish frame never arrived
        // (early close). The buffered tool calls are legitimate work — coerce
        // to a tool-calls turn boundary rather than discarding them.
        let frames = vec![json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_1","function":{"name":"echo","arguments":"{}"}}
        ]},"finish_reason":"tool_calls"}]})];
        let events = drive(&frames);
        let tys = types(&events);
        assert!(tys.contains(&"tool-call"));
        assert!(tys.contains(&"finish"));
    }

    #[test]
    fn numeric_error_code_maps_to_http_status() {
        // litellm/OpenRouter send numeric codes (`"code": 429`); this used to
        // fail strict decoding and kill the turn with a fatal EventDecode.
        let protocol = OpenAIChat;
        let req = request();
        let mut state = protocol.initial(&req);
        let event: OpenAIChatEvent = serde_json::from_value(json!({
            "error": {"message": "rate limited", "code": 429}
        }))
        .expect("numeric code decodes");
        let err = protocol.step(&mut state, event).unwrap_err();
        match err {
            LLMError::Http { status, .. } => assert_eq!(status, 429),
            other => panic!("expected Http {{429}}, got {other:?}"),
        }
    }

    #[test]
    fn string_http_error_code_maps_to_http_status() {
        let protocol = OpenAIChat;
        let req = request();
        let mut state = protocol.initial(&req);
        let event: OpenAIChatEvent = serde_json::from_value(json!({
            "error": {"message": "upstream overloaded", "code": "503"}
        }))
        .expect("string code decodes");
        let err = protocol.step(&mut state, event).unwrap_err();
        assert!(matches!(err, LLMError::Http { status: 503, .. }));
    }

    #[test]
    fn bare_string_error_frame_decodes_and_surfaces() {
        // Some gateways emit `{"error": "boom"}` — a bare string, not the
        // OpenAI object shape.
        let protocol = OpenAIChat;
        let req = request();
        let mut state = protocol.initial(&req);
        let event: OpenAIChatEvent =
            serde_json::from_value(json!({"error": "boom"})).expect("bare-string error decodes");
        let err = protocol.step(&mut state, event).unwrap_err();
        match err {
            LLMError::Stream(msg) => assert!(msg.contains("boom"), "got {msg}"),
            other => panic!("expected Stream, got {other:?}"),
        }
    }

    #[test]
    fn non_http_error_code_stays_stream_error() {
        let protocol = OpenAIChat;
        let req = request();
        let mut state = protocol.initial(&req);
        let event: OpenAIChatEvent = serde_json::from_value(json!({
            "error": {"message": "bad schema", "type": "invalid_request_error", "code": "invalid_function"}
        }))
        .expect("decodes");
        let err = protocol.step(&mut state, event).unwrap_err();
        assert!(matches!(err, LLMError::Stream(_)));
    }

    #[test]
    fn openrouter_reasoning_delta_maps_like_reasoning_content() {
        // OpenRouter/vLLM stream thinking as `delta.reasoning`.
        let frames = vec![
            json!({"choices":[{"delta":{"reasoning":"thinking…"}}]}),
            json!({"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}),
        ];
        let events = drive(&frames);
        let tys = types(&events);
        assert!(
            tys.contains(&"reasoning-delta"),
            "delta.reasoning must map to reasoning events; got {tys:?}"
        );
    }

    #[test]
    fn empty_stream_emits_nothing() {
        // A stream that produces no frames at all must NOT fabricate a finish
        // (mirrors bedrock_converse::on_halt's is_started() guard).
        let events = drive(&[]);
        assert!(
            events.is_empty(),
            "empty stream must not fabricate events; got {events:?}"
        );
    }

    #[test]
    fn midstream_error_frame_surfaces() {
        // OpenAI sends `data: {"error": {...}}` over an already-open stream when a
        // request is rejected after streaming starts. That frame must surface as
        // an error, not be silently swallowed into an empty event.
        let protocol = OpenAIChat;
        let req = request();
        let mut state = protocol.initial(&req);
        let frame = json!({"error":{
            "message":"context length exceeded",
            "type":"invalid_request_error",
            "code":"context_length_exceeded"
        }});
        let event: OpenAIChatEvent = serde_json::from_value(frame).unwrap();
        let result = protocol.step(&mut state, event);
        assert!(
            result.is_err(),
            "an OpenAI mid-stream error frame must surface as an error, not be swallowed"
        );
        assert!(
            format!("{}", result.unwrap_err()).contains("context length exceeded"),
            "surfaced error should carry the provider message"
        );
    }

    #[test]
    fn golden_tool_call_stream() {
        let frames = vec![
            json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_1","function":{"name":"get_weather","arguments":"{\"ci"}}
            ]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"ty\":\""}}
            ]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"paris\"}"}}
            ]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let events = drive(&frames);
        assert_eq!(
            types(&events),
            [
                "step-start",
                "tool-input-start",
                "tool-input-delta",
                "tool-input-delta",
                "tool-input-delta",
                "tool-input-end",
                "tool-call",
                "step-finish",
                "finish",
            ]
        );
        match events
            .iter()
            .find(|e| matches!(e, LLMEvent::ToolCall { .. }))
        {
            Some(LLMEvent::ToolCall { name, input, .. }) => {
                assert_eq!(name, "get_weather");
                assert_eq!(input["city"], "paris");
            }
            _ => panic!("expected tool-call"),
        }
        assert_eq!(
            match events.last().unwrap() {
                LLMEvent::Finish { reason, .. } => *reason,
                _ => panic!("expected finish"),
            },
            FinishReason::ToolCalls
        );
    }

    #[test]
    fn stop_coerced_to_tool_calls_when_tools_present() {
        let frames = vec![
            json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_1","function":{"name":"noop","arguments":"{}"}}
            ]}}]}),
            // provider (incorrectly) reports "stop" while emitting a tool call.
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        ];
        let events = drive(&frames);
        assert_eq!(
            match events.last().unwrap() {
                LLMEvent::Finish { reason, .. } => *reason,
                _ => panic!("expected finish"),
            },
            FinishReason::ToolCalls
        );
    }

    #[test]
    fn reasoning_content_precedes_text() {
        let frames = vec![
            json!({"choices":[{"delta":{"reasoning_content":"thinking"}}]}),
            json!({"choices":[{"delta":{"reasoning_content":" more"}}]}),
            json!({"choices":[{"delta":{"content":"answer"}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        ];
        let events = drive(&frames);
        assert_eq!(
            types(&events),
            [
                "step-start",
                "reasoning-start",
                "reasoning-delta",
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

    // ---- build_body units --------------------------------------------------

    #[test]
    fn tool_choice_mapping() {
        let cases = [
            (ToolChoice::Auto, json!("auto")),
            (ToolChoice::None, json!("none")),
            (ToolChoice::Required, json!("required")),
            (
                ToolChoice::Tool { name: "f".into() },
                json!({"type":"function","function":{"name":"f"}}),
            ),
        ];
        for (choice, expected) in cases {
            let lowered = lower_tool_choice(&choice);
            assert_eq!(serde_json::to_value(&lowered).unwrap(), expected);
        }
    }

    #[test]
    fn user_content_collapses_to_string_or_array() {
        // text-only → string.
        let msg = lower_user_message(&[ContentPart::text("a"), ContentPart::text("b")]).unwrap();
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            json!({"role":"user","content":"ab"})
        );
        // text + media → array.
        let msg = lower_user_message(&[
            ContentPart::text("look"),
            ContentPart::Media {
                media_type: "image/png".into(),
                data: "AAAA".into(),
                filename: None,
            },
        ])
        .unwrap();
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            json!({"role":"user","content":[
                {"type":"text","text":"look"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
            ]})
        );
    }

    #[test]
    fn tool_result_becomes_tool_role_and_flushes_images_to_next_user() {
        use otto_events::ToolResultValue;
        let mut req = request();
        req.messages = vec![
            Message::tool(vec![ContentPart::ToolResult {
                id: "call_1".into(),
                name: "screenshot".into(),
                result: ToolResultValue::Content {
                    value: vec![
                        json!({"type":"text","text":"here"}),
                        json!({"type":"file","mime":"image/png","uri":"AAAA","name":"s.png"}),
                    ],
                },
                provider_executed: None,
                cache: None,
            }]),
            Message::user(vec![ContentPart::text("thanks")]),
        ];
        let body = build_body(&req).unwrap();
        let value = serde_json::to_value(&body.messages).unwrap();
        assert_eq!(
            value,
            json!([
                {"role":"tool","tool_call_id":"call_1","content":"here"},
                {"role":"user","content":[
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
                ]},
                {"role":"user","content":"thanks"}
            ])
        );
    }

    #[test]
    fn reasoning_effort_and_store_from_provider_options() {
        let mut req = request();
        let mut openai = serde_json::Map::new();
        openai.insert("store".into(), json!(true));
        openai.insert("reasoningEffort".into(), json!("high"));
        let mut opts = std::collections::BTreeMap::new();
        opts.insert("openai".to_string(), Json::Object(openai));
        req.provider_options = Some(opts);
        let body = build_body(&req).unwrap();
        assert_eq!(body.store, Some(true));
        assert_eq!(body.reasoning_effort.as_deref(), Some("high"));

        // "max" is rejected for OpenAI Chat.
        let mut openai = serde_json::Map::new();
        openai.insert("reasoningEffort".into(), json!("max"));
        let mut opts = std::collections::BTreeMap::new();
        opts.insert("openai".to_string(), Json::Object(openai));
        req.provider_options = Some(opts);
        assert!(matches!(build_body(&req), Err(LLMError::Validation(_))));

        // an unknown effort is silently dropped, not an error.
        let mut openai = serde_json::Map::new();
        openai.insert("reasoningEffort".into(), json!("turbo"));
        let mut opts = std::collections::BTreeMap::new();
        opts.insert("openai".to_string(), Json::Object(openai));
        req.provider_options = Some(opts);
        let body = build_body(&req).unwrap();
        assert_eq!(body.reasoning_effort, None);
    }

    #[test]
    fn body_always_streams_with_usage() {
        let body = build_body(&request()).unwrap();
        assert!(body.stream);
        assert!(body.stream_options.include_usage);
    }

    #[test]
    fn temperature_gated_on_model_capability() {
        use crate::request::GenerationOptions;

        // Capability off (e.g. `o3`) → temperature must be omitted even when
        // the caller asked for one.
        let mut req = request();
        req.generation = Some(GenerationOptions {
            temperature: Some(0.7),
            ..GenerationOptions::default()
        });
        req.model.capabilities.temperature = false;
        let body = build_body(&req).unwrap();
        assert_eq!(body.temperature, None);
        let value = serde_json::to_value(&body).unwrap();
        assert!(value.get("temperature").is_none());

        // Capability on → temperature passes through.
        req.model.capabilities.temperature = true;
        let body = build_body(&req).unwrap();
        assert_eq!(body.temperature, Some(0.7));
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["temperature"], json!(0.7));
    }

    #[test]
    fn system_parts_joined_with_newline() {
        let mut req = request();
        req.system = vec![SystemPart::new("a"), SystemPart::new("b")];
        let body = build_body(&req).unwrap();
        match &body.messages[0] {
            OpenAIChatMessage::System { content } => assert_eq!(content, "a\nb"),
            _ => panic!("expected system message first"),
        }
    }

    #[test]
    fn assistant_reasoning_from_native_openai_compatible() {
        let content = [ContentPart::text("answer")];
        let native = json!({"openaiCompatible":{"reasoning_content":"because"}});
        let msg = lower_assistant_message(&content, Some(&native)).unwrap();
        match msg {
            OpenAIChatMessage::Assistant {
                reasoning_content, ..
            } => assert_eq!(reasoning_content.as_deref(), Some("because")),
            _ => panic!("expected assistant"),
        }
    }

    #[test]
    fn id_is_openai_chat() {
        assert_eq!(OpenAIChat.id(), "openai-chat");
    }
}
