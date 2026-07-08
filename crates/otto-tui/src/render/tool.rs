//! Rendering a tool-call transcript row (collapsed marker line, or expanded
//! with args + output/diff).

use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::render::diff::{compute_diff, render_diff};
use crate::state::ToolStatus;
use crate::theme::Theme;

#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn render_tool(
    name: &str,
    status: &ToolStatus,
    title: &str,
    input: &Option<Value>,
    output: &Option<String>,
    expanded: bool,
    selected: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let (sym, sym_style) = match status {
        ToolStatus::Running => ('⋯', theme.status_warn),
        ToolStatus::Ok => ('✓', theme.status_ok),
        ToolStatus::Error => ('✗', theme.status_err),
    };
    let caret = if expanded { '▾' } else { '▸' };
    let (prefix, prefix_style) = if selected {
        (format!("▌ {caret} "), theme.accent)
    } else {
        (format!("  {caret} "), theme.text)
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(prefix, prefix_style),
        Span::styled(sym.to_string(), sym_style),
        Span::styled(format!(" {title}"), theme.text),
    ])];
    if !expanded {
        return lines;
    }
    if matches!(name, "edit" | "write" | "apply_patch")
        && let Some(diff) = diff_for(name, input, output)
    {
        lines.extend(indent(render_diff(&diff, theme)));
        return lines;
    }
    if let Some(o) = output {
        for l in o.lines().take(40) {
            lines.push(Line::from(Span::styled(
                format!("      {l}"),
                theme.text_muted,
            )));
        }
    } else if let Some(i) = input {
        lines.push(Line::from(Span::styled(
            format!("      {i}"),
            theme.text_muted,
        )));
    }
    lines
}

/// Reconstruct a unified diff: prefer an `oldString`/`newString` edit input,
/// else treat a unified-diff-looking `output` as the diff itself.
fn diff_for(name: &str, input: &Option<Value>, output: &Option<String>) -> Option<String> {
    if name == "edit"
        && let Some(i) = input
    {
        let old = i.get("oldString").and_then(Value::as_str);
        let new = i.get("newString").and_then(Value::as_str);
        if let (Some(o), Some(n)) = (old, new) {
            return Some(compute_diff(o, n));
        }
    }
    output
        .as_ref()
        .filter(|o| o.contains("@@") || o.starts_with("---"))
        .cloned()
}

fn indent(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|mut l| {
            l.spans.insert(0, Span::raw("      "));
            l
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn collapsed_shows_marker_and_title() {
        let theme = crate::theme::Theme::dark();
        let out = render_tool(
            "read",
            &ToolStatus::Ok,
            "read a.rs",
            &None,
            &None,
            false,
            false,
            &theme,
        );
        let text = joined(&out);
        assert!(text.contains("read a.rs"));
        assert!(text.contains('▸'), "collapsed marker: {text:?}");
        assert_eq!(out.len(), 1, "collapsed is one line");
    }

    #[test]
    fn ok_glyph_is_green_error_glyph_is_red() {
        use ratatui::style::Color;
        let theme = crate::theme::Theme::dark();
        let ok = render_tool(
            "read",
            &ToolStatus::Ok,
            "t",
            &None,
            &None,
            false,
            false,
            &theme,
        );
        let err = render_tool(
            "read",
            &ToolStatus::Error,
            "t",
            &None,
            &None,
            false,
            false,
            &theme,
        );
        let glyph = |ls: &[Line]| {
            ls[0]
                .spans
                .iter()
                .find(|s| s.content == "✓" || s.content == "✗")
                .map(|s| s.style.fg)
                .unwrap()
        };
        assert_eq!(glyph(&ok), Some(Color::Green));
        assert_eq!(glyph(&err), Some(Color::Red));
    }

    #[test]
    fn expanded_edit_shows_diff() {
        let theme = crate::theme::Theme::dark();
        let input =
            serde_json::json!({ "filePath": "a.rs", "oldString": "a\nb\n", "newString": "a\nc\n" });
        let out = render_tool(
            "edit",
            &ToolStatus::Ok,
            "edit a.rs",
            &Some(input),
            &None,
            true,
            false,
            &theme,
        );
        let text = joined(&out);
        assert!(text.contains('▾'), "expanded marker");
        assert!(
            text.contains("-b") && text.contains("+c"),
            "shows a diff: {text}"
        );
    }

    #[test]
    fn expanded_generic_shows_output() {
        let theme = crate::theme::Theme::dark();
        let out = render_tool(
            "bash",
            &ToolStatus::Ok,
            "bash ls",
            &None,
            &Some("file1\nfile2".into()),
            true,
            false,
            &theme,
        );
        assert!(joined(&out).contains("file1"));
    }

    #[test]
    fn selected_row_shows_accent_bar() {
        use ratatui::style::Color;
        let theme = crate::theme::Theme::dark();
        let out = render_tool(
            "read",
            &ToolStatus::Ok,
            "read a.rs",
            &None,
            &None,
            false,
            true,
            &theme,
        );
        let first = &out[0].spans[0];
        assert!(
            first.content.starts_with('▌'),
            "selected marker: {:?}",
            first.content
        );
        assert_eq!(first.style.fg, Some(Color::Cyan)); // theme.accent
        // Unselected keeps the two-space indent.
        let plain = render_tool(
            "read",
            &ToolStatus::Ok,
            "read a.rs",
            &None,
            &None,
            false,
            false,
            &theme,
        );
        assert_eq!(plain[0].spans[0].content.as_ref(), "  ▸ ");
    }
}
