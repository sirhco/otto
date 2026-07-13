//! The subagent-spawn seam — the Rust analogue of the session/agent services
//! that opencode's `task` tool reaches through `ctx.extra` in
//! `packages/opencode/src/tool/task.ts` (TaskTool, task.ts:81-346).
//!
//! opencode's `TaskTool` pulls the `Agent`, `Session`, and prompt services out
//! of the Effect context and drives a child session inline. otto keeps
//! `otto-tools` free of the session/agent layer, so the `task` tool instead
//! depends only on this thin [`SubagentSpawner`] trait. The real
//! implementation (`SessionSubagentSpawner`) lives in `otto-session`, which
//! owns `run_loop`; here we only define the contract the tool calls.

use otto_id::{MessageId, SessionId};
use tokio_util::sync::CancellationToken;

use crate::tool::ToolError;

/// The request the `task` tool hands to a [`SubagentSpawner`] — the fields of
/// `TaskTool`'s parameters plus the owning-turn identity taken from
/// `ToolContext` (`ctx.sessionID`/`ctx.messageID`/`ctx.abort`, task.ts:121-160).
pub struct SubagentRequest {
    /// The subagent to resolve and run (`params.subagent_type`, task.ts:46).
    pub subagent_type: String,
    /// A short human title for the child session (`params.description`,
    /// task.ts:44); becomes the child session title / tool-result title.
    pub description: String,
    /// The instruction seeded as the child session's user message
    /// (`params.prompt`, task.ts:45,187).
    pub prompt: String,
    /// The parent session id (`ctx.sessionID`, task.ts:124) — the child's
    /// `parentID`.
    pub parent_session_id: SessionId,
    /// The parent assistant message id (`ctx.messageID`, task.ts:160).
    pub parent_message_id: MessageId,
    /// Resume a prior child session instead of creating a fresh one
    /// (`params.task_id`, task.ts:47-50, 121-123).
    pub task_id: Option<String>,
    /// The command that triggered the task, if any (`params.command`,
    /// task.ts:51).
    pub command: Option<String>,
    /// Cooperative cancellation inherited from the parent turn (`ctx.abort`,
    /// task.ts:305).
    pub abort: CancellationToken,
    /// Optional live event tap forwarded into the child run's `RunConfig.event_tx`.
    /// `None` (the default for the `task` tool + tests) leaves the child untapped.
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<otto_events::LLMEvent>>,
}

/// The seam the `task` tool calls to run a subagent turn — port of the inline
/// child-session drive in `TaskTool.execute` (task.ts:116-333), reduced to its
/// one observable output: the child's final text.
///
/// Implementors resolve the subagent, create/seed a child session, run the
/// child agent loop, and return the child's last assistant text. The `task`
/// tool wraps that text in the `<task>…</task>` envelope.
#[async_trait::async_trait]
pub trait SubagentSpawner: Send + Sync {
    /// Run the subagent described by `req` to completion and return its final
    /// assistant text.
    ///
    /// # Errors
    /// Returns a [`ToolError`] when the subagent type is unknown or the child
    /// run fails.
    async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError>;

    /// Spawn a batch of subagents, returning one result per request in input
    /// order, each request's error isolated from its siblings. The default
    /// runs serially (a safe fallback for every impl); parallel spawners
    /// override this — see `SessionSubagentSpawner`.
    async fn spawn_many(&self, reqs: Vec<SubagentRequest>) -> Vec<Result<String, ToolError>> {
        let mut out = Vec::with_capacity(reqs.len());
        for r in reqs {
            out.push(self.spawn(r).await);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    fn req(desc: &str) -> SubagentRequest {
        SubagentRequest {
            subagent_type: "general".into(),
            description: desc.into(),
            prompt: String::new(),
            parent_session_id: SessionId::default(),
            parent_message_id: MessageId::default(),
            task_id: None,
            command: None,
            abort: CancellationToken::new(),
            event_tx: None,
        }
    }

    /// Echoes the description; a description starting with "err" fails.
    /// Records the order in which spawn() was invoked.
    struct EchoSpawner {
        seen: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl SubagentSpawner for EchoSpawner {
        async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
            self.seen.lock().unwrap().push(req.description.clone());
            if req.description.starts_with("err") {
                Err(ToolError::Execution(format!("boom:{}", req.description)))
            } else {
                Ok(format!("ok:{}", req.description))
            }
        }
    }

    /// Overrides spawn_many with the parallel join_all pattern; each spawn
    /// sleeps to prove the override actually overlaps.
    struct SleepySpawner;

    #[async_trait::async_trait]
    impl SubagentSpawner for SleepySpawner {
        async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(format!("ok:{}", req.description))
        }
        async fn spawn_many(&self, reqs: Vec<SubagentRequest>) -> Vec<Result<String, ToolError>> {
            futures::future::join_all(reqs.into_iter().map(|r| self.spawn(r))).await
        }
    }

    #[tokio::test]
    async fn spawn_many_default_preserves_order() {
        let s = EchoSpawner {
            seen: Mutex::new(vec![]),
        };
        let out = s.spawn_many(vec![req("a"), req("b"), req("c")]).await;
        let got: Vec<_> = out.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(got, vec!["ok:a", "ok:b", "ok:c"]);
        assert_eq!(*s.seen.lock().unwrap(), vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn spawn_many_default_isolates_errors() {
        let s = EchoSpawner {
            seen: Mutex::new(vec![]),
        };
        let out = s.spawn_many(vec![req("a"), req("errX"), req("c")]).await;
        assert!(out[0].is_ok());
        assert!(out[1].is_err());
        assert!(out[2].is_ok());
        assert_eq!(out[0].as_ref().unwrap(), "ok:a");
        assert_eq!(out[2].as_ref().unwrap(), "ok:c");
    }

    #[tokio::test]
    async fn spawn_many_override_runs_concurrently() {
        let s = SleepySpawner;
        let start = Instant::now();
        let out = s
            .spawn_many(vec![req("a"), req("b"), req("c"), req("d")])
            .await;
        let elapsed = start.elapsed();
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(std::result::Result::is_ok));
        // 4 x 50ms serial = 200ms; concurrent join_all ~= 50ms. Assert well
        // under the serial sum (tokio timer sleeps overlap even on the
        // current-thread test runtime).
        assert!(elapsed < Duration::from_millis(150), "elapsed={elapsed:?}");
    }
}
