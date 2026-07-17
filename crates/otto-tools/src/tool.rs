//! The tool contract — a Rust port of the seam in opencode
//! `packages/opencode/src/tool/tool.ts`.
//!
//! opencode models a tool as a `Def` carrying an `id`, `description`,
//! parameter `Schema`, and an `execute` closure returning an
//! [`ExecuteResult`]. Here the same shape becomes the [`Tool`] trait plus the
//! supporting [`ToolContext`], permission/metadata seams, and the
//! [`ToolError`] taxonomy (including the model-facing
//! [`ToolError::InvalidArguments`] that mirrors `InvalidArgumentsError` at
//! `tool.ts:24-34`).

use std::path::PathBuf;
use std::sync::Arc;

use otto_id::{MessageId, SessionId};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::subagent::SubagentSpawner;

/// A file part attached to a tool result — the minimal subset of opencode's
/// `SessionV1.FilePart` (`tool.ts:52`) with the session/message ids removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// MIME type of the attached bytes, e.g. `image/png`.
    pub mime: String,
    /// Optional display filename.
    pub filename: Option<String>,
    /// URL (or `data:`/`file:` URI) locating the content.
    pub url: String,
}

/// The successful result of a tool run — mirrors `ExecuteResult`
/// (`tool.ts:48-53`). `metadata` is free-form JSON; the registry inspects it
/// for a `truncated` key to decide whether to post-process `output`.
#[derive(Debug, Clone)]
pub struct ExecuteResult {
    /// Short human/UI title for the run (e.g. the relative file path).
    pub title: String,
    /// Structured metadata; a `truncated` key opts out of registry truncation.
    pub metadata: Value,
    /// The model-facing textual output.
    pub output: String,
    /// Optional file attachments (images/PDFs).
    pub attachments: Vec<Attachment>,
}

impl ExecuteResult {
    /// Build a result with the given title/output and empty metadata.
    pub fn new(title: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            metadata: Value::Object(serde_json::Map::new()),
            output: output.into(),
            attachments: Vec::new(),
        }
    }

    /// Replace the metadata payload.
    #[must_use]
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }

    /// Attach a file part.
    #[must_use]
    pub fn with_attachment(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }
}

/// Raised when a caller (usually the LLM) supplies a permission that the gate
/// rejects. Ported from opencode's permission denial path.
#[derive(Debug, Clone)]
pub struct PermissionDenied {
    /// The permission that was rejected.
    pub permission: String,
    /// `true` when a human answered an ask with "reject" — the agent loop
    /// treats that as "stop the turn" (the user said no to the whole
    /// direction). `false` when a ruleset rule denied the call outright
    /// (agent/config policy): the tool fails with an error the model can
    /// adapt to, and the turn continues.
    pub by_user: bool,
    /// The human's typed `Reply::Reject` correction, when `by_user` and one
    /// was given. Always `None` for a policy deny (`by_user: false`) — those
    /// never go through a human reply.
    pub message: Option<String>,
}

impl std::fmt::Display for PermissionDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.by_user {
            // "denied" is load-bearing: the processor's rejection check
            // matches it and stops the turn (a human said no).
            match &self.message {
                Some(msg) => write!(f, "permission '{}' denied: {msg}", self.permission),
                None => write!(f, "permission '{}' denied", self.permission),
            }
        } else {
            // Deliberately avoids the "denied"/"rejected" keywords so a
            // policy deny reads as an ordinary tool failure the model can
            // route around (e.g. a subagent whose ruleset forbids todowrite)
            // instead of hard-stopping the turn.
            write!(
                f,
                "the '{}' permission is not granted to this agent; use a different approach",
                self.permission
            )
        }
    }
}

impl std::error::Error for PermissionDenied {}

/// A permission ask — the fields of opencode's `PermissionV1.Request` that a
/// tool controls (`tool.ts:45`, minus the host-managed `id`/`sessionID`/`tool`).
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    /// The permission being requested, e.g. `edit`, `write`, `bash`.
    pub permission: String,
    /// Concrete patterns (paths/commands) this call touches.
    pub patterns: Vec<String>,
    /// Patterns that, if approved with "always", cover this call.
    pub always: Vec<String>,
    /// Extra structured context (diff, filepath, command, ...).
    pub metadata: Value,
}

/// The permission seam. opencode threads `ctx.ask(...)` through every mutating
/// tool; here mutating tools call [`PermissionGate::ask`]. Phase 4 wires the
/// real interactive gate; [`AllowAll`] is the default no-op.
#[async_trait::async_trait]
pub trait PermissionGate: Send + Sync {
    /// Ask for permission. `Ok(())` grants; `Err` denies the operation.
    async fn ask(&self, req: PermissionRequest) -> Result<(), PermissionDenied>;
}

/// A permission gate that approves everything. Default for tests and for
/// contexts before the real gate lands.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAll;

#[async_trait::async_trait]
impl PermissionGate for AllowAll {
    async fn ask(&self, _req: PermissionRequest) -> Result<(), PermissionDenied> {
        Ok(())
    }
}

/// One selectable option in a `question` tool prompt (`QuestionV1.Option`).
#[derive(Debug, Clone, Deserialize)]
pub struct QuestionOption {
    /// Display text (1-5 words, concise).
    pub label: String,
    /// Explanation of this choice.
    pub description: String,
}

/// One question in a `question` tool call's batch (`QuestionV1.Prompt`).
#[derive(Debug, Clone, Deserialize)]
pub struct QuestionPrompt {
    /// The complete question text.
    pub question: String,
    /// Very short label (max 30 chars).
    pub header: String,
    /// Available choices.
    pub options: Vec<QuestionOption>,
    /// Whether multiple options may be selected.
    #[serde(default)]
    pub multiple: bool,
}

/// The user's answer to a question-tool ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionOutcome {
    /// One selected-index-list per question, in order (each inner `Vec`
    /// non-empty; exactly one element unless that question's `multiple` was
    /// `true`).
    Answered(Vec<Vec<usize>>),
    /// The user declined to answer.
    Cancelled,
}

/// The question-tool interactive seam. Mirrors [`PermissionGate`] but has no
/// allow/deny policy dimension — every ask reaches a human (or auto-cancels
/// non-interactively); there is nothing analogous to a config/agent ruleset
/// for a free-choice question.
#[async_trait::async_trait]
pub trait QuestionGate: Send + Sync {
    /// Ask the user to answer `questions`. Always resolves — there is no
    /// error path, only [`QuestionOutcome::Answered`] or
    /// [`QuestionOutcome::Cancelled`].
    async fn ask(&self, questions: Vec<QuestionPrompt>) -> QuestionOutcome;
}

/// A question gate that cancels every ask. Default for contexts before a
/// real interactive gate is wired (tests, headless tool-only usage) —
/// preserves today's "question tool doesn't work without a real client"
/// behavior, just via [`QuestionOutcome::Cancelled`] instead of a hard
/// `Err`.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAllQuestions;

#[async_trait::async_trait]
impl QuestionGate for DenyAllQuestions {
    async fn ask(&self, _questions: Vec<QuestionPrompt>) -> QuestionOutcome {
        QuestionOutcome::Cancelled
    }
}

/// Live-progress seam — mirrors opencode's `ctx.metadata({ title, metadata })`
/// (`tool.ts:44`). Fire-and-forget: tools stream partial state, the host may
/// render it. [`NoopSink`] discards updates.
pub trait MetadataSink: Send + Sync {
    /// Push a partial title/metadata update. Never fails; never blocks the tool.
    fn update(&self, title: Option<String>, metadata: Option<Value>);
}

/// A [`MetadataSink`] that ignores all updates.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSink;

impl MetadataSink for NoopSink {
    fn update(&self, _title: Option<String>, _metadata: Option<Value>) {}
}

/// Per-execution context handed to [`Tool::execute`] — the Rust analogue of
/// opencode's `Context` (`tool.ts:36-46`). Carries identity, the working
/// directory used to resolve relative paths and guard against external-dir
/// access, the cooperative [`CancellationToken`], and the permission/metadata
/// seams.
#[derive(Clone)]
pub struct ToolContext {
    /// Owning session id.
    pub session_id: SessionId,
    /// Owning message id.
    pub message_id: MessageId,
    /// Agent name (used by model gating / task tooling).
    pub agent: String,
    /// Working directory: relative paths resolve here and paths outside it
    /// trigger an `external_directory` permission ask.
    pub directory: PathBuf,
    /// Cooperative cancellation — bash and long tools honor this.
    pub abort: CancellationToken,
    /// Permission seam.
    pub permission: Arc<dyn PermissionGate>,
    /// Question-tool interactive seam.
    pub question: Arc<dyn QuestionGate>,
    /// Live-progress seam.
    pub metadata: Arc<dyn MetadataSink>,
    /// Subagent-spawn seam — `Some` only while a run loop is driving tool
    /// execution (the loop injects `RunConfig.subagent`). The `task` tool
    /// spawns through it; when `None` the tool reports subagents unavailable,
    /// preserving back-compat for call sites that build a bare context. The
    /// opencode analogue is the `Agent`/`Session` services pulled from the
    /// Effect context (task.ts:84-90).
    pub subagent: Option<Arc<dyn SubagentSpawner>>,
    /// Live event tap of the parent turn (`RunConfig.event_tx`), when a run
    /// loop is driving execution. The `task` tool forwards a *filtered* view
    /// of its child run's events through this so a subagent isn't a silent
    /// multi-minute pause on the client.
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<otto_events::LLMEvent>>,
}

impl ToolContext {
    /// Start building a context rooted at `directory` (defaults: `AllowAll`
    /// gate, `NoopSink`, fresh [`CancellationToken`], empty ids).
    pub fn builder(directory: impl Into<PathBuf>) -> ToolContextBuilder {
        ToolContextBuilder {
            session_id: SessionId::default(),
            message_id: MessageId::default(),
            agent: String::from("build"),
            directory: directory.into(),
            abort: CancellationToken::new(),
            permission: None,
            question: None,
            metadata: None,
            subagent: None,
            event_tx: None,
        }
    }
}

/// Builder for [`ToolContext`] — primarily for tests and call sites that only
/// care about a couple of fields.
pub struct ToolContextBuilder {
    session_id: SessionId,
    message_id: MessageId,
    agent: String,
    directory: PathBuf,
    abort: CancellationToken,
    permission: Option<Arc<dyn PermissionGate>>,
    question: Option<Arc<dyn QuestionGate>>,
    metadata: Option<Arc<dyn MetadataSink>>,
    subagent: Option<Arc<dyn SubagentSpawner>>,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<otto_events::LLMEvent>>,
}

impl ToolContextBuilder {
    /// Set the session id.
    #[must_use]
    pub fn session_id(mut self, id: impl Into<SessionId>) -> Self {
        self.session_id = id.into();
        self
    }

    /// Set the message id.
    #[must_use]
    pub fn message_id(mut self, id: impl Into<MessageId>) -> Self {
        self.message_id = id.into();
        self
    }

    /// Set the agent name.
    #[must_use]
    pub fn agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = agent.into();
        self
    }

    /// Set the cancellation token.
    #[must_use]
    pub fn abort(mut self, token: CancellationToken) -> Self {
        self.abort = token;
        self
    }

    /// Set the permission gate.
    #[must_use]
    pub fn permission(mut self, gate: Arc<dyn PermissionGate>) -> Self {
        self.permission = Some(gate);
        self
    }

    /// Set the question-tool gate.
    #[must_use]
    pub fn question(mut self, gate: Arc<dyn QuestionGate>) -> Self {
        self.question = Some(gate);
        self
    }

    /// Set the metadata sink.
    #[must_use]
    pub fn metadata(mut self, sink: Arc<dyn MetadataSink>) -> Self {
        self.metadata = Some(sink);
        self
    }

    /// Set the subagent-spawn seam. Left unset (`None`) the `task` tool reports
    /// subagents unavailable.
    #[must_use]
    pub fn subagent(mut self, spawner: Arc<dyn SubagentSpawner>) -> Self {
        self.subagent = Some(spawner);
        self
    }

    /// Set the parent turn's live event tap (see [`ToolContext::event_tx`]).
    #[must_use]
    pub fn event_tx(
        mut self,
        tx: tokio::sync::mpsc::UnboundedSender<otto_events::LLMEvent>,
    ) -> Self {
        self.event_tx = Some(tx);
        self
    }

    /// Finish building.
    pub fn build(self) -> ToolContext {
        ToolContext {
            session_id: self.session_id,
            message_id: self.message_id,
            agent: self.agent,
            directory: self.directory,
            abort: self.abort,
            permission: self.permission.unwrap_or_else(|| Arc::new(AllowAll)),
            question: self.question.unwrap_or_else(|| Arc::new(DenyAllQuestions)),
            metadata: self.metadata.unwrap_or_else(|| Arc::new(NoopSink)),
            subagent: self.subagent,
            event_tx: self.event_tx,
        }
    }
}

/// Error taxonomy for tool execution.
///
/// [`ToolError::InvalidArguments`] reproduces opencode's `InvalidArgumentsError`
/// message verbatim (`tool.ts:31-33`) so the model receives the identical
/// "rewrite the input" prose.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// Parameters failed to decode against the tool's schema.
    #[error(
        "The {tool} tool was called with invalid arguments: {detail}.\nPlease rewrite the input so it satisfies the expected schema."
    )]
    InvalidArguments {
        /// The tool id.
        tool: String,
        /// Decoder detail (serde message or custom explanation).
        detail: String,
    },

    /// A permission ask was denied.
    #[error(transparent)]
    Denied(#[from] PermissionDenied),

    /// An I/O error occurred.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A tool-specific execution failure (message is model-facing).
    #[error("{0}")]
    Execution(String),

    /// The run was cancelled via [`ToolContext::abort`].
    #[error("The operation was aborted")]
    Aborted,
}

/// The tool contract. Every built-in tool (read/write/edit/glob/grep/bash) and
/// every future tool implements this. Mirrors opencode's `Def` (`tool.ts:55-65`).
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Stable tool id exposed to the model (e.g. `read`, `edit`, `bash`).
    fn id(&self) -> &str;

    /// Human/model-facing description (the `.txt` prompt in opencode).
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters, shown to the LLM.
    fn parameters_schema(&self) -> Value;

    /// Execute the tool with decoded-from-JSON `args` and the shared context.
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError>;
}

/// Decode raw JSON tool arguments into a typed params struct, mapping serde
/// failures to [`ToolError::InvalidArguments`] — the Rust equivalent of the
/// `Schema.decodeUnknownEffect(...).mapError(InvalidArgumentsError)` wrapper in
/// `tool.ts:121-129`.
pub fn decode_args<T: DeserializeOwned>(tool_id: &str, args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|err| ToolError::InvalidArguments {
        tool: tool_id.to_string(),
        detail: err.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_arguments_display_is_exact() {
        let err = ToolError::InvalidArguments {
            tool: "read".to_string(),
            detail: "missing field `filePath`".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "The read tool was called with invalid arguments: missing field `filePath`.\nPlease rewrite the input so it satisfies the expected schema."
        );
    }

    #[test]
    fn decode_args_bad_input_yields_invalid_arguments() {
        #[derive(Debug, serde::Deserialize)]
        struct P {
            #[allow(dead_code)]
            file_path: String,
        }
        let err = decode_args::<P>("read", serde_json::json!({})).unwrap_err();
        match &err {
            ToolError::InvalidArguments { tool, .. } => assert_eq!(tool, "read"),
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
        assert!(
            err.to_string()
                .starts_with("The read tool was called with invalid arguments:")
        );
        assert!(
            err.to_string()
                .ends_with("Please rewrite the input so it satisfies the expected schema.")
        );
    }

    #[test]
    fn by_user_denial_display_includes_message_when_present() {
        let with_msg = PermissionDenied {
            permission: "hook".to_string(),
            by_user: true,
            message: Some("try a different approach".to_string()),
        };
        assert_eq!(
            with_msg.to_string(),
            "permission 'hook' denied: try a different approach"
        );

        let without_msg = PermissionDenied {
            permission: "hook".to_string(),
            by_user: true,
            message: None,
        };
        assert_eq!(without_msg.to_string(), "permission 'hook' denied");
    }

    #[test]
    fn policy_denial_display_ignores_message_field() {
        let denial = PermissionDenied {
            permission: "todowrite".to_string(),
            by_user: false,
            message: None,
        };
        assert!(!denial.to_string().contains("denied"));
    }

    #[tokio::test]
    async fn deny_all_questions_always_cancels() {
        let gate = DenyAllQuestions;
        let outcome = gate
            .ask(vec![QuestionPrompt {
                question: "pick one".into(),
                header: "choice".into(),
                options: vec![QuestionOption {
                    label: "A".into(),
                    description: "first".into(),
                }],
                multiple: false,
            }])
            .await;
        assert_eq!(outcome, QuestionOutcome::Cancelled);
    }

    #[test]
    fn tool_context_defaults_to_deny_all_questions() {
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        // No behavioral assertion beyond "it builds" — DenyAllQuestions'
        // behavior itself is covered by the test above; this just confirms
        // the builder's default wiring compiles and doesn't panic.
        let _ = ctx.question;
    }
}
