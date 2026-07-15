//! Workflow execution context and the `Workflow` trait.

use std::path::PathBuf;
use std::sync::Arc;

use otto_events::LLMEvent;
use otto_permission::Ruleset;
use otto_storage::Store;
use otto_tools::SubagentSpawner;
use otto_vcs::worktree::Worktree;
use tokio_util::sync::CancellationToken;

use crate::error::WfError;
use crate::runner::TestRunner;

/// Ambient services a workflow node needs: the subagent spawner, a worktree
/// handle, the test runner, the store, the working directory the runner + git
/// routines operate in, and the parent session id spawns attach to.
pub struct WfCtx {
    pub spawner: Arc<dyn SubagentSpawner>,
    pub worktree: Arc<Worktree>,
    pub runner: Arc<dyn TestRunner>,
    pub store: Store,
    pub directory: PathBuf,
    pub parent_session_id: String,
    pub permission: Arc<Ruleset>,
    pub progress: Option<ProgressSink>,
    pub subagent: Option<SubagentSink>,
    /// Cancelled when the caller wants a running workflow to stop starting
    /// new work. A genuinely cancellable token from the caller (CLI:
    /// `tokio::signal::ctrl_c()`; server: the per-session token
    /// `POST /workflow/{session}/cancel` cancels) — NOT a fresh
    /// `CancellationToken::new()` constructed inside a `Workflow::run` impl,
    /// which nothing external could ever cancel.
    pub abort: CancellationToken,
}

/// A single workflow progress event (side-channel; never gates control flow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WfProgress {
    pub task_index: Option<u32>,
    pub status: String,
    pub detail: String,
}

/// Sink a running workflow emits progress on. `None` = no observer (CLI/tests).
pub type ProgressSink = tokio::sync::mpsc::UnboundedSender<WfProgress>;

/// Best-effort emit: send a progress event if a sink is present. A dropped
/// receiver is ignored — progress must never fail a run.
pub fn emit(sink: &Option<ProgressSink>, task_index: Option<u32>, status: &str, detail: &str) {
    if let Some(tx) = sink {
        let _ = tx.send(WfProgress {
            task_index,
            status: status.to_string(),
            detail: detail.to_string(),
        });
    }
}

/// One filtered, human-readable subagent action for the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentActivity {
    pub task_index: u32,
    pub verb: String, // a tool name, or "text"
    pub detail: String,
}

/// Sink a workflow forwards filtered subagent activity on.
pub type SubagentSink = tokio::sync::mpsc::UnboundedSender<SubagentActivity>;

/// Truncate to `n` chars (char-safe), appending `…` if cut.
fn clip(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() > n {
        format!("{}…", t.chars().take(n).collect::<String>())
    } else {
        t.to_string()
    }
}

/// The salient argument of a tool call for display.
fn tool_detail(input: &serde_json::Value) -> String {
    for key in ["command", "file_path", "path", "pattern", "query", "url"] {
        if let Some(v) = input.get(key).and_then(serde_json::Value::as_str) {
            return clip(v, 100);
        }
    }
    clip(&input.to_string(), 80)
}

/// Fold one `LLMEvent` into an optional `(verb, detail)`. Tool calls map
/// immediately; text deltas coalesce into `buf` and flush (as `("text", …)`) on
/// `TextEnd`. Everything else is dropped (volume control).
pub(crate) fn summarize(ev: &LLMEvent, buf: &mut String) -> Option<(String, String)> {
    match ev {
        LLMEvent::ToolCall { name, input, .. } => Some((name.clone(), tool_detail(input))),
        LLMEvent::TextDelta { text, .. } => {
            buf.push_str(text);
            None
        }
        LLMEvent::TextEnd { .. } => {
            let d = clip(buf, 120);
            buf.clear();
            if d.is_empty() {
                None
            } else {
                Some(("text".to_string(), d))
            }
        }
        _ => None,
    }
}

/// Build the child `event_tx` for task `task_index`: a channel whose events are
/// filtered by `summarize` and forwarded to `sink` as `SubagentActivity`.
/// Returns `None` when `sink` is `None` (no tap → child stays untapped).
#[must_use]
pub fn tap_subagent(
    task_index: u32,
    sink: &Option<SubagentSink>,
) -> Option<tokio::sync::mpsc::UnboundedSender<LLMEvent>> {
    let sink = sink.clone()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LLMEvent>();
    tokio::spawn(async move {
        let mut buf = String::new();
        while let Some(ev) = rx.recv().await {
            if let Some((verb, detail)) = summarize(&ev, &mut buf) {
                let _ = sink.send(SubagentActivity {
                    task_index,
                    verb,
                    detail,
                });
            }
        }
        // flush any trailing coalesced text if the stream ended mid-block
        let d = clip(&buf, 120);
        if !d.is_empty() {
            let _ = sink.send(SubagentActivity {
                task_index,
                verb: "text".to_string(),
                detail: d,
            });
        }
    });
    Some(tx)
}

/// A deterministic workflow with a typed output.
#[async_trait::async_trait]
pub trait Workflow {
    type Output;
    async fn run(&self, cx: &WfCtx) -> Result<Self::Output, WfError>;
}

impl WfCtx {
    /// The working directory the runner and git routines operate in.
    #[must_use]
    pub fn directory(&self) -> &std::path::Path {
        &self.directory
    }
}

#[cfg(test)]
mod tests {
    // WfCtx holds non-trivial services (Arc<dyn ...>, Store, Worktree) that are
    // expensive to build here; its construction is exercised by the engine
    // integration tests and the CLI. This module asserts the struct SHAPE
    // compiles by referencing the fields in a function that is type-checked but
    // never run.
    #[allow(dead_code)]
    fn _shape(cx: &super::WfCtx) -> (&std::path::Path, &str, usize, bool, bool) {
        (
            cx.directory(),
            cx.parent_session_id.as_str(),
            cx.permission.rules().len(),
            cx.progress.is_some(),
            cx.subagent.is_some(),
        )
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn compiles() {
        // The real assertion is that `_shape` type-checks against the current
        // WfCtx field set (directory + parent_session_id + permission).
        assert!(true);
    }

    #[test]
    fn emit_is_noop_on_none() {
        // Must not panic with no sink.
        super::emit(&None, Some(1), "DONE", "ok");
    }

    #[tokio::test]
    async fn emit_sends_when_some() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        super::emit(&Some(tx), Some(2), "REVIEWING", "round 1");
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.task_index, Some(2));
        assert_eq!(ev.status, "REVIEWING");
        assert_eq!(ev.detail, "round 1");
    }

    #[test]
    fn summarize_maps_tool_call_and_coalesces_text() {
        use super::{SubagentActivity, summarize};
        use otto_events::LLMEvent;
        let _ = SubagentActivity {
            task_index: 0,
            verb: String::new(),
            detail: String::new(),
        };
        let mut buf = String::new();
        // A tool call → (verb, detail) immediately.
        let call = LLMEvent::ToolCall {
            id: "1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "cargo test"}),
            provider_executed: None,
            provider_metadata: None,
        };
        assert_eq!(
            summarize(&call, &mut buf),
            Some(("bash".to_string(), "cargo test".to_string()))
        );
        // Text deltas coalesce, flush on TextEnd.
        let d1 = LLMEvent::TextDelta {
            id: "t".into(),
            text: "tests ".into(),
            provider_metadata: None,
        };
        let d2 = LLMEvent::TextDelta {
            id: "t".into(),
            text: "pass".into(),
            provider_metadata: None,
        };
        assert_eq!(summarize(&d1, &mut buf), None);
        assert_eq!(summarize(&d2, &mut buf), None);
        let end = LLMEvent::TextEnd {
            id: "t".into(),
            provider_metadata: None,
        };
        assert_eq!(
            summarize(&end, &mut buf),
            Some(("text".to_string(), "tests pass".to_string()))
        );
        // Volume control: non-{ToolCall,TextDelta,TextEnd} variants are dropped.
        // A reasoning delta (and, by the same `_ => None` arm, ToolResult /
        // step / finish events) must NOT be forwarded.
        let reasoning = LLMEvent::ReasoningDelta {
            id: "r".into(),
            text: "thinking".into(),
            provider_metadata: None,
        };
        assert_eq!(summarize(&reasoning, &mut buf), None);
    }

    #[tokio::test]
    async fn tap_forwards_tagged_activity() {
        use super::{SubagentActivity, tap_subagent};
        use otto_events::LLMEvent;
        let (sink_tx, mut sink_rx) = tokio::sync::mpsc::unbounded_channel::<SubagentActivity>();
        let tap = tap_subagent(3, &Some(sink_tx)).expect("some sink");
        tap.send(LLMEvent::ToolCall {
            id: "1".into(),
            name: "read".into(),
            input: serde_json::json!({"file_path": "Cargo.toml"}),
            provider_executed: None,
            provider_metadata: None,
        })
        .unwrap();
        drop(tap); // close the channel so the forwarder task ends
        let act = sink_rx.recv().await.unwrap();
        assert_eq!(
            act,
            SubagentActivity {
                task_index: 3,
                verb: "read".into(),
                detail: "Cargo.toml".into(),
            }
        );
    }

    #[test]
    fn tap_is_none_when_sink_absent() {
        assert!(super::tap_subagent(0, &None).is_none());
    }
}
