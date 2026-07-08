//! The `todowrite` tool — a port of opencode
//! `packages/opencode/src/tool/todo.ts`.
//!
//! Validates and echoes a structured todo list. opencode persists the list via
//! a `Todo.Service` keyed by session id; that persistence is wired by the
//! session layer in a later phase, so here the validated list is returned as
//! pretty-printed JSON output and mirrored into `metadata.todos`
//! (`todo.ts:31-43`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool::{ExecuteResult, PermissionRequest, Tool, ToolContext, ToolError, decode_args};

/// Task status (`SessionTodo.Info.status`, `schema/src/session-todo.ts:8-10`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started.
    Pending,
    /// Actively working (exactly one at a time).
    InProgress,
    /// Finished successfully.
    Completed,
    /// No longer needed.
    Cancelled,
}

/// A single todo item. `id`/`priority` are optional to tolerate both the
/// opencode `{content,status,priority}` shape and id-carrying callers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    /// Optional stable id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Brief description of the task.
    pub content: String,
    /// Current status.
    pub status: TodoStatus,
    /// Optional priority (high/medium/low).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TodoWriteParams {
    todos: Vec<Todo>,
}

/// The `todowrite` tool (todo.ts:14).
#[derive(Debug, Default, Clone, Copy)]
pub struct TodoWriteTool;

#[async_trait::async_trait]
impl Tool for TodoWriteTool {
    fn id(&self) -> &str {
        "todowrite"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/todowrite.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The updated todo list",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "Unique identifier for the task" },
                            "content": { "type": "string", "description": "Brief description of the task" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"],
                                "description": "Current status of the task"
                            },
                            "priority": {
                                "type": "string",
                                "enum": ["high", "medium", "low"],
                                "description": "Priority level of the task"
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: TodoWriteParams = decode_args(self.id(), args)?;

        ctx.permission
            .ask(PermissionRequest {
                permission: "todowrite".to_string(),
                patterns: vec!["*".to_string()],
                always: vec!["*".to_string()],
                metadata: serde_json::json!({}),
            })
            .await?;

        let remaining = params
            .todos
            .iter()
            .filter(|t| t.status != TodoStatus::Completed)
            .count();
        let title = format!("{remaining} todos");
        let output =
            serde_json::to_string_pretty(&params.todos).unwrap_or_else(|_| "[]".to_string());

        Ok(ExecuteResult::new(title, output)
            .with_metadata(serde_json::json!({ "todos": params.todos })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::RecordingGate;
    use std::sync::Arc;

    #[tokio::test]
    async fn valid_list_echoes_and_asks_permission() {
        let gate = Arc::new(RecordingGate::allow());
        let ctx = ToolContext::builder(std::env::temp_dir())
            .permission(gate.clone())
            .build();
        let res = TodoWriteTool
            .execute(
                serde_json::json!({
                    "todos": [
                        { "id": "1", "content": "first", "status": "in_progress", "priority": "high" },
                        { "id": "2", "content": "second", "status": "completed" }
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(gate.asked_for("todowrite"));
        assert_eq!(res.title, "1 todos");
        assert!(res.output.contains("\"content\": \"first\""));
        assert_eq!(res.metadata["todos"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn bad_status_is_invalid_arguments() {
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        let err = TodoWriteTool
            .execute(
                serde_json::json!({
                    "todos": [{ "content": "x", "status": "bogus" }]
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments { .. }));
    }
}
