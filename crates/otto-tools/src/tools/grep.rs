//! The `grep` tool — a port of opencode `packages/opencode/src/tool/grep.ts`.
//!
//! opencode shells out to ripgrep; here we replicate the behavior in pure Rust
//! with the `ignore` crate (respecting `.gitignore`), `globset` for the
//! `include` filter, and `regex` for matching. Results are grouped by file with
//! line numbers, capped at 100 (grep.ts:63-99).

use std::path::Path;

use globset::{Glob, GlobMatcher};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use super::parallel_walk::parallel_collect;
use super::{assert_external_directory, resolve_path};
use crate::tool::{ExecuteResult, Tool, ToolContext, ToolError, decode_args};

const LIMIT: usize = 100;

#[derive(Debug, Deserialize)]
struct GrepParams {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
}

/// The `grep` tool (grep.ts:20).
#[derive(Debug, Default, Clone, Copy)]
pub struct GrepTool;

struct Match {
    path: String,
    line: usize,
    text: String,
}

fn include_ok(matcher: &Option<GlobMatcher>, root: &Path, entry: &Path) -> bool {
    let Some(matcher) = matcher else {
        return true;
    };
    if let Ok(rel) = entry.strip_prefix(root)
        && matcher.is_match(rel)
    {
        return true;
    }
    entry
        .file_name()
        .map(|n| matcher.is_match(Path::new(n)))
        .unwrap_or(false)
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn id(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/grep.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "The regex pattern to search for in file contents" },
                "path": { "type": "string", "description": "The directory to search in. Defaults to the current working directory." },
                "include": { "type": "string", "description": "File pattern to include in the search (e.g. \"*.js\", \"*.{ts,tsx}\")" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: GrepParams = decode_args(self.id(), args)?;
        if params.pattern.is_empty() {
            return Err(ToolError::Execution("pattern is required".to_string()));
        }

        let requested = match &params.path {
            Some(p) => resolve_path(ctx, p),
            None => ctx.directory.clone(),
        };
        let kind = if tokio::fs::metadata(&requested)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            "directory"
        } else {
            "file"
        };
        assert_external_directory(ctx, &requested, kind).await?;

        let cwd = if kind == "directory" {
            requested.clone()
        } else {
            requested
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| requested.clone())
        };

        let re = Regex::new(&params.pattern)
            .map_err(|e| ToolError::Execution(format!("invalid regex pattern: {e}")))?;
        let include = match &params.include {
            Some(p) => Some(
                Glob::new(p)
                    .map_err(|e| ToolError::Execution(format!("invalid include pattern: {e}")))?
                    .compile_matcher(),
            ),
            None => None,
        };

        let empty = || {
            ExecuteResult::new(params.pattern.clone(), "No files found")
                .with_metadata(serde_json::json!({ "matches": 0, "truncated": false }))
        };

        let root = cwd.clone();
        let mut matches: Vec<Match> = parallel_collect(cwd, Some(LIMIT), move |entry| {
            let path = entry.path();
            if !include_ok(&include, &root, path) {
                return Vec::new();
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                return Vec::new();
            };
            content
                .lines()
                .enumerate()
                .filter(|(_, line)| re.is_match(line))
                .map(|(i, line)| Match {
                    path: path.display().to_string(),
                    line: i + 1,
                    text: line.to_string(),
                })
                .collect()
        })
        .await;

        if matches.is_empty() {
            return Ok(empty());
        }

        // Concurrent threads can push past LIMIT before the shared stop flag
        // is observed; cap here to match the single-threaded walk's bound.
        let truncated = matches.len() > LIMIT;
        matches.truncate(LIMIT);
        let total = matches.len();
        // Group by file, stable within discovery order.
        matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));

        let mut output: Vec<String> = vec![format!(
            "Found {total} matches{}",
            if truncated {
                " (more matches available)"
            } else {
                ""
            }
        )];
        let mut current = String::new();
        for m in &matches {
            if current != m.path {
                if !current.is_empty() {
                    output.push(String::new());
                }
                current = m.path.clone();
                output.push(format!("{}:", m.path));
            }
            output.push(format!("  Line {}: {}", m.line, m.text));
        }
        if truncated {
            output.push(String::new());
            output.push(
                "(Results truncated. Consider using a more specific path or pattern.)".to_string(),
            );
        }

        Ok(
            ExecuteResult::new(params.pattern.clone(), output.join("\n"))
                .with_metadata(serde_json::json!({ "matches": total, "truncated": truncated })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_matches_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("a.rs"), "fn foo() {}\nlet x = 1;\nfn bar() {}")
            .await
            .unwrap();
        let ctx = ToolContext::builder(root).build();
        let res = GrepTool
            .execute(serde_json::json!({ "pattern": "fn \\w+" }), &ctx)
            .await
            .unwrap();
        assert!(res.output.contains("Found 2 matches"));
        assert!(res.output.contains("Line 1: fn foo() {}"));
        assert!(res.output.contains("Line 3: fn bar() {}"));
        assert!(!res.output.contains("Line 2"));
    }

    #[tokio::test]
    async fn include_filter_limits_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::fs::write(root.join("a.rs"), "target here")
            .await
            .unwrap();
        tokio::fs::write(root.join("b.txt"), "target here")
            .await
            .unwrap();
        let ctx = ToolContext::builder(root).build();
        let res = GrepTool
            .execute(
                serde_json::json!({ "pattern": "target", "include": "*.rs" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(res.output.contains("a.rs"));
        assert!(!res.output.contains("b.txt"));
    }

    #[tokio::test]
    async fn no_match_returns_no_files_found() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "nothing here")
            .await
            .unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let res = GrepTool
            .execute(serde_json::json!({ "pattern": "zzz_absent" }), &ctx)
            .await
            .unwrap();
        assert_eq!(res.output, "No files found");
    }
}
