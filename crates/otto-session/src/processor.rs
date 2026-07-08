//! The session `Processor` — an `LLMEvent → storage part` state machine.
//!
//! A faithful Rust port of opencode's `packages/opencode/src/session/processor.ts`.
//! A [`Processor`] consumes a stream of [`LLMEvent`]s for a single assistant
//! message and mutates the [`Store`] accordingly: it materializes text,
//! reasoning, tool, and step parts, transitions tool parts through their
//! lifecycle (`pending → running → completed | error`), tracks token/cost
//! accounting on the assistant message, and — when the stream drains, errors,
//! or is aborted — runs [`Processor::cleanup`] to flush open parts and mark any
//! dangling tools as interrupted.
//!
//! Unlike the opencode original this port omits the snapshot / summary / status
//! / plugin / retry seams (not yet present in otto); the event → part logic,
//! the doom-loop guard (`processor.ts:351-378`), and the cleanup invariants
//! (`processor.ts:537-595`) are ported directly.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::{Stream, StreamExt};
use otto_events::{FinishReason, LLMEvent, ProviderFailureClassification, ToolResultValue, Usage};
use otto_llm::{LLMError, Model};
use otto_storage::model::{
    Assistant, CompletedTime, Info, InfoBody, Json, Part, PartKind, StartEndReqTime, StartEndTime,
    StartTime, TokenCache, Tokens, ToolState, new_part_id,
};
use otto_storage::{StorageError, Store};
use otto_tools::tool::{PermissionGate, PermissionRequest};
use tokio::sync::oneshot;

/// Doom-loop detection window: this many identical, consecutive tool calls
/// trigger a `doom_loop` permission ask (`processor.ts:29`).
const DOOM_LOOP_THRESHOLD: usize = 3;

/// Grace period to await an in-flight tool call during [`Processor::cleanup`]
/// before force-marking it interrupted (`processor.ts:571`).
const CLEANUP_TOOL_GRACE_MS: u64 = 250;

/// The outcome of processing one assistant turn — the Rust analog of opencode's
/// `Result` union (`processor.ts:30`, return values at `processor.ts:677-679`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOutcome {
    /// The context window overflowed; the caller should compact and retry
    /// (`ctx.needsCompaction`, `processor.ts:677`).
    Compact,
    /// The turn is finished and the loop should stop — either the assistant was
    /// blocked (a permission/question denial with break semantics) or the
    /// message carries a terminal error (`processor.ts:678`).
    Stop,
    /// The turn produced output and the loop may continue (`processor.ts:679`).
    Continue,
}

/// Errors raised while processing an event stream.
#[derive(Debug, thiserror::Error)]
pub enum ProcessorError {
    /// A persistence failure while reading or writing the store.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// The underlying LLM stream yielded an error item.
    #[error(transparent)]
    Llm(#[from] LLMError),
    /// A `provider-error` event terminated the stream (`processor.ts:419-420`).
    #[error("provider error: {0}")]
    Provider(String),
    /// A tool call was emitted while generating a summary message
    /// (`processor.ts:314-316,330-332`).
    #[error("tool call not allowed while generating summary: {0}")]
    ToolWhileSummary(String),
    /// The assistant message named by the constructor was not found in the store.
    #[error("assistant message {0} not found")]
    MissingMessage(String),
    /// The message named by the constructor is not an assistant message.
    #[error("message {0} is not an assistant message")]
    NotAssistant(String),
}

/// Per-tool-call bookkeeping — the Rust analog of opencode's `ToolCall`
/// (`processor.ts:60-65`). Tracks the tool part's id and a one-shot `done`
/// signal fired when the call settles (completes or errors).
struct ToolCallHandle {
    /// Id of the backing [`PartKind::Tool`] part.
    part_id: String,
    /// Sender half of the `done` signal, taken and fired on settle.
    done_tx: Option<oneshot::Sender<()>>,
    /// Receiver half, taken and awaited in [`Processor::cleanup`].
    done_rx: Option<oneshot::Receiver<()>>,
}

/// Accumulated streaming state for the currently-open text part.
struct TextAccum {
    /// Backing part id.
    part_id: String,
    /// Concatenated text so far.
    text: String,
    /// Millisecond start timestamp.
    start: i64,
    /// Latest provider metadata, if any.
    metadata: Option<Json>,
}

/// Accumulated streaming state for one open reasoning block
/// (keyed by `reasoning-*` block id in [`Processor::reasoning_map`]).
struct ReasoningAccum {
    /// Backing part id.
    part_id: String,
    /// Concatenated reasoning text so far.
    text: String,
    /// Millisecond start timestamp.
    start: i64,
    /// Latest provider metadata, if any.
    metadata: Option<Json>,
}

/// Consumes a `Stream<LLMEvent>` for a single assistant message and drives the
/// storage part state machine. Port of opencode's `SessionProcessor` handle
/// (`processor.ts:98-691`), minus the snapshot/summary/status seams.
pub struct Processor {
    store: Store,
    session_id: String,
    assistant_message_id: String,
    model: Model,
    #[allow(dead_code)]
    agent: String,
    permission: Arc<dyn PermissionGate>,

    // -- ctx state (processor.ts:67-75) -------------------------------------
    /// In-memory copy of the assistant message, mutated as steps finish and
    /// re-persisted via [`Store::update_message`] (`ctx.assistantMessage`).
    message: Option<Info>,
    /// Live tool calls keyed by provider tool-call id (`ctx.toolcalls`).
    toolcalls: HashMap<String, ToolCallHandle>,
    /// Optional git snapshot ref (no snapshot seam yet — always `None`).
    snapshot: Option<String>,
    /// Whether the turn was blocked by a denial (`ctx.blocked`).
    blocked: bool,
    /// Whether a denial should break the loop (`ctx.shouldBreak`).
    should_break: bool,
    /// Whether the context window overflowed (`ctx.needsCompaction`).
    needs_compaction: bool,
    /// The open text part, if any (`ctx.currentText`).
    current_text: Option<TextAccum>,
    /// Open reasoning blocks keyed by block id (`ctx.reasoningMap`).
    reasoning_map: HashMap<String, ReasoningAccum>,
    /// Whether a terminal [`LLMEvent::Finish`] was seen this turn — the signal
    /// `LLMResponse.complete()` keys off. A clean EOF without it is a truncated
    /// response (Fix 4).
    saw_finish: bool,
    /// Whether the assistant produced real output this turn (text / reasoning /
    /// tool-call content). Distinguishes a truncated response (content but no
    /// finish) from an empty turn (Fix 4).
    saw_content: bool,
}

/// Current wall-clock time in milliseconds since the Unix epoch — the Rust
/// analog of `Date.now()`.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Serializes a [`FinishReason`] to the string opencode stores in
/// `assistantMessage.finish` (`processor.ts:441`).
fn finish_reason_str(reason: FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolCalls => "tool-calls",
        FinishReason::ContentFilter => "content-filter",
        FinishReason::Error => "error",
        FinishReason::Unknown => "unknown",
    }
    .to_string()
}

/// Whether a tool-error message denotes a permission / question rejection — the
/// port's stand-in for opencode's `error instanceof PermissionV1.RejectedError
/// || Question.RejectedError` check (`processor.ts:198`). Matches the
/// [`otto_tools::tool::PermissionDenied`] display (`… denied`) and question
/// rejections (`… rejected`).
fn is_rejection(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("denied") || lowered.contains("rejected")
}

impl Processor {
    /// Builds a processor for one assistant message. The message itself is
    /// loaded lazily from the store at the start of [`Processor::process`].
    #[must_use]
    pub fn new(
        store: Store,
        session_id: impl Into<String>,
        assistant_message_id: impl Into<String>,
        model: Model,
        agent: impl Into<String>,
        permission: Arc<dyn PermissionGate>,
    ) -> Self {
        Self {
            store,
            session_id: session_id.into(),
            assistant_message_id: assistant_message_id.into(),
            model,
            agent: agent.into(),
            permission,
            message: None,
            toolcalls: HashMap::new(),
            snapshot: None,
            blocked: false,
            should_break: true,
            needs_compaction: false,
            current_text: None,
            reasoning_map: HashMap::new(),
            saw_finish: false,
            saw_content: false,
        }
    }

    /// Consumes the event `stream`, mutating the store for the assistant
    /// message, and returns the turn's [`ProcessOutcome`]. Port of
    /// `SessionProcessor.process` (`processor.ts:625-681`): the stream drain is
    /// wrapped so that [`Processor::cleanup`] ALWAYS runs — on normal
    /// completion, on a stream error, and on an early `provider-error` — before
    /// the outcome is computed or the error propagated.
    ///
    /// # Errors
    /// Returns [`ProcessorError`] on a storage failure, an LLM stream error, a
    /// `provider-error` event, or a tool call during summary generation. Even in
    /// the error case, cleanup has already run.
    pub async fn process<S>(&mut self, stream: S) -> Result<ProcessOutcome, ProcessorError>
    where
        S: Stream<Item = Result<LLMEvent, LLMError>> + Unpin,
    {
        // Load the assistant message into ctx (`ctx.assistantMessage`).
        let info = self
            .store
            .get_message(&self.session_id, &self.assistant_message_id)
            .await?
            .ok_or_else(|| ProcessorError::MissingMessage(self.assistant_message_id.clone()))?;
        if !info.is_assistant() {
            return Err(ProcessorError::NotAssistant(
                self.assistant_message_id.clone(),
            ));
        }
        self.message = Some(info);

        // Reset per-run ctx (`processor.ts:630-637`).
        self.needs_compaction = false;
        self.blocked = false;
        self.should_break = true;
        self.current_text = None;
        self.reasoning_map.clear();
        self.toolcalls.clear();
        self.saw_finish = false;
        self.saw_content = false;

        // Drain the stream, stopping after `needs_compaction` flips
        // (`Stream.takeUntil`, `processor.ts:642`). The result is captured so
        // cleanup can run unconditionally afterwards (`Effect.ensuring`).
        let drain: Result<(), ProcessorError> = async {
            let mut stream = stream;
            while let Some(item) = stream.next().await {
                let event = item.map_err(ProcessorError::Llm)?;
                self.handle_event(event).await?;
                if self.needs_compaction {
                    break;
                }
            }
            Ok(())
        }
        .await;

        // `Effect.ensuring(cleanup())` — cleanup runs on every path.
        let clean = self.cleanup().await;
        drain?;
        clean?;

        // A clean EOF with real content but no terminal `Finish` is a truncated /
        // halted response — surface it as a retryable error so the run loop
        // retries. Gated so compaction / blocked / error / empty turns are not
        // misclassified; placed after cleanup so cleanup still ran (Fix 4).
        if !self.saw_finish
            && self.saw_content
            && !self.needs_compaction
            && !self.blocked
            && !self.assistant().map(|a| a.error.is_some()).unwrap_or(false)
        {
            return Err(ProcessorError::Llm(LLMError::NoTerminalFinish));
        }

        // Compute outcome (`processor.ts:677-679`).
        if self.needs_compaction {
            return Ok(ProcessOutcome::Compact);
        }
        let has_error = self.assistant().map(|a| a.error.is_some()).unwrap_or(false);
        if self.blocked || has_error {
            return Ok(ProcessOutcome::Stop);
        }
        Ok(ProcessOutcome::Continue)
    }

    // -- assistant message accessors ----------------------------------------

    /// Immutable view of the in-memory assistant payload.
    fn assistant(&self) -> Option<&Assistant> {
        match self.message.as_ref()?.body {
            InfoBody::Assistant(ref a) => Some(a),
            InfoBody::User(_) => None,
        }
    }

    /// Mutable view of the in-memory assistant payload.
    fn assistant_mut(&mut self) -> &mut Assistant {
        match self.message.as_mut().expect("message loaded").body {
            InfoBody::Assistant(ref mut a) => a,
            InfoBody::User(_) => unreachable!("processor message is always assistant"),
        }
    }

    /// Whether the assistant message is a compaction summary
    /// (`ctx.assistantMessage.summary`).
    fn is_summary(&self) -> bool {
        self.assistant().and_then(|a| a.summary).unwrap_or(false)
    }

    // -- event dispatch (processor.ts:276-535) ------------------------------

    /// Applies a single event to the store. Port of `handleEvent`
    /// (`processor.ts:276-535`).
    async fn handle_event(&mut self, event: LLMEvent) -> Result<(), ProcessorError> {
        match event {
            LLMEvent::ReasoningStart {
                id,
                provider_metadata,
            } => {
                self.saw_content = true;
                self.on_reasoning_start(id, provider_metadata).await
            }
            LLMEvent::ReasoningDelta {
                id,
                text,
                provider_metadata,
            } => {
                self.saw_content = true;
                self.on_reasoning_delta(&id, &text, provider_metadata).await
            }
            LLMEvent::ReasoningEnd {
                id,
                provider_metadata,
            } => self.on_reasoning_end(&id, provider_metadata).await,

            LLMEvent::ToolInputStart { id, name, .. } => {
                if self.is_summary() {
                    return Err(ProcessorError::ToolWhileSummary(name));
                }
                self.saw_content = true;
                self.ensure_tool_call(&id).await?;
                Ok(())
            }
            LLMEvent::ToolInputDelta { id, .. } => {
                self.ensure_tool_call(&id).await?;
                Ok(())
            }
            LLMEvent::ToolInputEnd { id, .. } => {
                self.ensure_tool_call(&id).await?;
                Ok(())
            }
            LLMEvent::ToolCall {
                id, name, input, ..
            } => {
                self.saw_content = true;
                self.on_tool_call(id, name, input).await
            }
            LLMEvent::ToolResult {
                id, name, result, ..
            } => self.on_tool_result(&id, &name, result).await,
            LLMEvent::ToolError { id, message, .. } => self.on_tool_error(&id, &message).await,

            LLMEvent::ProviderError {
                message,
                classification,
                retryable,
                ..
            } => self.on_provider_error(message, classification, retryable),

            LLMEvent::StepStart { .. } => self.on_step_start().await,
            LLMEvent::StepFinish { reason, usage, .. } => self.on_step_finish(reason, usage).await,

            LLMEvent::TextStart {
                provider_metadata, ..
            } => {
                self.saw_content = true;
                self.on_text_start(provider_metadata).await
            }
            LLMEvent::TextDelta {
                text,
                provider_metadata,
                ..
            } => {
                self.saw_content = true;
                self.on_text_delta(&text, provider_metadata).await
            }
            LLMEvent::TextEnd {
                provider_metadata, ..
            } => self.on_text_end(provider_metadata).await,

            LLMEvent::Finish { .. } => {
                self.saw_finish = true;
                Ok(())
            }
            LLMEvent::Retry { .. } => Ok(()),
        }
    }

    /// Classify a mid-stream `provider-error` event (Fix 2). A context-overflow
    /// classification routes to compaction; a provider-flagged-transient error
    /// (or a rate-limit / overloaded message) becomes a retryable
    /// [`LLMError::ProviderRetryable`]; anything else is a terminal
    /// [`ProcessorError::Provider`].
    ///
    // NOTE: `ProviderFailureClassification` has only `ContextOverflow` today. If
    // rate-limit / overloaded variants are added later, extend the retryable
    // branch to consider them here.
    fn on_provider_error(
        &mut self,
        message: String,
        classification: Option<ProviderFailureClassification>,
        retryable: Option<bool>,
    ) -> Result<(), ProcessorError> {
        if classification == Some(ProviderFailureClassification::ContextOverflow) {
            // Route overflow-as-provider-error to the compaction path; the drain
            // loop breaks and `process()` returns `ProcessOutcome::Compact`.
            self.needs_compaction = true;
            return Ok(());
        }
        if retryable == Some(true) || crate::retry::has_rate_limit_pattern(&message) {
            return Err(ProcessorError::Llm(LLMError::ProviderRetryable(message)));
        }
        Err(ProcessorError::Provider(message))
    }

    // -- reasoning (processor.ts:278-311) -----------------------------------

    async fn on_reasoning_start(
        &mut self,
        id: String,
        metadata: Option<Json>,
    ) -> Result<(), ProcessorError> {
        if self.reasoning_map.contains_key(&id) {
            return Ok(());
        }
        let part_id = new_part_id();
        let start = now_ms();
        self.reasoning_map.insert(
            id,
            ReasoningAccum {
                part_id: part_id.clone(),
                text: String::new(),
                start,
                metadata: metadata.clone(),
            },
        );
        let part = self.reasoning_part(&part_id, "", start, None, metadata);
        self.store.update_part(&part).await?;
        Ok(())
    }

    async fn on_reasoning_delta(
        &mut self,
        id: &str,
        text: &str,
        metadata: Option<Json>,
    ) -> Result<(), ProcessorError> {
        // Silently drop orphan deltas (`processor.ts:293-294`).
        let Some(accum) = self.reasoning_map.get_mut(id) else {
            return Ok(());
        };
        accum.text.push_str(text);
        if metadata.is_some() {
            accum.metadata = metadata;
        }
        let (part_id, text, start, meta) = (
            accum.part_id.clone(),
            accum.text.clone(),
            accum.start,
            accum.metadata.clone(),
        );
        let part = self.reasoning_part(&part_id, &text, start, None, meta);
        self.store.update_part(&part).await?;
        Ok(())
    }

    async fn on_reasoning_end(
        &mut self,
        id: &str,
        metadata: Option<Json>,
    ) -> Result<(), ProcessorError> {
        if let Some(accum) = self.reasoning_map.get_mut(id)
            && metadata.is_some()
        {
            accum.metadata = metadata;
        }
        self.finish_reasoning(id).await
    }

    /// Stamp the end time on an open reasoning block, persist it, and drop it
    /// from the map (`finishReasoning`, `processor.ts:205-212`).
    async fn finish_reasoning(&mut self, id: &str) -> Result<(), ProcessorError> {
        let Some(accum) = self.reasoning_map.remove(id) else {
            return Ok(());
        };
        let part = self.reasoning_part(
            &accum.part_id,
            &accum.text,
            accum.start,
            Some(now_ms()),
            accum.metadata,
        );
        self.store.update_part(&part).await?;
        Ok(())
    }

    /// Build a [`PartKind::Reasoning`] [`Part`] for the assistant message.
    fn reasoning_part(
        &self,
        part_id: &str,
        text: &str,
        start: i64,
        end: Option<i64>,
        metadata: Option<Json>,
    ) -> Part {
        Part {
            id: part_id.to_string(),
            session_id: self.session_id.clone(),
            message_id: self.assistant_message_id.clone(),
            kind: PartKind::Reasoning {
                text: text.to_string(),
                time: StartEndTime { start, end },
                metadata,
            },
        }
    }

    // -- text (processor.ts:484-530) ----------------------------------------

    async fn on_text_start(&mut self, metadata: Option<Json>) -> Result<(), ProcessorError> {
        let part_id = new_part_id();
        let start = now_ms();
        self.current_text = Some(TextAccum {
            part_id: part_id.clone(),
            text: String::new(),
            start,
            metadata: metadata.clone(),
        });
        let part = self.text_part(&part_id, "", start, None, metadata);
        self.store.update_part(&part).await?;
        Ok(())
    }

    async fn on_text_delta(
        &mut self,
        text: &str,
        metadata: Option<Json>,
    ) -> Result<(), ProcessorError> {
        let Some(accum) = self.current_text.as_mut() else {
            return Ok(());
        };
        accum.text.push_str(text);
        if metadata.is_some() {
            accum.metadata = metadata;
        }
        let (part_id, text, start, meta) = (
            accum.part_id.clone(),
            accum.text.clone(),
            accum.start,
            accum.metadata.clone(),
        );
        let part = self.text_part(&part_id, &text, start, None, meta);
        self.store.update_part(&part).await?;
        Ok(())
    }

    async fn on_text_end(&mut self, metadata: Option<Json>) -> Result<(), ProcessorError> {
        let Some(mut accum) = self.current_text.take() else {
            return Ok(());
        };
        if metadata.is_some() {
            accum.metadata = metadata;
        }
        let part = self.text_part(
            &accum.part_id,
            &accum.text,
            accum.start,
            Some(now_ms()),
            accum.metadata,
        );
        self.store.update_part(&part).await?;
        Ok(())
    }

    /// Build a [`PartKind::Text`] [`Part`] for the assistant message.
    fn text_part(
        &self,
        part_id: &str,
        text: &str,
        start: i64,
        end: Option<i64>,
        metadata: Option<Json>,
    ) -> Part {
        Part {
            id: part_id.to_string(),
            session_id: self.session_id.clone(),
            message_id: self.assistant_message_id.clone(),
            kind: PartKind::Text {
                text: text.to_string(),
                synthetic: None,
                ignored: None,
                time: Some(StartEndTime { start, end }),
                metadata,
            },
        }
    }

    // -- steps (processor.ts:422-482) ---------------------------------------

    async fn on_step_start(&mut self) -> Result<(), ProcessorError> {
        let part = Part {
            id: new_part_id(),
            session_id: self.session_id.clone(),
            message_id: self.assistant_message_id.clone(),
            kind: PartKind::StepStart {
                snapshot: self.snapshot.clone(),
            },
        };
        self.store.update_part(&part).await?;
        Ok(())
    }

    async fn on_step_finish(
        &mut self,
        reason: FinishReason,
        usage: Option<Usage>,
    ) -> Result<(), ProcessorError> {
        // Flush any open reasoning blocks first (`processor.ts:435`).
        let open: Vec<String> = self.reasoning_map.keys().cloned().collect();
        for id in open {
            self.finish_reasoning(&id).await?;
        }

        let usage = usage.unwrap_or_default();
        let (tokens, cost) = compute_usage(&self.model, &usage);

        let reason_str = finish_reason_str(reason);
        {
            let a = self.assistant_mut();
            a.finish = Some(reason_str.clone());
            a.cost += cost;
            a.tokens = tokens.clone();
        }

        let part = Part {
            id: new_part_id(),
            session_id: self.session_id.clone(),
            message_id: self.assistant_message_id.clone(),
            kind: PartKind::StepFinish {
                reason: reason_str,
                snapshot: self.snapshot.clone(),
                cost,
                tokens: tokens.clone(),
            },
        };
        self.store.update_part(&part).await?;
        let message = self.message.clone().expect("message loaded");
        self.store.update_message(&message).await?;

        // Overflow check (`processor.ts:475-480`, simplified): the recorded
        // token count meeting the model's context window flips compaction.
        if !self.is_summary()
            && let Some(context) = self.model.limits.context
            && context > 0
        {
            let count = tokens
                .total
                .unwrap_or(tokens.input + tokens.output + tokens.cache.read + tokens.cache.write);
            if count >= context as f64 {
                self.needs_compaction = true;
            }
        }
        Ok(())
    }

    // -- tool calls (processor.ts:214-417) ----------------------------------

    /// Read the current [`PartKind::Tool`] part backing a tool call, if any.
    async fn read_tool_part(&self, call_id: &str) -> Result<Option<Part>, ProcessorError> {
        let Some(handle) = self.toolcalls.get(call_id) else {
            return Ok(None);
        };
        let parts = self.store.list_parts(&self.assistant_message_id).await?;
        Ok(parts
            .into_iter()
            .find(|p| p.id == handle.part_id && matches!(p.kind, PartKind::Tool { .. })))
    }

    /// Ensure a tool part exists in `pending` state for `call_id`, registering
    /// its `done` signal (`ensureToolCall`, `processor.ts:214-251`).
    async fn ensure_tool_call(&mut self, call_id: &str) -> Result<String, ProcessorError> {
        if let Some(handle) = self.toolcalls.get(call_id) {
            return Ok(handle.part_id.clone());
        }
        let part_id = new_part_id();
        let part = Part {
            id: part_id.clone(),
            session_id: self.session_id.clone(),
            message_id: self.assistant_message_id.clone(),
            kind: PartKind::Tool {
                call_id: call_id.to_string(),
                tool: String::new(),
                metadata: None,
                state: ToolState::Pending {
                    input: Json::Object(serde_json::Map::new()),
                    raw: String::new(),
                },
            },
        };
        self.store.update_part(&part).await?;
        let (tx, rx) = oneshot::channel();
        self.toolcalls.insert(
            call_id.to_string(),
            ToolCallHandle {
                part_id: part_id.clone(),
                done_tx: Some(tx),
                done_rx: Some(rx),
            },
        );
        Ok(part_id)
    }

    /// Fire a tool call's `done` signal and forget it (`settleToolCall`,
    /// `processor.ts:123-127`).
    fn settle_tool_call(&mut self, call_id: &str) {
        if let Some(mut handle) = self.toolcalls.remove(call_id)
            && let Some(tx) = handle.done_tx.take()
        {
            let _ = tx.send(());
        }
    }

    async fn on_tool_call(
        &mut self,
        id: String,
        name: String,
        input: Json,
    ) -> Result<(), ProcessorError> {
        if self.is_summary() {
            return Err(ProcessorError::ToolWhileSummary(name));
        }
        let part_id = self.ensure_tool_call(&id).await?;
        // Normalize non-object inputs to `{ value: <input> }` (`processor.ts:334`).
        let input = if input.is_object() {
            input
        } else {
            let mut map = serde_json::Map::new();
            map.insert("value".to_string(), input);
            Json::Object(map)
        };

        // Transition pending → running (or keep running with new input)
        // (`processor.ts:335-349`).
        let current = self.read_tool_part(&id).await?;
        let start = match current.as_ref().map(|p| &p.kind) {
            Some(PartKind::Tool {
                state: ToolState::Running { time, .. },
                ..
            }) => time.start,
            _ => now_ms(),
        };
        let part = Part {
            id: part_id,
            session_id: self.session_id.clone(),
            message_id: self.assistant_message_id.clone(),
            kind: PartKind::Tool {
                call_id: id.clone(),
                tool: name.clone(),
                metadata: None,
                state: ToolState::Running {
                    input: input.clone(),
                    title: None,
                    metadata: None,
                    time: StartTime { start },
                },
            },
        };
        self.store.update_part(&part).await?;

        // Doom-loop guard (`processor.ts:351-378`): the last three parts are all
        // the same tool, past pending, with byte-identical input.
        let parts = self.store.list_parts(&self.assistant_message_id).await?;
        let recent: Vec<&Part> = parts.iter().rev().take(DOOM_LOOP_THRESHOLD).collect();
        let looped = recent.len() == DOOM_LOOP_THRESHOLD
            && recent.iter().all(|p| match &p.kind {
                PartKind::Tool { tool, state, .. } => {
                    tool == &name
                        && !matches!(state, ToolState::Pending { .. })
                        && tool_state_input(state) == Some(&input)
                }
                _ => false,
            });
        if looped {
            let metadata = serde_json::json!({ "tool": name, "input": input });
            let request = PermissionRequest {
                permission: "doom_loop".to_string(),
                patterns: vec![name.clone()],
                always: vec![name.clone()],
                metadata,
            };
            if self.permission.ask(request).await.is_err() {
                self.blocked = self.should_break;
            }
        }
        Ok(())
    }

    async fn on_tool_result(
        &mut self,
        id: &str,
        name: &str,
        result: ToolResultValue,
    ) -> Result<(), ProcessorError> {
        let existing = self.read_tool_part(id).await?;
        // Unknown call + error result → ignore (`processor.ts:382-383`).
        if existing.is_none() && matches!(result, ToolResultValue::Error { .. }) {
            return Ok(());
        }
        if let ToolResultValue::Error { value } = &result {
            let message = json_error_message(value);
            self.fail_tool_call(id, &message).await?;
            return Ok(());
        }
        let (title, metadata, output) = tool_result_output(name, &result);
        self.complete_tool_call(id, title, metadata, output).await
    }

    async fn on_tool_error(&mut self, id: &str, message: &str) -> Result<(), ProcessorError> {
        self.fail_tool_call(id, message).await?;
        Ok(())
    }

    /// Transition a `running` tool part to `completed` and settle it
    /// (`completeToolCall`, `processor.ts:160-184`).
    async fn complete_tool_call(
        &mut self,
        id: &str,
        title: String,
        metadata: Json,
        output: String,
    ) -> Result<(), ProcessorError> {
        let Some(part) = self.read_tool_part(id).await? else {
            return Ok(());
        };
        let (input, start, is_running) = match &part.kind {
            PartKind::Tool {
                state: ToolState::Running { input, time, .. },
                ..
            } => (input.clone(), time.start, true),
            _ => (Json::Null, now_ms(), false),
        };
        if !is_running {
            return Ok(());
        }
        let call_id = tool_call_id(&part).to_string();
        let tool = tool_name(&part).to_string();
        let updated = Part {
            id: part.id,
            session_id: part.session_id,
            message_id: part.message_id,
            kind: PartKind::Tool {
                call_id,
                tool,
                metadata: None,
                state: ToolState::Completed {
                    input,
                    output,
                    title,
                    metadata,
                    time: CompletedTime {
                        start,
                        end: now_ms(),
                        compacted: None,
                    },
                    attachments: None,
                },
            },
        };
        self.store.update_part(&updated).await?;
        self.settle_tool_call(id);
        Ok(())
    }

    /// Transition a `running` tool part to `error` and settle it
    /// (`failToolCall`, `processor.ts:186-203`). A rejection error blocks the
    /// loop when `should_break` is set.
    async fn fail_tool_call(&mut self, id: &str, message: &str) -> Result<(), ProcessorError> {
        let Some(part) = self.read_tool_part(id).await? else {
            return Ok(());
        };
        let (input, start, is_running) = match &part.kind {
            PartKind::Tool {
                state: ToolState::Running { input, time, .. },
                ..
            } => (input.clone(), time.start, true),
            _ => (Json::Null, now_ms(), false),
        };
        if !is_running {
            return Ok(());
        }
        let call_id = tool_call_id(&part).to_string();
        let tool = tool_name(&part).to_string();
        let updated = Part {
            id: part.id,
            session_id: part.session_id,
            message_id: part.message_id,
            kind: PartKind::Tool {
                call_id,
                tool,
                metadata: None,
                state: ToolState::Error {
                    input,
                    error: message.to_string(),
                    metadata: None,
                    time: StartEndReqTime {
                        start,
                        end: now_ms(),
                    },
                },
            },
        };
        self.store.update_part(&updated).await?;
        if is_rejection(message) {
            self.blocked = self.should_break;
        }
        self.settle_tool_call(id);
        Ok(())
    }

    // -- cleanup (processor.ts:537-595) -------------------------------------

    /// Flush open parts, await in-flight tools briefly, force-interrupt any
    /// survivors, and stamp the assistant message complete. Port of `cleanup`
    /// (`processor.ts:537-595`); runs on every exit path from
    /// [`Processor::process`].
    ///
    /// # Errors
    /// Returns [`ProcessorError::Storage`] on a persistence failure.
    pub async fn cleanup(&mut self) -> Result<(), ProcessorError> {
        // 1. Flush the open text part (`processor.ts:553-558`).
        if let Some(mut accum) = self.current_text.take() {
            let end = now_ms();
            if accum.start == 0 {
                accum.start = end;
            }
            let part = self.text_part(
                &accum.part_id,
                &accum.text,
                accum.start,
                Some(end),
                accum.metadata,
            );
            self.store.update_part(&part).await?;
        }

        // 2. Flush open reasoning parts (`processor.ts:560-567`).
        let reasoning: Vec<ReasoningAccum> = self.reasoning_map.drain().map(|(_, v)| v).collect();
        for accum in reasoning {
            let end = now_ms();
            let start = if accum.start == 0 { end } else { accum.start };
            let part = self.reasoning_part(
                &accum.part_id,
                &accum.text,
                start,
                Some(end),
                accum.metadata,
            );
            self.store.update_part(&part).await?;
        }

        // 3. Await in-flight tool calls up to the grace period
        //    (`processor.ts:569-573`). Nothing external settles them in this
        //    single-stream port, so this simply times out for true survivors.
        let waits: Vec<oneshot::Receiver<()>> = self
            .toolcalls
            .values_mut()
            .filter_map(|h| h.done_rx.take())
            .collect();
        for rx in waits {
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(CLEANUP_TOOL_GRACE_MS), rx)
                    .await;
        }

        // 4. Force-mark survivors interrupted (`processor.ts:575-591`). The
        //    `interrupted: true` metadata flag is the invariant the converter's
        //    dangling-tool handling and the loop's orphan-detection key on.
        let survivors: Vec<String> = self.toolcalls.keys().cloned().collect();
        for call_id in survivors {
            let Some(part) = self.read_tool_part(&call_id).await? else {
                continue;
            };
            let end = now_ms();
            let (input, start, existing_meta) = match &part.kind {
                PartKind::Tool { state, .. } => (
                    tool_state_input(state).cloned().unwrap_or(Json::Null),
                    tool_state_start(state).unwrap_or(end),
                    tool_state_metadata(state).cloned(),
                ),
                _ => (Json::Null, end, None),
            };
            let mut metadata = match existing_meta {
                Some(Json::Object(map)) => map,
                _ => serde_json::Map::new(),
            };
            metadata.insert("interrupted".to_string(), Json::Bool(true));
            let call_id_field = tool_call_id(&part).to_string();
            let tool = tool_name(&part).to_string();
            let updated = Part {
                id: part.id,
                session_id: part.session_id,
                message_id: part.message_id,
                kind: PartKind::Tool {
                    call_id: call_id_field,
                    tool,
                    metadata: None,
                    state: ToolState::Error {
                        input,
                        error: "Tool execution aborted".to_string(),
                        metadata: Some(Json::Object(metadata)),
                        time: StartEndReqTime { start, end },
                    },
                },
            };
            self.store.update_part(&updated).await?;
        }
        self.toolcalls.clear();

        // 5. Stamp the assistant message completed (`processor.ts:593-594`).
        self.assistant_mut().time.completed = Some(now_ms());
        let message = self.message.clone().expect("message loaded");
        self.store.update_message(&message).await?;
        Ok(())
    }
}

// -- free helpers -----------------------------------------------------------

/// Borrow a tool state's `input` field, present on every variant.
fn tool_state_input(state: &ToolState) -> Option<&Json> {
    match state {
        ToolState::Pending { input, .. }
        | ToolState::Running { input, .. }
        | ToolState::Completed { input, .. }
        | ToolState::Error { input, .. } => Some(input),
    }
}

/// Borrow a tool state's start timestamp, if the state carries one.
fn tool_state_start(state: &ToolState) -> Option<i64> {
    match state {
        ToolState::Pending { .. } => None,
        ToolState::Running { time, .. } => Some(time.start),
        ToolState::Completed { time, .. } => Some(time.start),
        ToolState::Error { time, .. } => Some(time.start),
    }
}

/// Borrow a tool state's `metadata`, if present.
fn tool_state_metadata(state: &ToolState) -> Option<&Json> {
    match state {
        ToolState::Running { metadata, .. } | ToolState::Error { metadata, .. } => {
            metadata.as_ref()
        }
        ToolState::Completed { metadata, .. } => Some(metadata),
        ToolState::Pending { .. } => None,
    }
}

/// Borrow the `callID` of a [`PartKind::Tool`] part.
fn tool_call_id(part: &Part) -> &str {
    match &part.kind {
        PartKind::Tool { call_id, .. } => call_id,
        _ => "",
    }
}

/// Borrow the tool `name` of a [`PartKind::Tool`] part.
fn tool_name(part: &Part) -> &str {
    match &part.kind {
        PartKind::Tool { tool, .. } => tool,
        _ => "",
    }
}

/// Extract a model-facing error string from a JSON error payload.
fn json_error_message(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        Json::Object(map) => map
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string()),
        other => other.to_string(),
    }
}

/// Normalize a tool result into `(title, metadata, output)` — port of
/// `toolResultOutput` (`processor.ts:255-274`).
fn tool_result_output(name: &str, result: &ToolResultValue) -> (String, Json, String) {
    let value: Json = match result {
        ToolResultValue::Json { value }
        | ToolResultValue::Text { value }
        | ToolResultValue::Error { value } => value.clone(),
        ToolResultValue::Content { value } => Json::Array(value.clone()),
    };

    if let Some(obj) = value.as_object()
        && let Some(output) = obj.get("output").and_then(|v| v.as_str())
    {
        let title = obj
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| name.to_string());
        let metadata = obj
            .get("metadata")
            .filter(|m| m.is_object())
            .cloned()
            .unwrap_or_else(|| Json::Object(serde_json::Map::new()));
        return (title, metadata, output.to_string());
    }

    let metadata = if matches!(result, ToolResultValue::Json { .. }) && value.is_object() {
        value.clone()
    } else {
        Json::Object(serde_json::Map::new())
    };
    let output = match &value {
        Json::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    (name.to_string(), metadata, output)
}

/// Compute storage [`Tokens`] and cost from a provider [`Usage`] — port of
/// `Session.getUsage` (`session.ts:338-407`), minus the tiered-pricing and
/// copilot-nano-AIU special cases.
fn compute_usage(model: &Model, usage: &Usage) -> (Tokens, f64) {
    fn safe(v: f64) -> f64 {
        if v.is_finite() { v.max(0.0) } else { 0.0 }
    }
    let input = safe(usage.input_tokens.unwrap_or(0) as f64);
    let output = safe(usage.output_tokens.unwrap_or(0) as f64);
    let reasoning = safe(usage.reasoning_tokens.unwrap_or(0) as f64);
    let cache_read = safe(usage.cache_read_input_tokens.unwrap_or(0) as f64);
    let cache_write = safe(usage.cache_write_input_tokens.unwrap_or(0) as f64);
    let adjusted_input = safe(input - cache_read - cache_write);
    let total = usage.total_tokens.map(|t| t as f64);

    let tokens = Tokens {
        total,
        input: adjusted_input,
        output: safe(output - reasoning),
        reasoning,
        cache: TokenCache {
            read: cache_read,
            write: cache_write,
        },
    };

    let cost = model.cost.as_ref().map_or(0.0, |c| {
        let per = |x: Option<f64>| x.unwrap_or(0.0);
        safe(
            tokens.input * per(c.input) / 1_000_000.0
                + tokens.output * per(c.output) / 1_000_000.0
                + cache_read * per(c.cache_read) / 1_000_000.0
                + cache_write * per(c.cache_write) / 1_000_000.0
                + reasoning * per(c.output) / 1_000_000.0,
        )
    });

    (tokens, cost)
}
