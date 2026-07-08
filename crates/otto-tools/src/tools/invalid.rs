//! The `invalid` tool — a port of opencode
//! `packages/opencode/src/tool/invalid.ts`.
//!
//! The repair sink: when the model emits a malformed tool call, the harness
//! routes it here and the tool echoes the decoded error back as output so the
//! model can correct itself (`invalid.ts:14-19`).

use serde::Deserialize;
use serde_json::Value;

use crate::tool::{ExecuteResult, Tool, ToolContext, ToolError, decode_args};

#[derive(Debug, Deserialize)]
struct InvalidParams {
    #[allow(dead_code)]
    tool: String,
    error: String,
}

/// The `invalid` tool (invalid.ts:9).
#[derive(Debug, Default, Clone, Copy)]
pub struct InvalidTool;

#[async_trait::async_trait]
impl Tool for InvalidTool {
    fn id(&self) -> &str {
        "invalid"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/invalid.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "tool": { "type": "string" },
                "error": { "type": "string" }
            },
            "required": ["tool", "error"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: InvalidParams = decode_args(self.id(), args)?;
        let _ = ctx;
        Ok(ExecuteResult::new(
            "Invalid Tool",
            format!(
                "The arguments provided to the tool are invalid: {}",
                params.error
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echoes_error_text() {
        let ctx = ToolContext::builder(std::env::temp_dir()).build();
        let res = InvalidTool
            .execute(
                serde_json::json!({ "tool": "read", "error": "missing filePath" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            res.output,
            "The arguments provided to the tool are invalid: missing filePath"
        );
        assert_eq!(res.title, "Invalid Tool");
    }
}
