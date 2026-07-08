//! Post-green regression routine: prove the just-written test actually fails
//! without the production change, then passes again with it restored. This is
//! what stops a vacuous test (one that passes with or without the impl) from
//! being accepted as green.

use std::path::Path;

use otto_vcs::git::run_git;

use crate::error::WfError;
use crate::runner::TestRunner;

/// Outcome of the regression check.
#[derive(Debug, PartialEq, Eq)]
pub enum RegressionOutcome {
    /// Test failed without the prod change and passed with it — verified.
    Verified,
    /// Test still passed after removing the prod change — it does not exercise
    /// the implementation (a false green).
    TestPassedWithoutProd,
}

fn vcs_err(e: otto_vcs::VcsError) -> WfError {
    WfError::Run(format!("git: {e}"))
}

/// Changed paths in `cwd` per `git status --porcelain` (tracked mods + untracked).
///
/// Each porcelain line is normally a fixed `XY<space>path` (3-byte prefix).
/// `run_git` trims the *whole* stdout blob (not per line), which can eat the
/// leading status-code space of the first line when `X == ' '` (e.g. a
/// worktree-only modification renders as `" M path"` and becomes `"M path"`
/// once the outer trim runs). Detect that shift by checking where the
/// separating space actually landed rather than assuming a fixed offset.
pub async fn git_changed_files(cwd: &Path) -> Result<Vec<String>, WfError> {
    let out = run_git(cwd, &["status", "--porcelain"])
        .await
        .map_err(vcs_err)?;
    let files = out
        .lines()
        .filter_map(|l| {
            let bytes = l.as_bytes();
            let prefix_len = if bytes.get(2) != Some(&b' ') && bytes.get(1) == Some(&b' ') {
                2
            } else {
                3
            };
            let path = l.get(prefix_len..)?.trim();
            let path = path.rsplit(" -> ").next().unwrap_or(path).trim();
            if path.is_empty() {
                None
            } else {
                Some(path.to_string())
            }
        })
        .collect();
    Ok(files)
}

/// Prove the committed test fails without the working-tree implementation and
/// passes with it. The caller MUST have committed the test (and any prior
/// state) already, so the only working-tree delta is the production change.
/// Stashing that delta must yield a *genuine* red (an assertion / test
/// failure), not a compile error — a compile break certifies nothing.
///
/// # Stashing assumption
///
/// This stashes the working-tree production change — including untracked files
/// (`--include-untracked`), so a brand-new untracked impl file is removed too —
/// to run the suite without it, then restores it with `git stash pop`. It
/// ASSUMES build outputs are gitignored (the standard cargo/npm/etc. layout, so
/// `target/`, `node_modules/`, etc. are never swept into the stash). Untracked,
/// non-ignored artifacts that the test run regenerates can make `git stash pop`
/// collide ("would be overwritten by merge") and fail; on that path the
/// production change stays safely in the git stash — see the error surfaced
/// below for recovery.
pub async fn regression_check(
    runner: &dyn TestRunner,
    cwd: &Path,
) -> Result<RegressionOutcome, WfError> {
    // 1. Remove the production change (test is committed, so this is impl-only).
    run_git(cwd, &["stash", "push", "--include-untracked"])
        .await
        .map_err(vcs_err)?;

    // 2. Without the impl, run and classify.
    let without = runner.run(None).await;
    let kind = crate::classify::classify_red(&without);

    // 3. Restore the production change no matter what. If `pop` collides (e.g. an
    //    untracked, non-ignored artifact the test run regenerated), the change is
    //    NOT lost — it stays in the git stash; surface that so it is recoverable.
    let restore = run_git(cwd, &["stash", "pop"]).await.map_err(|e| {
        WfError::Run(format!(
            "regression: failed to restore the production change with `git stash pop` ({e}). \
             Your production change is preserved in the git stash and can be recovered by \
             running `git stash pop` (resolve any conflicting untracked files first)."
        ))
    });

    match kind {
        crate::classify::RedKind::Passed => {
            restore?;
            return Ok(RegressionOutcome::TestPassedWithoutProd);
        }
        crate::classify::RedKind::CompileError => {
            restore?;
            return Err(WfError::Gate(
                "regression: removing the production change broke compilation, so the test's genuine failure cannot be verified".to_string(),
            ));
        }
        crate::classify::RedKind::GenuineRed => {
            restore?;
        }
    }

    // 4. With the impl back, the test MUST pass.
    let with = runner.run(None).await;
    if !with.passed {
        return Err(WfError::Gate(
            "regression: suite did not pass after restoring the production change".to_string(),
        ));
    }
    Ok(RegressionOutcome::Verified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::AutoRunner;

    async fn git(dir: &Path, args: &[&str]) {
        run_git(dir, args).await.unwrap();
    }

    /// Build a tiny cargo project in a temp git repo whose single test depends
    /// on a production function; commit a RED baseline (function returns wrong
    /// value), then add the correct impl as the working-tree change.
    async fn setup_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        std::fs::write(
            p.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(p.join("src")).unwrap();
        // Ignore build artifacts so the --include-untracked stash never sweeps
        // `target/` (which cargo recreates between stash and pop).
        std::fs::write(p.join(".gitignore"), "/target\nCargo.lock\n").unwrap();
        // committed baseline: add() returns 0 (wrong) + a test expecting 3.
        std::fs::write(
            p.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { 0 }\n#[cfg(test)]\nmod t { #[test] fn adds() { assert_eq!(super::add(1,2), 3); } }\n",
        )
        .unwrap();
        git(p, &["init", "-q"]).await;
        git(p, &["config", "user.email", "t@t.t"]).await;
        git(p, &["config", "user.name", "t"]).await;
        git(p, &["add", "-A"]).await;
        git(p, &["commit", "-qm", "baseline red"]).await;
        // working-tree production change: correct the impl.
        std::fs::write(
            p.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n#[cfg(test)]\nmod t { #[test] fn adds() { assert_eq!(super::add(1,2), 3); } }\n",
        )
        .unwrap();
        d
    }

    #[tokio::test]
    async fn regression_verifies_real_test() {
        let d = setup_repo().await;
        let runner = AutoRunner::new(d.path().to_path_buf());
        // Sanity: with the impl, tests pass.
        assert!(runner.run(None).await.passed);
        let out = regression_check(&runner, d.path()).await.unwrap();
        assert_eq!(out, RegressionOutcome::Verified);
    }

    #[tokio::test]
    async fn git_changed_files_lists_worktree_change() {
        let d = setup_repo().await;
        let files = git_changed_files(d.path()).await.unwrap();
        assert!(files.iter().any(|f| f == "src/lib.rs"), "files={files:?}");
    }

    /// Commit a failing test + a WRONG impl in ONE file (same-file layout), then
    /// overwrite the impl in the working tree with the correct version
    /// (uncommitted). This is the layout Phase-3 Imp1 could not handle.
    async fn setup_repo_committed_test() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        std::fs::write(
            p.join("Cargo.toml"),
            "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join(".gitignore"), "/target\nCargo.lock\n").unwrap();
        // Same file holds impl AND test — the layout Phase-3 Imp1 could not handle.
        std::fs::write(
            p.join("src/lib.rs"),
            "pub fn add(a:i32,b:i32)->i32{0}\n#[cfg(test)]\nmod t{use super::*;#[test]fn a(){assert_eq!(add(1,2),3);}}\n",
        )
        .unwrap();
        git(p, &["init", "-q"]).await;
        git(p, &["config", "user.email", "t@t.t"]).await;
        git(p, &["config", "user.name", "t"]).await;
        git(p, &["add", "-A"]).await;
        git(p, &["commit", "-qm", "red test + wrong impl"]).await;
        // GREEN impl as the ONLY working-tree change (same file):
        std::fs::write(
            p.join("src/lib.rs"),
            "pub fn add(a:i32,b:i32)->i32{a+b}\n#[cfg(test)]\nmod t{use super::*;#[test]fn a(){assert_eq!(add(1,2),3);}}\n",
        )
        .unwrap();
        d
    }

    /// The COMMITTED test references a function (`used`) that only the
    /// working-tree impl provides. `stash push --include-untracked` reverts the
    /// whole working-tree delta (removing `used`), so the committed test can no
    /// longer compile — the without-impl run is a COMPILE error, not a genuine
    /// red, and `classify_red` must stop it from certifying (Imp2).
    ///
    /// NOTE: the brief's original layout (add `used` only, keep `helper`
    /// calling it) does NOT compile-break here — a whole-file stash reverts to
    /// a self-consistent committed baseline that compiles and passes. Putting
    /// the dangling reference in the *committed* test is what forces the
    /// compile error under a whole-working-tree stash.
    async fn setup_repo_compile_dependent() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        std::fs::write(
            p.join("Cargo.toml"),
            "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join(".gitignore"), "/target\nCargo.lock\n").unwrap();
        // Committed: a test referencing `used()`, which does NOT yet exist —
        // this baseline compiles only once the working-tree impl adds `used`.
        std::fs::write(
            p.join("src/lib.rs"),
            "pub fn helper()->i32{0}\n#[cfg(test)]\nmod t{use super::*;#[test]fn a(){assert_eq!(used(),0);}}\n",
        )
        .unwrap();
        git(p, &["init", "-q"]).await;
        git(p, &["config", "user.email", "t@t.t"]).await;
        git(p, &["config", "user.name", "t"]).await;
        git(p, &["add", "-A"]).await;
        git(p, &["commit", "-qm", "baseline"]).await;
        // Working-tree change: add `used()`. Stashing this reverts to the
        // committed baseline, whose test then references a missing `used` ->
        // compile error (not a genuine red).
        std::fs::write(
            p.join("src/lib.rs"),
            "pub fn helper()->i32{0}\npub fn used()->i32{0}\n#[cfg(test)]\nmod t{use super::*;#[test]fn a(){assert_eq!(used(),0);}}\n",
        )
        .unwrap();
        d
    }

    // The test is COMMITTED, only the impl is a worktree change.
    #[tokio::test]
    async fn verified_when_committed_test_fails_without_worktree_impl() {
        let d = setup_repo_committed_test().await;
        let runner = AutoRunner::new(d.path().to_path_buf());
        // Working tree currently holds the CORRECT impl (uncommitted); test is committed.
        let outcome = regression_check(&runner, d.path()).await.unwrap();
        assert_eq!(outcome, RegressionOutcome::Verified);
    }

    #[tokio::test]
    async fn compile_break_without_impl_is_not_a_false_green() {
        // If stashing the impl breaks compilation (not a genuine test failure),
        // the check must NOT certify — it returns a Gate error, never Verified.
        let d = setup_repo_compile_dependent().await;
        let runner = AutoRunner::new(d.path().to_path_buf());
        let r = regression_check(&runner, d.path()).await;
        assert!(
            matches!(r, Err(WfError::Gate(_))),
            "compile break must gate, got {r:?}"
        );
    }
}
