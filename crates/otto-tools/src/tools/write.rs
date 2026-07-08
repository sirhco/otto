//! The `write` tool — a port of opencode `packages/opencode/src/tool/write.ts`.
//!
//! Creates or overwrites a file (creating parent directories), asking the
//! `write` permission first (write.ts:54-62) and reporting whether the file
//! previously existed.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::{assert_external_directory, resolve_path};
use crate::tool::{ExecuteResult, PermissionRequest, Tool, ToolContext, ToolError, decode_args};

#[derive(Debug, Deserialize)]
struct WriteParams {
    #[serde(rename = "filePath")]
    file_path: String,
    content: String,
}

/// The `write` tool (write.ts:27).
#[derive(Clone, Default)]
pub struct WriteTool {
    lsp: Option<Arc<dyn crate::LspHandle>>,
}

impl WriteTool {
    /// Construct with an optional [`crate::LspHandle`]. When present, a
    /// `<diagnostics>` block for the written file (plus up to 5 other files with
    /// errors) is appended to the success output.
    pub fn new(lsp: Option<Arc<dyn crate::LspHandle>>) -> Self {
        Self { lsp }
    }
}

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn id(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/write.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The content to write to the file" },
                "filePath": { "type": "string", "description": "The absolute path to the file to write (must be absolute, not relative)" }
            },
            "required": ["content", "filePath"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: WriteParams = decode_args(self.id(), args)?;
        let path = resolve_path(ctx, &params.file_path);
        assert_external_directory(ctx, &path, "file").await?;

        let exists = tokio::fs::try_exists(&path).await.unwrap_or(false);
        let rel = path
            .strip_prefix(&ctx.directory)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());

        ctx.permission
            .ask(PermissionRequest {
                permission: "write".to_string(),
                patterns: vec![rel.clone()],
                always: vec!["*".to_string()],
                metadata: serde_json::json!({ "filepath": path.display().to_string() }),
            })
            .await?;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, &params.content).await?;

        let mut output = String::from("Wrote file successfully.");
        if let Some(lsp) = &self.lsp {
            let block = lsp.report_for(&path).await;
            if !block.is_empty() {
                output.push_str("\n\nLSP errors detected in this file, please fix:\n");
                output.push_str(&block);
            }
            let others = lsp.other_files_with_errors(&path, 5).await;
            if !others.is_empty() {
                output.push_str("\n\nLSP errors detected in other files:\n");
                for (_p, b) in others {
                    output.push_str(&b);
                    output.push('\n');
                }
            }
        }

        Ok(
            ExecuteResult::new(rel, output).with_metadata(serde_json::json!({
                "filepath": path.display().to_string(),
                "exists": exists,
            })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::RecordingGate;
    use std::sync::Arc;

    #[tokio::test]
    async fn creates_file_and_dirs_and_asks_permission() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c.txt");
        let gate = Arc::new(RecordingGate::allow());
        let ctx = ToolContext::builder(dir.path())
            .permission(gate.clone())
            .build();
        WriteTool::new(None)
            .execute(
                serde_json::json!({ "filePath": path.to_str().unwrap(), "content": "hello" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "hello");
        assert!(gate.asked_for("write"));
    }
}
