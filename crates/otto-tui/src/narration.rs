//! Progress narration for the activity line: a playful rotating word while
//! thinking, or a literal action verb while a tool runs. Pure — no state, no
//! rng dependency.

/// otto-flavored progress words shown while thinking (no active tool).
/// Deliberately original to otto, NOT Claude Code's verbatim strings, spanning
/// engineering / organic / culinary / whimsical registers.
pub const WORDS: &[&str] = &[
    // engineering
    "Assembling",
    "Rewiring",
    "Machining",
    "Smithing",
    "Tightening",
    // organic
    "Sprouting",
    "Ripening",
    "Blooming",
    "Rooting",
    // culinary
    "Decanting",
    "Steeping",
    "Whisking",
    "Reducing",
    "Kneading",
    // whimsical
    "Doodling",
    "Pottering",
    "Finagling",
    "Whittling",
    "Rummaging",
    "Unriddling",
    "Corralling",
    "Fiddling",
];

/// Pick a word for the given rotation window. Deterministic (unit-testable) but
/// scrambled across consecutive windows so it reads as random rather than
/// stepping through the list in order. Hand-rolled integer hash — no `rand`.
#[must_use]
pub fn narration_word(rotation: u32) -> &'static str {
    let mut x = rotation.wrapping_mul(2_654_435_761); // Knuth multiplicative
    x ^= x >> 15;
    x = x.wrapping_mul(2_246_822_519);
    x ^= x >> 13;
    WORDS[(x as usize) % WORDS.len()]
}

/// The literal action for a running tool: a present-participle verb keyed off
/// the tool name plus its argument. The leading tool-name token is stripped
/// from `title` (so `bash` + `"bash ls -F"` -> `Running ls -F`). If no argument
/// remains, the verb is returned alone (no trailing space).
#[must_use]
pub fn tool_action(name: &str, title: &str) -> String {
    let verb: String = match name.to_ascii_lowercase().as_str() {
        "read" => "Reading".into(),
        "write" => "Writing".into(),
        "edit" | "apply_patch" | "patch" => "Editing".into(),
        "bash" => "Running".into(),
        "grep" | "glob" | "list" => "Searching".into(),
        "webfetch" | "fetch" => "Fetching".into(),
        "task" => "Delegating".into(),
        "todowrite" => "Planning".into(),
        _ => title_case(name),
    };
    let arg = strip_name_prefix(name, title);
    if arg.is_empty() {
        verb
    } else {
        format!("{verb} {arg}")
    }
}

/// Strip a leading `"{name}"` token (and following whitespace) from `title`,
/// returning the remaining argument (trimmed). If `title` doesn't start with
/// `name`, the trimmed title is returned unchanged.
fn strip_name_prefix(name: &str, title: &str) -> String {
    let t = title.trim();
    match t.strip_prefix(name) {
        Some(rest) => rest.trim_start().to_string(),
        None => t.to_string(),
    }
}

/// Upper-case the first character of `s`, leave the rest as-is.
fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narration_word_is_deterministic() {
        assert_eq!(narration_word(0), narration_word(0));
        assert_eq!(narration_word(7), narration_word(7));
    }

    #[test]
    fn narration_word_always_in_list() {
        for r in 0..100u32 {
            assert!(
                WORDS.contains(&narration_word(r)),
                "rotation {r} out of list"
            );
        }
    }

    #[test]
    fn narration_word_varies_across_windows() {
        // Over the first several rotations the word must change at least once
        // (so a long think visibly cycles, never looks frozen).
        let distinct: std::collections::HashSet<_> = (0..8u32).map(narration_word).collect();
        assert!(
            distinct.len() > 1,
            "narration word never changed across 8 windows"
        );
    }

    #[test]
    fn tool_action_read() {
        assert_eq!(tool_action("read", "README.md"), "Reading README.md");
    }

    #[test]
    fn tool_action_bash_strips_leading_name() {
        assert_eq!(tool_action("bash", "bash ls -F"), "Running ls -F");
    }

    #[test]
    fn tool_action_edit_and_write() {
        assert_eq!(tool_action("edit", "state.rs"), "Editing state.rs");
        assert_eq!(tool_action("write", "new.rs"), "Writing new.rs");
    }

    #[test]
    fn tool_action_search_and_fetch_and_task() {
        assert_eq!(tool_action("grep", "TODO"), "Searching TODO");
        assert_eq!(tool_action("webfetch", "https://x"), "Fetching https://x");
        assert_eq!(
            tool_action("task", "explore repo"),
            "Delegating explore repo"
        );
    }

    #[test]
    fn tool_action_unknown_name_title_cased() {
        assert_eq!(tool_action("frobnicate", "frobnicate x"), "Frobnicate x");
    }

    #[test]
    fn tool_action_verb_alone_when_no_arg() {
        // Title is just the name (no argument) -> verb only, no trailing space.
        assert_eq!(tool_action("bash", "bash"), "Running");
        assert_eq!(tool_action("read", "read"), "Reading");
    }
}
