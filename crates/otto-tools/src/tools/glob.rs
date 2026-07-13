//! The `glob` tool — a port of opencode `packages/opencode/src/tool/glob.ts`.
//!
//! opencode delegates to ripgrep's `--files --glob`; here we walk the tree with
//! the `ignore` crate (which honors `.gitignore`) and match paths with
//! `globset`, returning results newest-first by mtime, capped at 100
//! (glob.ts:49-63).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use serde_json::Value;

use super::parallel_walk::parallel_collect;
use super::{assert_external_directory, resolve_path};
use crate::tool::{ExecuteResult, Tool, ToolContext, ToolError, decode_args};

const LIMIT: usize = 100;

#[derive(Debug, Deserialize)]
struct GlobParams {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

/// The `glob` tool (glob.ts:17).
#[derive(Debug, Default, Clone, Copy)]
pub struct GlobTool;

fn matches(matcher: &GlobMatcher, root: &Path, entry: &Path) -> bool {
    if let Ok(rel) = entry.strip_prefix(root)
        && matcher.is_match(rel)
    {
        return true;
    }
    // rg-style: a bare pattern like "*.rs" should also match by file name.
    entry
        .file_name()
        .map(|n| matcher.is_match(Path::new(n)))
        .unwrap_or(false)
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn id(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/glob.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "The glob pattern to match files against" },
                "path": { "type": "string", "description": "The directory to search in. If not specified, the current working directory will be used." }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: GlobParams = decode_args(self.id(), args)?;
        let search = match &params.path {
            Some(p) => resolve_path(ctx, p),
            None => ctx.directory.clone(),
        };

        if tokio::fs::metadata(&search)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            return Err(ToolError::Execution(format!(
                "glob path must be a directory: {}",
                search.display()
            )));
        }
        assert_external_directory(ctx, &search, "directory").await?;

        let matcher = Glob::new(&params.pattern)
            .map_err(|e| ToolError::Execution(format!("invalid glob pattern: {e}")))?
            .compile_matcher();

        let root = search.clone();
        let mut found: Vec<(PathBuf, SystemTime)> = parallel_collect(root.clone(), None, move |entry| {
            let path = entry.path();
            if !matches(&matcher, &root, path) {
                return Vec::new();
            }
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            vec![(path.to_path_buf(), mtime)]
        })
        .await;

        // newest-first by mtime (glob.ts sorts by mtime).
        found.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
        let truncated = found.len() > LIMIT;
        found.truncate(LIMIT);

        let title = search
            .strip_prefix(&ctx.directory)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| search.display().to_string());

        let mut output: Vec<String> = Vec::new();
        if found.is_empty() {
            output.push("No files found".to_string());
        } else {
            output.extend(found.iter().map(|(p, _)| p.display().to_string()));
            if truncated {
                output.push(String::new());
                output.push(format!(
                    "(Results are truncated: showing first {LIMIT} results. Consider using a more specific path or pattern.)"
                ));
            }
        }

        Ok(
            ExecuteResult::new(title, output.join("\n")).with_metadata(serde_json::json!({
                "count": found.len(),
                "truncated": truncated,
            })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn matches_pattern_and_respects_gitignore_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join(".gitignore"), "ignored.txt\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("a.txt"), "a").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tokio::fs::write(root.join("b.txt"), "b").await.unwrap();
        tokio::fs::write(root.join("ignored.txt"), "x")
            .await
            .unwrap();
        tokio::fs::write(root.join("c.rs"), "c").await.unwrap();

        let ctx = ToolContext::builder(root).build();
        let res = GlobTool
            .execute(serde_json::json!({ "pattern": "*.txt" }), &ctx)
            .await
            .unwrap();

        // gitignored file excluded
        assert!(!res.output.contains("ignored.txt"));
        // non-matching extension excluded
        assert!(!res.output.contains("c.rs"));
        assert!(res.output.contains("a.txt"));
        assert!(res.output.contains("b.txt"));
        // newest-first: b.txt (written later) appears before a.txt
        let bi = res.output.find("b.txt").unwrap();
        let ai = res.output.find("a.txt").unwrap();
        assert!(bi < ai, "expected newest-first ordering");
    }

    #[tokio::test]
    async fn recursive_pattern_and_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::create_dir_all(root.join("src")).await.unwrap();
        tokio::fs::write(root.join("src/main.rs"), "fn main(){}")
            .await
            .unwrap();
        let ctx = ToolContext::builder(root).build();

        let hit = GlobTool
            .execute(serde_json::json!({ "pattern": "**/*.rs" }), &ctx)
            .await
            .unwrap();
        assert!(hit.output.contains("main.rs"));

        let miss = GlobTool
            .execute(serde_json::json!({ "pattern": "**/*.py" }), &ctx)
            .await
            .unwrap();
        assert_eq!(miss.output, "No files found");
    }
}
