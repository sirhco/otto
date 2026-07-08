//! Auto-naming a session from its first prompt.
//!
//! [`generate_session_title`] makes one non-streaming LLM call to summarize the
//! user's first message into a 3–5 word title. It is best-effort: any failure
//! (route error, empty/garbage output) yields `None` and the caller keeps the
//! session's default name. The parsing/cleanup is factored into the pure
//! [`clean_title`] so it can be unit-tested without a live model.

use std::sync::Arc;

use otto_llm::request::GenerationOptions;
use otto_llm::{ContentPart, LLMClient, LLMRequest, Message, Model, Route, SystemPart};

/// Instruction for the one-shot title call.
const TITLE_SYSTEM: &str = "You generate a very short title (3 to 5 words) that \
summarizes a conversation, based on the user's first message. Reply with ONLY \
the title — no quotes, no surrounding punctuation, no preamble. Use Title Case.";

/// Cap on how much of the first message is sent (a title needs only the gist,
/// and this keeps the call cheap on a long paste).
const MAX_INPUT_CHARS: usize = 800;
/// Cap on the stored title length.
const MAX_TITLE_CHARS: usize = 50;

/// Placeholder titles a fresh session carries before it is auto-named. Only a
/// session still wearing one of these is eligible for auto-naming, so a title
/// the user set explicitly (or a title already generated) is never clobbered.
const DEFAULT_TITLES: [&str; 3] = ["New session", "New Session", "otto"];

/// Whether `title` is a placeholder eligible to be replaced by an auto-name.
#[must_use]
pub(crate) fn is_default_session_title(title: &str) -> bool {
    DEFAULT_TITLES.contains(&title.trim())
}

/// Generate a short session title from `first_message`, or `None` on any
/// failure. Uses the session's own `model`/`route`; the call is tiny
/// (`max_tokens` is clamped low).
pub async fn generate_session_title(
    route: Arc<dyn Route>,
    model: Model,
    first_message: &str,
) -> Option<String> {
    let input: String = first_message.chars().take(MAX_INPUT_CHARS).collect();
    if input.trim().is_empty() {
        return None;
    }
    let mut req = LLMRequest::new(model, vec![Message::user(vec![ContentPart::text(input)])]);
    req.system = vec![SystemPart::new(TITLE_SYSTEM)];
    req.generation = Some(GenerationOptions {
        max_tokens: Some(24),
        ..Default::default()
    });

    let resp = LLMClient::new(route).generate(req).await.ok()?;
    let raw: String = resp
        .message
        .content
        .iter()
        .filter_map(|c| match c {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    clean_title(&raw)
}

/// Normalize a model's raw title output: first line only, surrounding quotes and
/// trailing sentence punctuation stripped, trimmed, and capped. Returns `None`
/// if nothing usable remains.
#[must_use]
fn clean_title(raw: &str) -> Option<String> {
    let line = raw.lines().next().unwrap_or("").trim();
    // Strip a wrapping pair of quotes/backticks, then trailing sentence marks.
    let line = line.trim_matches(['"', '\'', '`']).trim();
    let line = line.trim_end_matches(['.', ',', ';', ':', '!', '?']);
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let capped: String = line.chars().take(MAX_TITLE_CHARS).collect();
    let capped = capped.trim().to_string();
    (!capped.is_empty()).then_some(capped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_quotes_and_trailing_punctuation() {
        assert_eq!(
            clean_title("\"Retry logic for streams\"").as_deref(),
            Some("Retry logic for streams")
        );
        assert_eq!(
            clean_title("Streaming client retry logic.").as_deref(),
            Some("Streaming client retry logic")
        );
        assert_eq!(
            clean_title("`Add splash screen`").as_deref(),
            Some("Add splash screen")
        );
    }

    #[test]
    fn clean_takes_first_line_only() {
        assert_eq!(
            clean_title("Session Naming\nHere is the title you asked for").as_deref(),
            Some("Session Naming")
        );
    }

    #[test]
    fn clean_rejects_empty_and_whitespace() {
        assert_eq!(clean_title(""), None);
        assert_eq!(clean_title("   \n  "), None);
        assert_eq!(clean_title("\"\""), None);
    }

    #[test]
    fn default_titles_are_eligible_others_are_not() {
        assert!(is_default_session_title("New session"));
        assert!(is_default_session_title("New Session"));
        assert!(is_default_session_title("otto"));
        assert!(is_default_session_title("  New session  "), "trimmed");
        assert!(!is_default_session_title("Streaming client retry logic"));
        assert!(!is_default_session_title("Test"));
        assert!(!is_default_session_title(""));
    }

    #[test]
    fn clean_caps_length() {
        let long = "word ".repeat(40); // 200 chars
        let out = clean_title(&long).unwrap();
        assert!(out.chars().count() <= MAX_TITLE_CHARS, "capped: {out:?}");
    }
}
