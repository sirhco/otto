//! The `edit` tool and its string-replacement engine — a port of opencode
//! `packages/opencode/src/tool/edit.ts` (the 737-line 9-replacer engine).
//!
//! [`replace`] walks nine replacers in order (`edit.ts:694-704`), each a
//! progressively fuzzier strategy for locating `old_string` inside the file.
//! The first replacer that yields an *unambiguous*, non-disproportionate
//! candidate wins; ambiguous candidates are skipped (`edit.ts:705-721`). The
//! [`EditTool`] wraps that engine with line-ending detection/restoration, a
//! per-file lock, permission asks, and create-new-file semantics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use super::{assert_external_directory, resolve_path};
use crate::tool::{ExecuteResult, PermissionRequest, Tool, ToolContext, ToolError, decode_args};

// ---- error message constants (verbatim from edit.ts) ------------------------

const ERR_IDENTICAL: &str = "No changes to apply: oldString and newString are identical.";
const ERR_EMPTY_EXISTING: &str = "oldString cannot be empty when editing an existing file. Provide the exact text to replace, or use write for an intentional full-file replacement.";
const ERR_DISPROPORTIONATE: &str = "Refusing replacement because the matched span is much larger than oldString. Re-read the file and provide the full exact oldString for the intended replacement.";
const ERR_NOT_FOUND: &str = "Could not find oldString in the file. It must match exactly, including whitespace, indentation, and line endings.";
const ERR_MULTIPLE: &str = "Found multiple matches for oldString. Provide more surrounding context to make the match unique.";

// Similarity thresholds for block anchor fallback matching (edit.ts:220-221).
const SINGLE_CANDIDATE_SIMILARITY_THRESHOLD: f64 = 0.65;
const MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD: f64 = 0.65;

// ---- line-ending helpers (edit.ts:22-33) ------------------------------------

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn detect_line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

fn convert_to_line_ending(text: &str, ending: &str) -> String {
    if ending == "\n" {
        text.to_string()
    } else {
        text.replace('\n', "\r\n")
    }
}

// ---- Levenshtein (edit.ts:226-242) ------------------------------------------

fn levenshtein(a: &str, b: &str) -> usize {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    if ac.is_empty() || bc.is_empty() {
        return ac.len().max(bc.len());
    }
    let mut prev: Vec<usize> = (0..=bc.len()).collect();
    let mut cur = vec![0usize; bc.len() + 1];
    for i in 1..=ac.len() {
        cur[0] = i;
        for j in 1..=bc.len() {
            let cost = usize::from(ac[i - 1] != bc[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[bc.len()]
}

// ---- the nine replacers -----------------------------------------------------
//
// Each returns the candidate spans it would `yield*` in edit.ts. Byte offsets
// are used for span extraction (consistent with `content.find`/slicing in
// [`replace`]); `.trim()`/similarity comparisons operate on chars.

/// `SimpleReplacer` (edit.ts:244-246): the literal search string.
pub(crate) fn simple_replacer(_content: &str, find: &str) -> Vec<String> {
    vec![find.to_string()]
}

/// `LineTrimmedReplacer` (edit.ts:248-286): match lines ignoring per-line
/// leading/trailing whitespace.
pub(crate) fn line_trimmed_replacer(content: &str, find: &str) -> Vec<String> {
    let original: Vec<&str> = content.split('\n').collect();
    let mut search: Vec<&str> = find.split('\n').collect();
    if search.last() == Some(&"") {
        search.pop();
    }
    let mut out = Vec::new();
    if search.is_empty() || original.len() < search.len() {
        return out;
    }
    for i in 0..=(original.len() - search.len()) {
        let matches = (0..search.len()).all(|j| original[i + j].trim() == search[j].trim());
        if !matches {
            continue;
        }
        let mut start = 0usize;
        for line in original.iter().take(i) {
            start += line.len() + 1;
        }
        let mut end = start;
        for k in 0..search.len() {
            end += original[i + k].len();
            if k < search.len() - 1 {
                end += 1;
            }
        }
        out.push(content[start..end].to_string());
    }
    out
}

fn block_span(original: &[&str], start_line: usize, end_line: usize) -> (usize, usize) {
    let mut start = 0usize;
    for line in original.iter().take(start_line) {
        start += line.len() + 1;
    }
    let mut end = start;
    for (k, line) in original
        .iter()
        .enumerate()
        .take(end_line + 1)
        .skip(start_line)
    {
        end += line.len();
        if k < end_line {
            end += 1;
        }
    }
    (start, end)
}

/// `BlockAnchorReplacer` (edit.ts:288-425): match a ≥3-line block by its first
/// and last lines, disambiguating the middle with a Levenshtein similarity gate.
pub(crate) fn block_anchor_replacer(content: &str, find: &str) -> Vec<String> {
    let original: Vec<&str> = content.split('\n').collect();
    let mut search: Vec<&str> = find.split('\n').collect();
    if search.len() < 3 {
        return Vec::new();
    }
    if search.last() == Some(&"") {
        search.pop();
    }
    if search.len() < 3 {
        return Vec::new();
    }

    let first = search[0].trim();
    let last = search[search.len() - 1].trim();
    let block_size = search.len();
    let max_delta = 1.max((block_size as f64 * 0.25).floor() as usize);

    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for i in 0..original.len() {
        if original[i].trim() != first {
            continue;
        }
        #[allow(clippy::needless_range_loop)]
        for j in (i + 2)..original.len() {
            if original[j].trim() == last {
                let actual = j - i + 1;
                if (actual as isize - block_size as isize).unsigned_abs() <= max_delta {
                    candidates.push((i, j));
                }
                break;
            }
        }
    }

    if candidates.is_empty() {
        return Vec::new();
    }

    let similarity = |start_line: usize, end_line: usize| -> f64 {
        let actual = end_line - start_line + 1;
        let to_check = (block_size - 2).min(actual - 2);
        if to_check == 0 {
            return 1.0;
        }
        let mut sim = 0.0;
        let mut j = 1;
        while j < block_size - 1 && j < actual - 1 {
            let ol = original[start_line + j].trim();
            let sl = search[j].trim();
            let max_len = ol.chars().count().max(sl.chars().count());
            if max_len == 0 {
                j += 1;
                continue;
            }
            let dist = levenshtein(ol, sl);
            sim += (1.0 - dist as f64 / max_len as f64) / to_check as f64;
            j += 1;
        }
        sim
    };

    if candidates.len() == 1 {
        let (s, e) = candidates[0];
        // Early-exit variant (edit.ts:334-356): accumulate until threshold.
        let actual = e - s + 1;
        let to_check = (block_size - 2).min(actual - 2);
        let sim = if to_check == 0 {
            1.0
        } else {
            let mut acc = 0.0;
            let mut j = 1;
            while j < block_size - 1 && j < actual - 1 {
                let ol = original[s + j].trim();
                let sl = search[j].trim();
                let max_len = ol.chars().count().max(sl.chars().count());
                if max_len != 0 {
                    let dist = levenshtein(ol, sl);
                    acc += (1.0 - dist as f64 / max_len as f64) / to_check as f64;
                    if acc >= SINGLE_CANDIDATE_SIMILARITY_THRESHOLD {
                        break;
                    }
                }
                j += 1;
            }
            acc
        };
        if sim >= SINGLE_CANDIDATE_SIMILARITY_THRESHOLD {
            let (a, b) = block_span(&original, s, e);
            return vec![content[a..b].to_string()];
        }
        return Vec::new();
    }

    let mut best: Option<(usize, usize)> = None;
    let mut max_sim = -1.0f64;
    for &(s, e) in &candidates {
        let sim = similarity(s, e);
        if sim > max_sim {
            max_sim = sim;
            best = Some((s, e));
        }
    }
    if max_sim >= MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD
        && let Some((s, e)) = best
    {
        let (a, b) = block_span(&original, s, e);
        return vec![content[a..b].to_string()];
    }
    Vec::new()
}

fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `WhitespaceNormalizedReplacer` (edit.ts:427-469): match ignoring differences
/// in whitespace runs.
pub(crate) fn whitespace_normalized_replacer(content: &str, find: &str) -> Vec<String> {
    let normalized_find = normalize_whitespace(find);
    let lines: Vec<&str> = content.split('\n').collect();
    let mut out = Vec::new();

    for line in &lines {
        if normalize_whitespace(line) == normalized_find {
            out.push((*line).to_string());
        } else if normalize_whitespace(line).contains(&normalized_find) {
            let words: Vec<&str> = find.split_whitespace().collect();
            if !words.is_empty() {
                let pattern = words
                    .iter()
                    .map(|w| regex::escape(w))
                    .collect::<Vec<_>>()
                    .join(r"\s+");
                if let Ok(re) = Regex::new(&pattern)
                    && let Some(m) = re.find(line)
                {
                    out.push(m.as_str().to_string());
                }
            }
        }
    }

    let find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.len() > 1 && lines.len() >= find_lines.len() {
        for i in 0..=(lines.len() - find_lines.len()) {
            let block = lines[i..i + find_lines.len()].join("\n");
            if normalize_whitespace(&block) == normalized_find {
                out.push(block);
            }
        }
    }
    out
}

fn remove_indentation(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let non_empty: Vec<&&str> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
    if non_empty.is_empty() {
        return text.to_string();
    }
    let min_indent = non_empty
        .iter()
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                (*l).to_string()
            } else {
                l.chars().skip(min_indent).collect::<String>()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `IndentationFlexibleReplacer` (edit.ts:471-497): match after stripping the
/// common minimum indentation from both sides.
pub(crate) fn indentation_flexible_replacer(content: &str, find: &str) -> Vec<String> {
    let normalized_find = remove_indentation(find);
    let content_lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = find.split('\n').collect();
    let mut out = Vec::new();
    if content_lines.len() < find_lines.len() {
        return out;
    }
    for i in 0..=(content_lines.len() - find_lines.len()) {
        let block = content_lines[i..i + find_lines.len()].join("\n");
        if remove_indentation(&block) == normalized_find {
            out.push(block);
        }
    }
    out
}

fn unescape_string(input: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"\\(n|t|r|'|"|`|\\|\n|\$)"#).unwrap());
    re.replace_all(input, |caps: &regex::Captures| match &caps[1] {
        "n" | "\n" => "\n".to_string(),
        "t" => "\t".to_string(),
        "r" => "\r".to_string(),
        "'" => "'".to_string(),
        "\"" => "\"".to_string(),
        "`" => "`".to_string(),
        "\\" => "\\".to_string(),
        "$" => "$".to_string(),
        other => other.to_string(),
    })
    .into_owned()
}

/// `EscapeNormalizedReplacer` (edit.ts:499-546): match after resolving escape
/// sequences (`\n`, `\t`, ...).
pub(crate) fn escape_normalized_replacer(content: &str, find: &str) -> Vec<String> {
    let unescaped_find = unescape_string(find);
    let mut out = Vec::new();
    if content.contains(&unescaped_find) {
        out.push(unescaped_find.clone());
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = unescaped_find.split('\n').collect();
    if lines.len() >= find_lines.len() {
        for i in 0..=(lines.len() - find_lines.len()) {
            let block = lines[i..i + find_lines.len()].join("\n");
            if unescape_string(&block) == unescaped_find {
                out.push(block);
            }
        }
    }
    out
}

/// `TrimmedBoundaryReplacer` (edit.ts:562-586): match after trimming the search
/// string's outer whitespace.
pub(crate) fn trimmed_boundary_replacer(content: &str, find: &str) -> Vec<String> {
    let trimmed = find.trim();
    if trimmed == find {
        return Vec::new();
    }
    let mut out = Vec::new();
    if content.contains(trimmed) {
        out.push(trimmed.to_string());
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = find.split('\n').collect();
    if lines.len() >= find_lines.len() {
        for i in 0..=(lines.len() - find_lines.len()) {
            let block = lines[i..i + find_lines.len()].join("\n");
            if block.trim() == trimmed {
                out.push(block);
            }
        }
    }
    out
}

/// `ContextAwareReplacer` (edit.ts:588-644): match a ≥3-line block by first/last
/// anchors when ≥50% of the middle lines match exactly (when trimmed).
pub(crate) fn context_aware_replacer(content: &str, find: &str) -> Vec<String> {
    let mut find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.len() < 3 {
        return Vec::new();
    }
    if find_lines.last() == Some(&"") {
        find_lines.pop();
    }
    if find_lines.len() < 3 {
        return Vec::new();
    }

    let content_lines: Vec<&str> = content.split('\n').collect();
    let first = find_lines[0].trim();
    let last = find_lines[find_lines.len() - 1].trim();

    for i in 0..content_lines.len() {
        if content_lines[i].trim() != first {
            continue;
        }
        for j in (i + 2)..content_lines.len() {
            if content_lines[j].trim() == last {
                let block_lines = &content_lines[i..=j];
                if block_lines.len() == find_lines.len() {
                    let mut matching = 0usize;
                    let mut total = 0usize;
                    for k in 1..block_lines.len() - 1 {
                        let bl = block_lines[k].trim();
                        let fl = find_lines[k].trim();
                        if !bl.is_empty() || !fl.is_empty() {
                            total += 1;
                            if bl == fl {
                                matching += 1;
                            }
                        }
                    }
                    if total == 0 || matching as f64 / total as f64 >= 0.5 {
                        let (a, b) = block_span(&content_lines, i, j);
                        return vec![content[a..b].to_string()];
                    }
                }
                break;
            }
        }
    }
    Vec::new()
}

/// `MultiOccurrenceReplacer` (edit.ts:548-560): yield the literal search once
/// per occurrence so [`replace`] can honor `replace_all`.
pub(crate) fn multi_occurrence_replacer(content: &str, find: &str) -> Vec<String> {
    let mut out = Vec::new();
    if find.is_empty() {
        return out;
    }
    let mut start = 0usize;
    while let Some(rel) = content[start..].find(find) {
        let idx = start + rel;
        out.push(find.to_string());
        start = idx + find.len();
    }
    out
}

fn is_disproportionate_match(search: &str, old: &str) -> bool {
    let old_lines = old.split('\n').count();
    let search_lines = search.split('\n').count();
    if search_lines >= (old_lines + 3).max(old_lines * 2) {
        return true;
    }
    if old_lines == 1 {
        return false;
    }
    search.trim().len() > (old.trim().len() + 500).max(old.trim().len() * 4)
}

type Replacer = fn(&str, &str) -> Vec<String>;

const REPLACERS: &[Replacer] = &[
    simple_replacer,
    line_trimmed_replacer,
    block_anchor_replacer,
    whitespace_normalized_replacer,
    indentation_flexible_replacer,
    escape_normalized_replacer,
    trimmed_boundary_replacer,
    context_aware_replacer,
    multi_occurrence_replacer,
];

/// The core replacement engine (edit.ts:682-729). Walks the replacers in order;
/// the first non-disproportionate, unambiguous candidate is applied. `Err`
/// carries the model-facing message.
pub fn replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, ToolError> {
    if old_string == new_string {
        return Err(ToolError::Execution(ERR_IDENTICAL.to_string()));
    }
    if old_string.is_empty() {
        return Err(ToolError::Execution(ERR_EMPTY_EXISTING.to_string()));
    }

    let mut not_found = true;
    for replacer in REPLACERS {
        for search in replacer(content, old_string) {
            if search.is_empty() {
                continue;
            }
            let index = match content.find(&search) {
                Some(i) => i,
                None => continue,
            };
            not_found = false;
            if is_disproportionate_match(&search, old_string) {
                return Err(ToolError::Execution(ERR_DISPROPORTIONATE.to_string()));
            }
            if replace_all {
                return Ok(content.replace(&search, new_string));
            }
            let last = content.rfind(&search).unwrap_or(index);
            if index != last {
                continue;
            }
            return Ok(format!(
                "{}{}{}",
                &content[..index],
                new_string,
                &content[index + search.len()..]
            ));
        }
    }

    if not_found {
        Err(ToolError::Execution(ERR_NOT_FOUND.to_string()))
    } else {
        Err(ToolError::Execution(ERR_MULTIPLE.to_string()))
    }
}

// ---- per-file lock (edit.ts:35-45) ------------------------------------------

fn file_lock(path: &Path) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap();
    guard
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

// ---- the tool ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct EditParams {
    #[serde(rename = "filePath")]
    file_path: String,
    #[serde(rename = "oldString")]
    old_string: String,
    #[serde(rename = "newString")]
    new_string: String,
    #[serde(rename = "replaceAll", default)]
    replace_all: bool,
}

/// The `edit` tool (edit.ts:58-215).
#[derive(Clone, Default)]
pub struct EditTool {
    lsp: Option<Arc<dyn crate::LspHandle>>,
}

impl EditTool {
    /// Construct with an optional [`crate::LspHandle`]. When present, a
    /// `<diagnostics>` block is appended to the success output after the edit.
    pub fn new(lsp: Option<Arc<dyn crate::LspHandle>>) -> Self {
        Self { lsp }
    }
}

fn rel(ctx: &ToolContext, path: &Path) -> String {
    path.strip_prefix(&ctx.directory)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn id(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/edit.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "filePath": { "type": "string", "description": "The absolute path to the file to modify" },
                "oldString": { "type": "string", "description": "The text to replace" },
                "newString": { "type": "string", "description": "The text to replace it with (must be different from oldString)" },
                "replaceAll": { "type": "boolean", "description": "Replace all occurrences of oldString (default false)" }
            },
            "required": ["filePath", "oldString", "newString"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: EditParams = decode_args(self.id(), args)?;
        if params.file_path.is_empty() {
            return Err(ToolError::Execution("filePath is required".to_string()));
        }
        if params.old_string == params.new_string {
            return Err(ToolError::Execution(ERR_IDENTICAL.to_string()));
        }

        let path = resolve_path(ctx, &params.file_path);
        assert_external_directory(ctx, &path, "file").await?;

        let lock = file_lock(&path);
        let _guard = lock.lock().await;

        if params.old_string.is_empty() {
            if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                return Err(ToolError::Execution(ERR_EMPTY_EXISTING.to_string()));
            }
            ctx.permission
                .ask(PermissionRequest {
                    permission: "edit".to_string(),
                    patterns: vec![rel(ctx, &path)],
                    always: vec!["*".to_string()],
                    metadata: serde_json::json!({ "filepath": path.display().to_string() }),
                })
                .await?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&path, &params.new_string).await?;
            let mut output = String::from("Edit applied successfully.");
            self.append_diagnostics(&mut output, &path).await;
            return Ok(ExecuteResult::new(rel(ctx, &path), output).with_metadata(
                serde_json::json!({ "filepath": path.display().to_string(), "created": true }),
            ));
        }

        let meta = tokio::fs::metadata(&path).await;
        match meta {
            Err(_) => {
                return Err(ToolError::Execution(format!(
                    "File {} not found",
                    path.display()
                )));
            }
            Ok(m) if m.is_dir() => {
                return Err(ToolError::Execution(format!(
                    "Path is a directory, not a file: {}",
                    path.display()
                )));
            }
            Ok(_) => {}
        }

        let content_old = tokio::fs::read_to_string(&path).await?;
        let ending = detect_line_ending(&content_old);
        let old = convert_to_line_ending(&normalize_line_endings(&params.old_string), ending);
        let replacement =
            convert_to_line_ending(&normalize_line_endings(&params.new_string), ending);
        let content_new = replace(&content_old, &old, &replacement, params.replace_all)?;

        ctx.permission
            .ask(PermissionRequest {
                permission: "edit".to_string(),
                patterns: vec![rel(ctx, &path)],
                always: vec!["*".to_string()],
                metadata: serde_json::json!({ "filepath": path.display().to_string() }),
            })
            .await?;

        tokio::fs::write(&path, &content_new).await?;

        ctx.metadata.update(
            Some(rel(ctx, &path)),
            Some(serde_json::json!({ "filepath": path.display().to_string() })),
        );

        let mut output = String::from("Edit applied successfully.");
        self.append_diagnostics(&mut output, &path).await;

        Ok(ExecuteResult::new(rel(ctx, &path), output)
            .with_metadata(serde_json::json!({ "filepath": path.display().to_string() })))
    }
}

impl EditTool {
    /// Append the LSP `<diagnostics>` block for `path` to `output` when a handle
    /// is installed and the block is non-empty (mirrors `edit.ts:197-201`).
    async fn append_diagnostics(&self, output: &mut String, path: &Path) {
        if let Some(lsp) = &self.lsp {
            let block = lsp.report_for(path).await;
            if !block.is_empty() {
                output.push_str("\n\nLSP errors detected in this file, please fix:\n");
                output.push_str(&block);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::RecordingGate;
    use std::sync::Arc;

    fn present(content: &str, candidates: &[String]) -> bool {
        candidates.iter().any(|c| content.contains(c.as_str()))
    }

    #[test]
    fn simple_replacer_matches_exact() {
        let out = replace("hello world", "world", "there", false).unwrap();
        assert_eq!(out, "hello there");
    }

    #[test]
    fn line_trimmed_matches_where_simple_fails() {
        let content = "  foo\n  bar\n";
        // Simple's candidate is not literally present.
        assert!(!present(content, &simple_replacer(content, "foo\nbar")));
        let cands = line_trimmed_replacer(content, "foo\nbar");
        assert_eq!(cands, vec!["  foo\n  bar".to_string()]);
        let out = replace(content, "foo\nbar", "X", false).unwrap();
        assert_eq!(out, "X\n");
    }

    #[test]
    fn block_anchor_matches_where_line_trimmed_fails() {
        let content = "function foo() {\n  const x = 1;\n  return x;\n}";
        let find = "function foo() {\n  const y = 2;\n  return x;\n}";
        // Line-trimmed cannot match (middle line differs).
        assert!(line_trimmed_replacer(content, find).is_empty());
        let cands = block_anchor_replacer(content, find);
        assert_eq!(cands, vec![content.to_string()]);
        let out = replace(content, find, "REPLACED", false).unwrap();
        assert_eq!(out, "REPLACED");
    }

    #[test]
    fn whitespace_normalized_matches_where_simple_fails() {
        let content = "let   x    =    1;";
        assert!(!present(content, &simple_replacer(content, "let x = 1;")));
        let out = replace(content, "let x = 1;", "let y = 2;", false).unwrap();
        assert_eq!(out, "let y = 2;");
    }

    #[test]
    fn indentation_flexible_matches_where_simple_fails() {
        let content = "    a\n      b";
        let find = "a\n  b";
        assert!(!present(content, &simple_replacer(content, find)));
        let cands = indentation_flexible_replacer(content, find);
        assert_eq!(cands, vec![content.to_string()]);
    }

    #[test]
    fn escape_normalized_matches_literal_escapes() {
        let content = "line1\nline2";
        let find = "line1\\nline2"; // literal backslash-n
        assert!(!present(content, &simple_replacer(content, find)));
        let cands = escape_normalized_replacer(content, find);
        assert!(cands.iter().any(|c| c == "line1\nline2"));
    }

    #[test]
    fn trimmed_boundary_matches_where_simple_fails() {
        let content = "foobar";
        let find = "  foobar  ";
        assert!(!present(content, &simple_replacer(content, find)));
        let cands = trimmed_boundary_replacer(content, find);
        assert_eq!(cands.first().map(String::as_str), Some("foobar"));
    }

    #[test]
    fn context_aware_matches_half_middle() {
        let content = "start\n  aaa\n  bbb\nend";
        let find = "start\n  aaa\n  zzz\nend";
        let cands = context_aware_replacer(content, find);
        assert_eq!(cands, vec![content.to_string()]);
    }

    #[test]
    fn multi_occurrence_yields_each() {
        let cands = multi_occurrence_replacer("a a a", "a");
        assert_eq!(cands.len(), 3);
    }

    #[test]
    fn replace_all_replaces_every_occurrence() {
        let out = replace("a b a", "a", "X", true).unwrap();
        assert_eq!(out, "X b X");
    }

    #[test]
    fn ambiguous_match_errors() {
        let err = replace("a a", "a", "X", false).unwrap_err();
        assert_eq!(err.to_string(), ERR_MULTIPLE);
    }

    #[test]
    fn not_found_errors() {
        let err = replace("hello", "xyz", "q", false).unwrap_err();
        assert_eq!(err.to_string(), ERR_NOT_FOUND);
    }

    #[test]
    fn disproportionate_guard_logic() {
        // 5-line span vs 2-line old -> 5 >= max(5, 4) -> true.
        let big = "1\n2\n3\n4\n5";
        assert!(is_disproportionate_match(big, "a\nb"));
        // single-line old + single-line search: the length branch is skipped
        // (old_lines == 1), so even a very long span is allowed.
        let long_single = "z".repeat(9999);
        assert!(!is_disproportionate_match(&long_single, "a"));
        // 2-line span whose trimmed length exceeds 4x -> true (length branch).
        let long = format!("a\n{}", "z".repeat(600));
        assert!(is_disproportionate_match(&long, "a\nb"));
    }

    #[tokio::test]
    async fn create_new_file_when_old_string_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/new.txt");
        let ctx = ToolContext::builder(dir.path()).build();
        let args = serde_json::json!({
            "filePath": path.to_str().unwrap(),
            "oldString": "",
            "newString": "brand new\n"
        });
        EditTool::new(None).execute(args, &ctx).await.unwrap();
        let got = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(got, "brand new\n");
    }

    #[tokio::test]
    async fn empty_old_string_on_existing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exists.txt");
        tokio::fs::write(&path, "content").await.unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let args = serde_json::json!({
            "filePath": path.to_str().unwrap(),
            "oldString": "",
            "newString": "x"
        });
        let err = EditTool::new(None).execute(args, &ctx).await.unwrap_err();
        assert_eq!(err.to_string(), ERR_EMPTY_EXISTING);
    }

    #[tokio::test]
    async fn crlf_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.txt");
        tokio::fs::write(&path, "alpha\r\nbeta\r\ngamma")
            .await
            .unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let args = serde_json::json!({
            "filePath": path.to_str().unwrap(),
            "oldString": "beta",
            "newString": "BETA"
        });
        EditTool::new(None).execute(args, &ctx).await.unwrap();
        let got = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(got, "alpha\r\nBETA\r\ngamma");
    }

    #[tokio::test]
    async fn edit_appends_lsp_diagnostics() {
        use crate::lsp::LspHandle;
        use std::path::PathBuf;

        struct FakeLsp;
        #[async_trait::async_trait]
        impl LspHandle for FakeLsp {
            async fn report_for(&self, _p: &Path) -> String {
                "<diagnostics file=\"x\">\nERROR [1:1] boom\n</diagnostics>".into()
            }
            async fn other_files_with_errors(
                &self,
                _e: &Path,
                _m: usize,
            ) -> Vec<(PathBuf, String)> {
                vec![]
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        tokio::fs::write(&path, "hello world").await.unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let args = serde_json::json!({
            "filePath": path.to_str().unwrap(),
            "oldString": "world",
            "newString": "there"
        });
        let tool = EditTool::new(Some(Arc::new(FakeLsp)));
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(result.output.contains("LSP errors detected"));
        assert!(result.output.contains("ERROR [1:1] boom"));
    }

    #[tokio::test]
    async fn edit_asks_edit_permission() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        tokio::fs::write(&path, "hello world").await.unwrap();
        let gate = Arc::new(RecordingGate::allow());
        let ctx = ToolContext::builder(dir.path())
            .permission(gate.clone())
            .build();
        let args = serde_json::json!({
            "filePath": path.to_str().unwrap(),
            "oldString": "world",
            "newString": "there"
        });
        EditTool::new(None).execute(args, &ctx).await.unwrap();
        assert!(gate.asked_for("edit"));
        let got = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(got, "hello there");
    }
}
