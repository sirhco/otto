//! Pure renderers: `&str`/`&Value` -> `Vec<ratatui::text::Line>`. No terminal.

pub mod diff;
pub mod highlight;
pub mod markdown;
pub mod tool;
