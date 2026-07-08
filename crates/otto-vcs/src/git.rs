//! A thin wrapper over the local `git` binary: run a command in a working
//! directory and capture stdout, resolve the repository root, and detect
//! whether `git` is available. All worktree logic builds on these.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::VcsError;

/// Run `git <args>` with `cwd` as the working directory.
///
/// Returns trimmed stdout on a zero exit. A non-zero exit becomes
/// [`VcsError::GitFailed`] carrying the joined command, exit code, and stderr.
///
/// # Errors
/// [`VcsError::Spawn`] if `git` cannot be launched, [`VcsError::GitFailed`] on
/// a non-zero exit, [`VcsError::Utf8`] if stdout is not valid UTF-8.
pub async fn run_git(cwd: &Path, args: &[&str]) -> Result<String, VcsError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(VcsError::Spawn)?;
    if !output.status.success() {
        return Err(VcsError::GitFailed {
            command: format!("git {}", args.join(" ")),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| VcsError::Utf8)?;
    Ok(stdout.trim().to_string())
}

/// Resolve the repository root that contains `dir`.
///
/// # Errors
/// [`VcsError::NotGit`] if `dir` is not inside a git working tree.
pub async fn git_root(dir: &Path) -> Result<PathBuf, VcsError> {
    match run_git(dir, &["rev-parse", "--show-toplevel"]).await {
        Ok(path) => Ok(PathBuf::from(path)),
        Err(VcsError::GitFailed { .. }) => Err(VcsError::NotGit),
        Err(other) => Err(other),
    }
}

/// `true` if the local `git` binary can be invoked.
pub async fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Initialize a git repo in a fresh temp dir with one commit; return it.
    async fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        run_git(p, &["init", "-q"]).await.unwrap();
        run_git(p, &["config", "user.email", "t@t.t"])
            .await
            .unwrap();
        run_git(p, &["config", "user.name", "t"]).await.unwrap();
        run_git(p, &["config", "commit.gpgsign", "false"])
            .await
            .unwrap();
        std::fs::write(p.join("f.txt"), "hello").unwrap();
        run_git(p, &["add", "."]).await.unwrap();
        run_git(p, &["commit", "-q", "-m", "init"]).await.unwrap();
        dir
    }

    #[tokio::test]
    async fn git_root_resolves_repo() {
        if !git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let repo = init_repo().await;
        let root = git_root(repo.path()).await.unwrap();
        assert_eq!(
            root.canonicalize().unwrap(),
            repo.path().canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn git_root_errors_outside_repo() {
        if !git_available().await {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(git_root(dir.path()).await, Err(VcsError::NotGit)));
    }

    #[tokio::test]
    async fn run_git_reports_failure() {
        if !git_available().await {
            return;
        }
        let repo = init_repo().await;
        let err = run_git(repo.path(), &["not-a-subcommand"])
            .await
            .unwrap_err();
        assert!(matches!(err, VcsError::GitFailed { .. }));
    }
}
