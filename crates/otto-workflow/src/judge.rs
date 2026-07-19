//! The judgment node: dispatch a targeted prompt to an agent and parse its
//! reply as structured JSON.

use std::sync::Arc;

use otto_storage::model::MessageId;
use otto_tools::{SubagentOrigin, SubagentRequest, SubagentSpawner};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::error::WfError;

/// Run a judgment node: send `prompt` to `agent` (a fresh child session via the
/// spawner), then parse the returned text as JSON `T`. On a parse failure it
/// reprompts ONCE with an explicit "return only JSON" instruction before
/// giving up with `WfError::Parse`. `kind` is the calling workflow engine's
/// kind string (e.g. `"tdd"`), tagged onto the spawned child's
/// `SubagentRequest::origin` — this module is shared across engines, so it
/// cannot assume one.
pub async fn judge<T: DeserializeOwned>(
    spawner: &Arc<dyn SubagentSpawner>,
    agent: &str,
    parent_session_id: &str,
    prompt: String,
    abort: CancellationToken,
    kind: &str,
) -> Result<T, WfError> {
    let text = spawn_text(spawner, agent, parent_session_id, &prompt, &abort, kind).await?;
    if let Ok(v) = serde_json::from_str::<T>(text.trim()) {
        return Ok(v);
    }
    let reprompt = format!("{prompt}\n\nReturn ONLY valid JSON, no prose, no code fences.");
    let text2 = spawn_text(spawner, agent, parent_session_id, &reprompt, &abort, kind).await?;
    serde_json::from_str::<T>(text2.trim()).map_err(|e| WfError::Parse(e.to_string()))
}

async fn spawn_text(
    spawner: &Arc<dyn SubagentSpawner>,
    agent: &str,
    parent_session_id: &str,
    prompt: &str,
    abort: &CancellationToken,
    kind: &str,
) -> Result<String, WfError> {
    let req = SubagentRequest {
        subagent_type: agent.to_string(),
        description: "workflow judgment".to_string(),
        prompt: prompt.to_string(),
        parent_session_id: parent_session_id.into(),
        parent_message_id: MessageId::default(),
        task_id: None,
        command: None,
        abort: abort.clone(),
        // Intentionally UNTAPPED: the meaningfulness judge is a short,
        // low-value structured-JSON node whose activity is not worth streaming
        // to the TUI. Leave `event_tx: None` so no tap is attached.
        event_tx: None,
        directory: None,
        origin: SubagentOrigin::Workflow {
            kind: kind.to_string(),
        },
    };
    spawner.spawn(req).await.map_err(WfError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TaskStatus;
    use otto_tools::ToolError;
    use std::sync::Mutex;

    #[derive(serde::Deserialize)]
    struct Judgment {
        status: TaskStatus,
    }

    struct FixedJson(&'static str);

    #[async_trait::async_trait]
    impl SubagentSpawner for FixedJson {
        async fn spawn(&self, _req: SubagentRequest) -> Result<String, ToolError> {
            Ok(self.0.to_string())
        }
    }

    /// First call returns prose (unparseable); second returns JSON — proves the
    /// single reprompt path.
    struct ProseThenJson {
        calls: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl SubagentSpawner for ProseThenJson {
        async fn spawn(&self, _req: SubagentRequest) -> Result<String, ToolError> {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            if *c == 1 {
                Ok("Sure — the task looks done to me.".to_string())
            } else {
                Ok("{\"status\":\"BLOCKED\"}".to_string())
            }
        }
    }

    struct AlwaysProse;

    #[async_trait::async_trait]
    impl SubagentSpawner for AlwaysProse {
        async fn spawn(&self, _req: SubagentRequest) -> Result<String, ToolError> {
            Ok("not json at all".to_string())
        }
    }

    #[tokio::test]
    async fn judge_parses_clean_json() {
        let s: Arc<dyn SubagentSpawner> = Arc::new(FixedJson("{\"status\":\"DONE\"}"));
        let out: Judgment = judge(
            &s,
            "general",
            "ses_x",
            "assess".to_string(),
            CancellationToken::new(),
            "tdd",
        )
        .await
        .unwrap();
        assert_eq!(out.status, TaskStatus::Done);
    }

    #[tokio::test]
    async fn judge_reprompts_once_then_parses() {
        let s: Arc<dyn SubagentSpawner> = Arc::new(ProseThenJson {
            calls: Mutex::new(0),
        });
        let out: Judgment = judge(
            &s,
            "general",
            "ses_x",
            "assess".to_string(),
            CancellationToken::new(),
            "tdd",
        )
        .await
        .unwrap();
        assert_eq!(out.status, TaskStatus::Blocked);
    }

    #[tokio::test]
    async fn judge_errors_after_second_parse_failure() {
        let s: Arc<dyn SubagentSpawner> = Arc::new(AlwaysProse);
        let r: Result<Judgment, WfError> = judge(
            &s,
            "general",
            "ses_x",
            "assess".to_string(),
            CancellationToken::new(),
            "tdd",
        )
        .await;
        assert!(matches!(r, Err(WfError::Parse(_))));
    }
}
