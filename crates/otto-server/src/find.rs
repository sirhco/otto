//! Minimal file-search backends for the `find` / `file` routes.
//!
//! These back the TUI's file features (opencode
//! `server/routes/instance/httpapi/groups/file.ts`). They are intentionally
//! simple, dependency-free walks of the instance directory rather than a full
//! ripgrep/glob engine.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// How many entries a `find` walk will return before stopping.
const MAX_RESULTS: usize = 200;
/// How deep the directory walk descends.
const MAX_DEPTH: usize = 12;
/// Directory names skipped while walking (VCS/build noise).
const SKIP_DIRS: [&str; 5] = [".git", "target", "node_modules", ".svn", ".hg"];

/// Recursively collect files under `root`, skipping noise directories.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH || out.len() >= MAX_RESULTS * 8 {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                let name = entry.file_name();
                if SKIP_DIRS.iter().any(|s| *s == name) {
                    continue;
                }
                stack.push((path, depth + 1));
            } else if ft.is_file() {
                out.push(path);
            }
        }
    }
    out
}

/// A substring-in-content grep over `root` — the backend of `GET /find`.
///
/// Returns `[{ path, line, text }]` for lines containing `pattern`.
#[must_use]
pub fn grep(root: &Path, pattern: &str) -> Value {
    if pattern.is_empty() {
        return json!([]);
    }
    let mut matches = Vec::new();
    for path in walk(root) {
        if matches.len() >= MAX_RESULTS {
            break;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for (idx, line) in content.lines().enumerate() {
            if line.contains(pattern) {
                matches.push(json!({
                    "path": path.display().to_string(),
                    "line": idx + 1,
                    "text": line,
                }));
                if matches.len() >= MAX_RESULTS {
                    break;
                }
            }
        }
    }
    Value::Array(matches)
}

/// A filename substring match over `root` — the backend of `GET /find/file`.
///
/// Returns an array of matching paths (relative-to-`root` display strings).
#[must_use]
pub fn find_files(root: &Path, query: &str) -> Value {
    let paths: Vec<Value> = walk(root)
        .into_iter()
        .filter(|p| {
            query.is_empty()
                || p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(query))
        })
        .take(MAX_RESULTS)
        .map(|p| Value::String(p.display().to_string()))
        .collect();
    Value::Array(paths)
}

/// Read a file's contents — the backend of `GET /file/content`.
///
/// # Errors
/// Returns the underlying [`std::io::Error`] when the path cannot be read.
pub fn read(root: &Path, path: &str) -> std::io::Result<Value> {
    let candidate = Path::new(path);
    let full = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let content = fs::read_to_string(&full)?;
    Ok(json!({ "path": full.display().to_string(), "content": content }))
}
