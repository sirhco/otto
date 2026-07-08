//! The injected LSP seam. Defined here (not in `otto-lsp`) so that `otto-lsp`
//! can depend on `otto-tools` and implement this trait without a dependency
//! cycle. Tools hold an `Option<Arc<dyn LspHandle>>` and, when present, append a
//! `<diagnostics>` block to their success output after writing a file.

use std::path::{Path, PathBuf};

/// The seam through which the edit/write/apply_patch tools surface LSP
/// diagnostics. Implemented by `otto_lsp::Lsp`. Optional — when absent, tools
/// skip diagnostics entirely.
#[async_trait::async_trait]
pub trait LspHandle: Send + Sync {
    /// Open+wait for diagnostics on `path`, then return the formatted
    /// `<diagnostics>` block (`""` if no errors).
    async fn report_for(&self, path: &Path) -> String;

    /// Up to `max` OTHER files (≠ `exclude`) carrying error-severity
    /// diagnostics, each rendered as a `<diagnostics>` block. Uses
    /// already-collected diagnostics (no open/wait). For write-tool parity.
    async fn other_files_with_errors(&self, exclude: &Path, max: usize) -> Vec<(PathBuf, String)>;
}
