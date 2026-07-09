//! Config schema + loader/merge — synchronous port of opencode
//! `packages/opencode/src/config/config.ts` (+ `config/paths.ts`) and the
//! schemas in `packages/core/src/v1/config/*` plus `packages/core/src/global.ts`.
//!
//! * [`schema`] — the [`Config`] struct (port of `ConfigV1.Info`,
//!   `config.ts:32-189`). Stable fields are typed ([`LogLevel`], [`Share`],
//!   [`Compaction`], [`ToolOutput`], `model`, `instructions`, …); evolving
//!   sub-objects (`agent`/`provider`/`mcp`/`permission`/`experimental`/…) stay as
//!   [`serde_json::Value`] so this crate stays decoupled.
//! * [`paths`] — XDG-style global dirs (`global.ts:10-29`), honoring
//!   `OTTO_CONFIG_DIR`.
//! * [`loader`] — [`parse`] (JSONC), [`merge`] (deep merge + `instructions`
//!   concat/dedupe), [`discover`] (up-tree project configs), and
//!   [`load_with`] / [`load`] (global → project precedence + env overrides).
//! * [`EnvOverrides`] is the testable seam: pass paths + overrides explicitly to
//!   [`load_with`] instead of mutating process env.

#![forbid(unsafe_code)]

pub mod error;
pub mod loader;
pub mod paths;
pub mod schema;

pub use error::{Error, Result};
pub use loader::{EnvOverrides, discover, load, load_global, load_with, merge, parse};
pub use schema::{
    Compaction, Config, DEFAULT_SCHEMA, Enterprise, LogLevel, Retry, Share, Tersemode,
    TersemodeLevel, ToolOutput, Watcher,
};
