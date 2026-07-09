//! Auto-compaction — a Rust port of opencode `session/compaction.ts`.
//!
//! When the conversation approaches the model's context limit the older turns
//! are replaced with a single generated summary while a recent tail is kept
//! verbatim. This module ports three seams of the opencode original:
//!
//! * [`select`] — the tail-selection budget walk (`compaction.ts:188-239`,
//!   `splitTurn` at `:105-128`): choose which trailing turns to keep and the
//!   `tail_start_id` boundary where the retained tail begins.
//! * [`create`] — `processCompaction` (`compaction.ts:289-511`) fused with
//!   `create` (`:513-536`): summarize the pre-tail history with the hidden
//!   `compaction` agent and persist an assistant `summary` message plus a
//!   `compaction` part carrying the boundary, so [`filter_compacted`] reorders
//!   the history on the next loop read.
//! * [`prune`] — the tool-output eraser (`compaction.ts:243-287`): mark old,
//!   completed tool outputs compacted to free context, protecting the last two
//!   turns and `skill` outputs.
//!
//! [`filter_compacted`]: otto_storage::filter_compacted

use std::time::{SystemTime, UNIX_EPOCH};

use otto_agent::builtins::PROMPT_COMPACTION;
use otto_llm::message::{ContentPart, Message, SystemPart};
use otto_llm::model::{ModelId, ProviderId};
use otto_llm::{LLMClient, LLMError, LLMRequest, LLMResponse};
use otto_storage::StorageError;
use otto_storage::model::{
    Assistant, AssistantPath, AssistantTime, Info, InfoBody, MessageId, Part, PartKind,
    StartEndTime, TokenCache, Tokens, ToolState, User, UserModel, UserTime, new_message_id,
    new_part_id,
};

use crate::convert::{ConvertOptions, to_model_messages};
use crate::run::RunConfig;

/// Free at least this many tokens before pruning old tool outputs
/// (`PRUNE_MINIMUM`, `compaction.ts:28`).
pub const PRUNE_MINIMUM: u64 = 20_000;
/// Always keep this many trailing tool-output tokens uncompacted
/// (`PRUNE_PROTECT`, `compaction.ts:29`).
pub const PRUNE_PROTECT: u64 = 40_000;
/// Tools whose outputs are never pruned (`PRUNE_PROTECTED_TOOLS`,
/// `compaction.ts:31`).
const PRUNE_PROTECTED_TOOLS: &[&str] = &["skill"];
/// Cap on distinct file paths whose newest `read` output is exempt from
/// pruning — keeps long sessions from re-reading the same files after every
/// prune, without letting a 400-file sweep hold its whole history.
const PRUNE_READ_PATH_EXEMPT_MAX: usize = 30;
/// How many trailing turns [`select`] considers keeping (`DEFAULT_TAIL_TURNS`,
/// `compaction.ts:32`).
const DEFAULT_TAIL_TURNS: usize = 2;
/// Tool output truncation applied while summarizing (`TOOL_OUTPUT_MAX_CHARS`,
/// `compaction.ts:30`).
const TOOL_OUTPUT_MAX_CHARS: usize = 2_000;

/// The Markdown template appended as the compaction user prompt — the otto
/// analog of `buildPrompt` / `SUMMARY_TEMPLATE` (opencode
/// `core/session/compaction.ts`).
const SUMMARY_TEMPLATE: &str = "Output exactly the Markdown structure shown inside <template> and keep the section order unchanged. Do not include the <template> tags in your response.
<template>
## Goal
- [single-sentence task summary]

## Constraints & Preferences
- [user constraints, preferences, specs, or \"(none)\"]

## Progress
### Done
- [completed work or \"(none)\"]

### In Progress
- [current work or \"(none)\"]

### Blocked
- [blockers or \"(none)\"]

## Key Decisions
- [decision and why, or \"(none)\"]

## Next Steps
- [ordered next actions or \"(none)\"]

## Critical Context
- [important technical facts, errors, open questions, or \"(none)\"]

## Relevant Files
- [file or directory path: why it matters, or \"(none)\"]
</template>

Rules:
- Keep every section, even when empty.
- Use terse bullets, not prose paragraphs.
- Preserve exact file paths, commands, error strings, and identifiers when known.
- Do not mention the summary process or that context was compacted.";

/// The auto-continue prompt injected after a compaction so the loop proceeds
/// (`compaction.ts:485`).
const CONTINUE_PROMPT: &str = "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed.";
/// Extra preface for an overflow-triggered compaction (`compaction.ts:483`).
const OVERFLOW_PREFACE: &str = "The previous request exceeded the provider's size limit due to large attachments. The conversation was compacted and media files were removed from context.\n\n";

/// Errors raised by [`create`] / [`prune`].
#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    /// A persistence failure.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// The summarization LLM call failed.
    #[error(transparent)]
    Llm(#[from] LLMError),
}

/// Current wall-clock time in milliseconds since the Unix epoch.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Rough token estimate for a serialized string — the otto analog of
/// `Token.estimate` (`util/token.ts`): ~4 characters per token.
fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// A turn — a user message and everything up to the next user message
/// (`compaction.ts:35-39`, `turns` at `:87-103`).
struct Turn {
    start: usize,
    end: usize,
    id: MessageId,
}

/// A retained tail boundary (`compaction.ts:41-44`).
struct Tail {
    start: usize,
    id: MessageId,
}

/// The result of [`select`]: how many leading messages form the summarizable
/// head, and the id of the message where the retained tail begins (`None` when
/// nothing is dropped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectResult {
    /// Number of leading messages to summarize (`msgs[..head_len]`).
    pub head_len: usize,
    /// Id of the first retained tail message, or `None` to keep everything.
    pub tail_start_id: Option<MessageId>,
}

/// Estimate the token size of `msgs[range]` once lowered to provider messages.
fn estimate(
    msgs: &[otto_storage::model::WithParts],
    provider: &ProviderId,
    model: &ModelId,
) -> u64 {
    let converted = to_model_messages(msgs, provider, model, &ConvertOptions::default());
    let json = serde_json::to_string(&converted).unwrap_or_default();
    estimate_tokens(&json)
}

/// Split the message list into [`Turn`]s (`compaction.ts:87-103`). A user
/// message that carries a compaction part does not start a turn.
fn turns(msgs: &[otto_storage::model::WithParts]) -> Vec<Turn> {
    let mut result: Vec<Turn> = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        if !m.info.is_user() {
            continue;
        }
        if m.parts.iter().any(Part::is_compaction) {
            continue;
        }
        result.push(Turn {
            start: i,
            end: msgs.len(),
            id: m.info.id.clone(),
        });
    }
    let n = result.len();
    for i in 0..n.saturating_sub(1) {
        result[i].end = result[i + 1].start;
    }
    result
}

/// Find the earliest clean split inside `turn` whose suffix fits `budget` —
/// port of `splitTurn` (`compaction.ts:105-128`).
fn split_turn(
    msgs: &[otto_storage::model::WithParts],
    turn: &Turn,
    budget: u64,
    provider: &ProviderId,
    model: &ModelId,
) -> Option<Tail> {
    if budget == 0 {
        return None;
    }
    if turn.end.saturating_sub(turn.start) <= 1 {
        return None;
    }
    for start in (turn.start + 1)..turn.end {
        let size = estimate(&msgs[start..turn.end], provider, model);
        if size > budget {
            continue;
        }
        return Some(Tail {
            start,
            id: msgs[start].info.id.clone(),
        });
    }
    None
}

/// Choose the trailing turns to keep within `preserve_recent_tokens` — port of
/// `select` (`compaction.ts:188-239`).
///
/// Walks the last [`DEFAULT_TAIL_TURNS`] turns from newest to oldest,
/// accumulating whole turns while they fit the budget and, on the first turn
/// that does not fit, attempting a [`split_turn`] within the remaining budget.
/// Returns the message index where the retained tail begins (as
/// [`SelectResult::head_len`]) and its message id; when the whole history fits
/// (or nothing can be dropped) the head is the entire list and
/// `tail_start_id` is `None`.
#[must_use]
pub fn select(
    msgs: &[otto_storage::model::WithParts],
    preserve_recent_tokens: u64,
    provider: &ProviderId,
    model: &ModelId,
) -> SelectResult {
    let budget = preserve_recent_tokens;
    let all = turns(msgs);
    if all.is_empty() {
        return SelectResult {
            head_len: msgs.len(),
            tail_start_id: None,
        };
    }
    let recent = &all[all.len().saturating_sub(DEFAULT_TAIL_TURNS)..];
    let sizes: Vec<u64> = recent
        .iter()
        .map(|t| estimate(&msgs[t.start..t.end], provider, model))
        .collect();

    let mut total: u64 = 0;
    let mut keep: Option<Tail> = None;
    for i in (0..recent.len()).rev() {
        let turn = &recent[i];
        let size = sizes[i];
        if total + size <= budget {
            total += size;
            keep = Some(Tail {
                start: turn.start,
                id: turn.id.clone(),
            });
            continue;
        }
        let remaining = budget.saturating_sub(total);
        if let Some(split) = split_turn(msgs, turn, remaining, provider, model) {
            keep = Some(split);
        }
        break;
    }

    match keep {
        Some(k) if k.start != 0 => SelectResult {
            head_len: k.start,
            tail_start_id: Some(k.id),
        },
        _ => SelectResult {
            head_len: msgs.len(),
            tail_start_id: None,
        },
    }
}

/// Concatenate the assembled assistant text from a summarization response.
fn summary_text(resp: &LLMResponse) -> String {
    resp.message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Summarize the current session history and persist the compaction record —
/// port of `processCompaction` + `create` (`compaction.ts:289-536`).
///
/// The flow:
/// 1. [`select`] the retained tail over the current history.
/// 2. Build the summarization request: system = the hidden `compaction` agent
///    prompt ([`PROMPT_COMPACTION`]), messages = the pre-tail head lowered via
///    [`to_model_messages`] plus the [`SUMMARY_TEMPLATE`] user prompt.
/// 3. Non-streaming [`LLMClient::generate`] over `cfg.route` to produce the
///    summary text.
/// 4. Persist a user message carrying a [`PartKind::Compaction`] part
///    `{ auto, overflow, tail_start_id }`, then an assistant message with
///    `summary = true` and a [`PartKind::Text`] summary part parented to it.
/// 5. When `auto`, inject a synthetic "continue" user message so the loop
///    proceeds on the next iteration.
///
/// The message/part shapes match opencode's so [`filter_compacted`] reorders
/// the history into `[compaction, summary, tail…, continue]` on the next read.
///
/// # Errors
/// Returns [`CompactionError`] on a persistence failure or a summarization LLM
/// failure.
///
/// [`filter_compacted`]: otto_storage::filter_compacted
pub async fn create(
    cfg: &RunConfig,
    session_id: &str,
    auto: bool,
    overflow: bool,
) -> Result<(), CompactionError> {
    let provider = &cfg.model.provider;
    let model_id = &cfg.model.id;

    // 1. Select the retained tail over the current history.
    let history = cfg.store.messages_with_parts(session_id).await?;
    let selected = select(&history, cfg.preserve_recent_tokens, provider, model_id);
    let head = &history[..selected.head_len];

    // 2. Build the summarization request (strip media + truncate tool output,
    //    `compaction.ts:349-354`).
    let mut messages = to_model_messages(
        head,
        provider,
        model_id,
        &ConvertOptions {
            strip_media: true,
            tool_output_max_chars: Some(TOOL_OUTPUT_MAX_CHARS),
        },
    );
    messages.push(Message::user(vec![ContentPart::text(SUMMARY_TEMPLATE)]));
    let mut request = LLMRequest::new(cfg.model.clone(), messages);
    request.system = vec![SystemPart::new(PROMPT_COMPACTION)];

    // 3. Generate the summary (non-streaming).
    let client = LLMClient::new(cfg.route.clone());
    let response = client.generate(request).await?;
    let summary = summary_text(&response);

    let dir = cfg.directory.display().to_string();

    // 4a. Persist the compaction user message (`compaction.ts:513-536`).
    let compaction_id = new_message_id();
    let compaction_msg = Info {
        id: compaction_id.clone(),
        session_id: session_id.to_string(),
        body: InfoBody::User(User {
            time: UserTime { created: now_ms() },
            format: None,
            summary: None,
            agent: cfg.agent.clone(),
            model: UserModel {
                provider_id: provider.0.clone(),
                model_id: model_id.0.clone(),
                variant: None,
            },
            system: None,
            tools: None,
        }),
    };
    cfg.store.insert_message(&compaction_msg).await?;
    cfg.store
        .insert_part(&Part {
            id: new_part_id(),
            session_id: session_id.to_string(),
            message_id: compaction_id.clone(),
            kind: PartKind::Compaction {
                auto,
                overflow: Some(overflow),
                tail_start_id: selected.tail_start_id.clone(),
            },
        })
        .await?;

    // 4b. Persist the assistant summary message (`compaction.ts:356-382`).
    let summary_id = new_message_id();
    let now = now_ms();
    let summary_msg = Info {
        id: summary_id.clone(),
        session_id: session_id.to_string(),
        body: InfoBody::Assistant(Assistant {
            time: AssistantTime {
                created: now,
                completed: Some(now),
            },
            error: None,
            parent_id: compaction_id.clone(),
            model_id: model_id.0.clone(),
            provider_id: provider.0.clone(),
            mode: "compaction".to_string(),
            agent: "compaction".to_string(),
            path: AssistantPath {
                cwd: dir.clone(),
                root: dir,
            },
            summary: Some(true),
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
            finish: Some("stop".to_string()),
        }),
    };
    cfg.store.insert_message(&summary_msg).await?;
    cfg.store
        .insert_part(&Part {
            id: new_part_id(),
            session_id: session_id.to_string(),
            message_id: summary_id.clone(),
            kind: PartKind::Text {
                text: summary,
                synthetic: None,
                ignored: None,
                time: Some(StartEndTime {
                    start: now,
                    end: Some(now),
                }),
                metadata: None,
            },
        })
        .await?;

    // 5. Inject the auto-continue user message (`compaction.ts:472-502`).
    if auto {
        let continue_id = new_message_id();
        let continue_msg = Info {
            id: continue_id.clone(),
            session_id: session_id.to_string(),
            body: InfoBody::User(User {
                time: UserTime { created: now_ms() },
                format: None,
                summary: None,
                agent: cfg.agent.clone(),
                model: UserModel {
                    provider_id: provider.0.clone(),
                    model_id: model_id.0.clone(),
                    variant: None,
                },
                system: None,
                tools: None,
            }),
        };
        cfg.store.insert_message(&continue_msg).await?;
        let text = if overflow {
            format!("{OVERFLOW_PREFACE}{CONTINUE_PROMPT}")
        } else {
            CONTINUE_PROMPT.to_string()
        };
        cfg.store
            .insert_part(&Part {
                id: new_part_id(),
                session_id: session_id.to_string(),
                message_id: continue_id,
                kind: PartKind::Text {
                    text,
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
    }

    Ok(())
}

/// Erase old completed tool outputs to reclaim context — port of `prune`
/// (`compaction.ts:243-287`).
///
/// Walks the history newest-first, protecting the last two turns and any
/// `skill` tool outputs. Once at least [`PRUNE_PROTECT`] trailing tokens of
/// tool output have been seen, subsequent (older) completed tool outputs are
/// collected; if the collected total exceeds [`PRUNE_MINIMUM`] their
/// [`ToolState::Completed`] `time.compacted` timestamps are stamped so the
/// converter substitutes a cleared-output placeholder. The walk stops at an
/// already-compacted part or an assistant summary boundary.
///
/// # Errors
/// Returns [`CompactionError::Storage`] on a persistence failure.
pub async fn prune(cfg: &RunConfig, session_id: &str) -> Result<(), CompactionError> {
    let msgs = cfg.store.messages_with_parts(session_id).await?;

    let mut total: u64 = 0;
    let mut pruned: u64 = 0;
    let mut to_prune: Vec<Part> = Vec::new();
    let mut turns: usize = 0;
    let mut seen_read_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    'outer: for msg in msgs.iter().rev() {
        if msg.info.is_user() {
            turns += 1;
        }
        // Protect the last two turns (`compaction.ts:260-261`).
        if turns < 2 {
            continue;
        }
        // Stop at a summary boundary (`compaction.ts:262`).
        if msg
            .info
            .as_assistant()
            .is_some_and(|a| a.summary == Some(true))
        {
            break;
        }
        for part in msg.parts.iter().rev() {
            let PartKind::Tool {
                tool,
                state:
                    ToolState::Completed {
                        input, output, time, ..
                    },
                ..
            } = &part.kind
            else {
                continue;
            };
            if PRUNE_PROTECTED_TOOLS.contains(&tool.as_str()) {
                continue;
            }
            if time.compacted.is_some() {
                break 'outer;
            }
            // Keep the NEWEST read of each file path (walk is newest-first),
            // bounded so a session that touched hundreds of files still frees
            // memory. Pruning every old read forces the agent to re-read the
            // same files over and over in long sessions — the "re-exploration
            // loop". Exempt reads don't consume the protect budget.
            if tool == "read"
                && seen_read_paths.len() < PRUNE_READ_PATH_EXEMPT_MAX
                && let Some(path) = input.get("filePath").and_then(|v| v.as_str())
                && seen_read_paths.insert(path.to_string())
            {
                continue;
            }
            let est = estimate_tokens(output);
            total += est;
            if total <= cfg.prune_protect_tokens {
                continue;
            }
            pruned += est;
            to_prune.push(part.clone());
        }
    }

    if pruned > PRUNE_MINIMUM {
        for mut part in to_prune {
            if let PartKind::Tool {
                state: ToolState::Completed { time, .. },
                ..
            } = &mut part.kind
            {
                time.compacted = Some(now_ms());
            }
            cfg.store.update_part(&part).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_storage::model::{
        Assistant, AssistantPath, AssistantTime, TokenCache, Tokens, User, UserModel, UserTime,
        WithParts,
    };

    fn provider() -> ProviderId {
        ProviderId::new("anthropic")
    }
    fn model() -> ModelId {
        ModelId::new("claude-3")
    }

    fn user_turn(id: &str, text: &str) -> WithParts {
        WithParts {
            info: Info {
                id: id.into(),
                session_id: "ses_1".into(),
                body: InfoBody::User(User {
                    time: UserTime { created: 0 },
                    format: None,
                    summary: None,
                    agent: "build".into(),
                    model: UserModel {
                        provider_id: "anthropic".into(),
                        model_id: "claude-3".into(),
                        variant: None,
                    },
                    system: None,
                    tools: None,
                }),
            },
            parts: vec![Part {
                id: new_part_id(),
                session_id: "ses_1".into(),
                message_id: id.into(),
                kind: PartKind::Text {
                    text: text.into(),
                    synthetic: None,
                    ignored: None,
                    time: None,
                    metadata: None,
                },
            }],
        }
    }

    fn assistant_turn(id: &str, parent: &str, text: &str) -> WithParts {
        WithParts {
            info: Info {
                id: id.into(),
                session_id: "ses_1".into(),
                body: InfoBody::Assistant(Assistant {
                    time: AssistantTime {
                        created: 0,
                        completed: Some(0),
                    },
                    error: None,
                    parent_id: parent.into(),
                    model_id: "claude-3".into(),
                    provider_id: "anthropic".into(),
                    mode: "build".into(),
                    agent: "build".into(),
                    path: AssistantPath {
                        cwd: "/w".into(),
                        root: "/w".into(),
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
                    finish: Some("stop".into()),
                }),
            },
            parts: vec![Part {
                id: new_part_id(),
                session_id: "ses_1".into(),
                message_id: id.into(),
                kind: PartKind::Text {
                    text: text.into(),
                    synthetic: None,
                    ignored: None,
                    time: None,
                    metadata: None,
                },
            }],
        }
    }

    /// A three-turn history where the middle turn is huge forces `select` to
    /// keep only the last turn, with `tail_start_id` at that turn's user id.
    #[test]
    fn select_keeps_last_turn_within_budget() {
        let big = "x".repeat(4_000); // ~1000+ tokens once serialized
        let msgs = vec![
            user_turn("msg_001", &big),
            assistant_turn("msg_002", "msg_001", &big),
            user_turn("msg_003", &big),
            assistant_turn("msg_004", "msg_003", &big),
            user_turn("msg_005", "hi"),
            assistant_turn("msg_006", "msg_005", "ok"),
        ];

        // Budget only fits the tiny last turn (msg_005/msg_006).
        let out = select(&msgs, 60, &provider(), &model());
        assert_eq!(out.tail_start_id.as_deref(), Some("msg_005"));
        assert_eq!(out.head_len, 4, "msg_001..=msg_004 summarized");
    }

    /// When the whole history fits, nothing is dropped.
    #[test]
    fn select_keeps_everything_when_within_budget() {
        let msgs = vec![
            user_turn("msg_001", "hi"),
            assistant_turn("msg_002", "msg_001", "ok"),
        ];
        let out = select(&msgs, 1_000_000, &provider(), &model());
        assert_eq!(out.tail_start_id, None);
        assert_eq!(out.head_len, msgs.len());
    }
}
