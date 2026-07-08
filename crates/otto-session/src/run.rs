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
use otto_llm::message::{ContentPart, Message, SystemPart, ToolChoice, ToolDefinition};
use otto_llm::{LLMRequest, Model, Route};
use otto_storage::model::{
    Assistant, AssistantError, AssistantPath, AssistantTime, Info, InfoBody, Part, PartKind,
    TokenCache, Tokens, ToolState, new_message_id,
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

/// Assistant-role prompt injected on the final allowed step to force a
/// text-only wrap-up (`prompt.ts:1280`, text from `session/runner/max-steps.ts`).
const MAX_STEPS_PROMPT: &str = "CRITICAL - MAXIMUM STEPS REACHED\n\nThe maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.\n\nSTRICT REQUIREMENTS:\n1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)\n2. MUST provide a text response summarizing work done so far\n3. This constraint overrides ALL other instructions, including any user requests for edits or tool use\n\nResponse must include:\n- Statement that maximum steps for this agent have been reached\n- Summary of what has been accomplished so far\n- List of any remaining tasks that were not completed\n- Recommendations for what should be done next\n\nAny attempt to use tools is a critical violation. Respond with text ONLY.";

/// Hard cap on loop iterations — a guard against a misbehaving exit condition
/// spinning forever (no opencode analog; otto safety net).
const MAX_ITERATIONS: u32 = 1000;

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
    /// Cap on per-turn retries of a retryable provider failure
    /// ([`retry::with_retry`]). See [`DEFAULT_MAX_RETRIES`].
    pub max_retries: u32,
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
fn new_assistant(cfg: &RunConfig, session_id: &str, assistant_id: &str, parent_id: &str) -> Info {
    let dir = cfg.directory.display().to_string();
    Info {
        id: assistant_id.to_string(),
        session_id: session_id.to_string(),
        body: InfoBody::Assistant(Assistant {
            time: AssistantTime {
                created: now_ms(),
                completed: None,
            },
            error: None,
            parent_id: parent_id.to_string(),
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

/// Mark an interrupted assistant message finalized — port of
/// `finalizeInterruptedAssistant` (`prompt.ts:1203-1211`). The processor's
/// cleanup already stamps `time.completed`; this additionally records the abort
/// error if none is set.
async fn finalize_interrupted(
    store: &Store,
    session_id: &str,
    assistant_id: &str,
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

/// Resolve the message returned by [`run_loop`] — the newest assistant message,
/// else the newest message (`prompt.ts:1073-1079`, `lastAssistant`).
async fn last_assistant(store: &Store, session_id: &str) -> Result<Info, RunError> {
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
pub async fn run_loop(cfg: &RunConfig, session_id: &str) -> Result<Info, RunError> {
    let mut step: u32 = 0;
    let mut iterations: u32 = 0;

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
            let ctx = ctx_builder.build();
            let augmented =
                augment_with_tools(provider_stream, cfg.tools.clone(), ctx, model_id.0.clone());

            // Live event tap (`cfg.event_tx`): forward a clone of each event to
            // the consumer as it passes through, leaving the original in the
            // stream for the processor. A closed/absent receiver is ignored so
            // the run is never blocked or failed by a slow/gone consumer.
            let augmented = match &cfg.event_tx {
                Some(tx) => {
                    let tx = tx.clone();
                    augmented
                        .map(move |item| {
                            if let Ok(event) = &item {
                                let _ = tx.send(event.clone());
                            }
                            item
                        })
                        .boxed()
                }
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
                    if attempt >= cfg.max_retries || !retry::retryable(&err, &provider.0) {
                        return Err(ProcessorError::Llm(err).into());
                    }
                    let wait = retry::delay(attempt, err.retry_after());
                    if let Some(tx) = &cfg.event_tx {
                        let _ = tx.send(otto_events::LLMEvent::Retry {
                            attempt,
                            max: cfg.max_retries,
                            delay_ms: wait.as_millis() as u64,
                            message: err.to_string(),
                        });
                    }
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {}
                        // Abort during backoff → graceful interrupt, not a
                        // surfaced error (same rationale as the arm-top check).
                        () = cfg.abort.cancelled() => break ProcessOutcome::Stop,
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
