//! The OpenAI Responses wire protocol — request body + lowering (Task 1) and
//! stream event parsing (Task 2).
//!
//! Faithful port of opencode `packages/llm/src/protocols/openai-responses.ts`
//! lines 32-498 (the "Request Body Schema" + "Request Lowering" sections) and
//! lines 500-949 (the "Stream Parsing" section, minus `HOSTED_TOOLS` /
//! `hostedToolEvents` / `isHostedToolItem` and the hosted-tool branch of
//! `onOutputItemDone` — provider-executed tool replay is deferred). This file
//! builds the provider-native `/responses` request body from a neutral
//! [`LLMRequest`] and folds streamed events into provider-neutral
//! [`LLMEvent`]s via the shared [`lifecycle`] / [`tool_stream`] state
//! machines. The [`crate::protocol::Protocol`] impl that wires `step`/
//! `initial` up to HTTP+SSE transport is Task 3's job.
//!
//! Sibling protocol: [`crate::protocols::openai_chat`], which this file
//! mirrors for serde idioms, media lowering, JSON-argument encoding, and the
//! exact `lifecycle`/`tool_stream` call surface.
//!
use otto_events::{
    FinishReason, Json, LLMEvent, ProviderFailureClassification, ProviderMetadata, ToolResultValue,
    Usage,
};
use serde::{Deserialize, Serialize};

use crate::error::LLMError;
use crate::message::{ContentPart, Role, ToolChoice, ToolDefinition};
use crate::protocols::utils::{lifecycle, tool_stream};
use crate::request::LLMRequest;

/// Protocol id (`ADAPTER`, `openai-responses.ts:28`).
const ADAPTER: &str = "openai-responses";

/// Default base URL (`DEFAULT_BASE_URL`, `openai-responses.ts:29`).
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Request path (`PATH`, `openai-responses.ts:30`).
pub const PATH: &str = "/responses";

/// MIME types OpenAI Responses accepts as inline image content
/// (`ProviderShared.IMAGE_MIMES`, shared with `openai_chat::IMAGE_MIMES`).
const IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Reasoning-effort literals OpenAI Responses accepts (`OpenAIReasoningEfforts`,
/// `openai-options.ts:5-8`: every effort except `"max"`). Unlike
/// `openai_chat::lower_options`, an unrecognized value here is a hard error —
/// see [`lower_options`].
const REASONING_EFFORTS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

// =============================================================================
// Request Body Schema (openai-responses.ts:35-156)
// =============================================================================

/// One input content block (`OpenAIResponsesInputContent`,
/// `openai-responses.ts:35-44`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InputContent {
    /// A text segment.
    InputText {
        /// The text.
        text: String,
    },
    /// An inline image reference.
    InputImage {
        /// The image data URL.
        image_url: String,
    },
}

/// `function_call_output.output`: a plain string or an ordered content array
/// (`OpenAIResponsesFunctionCallOutput`, `openai-responses.ts:68-76`).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum FunctionCallOutput {
    /// Text/json/error results flattened to a string.
    Text(String),
    /// Structured content blocks (tool results carrying images).
    Content(Vec<InputContent>),
}

/// One assistant output-text replay block (`OpenAIResponsesOutputText`,
/// `openai-responses.ts:46-49`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputContent {
    /// A text segment.
    OutputText {
        /// The text.
        text: String,
    },
}

/// One item in the `input` array (`OpenAIResponsesInputItem`,
/// `openai-responses.ts:78-96`).
///
/// `#[serde(untagged)]` because the union mixes `role`-tagged items
/// (system/user/assistant) with `type`-tagged items (function_call/
/// function_call_output); serde only supports a single tag key per enum, so
/// each variant instead carries its own fixed-literal `role`/`type` field —
/// the same `kind: &'static str` idiom `openai_chat::AssistantToolCall` /
/// `openai_chat::OpenAIChatTool` use for their fixed `type` fields.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum InputItem {
    /// A system instruction item.
    System {
        /// Always `"system"`.
        role: &'static str,
        /// The joined system text.
        content: String,
    },
    /// A user message item.
    User {
        /// Always `"user"`.
        role: &'static str,
        /// The content blocks.
        content: Vec<InputContent>,
    },
    /// An assistant text-replay item.
    Assistant {
        /// Always `"assistant"`.
        role: &'static str,
        /// The output-text blocks.
        content: Vec<OutputContent>,
    },
    /// A function call the assistant made.
    FunctionCall {
        /// Always `"function_call"`.
        #[serde(rename = "type")]
        kind: &'static str,
        /// The tool-call id.
        call_id: String,
        /// Tool name.
        name: String,
        /// JSON-encoded arguments string.
        arguments: String,
    },
    /// A tool result fed back to the model.
    FunctionCallOutputItem {
        /// Always `"function_call_output"`.
        #[serde(rename = "type")]
        kind: &'static str,
        /// The tool-call id this result answers.
        call_id: String,
        /// The tool output.
        output: FunctionCallOutput,
    },
}

/// A `function` tool definition (`OpenAIResponsesTool`,
/// `openai-responses.ts:108-115`).
#[derive(Debug, Clone, Serialize)]
struct ResponsesTool {
    /// Always `"function"`.
    #[serde(rename = "type")]
    kind: &'static str,
    /// Tool name.
    name: String,
    /// Tool description (defaults to empty; the wire field is required).
    description: String,
    /// JSON Schema for the tool input.
    parameters: Json,
    /// Always `false` (opencode TODO: read this from OpenAI-specific tool
    /// options so direct callers can opt into strict schemas).
    strict: bool,
}

/// The `tool_choice` field (`OpenAIResponsesToolChoice`,
/// `openai-responses.ts:117-120`).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum ResponsesToolChoice {
    /// One of `"auto"` / `"none"` / `"required"`.
    Mode(&'static str),
    /// Force a specific named function.
    Function {
        /// Always `"function"`.
        #[serde(rename = "type")]
        kind: &'static str,
        /// The forced function name.
        name: String,
    },
}

/// The `reasoning` options block (`openai-responses.ts:136-141`).
#[derive(Debug, Clone, Serialize)]
struct ReasoningOptions {
    /// Reasoning effort.
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    /// Reasoning summary mode (only `"auto"` is surfaced).
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}

/// The `text` options block (`openai-responses.ts:142-146`).
#[derive(Debug, Clone, Serialize)]
struct TextOptions {
    /// Response verbosity.
    #[serde(skip_serializing_if = "Option::is_none")]
    verbosity: Option<String>,
}

/// The provider-native request body (`OpenAIResponsesBody`,
/// `openai-responses.ts:126-156`).
#[derive(Debug, Clone, Serialize)]
pub struct OpenAIResponsesBody {
    model: String,
    input: Vec<InputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ResponsesToolChoice>,
    /// Always `true`; never skipped (`Schema.Literal(true)`,
    /// `openai-responses.ts:154`).
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<TextOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
}

// =============================================================================
// Request Lowering (openai-responses.ts:259-498)
// =============================================================================

/// Format a content-type list (`ProviderShared.formatContentTypes`, shared
/// with `openai_chat::format_content_types`).
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
        "OpenAI Responses {role} messages only support {} content for now",
        format_content_types(types)
    ))
}

/// Lower one media reference into a data-URL string (`lowerMedia` /
/// `ProviderShared.validateMedia`, `openai-responses.ts:312-319`; shared logic
/// with `openai_chat::lower_media`).
fn lower_media(media_type: &str, data: &str) -> Result<String, LLMError> {
    let mime = media_type.to_lowercase();
    if !IMAGE_MIMES.contains(&mime.as_str()) {
        return Err(LLMError::Validation(format!(
            "OpenAI Responses does not support media type {media_type}"
        )));
    }
    if data.starts_with("data:") {
        Ok(data.to_string())
    } else {
        Ok(format!("data:{mime};base64,{data}"))
    }
}

/// Lower a tool definition (`lowerTool`, `openai-responses.ts:259-266`).
fn lower_tool(tool: &ToolDefinition) -> ResponsesTool {
    ResponsesTool {
        kind: "function",
        name: tool.name.clone(),
        description: tool.description.clone().unwrap_or_default(),
        parameters: tool.input_schema.clone(),
        strict: false,
    }
}

/// Lower the tool-choice policy (`lowerToolChoice`,
/// `openai-responses.ts:268-274`).
fn lower_tool_choice(choice: &ToolChoice) -> ResponsesToolChoice {
    match choice {
        ToolChoice::Auto => ResponsesToolChoice::Mode("auto"),
        ToolChoice::None => ResponsesToolChoice::Mode("none"),
        ToolChoice::Required => ResponsesToolChoice::Mode("required"),
        ToolChoice::Tool { name } => ResponsesToolChoice::Function {
            kind: "function",
            name: name.clone(),
        },
    }
}

/// Lower one user content part (`lowerUserContent`,
/// `openai-responses.ts:308-321`).
fn lower_user_content(part: &ContentPart) -> Result<InputContent, LLMError> {
    match part {
        ContentPart::Text { text, .. } => Ok(InputContent::InputText { text: text.clone() }),
        ContentPart::Media {
            media_type, data, ..
        } => Ok(InputContent::InputImage {
            image_url: lower_media(media_type, data)?,
        }),
        _ => Err(unsupported("user", &["text", "media"])),
    }
}

/// Render a non-`content` tool result as text (`ProviderShared.toolResultText`,
/// shared logic with `openai_chat::tool_result_text`).
fn tool_result_text(result: &ToolResultValue) -> String {
    match result {
        ToolResultValue::Text { value } => json_to_plain_string(value),
        ToolResultValue::Error { value } => {
            if value.is_object() || value.is_array() {
                serde_json::to_string(value).unwrap_or_default()
            } else {
                json_to_plain_string(value)
            }
        }
        ToolResultValue::Json { value } => serde_json::to_string(value).unwrap_or_default(),
        // `content` results are handled by the caller; never reach here.
        ToolResultValue::Content { .. } => String::new(),
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

/// Lower one tool-result content block (`lowerToolResultContentItem`,
/// `openai-responses.ts:325-335`). Text blocks stay `input_text`; everything
/// else is treated as a `file` block and validated against the image
/// MIME allow-list.
fn lower_tool_result_content_item(item: &Json) -> Result<InputContent, LLMError> {
    if item.get("type").and_then(Json::as_str) == Some("text") {
        let text = item
            .get("text")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        return Ok(InputContent::InputText { text });
    }
    let mime = item.get("mime").and_then(Json::as_str).unwrap_or_default();
    let uri = item.get("uri").and_then(Json::as_str).unwrap_or_default();
    Ok(InputContent::InputImage {
        image_url: lower_media(mime, uri)?,
    })
}

/// Lower a tool result's `output` (`lowerToolResultOutput`,
/// `openai-responses.ts:337-344`). Text/json/error results flatten to a
/// string (`tool_result_text`); `content` results become an ordered array.
fn lower_tool_result_output(result: &ToolResultValue) -> Result<FunctionCallOutput, LLMError> {
    match result {
        ToolResultValue::Content { value } => {
            let items = value
                .iter()
                .map(lower_tool_result_content_item)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(FunctionCallOutput::Content(items))
        }
        other => Ok(FunctionCallOutput::Text(tool_result_text(other))),
    }
}

/// Wrap a chronological system update as visible user text
/// (`ProviderShared.wrappedSystemUpdate`, shared logic with
/// `openai_chat::wrapped_system_update`).
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

/// Flush buffered assistant text into one `assistant` output-text item
/// (`flushText`, `openai-responses.ts:375-379`). A no-op when the buffer is
/// empty.
fn flush_assistant_text(items: &mut Vec<InputItem>, buffer: &mut Vec<String>) {
    if buffer.is_empty() {
        return;
    }
    let content = buffer
        .drain(..)
        .map(|text| OutputContent::OutputText { text })
        .collect();
    items.push(InputItem::Assistant {
        role: "assistant",
        content,
    });
}

/// Lower the full message list (`lowerMessages`, `openai-responses.ts:346-454`).
///
/// `Reasoning` input parts are dropped entirely (v1 scope, no hosted-item
/// replay yet); provider-executed (hosted) tool-call/tool-result parts are
/// skipped for the same reason.
fn lower_messages(req: &LLMRequest) -> Result<Vec<InputItem>, LLMError> {
    let mut items: Vec<InputItem> = Vec::new();
    if !req.system.is_empty() {
        let content = req
            .system
            .iter()
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        items.push(InputItem::System {
            role: "system",
            content,
        });
    }

    for message in &req.messages {
        match message.role {
            Role::System => {
                let text = wrapped_system_update(&message.content)?;
                match items.last_mut() {
                    Some(InputItem::User { content, .. }) => {
                        content.push(InputContent::InputText { text });
                    }
                    _ => items.push(InputItem::User {
                        role: "user",
                        content: vec![InputContent::InputText { text }],
                    }),
                }
            }
            Role::User => {
                let content = message
                    .content
                    .iter()
                    .map(lower_user_content)
                    .collect::<Result<Vec<_>, _>>()?;
                items.push(InputItem::User {
                    role: "user",
                    content,
                });
            }
            Role::Assistant => {
                let mut text_buffer: Vec<String> = Vec::new();
                for part in &message.content {
                    match part {
                        ContentPart::Text { text, .. } => text_buffer.push(text.clone()),
                        ContentPart::Reasoning { .. } => {
                            flush_assistant_text(&mut items, &mut text_buffer);
                            // Dropped in v1 scope.
                        }
                        ContentPart::ToolCall {
                            id,
                            name,
                            input,
                            provider_executed,
                        } => {
                            flush_assistant_text(&mut items, &mut text_buffer);
                            if *provider_executed == Some(true) {
                                // Hosted (provider-executed) tool call; replay deferred.
                                continue;
                            }
                            items.push(InputItem::FunctionCall {
                                kind: "function_call",
                                call_id: id.clone(),
                                name: name.clone(),
                                arguments: serde_json::to_string(input)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            });
                        }
                        ContentPart::ToolResult {
                            provider_executed: Some(true),
                            ..
                        } => {
                            // Hosted tool result; replay deferred.
                            flush_assistant_text(&mut items, &mut text_buffer);
                        }
                        _ => {
                            return Err(unsupported(
                                "assistant",
                                &["text", "reasoning", "tool-call", "tool-result"],
                            ));
                        }
                    }
                }
                flush_assistant_text(&mut items, &mut text_buffer);
            }
            Role::Tool => {
                for part in &message.content {
                    let ContentPart::ToolResult { id, result, .. } = part else {
                        return Err(unsupported("tool", &["tool-result"]));
                    };
                    items.push(InputItem::FunctionCallOutputItem {
                        kind: "function_call_output",
                        call_id: id.clone(),
                        output: lower_tool_result_output(result)?,
                    });
                }
            }
        }
    }
    Ok(items)
}

/// Resolve `provider_options.openai.store`, defaulting to `Some(false)` when
/// absent.
///
/// Shared by [`lower_options`] (Task 1's request body, which always sends an
/// explicit wire `store` value) and [`initial`] (Task 2's stream parser,
/// whose reasoning-summary-index machine gates eager-conclude behavior on
/// this same flag) so the two halves of this protocol can never read `store`
/// differently.
fn store_option(req: &LLMRequest) -> Option<bool> {
    Some(
        req.provider_options
            .as_ref()
            .and_then(|opts| opts.get("openai"))
            .and_then(|o| o.get("store"))
            .and_then(Json::as_bool)
            .unwrap_or(false),
    )
}

/// Options resolved from `provider_options["openai"]`
/// (`lowerOptions`'s return value, `openai-responses.ts:456-476`).
struct LoweredOptions {
    store: Option<bool>,
    prompt_cache_key: Option<String>,
    reasoning: Option<ReasoningOptions>,
    text: Option<TextOptions>,
    include: Option<Vec<String>>,
    instructions: Option<String>,
    service_tier: Option<String>,
}

/// Resolve provider-scoped options (`lowerOptions`, `openai-responses.ts:456-476`,
/// reading via the same `provider_options["openai"]` accessor style as
/// `openai_chat::lower_options`).
///
/// `store` defaults to `Some(false)` when absent (we always send an explicit
/// `store` value; unlike `openai_chat`, whose `store` is omitted entirely
/// when unset). `reasoningEffort` validation is strict: unlike `openai_chat`
/// (which silently drops an unrecognized effort), an unrecognized value here
/// is a hard error.
fn lower_options(req: &LLMRequest) -> Result<LoweredOptions, LLMError> {
    let openai = req
        .provider_options
        .as_ref()
        .and_then(|opts| opts.get("openai"));

    let store = store_option(req);

    let prompt_cache_key = openai
        .and_then(|o| o.get("promptCacheKey"))
        .and_then(Json::as_str)
        .map(str::to_string);

    let effort = openai
        .and_then(|o| o.get("reasoningEffort"))
        .and_then(Json::as_str);
    if let Some(effort) = effort
        && !REASONING_EFFORTS.contains(&effort)
    {
        return Err(LLMError::Validation(format!(
            "OpenAI Responses does not support reasoning effort {effort}"
        )));
    }
    let effort = effort.map(str::to_string);

    let summary = openai
        .and_then(|o| o.get("reasoningSummary"))
        .and_then(Json::as_str)
        .filter(|s| *s == "auto")
        .map(str::to_string);

    let reasoning = if effort.is_some() || summary.is_some() {
        Some(ReasoningOptions { effort, summary })
    } else {
        None
    };

    let include = openai
        .and_then(|o| o.get("include"))
        .and_then(Json::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty());

    let verbosity = openai
        .and_then(|o| o.get("textVerbosity"))
        .and_then(Json::as_str)
        .map(str::to_string);
    let text = verbosity.map(|verbosity| TextOptions {
        verbosity: Some(verbosity),
    });

    let instructions = openai
        .and_then(|o| o.get("instructions"))
        .and_then(Json::as_str)
        .map(str::to_string);

    let service_tier = openai
        .and_then(|o| o.get("serviceTier"))
        .and_then(Json::as_str)
        .map(str::to_string);

    Ok(LoweredOptions {
        store,
        prompt_cache_key,
        reasoning,
        text,
        include,
        instructions,
        service_tier,
    })
}

/// Build the full request body (`fromRequest`, `openai-responses.ts:478-498`).
fn build_body(req: &LLMRequest) -> Result<OpenAIResponsesBody, LLMError> {
    let input = lower_messages(req)?;
    let tools = if req.tools.is_empty() {
        None
    } else {
        Some(req.tools.iter().map(lower_tool).collect())
    };
    let tool_choice = req.tool_choice.as_ref().map(lower_tool_choice);
    let options = lower_options(req)?;
    let generation = req.generation.as_ref();
    Ok(OpenAIResponsesBody {
        model: req.model.id.0.clone(),
        input,
        instructions: options.instructions,
        tools,
        tool_choice,
        stream: true,
        store: options.store,
        prompt_cache_key: options.prompt_cache_key,
        service_tier: options.service_tier,
        include: options.include,
        reasoning: options.reasoning,
        text: options.text,
        max_output_tokens: generation.and_then(|g| g.max_tokens),
        temperature: generation.and_then(|g| g.temperature),
        top_p: generation.and_then(|g| g.top_p),
    })
}

// =============================================================================
// Stream Parsing (openai-responses.ts:500-949, minus HOSTED_TOOLS /
// hostedToolEvents / isHostedToolItem and the hosted-tool branch of
// onOutputItemDone — provider-executed (hosted) tool replay is deferred).
// =============================================================================

/// `usage.input_tokens_details` (`OpenAIResponsesUsage`, `openai-responses.ts:168-179`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

/// `usage.output_tokens_details` (`OpenAIResponsesUsage`, `openai-responses.ts:168-179`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

/// The streamed `usage` block (`OpenAIResponsesUsage`, `openai-responses.ts:168-179`).
///
/// OpenAI Responses reports `input_tokens` (inclusive total) with a
/// `cached_tokens` subset, and `output_tokens` (inclusive total) with a
/// `reasoning_tokens` subset — the totals pass through and the non-cached
/// breakdown is derived ([`map_usage`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetails>,
    #[serde(default)]
    total_tokens: Option<u64>,
}

/// One streamed `item` payload (`OpenAIResponsesStreamItem`,
/// `openai-responses.ts:181-206`; hosted-tool-only fields intentionally
/// omitted — see module docs).
#[derive(Debug, Clone, Deserialize)]
struct StreamItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    encrypted_content: Option<String>,
}

/// A nested provider error payload (`OpenAIResponsesErrorPayload`,
/// `openai-responses.ts:208-212`).
#[derive(Debug, Clone, Deserialize)]
struct ErrorPayload {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    /// Captured for wire-shape fidelity with `openai-responses.ts:208-212`;
    /// not yet surfaced in [`LLMEvent::ProviderError`].
    #[serde(default)]
    #[allow(dead_code)]
    param: Option<String>,
}

/// `response.incomplete_details` (`openai-responses.ts:214-216`).
#[derive(Debug, Clone, Deserialize)]
struct IncompleteDetails {
    reason: String,
}

/// The `response` object carried by finish/failure events
/// (`OpenAIResponsesResponseObject`, `openai-responses.ts:218-225`).
#[derive(Debug, Clone, Deserialize)]
struct ResponseObject {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    service_tier: Option<String>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
    #[serde(default)]
    error: Option<ErrorPayload>,
}

/// One decoded SSE event (`OpenAIResponsesEvent`, `openai-responses.ts:227-234`).
/// All fields except `type` are optional: the wire union carries many event
/// shapes, and this struct is deliberately the union of every field any of
/// them use.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIResponsesEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    summary_index: Option<u64>,
    #[serde(default)]
    item: Option<StreamItem>,
    #[serde(default)]
    response: Option<ResponseObject>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    /// Captured for wire-shape fidelity with `openai-responses.ts:227-234`;
    /// not yet surfaced in [`LLMEvent::ProviderError`].
    #[serde(default)]
    #[allow(dead_code)]
    param: Option<String>,
}

// =============================================================================
// Parser State (openai-responses.ts:236-252)
// =============================================================================

/// One in-flight reasoning-summary part's lifecycle status
/// (`openai-responses.ts:236-240`). OpenAI Responses can open a *new*
/// summary-index before conclusively closing the previous one; `CanConclude`
/// marks a part as "closeable, but not yet closed" so a later event (either
/// the next summary part opening, or `output_item.done`) can retroactively
/// decide whether it truly ends there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasoningSummaryStatus {
    /// Currently streaming.
    Active,
    /// Its `summary_part.done` arrived, but `store` is disabled so the
    /// `reasoning-end` event is deferred until the next part opens or the
    /// item finishes.
    CanConclude,
    /// Closed — a matching `reasoning-end` has been emitted.
    Concluded,
}

/// Per-reasoning-item tracking (`openai-responses.ts:242-245`).
#[derive(Debug, Clone)]
struct ReasoningStreamItem {
    /// The item's encrypted reasoning content, when the caller requested it
    /// via `include: ["reasoning.encrypted_content"]`.
    encrypted_content: Option<String>,
    /// Status of each summary-index part seen for this item.
    summary_parts: std::collections::BTreeMap<u64, ReasoningSummaryStatus>,
}

/// Per-stream accumulator (`ParserState`, `openai-responses.ts:247-252`).
pub struct ParserState {
    /// The streaming tool-call accumulator, keyed by the function-call
    /// item's own `id` (not its `call_id`).
    tools: tool_stream::State<String>,
    /// Whether any function-call tool has been finalized this stream — folds
    /// into [`map_finish_reason`]'s `stop` → `tool-calls` coercion.
    has_function_call: bool,
    /// Text/reasoning/step lifecycle machine.
    lifecycle: lifecycle::State,
    /// Per-item reasoning-summary tracking, keyed by the reasoning item's id.
    reasoning_items: std::collections::BTreeMap<String, ReasoningStreamItem>,
    /// `provider_options.openai.store`, resolved once via [`store_option`].
    store: Option<bool>,
}

// =============================================================================
// Stream Parsing helpers
// =============================================================================

/// Wrap a field bag as OpenAI provider metadata (`openaiMetadata`,
/// `openai-responses.ts:531`).
fn openai_metadata(fields: serde_json::Value) -> ProviderMetadata {
    serde_json::json!({ "openai": fields })
}

/// Provider metadata for a reasoning item/summary event (`reasoningMetadata`,
/// `openai-responses.ts:641-642`).
fn reasoning_metadata(id: &str, encrypted_content: Option<&str>) -> ProviderMetadata {
    openai_metadata(serde_json::json!({
        "itemId": id,
        "reasoningEncryptedContent": encrypted_content,
    }))
}

/// Whether a stream item is a reasoning item with a usable id
/// (`isReasoningItem`, `openai-responses.ts:570-573`).
fn is_reasoning_item(item: &StreamItem) -> bool {
    item.kind == "reasoning" && item.id.as_deref().is_some_and(|id| !id.is_empty())
}

/// `Math.max(0, total - subtrahend)` token subtraction
/// (`ProviderShared.subtractTokens`), duplicated per-protocol like
/// `openai_chat::subtract_tokens` — no shared helper module exists yet.
fn subtract_tokens(total: Option<u64>, subtrahend: Option<u64>) -> Option<u64> {
    match (total, subtrahend) {
        (None, _) => None,
        (Some(total), None) => Some(total),
        (Some(total), Some(sub)) => Some(total.saturating_sub(sub)),
    }
}

/// Provider total, else `input + output` when at least one is present
/// (`ProviderShared.totalTokens`).
fn total_tokens(input: Option<u64>, output: Option<u64>, total: Option<u64>) -> Option<u64> {
    if let Some(total) = total {
        return Some(total);
    }
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(input.unwrap_or(0) + output.unwrap_or(0))
}

/// Map the streamed `usage` block into the additive [`Usage`] contract
/// (`mapUsage`, `openai-responses.ts:507-521`).
fn map_usage(usage: Option<&ResponsesUsage>) -> Option<Usage> {
    let usage = usage?;
    let cached = usage
        .input_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens);
    let reasoning = usage
        .output_tokens_details
        .as_ref()
        .and_then(|d| d.reasoning_tokens);
    let non_cached = subtract_tokens(usage.input_tokens, cached);
    let metadata = serde_json::json!({
        "openai": serde_json::to_value(usage).unwrap_or(Json::Null),
    });
    Some(Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        non_cached_input_tokens: non_cached,
        cache_read_input_tokens: cached,
        cache_write_input_tokens: None,
        reasoning_tokens: reasoning,
        total_tokens: total_tokens(usage.input_tokens, usage.output_tokens, usage.total_tokens),
        provider_metadata: Some(metadata),
    })
}

/// Map the terminal event's incomplete-reason (or its absence) to a
/// [`FinishReason`] (`mapFinishReason`, `openai-responses.ts:523-529`).
fn map_finish_reason(event: &OpenAIResponsesEvent, has_function_call: bool) -> FinishReason {
    let reason = event
        .response
        .as_ref()
        .and_then(|r| r.incomplete_details.as_ref())
        .map(|d| d.reason.as_str());
    match reason {
        None => {
            if has_function_call {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            }
        }
        Some("max_output_tokens") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        Some(_) => {
            if has_function_call {
                FinishReason::ToolCalls
            } else {
                FinishReason::Unknown
            }
        }
    }
}

/// OpenAI Responses context-overflow detection. Port of `isContextOverflow`
/// (`provider-error.ts:26-27`), duplicated per-protocol like
/// `anthropic_messages::is_context_overflow` (case-insensitive substring
/// checks, no regex dependency).
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
    (lower.starts_with("400") || lower.starts_with("413")) && lower.contains("no body")
}

/// Build a single human-readable message from whatever the provider supplied
/// (`providerErrorMessage`, `openai-responses.ts:896-902`). When both a code
/// and message are present the code is prefixed so consumers can distinguish
/// e.g. `rate_limit_exceeded: Slow down` from a bare generic message.
fn provider_error_message(event: &OpenAIResponsesEvent, fallback: &str) -> String {
    let nested = event.response.as_ref().and_then(|r| r.error.as_ref());
    let message = event
        .message
        .clone()
        .or_else(|| nested.and_then(|n| n.message.clone()));
    let code = event
        .code
        .clone()
        .or_else(|| nested.and_then(|n| n.code.clone()));
    match (message, code) {
        (Some(m), Some(c)) => format!("{c}: {m}"),
        (Some(m), None) => m,
        (None, Some(c)) => c,
        (None, None) => fallback.to_string(),
    }
}

/// Build a `provider-error` event, classifying context-overflow failures
/// (`providerError`, `openai-responses.ts:904-911`).
fn provider_error(event: &OpenAIResponsesEvent, fallback: &str) -> LLMEvent {
    let code = event.code.clone().or_else(|| {
        event
            .response
            .as_ref()
            .and_then(|r| r.error.as_ref())
            .and_then(|e| e.code.clone())
    });
    let message = provider_error_message(event, fallback);
    let classification =
        if code.as_deref() == Some("context_length_exceeded") || is_context_overflow(&message) {
            Some(ProviderFailureClassification::ContextOverflow)
        } else {
            None
        };
    LLMEvent::ProviderError {
        message,
        classification,
        retryable: None,
        provider_metadata: None,
    }
}

// =============================================================================
// Stream Parsing handlers (openai-responses.ts:615-921)
// =============================================================================

/// `response.output_text.delta` (`onOutputTextDelta`, `openai-responses.ts:615-622`).
fn on_output_text_delta(state: &mut ParserState, event: &OpenAIResponsesEvent) -> Vec<LLMEvent> {
    let Some(delta) = event.delta.as_deref().filter(|d| !d.is_empty()) else {
        return Vec::new();
    };
    let id = event.item_id.as_deref().unwrap_or("text-0");
    let mut events = state.lifecycle.step_start(0);
    events.extend(state.lifecycle.text_delta(id, delta));
    events
}

/// `response.reasoning*.delta` (`onReasoningDelta`, `openai-responses.ts:624-637`).
///
/// Uses a bare `item_id` while the item is still on its first (index-0)
/// summary part; once a `summary_index` is observed (or the item is already
/// tracked, meaning a summary part has been seen), switches to the composite
/// `"{item_id}:{index}"` id so concurrent summary parts don't collide.
fn on_reasoning_delta(state: &mut ParserState, event: &OpenAIResponsesEvent) -> Vec<LLMEvent> {
    let Some(delta) = event.delta.as_deref().filter(|d| !d.is_empty()) else {
        return Vec::new();
    };
    let item_id = event.item_id.as_deref().unwrap_or("reasoning-0");
    let id = if event.summary_index.is_some() || state.reasoning_items.contains_key(item_id) {
        format!("{item_id}:{}", event.summary_index.unwrap_or(0))
    } else {
        item_id.to_string()
    };
    let mut events = state.lifecycle.step_start(0);
    events.extend(state.lifecycle.reasoning_delta(&id, delta));
    events
}

/// `response.output_item.added` (`onOutputItemAdded`, `openai-responses.ts:656-690`).
fn on_output_item_added(state: &mut ParserState, event: &OpenAIResponsesEvent) -> Vec<LLMEvent> {
    let Some(item) = event.item.as_ref() else {
        return Vec::new();
    };

    if is_reasoning_item(item) {
        let id = item.id.clone().expect("checked by is_reasoning_item");
        let metadata = reasoning_metadata(&id, item.encrypted_content.as_deref());
        let mut events = state.lifecycle.step_start(0);
        events.extend(
            state
                .lifecycle
                .reasoning_start(&format!("{id}:0"), Some(metadata)),
        );
        let mut summary_parts = std::collections::BTreeMap::new();
        summary_parts.insert(0, ReasoningSummaryStatus::Active);
        state.reasoning_items.insert(
            id,
            ReasoningStreamItem {
                encrypted_content: item.encrypted_content.clone(),
                summary_parts,
            },
        );
        return events;
    }

    if item.kind != "function_call" {
        return Vec::new();
    }
    let Some(id) = item.id.clone().filter(|id| !id.is_empty()) else {
        return Vec::new();
    };
    let call_id = item.call_id.clone().unwrap_or_else(|| id.clone());
    let name = item.name.clone().unwrap_or_default();
    let provider_metadata = openai_metadata(serde_json::json!({ "itemId": id }));
    let mut events = state.lifecycle.step_start(0);
    events.extend(
        state
            .tools
            .start(id, call_id, name, None, Some(provider_metadata)),
    );
    events
}

/// `response.reasoning_summary_part.added` (`onReasoningSummaryPartAdded`,
/// `openai-responses.ts:692-755`).
fn on_reasoning_summary_part_added(
    state: &mut ParserState,
    event: &OpenAIResponsesEvent,
) -> Vec<LLMEvent> {
    let (Some(item_id), Some(summary_index)) = (event.item_id.clone(), event.summary_index) else {
        return Vec::new();
    };

    if summary_index == 0 {
        // Already seeded by `output_item.added` — avoid re-opening index 0.
        if state.reasoning_items.contains_key(&item_id) {
            return Vec::new();
        }
        let mut events = state.lifecycle.step_start(0);
        let metadata = reasoning_metadata(&item_id, None);
        events.extend(
            state
                .lifecycle
                .reasoning_start(&format!("{item_id}:0"), Some(metadata)),
        );
        let mut summary_parts = std::collections::BTreeMap::new();
        summary_parts.insert(0, ReasoningSummaryStatus::Active);
        state.reasoning_items.insert(
            item_id,
            ReasoningStreamItem {
                encrypted_content: None,
                summary_parts,
            },
        );
        return events;
    }

    // Read (and, if absent, seed) the existing entry's closeable indices and
    // encrypted content, then drop the borrow before touching other `state`
    // fields (the lifecycle calls below need `&mut state.lifecycle`).
    let (close_indices, encrypted_content): (Vec<u64>, Option<String>) = {
        let item = state
            .reasoning_items
            .entry(item_id.clone())
            .or_insert_with(|| ReasoningStreamItem {
                encrypted_content: None,
                summary_parts: std::collections::BTreeMap::new(),
            });
        let close_indices = item
            .summary_parts
            .iter()
            .filter(|(_, status)| **status == ReasoningSummaryStatus::CanConclude)
            .map(|(idx, _)| *idx)
            .collect();
        (close_indices, item.encrypted_content.clone())
    };

    let mut events = state.lifecycle.step_start(0);
    for idx in close_indices {
        events.extend(state.lifecycle.reasoning_end_with_metadata(
            &format!("{item_id}:{idx}"),
            Some(reasoning_metadata(&item_id, None)),
        ));
    }
    events.extend(state.lifecycle.reasoning_start(
        &format!("{item_id}:{summary_index}"),
        Some(reasoning_metadata(&item_id, encrypted_content.as_deref())),
    ));

    let item = state
        .reasoning_items
        .get_mut(&item_id)
        .expect("inserted/seeded above");
    for status in item.summary_parts.values_mut() {
        if *status == ReasoningSummaryStatus::CanConclude {
            *status = ReasoningSummaryStatus::Concluded;
        }
    }
    item.summary_parts
        .insert(summary_index, ReasoningSummaryStatus::Active);

    events
}

/// `response.reasoning_summary_part.done` (`onReasoningSummaryPartDone`,
/// `openai-responses.ts:757-787`).
///
/// When `store` is enabled the summary part concludes immediately
/// (`reasoning-end` fires now); otherwise it's marked `CanConclude` so the
/// next summary part (or `output_item.done`) decides whether it truly ends.
fn on_reasoning_summary_part_done(
    state: &mut ParserState,
    event: &OpenAIResponsesEvent,
) -> Vec<LLMEvent> {
    let (Some(item_id), Some(summary_index)) = (event.item_id.clone(), event.summary_index) else {
        return Vec::new();
    };
    if !state.reasoning_items.contains_key(&item_id) {
        return Vec::new();
    }

    let mut events = Vec::new();
    let eager_conclude = state.store != Some(false);
    let status = if eager_conclude {
        events.extend(state.lifecycle.reasoning_end_with_metadata(
            &format!("{item_id}:{summary_index}"),
            Some(reasoning_metadata(&item_id, None)),
        ));
        ReasoningSummaryStatus::Concluded
    } else {
        ReasoningSummaryStatus::CanConclude
    };
    if let Some(item) = state.reasoning_items.get_mut(&item_id) {
        item.summary_parts.insert(summary_index, status);
    }
    events
}

/// `response.function_call_arguments.delta` (`onFunctionCallArgumentsDelta`,
/// `openai-responses.ts:789-806`).
fn on_function_call_arguments_delta(
    state: &mut ParserState,
    event: &OpenAIResponsesEvent,
) -> Result<Vec<LLMEvent>, LLMError> {
    let Some(item_id) = event.item_id.clone() else {
        return Ok(Vec::new());
    };
    let Some(delta) = event.delta.as_deref().filter(|d| !d.is_empty()) else {
        return Ok(Vec::new());
    };
    let produced = state.tools.append_existing(&item_id, delta)?;
    let mut events = Vec::new();
    if !produced.is_empty() {
        events.extend(state.lifecycle.step_start(0));
    }
    events.extend(produced);
    Ok(events)
}

/// `response.output_item.done` (`onOutputItemDone`, `openai-responses.ts:808-873`,
/// minus the hosted-tool branch).
fn on_output_item_done(
    state: &mut ParserState,
    event: &OpenAIResponsesEvent,
) -> Result<Vec<LLMEvent>, LLMError> {
    let Some(item) = event.item.clone() else {
        return Ok(Vec::new());
    };

    if item.kind == "function_call" {
        let (Some(id), Some(call_id), Some(name)) = (item.id, item.call_id, item.name) else {
            return Ok(Vec::new());
        };
        if !state.tools.contains(&id) {
            // Fallback registration for out-of-order streams where
            // `output_item.added` never arrived. Discard the returned
            // `tool-input-start` — this repairs internal state only, matching
            // opencode's silent `ToolStream.start` in this fallback path.
            state.tools.start(id.clone(), call_id, name, None, None);
        }
        let result_events = match item.arguments {
            None => state.tools.finish(&id)?,
            Some(args) => {
                let parsed = tool_stream::parse_tool_input(&args)?;
                state.tools.finish_with_input(&id, parsed)?
            }
        };
        let mut events = Vec::new();
        if !result_events.is_empty() {
            events.extend(state.lifecycle.step_start(0));
        }
        if result_events
            .iter()
            .any(|e| matches!(e, LLMEvent::ToolCall { .. }))
        {
            state.has_function_call = true;
        }
        events.extend(result_events);
        return Ok(events);
    }

    if is_reasoning_item(&item) {
        let id = item.id.expect("checked by is_reasoning_item");
        let provider_metadata = reasoning_metadata(&id, item.encrypted_content.as_deref());

        if let Some(reasoning_item) = state.reasoning_items.remove(&id) {
            let mut events = Vec::new();
            let open_indices: Vec<u64> = reasoning_item
                .summary_parts
                .iter()
                .filter(|(_, status)| {
                    matches!(
                        status,
                        ReasoningSummaryStatus::Active | ReasoningSummaryStatus::CanConclude
                    )
                })
                .map(|(idx, _)| *idx)
                .collect();
            for idx in open_indices {
                events.extend(state.lifecycle.reasoning_end_with_metadata(
                    &format!("{id}:{idx}"),
                    Some(provider_metadata.clone()),
                ));
            }
            return Ok(events);
        }

        if !state.lifecycle.is_reasoning_open(&id) {
            // The item never streamed any delta at all (empty reasoning) —
            // synthesize a start/end pair directly so consumers still learn
            // it existed.
            let mut events = state.lifecycle.step_start(0);
            events.push(LLMEvent::ReasoningStart {
                id: id.clone(),
                provider_metadata: Some(provider_metadata.clone()),
            });
            events.push(LLMEvent::ReasoningEnd {
                id,
                provider_metadata: Some(provider_metadata),
            });
            return Ok(events);
        }

        return Ok(state
            .lifecycle
            .reasoning_end_with_metadata(&id, Some(provider_metadata)));
    }

    // Anything else — including hosted (provider-executed) tool items — is a
    // no-op. Hosted-tool replay is deferred (see module docs).
    Ok(Vec::new())
}

/// `response.completed` / `response.incomplete` (`onResponseFinish`,
/// `openai-responses.ts:875-889`).
fn on_response_finish(state: &mut ParserState, event: &OpenAIResponsesEvent) -> Vec<LLMEvent> {
    let response = event.response.as_ref();
    let reason = map_finish_reason(event, state.has_function_call);
    let usage = map_usage(response.and_then(|r| r.usage.as_ref()));
    let provider_metadata = response
        .filter(|r| r.id.is_some() || r.service_tier.is_some())
        .map(|r| {
            openai_metadata(serde_json::json!({
                "responseId": r.id,
                "serviceTier": r.service_tier,
            }))
        });
    state
        .lifecycle
        .finish_with_metadata(reason, usage, 0, provider_metadata)
}

// =============================================================================
// step / initial / is_terminal (openai-responses.ts:923-949)
// =============================================================================

/// Stream event types that end the response — `response.completed` /
/// `response.incomplete` are clean finishes; `response.failed` is a hard
/// failure. Kept in one place so [`step`] and [`is_terminal`] stay in sync
/// (`TERMINAL_TYPES`, `openai-responses.ts:613`).
const TERMINAL_TYPES: [&str; 3] = [
    "response.completed",
    "response.incomplete",
    "response.failed",
];

/// Whether `event` ends the stream (the protocol's `terminal` predicate,
/// backed by [`TERMINAL_TYPES`]).
pub fn is_terminal(event: &OpenAIResponsesEvent) -> bool {
    TERMINAL_TYPES.contains(&event.kind.as_str())
}

/// Fold one streamed event into neutral [`LLMEvent`]s (`step`,
/// `openai-responses.ts:923-949`).
pub fn step(
    state: &mut ParserState,
    event: OpenAIResponsesEvent,
) -> Result<Vec<LLMEvent>, LLMError> {
    match event.kind.as_str() {
        "response.output_text.delta" => Ok(on_output_text_delta(state, &event)),
        "response.reasoning_text.delta"
        | "response.reasoning_summary.delta"
        | "response.reasoning_summary_text.delta" => Ok(on_reasoning_delta(state, &event)),
        // `*.done` variants of the reasoning-text/summary deltas: the final
        // text was already captured by the preceding deltas, so these are
        // no-ops (`onReasoningDone`, `openai-responses.ts:639`).
        "response.reasoning_text.done"
        | "response.reasoning_summary.done"
        | "response.reasoning_summary_text.done" => Ok(Vec::new()),
        "response.reasoning_summary_part.added" => {
            Ok(on_reasoning_summary_part_added(state, &event))
        }
        "response.reasoning_summary_part.done" => Ok(on_reasoning_summary_part_done(state, &event)),
        "response.output_item.added" => Ok(on_output_item_added(state, &event)),
        "response.function_call_arguments.delta" => on_function_call_arguments_delta(state, &event),
        "response.output_item.done" => on_output_item_done(state, &event),
        "response.completed" | "response.incomplete" => Ok(on_response_finish(state, &event)),
        "response.failed" => Ok(vec![provider_error(
            &event,
            "OpenAI Responses response failed",
        )]),
        "error" => Ok(vec![provider_error(
            &event,
            "OpenAI Responses stream error",
        )]),
        _ => Ok(Vec::new()),
    }
}

/// The initial parser state (`initial`, folding `ParserState`'s shape +
/// `Lifecycle.initial` + `ToolStream.empty`, `openai-responses.ts:247-252`).
pub fn initial(req: &LLMRequest) -> ParserState {
    ParserState {
        tools: tool_stream::State::initial(),
        has_function_call: false,
        lifecycle: lifecycle::State::initial(),
        reasoning_items: std::collections::BTreeMap::new(),
        store: store_option(req),
    }
}

// =============================================================================
// Protocol impl (Task 3)
// =============================================================================

/// The OpenAI Responses protocol.
///
/// Port of the `protocol` export in `openai-responses.ts` (mirrors
/// [`crate::protocols::openai_chat::OpenAIChat`]'s `Protocol` impl shape).
pub struct OpenAIResponses;

impl crate::protocol::Protocol for OpenAIResponses {
    type Body = OpenAIResponsesBody;
    type Event = OpenAIResponsesEvent;
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

    fn initial(&self, req: &LLMRequest) -> Self::State {
        initial(req)
    }

    fn step(&self, state: &mut Self::State, event: Self::Event) -> Result<Vec<LLMEvent>, LLMError> {
        step(state, event)
    }

    fn terminal(&self, event: &Self::Event) -> bool {
        is_terminal(event)
    }
}

/// Whether `model_id` should be routed to the OpenAI Responses API rather
/// than OpenAI Chat Completions: `gpt-N` where `N >= 5`, excluding the
/// `gpt-5-mini` family. Port of github-copilot.ts `shouldUseResponsesApi`.
#[must_use]
pub fn should_use_responses(model_id: &str) -> bool {
    let Some(rest) = model_id.strip_prefix("gpt-") else {
        return false;
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let Ok(major) = digits.parse::<u32>() else {
        return false;
    };
    major >= 5 && !model_id.starts_with("gpt-5-mini")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use crate::model::Model;
    use crate::protocol::Protocol;
    use serde_json::json;

    /// Minimal `Model` construction for tests (mirrors
    /// `openai_chat::tests::request`'s `Model::new` call).
    fn test_model() -> Model {
        Model::new("openai", "gpt-4o", "openai-responses")
    }

    #[test]
    fn constants_match_opencode_wire_values() {
        assert_eq!(ADAPTER, "openai-responses");
        assert_eq!(PATH, "/responses");
        assert_eq!(DEFAULT_BASE_URL, "https://api.openai.com/v1");
    }

    #[test]
    fn body_lowers_system_user_assistant_tool() {
        #[allow(unused_imports)] // `Role` is already in scope via `use super::*`.
        use crate::message::{ContentPart, Message, Role};
        use otto_events::ToolResultValue;
        let mut req = LLMRequest::new(
            test_model(),
            vec![
                Message::user(vec![ContentPart::text("hi")]),
                Message::assistant(vec![
                    ContentPart::reasoning("thinking..."), // must be DROPPED
                    ContentPart::Text {
                        text: "hello".into(),
                        cache: None,
                    },
                    ContentPart::ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        input: serde_json::json!({"p":1}),
                        provider_executed: None,
                    },
                ]),
                Message::tool(vec![ContentPart::ToolResult {
                    id: "c1".into(),
                    name: "read".into(),
                    result: ToolResultValue::Text {
                        value: serde_json::json!("ok"),
                    },
                    provider_executed: None,
                    cache: None,
                }]),
            ],
        );
        req.system = vec![crate::message::SystemPart::new("be terse")];
        let body = build_body(&req).unwrap();
        let v = serde_json::to_value(&body).unwrap();
        let input = v["input"].as_array().unwrap();
        // system item first
        assert_eq!(input[0]["role"], "system");
        assert_eq!(input[0]["content"], "be terse");
        // user item
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["content"][0]["text"], "hi");
        // assistant output_text (reasoning dropped -> no reasoning item anywhere)
        assert_eq!(input[2]["role"], "assistant");
        assert_eq!(input[2]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["content"][0]["text"], "hello");
        // function_call
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["call_id"], "c1");
        assert_eq!(input[3]["name"], "read");
        // function_call_output
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "c1");
        assert_eq!(input[4]["output"], "ok");
        // no reasoning item leaked
        assert!(input.iter().all(|i| i["type"] != "reasoning"));
        assert_eq!(v["stream"], true);
        assert_eq!(v["store"], false); // default
    }

    #[test]
    fn store_defaults_false_and_overrides() {
        let req = LLMRequest::new(
            test_model(),
            vec![Message::user(vec![ContentPart::text("hi")])],
        );
        let body = build_body(&req).unwrap();
        assert_eq!(body.store, Some(false));

        let mut req = req;
        let mut openai = serde_json::Map::new();
        openai.insert("store".into(), json!(true));
        let mut opts = std::collections::BTreeMap::new();
        opts.insert("openai".to_string(), Json::Object(openai));
        req.provider_options = Some(opts);
        let body = build_body(&req).unwrap();
        assert_eq!(body.store, Some(true));
    }

    #[test]
    fn reasoning_effort_valid_and_invalid() {
        let mut req = LLMRequest::new(
            test_model(),
            vec![Message::user(vec![ContentPart::text("hi")])],
        );
        let mut openai = serde_json::Map::new();
        openai.insert("reasoningEffort".into(), json!("high"));
        let mut opts = std::collections::BTreeMap::new();
        opts.insert("openai".to_string(), Json::Object(openai));
        req.provider_options = Some(opts);
        let body = build_body(&req).unwrap();
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["reasoning"]["effort"], "high");

        let mut openai = serde_json::Map::new();
        openai.insert("reasoningEffort".into(), json!("max"));
        let mut opts = std::collections::BTreeMap::new();
        opts.insert("openai".to_string(), Json::Object(openai));
        req.provider_options = Some(opts);
        assert!(build_body(&req).is_err());
    }

    #[test]
    fn options_map_through() {
        let mut req = LLMRequest::new(
            test_model(),
            vec![Message::user(vec![ContentPart::text("hi")])],
        );
        let mut openai = serde_json::Map::new();
        openai.insert("textVerbosity".into(), json!("high"));
        openai.insert("promptCacheKey".into(), json!("cache-key"));
        openai.insert("instructions".into(), json!("be nice"));
        openai.insert("serviceTier".into(), json!("flex"));
        openai.insert("include".into(), json!(["reasoning.encrypted_content"]));
        let mut opts = std::collections::BTreeMap::new();
        opts.insert("openai".to_string(), Json::Object(openai));
        req.provider_options = Some(opts);
        let body = build_body(&req).unwrap();
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["text"]["verbosity"], "high");
        assert_eq!(v["prompt_cache_key"], "cache-key");
        assert_eq!(v["instructions"], "be nice");
        assert_eq!(v["service_tier"], "flex");
        assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn tool_choice_variants() {
        assert_eq!(
            serde_json::to_value(lower_tool_choice(&ToolChoice::Auto)).unwrap(),
            json!("auto")
        );
        assert_eq!(
            serde_json::to_value(lower_tool_choice(&ToolChoice::Tool {
                name: "read".into()
            }))
            .unwrap(),
            json!({"type":"function","name":"read"})
        );
    }

    #[test]
    fn user_image_media_and_unsupported() {
        let req = LLMRequest::new(
            test_model(),
            vec![Message::user(vec![ContentPart::Media {
                media_type: "image/png".into(),
                data: "AAAA".into(),
                filename: None,
            }])],
        );
        let body = build_body(&req).unwrap();
        let v = serde_json::to_value(&body).unwrap();
        let image_url = v["input"][0]["content"][0]["image_url"].as_str().unwrap();
        assert!(image_url.starts_with("data:image/png;base64,"));

        let req = LLMRequest::new(
            test_model(),
            vec![Message::user(vec![ContentPart::Media {
                media_type: "application/pdf".into(),
                data: "AAAA".into(),
                filename: None,
            }])],
        );
        assert!(build_body(&req).is_err());
    }

    #[test]
    fn tool_result_content_array() {
        use otto_events::ToolResultValue;
        let req = LLMRequest::new(
            test_model(),
            vec![Message::tool(vec![ContentPart::ToolResult {
                id: "c1".into(),
                name: "read".into(),
                result: ToolResultValue::Content {
                    value: vec![json!({"type":"text","text":"ok"})],
                },
                provider_executed: None,
                cache: None,
            }])],
        );
        let body = build_body(&req).unwrap();
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(
            v["input"][0]["output"],
            json!([{"type":"input_text","text":"ok"}])
        );
    }

    // =========================================================================
    // Stream Parsing tests (Task 2)
    // =========================================================================

    fn ev(json: serde_json::Value) -> OpenAIResponsesEvent {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn text_delta_emits_text_events() {
        let req = LLMRequest::new(test_model(), vec![]);
        let mut st = initial(&req);
        let out = step(
            &mut st,
            ev(json!({
                "type":"response.output_text.delta","item_id":"m1","delta":"Hel"
            })),
        )
        .unwrap();
        assert!(
            out.iter()
                .any(|e| matches!(e, LLMEvent::TextDelta { text, .. } if text == "Hel"))
        );
    }

    #[test]
    fn reasoning_summary_roundtrip() {
        let req = LLMRequest::new(test_model(), vec![]);
        let mut st = initial(&req);
        let mut out = Vec::new();
        out.extend(
            step(
                &mut st,
                ev(json!({
                    "type":"response.output_item.added",
                    "item":{"type":"reasoning","id":"r1"}
                })),
            )
            .unwrap(),
        );
        out.extend(
            step(
                &mut st,
                ev(json!({
                    "type":"response.reasoning_summary_part.added",
                    "item_id":"r1","summary_index":0
                })),
            )
            .unwrap(),
        );
        out.extend(
            step(
                &mut st,
                ev(json!({
                    "type":"response.reasoning_summary_text.delta",
                    "item_id":"r1","delta":"why"
                })),
            )
            .unwrap(),
        );
        out.extend(
            step(
                &mut st,
                ev(json!({
                    "type":"response.output_item.done",
                    "item":{"type":"reasoning","id":"r1"}
                })),
            )
            .unwrap(),
        );
        assert!(
            out.iter()
                .any(|e| matches!(e, LLMEvent::ReasoningStart { .. }))
        );
        assert!(
            out.iter()
                .any(|e| matches!(e, LLMEvent::ReasoningDelta { text, .. } if text == "why"))
        );
        assert!(
            out.iter()
                .any(|e| matches!(e, LLMEvent::ReasoningEnd { .. }))
        );
    }

    #[test]
    fn reasoning_summary_multi_index_store_false_defers_close() {
        // Two summary indices under the default `store: false`: each
        // `summary_part.done` only marks the part `CanConclude` (deferred
        // close); the pending index actually closes when the *next* summary
        // part opens (index 1's `part.added` retroactively closes index 0),
        // and the last still-open index only closes at `output_item.done`.
        // Port of `onReasoningSummaryPartAdded` / `onReasoningSummaryPartDone`
        // (`openai-responses.ts:692-787`).
        let req = LLMRequest::new(test_model(), vec![]);
        let mut st = initial(&req);
        assert_eq!(st.store, Some(false));

        let mut out = Vec::new();
        out.extend(
            step(
                &mut st,
                ev(json!({
                    "type":"response.output_item.added",
                    "item":{"type":"reasoning","id":"r1"}
                })),
            )
            .unwrap(),
        );
        out.extend(
            step(
                &mut st,
                ev(json!({
                    "type":"response.reasoning_summary_part.added",
                    "item_id":"r1","summary_index":0
                })),
            )
            .unwrap(),
        );
        out.extend(
            step(
                &mut st,
                ev(json!({
                    "type":"response.reasoning_summary_text.delta",
                    "item_id":"r1","summary_index":0,"delta":"a"
                })),
            )
            .unwrap(),
        );
        let part_done_0 = step(
            &mut st,
            ev(json!({
                "type":"response.reasoning_summary_part.done",
                "item_id":"r1","summary_index":0
            })),
        )
        .unwrap();
        // Deferred close: `store: false` means no `reasoning-end` yet.
        assert!(
            !part_done_0
                .iter()
                .any(|e| matches!(e, LLMEvent::ReasoningEnd { .. }))
        );
        out.extend(part_done_0);

        let part_added_1 = step(
            &mut st,
            ev(json!({
                "type":"response.reasoning_summary_part.added",
                "item_id":"r1","summary_index":1
            })),
        )
        .unwrap();
        // Opening index 1 retroactively closes the deferred index 0, then
        // opens index 1.
        assert!(
            part_added_1
                .iter()
                .any(|e| matches!(e, LLMEvent::ReasoningEnd { id, .. } if id == "r1:0"))
        );
        assert!(
            part_added_1
                .iter()
                .any(|e| matches!(e, LLMEvent::ReasoningStart { id, .. } if id == "r1:1"))
        );
        out.extend(part_added_1);

        out.extend(
            step(
                &mut st,
                ev(json!({
                    "type":"response.reasoning_summary_text.delta",
                    "item_id":"r1","summary_index":1,"delta":"b"
                })),
            )
            .unwrap(),
        );
        out.extend(
            step(
                &mut st,
                ev(json!({
                    "type":"response.reasoning_summary_part.done",
                    "item_id":"r1","summary_index":1
                })),
            )
            .unwrap(),
        );
        let item_done = step(
            &mut st,
            ev(json!({
                "type":"response.output_item.done",
                "item":{"type":"reasoning","id":"r1"}
            })),
        )
        .unwrap();
        // Index 1 was still only `CanConclude` — it only closes now, at item
        // end.
        assert!(
            item_done
                .iter()
                .any(|e| matches!(e, LLMEvent::ReasoningEnd { id, .. } if id == "r1:1"))
        );
        out.extend(item_done);

        // Both summary indices' deltas made it through as distinct blocks.
        let deltas: Vec<&str> = out
            .iter()
            .filter_map(|e| match e {
                LLMEvent::ReasoningDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["a", "b"]);

        // Every opened reasoning block was closed by the time the item ended.
        let starts = out
            .iter()
            .filter(|e| matches!(e, LLMEvent::ReasoningStart { .. }))
            .count();
        let ends = out
            .iter()
            .filter(|e| matches!(e, LLMEvent::ReasoningEnd { .. }))
            .count();
        assert_eq!(starts, 2);
        assert_eq!(ends, 2);
    }

    #[test]
    fn reasoning_summary_store_true_closes_eagerly_at_part_done() {
        // With `store: true` (or any non-`false` value), `reasoning_summary_
        // part.done` closes the part immediately instead of deferring to the
        // next part/`output_item.done`. Port of `onReasoningSummaryPartDone`'s
        // `state.store !== false` branch (`openai-responses.ts:757-787`).
        let mut req = LLMRequest::new(test_model(), vec![]);
        let mut openai = serde_json::Map::new();
        openai.insert("store".into(), json!(true));
        let mut opts = std::collections::BTreeMap::new();
        opts.insert("openai".to_string(), Json::Object(openai));
        req.provider_options = Some(opts);

        let mut st = initial(&req);
        assert_eq!(st.store, Some(true));

        step(
            &mut st,
            ev(json!({
                "type":"response.output_item.added",
                "item":{"type":"reasoning","id":"r1"}
            })),
        )
        .unwrap();
        step(
            &mut st,
            ev(json!({
                "type":"response.reasoning_summary_part.added",
                "item_id":"r1","summary_index":0
            })),
        )
        .unwrap();
        step(
            &mut st,
            ev(json!({
                "type":"response.reasoning_summary_text.delta",
                "item_id":"r1","summary_index":0,"delta":"x"
            })),
        )
        .unwrap();

        let part_done = step(
            &mut st,
            ev(json!({
                "type":"response.reasoning_summary_part.done",
                "item_id":"r1","summary_index":0
            })),
        )
        .unwrap();
        // The close happens right here — before `output_item.done` — unlike
        // the `store: false` case above.
        assert!(
            part_done
                .iter()
                .any(|e| matches!(e, LLMEvent::ReasoningEnd { id, .. } if id == "r1:0"))
        );

        let item_done = step(
            &mut st,
            ev(json!({
                "type":"response.output_item.done",
                "item":{"type":"reasoning","id":"r1"}
            })),
        )
        .unwrap();
        // Already concluded — item end emits no further `reasoning-end` for
        // this index.
        assert!(
            !item_done
                .iter()
                .any(|e| matches!(e, LLMEvent::ReasoningEnd { .. }))
        );
    }

    #[test]
    fn function_call_lifecycle() {
        let req = LLMRequest::new(test_model(), vec![]);
        let mut st = initial(&req);
        let mut out = Vec::new();
        out.extend(
            step(
                &mut st,
                ev(json!({
                    "type":"response.output_item.added",
                    "item":{"type":"function_call","id":"i1","call_id":"c1","name":"read"}
                })),
            )
            .unwrap(),
        );
        out.extend(
            step(
                &mut st,
                ev(json!({
                    "type":"response.function_call_arguments.delta",
                    "item_id":"i1","delta":"{\"p\":1}"
                })),
            )
            .unwrap(),
        );
        out.extend(
            step(
                &mut st,
                ev(json!({
                    "type":"response.output_item.done",
                    "item":{"type":"function_call","id":"i1","call_id":"c1","name":"read","arguments":"{\"p\":1}"}
                })),
            )
            .unwrap(),
        );
        assert!(
            out.iter()
                .any(|e| matches!(e, LLMEvent::ToolInputStart { .. }))
        );
        assert!(
            out.iter()
                .any(|e| matches!(e, LLMEvent::ToolInputDelta { .. }))
        );
        assert!(
            out.iter()
                .any(|e| matches!(e, LLMEvent::ToolCall { name, .. } if name == "read"))
        );
        assert!(st.has_function_call);
    }

    #[test]
    fn completed_maps_finish_and_usage() {
        let req = LLMRequest::new(test_model(), vec![]);
        let mut st = initial(&req);
        let out = step(
            &mut st,
            ev(json!({
                "type":"response.completed",
                "response":{
                    "usage":{
                        "input_tokens":10,
                        "output_tokens":5,
                        "input_tokens_details":{"cached_tokens":4}
                    }
                }
            })),
        )
        .unwrap();
        match out.iter().find(|e| matches!(e, LLMEvent::Finish { .. })) {
            Some(LLMEvent::Finish {
                reason,
                usage: Some(u),
                ..
            }) => {
                assert_eq!(*reason, FinishReason::Stop);
                assert_eq!(u.input_tokens, Some(10));
                assert_eq!(u.cache_read_input_tokens, Some(4));
            }
            _ => panic!("expected finish with usage"),
        }
    }

    #[test]
    fn incomplete_length() {
        let req = LLMRequest::new(test_model(), vec![]);
        let mut st = initial(&req);
        let out = step(
            &mut st,
            ev(json!({
                "type":"response.incomplete",
                "response":{"incomplete_details":{"reason":"max_output_tokens"}}
            })),
        )
        .unwrap();
        match out.iter().find(|e| matches!(e, LLMEvent::Finish { .. })) {
            Some(LLMEvent::Finish { reason, .. }) => assert_eq!(*reason, FinishReason::Length),
            _ => panic!("expected finish"),
        }
    }

    #[test]
    fn failed_and_error_provider_error() {
        let req = LLMRequest::new(test_model(), vec![]);
        let mut st = initial(&req);
        let out = step(
            &mut st,
            ev(json!({
                "type":"response.failed",
                "response":{"error":{"code":"context_length_exceeded","message":"too long"}}
            })),
        )
        .unwrap();
        match out.first() {
            Some(LLMEvent::ProviderError { classification, .. }) => {
                assert_eq!(
                    *classification,
                    Some(ProviderFailureClassification::ContextOverflow)
                );
            }
            _ => panic!("expected provider-error"),
        }

        let out = step(
            &mut st,
            ev(json!({
                "type":"error","code":"rate_limit_exceeded","message":"slow"
            })),
        )
        .unwrap();
        match out.first() {
            Some(LLMEvent::ProviderError { message, .. }) => {
                assert!(message.contains("rate_limit_exceeded"));
                assert!(message.contains("slow"));
            }
            _ => panic!("expected provider-error"),
        }
    }

    #[test]
    fn terminal_predicate() {
        assert!(is_terminal(&ev(json!({"type":"response.completed"}))));
        assert!(is_terminal(&ev(json!({"type":"response.incomplete"}))));
        assert!(is_terminal(&ev(json!({"type":"response.failed"}))));
        assert!(!is_terminal(&ev(
            json!({"type":"response.output_text.delta"})
        )));
    }

    #[test]
    fn unknown_item_is_noop() {
        let req = LLMRequest::new(test_model(), vec![]);
        let mut st = initial(&req);
        let out = step(
            &mut st,
            ev(json!({
                "type":"response.output_item.done",
                "item":{"type":"web_search_call","id":"w1"}
            })),
        )
        .unwrap();
        assert!(out.is_empty());
    }

    // =========================================================================
    // Protocol impl + should_use_responses tests (Task 3)
    // =========================================================================

    #[test]
    fn protocol_id_and_decode() {
        let p = OpenAIResponses;
        assert_eq!(p.id(), "openai-responses");
        let e = p
            .decode_event(r#"{"type":"response.output_text.delta","delta":"x"}"#)
            .unwrap();
        let req = LLMRequest::new(test_model(), vec![]);
        let mut st = p.initial(&req);
        let out = p.step(&mut st, e).unwrap();
        assert!(out.iter().any(|e| matches!(e, LLMEvent::TextDelta { .. })));
        assert!(p.terminal(&p.decode_event(r#"{"type":"response.completed"}"#).unwrap()));
    }

    #[test]
    fn should_use_responses_truth_table() {
        assert!(should_use_responses("gpt-5"));
        assert!(should_use_responses("gpt-5.1"));
        assert!(should_use_responses("gpt-6"));
        assert!(!should_use_responses("gpt-5-mini"));
        assert!(!should_use_responses("gpt-4o"));
        assert!(!should_use_responses("claude-sonnet-4.5"));
        assert!(!should_use_responses("o3"));
    }
}
