//! Startup splash: the otto wordmark + tagline and a llama portrait, shown
//! for ~5 seconds on TUI launch (or until the first keypress). Rendered as an
//! [`App::splash`](crate::state::App) state branch in [`view`](crate::view) so
//! nothing arriving on the message channel during the splash is dropped.
//!
//! Colour comes from the [`Theme`](crate::theme::Theme) seam: the wordmark in
//! `theme.accent`, the portrait in `theme.text`; `NO_COLOR` collapses both to
//! mono via `Theme::mono()`. This module holds no raw `Color`.
//!
//! All dimensions are derived from the embedded art at runtime, so the art file
//! (`splash_art.txt`) can be re-spaced or re-centred freely without touching any
//! code or test.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::theme::Theme;

/// The full splash art: the wordmark + tagline banner, then the llama portrait.
/// The portrait is identified by its `@` fill; the banner is everything before
/// it (see [`sections`]).
const ART: &str = include_str!("splash_art.txt");

/// Vertical breathing room (rows) required above+below the art block.
const VERT_MARGIN: u16 = 2;

/// Ticks the splash stays up before auto-dismissing. The tick pump runs at
/// 8/s (`view::TICKS_PER_SEC`), so 24 ticks ≈ 3 seconds. Any keypress dismisses
/// it earlier (see `App::on_key`).
pub const SPLASH_TICKS: u16 = 24;

/// Split the art into `(banner, portrait)` line slices. The portrait block
/// starts at the first line containing `@`; the banner is everything before it.
/// Trailing blank lines are trimmed from each block so the blank separator (and
/// any trailing newline) never inflates the measured height.
fn sections() -> (Vec<&'static str>, Vec<&'static str>) {
    let all: Vec<&'static str> = ART.lines().collect();
    let split = all
        .iter()
        .position(|l| l.contains('@'))
        .unwrap_or(all.len());
    (
        trim_trailing_blank(&all[..split]),
        trim_trailing_blank(&all[split..]),
    )
}

/// Drop trailing all-whitespace lines from a slice.
fn trim_trailing_blank(lines: &[&'static str]) -> Vec<&'static str> {
    let mut end = lines.len();
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    lines[..end].to_vec()
}

/// Widest line (in cells) of `lines`.
fn max_width(lines: &[&str]) -> u16 {
    lines
        .iter()
        .map(|l| l.chars().count() as u16)
        .max()
        .unwrap_or(0)
}

/// Which splash layout fits the current terminal, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplashVariant {
    /// Terminal too small for even the banner — show nothing.
    None,
    /// Room for the wordmark + tagline banner only.
    Banner,
    /// Room for the banner and the full llama portrait.
    Full,
}

/// Pick the largest splash layout that fits a `width`×`height` terminal. All
/// thresholds are measured from the art, so re-spacing the art re-tunes them.
#[must_use]
pub fn splash_variant(width: u16, height: u16) -> SplashVariant {
    let (banner, portrait) = sections();
    let banner_w = max_width(&banner);
    let banner_h = banner.len() as u16;
    // Full = banner + one gap row + portrait.
    let full_w = banner_w.max(max_width(&portrait));
    let full_h = banner_h + 1 + portrait.len() as u16;
    if width >= full_w && height >= full_h + VERT_MARGIN {
        SplashVariant::Full
    } else if width >= banner_w && height >= banner_h + VERT_MARGIN {
        SplashVariant::Banner
    } else {
        SplashVariant::None
    }
}

/// Whether the splash should show at all, given the `--no-splash` flag, the
/// `otto_NO_SPLASH` env var, and whether stdout is a TTY. Pure so the policy
/// is unit-testable; the caller resolves the env/TTY facts.
#[must_use]
pub fn should_show_splash(no_splash_flag: bool, no_splash_env: bool, is_tty: bool) -> bool {
    !no_splash_flag && !no_splash_env && is_tty
}

/// The art rows for `variant`: the banner alone, or the banner + a blank gap +
/// the portrait. Each row is paired with `true` when it belongs to the accent
/// banner (vs the plain-text portrait).
fn rows(variant: SplashVariant) -> Vec<(&'static str, bool)> {
    let (banner, portrait) = sections();
    let mut out: Vec<(&'static str, bool)> = banner.iter().map(|l| (*l, true)).collect();
    if variant == SplashVariant::Full {
        out.push(("", false));
        out.extend(portrait.iter().map(|l| (*l, false)));
    }
    out
}

/// Draw the splash centred in `area`. `SplashVariant::None` clears and draws
/// nothing (the caller should not call it in that case, but it is safe).
pub fn render(frame: &mut Frame, area: Rect, variant: SplashVariant, theme: &Theme) {
    frame.render_widget(Clear, area);
    if variant == SplashVariant::None {
        return;
    }
    let rows = rows(variant);
    let block_w = rows
        .iter()
        .map(|(l, _)| l.chars().count() as u16)
        .max()
        .unwrap_or(0);
    let block_h = rows.len() as u16;
    let lines: Vec<Line> = rows
        .into_iter()
        .map(|(l, is_banner)| {
            let style = if is_banner { theme.accent } else { theme.text };
            Line::from(Span::styled(l, style))
        })
        .collect();
    let rect = centered(area, block_w, block_h);
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Left), rect);
}

/// Centre a `w`×`h` sub-rect within `area` (clamped to fit).
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The art dimensions, measured from the embedded file.
    fn dims() -> (u16, u16, u16, u16) {
        let (banner, portrait) = sections();
        let banner_w = max_width(&banner);
        let banner_h = banner.len() as u16;
        let full_w = banner_w.max(max_width(&portrait));
        let full_h = banner_h + 1 + portrait.len() as u16;
        (banner_w, banner_h, full_w, full_h)
    }

    #[test]
    fn art_splits_into_banner_and_portrait() {
        let (banner, portrait) = sections();
        assert!(!banner.is_empty(), "banner present");
        assert!(!portrait.is_empty(), "portrait present");
        assert!(
            portrait.iter().any(|l| l.contains('@')),
            "portrait is the @-art"
        );
        assert!(
            banner.iter().all(|l| !l.contains('@')),
            "banner excludes @-art"
        );
        // Trailing blank separator / newline is trimmed from each block.
        assert!(!banner.last().unwrap().trim().is_empty());
        assert!(!portrait.last().unwrap().trim().is_empty());
    }

    #[test]
    fn variant_picks_largest_that_fits() {
        let (banner_w, banner_h, full_w, full_h) = dims();

        // Exactly enough for the full portrait.
        assert_eq!(
            splash_variant(full_w, full_h + VERT_MARGIN),
            SplashVariant::Full
        );
        assert_eq!(
            splash_variant(full_w + 40, full_h + 20),
            SplashVariant::Full
        );

        // One column short of the portrait: banner if it still fits, else none.
        let expect_narrow = if full_w > banner_w {
            SplashVariant::Banner
        } else {
            SplashVariant::None
        };
        assert_eq!(
            splash_variant(full_w - 1, full_h + VERT_MARGIN),
            expect_narrow
        );

        // Below the banner width or height floor → nothing.
        assert_eq!(
            splash_variant(banner_w - 1, full_h + VERT_MARGIN),
            SplashVariant::None
        );
        assert_eq!(
            splash_variant(full_w, banner_h + VERT_MARGIN - 1),
            SplashVariant::None
        );
    }

    #[test]
    fn show_policy() {
        assert!(should_show_splash(false, false, true));
        assert!(!should_show_splash(true, false, true), "--no-splash wins");
        assert!(
            !should_show_splash(false, true, true),
            "otto_NO_SPLASH wins"
        );
        assert!(!should_show_splash(false, false, false), "non-TTY skips");
    }

    #[test]
    fn full_has_accent_banner_then_plain_portrait() {
        let (banner, portrait) = sections();
        let full = rows(SplashVariant::Full);
        assert_eq!(full.len(), banner.len() + 1 + portrait.len());
        assert!(
            full[..banner.len()].iter().all(|(_, b)| *b),
            "banner rows accent"
        );
        assert!(
            full[banner.len() + 1..].iter().all(|(_, b)| !*b),
            "portrait rows plain"
        );
        assert_eq!(rows(SplashVariant::Banner).len(), banner.len());
    }
}
