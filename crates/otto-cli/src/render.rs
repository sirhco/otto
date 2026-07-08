//! Terminal rendering of the streaming [`LLMEvent`] feed.
//!
//! [`Renderer`] folds a sequence of [`LLMEvent`]s into human-facing terminal
//! output: assistant text is streamed inline, reasoning is dimmed, each tool
//! call is announced with a `⏺` marker and a short argument summary, tool
//! results collapse to a short line or a `✓`, and a token-usage footer is
//! printed once the stream ends.
//!
//! The renderer is deliberately decoupled from any real terminal: it writes to
//! an arbitrary [`Write`] and its ANSI styling is gated on a `color` flag, so a
//! test can feed a scripted `Vec<LLMEvent>` into a `Vec<u8>` and assert on the
//! produced string. Colour is suppressed when `NO_COLOR` is set (see
//! [`color_enabled`]).

use std::io::{self, Write};

use otto_events::{LLMEvent, ToolResultValue, Usage};
use serde_json::Value;

/// ANSI dim (used for reasoning + the footer).
const DIM: &str = "\x1b[2m";
/// ANSI cyan (used for the tool-call marker).
const CYAN: &str = "\x1b[36m";
/// ANSI red (used for errors).
const RED: &str = "\x1b[31m";
/// ANSI reset.
const RESET: &str = "\x1b[0m";

/// Maximum length of a rendered argument / result summary before truncation.
const SUMMARY_MAX: usize = 72;

/// Whether ANSI colour should be emitted.
///
/// Honours the [`NO_COLOR`](https://no-color.org) convention: any value of the
/// `NO_COLOR` environment variable disables colour. Callers additionally gate
/// this on whether stdout is a TTY.
#[must_use]
pub fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

/// Renders a stream of [`LLMEvent`]s to a [`Write`] sink.
///
/// Feed events one at a time with [`Renderer::handle`]; call
/// [`Renderer::finish`] once the stream closes to flush the trailing newline
/// and the token footer.
pub struct Renderer<W: Write> {
    out: W,
    color: bool,
    /// Whether any assistant text has been written (controls the trailing
    /// newline before markers / the footer).
    wrote_text: bool,
    /// The most recent usage seen on a `step-finish` / `finish` event; rendered
    /// as the footer.
    usage: Option<Usage>,
}

impl<W: Write> Renderer<W> {
    /// Build a renderer writing to `out`. When `color` is false no ANSI escape
    /// sequences are emitted.
    pub fn new(out: W, color: bool) -> Self {
        Self {
            out,
            color,
            wrote_text: false,
            usage: None,
        }
    }

    /// Wrap `text` in the ANSI `code` when colour is enabled, else return it
    /// unchanged.
    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    /// Handle a single streamed event, writing any output it produces.
    ///
    /// # Errors
    /// Propagates any [`io::Error`] from the underlying writer.
    pub fn handle(&mut self, event: &LLMEvent) -> io::Result<()> {
        match event {
            LLMEvent::TextDelta { text, .. } => {
                write!(self.out, "{text}")?;
                if !text.is_empty() {
                    self.wrote_text = true;
                }
            }
            LLMEvent::ReasoningDelta { text, .. } => {
                let painted = self.paint(DIM, text);
                write!(self.out, "{painted}")?;
            }
            LLMEvent::ToolCall { name, input, .. } => {
                self.break_line()?;
                let marker = self.paint(CYAN, "⏺");
                writeln!(self.out, "{marker} {name}({})", summarize_value(input))?;
            }
            LLMEvent::ToolResult { name, result, .. } => {
                let summary = summarize_result(result);
                let body = if summary.is_empty() {
                    "✓".to_string()
                } else {
                    summary
                };
                let line = self.paint(DIM, &format!("  ⎿ {name}: {body}"));
                writeln!(self.out, "{line}")?;
            }
            LLMEvent::ToolError { name, message, .. } => {
                let line = self.paint(RED, &format!("  ⎿ {name}: error: {message}"));
                writeln!(self.out, "{line}")?;
            }
            LLMEvent::ProviderError { message, .. } => {
                self.break_line()?;
                let line = self.paint(RED, &format!("provider error: {message}"));
                writeln!(self.out, "{line}")?;
            }
            LLMEvent::StepFinish {
                usage: Some(usage), ..
            }
            | LLMEvent::Finish {
                usage: Some(usage), ..
            } => {
                self.usage = Some(usage.clone());
            }
            // Block open/close + tool-input streaming are structural; nothing
            // to render for them here.
            _ => {}
        }
        Ok(())
    }

    /// Emit a newline only if the last thing written was inline text, so
    /// markers/footers start on their own line.
    fn break_line(&mut self) -> io::Result<()> {
        if self.wrote_text {
            writeln!(self.out)?;
            self.wrote_text = false;
        }
        Ok(())
    }

    /// Flush the trailing newline and the token-usage footer (if any usage was
    /// reported). Call once the event stream has closed.
    ///
    /// # Errors
    /// Propagates any [`io::Error`] from the underlying writer.
    pub fn finish(&mut self) -> io::Result<()> {
        self.break_line()?;
        if let Some(usage) = self.usage.clone() {
            let footer = format_footer(&usage);
            let line = self.paint(DIM, &footer);
            writeln!(self.out, "{line}")?;
        }
        self.out.flush()
    }

    /// Consume the renderer, returning the underlying writer.
    pub fn into_inner(self) -> W {
        self.out
    }
}

/// Format the token footer from a [`Usage`], e.g.
/// `tokens: 120 in / 45 out (12 reasoning)`.
fn format_footer(usage: &Usage) -> String {
    let input = usage.input_tokens.unwrap_or(0);
    let output = usage.visible_output_tokens();
    let mut footer = format!("tokens: {input} in / {output} out");
    let reasoning = usage.reasoning_tokens.unwrap_or(0);
    if reasoning > 0 {
        footer.push_str(&format!(" ({reasoning} reasoning)"));
    }
    footer
}

/// Truncate `s` to [`SUMMARY_MAX`] chars, appending an ellipsis when cut.
fn truncate(s: &str) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > SUMMARY_MAX {
        let head: String = s.chars().take(SUMMARY_MAX).collect();
        format!("{head}…")
    } else {
        s
    }
}

/// Summarize a tool-call input value into a compact single line.
fn summarize_value(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .take(4)
                .map(|(k, val)| format!("{k}: {}", scalar(val)))
                .collect();
            truncate(&parts.join(", "))
        }
        other => truncate(&scalar(other)),
    }
}

/// Render a JSON scalar compactly (strings unquoted, containers abbreviated).
fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(_) => "[…]".into(),
        Value::Object(_) => "{…}".into(),
    }
}

/// Summarize a [`ToolResultValue`] into a compact single line.
fn summarize_result(result: &ToolResultValue) -> String {
    let value = match result {
        ToolResultValue::Json { value }
        | ToolResultValue::Text { value }
        | ToolResultValue::Error { value } => value.clone(),
        ToolResultValue::Content { value } => Value::Array(value.clone()),
    };
    match &value {
        Value::String(s) => truncate(s),
        Value::Null => String::new(),
        other => truncate(&other.to_string()),
    }
}

/// Render a scripted slice of events to a `String` — a convenience for tests.
#[must_use]
pub fn render_to_string(events: &[LLMEvent], color: bool) -> String {
    let mut renderer = Renderer::new(Vec::<u8>::new(), color);
    for event in events {
        renderer.handle(event).expect("vec writer is infallible");
    }
    renderer.finish().expect("vec writer is infallible");
    String::from_utf8(renderer.into_inner()).expect("utf-8 output")
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_events::FinishReason;
    use serde_json::json;

    /// A scripted turn: step-start, text ×2, a tool call + result, then
    /// step-finish (with usage) and finish.
    fn scripted() -> Vec<LLMEvent> {
        vec![
            LLMEvent::StepStart { index: 0 },
            LLMEvent::TextStart {
                id: "t1".into(),
                provider_metadata: None,
            },
            LLMEvent::TextDelta {
                id: "t1".into(),
                text: "Hello, ".into(),
                provider_metadata: None,
            },
            LLMEvent::TextDelta {
                id: "t1".into(),
                text: "world".into(),
                provider_metadata: None,
            },
            LLMEvent::TextEnd {
                id: "t1".into(),
                provider_metadata: None,
            },
            LLMEvent::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                input: json!({ "path": "src/main.rs" }),
                provider_executed: None,
                provider_metadata: None,
            },
            LLMEvent::ToolResult {
                id: "c1".into(),
                name: "read".into(),
                result: ToolResultValue::Text {
                    value: json!("fn main() {}"),
                },
                output: None,
                provider_executed: None,
                provider_metadata: None,
            },
            LLMEvent::StepFinish {
                index: 0,
                reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: Some(120),
                    output_tokens: Some(45),
                    ..Usage::default()
                }),
                provider_metadata: None,
            },
            LLMEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
                provider_metadata: None,
            },
        ]
    }

    #[test]
    fn renders_text_tool_marker_and_footer() {
        let out = render_to_string(&scripted(), false);
        assert!(out.contains("Hello, world"), "concatenated text: {out}");
        assert!(out.contains("⏺ read("), "tool marker: {out}");
        assert!(out.contains("path: src/main.rs"), "arg summary: {out}");
        assert!(out.contains("read: fn main() {}"), "tool result: {out}");
        assert!(out.contains("tokens: 120 in / 45 out"), "footer: {out}");
    }

    #[test]
    fn no_color_suppresses_ansi() {
        let plain = render_to_string(&scripted(), false);
        assert!(!plain.contains('\x1b'), "no ANSI when color off: {plain:?}");

        let colored = render_to_string(&scripted(), true);
        assert!(colored.contains('\x1b'), "ANSI present when color on");
    }

    #[test]
    fn empty_tool_result_shows_check() {
        let events = vec![
            LLMEvent::ToolCall {
                id: "c1".into(),
                name: "write".into(),
                input: json!({}),
                provider_executed: None,
                provider_metadata: None,
            },
            LLMEvent::ToolResult {
                id: "c1".into(),
                name: "write".into(),
                result: ToolResultValue::Text { value: Value::Null },
                output: None,
                provider_executed: None,
                provider_metadata: None,
            },
        ];
        let out = render_to_string(&events, false);
        assert!(out.contains("write: ✓"), "check mark: {out}");
    }

    #[test]
    fn reasoning_is_dimmed_when_color_on() {
        let events = vec![LLMEvent::ReasoningDelta {
            id: "r1".into(),
            text: "thinking".into(),
            provider_metadata: None,
        }];
        let colored = render_to_string(&events, true);
        assert!(colored.contains(DIM), "reasoning dimmed");
        assert!(colored.contains("thinking"));
        let plain = render_to_string(&events, false);
        assert_eq!(plain, "thinking");
    }
}
