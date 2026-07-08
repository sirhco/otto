//! Message-list derivations ported from opencode's
//! `packages/opencode/src/session/message-v2.ts`.
//!
//! * [`latest`] — the `latest()` reducer (`message-v2.ts:585-601`).
//! * [`filter_compacted`] — the `filterCompacted()` transform
//!   (`message-v2.ts:521-572`).

use std::collections::HashSet;

use crate::model::{Info, Part, PartKind, WithParts};

/// Result of [`latest`] — the newest user / assistant / finished-assistant
/// messages plus outstanding tasks (`message-v2.ts:585-601`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Latest {
    /// Highest-id user message.
    pub user: Option<Info>,
    /// Highest-id assistant message.
    pub assistant: Option<Info>,
    /// Highest-id assistant message with a `finish` reason set.
    pub finished: Option<Info>,
    /// Compaction/subtask parts newer than [`finished`](Latest::finished).
    pub tasks: Vec<Part>,
}

/// Computes the newest bindings by **max message id** — not by array position,
/// because [`filter_compacted`] reorders messages for model consumption.
///
/// Direct port of `latest()` (`message-v2.ts:585-601`): `user` / `assistant`
/// are the highest-id messages of each role, `finished` is the highest-id
/// assistant with `finish` set, and `tasks` collects every compaction/subtask
/// part attached to messages with id greater than `finished.id`.
#[must_use]
pub fn latest(msgs: &[WithParts]) -> Latest {
    let mut user: Option<&Info> = None;
    let mut assistant: Option<&Info> = None;
    let mut finished: Option<&Info> = None;

    for m in msgs {
        let info = &m.info;
        if info.is_user() && user.is_none_or(|u| info.id > u.id) {
            user = Some(info);
        }
        if info.is_assistant() && assistant.is_none_or(|a| info.id > a.id) {
            assistant = Some(info);
        }
        if info.is_assistant()
            && info.as_assistant().is_some_and(|a| a.finish.is_some())
            && finished.is_none_or(|f| info.id > f.id)
        {
            finished = Some(info);
        }
    }

    let finished_id = finished.map(|f| f.id.clone());
    let tasks: Vec<Part> = msgs
        .iter()
        .flat_map(|m| {
            if let Some(fid) = &finished_id
                && &m.info.id <= fid
            {
                return Vec::new();
            }
            m.parts
                .iter()
                .filter(|p| {
                    matches!(
                        p.kind,
                        PartKind::Compaction { .. } | PartKind::Subtask { .. }
                    )
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect();

    Latest {
        user: user.cloned(),
        assistant: assistant.cloned(),
        finished: finished.cloned(),
        tasks,
    }
}

/// Drops superseded (pre-compaction) messages and reorders the retained tail
/// for model consumption.
///
/// Direct port of `filterCompacted()` (`message-v2.ts:521-572`):
///
/// 1. Walk the messages, tracking assistant messages that are compaction
///    summaries (`summary && finish && !error`) by their `parentID`. When a
///    user message that is such a parent carries a `compaction` part, stop the
///    walk at the part's `tail_start_id` (or immediately if unset).
/// 2. Reverse the collected prefix.
/// 3. When the compaction/summary/tail indices line up, splice-reorder into
///    `[compaction..=summary, tail..compaction, after-summary]`.
#[must_use]
pub fn filter_compacted(msgs: Vec<WithParts>) -> Vec<WithParts> {
    let mut result: Vec<WithParts> = Vec::new();
    let mut completed: HashSet<String> = HashSet::new();
    let mut retain: Option<String> = None;

    for msg in msgs {
        // Snapshot the values we need before moving `msg` into `result`.
        let msg_id = msg.info.id.clone();
        let is_user = msg.info.is_user();
        // `Some(tail)` if a compaction part exists on this message; the inner
        // `Option` is that part's `tail_start_id`.
        let compaction_part_tail: Option<Option<String>> =
            msg.parts.iter().find_map(|p| match &p.kind {
                PartKind::Compaction { tail_start_id, .. } => Some(tail_start_id.clone()),
                _ => None,
            });
        // `Some(parentID)` if this is a compaction-summary assistant message.
        let summary_parent: Option<String> = msg.info.as_assistant().and_then(|a| {
            if a.summary == Some(true) && a.finish.is_some() && a.error.is_none() {
                Some(a.parent_id.clone())
            } else {
                None
            }
        });

        result.push(msg);

        if let Some(r) = &retain {
            if &msg_id == r {
                break;
            }
            continue;
        }

        if is_user && completed.contains(&msg_id) {
            match compaction_part_tail {
                None => continue,
                Some(None) => break,
                Some(Some(tail)) => {
                    let stop = msg_id == tail;
                    retain = Some(tail);
                    if stop {
                        break;
                    }
                    continue;
                }
            }
        }

        if let Some(parent) = summary_parent {
            completed.insert(parent);
        }
    }

    result.reverse();

    // findLastIndex: user message carrying a compaction part with a defined
    // tail_start_id (message-v2.ts:545-549).
    let compaction_index = result.iter().rposition(|msg| {
        msg.info.is_user()
            && msg.parts.iter().any(|p| {
                matches!(
                    &p.kind,
                    PartKind::Compaction {
                        tail_start_id: Some(_),
                        ..
                    }
                )
            })
    });

    let compaction_id = compaction_index.map(|i| result[i].info.id.clone());
    let tail_start_id: Option<String> = compaction_index.and_then(|i| {
        result[i].parts.iter().find_map(|p| match &p.kind {
            PartKind::Compaction {
                tail_start_id: Some(t),
                ..
            } => Some(t.clone()),
            _ => None,
        })
    });

    let summary_index = match (compaction_index, &compaction_id) {
        (Some(ci), Some(cid)) => result.iter().enumerate().position(|(idx, msg)| {
            idx > ci
                && msg
                    .info
                    .as_assistant()
                    .is_some_and(|a| a.summary == Some(true) && &a.parent_id == cid)
        }),
        _ => None,
    };

    let tail_index = tail_start_id.and_then(|t| result.iter().position(|msg| msg.info.id == t));

    // -1 sentinels to mirror the JS index comparisons (message-v2.ts:564-570).
    let ci = index_or_neg1(compaction_index);
    let si = index_or_neg1(summary_index);
    let ti = index_or_neg1(tail_index);

    if ti >= 0 && ti < ci && si > ci {
        let ci = ci as usize;
        let si = si as usize;
        let ti = ti as usize;
        let mut out = Vec::with_capacity(result.len());
        out.extend(result[ci..=si].iter().cloned());
        out.extend(result[ti..ci].iter().cloned());
        out.extend(result[si + 1..].iter().cloned());
        return out;
    }

    result
}

/// Maps `Some(i)` to `i as isize` and `None` to `-1`, mirroring the JS
/// `findIndex`/`findLastIndex` sentinel semantics.
fn index_or_neg1(index: Option<usize>) -> isize {
    index.map_or(-1, |i| i as isize)
}
