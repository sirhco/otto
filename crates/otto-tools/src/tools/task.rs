//! The `task` tool — subagent spawn, a port of opencode
//! `packages/opencode/src/tool/task.ts` (TaskTool, task.ts:81-346).
//!
//! The tool decodes the full parameter surface (task.ts:43-62) and, when the
//! context carries a [`SubagentSpawner`](crate::subagent::SubagentSpawner),
//! hands off to it and wraps the child's final text in the
//! `<task>…</task_result>…</task>` envelope opencode emits from `renderOutput`
//! (task.ts:64-79, 319). When no spawner is present (a bare
//! [`ToolContext`] built by a call site outside a run loop) the tool reports
//! that subagents are unavailable, preserving the pre-spawner behavior.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::subagent::SubagentRequest;
use crate::tool::{ExecuteResult, Tool, ToolContext, ToolError, decode_args};

/// Parameters for `task` (task.ts:43-62).
#[derive(Debug, Deserialize)]
struct TaskParams {
    description: String,
    prompt: String,
    subagent_type: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    background: Option<bool>,
}

/// The `task` tool (task.ts:81).
#[derive(Debug, Default, Clone, Copy)]
pub struct TaskTool;

impl TaskTool {
    /// Wrap the child's final text in the `<task>…</task>` envelope — port of
    /// `renderOutput` for the completed state (task.ts:64-79, 319). The exact
    /// tag layout matches the wrapper the model is trained to parse.
    fn render_output(result: &str) -> String {
        format!("<task>\n<task_result>\n{result}\n</task_result>\n</task>")
    }
}

#[async_trait::async_trait]
impl Tool for TaskTool {
    fn id(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/task.txt")
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "A short (3-5 words) description of the task" },
                "prompt": { "type": "string", "description": "The task for the agent to perform" },
                "subagent_type": { "type": "string", "description": "The type of specialized agent to use for this task" },
                "task_id": { "type": "string", "description": "Set only to resume a previous task's subagent session" },
                "command": { "type": "string", "description": "The command that triggered this task" },
                "background": { "type": "boolean", "description": "Run the agent in the background" }
            },
            "required": ["description", "prompt", "subagent_type"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: TaskParams = decode_args(self.id(), args)?;

        // Without a spawner (a bare context outside a run loop) the tool cannot
        // create a child session. Preserve the pre-spawner marker so callers
        // that never inject `RunConfig.subagent` see the same failure.
        let Some(spawner) = ctx.subagent.clone() else {
            return Err(ToolError::Execution(
                "subagents not yet available (Phase 4)".to_string(),
            ));
        };

        // `background` is accepted for parameter parity (task.ts:56-62) but the
        // otto spawner always runs foreground; ignore the flag.
        let _ = params.background;

        let req = SubagentRequest {
            subagent_type: params.subagent_type,
            description: params.description.clone(),
            prompt: params.prompt,
            parent_session_id: ctx.session_id.clone(),
            parent_message_id: ctx.message_id.clone(),
            task_id: params.task_id,
            command: params.command,
            abort: ctx.abort.clone(),
            event_tx: None,
        };

        let text = spawner.spawn(req).await?;
        Ok(ExecuteResult::new(
            params.description,
            Self::render_output(&text),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_stub_marker_without_spawner() {
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        let err = TaskTool
            .execute(
                json!({
                    "description": "do a thing",
                    "prompt": "detailed prompt",
                    "subagent_type": "general"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Phase 4"));
    }

    #[tokio::test]
    async fn bad_params_are_invalid_arguments() {
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        let err = TaskTool
            .execute(json!({ "description": "x" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments { .. }));
    }

    #[tokio::test]
    async fn spawner_result_is_wrapped() {
        use crate::subagent::{SubagentRequest, SubagentSpawner};
        use std::sync::Arc;

        struct FakeSpawner;
        #[async_trait::async_trait]
        impl SubagentSpawner for FakeSpawner {
            async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
                assert_eq!(req.subagent_type, "general");
                assert_eq!(req.parent_session_id, "ses_parent");
                Ok("child answer".to_string())
            }
        }

        let ctx = ToolContext::builder(std::env::temp_dir())
            .session_id("ses_parent")
            .message_id("msg_parent")
            .subagent(Arc::new(FakeSpawner))
            .build();

        let out = TaskTool
            .execute(
                json!({
                    "description": "do X",
                    "prompt": "please do X",
                    "subagent_type": "general"
                }),
                &ctx,
            )
            .await
            .expect("spawn");

        assert_eq!(out.title, "do X");
        assert_eq!(
            out.output,
            "<task>\n<task_result>\nchild answer\n</task_result>\n</task>"
        );
    }
}
