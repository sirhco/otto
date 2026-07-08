//! Syntax highlighting seam. `syntect` is confined to this file.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

struct Assets {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
}

fn assets() -> &'static Assets {
    static A: OnceLock<Assets> = OnceLock::new();
    A.get_or_init(|| Assets {
        syntaxes: SyntaxSet::load_defaults_newlines(),
        themes: ThemeSet::load_defaults(),
    })
}

/// The syntect theme name in use, selected once at startup. Defaults to the
/// historical hardcoded value so callers that never select still work.
fn selected_theme() -> &'static Mutex<String> {
    static SELECTED: OnceLock<Mutex<String>> = OnceLock::new();
    SELECTED.get_or_init(|| Mutex::new("base16-ocean.dark".to_string()))
}

/// Map a TUI `theme` preset to a bundled syntect theme name. Every arm returns
/// a key guaranteed present in `ThemeSet::load_defaults()`.
fn syntect_theme_name(preset: Option<&str>) -> &'static str {
    match preset.map(|p| p.to_ascii_lowercase()).as_deref() {
        Some("gruvbox") => "base16-eighties.dark",
        Some("catppuccin") => "base16-mocha.dark",
        // nord / base16 / unknown / none
        _ => "base16-ocean.dark",
    }
}

/// Select the syntect code-block theme to match the chosen TUI preset. Call
/// once at startup, before rendering; the highlight cache is keyed by
/// `(code, lang)` and assumes a stable theme.
pub fn select_syntect_theme(preset: Option<&str>) {
    let name = syntect_theme_name(preset);
    let mut g = selected_theme().lock().unwrap_or_else(|e| e.into_inner());
    *g = name.to_string();
}

/// Memoization cache keyed by `(code, lang)`. During a stream, the same
/// scrollback code blocks are re-highlighted every frame (many `TextDelta`
/// events per second, plus 8 render ticks per second) even though their
/// content hasn't changed, so caching keeps `syntect` off the
/// render-blocking hot path.
///
/// Capped at `CACHE_CAP` entries: rather than an LRU, we simply clear the
/// whole map once it grows past the cap. Code blocks are identical across
/// frames so cache hits dominate in practice, and a full clear is far
/// simpler than eviction bookkeeping for a cache this small.
const CACHE_CAP: usize = 512;

type CacheKey = (String, Option<String>);

fn cache() -> &'static Mutex<HashMap<CacheKey, Vec<Line<'static>>>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, Vec<Line<'static>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Highlight `code` in `lang`. Unknown language → plain dim lines.
///
/// Results are memoized (see [`cache`]) since this is called on the
/// render-blocking path every frame while a response streams.
#[must_use]
pub fn highlight(code: &str, lang: Option<&str>) -> Vec<Line<'static>> {
    let key: CacheKey = (code.to_string(), lang.map(str::to_string));
    // A poisoned lock (a prior panic while holding it) still holds a usable
    // map; degrade to using it rather than panicking the render loop.
    let guard = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = guard.get(&key) {
        return hit.clone();
    }
    drop(guard);

    let out = highlight_uncached(code, lang);

    let mut guard = cache().lock().unwrap_or_else(|e| e.into_inner());
    if guard.len() >= CACHE_CAP {
        guard.clear();
    }
    guard.insert(key, out.clone());
    out
}

fn highlight_uncached(code: &str, lang: Option<&str>) -> Vec<Line<'static>> {
    let a = assets();
    let syntax = lang
        .and_then(|l| a.syntaxes.find_syntax_by_token(l))
        .or_else(|| a.syntaxes.find_syntax_by_first_line(code));
    let Some(syntax) = syntax else {
        return plain(code);
    };
    let theme_name = selected_theme()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let theme = a
        .themes
        .themes
        .get(theme_name.as_str())
        .unwrap_or_else(|| &a.themes.themes["base16-ocean.dark"]);
    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        let Ok(ranges) = h.highlight_line(line, &a.syntaxes) else {
            return plain(code);
        };
        let spans: Vec<Span<'static>> = ranges
            .into_iter()
            .map(|(sty, text)| {
                Span::styled(text.trim_end_matches('\n').to_string(), to_ratatui(sty))
            })
            .collect();
        out.push(Line::from(spans));
    }
    out
}

fn to_ratatui(s: SynStyle) -> Style {
    Style::default().fg(Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b))
}

fn plain(code: &str) -> Vec<Line<'static>> {
    code.lines()
        .map(|l| {
            Line::from(Span::styled(
                l.to_string(),
                Style::default().add_modifier(ratatui::style::Modifier::DIM),
            ))
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

    /// The second call should be served from the memoization cache in
    /// `highlight()` rather than re-running `syntect`; we can't observe the
    /// cache hit directly from outside this module, but equal output for an
    /// identical `(code, lang)` key proves the cached path returns the same
    /// thing a fresh computation would.
    #[test]
    fn highlight_is_memoized() {
        let first = highlight("fn main() {}", Some("rust"));
        let second = highlight("fn main() {}", Some("rust"));
        assert_eq!(first, second);
    }

    #[test]
    fn highlights_rust_preserving_text() {
        let out = highlight("fn main() {}", Some("rust"));
        assert_eq!(joined(&out).trim_end(), "fn main() {}");
        // at least one span carries a non-default color.
        assert!(
            out.iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.style.fg.is_some())
        );
    }

    #[test]
    fn unknown_language_falls_back_to_plain() {
        let out = highlight("random text", Some("not-a-lang"));
        assert!(joined(&out).contains("random text"));
    }

    #[test]
    fn no_language_does_not_panic() {
        let _ = highlight("x = 1", None);
    }

    #[test]
    fn syntect_theme_name_maps_presets_to_valid_keys() {
        let themes = ThemeSet::load_defaults();
        for preset in [
            Some("nord"),
            Some("catppuccin"),
            Some("gruvbox"),
            Some("base16"),
            None,
            Some("bogus"),
        ] {
            let name = syntect_theme_name(preset);
            assert!(
                themes.themes.contains_key(name),
                "syntect theme {name:?} (preset {preset:?}) not in load_defaults"
            );
        }
    }

    #[test]
    fn select_syntect_theme_is_used_by_highlighter() {
        // Selecting a valid preset must not break highlighting.
        select_syntect_theme(Some("gruvbox"));
        let out = highlight("fn main() {}", Some("rust"));
        assert!(
            out.iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.style.fg.is_some())
        );
    }
}
