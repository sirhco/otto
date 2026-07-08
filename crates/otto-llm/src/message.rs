//! Input message types.
//!
//! Port of opencode `packages/llm/src/schema/messages.ts`. These are the
//! *input* shapes callers hand to the client; the streaming *output* is the
//! provider-neutral [`otto_events::LLMEvent`] union.

use otto_events::{Json, ToolResultValue};
use serde::{Deserialize, Serialize};

use crate::request::CacheHint;

/// The role a [`Message`] plays in a conversation.
///
/// Port of the `role` literal union in `messages.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A system instruction message.
    System,
    /// A user (human) message.
    User,
    /// An assistant (model) message.
    Assistant,
    /// A tool message carrying tool results.
    Tool,
}

/// A single piece of message content.
///
/// Port of the `ContentPart` union in `messages.ts`, tagged by `type` with
/// kebab-case discriminators (`text`, `media`, `tool-call`, `tool-result`,
/// `reasoning`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ContentPart {
    /// Plain text.
    Text {
        /// The text content.
        text: String,
        /// Optional caching hint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache: Option<CacheHint>,
    },

    /// Binary media (image, audio, document, …) as base64 data.
    Media {
        /// MIME type, e.g. `image/png`.
        #[serde(rename = "mediaType")]
        media_type: String,
        /// Base64-encoded payload.
        data: String,
        /// Optional original filename.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },

    /// A tool call requested by the assistant.
    ToolCall {
        /// Tool-call id.
        id: String,
        /// Tool name.
        name: String,
        /// Parsed tool input.
        input: Json,
        /// Whether the provider executed the tool itself.
        #[serde(
            rename = "providerExecuted",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_executed: Option<bool>,
    },

    /// The result of a tool call.
    ToolResult {
        /// Tool-call id.
        id: String,
        /// Tool name.
        name: String,
        /// The tool result value (reused from `otto_events`).
        result: ToolResultValue,
        /// Whether the provider executed the tool itself.
        #[serde(
            rename = "providerExecuted",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        provider_executed: Option<bool>,
        /// Optional caching hint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache: Option<CacheHint>,
    },

    /// Model reasoning ("thinking") content.
    Reasoning {
        /// The reasoning text.
        text: String,
        /// Provider-encrypted reasoning blob, if the text is redacted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
    },
}

impl ContentPart {
    /// Build a plain [`ContentPart::Text`].
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        ContentPart::Text {
            text: text.into(),
            cache: None,
        }
    }

    /// Build a [`ContentPart::Reasoning`].
    #[must_use]
    pub fn reasoning(text: impl Into<String>) -> Self {
        ContentPart::Reasoning {
            text: text.into(),
            encrypted: None,
        }
    }
}

/// A conversation message.
///
/// Port of the `Message` shape in `messages.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Optional message id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The role of the sender.
    pub role: Role,
    /// The ordered content parts.
    pub content: Vec<ContentPart>,
    /// Provider-native escape hatch preserved across round trips.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<Json>,
}

impl Message {
    fn with_role(role: Role, content: Vec<ContentPart>) -> Self {
        Message {
            id: None,
            role,
            content,
            native: None,
        }
    }

    /// Construct a [`Role::User`] message.
    #[must_use]
    pub fn user(content: Vec<ContentPart>) -> Self {
        Self::with_role(Role::User, content)
    }

    /// Construct a [`Role::Assistant`] message.
    #[must_use]
    pub fn assistant(content: Vec<ContentPart>) -> Self {
        Self::with_role(Role::Assistant, content)
    }

    /// Construct a [`Role::System`] message.
    #[must_use]
    pub fn system(content: Vec<ContentPart>) -> Self {
        Self::with_role(Role::System, content)
    }

    /// Construct a [`Role::Tool`] message.
    #[must_use]
    pub fn tool(content: Vec<ContentPart>) -> Self {
        Self::with_role(Role::Tool, content)
    }
}

/// A tool the model may call.
///
/// Port of the `ToolDefinition` shape in `messages.ts`. `input_schema` /
/// `output_schema` are JSON Schema documents (`JsonSchema = serde_json::Value`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Unique tool name.
    pub name: String,
    /// Human-readable description shown to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the tool input.
    #[serde(rename = "inputSchema")]
    pub input_schema: Json,
    /// Optional JSON Schema describing the tool output.
    #[serde(
        rename = "outputSchema",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub output_schema: Option<Json>,
    /// Optional caching hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
}

/// How the model should choose among available tools.
///
/// Port of the `toolChoice` union in `messages.ts`. Serialized in AI-SDK
/// object form: `{ "type": "auto" }`, `{ "type": "tool", "name": "…" }`, etc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolChoice {
    /// The model decides whether to call a tool.
    Auto,
    /// The model must not call a tool.
    None,
    /// The model must call some tool.
    Required,
    /// The model must call the named tool.
    Tool {
        /// Name of the required tool.
        name: String,
    },
}

/// A system-prompt segment.
///
/// Port of the `SystemPart` shape in `messages.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemPart {
    /// The system text.
    pub text: String,
    /// Optional caching hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
}

impl SystemPart {
    /// Build a [`SystemPart`] from text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        SystemPart {
            text: text.into(),
            cache: None,
        }
    }
}
