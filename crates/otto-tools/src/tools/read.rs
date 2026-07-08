//! The `read` tool — a port of opencode `packages/opencode/src/tool/read.ts`.
//!
//! Reads a file (or directory listing) resolving relative paths against
//! `ctx.directory`, emitting line-numbered content in the `<path>/<type>/
//! <content>` envelope with the same offset/limit/byte-cap accounting as
//! `read.ts:137-351`.

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::{assert_external_directory, resolve_path};
use crate::tool::{Attachment, ExecuteResult, Tool, ToolContext, ToolError, decode_args};

const DEFAULT_READ_LIMIT: usize = 2000;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;
const MAX_BYTES_LABEL: &str = "50 KB";
const SAMPLE_BYTES: usize = 4096;

#[derive(Debug, Deserialize)]
struct ReadParams {
    #[serde(rename = "filePath")]
    file_path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

/// The `read` tool (read.ts:64).
#[derive(Debug, Default, Clone, Copy)]
pub struct ReadTool;

fn image_mime(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

/// Heuristic binary sniff (read.ts:182-227): known binary extensions, NUL bytes,
/// or >30% non-printable bytes in the sample.
fn is_binary(path: &Path, sample: &[u8]) -> bool {
    const BINARY_EXT: &[&str] = &[
        "zip", "tar", "gz", "exe", "dll", "so", "class", "jar", "war", "7z", "doc", "docx", "xls",
        "xlsx", "ppt", "pptx", "odt", "ods", "odp", "bin", "dat", "obj", "o", "a", "lib", "wasm",
        "pyc", "pyo",
    ];
    if let Some(ext) = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        && BINARY_EXT.contains(&ext.as_str())
    {
        return true;
    }
    if sample.is_empty() {
        return false;
    }
    let mut non_printable = 0usize;
    for &b in sample {
        if b == 0 {
            return true;
        }
        if b < 9 || (b > 13 && b < 32) {
            non_printable += 1;
        }
    }
    non_printable as f64 / sample.len() as f64 > 0.3
}

/// Split content into logical lines, dropping a single trailing empty produced
/// by a terminating newline; an empty file has zero lines (read.ts streaming).
fn logical_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut v: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        v.pop();
    }
    v
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn id(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/read.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "filePath": { "type": "string", "description": "The absolute path to the file or directory to read" },
                "offset": { "type": "number", "description": "The line number to start reading from (1-indexed)" },
                "limit": { "type": "number", "description": "The maximum number of lines to read (defaults to 2000)" }
            },
            "required": ["filePath"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: ReadParams = decode_args(self.id(), args)?;
        let path = resolve_path(ctx, &params.file_path);

        let meta = tokio::fs::metadata(&path).await.ok();
        let kind = if matches!(&meta, Some(m) if m.is_dir()) {
            "directory"
        } else {
            "file"
        };
        assert_external_directory(ctx, &path, kind).await?;

        let title = path
            .strip_prefix(&ctx.directory)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());

        let Some(meta) = meta else {
            return Err(ToolError::Execution(format!(
                "File not found: {}",
                path.display()
            )));
        };

        if meta.is_dir() {
            return read_directory(&path, &title, params.offset, params.limit).await;
        }

        // Image / PDF attachment path (read.ts:304-325).
        if let Some(mime) = image_mime(&path) {
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string);
            return Ok(ExecuteResult::new(title, "Image read successfully")
                .with_metadata(serde_json::json!({ "truncated": false }))
                .with_attachment(Attachment {
                    mime: mime.to_string(),
                    filename,
                    url: format!("file://{}", path.display()),
                }));
        }

        let bytes = tokio::fs::read(&path).await?;
        let sample = &bytes[..bytes.len().min(SAMPLE_BYTES)];
        if is_binary(&path, sample) {
            return Err(ToolError::Execution(format!(
                "Cannot read binary file: {}",
                path.display()
            )));
        }

        let content = String::from_utf8_lossy(&bytes);
        let all = logical_lines(&content);
        let count = all.len();
        let offset = params.offset.unwrap_or(1).max(1);
        let limit = params.limit.unwrap_or(DEFAULT_READ_LIMIT);

        if count < offset && !(count == 0 && offset == 1) {
            return Err(ToolError::Execution(format!(
                "Offset {offset} is out of range for this file ({count} lines)"
            )));
        }

        let start = offset - 1;
        let mut raw: Vec<String> = Vec::new();
        let mut bytes_used = 0usize;
        let mut cut = false;
        let mut more = false;
        for line in all.iter().skip(start) {
            if raw.len() >= limit {
                more = true;
                break;
            }
            let line: String = if line.chars().count() > MAX_LINE_LENGTH {
                let truncated: String = line.chars().take(MAX_LINE_LENGTH).collect();
                format!("{truncated}... (line truncated to {MAX_LINE_LENGTH} chars)")
            } else {
                (*line).to_string()
            };
            let size = line.len() + usize::from(!raw.is_empty());
            if bytes_used + size > MAX_BYTES {
                cut = true;
                more = true;
                break;
            }
            bytes_used += size;
            raw.push(line);
        }

        let mut output = format!(
            "<path>{}</path>\n<type>file</type>\n<content>\n",
            path.display()
        );
        output.push_str(
            &raw.iter()
                .enumerate()
                .map(|(i, line)| format!("{}: {line}", i + offset))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        let last = offset + raw.len() - 1;
        let next = last + 1;
        let truncated = more || cut;
        if cut {
            output.push_str(&format!(
                "\n\n(Output capped at {MAX_BYTES_LABEL}. Showing lines {offset}-{last}. Use offset={next} to continue.)"
            ));
        } else if more {
            output.push_str(&format!(
                "\n\n(Showing lines {offset}-{last} of {count}. Use offset={next} to continue.)"
            ));
        } else {
            output.push_str(&format!("\n\n(End of file - total {count} lines)"));
        }
        output.push_str("\n</content>");

        Ok(
            ExecuteResult::new(title, output).with_metadata(serde_json::json!({
                "truncated": truncated,
            })),
        )
    }
}

async fn read_directory(
    path: &Path,
    title: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<ExecuteResult, ToolError> {
    let mut entries: Vec<String> = Vec::new();
    let mut rd = tokio::fs::read_dir(path).await?;
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
        entries.push(if is_dir { format!("{name}/") } else { name });
    }
    entries.sort();

    let limit = limit.unwrap_or(DEFAULT_READ_LIMIT);
    let offset = offset.unwrap_or(1).max(1);
    let start = offset - 1;
    let sliced: Vec<String> = entries.iter().skip(start).take(limit).cloned().collect();
    let truncated = start + sliced.len() < entries.len();

    let footer = if truncated {
        format!(
            "\n(Showing {} of {} entries. Use 'offset' parameter to read beyond entry {})",
            sliced.len(),
            entries.len(),
            offset + sliced.len()
        )
    } else {
        format!("\n({} entries)", entries.len())
    };

    let output = [
        format!("<path>{}</path>", path.display()),
        "<type>directory</type>".to_string(),
        "<entries>".to_string(),
        sliced.join("\n"),
        footer,
        "</entries>".to_string(),
    ]
    .join("\n");

    Ok(ExecuteResult::new(title.to_string(), output)
        .with_metadata(serde_json::json!({ "truncated": truncated })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_file_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        tokio::fs::write(&path, "alpha\nbeta\ngamma\n")
            .await
            .unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let res = ReadTool
            .execute(
                serde_json::json!({ "filePath": path.to_str().unwrap() }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(res.output.contains("1: alpha"));
        assert!(res.output.contains("2: beta"));
        assert!(res.output.contains("3: gamma"));
        assert!(res.output.contains("(End of file - total 3 lines)"));
        assert!(res.output.contains("<type>file</type>"));
    }

    #[tokio::test]
    async fn offset_and_limit_slice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        let body = (1..=10)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        tokio::fs::write(&path, body).await.unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let res = ReadTool
            .execute(
                serde_json::json!({ "filePath": path.to_str().unwrap(), "offset": 3, "limit": 2 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(res.output.contains("3: 3"));
        assert!(res.output.contains("4: 4"));
        assert!(!res.output.contains("5: 5"));
        assert!(res.output.contains("Use offset=5 to continue"));
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let missing = dir.path().join("nope.txt");
        let err = ReadTool
            .execute(
                serde_json::json!({ "filePath": missing.to_str().unwrap() }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("File not found:"));
    }
}
