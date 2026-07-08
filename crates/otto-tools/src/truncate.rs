//! Output truncation — a port of opencode `packages/opencode/src/tool/truncate.ts`.
//!
//! The registry applies [`truncate_output`] to a tool's output unless the tool
//! already set a `truncated` key in its metadata (the opt-out at
//! `tool.ts:131-133`). The line/byte accounting here mirrors the `head`
//! direction of `truncate.ts:85-141` exactly.
//!
//! Phase 2 note: opencode writes the full untruncated text to a file under its
//! truncation dir and points the hint at that path. That file write is left as
//! a TODO; we return the preview + marker + hint without persisting the full
//! output. See `outputPath` in `truncate.ts:20`.

/// Default maximum number of lines (`MAX_LINES` in `truncate.ts:15`).
pub const MAX_LINES: usize = 2000;

/// Default maximum number of bytes (`MAX_BYTES` in `truncate.ts:16`).
pub const MAX_BYTES: usize = 50 * 1024;

/// The outcome of a truncation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncated {
    /// The (possibly truncated) content to surface to the model.
    pub content: String,
    /// Whether truncation occurred.
    pub truncated: bool,
    // TODO(phase-2+): `output_path: Option<PathBuf>` once the full-output file
    // write from truncate.ts is ported.
}

/// Truncate `text` to at most `max_lines` lines and `max_bytes` bytes using the
/// `head` direction, appending a `...N lines/bytes truncated...` marker and a
/// hint. `has_task_tool` selects between the two hint variants
/// (`truncate.ts:129-131`).
///
/// When the text already fits, it is returned unchanged with
/// `truncated == false`.
pub fn truncate_output(
    text: &str,
    max_lines: usize,
    max_bytes: usize,
    has_task_tool: bool,
) -> Truncated {
    let lines: Vec<&str> = text.split('\n').collect();
    let total_bytes = text.len();

    if lines.len() <= max_lines && total_bytes <= max_bytes {
        return Truncated {
            content: text.to_string(),
            truncated: false,
        };
    }

    let mut out: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut hit_bytes = false;

    // head direction (truncate.ts:102-111)
    for (i, line) in lines.iter().enumerate() {
        if i >= max_lines {
            break;
        }
        let size = line.len() + usize::from(i > 0);
        if bytes + size > max_bytes {
            hit_bytes = true;
            break;
        }
        out.push(line);
        bytes += size;
    }

    let removed = if hit_bytes {
        total_bytes - bytes
    } else {
        lines.len() - out.len()
    };
    let unit = if hit_bytes { "bytes" } else { "lines" };
    let preview = out.join("\n");

    let hint = if has_task_tool {
        "The tool call succeeded but the output was truncated.\nUse the Task tool to have the explore agent process the full output with Grep and Read (with offset/limit). Do NOT read the full output yourself - delegate to save context."
    } else {
        "The tool call succeeded but the output was truncated.\nUse Grep to search the full content or Read with offset/limit to view specific sections."
    };

    Truncated {
        content: format!("{preview}\n\n...{removed} {unit} truncated...\n\n{hint}"),
        truncated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_within_limits_is_untouched() {
        let r = truncate_output("a\nb\nc", MAX_LINES, MAX_BYTES, false);
        assert!(!r.truncated);
        assert_eq!(r.content, "a\nb\nc");
    }

    #[test]
    fn line_accounting_and_grep_hint() {
        let text = (1..=10)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_output(&text, 4, MAX_BYTES, false);
        assert!(r.truncated);
        // head: first 4 lines
        assert!(r.content.starts_with("1\n2\n3\n4\n\n"));
        // 10 - 4 = 6 lines removed
        assert!(r.content.contains("...6 lines truncated..."));
        assert!(
            r.content
                .contains("Use Grep to search the full content or Read with offset/limit")
        );
    }

    #[test]
    fn byte_accounting_and_task_hint() {
        // 5 lines of 10 'x' each; cap bytes so only ~2 lines fit.
        let text = ["xxxxxxxxxx"; 5].join("\n"); // 54 bytes total
        // maxBytes 20: line0=10, line1=1+10=11 -> 21 > 20 -> stop after line0
        let r = truncate_output(&text, MAX_LINES, 20, true);
        assert!(r.truncated);
        assert_eq!(r.content.split("\n\n").next().unwrap(), "xxxxxxxxxx");
        // removed bytes = 54 - 10 = 44
        assert!(r.content.contains("...44 bytes truncated..."));
        assert!(r.content.contains("Use the Task tool"));
    }
}
