//! The `apply_patch` tool — a port of opencode
//! `packages/opencode/src/tool/apply_patch.ts`.
//!
//! Parses a patch envelope, resolves each hunk against `ctx.directory`
//! (guarding external directories), derives the resulting content and a unified
//! diff, asks the `edit` permission with the affected files, applies the
//! changes, then returns a `Success. Updated the following files:` summary
//! (`apply_patch.ts:274-303`). The parser/applier live in [`crate::patch`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::assert_external_directory;
use crate::patch::{
    ApplyPatchFileChange, Hunk, bom_join, derive_new_contents_from_chunks, generate_unified_diff,
    parse_patch,
};
use crate::tool::{ExecuteResult, PermissionRequest, Tool, ToolContext, ToolError, decode_args};

#[derive(Debug, Deserialize)]
struct ApplyPatchParams {
    #[serde(rename = "patchText")]
    patch_text: String,
}

/// The `apply_patch` tool (apply_patch.ts:22).
#[derive(Clone, Default)]
pub struct ApplyPatchTool {
    lsp: Option<Arc<dyn crate::LspHandle>>,
}

impl ApplyPatchTool {
    /// Construct with an optional [`crate::LspHandle`]. When present, a
    /// `<diagnostics>` block is appended per changed (non-deleted) target.
    pub fn new(lsp: Option<Arc<dyn crate::LspHandle>>) -> Self {
        Self { lsp }
    }
}

// A resolved change, retaining the display path + operation for the summary.
struct ResolvedChange {
    display: String,
    target: PathBuf,
    change: ApplyPatchFileChange,
    kind: char, // 'A' | 'M' | 'D'
    move_target: Option<PathBuf>,
    diff: String,
}

fn resolve(ctx: &ToolContext, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.directory.join(path)
    }
}

fn rel(ctx: &ToolContext, path: &Path) -> String {
    path.strip_prefix(&ctx.directory)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

#[async_trait::async_trait]
impl Tool for ApplyPatchTool {
    fn id(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/apply_patch.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "patchText": {
                    "type": "string",
                    "description": "The full patch text that describes all changes to be made"
                }
            },
            "required": ["patchText"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: ApplyPatchParams = decode_args(self.id(), args)?;
        if params.patch_text.is_empty() {
            return Err(ToolError::Execution("patchText is required".to_string()));
        }

        let hunks = parse_patch(&params.patch_text)
            .map_err(|e| ToolError::Execution(format!("apply_patch verification failed: {e}")))?;

        if hunks.is_empty() {
            let normalized = params.patch_text.replace("\r\n", "\n").replace('\r', "\n");
            let normalized = normalized.trim();
            if normalized == "*** Begin Patch\n*** End Patch" {
                return Err(ToolError::Execution(
                    "patch rejected: empty patch".to_string(),
                ));
            }
            return Err(ToolError::Execution(
                "apply_patch verification failed: no hunks found".to_string(),
            ));
        }

        let mut changes: Vec<ResolvedChange> = Vec::new();
        let mut total_diff = String::new();

        for hunk in &hunks {
            match hunk {
                Hunk::Add { path, contents } => {
                    let target = resolve(ctx, path);
                    assert_external_directory(ctx, &target, "file").await?;
                    let new_content = if contents.is_empty() || contents.ends_with('\n') {
                        contents.clone()
                    } else {
                        format!("{contents}\n")
                    };
                    let diff = generate_unified_diff("", &new_content);
                    total_diff.push_str(&diff);
                    total_diff.push('\n');
                    changes.push(ResolvedChange {
                        display: rel(ctx, &target),
                        target,
                        change: ApplyPatchFileChange::Add {
                            content: new_content,
                        },
                        kind: 'A',
                        move_target: None,
                        diff,
                    });
                }
                Hunk::Delete { path } => {
                    let target = resolve(ctx, path);
                    assert_external_directory(ctx, &target, "file").await?;
                    let content = tokio::fs::read_to_string(&target).await.map_err(|e| {
                        ToolError::Execution(format!("apply_patch verification failed: {e}"))
                    })?;
                    let diff = generate_unified_diff(&content, "");
                    total_diff.push_str(&diff);
                    total_diff.push('\n');
                    changes.push(ResolvedChange {
                        display: rel(ctx, &target),
                        target,
                        change: ApplyPatchFileChange::Delete { content },
                        kind: 'D',
                        move_target: None,
                        diff,
                    });
                }
                Hunk::Update {
                    path,
                    move_path,
                    chunks,
                } => {
                    let target = resolve(ctx, path);
                    assert_external_directory(ctx, &target, "file").await?;
                    let meta = tokio::fs::metadata(&target).await;
                    match meta {
                        Ok(m) if !m.is_dir() => {}
                        _ => {
                            return Err(ToolError::Execution(format!(
                                "apply_patch verification failed: Failed to read file to update: {}",
                                target.display()
                            )));
                        }
                    }
                    let original = tokio::fs::read_to_string(&target).await.map_err(|e| {
                        ToolError::Execution(format!("apply_patch verification failed: {e}"))
                    })?;
                    let update =
                        derive_new_contents_from_chunks(path, chunks, &original).map_err(|e| {
                            ToolError::Execution(format!("apply_patch verification failed: {e}"))
                        })?;
                    let content = bom_join(&update.content, update.bom);

                    let move_target = match move_path {
                        Some(mv) => {
                            let mt = resolve(ctx, mv);
                            assert_external_directory(ctx, &mt, "file").await?;
                            Some(mt)
                        }
                        None => None,
                    };
                    let display = rel(ctx, move_target.as_deref().unwrap_or(&target));
                    total_diff.push_str(&update.unified_diff);
                    total_diff.push('\n');
                    changes.push(ResolvedChange {
                        display,
                        target,
                        change: ApplyPatchFileChange::Update {
                            unified_diff: update.unified_diff.clone(),
                            move_path: move_path.clone(),
                            new_content: content,
                        },
                        kind: 'M',
                        move_target,
                        diff: update.unified_diff,
                    });
                }
            }
        }

        // Ask the edit permission with the affected files (apply_patch.ts:205-215).
        let patterns: Vec<String> = changes.iter().map(|c| c.display.clone()).collect();
        ctx.permission
            .ask(PermissionRequest {
                permission: "edit".to_string(),
                patterns: patterns.clone(),
                always: vec!["*".to_string()],
                metadata: serde_json::json!({
                    "filepath": patterns.join(", "),
                    "diff": total_diff,
                }),
            })
            .await?;

        // Apply the changes (apply_patch.ts:217-258).
        for c in &changes {
            match &c.change {
                ApplyPatchFileChange::Add { content } => {
                    write_with_dirs(&c.target, content).await?;
                }
                ApplyPatchFileChange::Delete { .. } => {
                    tokio::fs::remove_file(&c.target).await?;
                }
                ApplyPatchFileChange::Update { new_content, .. } => {
                    if let Some(mt) = &c.move_target {
                        write_with_dirs(mt, new_content).await?;
                        tokio::fs::remove_file(&c.target).await?;
                    } else {
                        write_with_dirs(&c.target, new_content).await?;
                    }
                }
            }
        }

        // Summary (apply_patch.ts:274-284).
        let mut summary = String::from("Success. Updated the following files:");
        for c in &changes {
            summary.push('\n');
            summary.push(c.kind);
            summary.push(' ');
            summary.push_str(&c.display);
        }

        ctx.metadata.update(
            Some(summary.clone()),
            Some(serde_json::json!({ "diff": total_diff })),
        );

        let files: Vec<Value> = changes
            .iter()
            .map(|c| {
                serde_json::json!({
                    "filePath": c.target.display().to_string(),
                    "relativePath": c.display,
                    "type": match c.kind { 'A' => "add", 'D' => "delete", _ => "update" },
                    "patch": c.diff,
                })
            })
            .collect();

        // Append a `<diagnostics>` block per changed, non-deleted target
        // (apply_patch.ts:266-293).
        let mut output = summary.clone();
        if let Some(lsp) = &self.lsp {
            for c in &changes {
                if c.kind == 'D' {
                    continue;
                }
                let touched = c.move_target.as_deref().unwrap_or(&c.target);
                let block = lsp.report_for(touched).await;
                if !block.is_empty() {
                    output.push_str(&format!(
                        "\n\nLSP errors detected in {}, please fix:\n{}",
                        c.display, block
                    ));
                }
            }
        }

        Ok(ExecuteResult::new(summary, output)
            .with_metadata(serde_json::json!({ "diff": total_diff, "files": files })))
    }
}

async fn write_with_dirs(path: &Path, content: &str) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, content).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::RecordingGate;
    use std::sync::Arc;

    #[tokio::test]
    async fn add_file_creates_and_asks_edit() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(RecordingGate::allow());
        let ctx = ToolContext::builder(dir.path())
            .permission(gate.clone())
            .build();
        let patch = "*** Begin Patch\n*** Add File: greeting.txt\n+Hello world\n*** End Patch";
        let res = ApplyPatchTool::new(None)
            .execute(serde_json::json!({ "patchText": patch }), &ctx)
            .await
            .unwrap();
        assert!(gate.asked_for("edit"));
        assert!(res.output.contains("A greeting.txt"));
        let got = tokio::fs::read_to_string(dir.path().join("greeting.txt"))
            .await
            .unwrap();
        assert_eq!(got, "Hello world\n");
    }

    #[tokio::test]
    async fn update_and_move() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("app.py"),
            "def greet():\n    print(\"Hi\")\n",
        )
        .await
        .unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let patch = "*** Begin Patch\n*** Update File: app.py\n*** Move to: main.py\n@@ def greet():\n-    print(\"Hi\")\n+    print(\"Hello\")\n*** End Patch";
        let res = ApplyPatchTool::new(None)
            .execute(serde_json::json!({ "patchText": patch }), &ctx)
            .await
            .unwrap();
        assert!(res.output.contains("M main.py"));
        assert!(!dir.path().join("app.py").exists());
        let got = tokio::fs::read_to_string(dir.path().join("main.py"))
            .await
            .unwrap();
        assert_eq!(got, "def greet():\n    print(\"Hello\")\n");
    }

    #[tokio::test]
    async fn delete_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("old.txt"), "gone\n")
            .await
            .unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let patch = "*** Begin Patch\n*** Delete File: old.txt\n*** End Patch";
        let res = ApplyPatchTool::new(None)
            .execute(serde_json::json!({ "patchText": patch }), &ctx)
            .await
            .unwrap();
        assert!(res.output.contains("D old.txt"));
        assert!(!dir.path().join("old.txt").exists());
    }

    #[tokio::test]
    async fn missing_markers_is_execution_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let err = ApplyPatchTool::new(None)
            .execute(serde_json::json!({ "patchText": "nope" }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing Begin/End markers"));
    }

    #[tokio::test]
    async fn empty_patch_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let err = ApplyPatchTool::new(None)
            .execute(
                serde_json::json!({ "patchText": "*** Begin Patch\n*** End Patch" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty patch"));
    }

    #[tokio::test]
    async fn denied_permission_blocks_write() {
        struct DenyEdit;
        #[async_trait::async_trait]
        impl crate::tool::PermissionGate for DenyEdit {
            async fn ask(
                &self,
                req: crate::tool::PermissionRequest,
            ) -> Result<(), crate::tool::PermissionDenied> {
                Err(crate::tool::PermissionDenied {
                    permission: req.permission,
                    by_user: true,
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::builder(dir.path())
            .permission(Arc::new(DenyEdit))
            .build();
        let patch = "*** Begin Patch\n*** Add File: x.txt\n+hi\n*** End Patch";
        let err = ApplyPatchTool::new(None)
            .execute(serde_json::json!({ "patchText": patch }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("denied"));
        assert!(!dir.path().join("x.txt").exists());
    }
}
