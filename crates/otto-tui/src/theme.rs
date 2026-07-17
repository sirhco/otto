//! Semantic style tokens. Every color/modifier in the TUI comes from here
//! (the sole exceptions are `render/highlight.rs`, which owns its syntect RGB,
//! and `render/markdown.rs`, which is modifier-only and mono-safe by nature).

use ratatui::style::{Color, Modifier, Style};

/// Named style tokens consumed by `view.rs` and `render/*`.
#[derive(Debug, Clone)]
pub struct Theme {
    pub text: Style,
    pub text_muted: Style,
    pub accent: Style,
    pub accent_dim: Style,
    pub status_ok: Style,
    pub status_warn: Style,
    pub status_err: Style,
    pub border: Style,
    pub border_focus: Style,
    pub selection: Style,
    pub search_match: Style,
    pub reasoning: Style,
    pub diff_add: Style,
    pub diff_del: Style,
    pub diff_hunk: Style,
    pub diff_ctx: Style,
}

impl Theme {
    /// The default colored theme: cyan accent, green/yellow/red state triad.
    #[must_use]
    pub fn dark() -> Self {
        Self {
            text: Style::default(),
            text_muted: Style::default().add_modifier(Modifier::DIM),
            accent: Style::default().fg(Color::Cyan),
            accent_dim: Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
            status_ok: Style::default().fg(Color::Green),
            status_warn: Style::default().fg(Color::Yellow),
            status_err: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            border: Style::default(),
            border_focus: Style::default().fg(Color::Cyan),
            selection: Style::default().add_modifier(Modifier::REVERSED),
            // Non-current search matches: a muted accent (yellow already
            // lives in the palette via `status_warn`, so this isn't a new
            // hue) plus underline so it stays legible on any base style
            // (muted/reasoning text included) without competing with the
            // brighter, reversed `selection` used for the current match.
            search_match: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::UNDERLINED),
            reasoning: Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
            diff_add: Style::default().fg(Color::Green),
            diff_del: Style::default().fg(Color::Red),
            diff_hunk: Style::default().fg(Color::Cyan),
            diff_ctx: Style::default(),
        }
    }

    /// Monochrome variant: same struct with every `Color` stripped, modifiers
    /// preserved. Derived mechanically from `dark()` so the two never drift.
    #[must_use]
    pub fn mono() -> Self {
        fn strip(s: Style) -> Style {
            Style::default().add_modifier(s.add_modifier)
        }
        let d = Self::dark();
        Self {
            text: strip(d.text),
            text_muted: strip(d.text_muted),
            accent: strip(d.accent),
            accent_dim: strip(d.accent_dim),
            status_ok: strip(d.status_ok),
            status_warn: strip(d.status_warn),
            status_err: strip(d.status_err),
            border: strip(d.border),
            border_focus: strip(d.border_focus),
            selection: strip(d.selection),
            search_match: strip(d.search_match),
            reasoning: strip(d.reasoning),
            diff_add: strip(d.diff_add),
            diff_del: strip(d.diff_del),
            diff_hunk: strip(d.diff_hunk),
            diff_ctx: strip(d.diff_ctx),
        }
    }

    /// Pick a theme from a `NO_COLOR`-set flag (testable without env mutation).
    #[must_use]
    pub fn select(no_color: bool) -> Self {
        if no_color { Self::mono() } else { Self::dark() }
    }

    /// Active theme from the environment: `NO_COLOR` set (any value, per
    /// no-color.org) → `mono()`, else `dark()`.
    #[must_use]
    pub fn from_env() -> Self {
        Self::select(std::env::var_os("NO_COLOR").is_some())
    }

    /// A named color preset. Unknown names fall back to [`Theme::dark`].
    /// Case-insensitive.
    #[must_use]
    pub fn preset(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            // Nord — https://www.nordtheme.com
            "nord" => from_palette(Palette {
                text: Color::Rgb(0xD8, 0xDE, 0xE9),
                muted: Color::Rgb(0x4C, 0x56, 0x6A),
                accent: Color::Rgb(0x88, 0xC0, 0xD0),
                ok: Color::Rgb(0xA3, 0xBE, 0x8C),
                warn: Color::Rgb(0xEB, 0xCB, 0x8B),
                err: Color::Rgb(0xBF, 0x61, 0x6A),
            }),
            // Catppuccin Mocha — https://catppuccin.com
            "catppuccin" => from_palette(Palette {
                text: Color::Rgb(0xCD, 0xD6, 0xF4),
                muted: Color::Rgb(0x6C, 0x70, 0x86),
                accent: Color::Rgb(0x89, 0xB4, 0xFA),
                ok: Color::Rgb(0xA6, 0xE3, 0xA1),
                warn: Color::Rgb(0xF9, 0xE2, 0xAF),
                err: Color::Rgb(0xF3, 0x8B, 0xA8),
            }),
            // Gruvbox dark — https://github.com/morhetz/gruvbox
            "gruvbox" => from_palette(Palette {
                text: Color::Rgb(0xEB, 0xDB, 0xB2),
                muted: Color::Rgb(0x92, 0x83, 0x74),
                accent: Color::Rgb(0x83, 0xA5, 0x98),
                ok: Color::Rgb(0xB8, 0xBB, 0x26),
                warn: Color::Rgb(0xFA, 0xBD, 0x2F),
                err: Color::Rgb(0xFB, 0x49, 0x34),
            }),
            // Neutral base16 (Ocean-ish) — the config-explicit "default colored".
            "base16" => from_palette(Palette {
                text: Color::Rgb(0xC0, 0xC5, 0xCE),
                muted: Color::Rgb(0x65, 0x73, 0x7E),
                accent: Color::Rgb(0x8F, 0xA1, 0xB3),
                ok: Color::Rgb(0xA3, 0xBE, 0x8C),
                warn: Color::Rgb(0xEB, 0xCB, 0x8B),
                err: Color::Rgb(0xBF, 0x61, 0x6A),
            }),
            // Light — a light-background counterpart for OS-appearance
            // auto-detection (`theme = "auto"`). Not derived from `dark()`
            // like `mono()` is: needs its own palette, not a stripped one.
            "light" => from_palette(Palette {
                text: Color::Rgb(0x24, 0x29, 0x2E),
                muted: Color::Rgb(0x6E, 0x77, 0x81),
                accent: Color::Rgb(0x03, 0x66, 0xD6),
                ok: Color::Rgb(0x22, 0x86, 0x3A),
                warn: Color::Rgb(0xB0, 0x80, 0x00),
                err: Color::Rgb(0xD7, 0x3A, 0x49),
            }),
            _ => Self::dark(),
        }
    }

    /// Startup theme selection with full precedence: `NO_COLOR` → `mono()`;
    /// else a named `theme_name` preset; else `dark()`.
    #[must_use]
    pub fn select_with(no_color: bool, theme_name: Option<&str>) -> Self {
        if no_color {
            Self::mono()
        } else {
            match theme_name {
                Some(name) => Self::preset(name),
                None => Self::dark(),
            }
        }
    }

    /// The accent color as an uppercase 6-digit hex string (no `#`), for OSC
    /// 12 terminal cursor coloring. `None` for named/indexed colors (the
    /// plain `dark()`/`mono()` presets) — there's no cursor-color equivalent
    /// for a named ANSI color.
    #[must_use]
    pub fn accent_hex(&self) -> Option<String> {
        match self.accent.fg {
            Some(Color::Rgb(r, g, b)) => Some(format!("{r:02X}{g:02X}{b:02X}")),
            _ => None,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// A minimal 6-color source palette. `from_palette` expands it into the full
/// 16-token `Theme`, reusing `dark()`'s modifier choices so presets never drift
/// from the default's structure.
struct Palette {
    text: Color,
    muted: Color,
    accent: Color,
    ok: Color,
    warn: Color,
    err: Color,
}

fn from_palette(p: Palette) -> Theme {
    Theme {
        text: Style::default().fg(p.text),
        text_muted: Style::default().fg(p.muted).add_modifier(Modifier::DIM),
        accent: Style::default().fg(p.accent),
        accent_dim: Style::default().fg(p.accent).add_modifier(Modifier::DIM),
        status_ok: Style::default().fg(p.ok),
        status_warn: Style::default().fg(p.warn),
        status_err: Style::default().fg(p.err).add_modifier(Modifier::BOLD),
        border: Style::default().fg(p.muted),
        border_focus: Style::default().fg(p.accent),
        selection: Style::default().add_modifier(Modifier::REVERSED),
        search_match: Style::default()
            .fg(p.warn)
            .add_modifier(Modifier::UNDERLINED),
        reasoning: Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC),
        diff_add: Style::default().fg(p.ok),
        diff_del: Style::default().fg(p.err),
        diff_hunk: Style::default().fg(p.accent),
        diff_ctx: Style::default().fg(p.text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_picks_mono_under_no_color() {
        // mono has no fg color on the accent token; dark does.
        assert_eq!(Theme::select(false).accent.fg, Some(Color::Cyan));
        assert_eq!(Theme::select(true).accent.fg, None);
    }

    #[test]
    fn mono_strips_color_keeps_modifiers() {
        let m = Theme::mono();
        // No token carries a foreground/background color.
        for s in [
            m.text,
            m.text_muted,
            m.accent,
            m.accent_dim,
            m.status_ok,
            m.status_warn,
            m.status_err,
            m.border,
            m.border_focus,
            m.selection,
            m.search_match,
            m.reasoning,
            m.diff_add,
            m.diff_del,
            m.diff_hunk,
            m.diff_ctx,
        ] {
            assert_eq!(s.fg, None, "mono token has no fg");
            assert_eq!(s.bg, None, "mono token has no bg");
        }
        // Distinguishing modifiers survive.
        assert!(m.text_muted.add_modifier.contains(Modifier::DIM));
        assert!(m.status_err.add_modifier.contains(Modifier::BOLD));
        assert!(m.selection.add_modifier.contains(Modifier::REVERSED));
        assert!(m.search_match.add_modifier.contains(Modifier::UNDERLINED));
        assert!(
            m.reasoning
                .add_modifier
                .contains(Modifier::DIM | Modifier::ITALIC)
        );
    }

    #[test]
    fn search_match_distinct_from_selection() {
        // Current-match (`selection`, REVERSED) and other-match
        // (`search_match`, underlined) must remain visually distinguishable
        // in both the colored and NO_COLOR themes.
        for theme in [Theme::dark(), Theme::mono()] {
            assert_ne!(
                theme.selection.add_modifier, theme.search_match.add_modifier,
                "current vs other match modifiers must differ"
            );
        }
    }

    #[test]
    fn preset_nord_is_distinct_and_colored() {
        let n = Theme::preset("nord");
        assert!(n.accent.fg.is_some());
        assert_ne!(n.accent.fg, Theme::dark().accent.fg);
    }

    #[test]
    fn preset_is_case_insensitive() {
        assert_eq!(
            Theme::preset("NORD").accent.fg,
            Theme::preset("nord").accent.fg
        );
    }

    #[test]
    fn preset_unknown_falls_back_to_dark() {
        assert_eq!(Theme::preset("bogus").accent.fg, Theme::dark().accent.fg);
    }

    #[test]
    fn select_with_no_color_wins_over_preset() {
        assert_eq!(Theme::select_with(true, Some("nord")).accent.fg, None);
    }

    #[test]
    fn select_with_applies_named_preset() {
        assert_eq!(
            Theme::select_with(false, Some("gruvbox")).accent.fg,
            Theme::preset("gruvbox").accent.fg
        );
    }

    #[test]
    fn select_with_none_is_dark() {
        assert_eq!(
            Theme::select_with(false, None).accent.fg,
            Theme::dark().accent.fg
        );
    }

    #[test]
    fn preset_light_is_distinct_and_colored() {
        let l = Theme::preset("light");
        assert!(l.accent.fg.is_some());
        assert_ne!(l.accent.fg, Theme::dark().accent.fg);
    }

    #[test]
    fn accent_hex_for_rgb_preset() {
        // Nord's accent: Rgb(0x88, 0xC0, 0xD0).
        assert_eq!(
            Theme::preset("nord").accent_hex(),
            Some("88C0D0".to_string())
        );
    }

    #[test]
    fn accent_hex_none_for_named_color() {
        // `dark()`'s accent is the named `Color::Cyan`, not RGB.
        assert_eq!(Theme::dark().accent_hex(), None);
    }

    #[test]
    fn accent_hex_none_for_mono() {
        assert_eq!(Theme::mono().accent_hex(), None);
    }
}
