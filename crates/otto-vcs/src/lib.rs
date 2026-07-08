//! Local git operations for otto: a `git` process runner, repository-root
//! resolution, and git-worktree management. Pure local git — no network, no
//! external service.

use std::path::Path;

use sha2::{Digest, Sha256};

mod files;
pub mod git;
pub mod worktree;

pub use files::{find_entries, find_files};

/// Errors from shelling out to `git` or from worktree management.
#[derive(Debug, thiserror::Error)]
pub enum VcsError {
    /// The directory is not inside a git working tree.
    #[error("not a git repository")]
    NotGit,
    /// A `git` invocation exited non-zero.
    #[error("`{command}` failed (status {status:?}): {stderr}")]
    GitFailed {
        /// The joined command line, for diagnostics.
        command: String,
        /// The process exit code, if any.
        status: Option<i32>,
        /// Captured stderr (trimmed).
        stderr: String,
    },
    /// `git` could not be spawned.
    #[error("failed to spawn git: {0}")]
    Spawn(#[source] std::io::Error),
    /// `git` produced non-UTF-8 output.
    #[error("git produced non-UTF-8 output")]
    Utf8,
    /// A worktree operation failed for a non-git reason (e.g. filesystem).
    #[error("{0}")]
    Other(String),
}

/// A filesystem-safe, stable slug for a project rooted at `git_root`.
///
/// `"prj_"` + the first 16 lowercase-hex chars of `sha256` over the
/// canonicalized path bytes (raw bytes if canonicalization fails). Used as the
/// per-project directory name under `<data_dir>/worktree/`.
#[must_use]
pub fn project_slug(git_root: &Path) -> String {
    let canonical = git_root
        .canonicalize()
        .unwrap_or_else(|_| git_root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("prj_{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_stable_and_prefixed() {
        let a = project_slug(Path::new("/tmp/some/project"));
        let b = project_slug(Path::new("/tmp/some/project"));
        assert_eq!(a, b);
        assert!(a.starts_with("prj_"));
        assert_eq!(a.len(), 4 + 16);
        let c = project_slug(Path::new("/tmp/other/project"));
        assert_ne!(a, c);
    }
}
