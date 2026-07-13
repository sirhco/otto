//! Built-in filesystem tools — ports of opencode `packages/opencode/src/tool/*`.
//!
//! Each submodule is a struct implementing [`crate::tool::Tool`]. Shared helpers
//! (path resolution, the external-directory guard) live here.

pub mod apply_patch;
pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod invalid;
pub(crate) mod parallel_walk;
pub mod question;
pub mod read;
pub mod skill;
pub mod task;
pub mod todo;
pub mod webfetch;
pub mod websearch;
pub mod write;

use std::path::{Path, PathBuf};

use crate::tool::{PermissionRequest, ToolContext, ToolError};

pub use apply_patch::ApplyPatchTool;
pub use bash::BashTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use invalid::InvalidTool;
pub use question::QuestionTool;
pub use read::ReadTool;
pub use skill::SkillTool;
pub use task::TaskTool;
pub use todo::{Todo, TodoStatus, TodoWriteTool};
pub use webfetch::WebFetchTool;
pub use websearch::{WebSearchProvider, WebSearchQuery, WebSearchTool};
pub use write::WriteTool;

/// Resolve `input` against `ctx.directory` when it is relative, mirroring the
/// `path.isAbsolute(...) ? ... : path.resolve(directory, ...)` idiom used by
/// every opencode filesystem tool (e.g. `read.ts:235-237`).
pub(crate) fn resolve_path(ctx: &ToolContext, input: &str) -> PathBuf {
    let p = Path::new(input);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        ctx.directory.join(p)
    }
}

/// Whether `target` lies within `ctx.directory` — the lexical analogue of
/// opencode's `containsPath` (`external-directory.ts:26`).
fn contains_path(ctx: &ToolContext, target: &Path) -> bool {
    target.starts_with(&ctx.directory)
}

/// Guard against operating outside the working directory. Ports
/// `assertExternalDirectoryEffect` (`external-directory.ts:15-45`): if `target`
/// escapes `ctx.directory`, ask for the `external_directory` permission (which
/// [`crate::tool::AllowAll`] approves) before proceeding.
pub(crate) async fn assert_external_directory(
    ctx: &ToolContext,
    target: &Path,
    kind: &str,
) -> Result<(), ToolError> {
    if contains_path(ctx, target) {
        return Ok(());
    }
    let dir = if kind == "directory" {
        target.to_path_buf()
    } else {
        target.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    let glob = format!("{}/*", dir.display());
    ctx.permission
        .ask(PermissionRequest {
            permission: "external_directory".to_string(),
            patterns: vec![glob.clone()],
            always: vec![glob],
            metadata: serde_json::json!({
                "filepath": target.display().to_string(),
                "parentDir": dir.display().to_string(),
            }),
        })
        .await?;
    Ok(())
}
