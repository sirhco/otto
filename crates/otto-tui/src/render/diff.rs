//! Rendering unified diffs as styled ratatui lines.

use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// Colorize a unified diff via theme tokens. `+` add, `-` del, `@@` hunk,
/// else context. One span per line (never per-char).
#[must_use]
pub fn render_diff(diff: &str, theme: &Theme) -> Vec<Line<'static>> {
    diff.lines()
        .map(|line| {
            let style = if line.starts_with('+') {
                theme.diff_add
            } else if line.starts_with('-') {
                theme.diff_del
            } else if line.starts_with("@@") {
                theme.diff_hunk
            } else {
                theme.diff_ctx
            };
            Line::from(Span::styled(line.to_string(), style))
        })
        .collect()
}

/// A unified diff between `old` and `new` via `similar`.
#[must_use]
pub fn compute_diff(old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .header("old", "new")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn colorizes_add_remove_context() {
        use ratatui::style::Color;
        let theme = crate::theme::Theme::dark();
        let diff = "@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n";
        let out = render_diff(diff, &theme);
        assert_eq!(texts(&out), vec!["@@ -1,2 +1,2 @@", " ctx", "-old", "+new"]);
        assert_eq!(out[2].spans[0].style.fg, Some(Color::Red));
        assert_eq!(out[3].spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn compute_diff_produces_unified() {
        let d = compute_diff("a\nb\n", "a\nc\n");
        assert!(d.contains("-b"), "diff: {d}");
        assert!(d.contains("+c"), "diff: {d}");
    }
}
