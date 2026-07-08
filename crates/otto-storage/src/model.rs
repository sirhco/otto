//! Session message / part data model — a faithful Rust port of opencode's
//! `packages/schema/src/v1/session.ts`.
//!
//! The union types mirror the effect-schema unions defined there:
//!
//! * [`Part`] — the message-part union, discriminated by a `type` field
//!   (`session.ts:357-383`). The three base fields `id` / `sessionID` /
//!   `messageID` (`partBase`, `session.ts:81-85`) live on the [`Part`] wrapper;
//!   the variant payload lives in [`PartKind`], which is what gets persisted as
//!   the SQLite `data` blob (`sql.ts:20` — `V1PartData = Omit<Part, "id" |
//!   "sessionID" | "messageID">`).
//! * [`ToolState`] — tool-execution state, discriminated by `status`
//!   (`session.ts:304-313`).
//! * [`Info`] — the message union `User | Assistant`, discriminated by `role`
//!   (`session.ts:490-491`). `id` / `sessionID` (`messageBase`,
//!   `session.ts:327-330`) live on the [`Info`] wrapper; the variant payload
//!   [`InfoBody`] is the persisted `data` blob (`sql.ts:19` — `V1MessageData =
//!   Omit<Info, "id" | "sessionID">`).
//! * [`AssistantError`] — the assistant-error union, discriminated by `name`
//!   with payload under `data` (`session.ts:385-395`, adjacently tagged).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Arbitrary JSON value — the Rust analog of effect-schema `Schema.Any` /
/// `Schema.Record(String, Any)` used throughout `session.ts` for `metadata`,
/// tool `input`, and `structured` payloads.
pub type Json = serde_json::Value;

/// Session identifier string (`ses_…`). Mirrors `SessionID` in `session.ts`.
pub type SessionId = String;
/// Message identifier string (`msg_…`). Mirrors `MessageID` (`session.ts:17`).
pub type MessageId = String;
/// Part identifier string (`prt_…`). Mirrors `PartID` (`session.ts:23`).
pub type PartId = String;

/// Generates a fresh ascending [`MessageId`] (`msg_…`) via `otto-id`.
///
/// Equivalent to `MessageID.ascending()` (`session.ts:19`).
#[must_use]
pub fn new_message_id() -> MessageId {
    otto_id::ascending(otto_id::Prefix::Message)
}

/// Generates a fresh ascending [`PartId`] (`prt_…`) via `otto-id`.
///
/// Equivalent to `PartID.ascending()` (`session.ts:25`).
#[must_use]
pub fn new_part_id() -> PartId {
    otto_id::ascending(otto_id::Prefix::Part)
}

// ---------------------------------------------------------------------------
// Shared value objects
// ---------------------------------------------------------------------------

/// A `{ start, end? }` time span (`session.ts:108-113`, `123-126`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartEndTime {
    /// Millisecond start timestamp.
    pub start: i64,
    /// Optional millisecond end timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
}

/// A `{ start }` time (`session.ts:271-273`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartTime {
    /// Millisecond start timestamp.
    pub start: i64,
}

/// A `{ start, end, compacted? }` time (`session.ts:283-287`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletedTime {
    /// Millisecond start timestamp.
    pub start: i64,
    /// Millisecond end timestamp.
    pub end: i64,
    /// Millisecond timestamp at which the output was compacted, if ever.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compacted: Option<i64>,
}

/// A `{ start, end }` time (`session.ts:297-300`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartEndReqTime {
    /// Millisecond start timestamp.
    pub start: i64,
    /// Millisecond end timestamp.
    pub end: i64,
}

/// Token accounting shared by assistant messages and step-finish parts
/// (`session.ts:246-255`, `472-481`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    /// Optional inclusive total.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// Prompt/input tokens.
    pub input: f64,
    /// Completion/output tokens.
    pub output: f64,
    /// Reasoning tokens.
    pub reasoning: f64,
    /// Prompt-cache read/write breakdown.
    pub cache: TokenCache,
}

/// Prompt-cache read/write token counts (`session.ts:251-254`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenCache {
    /// Cache-read tokens.
    pub read: f64,
    /// Cache-write tokens.
    pub write: f64,
}

/// `{ providerID, modelID }` reference (`session.ts:211-214`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRef {
    /// Provider identifier.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Model identifier.
    #[serde(rename = "modelID")]
    pub model_id: String,
}

// ---------------------------------------------------------------------------
// FilePartSource (session.ts:130-169)
// ---------------------------------------------------------------------------

/// `{ value, start, end }` text span shared by every file-part source
/// (`filePartSourceBase`, `session.ts:130-136`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilePartSourceText {
    /// Raw referenced text.
    pub value: String,
    /// Character start offset.
    pub start: f64,
    /// Character end offset.
    pub end: f64,
}

/// A line/character position (`session.ts:139`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Zero-based line.
    pub line: i64,
    /// Zero-based character within the line.
    pub character: i64,
}

/// A `{ start, end }` range of [`Position`]s (`session.ts:138-141`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Range {
    /// Range start.
    pub start: Position,
    /// Range end.
    pub end: Position,
}

/// File-part source union, discriminated by `type` (`session.ts:166-169`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FilePartSource {
    /// A whole-file reference (`session.ts:144-148`).
    File {
        /// Referenced text span.
        text: FilePartSourceText,
        /// File path.
        path: String,
    },
    /// A symbol reference (`session.ts:150-157`).
    Symbol {
        /// Referenced text span.
        text: FilePartSourceText,
        /// File path.
        path: String,
        /// Symbol source range.
        range: Range,
        /// Symbol name.
        name: String,
        /// LSP symbol kind.
        kind: i64,
    },
    /// An MCP resource reference (`session.ts:159-164`).
    Resource {
        /// Referenced text span.
        text: FilePartSourceText,
        /// Originating MCP client name.
        #[serde(rename = "clientName")]
        client_name: String,
        /// Resource URI.
        uri: String,
    },
}

// ---------------------------------------------------------------------------
// APIError (session.ts:48-56) — used by RetryPart and AssistantError
// ---------------------------------------------------------------------------

/// Payload of an `APIError` named error (`session.ts:48-55`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiErrorData {
    /// Human-readable message.
    pub message: String,
    /// Optional HTTP status code.
    #[serde(rename = "statusCode", skip_serializing_if = "Option::is_none")]
    pub status_code: Option<i64>,
    /// Whether the request may be retried.
    #[serde(rename = "isRetryable")]
    pub is_retryable: bool,
    /// Response headers, if captured.
    #[serde(rename = "responseHeaders", skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<HashMap<String, String>>,
    /// Response body, if captured.
    #[serde(rename = "responseBody", skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    /// Provider-specific metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

/// `APIError` named error wrapper `{ name: "APIError", data }` as used by
/// [`PartKind::Retry`] (`session.ts:48-56`, `224`). Adjacently tagged so it
/// serializes as `{ "name": "APIError", "data": { … } }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "data")]
pub enum ApiError {
    /// The single `APIError` variant.
    #[serde(rename = "APIError")]
    ApiError(ApiErrorData),
}

// ---------------------------------------------------------------------------
// ToolState (session.ts:259-313)
// ---------------------------------------------------------------------------

/// Tool-execution state union, discriminated by `status`
/// (`session.ts:304-313`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ToolState {
    /// Queued, not yet started (`session.ts:259-263`).
    Pending {
        /// Parsed tool input.
        input: Json,
        /// Raw (unparsed) tool input string.
        raw: String,
    },
    /// Currently executing (`session.ts:266-274`).
    Running {
        /// Parsed tool input.
        input: Json,
        /// Optional display title.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Optional metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Json>,
        /// Start time.
        time: StartTime,
    },
    /// Finished successfully (`session.ts:277-290`).
    Completed {
        /// Parsed tool input.
        input: Json,
        /// Tool output text.
        output: String,
        /// Display title.
        title: String,
        /// Metadata (required for completed state).
        metadata: Json,
        /// Start/end/compacted time.
        time: CompletedTime,
        /// Media/file attachments produced by the tool.
        #[serde(skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<Part>>,
    },
    /// Failed (`session.ts:292-302`).
    Error {
        /// Parsed tool input.
        input: Json,
        /// Error message.
        error: String,
        /// Optional metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Json>,
        /// Start/end time.
        time: StartEndReqTime,
    },
}

// ---------------------------------------------------------------------------
// Part (session.ts:81-383)
// ---------------------------------------------------------------------------

/// Source anchor for an [`PartKind::Agent`] mention (`session.ts:185-191`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSource {
    /// Matched text.
    pub value: String,
    /// Character start offset.
    pub start: i64,
    /// Character end offset.
    pub end: i64,
}

/// The variant payload of a [`Part`] — the union tail discriminated by `type`
/// (`session.ts:357-383`). This is exactly what is persisted as the SQLite
/// `part.data` blob (`sql.ts:20`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PartKind {
    /// User/assistant text (`session.ts:102-116`).
    Text {
        /// The text content.
        text: String,
        /// Whether the part was synthesized rather than authored.
        #[serde(skip_serializing_if = "Option::is_none")]
        synthetic: Option<bool>,
        /// Whether the part should be ignored when building model input.
        #[serde(skip_serializing_if = "Option::is_none")]
        ignored: Option<bool>,
        /// Optional streaming time span.
        #[serde(skip_serializing_if = "Option::is_none")]
        time: Option<StartEndTime>,
        /// Optional metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Json>,
    },
    /// Model reasoning trace (`session.ts:118-128`).
    Reasoning {
        /// The reasoning text.
        text: String,
        /// Streaming time span (required).
        time: StartEndTime,
        /// Optional metadata (e.g. provider signature).
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Json>,
    },
    /// File attachment (`session.ts:171-178`).
    File {
        /// MIME type.
        mime: String,
        /// Optional filename.
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        /// Data/URL of the file.
        url: String,
        /// Optional source anchor.
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<FilePartSource>,
    },
    /// Tool invocation + state (`session.ts:315-322`).
    Tool {
        /// Provider tool-call id.
        #[serde(rename = "callID")]
        call_id: String,
        /// Tool name.
        tool: String,
        /// Optional metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Json>,
        /// Execution state.
        state: ToolState,
    },
    /// Beginning of an assistant step (`session.ts:233-237`).
    StepStart {
        /// Optional git snapshot ref.
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<String>,
    },
    /// End of an assistant step, with cost/token accounting
    /// (`session.ts:240-256`).
    StepFinish {
        /// Finish reason.
        reason: String,
        /// Optional git snapshot ref.
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<String>,
        /// Step cost.
        cost: f64,
        /// Step token accounting.
        tokens: Tokens,
    },
    /// Git snapshot marker (`session.ts:87-91`).
    Snapshot {
        /// Snapshot ref.
        snapshot: String,
    },
    /// Applied patch marker (`session.ts:94-99`).
    Patch {
        /// Patch hash.
        hash: String,
        /// Affected file paths.
        files: Vec<String>,
    },
    /// Agent mention (`session.ts:181-192`).
    Agent {
        /// Agent name.
        name: String,
        /// Optional source anchor.
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<AgentSource>,
    },
    /// Subtask spawn (`session.ts:204-217`).
    Subtask {
        /// Subtask prompt.
        prompt: String,
        /// Subtask description.
        description: String,
        /// Agent to run the subtask.
        agent: String,
        /// Optional model override.
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        /// Optional originating command.
        #[serde(skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
    /// Context-compaction marker (`session.ts:195-201`).
    Compaction {
        /// Whether compaction was triggered automatically.
        auto: bool,
        /// Whether compaction was due to context overflow.
        #[serde(skip_serializing_if = "Option::is_none")]
        overflow: Option<bool>,
        /// Message id at which the retained tail begins.
        #[serde(skip_serializing_if = "Option::is_none")]
        tail_start_id: Option<MessageId>,
    },
    /// Retry marker (`session.ts:220-228`).
    Retry {
        /// Retry attempt number.
        attempt: i64,
        /// The error that triggered the retry.
        error: ApiError,
        /// Creation time.
        time: RetryTime,
    },
}

/// A `{ created }` time (`session.ts:225-227`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetryTime {
    /// Millisecond creation timestamp.
    pub created: i64,
}

/// A message part: the `partBase` columns (`session.ts:81-85`) plus the
/// variant [`PartKind`]. Serializing a [`Part`] reproduces opencode's flat part
/// JSON (`{ id, sessionID, messageID, type, … }`); serializing only its
/// [`kind`](Part::kind) reproduces the persisted `data` blob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Part {
    /// Part id (`prt_…`).
    pub id: PartId,
    /// Owning session id.
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    /// Owning message id.
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    /// Variant payload.
    #[serde(flatten)]
    pub kind: PartKind,
}

impl Part {
    /// Serializes the persisted `data` blob — the variant payload only, with
    /// `id`/`sessionID`/`messageID` excluded (`sql.ts:20`).
    ///
    /// # Errors
    /// Returns any [`serde_json`] serialization error.
    pub fn data_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.kind)
    }

    /// Reconstructs a [`Part`] from a stored row, merging the column
    /// `id`/`session_id`/`message_id` back onto the `data` blob — the Rust
    /// analog of `part()` (`message-v2.ts:87-93`).
    ///
    /// # Errors
    /// Returns any [`serde_json`] deserialization error.
    pub fn from_row(
        id: PartId,
        session_id: SessionId,
        message_id: MessageId,
        data: &str,
    ) -> Result<Self, serde_json::Error> {
        let kind: PartKind = serde_json::from_str(data)?;
        Ok(Self {
            id,
            session_id,
            message_id,
            kind,
        })
    }

    /// Returns the compaction tail-start id if this is a compaction part.
    #[must_use]
    pub fn compaction_tail_start_id(&self) -> Option<&MessageId> {
        match &self.kind {
            PartKind::Compaction { tail_start_id, .. } => tail_start_id.as_ref(),
            _ => None,
        }
    }

    /// Whether this part is a [`PartKind::Compaction`].
    #[must_use]
    pub fn is_compaction(&self) -> bool {
        matches!(self.kind, PartKind::Compaction { .. })
    }

    /// Whether this part is a [`PartKind::Subtask`].
    #[must_use]
    pub fn is_subtask(&self) -> bool {
        matches!(self.kind, PartKind::Subtask { .. })
    }
}

// ---------------------------------------------------------------------------
// AssistantError (session.ts:36-63, 385-395)
// ---------------------------------------------------------------------------

/// Assistant-error union, discriminated by `name` with payload under `data`
/// (`session.ts:385-395`). Adjacently tagged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "data")]
pub enum AssistantError {
    /// Provider authentication failed (`session.ts:38-41`).
    ProviderAuthError {
        /// Provider identifier.
        #[serde(rename = "providerID")]
        provider_id: String,
        /// Error message.
        message: String,
    },
    /// Catch-all unknown error (`session.ts:387`).
    UnknownError {
        /// Error message.
        message: String,
        /// Optional error reference id.
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        r#ref: Option<String>,
    },
    /// Model output-length limit hit (`session.ts:36`).
    MessageOutputLengthError {},
    /// Generation aborted (`session.ts:43`).
    MessageAbortedError {
        /// Error message.
        message: String,
    },
    /// Structured-output decoding failed (`session.ts:44-47`).
    StructuredOutputError {
        /// Error message.
        message: String,
        /// Retry count.
        retries: i64,
    },
    /// Context window overflowed (`session.ts:57-60`).
    ContextOverflowError {
        /// Error message.
        message: String,
        /// Optional response body.
        #[serde(rename = "responseBody", skip_serializing_if = "Option::is_none")]
        response_body: Option<String>,
    },
    /// Provider content filter tripped (`session.ts:61-63`).
    ContentFilterError {
        /// Error message.
        message: String,
    },
    /// Generic provider API error (`session.ts:48-55`).
    #[serde(rename = "APIError")]
    ApiError(ApiErrorData),
}

// ---------------------------------------------------------------------------
// Info: User | Assistant (session.ts:332-491)
// ---------------------------------------------------------------------------

/// Structured-output format union, discriminated by `type`
/// (`session.ts:65-79`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OutputFormat {
    /// Plain text output (`session.ts:65-67`).
    #[serde(rename = "text")]
    Text,
    /// JSON-schema constrained output (`session.ts:69-73`).
    #[serde(rename = "json_schema")]
    JsonSchema {
        /// The JSON schema.
        schema: Json,
        /// Retry count (default 2 in opencode).
        #[serde(rename = "retryCount", skip_serializing_if = "Option::is_none")]
        retry_count: Option<i64>,
    },
}

/// A `{ created }` user-message time (`session.ts:335-337`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserTime {
    /// Millisecond creation timestamp.
    pub created: i64,
}

/// User-message model reference (`session.ts:347-351`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserModel {
    /// Provider identifier.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Model identifier.
    #[serde(rename = "modelID")]
    pub model_id: String,
    /// Optional model variant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// User-message summary (`session.ts:339-345`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserSummary {
    /// Optional summary title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional summary body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// File diffs captured with the summary (opaque `FileDiff.Info`).
    pub diffs: Vec<Json>,
}

/// User message payload — `User` minus `id`/`sessionID`/`role`
/// (`session.ts:332-354`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    /// Creation time.
    pub time: UserTime,
    /// Optional structured-output format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
    /// Optional summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<UserSummary>,
    /// Agent name.
    pub agent: String,
    /// Model reference.
    pub model: UserModel,
    /// Optional system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Optional per-message tool enablement map.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<HashMap<String, bool>>,
}

/// A `{ created, completed? }` assistant-message time (`session.ts:456-459`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantTime {
    /// Millisecond creation timestamp.
    pub created: i64,
    /// Optional millisecond completion timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

/// Assistant working-directory paths (`session.ts:466-469`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantPath {
    /// Current working directory.
    pub cwd: String,
    /// Project root.
    pub root: String,
}

/// Assistant message payload — `Assistant` minus `id`/`sessionID`/`role`
/// (`session.ts:453-488`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assistant {
    /// Creation/completion time.
    pub time: AssistantTime,
    /// Optional terminal error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AssistantError>,
    /// Parent (triggering user) message id.
    #[serde(rename = "parentID")]
    pub parent_id: MessageId,
    /// Model identifier.
    #[serde(rename = "modelID")]
    pub model_id: String,
    /// Provider identifier.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Agent mode.
    pub mode: String,
    /// Agent name.
    pub agent: String,
    /// Working-directory paths.
    pub path: AssistantPath,
    /// Whether this message is a compaction summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<bool>,
    /// Accumulated cost.
    pub cost: f64,
    /// Accumulated token accounting.
    pub tokens: Tokens,
    /// Optional structured output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured: Option<Json>,
    /// Optional model variant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Optional finish reason (set once the turn completes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish: Option<String>,
}

/// The variant payload of an [`Info`] — `User | Assistant` discriminated by
/// `role` (`session.ts:490-491`). This is exactly the persisted SQLite
/// `message.data` blob (`sql.ts:19`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
// A faithful message data model: both arms are large structs and boxing either
// would only complicate construction/serialization with no real memory win.
#[allow(clippy::large_enum_variant)]
pub enum InfoBody {
    /// A user message.
    User(User),
    /// An assistant message.
    Assistant(Assistant),
}

/// A message: the `messageBase` columns (`session.ts:327-330`) plus the variant
/// [`InfoBody`]. Serializing an [`Info`] reproduces opencode's flat message JSON
/// (`{ id, sessionID, role, … }`); serializing only its [`body`](Info::body)
/// reproduces the persisted `data` blob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Info {
    /// Message id (`msg_…`).
    pub id: MessageId,
    /// Owning session id.
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    /// Variant payload.
    #[serde(flatten)]
    pub body: InfoBody,
}

impl Info {
    /// Serializes the persisted `data` blob — the variant payload only, with
    /// `id`/`sessionID` excluded (`sql.ts:19`).
    ///
    /// # Errors
    /// Returns any [`serde_json`] serialization error.
    pub fn data_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.body)
    }

    /// Reconstructs an [`Info`] from a stored row, merging the column
    /// `id`/`session_id` back onto the `data` blob — the Rust analog of
    /// `info()` (`message-v2.ts:80-85`).
    ///
    /// # Errors
    /// Returns any [`serde_json`] deserialization error.
    pub fn from_row(
        id: MessageId,
        session_id: SessionId,
        data: &str,
    ) -> Result<Self, serde_json::Error> {
        let body: InfoBody = serde_json::from_str(data)?;
        Ok(Self {
            id,
            session_id,
            body,
        })
    }

    /// The message id.
    #[must_use]
    pub fn id(&self) -> &MessageId {
        &self.id
    }

    /// Whether this is a user message.
    #[must_use]
    pub fn is_user(&self) -> bool {
        matches!(self.body, InfoBody::User(_))
    }

    /// Whether this is an assistant message.
    #[must_use]
    pub fn is_assistant(&self) -> bool {
        matches!(self.body, InfoBody::Assistant(_))
    }

    /// Borrows the user payload if this is a user message.
    #[must_use]
    pub fn as_user(&self) -> Option<&User> {
        match &self.body {
            InfoBody::User(u) => Some(u),
            InfoBody::Assistant(_) => None,
        }
    }

    /// Borrows the assistant payload if this is an assistant message.
    #[must_use]
    pub fn as_assistant(&self) -> Option<&Assistant> {
        match &self.body {
            InfoBody::Assistant(a) => Some(a),
            InfoBody::User(_) => None,
        }
    }

    /// The message creation timestamp (`time.created`), used to order the
    /// `message` table (`sql.ts:79`).
    #[must_use]
    pub fn time_created(&self) -> i64 {
        match &self.body {
            InfoBody::User(u) => u.time.created,
            InfoBody::Assistant(a) => a.time.created,
        }
    }
}

/// A message together with its ordered parts — the Rust analog of
/// `WithParts` (`session.ts:493-500`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WithParts {
    /// The message.
    pub info: Info,
    /// The message parts, in id order.
    pub parts: Vec<Part>,
}
