//! Message converter — a faithful Rust port of opencode's
//! `toModelMessagesEffect` (`session/message-v2.ts:131-415`).
//!
//! [`to_model_messages`] does in one pass what opencode splits across the
//! `UIMessage` build (`message-v2.ts:198-400`) and the AI-SDK
//! `convertToModelMessages` lowering: it takes the persisted
//! [`WithParts`] history and produces the final `Vec<`[`otto_llm::Message`]`>`
//! (roles user / assistant / tool) that a provider request is built from.

use otto_events::ToolResultValue;
use otto_llm::message::{ContentPart, Message};
use otto_llm::model::{ModelId, ProviderId};
use otto_storage::model::{InfoBody, Part, PartKind, ToolState, WithParts};
use serde_json::{Value, json};

/// Options controlling the conversion (`message-v2.ts:134`).
#[derive(Debug, Clone, Default)]
pub struct ConvertOptions {
    /// When set, media file parts are replaced with a textual placeholder and
    /// tool-result attachments are dropped (`message-v2.ts:213`, `296`).
    pub strip_media: bool,
    /// When set, completed tool output longer than this is truncated
    /// (`message-v2.ts:49-53`, `295`).
    pub tool_output_max_chars: Option<usize>,
}

/// Synthetic user prompt injected ahead of extracted tool-result media
/// (`message-v2.ts:46`).
const SYNTHETIC_ATTACHMENT_PROMPT: &str = "Attached media from tool result:";
/// Text substituted for a user [`PartKind::Compaction`] part
/// (`message-v2.ts:231`).
const COMPACTION_TEXT: &str = "What did we do so far?";
/// Text substituted for a user [`PartKind::Subtask`] part (`message-v2.ts:237`).
const SUBTASK_TEXT: &str = "The following tool was executed by the user";
/// Error text emitted for a dangling (pending/running) tool call
/// (`message-v2.ts:357`).
const INTERRUPTED_TEXT: &str = "[Tool execution was interrupted]";
/// Output substituted for a compacted completed tool (`message-v2.ts:293`).
const OLD_TOOL_CLEARED: &str = "[Old tool result content cleared]";

/// Port of `isMedia` (`util/media.ts:7-9`): images and PDFs.
fn is_media(mime: &str) -> bool {
    mime.starts_with("image/") || mime == "application/pdf"
}

/// Port of `truncateToolOutput` (`message-v2.ts:49-53`). Slices on `char`
/// boundaries (JS uses UTF-16 units; chars are the safe Rust analog).
fn truncate_tool_output(text: &str, max_chars: Option<usize>) -> String {
    let Some(max) = max_chars else {
        return text.to_string();
    };
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let omitted = count - max;
    let head: String = text.chars().take(max).collect();
    format!("{head}\n[Tool output truncated for compaction: omitted {omitted} chars]")
}

/// Extract the base64 payload of a `data:` URL (the part after the first comma),
/// matching opencode's `toModelOutput` (`message-v2.ts:183-186`). Non-`data:`
/// URLs are returned unchanged.
fn data_url_payload(url: &str) -> &str {
    match url.find(',') {
        Some(idx) => &url[idx + 1..],
        None => url,
    }
}

/// Port of `supportsMediaInToolResult` (`message-v2.ts:147-159`).
///
/// opencode keys on the AI-SDK npm package of the model's api; otto has no
/// `api.npm`, so this keys on the provider id string (and, for Google, the model
/// id). `mime` decides the image-only providers.
fn supports_media_in_tool_result(provider: &ProviderId, model: &ModelId, mime: &str) -> bool {
    let p = provider.0.to_lowercase();
    if p.contains("anthropic") {
        return true;
    }
    if p == "openai" || p.contains("openai") {
        return true;
    }
    if p.contains("bedrock") {
        return mime.starts_with("image/");
    }
    if p == "xai" || p.contains("xai") {
        return mime.starts_with("image/");
    }
    if p.contains("google") || p.contains("vertex") {
        let id = model.0.to_lowercase();
        return id.contains("gemini-3") && !id.contains("gemini-2");
    }
    false
}

/// Whether an assistant `error` should cause the whole message to be skipped
/// (`message-v2.ts:248-256`): skip if an error is present, UNLESS it is an
/// abort AND the message has a part that is not step-start/reasoning.
fn should_skip_errored(info: &otto_storage::model::Assistant, parts: &[Part]) -> bool {
    let Some(error) = &info.error else {
        return false;
    };
    let is_aborted = matches!(
        error,
        otto_storage::model::AssistantError::MessageAbortedError { .. }
    );
    let has_real_part = parts.iter().any(|p| {
        !matches!(
            p.kind,
            PartKind::StepStart { .. } | PartKind::Reasoning { .. }
        )
    });
    !(is_aborted && has_real_part)
}

/// Read `metadata.anthropic.signature` as a string, if present.
fn anthropic_signature(metadata: &Option<Value>) -> Option<String> {
    metadata
        .as_ref()?
        .get("anthropic")?
        .get("signature")?
        .as_str()
        .map(str::to_string)
}

/// Read `metadata.providerExecuted == true`.
fn provider_executed(metadata: &Option<Value>) -> Option<bool> {
    if metadata
        .as_ref()
        .and_then(|m| m.get("providerExecuted"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        Some(true)
    } else {
        None
    }
}

/// Build a tool-content JSON block for a media attachment, matching
/// `toModelOutput` (`message-v2.ts:180-187`).
fn media_content_block(mime: &str, url: &str) -> Value {
    json!({
        "type": "media",
        "mediaType": mime,
        "data": data_url_payload(url),
    })
}

/// Convert persisted history to the final provider-neutral messages.
///
/// Port of `toModelMessagesEffect` (`message-v2.ts:131-415`), fused with the
/// AI-SDK `convertToModelMessages` lowering so the result is the terminal
/// user/assistant/tool `Vec<`[`Message`]`>`. See the module docs for the
/// per-role rules and quirks.
#[must_use]
pub fn to_model_messages(
    msgs: &[WithParts],
    provider: &ProviderId,
    model: &ModelId,
    opts: &ConvertOptions,
) -> Vec<Message> {
    let mut result: Vec<Message> = Vec::new();

    for msg in msgs {
        if msg.parts.is_empty() {
            continue;
        }
        match &msg.info.body {
            InfoBody::User(_) => convert_user(msg, opts, &mut result),
            InfoBody::Assistant(info) => {
                convert_assistant(msg, info, provider, model, opts, &mut result);
            }
        }
    }

    result
}

/// User-message lowering (`message-v2.ts:198-242`).
fn convert_user(msg: &WithParts, opts: &ConvertOptions, result: &mut Vec<Message>) {
    let mut content: Vec<ContentPart> = Vec::new();
    for part in &msg.parts {
        match &part.kind {
            PartKind::Text { text, ignored, .. } if ignored != &Some(true) && !text.is_empty() => {
                content.push(ContentPart::text(text.clone()));
            }
            PartKind::File {
                mime,
                filename,
                url,
                ..
            } => {
                // text/plain and directory files are converted to text
                // elsewhere; ignore them here (`message-v2.ts:212`).
                if mime == "text/plain" || mime == "application/x-directory" {
                    continue;
                }
                if opts.strip_media && is_media(mime) {
                    let name = filename.as_deref().unwrap_or("file");
                    content.push(ContentPart::text(format!("[Attached {mime}: {name}]")));
                } else {
                    content.push(ContentPart::Media {
                        media_type: mime.clone(),
                        data: data_url_payload(url).to_string(),
                        filename: filename.clone(),
                    });
                }
            }
            PartKind::Compaction { .. } => content.push(ContentPart::text(COMPACTION_TEXT)),
            PartKind::Subtask { .. } => content.push(ContentPart::text(SUBTASK_TEXT)),
            _ => {}
        }
    }
    if !content.is_empty() {
        result.push(Message::user(content));
    }
}

/// Assistant-message lowering (`message-v2.ts:244-400`), including the tool
/// call/result split and tool-result media extraction.
fn convert_assistant(
    msg: &WithParts,
    info: &otto_storage::model::Assistant,
    provider: &ProviderId,
    model: &ModelId,
    opts: &ConvertOptions,
    result: &mut Vec<Message>,
) {
    if should_skip_errored(info, &msg.parts) {
        return;
    }

    // differentModel: `${provider}/${model}` != stored `${providerID}/${modelID}`
    // (`message-v2.ts:245`).
    let different_model = provider.0 != info.provider_id || model.0 != info.model_id;

    // Empty text separators are preserved as a single space when signed
    // reasoning is present (`message-v2.ts:273-279`).
    let has_signed_reasoning = msg.parts.iter().any(|p| match &p.kind {
        PartKind::Reasoning { metadata, .. } => anthropic_signature(metadata).is_some(),
        _ => false,
    });

    let mut content: Vec<ContentPart> = Vec::new();
    let mut tool_results: Vec<ContentPart> = Vec::new();
    // Media extracted from tool results, injected as a follow-up user message
    // (`message-v2.ts:246`, `302-304`, `382-399`).
    let mut extracted_media: Vec<(String, String, Option<String>)> = Vec::new();

    for part in &msg.parts {
        match &part.kind {
            PartKind::Text { text, metadata, .. } => {
                let _ = metadata; // otto text has no provider-metadata field
                let out = if text.is_empty() && has_signed_reasoning {
                    " ".to_string()
                } else {
                    text.clone()
                };
                content.push(ContentPart::text(out));
            }
            // step-start is a boundary only; otto has no step-start content
            // part, so it is dropped (`message-v2.ts:286-289`).
            PartKind::StepStart { .. } => {}
            PartKind::Reasoning { text, metadata, .. } => {
                if different_model {
                    // Downgrade reasoning to plain text; drop if empty
                    // (`message-v2.ts:362-370`).
                    if !text.trim().is_empty() {
                        content.push(ContentPart::text(text.clone()));
                    }
                } else {
                    content.push(ContentPart::Reasoning {
                        text: text.clone(),
                        encrypted: anthropic_signature(metadata),
                    });
                }
            }
            PartKind::Tool {
                call_id,
                tool,
                metadata,
                state,
            } => {
                convert_tool(
                    call_id,
                    tool,
                    metadata,
                    state,
                    different_model,
                    provider,
                    model,
                    opts,
                    &mut content,
                    &mut tool_results,
                    &mut extracted_media,
                );
            }
            _ => {}
        }
    }

    if content.is_empty() {
        return;
    }

    result.push(Message::assistant(content));
    if !tool_results.is_empty() {
        result.push(Message::tool(tool_results));
    }
    if !extracted_media.is_empty() {
        let mut media_content = vec![ContentPart::text(SYNTHETIC_ATTACHMENT_PROMPT)];
        for (mime, url, filename) in extracted_media {
            media_content.push(ContentPart::Media {
                media_type: mime,
                data: data_url_payload(&url).to_string(),
                filename,
            });
        }
        result.push(Message::user(media_content));
    }
}

/// Lower a single `tool` part into an assistant tool-call plus a tool-role
/// tool-result (`message-v2.ts:290-361`). Every branch emits a matching
/// tool-call so no tool_use is left dangling.
#[allow(clippy::too_many_arguments)]
fn convert_tool(
    call_id: &str,
    tool: &str,
    part_metadata: &Option<Value>,
    state: &ToolState,
    different_model: bool,
    provider: &ProviderId,
    model: &ModelId,
    opts: &ConvertOptions,
    content: &mut Vec<ContentPart>,
    tool_results: &mut Vec<ContentPart>,
    extracted_media: &mut Vec<(String, String, Option<String>)>,
) {
    let _ = different_model; // otto tool-call has no callProviderMetadata field
    let executed = provider_executed(part_metadata);

    let (input, result_value): (&Value, ToolResultValue) = match state {
        ToolState::Completed {
            input,
            output,
            time,
            attachments,
            ..
        } => {
            let output_text = if time.compacted.is_some() {
                OLD_TOOL_CLEARED.to_string()
            } else {
                truncate_tool_output(output, opts.tool_output_max_chars)
            };

            // Attachments are dropped on compaction / strip_media
            // (`message-v2.ts:296`).
            let attachments: &[Part] = if time.compacted.is_some() || opts.strip_media {
                &[]
            } else {
                attachments.as_deref().unwrap_or(&[])
            };

            // Split media by provider support: unsupported media is extracted
            // to a follow-up user message (`message-v2.ts:300-305`).
            let mut final_attachments: Vec<(&str, &str)> = Vec::new();
            for att in attachments {
                let PartKind::File {
                    mime,
                    url,
                    filename,
                    ..
                } = &att.kind
                else {
                    continue;
                };
                if is_media(mime) && !supports_media_in_tool_result(provider, model, mime) {
                    extracted_media.push((mime.clone(), url.clone(), filename.clone()));
                } else {
                    final_attachments.push((mime, url));
                }
            }

            let value = if final_attachments.is_empty() {
                ToolResultValue::Text {
                    value: Value::String(output_text),
                }
            } else {
                // `toModelOutput` content branch (`message-v2.ts:176-190`):
                // optional text block then a media block per data: attachment.
                let mut blocks: Vec<Value> = Vec::new();
                if !output_text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": output_text }));
                }
                for (mime, url) in &final_attachments {
                    if url.starts_with("data:") && url.contains(',') {
                        blocks.push(media_content_block(mime, url));
                    }
                }
                ToolResultValue::Content { value: blocks }
            };
            (input, value)
        }
        ToolState::Error {
            input,
            error,
            metadata,
            ..
        } => {
            // interrupted errors with string output become a normal text result
            // (`message-v2.ts:326-347`).
            let interrupted_output = metadata
                .as_ref()
                .filter(|m| m.get("interrupted").and_then(Value::as_bool) == Some(true))
                .and_then(|m| m.get("output"))
                .and_then(Value::as_str);
            let value = match interrupted_output {
                Some(text) => ToolResultValue::Text {
                    value: Value::String(text.to_string()),
                },
                None => ToolResultValue::Error {
                    value: Value::String(error.clone()),
                },
            };
            (input, value)
        }
        // Dangling pending/running tool → synthetic interrupted error
        // (`message-v2.ts:349-360`).
        ToolState::Pending { input, .. } | ToolState::Running { input, .. } => (
            input,
            ToolResultValue::Error {
                value: Value::String(INTERRUPTED_TEXT.to_string()),
            },
        ),
    };

    content.push(ContentPart::ToolCall {
        id: call_id.to_string(),
        name: tool.to_string(),
        input: input.clone(),
        provider_executed: executed,
    });
    tool_results.push(ContentPart::ToolResult {
        id: call_id.to_string(),
        name: tool.to_string(),
        result: result_value,
        provider_executed: executed,
        cache: None,
    });
}
