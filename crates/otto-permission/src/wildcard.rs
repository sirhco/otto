//! Glob-style matcher — a port of opencode's `util/wildcard.ts` `match`.
//!
//! opencode builds a regular expression from the pattern:
//!
//! ```text
//! match(input, pattern):
//!   normalized = input.replaceAll("\\", "/")
//!   escaped = pattern.replaceAll("\\", "/")
//!     .replace(/[.+^${}()|[\]\\]/g, "\\$&")   // escape regex metachars
//!     .replace(/\*/g, ".*")                    // `*`  -> any run
//!     .replace(/\?/g, ".")                     // `?`  -> any single
//!   if escaped.endsWith(" .*") escaped = escaped.slice(0, -3) + "( .*)?"
//!   return new RegExp("^" + escaped + "$", "s").test(normalized)
//! ```
//!
//! Rather than depend on a regex engine we translate the pattern into a tiny
//! token stream that reproduces exactly those constructs and match it with a
//! backtracking walker. The `s` (dotall) flag means `.` matches every
//! character including newlines, so [`Token::AnyOne`] / [`Token::AnyRun`]
//! consume any byte-run.

/// A single element of the compiled pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A literal character that must match exactly.
    Literal(char),
    /// `?` → `.` — matches exactly one character (any character).
    AnyOne,
    /// `*` → `.*` — matches any run of characters (including empty).
    AnyRun,
    /// The trailing ` *` special case: opencode rewrites a trailing ` .*`
    /// (a literal space followed by `.*`) into `( .*)?`, making the whole
    /// " suffix" optional. Matches either the empty string, or a leading
    /// space followed by any run.
    OptSpaceRun,
}

/// Compile `pattern` into the token stream, applying the same normalization
/// and the trailing ` *` → `( .*)?` rewrite that `wildcard.ts` performs.
fn compile(pattern: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    for ch in pattern.chars() {
        match ch {
            '\\' => tokens.push(Token::Literal('/')), // replaceAll("\\", "/")
            '*' => tokens.push(Token::AnyRun),
            '?' => tokens.push(Token::AnyOne),
            other => tokens.push(Token::Literal(other)),
        }
    }
    // `escaped.endsWith(" .*")` ⇔ tokens end with [Literal(' '), AnyRun].
    let n = tokens.len();
    if n >= 2 && tokens[n - 1] == Token::AnyRun && tokens[n - 2] == Token::Literal(' ') {
        tokens.truncate(n - 2);
        tokens.push(Token::OptSpaceRun);
    }
    tokens
}

/// Backtracking matcher over the compiled token stream.
fn matches(tokens: &[Token], input: &[char]) -> bool {
    match tokens.split_first() {
        None => input.is_empty(),
        Some((Token::Literal(c), rest)) => {
            !input.is_empty() && input[0] == *c && matches(rest, &input[1..])
        }
        Some((Token::AnyOne, rest)) => !input.is_empty() && matches(rest, &input[1..]),
        Some((Token::AnyRun, rest)) => (0..=input.len()).any(|i| matches(rest, &input[i..])),
        Some((Token::OptSpaceRun, rest)) => {
            // `( .*)?`: either the group is absent, or it is a space + any run.
            if matches(rest, input) {
                return true;
            }
            if !input.is_empty() && input[0] == ' ' {
                return (1..=input.len()).any(|i| matches(rest, &input[i..]));
            }
            false
        }
    }
}

/// Returns whether `input` matches glob `pattern`, mirroring
/// `Wildcard.match(input, pattern)` in `util/wildcard.ts`.
///
/// `input` has its backslashes normalized to forward slashes, `*` matches any
/// run, `?` matches any single character, and a trailing ` *` in the pattern
/// makes the space-suffix optional.
pub fn wildcard_match(input: &str, pattern: &str) -> bool {
    let normalized: Vec<char> = input.replace('\\', "/").chars().collect();
    let tokens = compile(pattern);
    matches(&tokens, &normalized)
}

#[cfg(test)]
mod tests {
    use super::wildcard_match;

    #[test]
    fn literal_and_star() {
        assert!(wildcard_match("edit", "edit"));
        assert!(!wildcard_match("edit", "read"));
        assert!(wildcard_match("anything", "*"));
        assert!(wildcard_match("", "*"));
    }

    #[test]
    fn star_segments() {
        assert!(wildcard_match("src/main.rs", "src/*"));
        assert!(wildcard_match("src/a/b.rs", "src/*"));
        assert!(wildcard_match("foo.rs", "*.rs"));
        assert!(!wildcard_match("foo.ts", "*.rs"));
        assert!(wildcard_match("git commit -m x", "git *"));
    }

    #[test]
    fn question_matches_single() {
        assert!(wildcard_match("a", "?"));
        assert!(!wildcard_match("ab", "?"));
        assert!(wildcard_match("cat", "c?t"));
    }

    #[test]
    fn regex_metachars_are_literal() {
        assert!(wildcard_match("a.b", "a.b"));
        assert!(!wildcard_match("axb", "a.b"));
        assert!(wildcard_match("a+b", "a+b"));
        assert!(wildcard_match("(x)", "(x)"));
    }

    #[test]
    fn backslash_normalized() {
        assert!(wildcard_match("a\\b\\c", "a/b/c"));
        assert!(wildcard_match("a/b/c", "a\\b\\c"));
    }

    #[test]
    fn trailing_space_star_is_optional() {
        // pattern "git *" -> "^git( .*)?$"
        assert!(wildcard_match("git", "git *"));
        assert!(wildcard_match("git status", "git *"));
        assert!(wildcard_match("git ", "git *"));
        assert!(!wildcard_match("gitx", "git *"));
    }
}
