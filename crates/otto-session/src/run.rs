//! The agent `run_loop` — a Rust port of opencode `session/prompt.ts`
//! `runLoop` (`prompt.ts:1081-1340`).
//!
//! The loop drives one agent turn at a time: it re-reads the persisted history
//! every iteration, decides whether the turn is finished, and — if not — creates
//! a fresh assistant message, builds the provider request (system + converted
//! messages + tool defs), streams it through [`augment_with_tools`], and feeds
//! the augmented stream to a [`Processor`]. Persisted tool results re-enter the
//! conversation on the next iteration's fresh read, which is how a tool cycle
//! continues.
//!
//! Auto-compaction is wired in (the pre-check, the mid-stream `Compact`
//! outcome, and post-run pruning — see [`crate::compaction`]) along with
//! per-turn retry of transient provider failures ([`crate::retry`]). The
//! remaining subtask / summary / title seams of the opencode original are
//! deferred (Phase 5); their decision points are marked with `TODO`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use otto_hooks::{Decision, HookEvent, HookRunner};
use otto_llm::message::{ContentPart, Message, SystemPart, ToolChoice, ToolDefinition};
use otto_llm::{LLMRequest, Model, Route};
use otto_storage::model::{
    Assistant, AssistantError, AssistantPath, AssistantTime, Info, InfoBody, MessageId, Part,
    PartKind, SessionId, StartEndTime, TokenCache, Tokens, ToolState, User, UserModel, UserTime,
    new_message_id, new_part_id,
};
use otto_storage::{StorageError, Store, filter_compacted, latest};
use otto_tools::tool::PermissionGate;
use otto_tools::{Tool, ToolContext, ToolRegistry};
use tokio_util::sync::CancellationToken;

use crate::compaction::{self, CompactionError};
use crate::processor::{ProcessOutcome, Processor, ProcessorError};
use crate::runtime::augment_with_tools;
use crate::warm::WarmCache;
use crate::{ConvertOptions, build_system, overflow, retry, to_model_messages};

/// Default recent-token budget preserved verbatim past a compaction
/// (`compaction.ts` `preserve_recent_tokens`).
pub const DEFAULT_PRESERVE_RECENT_TOKENS: u64 = 20_000;
/// Default tokens reserved on top of the model's max output when computing the
/// overflow boundary (`COMPACTION_BUFFER`, `overflow.ts:8`).
pub const DEFAULT_COMPACTION_RESERVED: u64 = 20_000;
/// Default cap on per-turn retries of a retryable provider failure.
pub const DEFAULT_MAX_RETRIES: u32 = 5;
/// Default cap on retries across the *whole prompt* (all steps combined). The
/// per-step budget ([`DEFAULT_MAX_RETRIES`]) resets on every successful step,
/// so a long multi-step turn against a flaky provider could otherwise retry
/// indefinitely; this bounds the total.
pub const DEFAULT_MAX_TOTAL_RETRIES: u32 = 20;

/// Assistant-role prompt injected on the final allowed step to force a
/// text-only wrap-up (`prompt.ts:1280`, text from `session/runner/max-steps.ts`).
const MAX_STEPS_PROMPT: &str = "CRITICAL - MAXIMUM STEPS REACHED\n\nThe maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.\n\nSTRICT REQUIREMENTS:\n1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)\n2. MUST provide a text response summarizing work done so far\n3. This constraint overrides ALL other instructions, including any user requests for edits or tool use\n\nResponse must include:\n- Statement that maximum steps for this agent have been reached\n- Summary of what has been accomplished so far\n- List of any remaining tasks that were not completed\n- Recommendations for what should be done next\n\nAny attempt to use tools is a critical violation. Respond with text ONLY.";

/// Hard cap on loop iterations — a guard against a misbehaving exit condition
/// spinning forever (no opencode analog; otto safety net). Also bounds a
/// misbehaving always-deny `Stop` hook: each deny re-enters this same loop
/// via `continue`, so it counts against this cap too.
const MAX_ITERATIONS: u32 = 1000;

/// Default synthetic continuation prompt when a denying `Stop`/`SubagentStop`
/// hook supplies no `reason` (Claude-Code-inspired otto extension; no
/// opencode analog).
pub(crate) const DEFAULT_STOP_CONTINUE_PROMPT: &str = "Please continue.";

/// Configuration for one [`run_loop`] invocation — the ambient services and the
/// per-session model/agent settings that opencode threads through
/// `SessionPrompt.run` (`prompt.ts:1081-1340`).
#[derive(Clone)]
pub struct RunConfig {
    /// Persistence backend.
    pub store: Store,
    /// The LLM route (protocol + endpoint + transport) for this session's model.
    pub route: Arc<dyn Route>,
    /// The tool registry available to the agent.
    pub tools: Arc<ToolRegistry>,
    /// The permission gate threaded into tool execution.
    pub permission: Arc<dyn PermissionGate>,
    /// The model to generate with.
    pub model: Model,
    /// The agent name.
    pub agent: String,
    /// Optional agent system prompt (overrides the base prompt).
    pub agent_prompt: Option<String>,
    /// Working directory (resolves relative tool paths).
    pub directory: PathBuf,
    /// Maximum steps (`agent.steps ?? Infinity`, `prompt.ts:1178`).
    pub max_steps: Option<u32>,
    /// Cooperative cancellation for the whole run.
    pub abort: CancellationToken,
    /// The subagent-spawn seam injected into every tool-execution
    /// [`ToolContext`] so the `task` tool can spawn children. For a nested
    /// subagent the child [`RunConfig`] carries the same spawner `Arc` (with the
    /// child's derived ruleset), which is how recursion works. `None` disables
    /// the `task` tool (task.ts:84-90 has no analogue when the services are
    /// absent).
    pub subagent: Option<Arc<dyn otto_tools::SubagentSpawner>>,
    /// Recent-token budget kept verbatim past a compaction — the
    /// `preserve_recent_tokens` fed to [`compaction::select`]
    /// (`compaction.ts:80-85`). See [`DEFAULT_PRESERVE_RECENT_TOKENS`].
    pub preserve_recent_tokens: u64,
    /// Tokens reserved above the model's max output when computing the overflow
    /// boundary ([`overflow::is_overflow`], `overflow.ts:14-16`). See
    /// [`DEFAULT_COMPACTION_RESERVED`].
    pub compaction_reserved: u64,
    /// Whether the auto-compaction pre-check runs (`cfg.compaction.auto`,
    /// `overflow.ts:28`; `prompt.ts:1161`). Defaults to `true`.
    pub auto_compact: bool,
    /// Trailing tool-output token budget protected from post-turn pruning
    /// ([`compaction::prune`]). See [`compaction::PRUNE_PROTECT`] for the
    /// default; config knob `compaction.prune_protect_tokens`.
    pub prune_protect_tokens: u64,
    /// Cap on per-turn retries of a retryable provider failure
    /// ([`retry::with_retry`]). See [`DEFAULT_MAX_RETRIES`].
    pub max_retries: u32,
    /// Cap on retries summed across all steps of one prompt. The per-step
    /// `max_retries` budget resets after each successful step; this bounds the
    /// run as a whole. See [`DEFAULT_MAX_TOTAL_RETRIES`].
    pub max_total_retries: u32,
    /// Optional live event tap. When `Some`, a clone of every
    /// [`otto_events::LLMEvent`] flowing through the tool-augmented stream into
    /// the [`Processor`] is forwarded on this channel *without* disturbing the
    /// processor. `otto-app`'s `Runtime::run` uses this to surface the
    /// streaming turn to a CLI/server consumer. Defaults to `None` (no tap).
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<otto_events::LLMEvent>>,
    /// Optional warm-boot system-prompt cache (see [`crate::warm::WarmCache`]).
    /// `None` for main sessions (always rebuild); `Some` for child spawns,
    /// where [`SessionSubagentSpawner`](crate::subagent::SessionSubagentSpawner)
    /// memoizes the prompt per `(provider, model, agent, directory)`.
    pub system_cache: Option<Arc<WarmCache>>,
    /// Optional tersemode brevity directive, resolved from `config.tersemode` by
    /// `otto-app`. When `Some`, it is passed as `build_system`'s `user_system`
    /// arg so it is appended last in the system prompt. For subagents the same
    /// text is baked into the warm cache (see [`crate::warm::compute_warm`]), so
    /// this field is unused on the cached path.
    pub tersemode_directive: Option<String>,
    /// Optional external lifecycle-hooks runner (`UserPromptSubmit`,
    /// `PreCompact`, and — in a later plan — `Stop`). `None` disables all
    /// `otto-hooks` firing for this run. Distinct from `otto-tools`' own
    /// `PreToolUse`/`PostToolUse` wiring, though in production both point at
    /// the same shared `Arc<HookRunner>` (`otto-app::Runtime`).
    pub hooks: Option<Arc<HookRunner>>,
}

/// Errors raised by [`run_loop`].
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// A persistence failure.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// A processing failure while consuming a turn's event stream.
    #[error(transparent)]
    Processor(#[from] ProcessorError),
    /// A failure while summarizing / pruning during compaction.
    #[error(transparent)]
    Compaction(#[from] CompactionError),
    /// The session had no user message to reply to (`prompt.ts:1098`).
    #[error("no user message found in session {0}")]
    NoUserMessage(String),
    /// The session had no messages when resolving the last assistant.
    #[error("session {0} has no messages")]
    NoMessages(String),
    /// A `user_prompt_submit` lifecycle hook denied the turn before any
    /// provider call was made.
    #[error("{0}")]
    HookDenied(String),
    /// The loop exceeded [`MAX_ITERATIONS`] without terminating.
    #[error("run loop exceeded {0} iterations without terminating")]
    IterationCap(u32),
}

/// Current wall-clock time in milliseconds since the Unix epoch.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Wrap `stream` so every `Ok` event is forwarded to `tx` **the moment it
/// arrives from the provider**, independent of how fast the returned stream is
/// consumed.
///
/// The old `.map()` tap only fired when the [`Processor`] polled the next item
/// — and the processor awaits a store write per event, so client-visible
/// streaming advanced at SQLite-commit rate and a stalled write froze the SSE
/// mid-turn. Here a dedicated task drains the provider at wire speed into an
/// unbounded channel; the processor consumes from the channel at its own pace.
///
/// The pump stops (dropping the provider stream and cancelling its HTTP
/// request) as soon as the consumer side is dropped, and a closed/absent `tx`
/// receiver is ignored so the run is never blocked or failed by a slow/gone
/// consumer. Events are forwarded to `tx` only after they are queued for the
/// consumer, so the tap can never run ahead of what the processor will see.
pub fn tap_events(
    mut stream: futures::stream::BoxStream<
        'static,
        Result<otto_events::LLMEvent, otto_llm::LLMError>,
    >,
    tx: tokio::sync::mpsc::UnboundedSender<otto_events::LLMEvent>,
) -> futures::stream::BoxStream<'static, Result<otto_events::LLMEvent, otto_llm::LLMError>> {
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            let event = item.as_ref().ok().cloned();
            if out_tx.send(item).is_err() {
                break; // consumer gone — stop draining the provider
            }
            if let Some(event) = event {
                let _ = tx.send(event);
            }
        }
    });
    Box::pin(async_stream::stream! {
        while let Some(item) = out_rx.recv().await {
            yield item;
        }
    })
}

/// Whether a tool part represents live (unsatisfied) tool work — the port of
/// opencode's `hasToolCalls` predicate (`prompt.ts:1106-1109`): a `tool` part
/// that is not `providerExecuted` and not an orphaned interrupted tool.
fn is_live_tool_call(part: &Part) -> bool {
    let PartKind::Tool {
        metadata, state, ..
    } = &part.kind
    else {
        return false;
    };
    let provider_executed = metadata
        .as_ref()
        .and_then(|m| m.get("providerExecuted"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    if provider_executed {
        return false;
    }
    !is_orphaned_interrupted(state)
}

/// Whether a tool state is a cleanup-marked interrupted orphan — port of
/// `isOrphanedInterruptedTool` (`prompt.ts` helper): `status == error` with
/// `metadata.interrupted == true`.
fn is_orphaned_interrupted(state: &ToolState) -> bool {
    match state {
        ToolState::Error { metadata, .. } => {
            metadata
                .as_ref()
                .and_then(|m| m.get("interrupted"))
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        }
        _ => false,
    }
}

/// Build the ToolDefinition list for the request from the model-gated tools
/// (`prompt.ts:1226-1240`, `SessionTools.resolve`).
fn to_tool_def(tool: &Arc<dyn Tool>) -> ToolDefinition {
    ToolDefinition {
        name: tool.id().to_string(),
        description: Some(tool.description().to_string()),
        input_schema: tool.parameters_schema(),
        output_schema: None,
        cache: None,
    }
}

/// Construct the fresh assistant [`Info`] created at the top of each generating
/// iteration (`prompt.ts:1186-1200`).
fn new_assistant(
    cfg: &RunConfig,
    session_id: &SessionId,
    assistant_id: &MessageId,
    parent_id: &MessageId,
) -> Info {
    let dir = cfg.directory.display().to_string();
    Info {
        id: assistant_id.clone(),
        session_id: session_id.clone(),
        body: InfoBody::Assistant(Assistant {
            time: AssistantTime {
                created: now_ms(),
                completed: None,
            },
            error: None,
            parent_id: parent_id.clone(),
            model_id: cfg.model.id.0.clone(),
            provider_id: cfg.model.provider.0.clone(),
            mode: cfg.agent.clone(),
            agent: cfg.agent.clone(),
            path: AssistantPath {
                cwd: dir.clone(),
                root: dir,
            },
            summary: None,
            cost: 0.0,
            tokens: Tokens {
                total: None,
                input: 0.0,
                output: 0.0,
                reasoning: 0.0,
                cache: TokenCache {
                    read: 0.0,
                    write: 0.0,
                },
            },
            structured: None,
            variant: None,
            finish: None,
        }),
    }
}

/// Persist a synthetic user-role message that re-enters the loop as if the
/// user had sent `text` — the mechanism behind a denying `Stop`/
/// `SubagentStop` hook (`crate::subagent::SessionSubagentSpawner::spawn`'s
/// `SubagentStop` handling calls this too). Mirrors `compaction::create`'s
/// existing auto-continue-message injection (`compaction.rs`), including
/// marking the part `synthetic: Some(true)` the same way.
pub(crate) async fn synthesize_continuation(
    cfg: &RunConfig,
    session_id: &SessionId,
    text: &str,
) -> Result<(), StorageError> {
    let id = new_message_id();
    let msg = Info {
        id: id.clone(),
        session_id: session_id.clone(),
        body: InfoBody::User(User {
            time: UserTime { created: now_ms() },
            format: None,
            summary: None,
            agent: cfg.agent.clone(),
            model: UserModel {
                provider_id: cfg.model.provider.0.clone(),
                model_id: cfg.model.id.0.clone(),
                variant: None,
            },
            system: None,
            tools: None,
        }),
    };
    cfg.store.insert_message(&msg).await?;
    cfg.store
        .insert_part(&Part {
            id: new_part_id(),
            session_id: session_id.clone(),
            message_id: id,
            kind: PartKind::Text {
                text: text.to_string(),
                synthetic: Some(true),
                ignored: None,
                time: Some(StartEndTime {
                    start: now_ms(),
                    end: Some(now_ms()),
                }),
                metadata: None,
            },
        })
        .await?;
    Ok(())
}

/// Mark an interrupted assistant message finalized — port of
/// `finalizeInterruptedAssistant` (`prompt.ts:1203-1211`). The processor's
/// cleanup already stamps `time.completed`; this additionally records the abort
/// error if none is set.
async fn finalize_interrupted(
    store: &Store,
    session_id: &SessionId,
    assistant_id: &MessageId,
) -> Result<(), StorageError> {
    let Some(mut info) = store.get_message(session_id, assistant_id).await? else {
        return Ok(());
    };
    if let InfoBody::Assistant(a) = &mut info.body {
        if a.time.completed.is_none() {
            a.time.completed = Some(now_ms());
        }
        if a.error.is_none() {
            a.error = Some(AssistantError::MessageAbortedError {
                message: "Aborted".to_string(),
            });
        }
        store.update_message(&info).await?;
    }
    Ok(())
}

/// Salvage a failed attempt that already executed tools: keep its completed
/// tool parts (and streamed narration), drop only the incomplete
/// pending/running tool parts (they never executed — the model will re-issue
/// them), and finalize the assistant as a normal `tool-calls` step. The next
/// outer-loop iteration re-reads history, where `convert` lowers the completed
/// tool parts into tool results — so the retried request naturally contains
/// the finished work instead of re-running every read from scratch.
///
/// Returns `false` (caller falls back to the purge-and-replay retry) when the
/// attempt completed no tool work. Provider-executed tools are not counted:
/// their results live provider-side and a replay is required anyway.
async fn salvage_completed_tools(
    store: &Store,
    session_id: &SessionId,
    assistant_id: &MessageId,
) -> Result<bool, StorageError> {
    let parts = store.list_parts(assistant_id).await?;
    let mut completed = 0usize;
    let mut incomplete: Vec<otto_storage::model::PartId> = Vec::new();
    for part in &parts {
        let PartKind::Tool {
            metadata, state, ..
        } = &part.kind
        else {
            continue;
        };
        let provider_executed = metadata
            .as_ref()
            .and_then(|m| m.get("providerExecuted"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        match state {
            ToolState::Completed { .. } if !provider_executed => completed += 1,
            ToolState::Pending { .. } | ToolState::Running { .. } => {
                incomplete.push(part.id.clone());
            }
            _ => {}
        }
    }
    if completed == 0 {
        return Ok(false);
    }
    for id in incomplete {
        store.delete_part(&id).await?;
    }
    let Some(mut info) = store.get_message(session_id, assistant_id).await? else {
        return Ok(false);
    };
    if let InfoBody::Assistant(a) = &mut info.body {
        if a.time.completed.is_none() {
            a.time.completed = Some(now_ms());
        }
        a.finish = Some("tool-calls".to_string());
        a.error = None;
        store.update_message(&info).await?;
    }
    Ok(true)
}

/// Accept a truncated assistant response (stream ended without a terminal
/// finish on every attempt): stamp `time.completed` and a `finish = "unknown"`
/// so the exit condition sees a terminal turn. The streamed parts are kept.
async fn finalize_truncated(
    store: &Store,
    session_id: &SessionId,
    assistant_id: &MessageId,
) -> Result<(), StorageError> {
    let Some(mut info) = store.get_message(session_id, assistant_id).await? else {
        return Ok(());
    };
    if let InfoBody::Assistant(a) = &mut info.body {
        if a.time.completed.is_none() {
            a.time.completed = Some(now_ms());
        }
        if a.finish.is_none() {
            a.finish = Some("unknown".to_string());
        }
        store.update_message(&info).await?;
    }
    Ok(())
}

/// Mark an assistant message failed after retry exhaustion (or a non-retryable
/// provider error): stamp `time.completed` and record the provider error so
/// the turn never leaves an unfinalized message behind. Companion to
/// [`finalize_interrupted`], which handles the abort path.
async fn finalize_failed(
    store: &Store,
    session_id: &SessionId,
    assistant_id: &MessageId,
    err: &otto_llm::LLMError,
) -> Result<(), StorageError> {
    let Some(mut info) = store.get_message(session_id, assistant_id).await? else {
        return Ok(());
    };
    if let InfoBody::Assistant(a) = &mut info.body {
        if a.time.completed.is_none() {
            a.time.completed = Some(now_ms());
        }
        if a.error.is_none() {
            a.error = Some(AssistantError::UnknownError {
                message: err.to_string(),
                r#ref: None,
            });
        }
        store.update_message(&info).await?;
    }
    Ok(())
}

/// Resolve the message returned by [`run_loop`] — the newest assistant message,
/// else the newest message (`prompt.ts:1073-1079`, `lastAssistant`).
async fn last_assistant(store: &Store, session_id: &SessionId) -> Result<Info, RunError> {
    let msgs = store.list_messages(session_id).await?;
    if let Some(assistant) = msgs.iter().rev().find(|m| m.is_assistant()) {
        return Ok(assistant.clone());
    }
    msgs.into_iter()
        .next_back()
        .ok_or_else(|| RunError::NoMessages(session_id.to_string()))
}

/// Run the agent loop for `session_id` until the turn is finished, returning the
/// last assistant [`Info`]. Port of `SessionPrompt.run` (`prompt.ts:1081-1340`).
///
/// Each iteration:
/// 1. re-reads and [`filter_compacted`]s the history (persisted tool results
///    re-enter here — `prompt.ts:1092-1094`);
/// 2. computes [`latest`] and evaluates the exit condition
///    (`prompt.ts:1111-1130`);
/// 3. creates + persists a fresh assistant message (`prompt.ts:1186-1201`);
/// 4. builds the system prompt, converts the history to provider messages, and
///    appends the max-steps prompt on the final step (`prompt.ts:1256-1281`);
/// 5. streams the request through [`augment_with_tools`] into a [`Processor`]
///    (`prompt.ts:1271-1285`), then interprets the [`ProcessOutcome`]
///    (`prompt.ts:1318-1334`).
///
/// # Errors
/// Returns [`RunError`] on a storage/processor failure, a missing user message,
/// or if the iteration cap is exceeded.
pub async fn run_loop(cfg: &RunConfig, session_id: &SessionId) -> Result<Info, RunError> {
    let mut step: u32 = 0;
    let mut iterations: u32 = 0;
    // Retries summed across every step of this prompt. The per-step `attempt`
    // counter resets each step; without this a flaky provider on a long
    // multi-step turn gets a fresh 5-attempt budget every step, forever.
    let mut total_retries: u32 = 0;

    // Session-level hook context, spliced into every turn's system prompt at
    // the same slot `mcp_instructions` uses (`build_system`). Two sources,
    // newline-joined when both are present:
    // 1. SessionStart's `additional_context`, persisted once onto the
    //    session's metadata at creation time
    //    (`otto-app::Runtime::create_session`).
    // 2. UserPromptSubmit's `additional_context`, fired exactly once here —
    //    a `run_loop` call is always exactly one user submission; the loop
    //    below re-reads history for tool-continuation cycles of the SAME
    //    prompt, not new submissions, so firing inside the loop would
    //    misfire on every such cycle.
    let mut hook_context: Option<String> = None;
    if let Some(runner) = &cfg.hooks {
        hook_context = cfg
            .store
            .get_session(session_id)
            .await?
            .and_then(|s| s.metadata)
            .and_then(|m| m.get("hookContext").and_then(|v| v.as_str().map(str::to_string)));

        let msgs = filter_compacted(cfg.store.messages_with_parts(session_id).await?);
        let last_user = latest(&msgs)
            .user
            .ok_or_else(|| RunError::NoUserMessage(session_id.to_string()))?;
        let prompt_text: String = msgs
            .iter()
            .rev()
            .find(|m| m.info.is_user() && m.info.id == last_user.id)
            .map(|m| {
                m.parts
                    .iter()
                    .filter_map(|p| match &p.kind {
                        PartKind::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();

        let verdict = runner
            .fire(HookEvent::UserPromptSubmit {
                session_id: session_id.clone(),
                prompt: prompt_text,
                cwd: cfg.directory.clone(),
            })
            .await;
        match verdict.decision {
            Decision::Allow => {}
            Decision::Deny => {
                return Err(RunError::HookDenied(verdict.reason.clone().unwrap_or_else(|| {
                    "blocked by user_prompt_submit hook".to_string()
                })));
            }
            Decision::Ask => {
                let req = otto_tools::build_hook_permission_request(
                    "user_prompt_submit",
                    &verdict,
                    None,
                );
                let result = cfg.permission.ask(req).await;
                let outcome = otto_tools::interpret_hook_ask_result(result, &verdict);
                if !outcome.approved {
                    return Err(RunError::HookDenied(outcome.message.unwrap_or_else(|| {
                        "blocked by user_prompt_submit hook".to_string()
                    })));
                }
            }
        }
        if let Some(ctx) = verdict.additional_context {
            hook_context = Some(match hook_context.take() {
                Some(existing) => format!("{existing}\n{ctx}"),
                None => ctx,
            });
        }
    }

    loop {
        iterations += 1;
        if iterations > MAX_ITERATIONS {
            return Err(RunError::IterationCap(MAX_ITERATIONS));
        }

        // 1. Fresh read every iteration (`prompt.ts:1092-1094`).
        let msgs = filter_compacted(cfg.store.messages_with_parts(session_id).await?);
        let snapshot = latest(&msgs);

        let last_user = snapshot
            .user
            .as_ref()
            .ok_or_else(|| RunError::NoUserMessage(session_id.to_string()))?;

        // The latest assistant message + parts (`prompt.ts:1100-1102`).
        let last_assistant_msg = snapshot.assistant.as_ref().and_then(|la| {
            msgs.iter()
                .rev()
                .find(|m| m.info.is_assistant() && m.info.id == la.id)
        });

        // Providers may return "stop" with tool calls present; keep looping so
        // tool results flow back (`prompt.ts:1106-1109`).
        let has_tool_calls = last_assistant_msg
            .map(|m| m.parts.iter().any(is_live_tool_call))
            .unwrap_or(false);

        // 3. Exit condition (`prompt.ts:1111-1130`).
        if let Some(la) = snapshot.assistant.as_ref() {
            let finish = la.as_assistant().and_then(|a| a.finish.as_deref());
            let finished_terminal = finish.is_some_and(|f| f != "tool-calls");
            if finished_terminal && !has_tool_calls && last_user.id < la.id {
                // Stop: informational unless the verdict denies. A deny
                // synthesizes a follow-up user turn and re-enters the loop
                // instead of ending the turn — bounded by MAX_ITERATIONS
                // above, since this `continue` re-enters the same loop.
                if let Some(runner) = &cfg.hooks {
                    let verdict = runner
                        .fire(HookEvent::Stop {
                            session_id: session_id.clone(),
                        })
                        .await;
                    match verdict.decision {
                        Decision::Allow => {}
                        Decision::Deny => {
                            let text = verdict
                                .reason
                                .unwrap_or_else(|| DEFAULT_STOP_CONTINUE_PROMPT.to_string());
                            synthesize_continuation(cfg, session_id, &text).await?;
                            continue;
                        }
                        Decision::Ask => {
                            let req =
                                otto_tools::build_hook_permission_request("stop", &verdict, None);
                            let result = cfg.permission.ask(req).await;
                            let outcome = otto_tools::interpret_hook_ask_result(result, &verdict);
                            if !outcome.approved {
                                let text = outcome
                                    .message
                                    .unwrap_or_else(|| DEFAULT_STOP_CONTINUE_PROMPT.to_string());
                                synthesize_continuation(cfg, session_id, &text).await?;
                                continue;
                            }
                        }
                    }
                }
                break;
            }
        }

        // Auto-compaction pre-check (`prompt.ts:1160-1168`): if the last
        // finished assistant's recorded tokens have reached the usable context
        // slice, summarize before generating and re-read the compacted history
        // on the next iteration. The summary assistant (`summary == true`) is
        // itself excluded so this cannot loop.
        if cfg.auto_compact
            && let Some(finished) = snapshot.finished.as_ref().and_then(Info::as_assistant)
            && finished.summary != Some(true)
            && overflow::is_overflow(&finished.tokens, &cfg.model, cfg.compaction_reserved)
        {
            compaction::create(cfg, session_id, true, true).await?;
            continue;
        }

        // 4. Step accounting (`prompt.ts:1132`, `1178-1179`).
        step += 1;
        let is_last_step = cfg.max_steps.is_some_and(|max| step >= max);

        // TODO(Phase 5): pop `snapshot.tasks` for subtask handling
        // (`prompt.ts:1142-1159`).

        // 5. Create + persist a fresh assistant message (`prompt.ts:1186-1201`).
        let assistant_id = new_message_id();
        let parent_id = last_user.id.clone();
        let info = new_assistant(cfg, session_id, &assistant_id, &parent_id);
        cfg.store.insert_message(&info).await?;

        // 6. System + converted messages (`prompt.ts:1256-1281`).
        let provider = &cfg.model.provider;
        let model_id = &cfg.model.id;
        let is_git = cfg.directory.join(".git").exists();
        let platform = std::env::consts::OS;
        // TODO(Phase 5): thread a real date/user-system prompt.
        // TODO(Phase 6+): thread the pre-built `<mcp_instructions>` block
        // (`otto_mcp::McpClient::instructions`) through `RunConfig` once the
        // CLI/server owns the `McpClient`; `None` here means no MCP servers.
        let system = build_system(
            provider,
            model_id,
            cfg.agent_prompt.as_deref(),
            &cfg.directory,
            is_git,
            platform,
            "",
            None,
            hook_context.as_deref(),
            cfg.tersemode_directive.as_deref(),
            cfg.system_cache.as_deref(),
        );

        let mut messages = to_model_messages(&msgs, provider, model_id, &ConvertOptions::default());
        if is_last_step {
            messages.push(Message::assistant(vec![ContentPart::text(
                MAX_STEPS_PROMPT,
            )]));
        }

        // 7. Tool defs + request (`prompt.ts:1271-1285`).
        let tool_defs: Vec<ToolDefinition> = cfg
            .tools
            .tools_for_model(&model_id.0)
            .iter()
            .map(to_tool_def)
            .collect();

        let mut request = LLMRequest::new(cfg.model.clone(), messages);
        request.system = system.into_iter().map(SystemPart::new).collect();
        request.tools = tool_defs;
        request.tool_choice = Some(ToolChoice::Auto);

        // 8-9. Provider stream + tool augmentation + processing, wrapped in a
        // retry loop (`prompt.ts:1271-1285`). A retryable provider [`LLMError`]
        // (rate limit / 5xx / transient) reruns the turn with the
        // [`retry::delay`] backoff up to `cfg.max_retries` total attempts; a
        // non-retryable error propagates. Each attempt rebuilds the stream, the
        // tool context, and the [`Processor`] since all are single-use. The
        // abort token cancels the backoff wait.
        let mut attempt: u32 = 0;
        let outcome = loop {
            // A retry reuses the same `assistant_id`; purge any partial parts a
            // failed attempt persisted so a fresh attempt cannot duplicate them
            // (Fix 5 — idempotency). The first pass (attempt 0) has nothing to
            // purge.
            if attempt > 0 {
                cfg.store.delete_parts_for_message(&assistant_id).await?;
            }
            let provider_stream = cfg.route.stream(request.clone());
            let mut ctx_builder = ToolContext::builder(cfg.directory.clone())
                .session_id(session_id)
                .message_id(&assistant_id)
                .agent(&cfg.agent)
                .abort(cfg.abort.clone())
                .permission(cfg.permission.clone());
            // Inject the spawner so the `task` tool can drive a child loop; a
            // nested subagent re-enters here via the child `RunConfig.subagent`.
            if let Some(spawner) = &cfg.subagent {
                ctx_builder = ctx_builder.subagent(spawner.clone());
            }
            // Hand the live tap to tool execution so the `task` tool can
            // forward its child run's tool activity (filtered) to the client.
            if let Some(tx) = &cfg.event_tx {
                ctx_builder = ctx_builder.event_tx(tx.clone());
            }
            let ctx = ctx_builder.build();
            let augmented =
                augment_with_tools(provider_stream, cfg.tools.clone(), ctx, model_id.0.clone());

            // Live event tap (`cfg.event_tx`): a dedicated pump forwards each
            // event to the consumer as it arrives from the provider, decoupled
            // from the processor's per-event persistence awaits (see
            // [`tap_events`]).
            let augmented = match &cfg.event_tx {
                Some(tx) => tap_events(augmented, tx.clone()),
                None => augmented,
            };

            let mut processor = Processor::new(
                cfg.store.clone(),
                session_id,
                &assistant_id,
                cfg.model.clone(),
                &cfg.agent,
                cfg.permission.clone(),
            );
            match processor.process(augmented).await {
                Ok(outcome) => break outcome,
                Err(ProcessorError::Llm(err)) => {
                    // An aborted turn also ends its stream without a terminal
                    // finish, so Fix 4 surfaces `NoTerminalFinish` here. Handle
                    // abort FIRST by breaking gracefully so the post-loop abort
                    // check runs `finalize_interrupted` (the graceful interrupt
                    // path) instead of surfacing the error. `outcome` is unused
                    // because that abort check fires before it is read.
                    if cfg.abort.is_cancelled() {
                        break ProcessOutcome::Stop;
                    }
                    attempt += 1;
                    total_retries += 1;
                    let exhausted =
                        attempt >= cfg.max_retries || total_retries >= cfg.max_total_retries;
                    if exhausted || !retry::retryable(&err, &provider.0) {
                        // A provider that chronically omits `finish_reason`
                        // (some OpenAI-compatible gateways) truncates every
                        // attempt the same way. Once the budget is spent,
                        // accept the streamed content with a warning instead
                        // of failing the turn — the parts are intact because
                        // the purge only runs at the top of the NEXT attempt.
                        if exhausted && matches!(err, otto_llm::LLMError::NoTerminalFinish) {
                            tracing::warn!(
                                session = session_id.as_str(),
                                message = assistant_id.as_str(),
                                attempts = attempt,
                                "retry budget exhausted on truncated stream; accepting response as-is"
                            );
                            finalize_truncated(&cfg.store, session_id, &assistant_id).await?;
                            if let Some(tx) = &cfg.event_tx {
                                let _ = tx.send(otto_events::LLMEvent::Warning {
                                    message: format!(
                                        "provider stream ended without finish_reason on all {attempt} attempts; accepting the response as-is"
                                    ),
                                });
                            }
                            break ProcessOutcome::Continue;
                        }
                        // Stamp the failure on the assistant message before
                        // propagating so the turn never leaves an unfinalized
                        // message behind (mirrors `finalize_interrupted`).
                        tracing::error!(
                            session = session_id.as_str(),
                            message = assistant_id.as_str(),
                            attempt,
                            total_retries,
                            exhausted,
                            error = %err,
                            "turn failed: retries exhausted or error not retryable"
                        );
                        finalize_failed(&cfg.store, session_id, &assistant_id, &err).await?;
                        return Err(ProcessorError::Llm(err).into());
                    }
                    // Salvage before retrying: when the failed attempt already
                    // executed tools, keep that work as a finished tool-call
                    // step and continue the outer loop from it — instead of
                    // the purge-and-replay path, which forgets the executed
                    // tools and makes the model re-run the same reads and
                    // re-narrate on every retry.
                    let salvaged =
                        salvage_completed_tools(&cfg.store, session_id, &assistant_id).await?;
                    let wait = retry::delay(attempt, err.retry_after());
                    tracing::warn!(
                        session = session_id.as_str(),
                        message = assistant_id.as_str(),
                        attempt,
                        max = cfg.max_retries,
                        total_retries,
                        delay_ms = wait.as_millis() as u64,
                        salvaged,
                        error = %err,
                        "retryable provider failure; backing off"
                    );
                    if let Some(tx) = &cfg.event_tx {
                        let message = if salvaged {
                            format!("{err} — resuming with completed tool calls kept")
                        } else {
                            err.to_string()
                        };
                        let _ = tx.send(otto_events::LLMEvent::Retry {
                            attempt,
                            max: cfg.max_retries,
                            delay_ms: wait.as_millis() as u64,
                            message,
                            salvaged,
                        });
                    }
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {}
                        // Abort during backoff → graceful interrupt, not a
                        // surfaced error (same rationale as the arm-top check).
                        () = cfg.abort.cancelled() => break ProcessOutcome::Stop,
                    }
                    if salvaged {
                        // The step is finalized as `tool-calls`; the outer loop
                        // re-reads history and continues from the kept work.
                        break ProcessOutcome::Continue;
                    }
                }
                Err(other) => return Err(other.into()),
            }
        };

        // Interrupted mid-flight → finalize and stop (`prompt.ts:1331`).
        if cfg.abort.is_cancelled() {
            finalize_interrupted(&cfg.store, session_id, &assistant_id).await?;
            break;
        }

        // 10. Interpret the outcome (`prompt.ts:1318-1334`).
        match outcome {
            ProcessOutcome::Stop => break,
            // Context overflowed mid-stream: summarize and continue so the loop
            // re-reads the compacted history (`prompt.ts:1319-1327`). `overflow`
            // is `true` when the stream never reached a terminal finish.
            ProcessOutcome::Compact => {
                let finished = cfg
                    .store
                    .get_message(session_id, &assistant_id)
                    .await?
                    .and_then(|info| info.as_assistant().and_then(|a| a.finish.clone()))
                    .is_some();
                compaction::create(cfg, session_id, true, !finished).await?;
                continue;
            }
            ProcessOutcome::Continue => continue,
        }
    }

    // Reclaim context by erasing old tool outputs (`prompt.ts:1337`). Best
    // effort — a prune failure must not fail an otherwise-successful run.
    let _ = compaction::prune(cfg, session_id).await;
    last_assistant(&cfg.store, session_id).await
}
