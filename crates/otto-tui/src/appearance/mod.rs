//! Terminal appearance detection: OS light/dark mode and color-depth
//! quantization. See [`os_theme`] for OS-appearance detection.

pub mod os_theme;

use ratatui::style::{Color, Style};

use crate::theme::Theme;

/// The OS's reported light/dark appearance setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Light,
    Dark,
}

/// Terminal color-depth support, most to least capable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    TrueColor,
    Ansi256,
    Ansi16,
}

/// Detect the terminal's color depth from `COLORTERM`/`TERM`. Env-var
/// heuristic only — no terminfo query, no config override. Total: an
/// unrecognized or absent pair of env vars falls through to `Ansi16`, the
/// safest assumption for an unknown terminal.
#[must_use]
pub fn detect_color_depth() -> ColorDepth {
    detect_color_depth_from(
        std::env::var("COLORTERM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
    )
}

/// Pure core of [`detect_color_depth`], testable without env mutation.
fn detect_color_depth_from(colorterm: Option<&str>, term: Option<&str>) -> ColorDepth {
    let truecolor = colorterm
        .is_some_and(|v| v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit"));
    if truecolor {
        ColorDepth::TrueColor
    } else if term.is_some_and(|t| t.contains("256color")) {
        ColorDepth::Ansi256
    } else {
        ColorDepth::Ansi16
    }
}

/// Downgrade every RGB style token in `theme` to `depth`. Non-RGB colors
/// (terminal defaults, and the named ANSI colors `dark()`/`mono()` use) pass
/// through unchanged — only the RGB-based presets (nord/catppuccin/gruvbox/
/// base16/light) are actually affected. `TrueColor` is a no-op clone.
#[must_use]
pub fn quantize(theme: &Theme, depth: ColorDepth) -> Theme {
    if depth == ColorDepth::TrueColor {
        return theme.clone();
    }
    Theme {
        text: quantize_style(theme.text, depth),
        text_muted: quantize_style(theme.text_muted, depth),
        accent: quantize_style(theme.accent, depth),
        accent_dim: quantize_style(theme.accent_dim, depth),
        status_ok: quantize_style(theme.status_ok, depth),
        status_warn: quantize_style(theme.status_warn, depth),
        status_err: quantize_style(theme.status_err, depth),
        border: quantize_style(theme.border, depth),
        border_focus: quantize_style(theme.border_focus, depth),
        selection: theme.selection,
        search_match: quantize_style(theme.search_match, depth),
        reasoning: theme.reasoning,
        diff_add: quantize_style(theme.diff_add, depth),
        diff_del: quantize_style(theme.diff_del, depth),
        diff_hunk: quantize_style(theme.diff_hunk, depth),
        diff_ctx: quantize_style(theme.diff_ctx, depth),
    }
}

fn quantize_style(style: Style, depth: ColorDepth) -> Style {
    Style {
        fg: style.fg.map(|c| quantize_color(c, depth)),
        bg: style.bg.map(|c| quantize_color(c, depth)),
        ..style
    }
}

fn quantize_color(color: Color, depth: ColorDepth) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color; // named/indexed colors pass through unchanged
    };
    match depth {
        ColorDepth::TrueColor => color,
        ColorDepth::Ansi256 => Color::Indexed(rgb_to_256(r, g, b)),
        ColorDepth::Ansi16 => rgb_to_16(r, g, b),
    }
}

/// Map an RGB triple to the nearest xterm 256-color cube index (16-231, the
/// standard 6x6x6 cube). Deliberately does not also check the grayscale ramp
/// (232-255) for a closer match on near-gray colors — out of scope for this
/// heuristic; the cube alone always returns a valid, reasonably close index.
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let to_cube = |v: u8| -> u8 {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|&(_, &level)| (i32::from(level) - i32::from(v)).abs())
            .map_or(0, |(i, _)| i as u8)
    };
    16 + 36 * to_cube(r) + 6 * to_cube(g) + to_cube(b)
}

/// Map an RGB triple to the nearest of the 16 basic ANSI colors by squared
/// Euclidean distance in RGB space.
fn rgb_to_16(r: u8, g: u8, b: u8) -> Color {
    const PALETTE: [(Color, (i32, i32, i32)); 16] = [
        (Color::Black, (0, 0, 0)),
        (Color::Red, (205, 0, 0)),
        (Color::Green, (0, 205, 0)),
        (Color::Yellow, (205, 205, 0)),
        (Color::Blue, (0, 0, 238)),
        (Color::Magenta, (205, 0, 205)),
        (Color::Cyan, (0, 205, 205)),
        (Color::Gray, (229, 229, 229)),
        (Color::DarkGray, (127, 127, 127)),
        (Color::LightRed, (255, 0, 0)),
        (Color::LightGreen, (0, 255, 0)),
        (Color::LightYellow, (255, 255, 0)),
        (Color::LightBlue, (92, 92, 255)),
        (Color::LightMagenta, (255, 0, 255)),
        (Color::LightCyan, (0, 255, 255)),
        (Color::White, (255, 255, 255)),
    ];
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    PALETTE
        .iter()
        .min_by_key(|(_, (pr, pg, pb))| (pr - r).pow(2) + (pg - g).pow(2) + (pb - b).pow(2))
        .map_or(Color::White, |(c, _)| *c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_color_depth_from_truecolor_colorterm() {
        assert_eq!(
            detect_color_depth_from(Some("truecolor"), Some("xterm")),
            ColorDepth::TrueColor
        );
        assert_eq!(
            detect_color_depth_from(Some("24bit"), None),
            ColorDepth::TrueColor
        );
        assert_eq!(
            detect_color_depth_from(Some("TrueColor"), None),
            ColorDepth::TrueColor,
            "case-insensitive"
        );
    }

    #[test]
    fn detect_color_depth_from_256color_term() {
        assert_eq!(
            detect_color_depth_from(None, Some("xterm-256color")),
            ColorDepth::Ansi256
        );
    }

    #[test]
    fn detect_color_depth_from_neither_is_16() {
        assert_eq!(detect_color_depth_from(None, None), ColorDepth::Ansi16);
        assert_eq!(
            detect_color_depth_from(None, Some("xterm")),
            ColorDepth::Ansi16
        );
    }

    #[test]
    fn quantize_truecolor_is_identity() {
        let dark = Theme::dark();
        let q = quantize(&dark, ColorDepth::TrueColor);
        assert_eq!(q.accent.fg, dark.accent.fg);
    }

    #[test]
    fn quantize_named_color_passes_through() {
        // `dark()`'s accent is `Color::Cyan` (named, not RGB) — quantization
        // must not touch it at any depth.
        let dark = Theme::dark();
        for depth in [
            ColorDepth::TrueColor,
            ColorDepth::Ansi256,
            ColorDepth::Ansi16,
        ] {
            assert_eq!(quantize(&dark, depth).accent.fg, Some(Color::Cyan));
        }
    }

    #[test]
    fn quantize_rgb_to_256_maps_to_indexed() {
        // Nord's accent: Rgb(0x88, 0xC0, 0xD0).
        let nord = Theme::preset("nord");
        let q = quantize(&nord, ColorDepth::Ansi256);
        assert!(matches!(q.accent.fg, Some(Color::Indexed(_))));
    }

    #[test]
    fn quantize_rgb_to_16_returns_a_named_color() {
        let nord = Theme::preset("nord");
        let q = quantize(&nord, ColorDepth::Ansi16);
        // Must round-trip to one of the 16 basic ANSI colors, never a raw
        // Rgb/Indexed value.
        assert!(!matches!(
            q.accent.fg,
            Some(Color::Rgb(..)) | Some(Color::Indexed(_))
        ));
    }

    #[test]
    fn quantize_rgb_to_16_maps_pure_red_to_light_red() {
        // An unambiguous case: (255, 0, 0) is an exact match for LightRed,
        // strictly closer than every other palette entry — verifies the
        // nearest-neighbor search picks the right color, not just *a* named
        // color.
        let mut theme = Theme::dark();
        theme.accent = Style::default().fg(Color::Rgb(255, 0, 0));
        let q = quantize(&theme, ColorDepth::Ansi16);
        assert_eq!(q.accent.fg, Some(Color::LightRed));
    }

    #[test]
    fn quantize_preserves_modifier_only_tokens() {
        // `selection`/`reasoning` carry no fg/bg — must pass through untouched
        // at every depth.
        let nord = Theme::preset("nord");
        for depth in [ColorDepth::Ansi256, ColorDepth::Ansi16] {
            let q = quantize(&nord, depth);
            assert_eq!(q.selection.add_modifier, nord.selection.add_modifier);
            assert_eq!(q.reasoning.add_modifier, nord.reasoning.add_modifier);
        }
    }
}
