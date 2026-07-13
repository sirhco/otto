//! Lifecycle hooks: user-configured external commands fired at points in
//! otto's session/tool/compaction pipeline. No opencode analog — otto
//! extension inspired by Claude Code's hooks. See
//! `docs/superpowers/specs/2026-07-12-lifecycle-hooks-design.md`.

mod config;
mod event;
mod runner;

pub use config::{HookCommand, HookMatcherGroup, HooksConfig};
pub use event::{CompactTrigger, Decision, HookEvent, HookKind, HookVerdict, SessionStartSource};
pub use runner::{DEFAULT_TIMEOUT_MS, HookRunner};
