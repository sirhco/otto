//! Rendering a markdown string as styled ratatui lines (terminal-flavored).

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Render markdown to terminal lines. A small visitor over pulldown-cmark
/// events; unknown/rich constructs (tables, images) degrade to their text.
#[must_use]
pub fn render_markdown(src: &str) -> Vec<Line<'static>> {
    let parser = Parser::new_ext(src, Options::empty());
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut list_depth: usize = 0;
    let mut in_code_block = false;
    let mut code_buf = String::new();
    let mut code_lang: Option<String> = None;

    let flush = |lines: &mut Vec<Line<'static>>, cur: &mut Vec<Span<'static>>| {
        if !cur.is_empty() {
            lines.push(Line::from(std::mem::take(cur)));
        }
    };

    for ev in parser {
        match ev {
            Event::Start(Tag::Heading { .. }) => style = style.add_modifier(Modifier::BOLD),
            Event::End(TagEnd::Heading(_)) => {
                flush(&mut lines, &mut cur);
                style = Style::default();
                lines.push(Line::from(""));
            }
            Event::Start(Tag::Emphasis) => style = style.add_modifier(Modifier::ITALIC),
            Event::End(TagEnd::Emphasis) => style = style.remove_modifier(Modifier::ITALIC),
            Event::Start(Tag::Strong) => style = style.add_modifier(Modifier::BOLD),
            Event::End(TagEnd::Strong) => style = style.remove_modifier(Modifier::BOLD),
            Event::Start(Tag::List(_)) => list_depth += 1,
            Event::End(TagEnd::List(_)) => list_depth = list_depth.saturating_sub(1),
            Event::Start(Tag::Item) => {
                flush(&mut lines, &mut cur);
                cur.push(Span::raw(format!(
                    "{}• ",
                    "  ".repeat(list_depth.saturating_sub(1))
                )));
            }
            Event::End(TagEnd::Item) => flush(&mut lines, &mut cur),
            Event::Start(Tag::BlockQuote(_)) => {
                cur.push(Span::styled(
                    "▏ ",
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush(&mut lines, &mut cur);
                in_code_block = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(l) if !l.is_empty() => Some(l.to_string()),
                    _ => None,
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                lines.extend(render_code_block(&code_buf, code_lang.as_deref()));
                in_code_block = false;
                code_lang = None;
            }
            Event::Text(t) => {
                if in_code_block {
                    code_buf.push_str(&t);
                } else {
                    cur.push(Span::styled(t.to_string(), style));
                }
            }
            Event::Code(t) => {
                cur.push(Span::styled(
                    t.to_string(),
                    Style::default().add_modifier(Modifier::REVERSED),
                ));
            }
            Event::SoftBreak | Event::HardBreak => flush(&mut lines, &mut cur),
            Event::End(TagEnd::Paragraph) => {
                flush(&mut lines, &mut cur);
                lines.push(Line::from(""));
            }
            _ => {}
        }
    }
    // Mid-stream: an unterminated code fence leaves buffered code — emit it.
    if in_code_block && !code_buf.is_empty() {
        lines.extend(render_code_block(&code_buf, code_lang.as_deref()));
    }
    flush(&mut lines, &mut cur);
    lines
}

/// Delegates to the syntax-highlight seam, then adds a two-space gutter
/// indent (the seam itself stays indent-free so it composes cleanly).
fn render_code_block(code: &str, lang: Option<&str>) -> Vec<Line<'static>> {
    crate::render::highlight::highlight(code, lang)
        .into_iter()
        .map(|line| {
            let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn heading_and_paragraph() {
        let out = render_markdown("# Title\n\nhello world");
        let text = joined(&out);
        assert!(text.iter().any(|l| l.contains("Title")));
        assert!(text.iter().any(|l| l.contains("hello world")));
        // heading line has a bold span.
        let h = out
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("Title")))
            .unwrap();
        assert!(
            h.spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn bullet_list_and_inline_code() {
        let out = render_markdown("- one\n- two\n\nuse `cargo test` now");
        let text = joined(&out).join("\n");
        assert!(text.contains("one") && text.contains("two"));
        assert!(text.contains("cargo test"));
    }

    #[test]
    fn incomplete_code_fence_does_not_panic() {
        // A fence opened but not closed (mid-stream) must render, not panic.
        let out = render_markdown("text before\n```rust\nlet x = 1;");
        assert!(joined(&out).join("\n").contains("let x = 1;"));
    }
}
