//! LSP client: spawn language servers over stdio, collect diagnostics,
//! surface errors-only `<diagnostics>` blocks to the model after edits.
//! Behavior ported from opencode `packages/opencode/src/lsp/`.

pub mod client;
pub mod config;
pub mod framing;
pub mod protocol;
pub mod registry;
pub mod report;
pub mod service;
pub mod transport;

pub use service::{Lsp, LspConfigResolved, LspStatus, ServerOverride};
