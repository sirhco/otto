//! git-worktree management: create, list, remove, and reset isolated
//! working trees rooted under a per-project data directory.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::VcsError;
use crate::git::{git_root, run_git};
use crate::project_slug;

/// One managed worktree, as reported by [`Worktree::list`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorktreeInfo {
    /// The worktree's short name (final path component).
    pub name: String,
    /// The checked-out branch, `refs/heads/` stripped; `None` if detached.
    pub branch: Option<String>,
    /// Absolute path to the worktree directory.
    pub directory: String,
}

/// Input for [`Worktree::create`].
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct CreateInput {
    /// A human name; slugified for the branch and directory. Defaults to
    /// `"workspace"` when absent or empty after slugifying.
    #[serde(default)]
    pub name: Option<String>,
}

/// Input for [`Worktree::remove`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoveInput {
    /// Absolute path of the worktree directory to remove.
    pub directory: String,
}

/// Input for [`Worktree::reset`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResetInput {
    /// Absolute path of the worktree directory to reset.
    pub directory: String,
}

/// Worktree manager for one git project.
///
/// Stateless beyond the two roots: every call shells out to `git`.
#[derive(Debug, Clone)]
pub struct Worktree {
    /// The primary repository root.
    pub git_root: PathBuf,
    /// `<data_base>/<project-slug>` — where new worktrees are created.
    pub data_root: PathBuf,
}

impl Worktree {
    /// Construct from explicit roots.
    #[must_use]
    pub fn new(git_root: PathBuf, data_root: PathBuf) -> Self {
        Self {
            git_root,
            data_root,
        }
    }

    /// Resolve the git root containing `dir` and derive the per-project data
    /// root as `data_base/<project-slug>`.
    ///
    /// # Errors
    /// [`VcsError::NotGit`] if `dir` is not in a repository.
    pub async fn discover(dir: &Path, data_base: &Path) -> Result<Self, VcsError> {
        let root = git_root(dir).await?;
        let data_root = data_base.join(project_slug(&root));
        Ok(Self::new(root, data_root))
    }

    /// List managed worktrees, excluding the primary.
    ///
    /// # Errors
    /// Propagates git failures.
    pub async fn list(&self) -> Result<Vec<WorktreeInfo>, VcsError> {
        let out = run_git(&self.git_root, &["worktree", "list", "--porcelain"]).await?;
        let primary = self
            .git_root
            .canonicalize()
            .unwrap_or_else(|_| self.git_root.clone());

        let mut infos = Vec::new();
        let mut cur_path: Option<String> = None;
        let mut cur_branch: Option<String> = None;

        // Porcelain blocks are separated by blank lines; a block starts with
        // `worktree <path>` and may carry `branch refs/heads/<b>`, `detached`,
        // or `bare`.
        let flush = |path: &mut Option<String>,
                     branch: &mut Option<String>,
                     out: &mut Vec<WorktreeInfo>| {
            if let Some(p) = path.take() {
                let pb = PathBuf::from(&p);
                let is_primary = pb.canonicalize().map(|c| c == primary).unwrap_or(false);
                if !is_primary {
                    let name = pb
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.clone());
                    out.push(WorktreeInfo {
                        name,
                        branch: branch.take(),
                        directory: p,
                    });
                }
            }
            *branch = None;
        };

        for line in out.lines() {
            if let Some(rest) = line.strip_prefix("worktree ") {
                flush(&mut cur_path, &mut cur_branch, &mut infos);
                cur_path = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("branch ") {
                cur_branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string());
            }
            // `detached`, `bare`, `HEAD <sha>` lines need no field here.
        }
        flush(&mut cur_path, &mut cur_branch, &mut infos);
        Ok(infos)
    }

    /// Create a fresh worktree with a unique name and branch `otto/<name>`.
    ///
    /// # Errors
    /// [`VcsError::Other`] if no free name is found; propagates git failures.
    pub async fn create(&self, input: CreateInput) -> Result<WorktreeInfo, VcsError> {
        let base = slugify(input.name.as_deref().unwrap_or("workspace"));
        let mut name = base.clone();
        let mut path = self.data_root.join(&name);
        let mut branch = format!("otto/{name}");

        for attempt in 2..=50 {
            let dir_free = !path.exists();
            let branch_free = run_git(
                &self.git_root,
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ],
            )
            .await
            .is_err();
            if dir_free && branch_free {
                break;
            }
            name = format!("{base}-{attempt}");
            path = self.data_root.join(&name);
            branch = format!("otto/{name}");
            if attempt == 50 {
                return Err(VcsError::Other(format!(
                    "could not find a free worktree name for '{base}'"
                )));
            }
        }

        std::fs::create_dir_all(&self.data_root).map_err(|e| VcsError::Other(e.to_string()))?;
        let path_str = path.to_string_lossy().into_owned();
        run_git(
            &self.git_root,
            &["worktree", "add", "-b", &branch, &path_str, "HEAD"],
        )
        .await?;

        Ok(WorktreeInfo {
            name,
            branch: Some(branch),
            directory: path_str,
        })
    }

    /// Find the managed worktree matching `directory`, if any.
    ///
    /// Compares canonicalized paths: git's porcelain output reports the
    /// resolved path (e.g. `/private/var/...` on macOS), which may differ
    /// from `directory`'s raw form even when they name the same directory.
    /// Falls back to raw string equality when canonicalization fails (e.g.
    /// the path no longer exists).
    ///
    /// # Errors
    /// Propagates `git worktree list` failures.
    async fn find_managed(&self, directory: &str) -> Result<Option<WorktreeInfo>, VcsError> {
        let target = PathBuf::from(directory)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(directory));
        Ok(self.list().await?.into_iter().find(|w| {
            PathBuf::from(&w.directory)
                .canonicalize()
                .map(|c| c == target)
                .unwrap_or(false)
                || w.directory == directory
        }))
    }

    /// Remove a worktree and delete its `otto/<name>` branch.
    ///
    /// # Errors
    /// Propagates `git worktree remove` failures. Branch deletion is
    /// best-effort and never fails the call.
    pub async fn remove(&self, input: RemoveInput) -> Result<bool, VcsError> {
        let branch = self
            .find_managed(&input.directory)
            .await?
            .and_then(|w| w.branch);

        run_git(
            &self.git_root,
            &["worktree", "remove", "--force", "--", &input.directory],
        )
        .await?;

        if let Some(branch) = branch {
            // Best-effort: the worktree is already gone regardless.
            let _ = run_git(&self.git_root, &["branch", "-D", &branch]).await;
        }
        Ok(true)
    }

    /// Resolve the remote default branch (`main`/`master`/`origin/HEAD`).
    ///
    /// # Errors
    /// [`VcsError::Other`] if no default branch can be determined.
    pub async fn default_branch(&self) -> Result<String, VcsError> {
        if let Ok(head) = run_git(
            &self.git_root,
            &["rev-parse", "--abbrev-ref", "origin/HEAD"],
        )
        .await
            && let Some(b) = head.strip_prefix("origin/")
            && !b.is_empty()
        {
            return Ok(b.to_string());
        }
        for candidate in ["main", "master"] {
            let exists = run_git(
                &self.git_root,
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/remotes/origin/{candidate}"),
                ],
            )
            .await
            .is_ok();
            if exists {
                return Ok(candidate.to_string());
            }
        }
        Err(VcsError::Other("could not resolve a default branch".into()))
    }

    /// Reset a worktree hard to `origin/<default>` and remove untracked files.
    ///
    /// # Errors
    /// [`VcsError::Other`] if `input.directory` is not a managed worktree
    /// (checked before any destructive git command runs). Propagates git
    /// failures (e.g. no `origin` remote).
    pub async fn reset(&self, input: ResetInput) -> Result<bool, VcsError> {
        // Guard: `git reset --hard` + `git clean -ffdx` are destructive and,
        // unlike `git worktree remove`, git places no bound on `cwd` here —
        // it will happily hard-reset and wipe gitignored files in *any*
        // directory we point it at. Refuse anything that isn't a worktree we
        // manage before touching git at all.
        if self.find_managed(&input.directory).await?.is_none() {
            return Err(VcsError::Other(format!(
                "not a managed worktree: {}",
                input.directory
            )));
        }

        let branch = self.default_branch().await?;
        run_git(&self.git_root, &["fetch", "origin", &branch]).await?;

        let dir = Path::new(&input.directory);
        run_git(dir, &["reset", "--hard", &format!("origin/{branch}")]).await?;
        run_git(dir, &["clean", "-ffdx"]).await?;
        Ok(true)
    }

    /// Merge `from_directory`'s uncommitted working-tree changes —
    /// including new/untracked files — into this manager's primary
    /// `git_root`, as an unstaged working-tree edit. Nothing is committed or
    /// left staged in either tree. Used by `SddWorkflow`'s Phase A to fold
    /// each isolated implementer worktree back into the shared tree before
    /// review.
    ///
    /// Returns `true` if there was anything to merge, `false` if
    /// `from_directory` was already clean relative to its own `HEAD`.
    ///
    /// # Errors
    /// [`VcsError::GitFailed`] if the diff does not apply cleanly onto
    /// `git_root` (e.g. two worktrees touched overlapping lines) — the
    /// apply is atomic, so a failure never partially modifies `git_root`.
    pub async fn merge_working_tree(&self, from_directory: &str) -> Result<bool, VcsError> {
        let from = Path::new(from_directory);
        run_git(from, &["add", "-A"]).await?;
        let patch = run_git(from, &["diff", "--cached"]).await?;
        // Always unstage again, regardless of outcome — from_directory is a
        // disposable worktree the caller removes right after this call
        // either way, but leaving it un-staged keeps state predictable if
        // it isn't removed immediately.
        let _ = run_git(from, &["reset"]).await;
        if patch.trim().is_empty() {
            return Ok(false);
        }
        let patch_path = merge_patch_path(from_directory);
        std::fs::write(&patch_path, format!("{patch}\n"))
            .map_err(|e| VcsError::Other(e.to_string()))?;
        let result = run_git(
            &self.git_root,
            &[
                "apply",
                "--whitespace=nowarn",
                &patch_path.to_string_lossy(),
            ],
        )
        .await;
        let _ = std::fs::remove_file(&patch_path);
        result?;
        Ok(true)
    }
}

/// Slugify a user-provided name into a branch/dir-safe token.
fn slugify(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "workspace".to_string()
    } else {
        out
    }
}

/// A temp-dir path unique to `from_directory`, used to stage the merge
/// patch on disk (`git apply` needs a real file path; `run_git` has no
/// stdin support). Hashed rather than reusing `from_directory`'s own name so
/// it stays filesystem-safe regardless of what the caller names a worktree.
fn merge_patch_path(from_directory: &str) -> PathBuf {
    let hash = Sha256::digest(from_directory.as_bytes());
    std::env::temp_dir().join(format!("otto-vcs-merge-{hash:x}.patch"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{git_available, run_git};

    pub(super) async fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        run_git(p, &["init", "-q", "-b", "main"]).await.unwrap();
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
    async fn list_excludes_primary_and_parses_added() {
        if !git_available().await {
            return;
        }
        let repo = init_repo().await;
        let data = tempfile::tempdir().unwrap();
        let wt = Worktree::discover(repo.path(), data.path()).await.unwrap();

        // Empty to start (only the primary worktree exists).
        assert_eq!(wt.list().await.unwrap(), vec![]);

        // Add one worktree with a raw git command and confirm it is parsed.
        let added = wt.data_root.join("feature");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "otto/feature",
                added.to_str().unwrap(),
                "HEAD",
            ],
        )
        .await
        .unwrap();

        let list = wt.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "feature");
        assert_eq!(list[0].branch.as_deref(), Some("otto/feature"));
        assert_eq!(
            std::path::Path::new(&list[0].directory)
                .canonicalize()
                .unwrap(),
            added.canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn create_makes_worktree_and_branch() {
        if !git_available().await {
            return;
        }
        let repo = init_repo().await;
        let data = tempfile::tempdir().unwrap();
        let wt = Worktree::discover(repo.path(), data.path()).await.unwrap();

        let info = wt
            .create(CreateInput {
                name: Some("API Refactor!".into()),
            })
            .await
            .unwrap();
        assert_eq!(info.name, "api-refactor");
        assert_eq!(info.branch.as_deref(), Some("otto/api-refactor"));
        assert!(std::path::Path::new(&info.directory).join("f.txt").exists());
        assert_eq!(wt.list().await.unwrap().len(), 1);

        // A second create with the same name must not collide.
        let info2 = wt
            .create(CreateInput {
                name: Some("api refactor".into()),
            })
            .await
            .unwrap();
        assert_eq!(info2.name, "api-refactor-2");
        assert_eq!(wt.list().await.unwrap().len(), 2);
    }

    #[test]
    fn slugify_cleans_names() {
        assert_eq!(super::slugify("API Refactor!"), "api-refactor");
        assert_eq!(super::slugify("  multi   space "), "multi-space");
        assert_eq!(super::slugify("***"), "workspace");
    }

    #[tokio::test]
    async fn remove_deletes_worktree_and_branch() {
        if !git_available().await {
            return;
        }
        let repo = init_repo().await;
        let data = tempfile::tempdir().unwrap();
        let wt = Worktree::discover(repo.path(), data.path()).await.unwrap();
        let info = wt
            .create(CreateInput {
                name: Some("gone".into()),
            })
            .await
            .unwrap();
        assert_eq!(wt.list().await.unwrap().len(), 1);

        let ok = wt
            .remove(RemoveInput {
                directory: info.directory.clone(),
            })
            .await
            .unwrap();
        assert!(ok);
        assert_eq!(wt.list().await.unwrap().len(), 0);
        assert!(!std::path::Path::new(&info.directory).exists());
        // Branch is gone too.
        let branch_check = run_git(
            repo.path(),
            &["show-ref", "--verify", "--quiet", "refs/heads/otto/gone"],
        )
        .await;
        assert!(branch_check.is_err());
    }

    #[tokio::test]
    async fn merge_working_tree_applies_new_and_modified_files() {
        if !git_available().await {
            return;
        }
        let repo = init_repo().await;
        let data = tempfile::tempdir().unwrap();
        let wt = Worktree::discover(repo.path(), data.path()).await.unwrap();
        let info = wt
            .create(CreateInput {
                name: Some("merge-src".into()),
            })
            .await
            .unwrap();
        let src = std::path::Path::new(&info.directory);
        std::fs::write(src.join("f.txt"), "modified").unwrap();
        std::fs::write(src.join("new.txt"), "brand new").unwrap();

        let changed = wt.merge_working_tree(&info.directory).await.unwrap();
        assert!(changed);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("f.txt")).unwrap(),
            "modified"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("new.txt")).unwrap(),
            "brand new"
        );
    }

    #[tokio::test]
    async fn merge_working_tree_returns_false_when_clean() {
        if !git_available().await {
            return;
        }
        let repo = init_repo().await;
        let data = tempfile::tempdir().unwrap();
        let wt = Worktree::discover(repo.path(), data.path()).await.unwrap();
        let info = wt
            .create(CreateInput {
                name: Some("clean".into()),
            })
            .await
            .unwrap();

        let changed = wt.merge_working_tree(&info.directory).await.unwrap();
        assert!(!changed);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("f.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn merge_working_tree_errors_on_conflict_without_corrupting_git_root() {
        if !git_available().await {
            return;
        }
        let repo = init_repo().await;
        let data = tempfile::tempdir().unwrap();
        let wt = Worktree::discover(repo.path(), data.path()).await.unwrap();
        let a = wt
            .create(CreateInput {
                name: Some("a".into()),
            })
            .await
            .unwrap();
        let b = wt
            .create(CreateInput {
                name: Some("b".into()),
            })
            .await
            .unwrap();
        std::fs::write(std::path::Path::new(&a.directory).join("f.txt"), "AAA").unwrap();
        std::fs::write(std::path::Path::new(&b.directory).join("f.txt"), "BBB").unwrap();

        assert!(wt.merge_working_tree(&a.directory).await.unwrap());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("f.txt")).unwrap(),
            "AAA"
        );

        let err = wt.merge_working_tree(&b.directory).await.unwrap_err();
        assert!(matches!(err, VcsError::GitFailed { .. }), "got {err:?}");
        // The failed apply must not have partially modified git_root.
        assert_eq!(
            std::fs::read_to_string(repo.path().join("f.txt")).unwrap(),
            "AAA"
        );
    }

    /// Build a repo cloned from a local bare origin (so `git fetch origin`
    /// works with no network). Returns (origin_dir, work_dir).
    async fn init_repo_with_origin() -> (tempfile::TempDir, tempfile::TempDir) {
        let origin = tempfile::tempdir().unwrap();
        run_git(origin.path(), &["init", "-q", "--bare", "-b", "main"])
            .await
            .unwrap();

        let work = tempfile::tempdir().unwrap();
        let wp = work.path();
        run_git(wp, &["init", "-q", "-b", "main"]).await.unwrap();
        run_git(wp, &["config", "user.email", "t@t.t"])
            .await
            .unwrap();
        run_git(wp, &["config", "user.name", "t"]).await.unwrap();
        run_git(wp, &["config", "commit.gpgsign", "false"])
            .await
            .unwrap();
        run_git(
            wp,
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        )
        .await
        .unwrap();
        std::fs::write(wp.join("f.txt"), "v1").unwrap();
        run_git(wp, &["add", "."]).await.unwrap();
        run_git(wp, &["commit", "-q", "-m", "init"]).await.unwrap();
        run_git(wp, &["push", "-q", "-u", "origin", "main"])
            .await
            .unwrap();
        (origin, work)
    }

    #[tokio::test]
    async fn reset_restores_worktree_to_origin() {
        if !git_available().await {
            return;
        }
        let (_origin, work) = init_repo_with_origin().await;
        let data = tempfile::tempdir().unwrap();
        let wt = Worktree::discover(work.path(), data.path()).await.unwrap();
        let info = wt
            .create(CreateInput {
                name: Some("wip".into()),
            })
            .await
            .unwrap();

        // Dirty the worktree.
        let f = std::path::Path::new(&info.directory).join("f.txt");
        std::fs::write(&f, "TAMPERED").unwrap();
        std::fs::write(std::path::Path::new(&info.directory).join("junk.txt"), "x").unwrap();

        let ok = wt
            .reset(ResetInput {
                directory: info.directory.clone(),
            })
            .await
            .unwrap();
        assert!(ok);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        assert!(
            !std::path::Path::new(&info.directory)
                .join("junk.txt")
                .exists()
        );
    }

    #[tokio::test]
    async fn reset_rejects_unmanaged_directory() {
        if !git_available().await {
            return;
        }
        let (_origin, work) = init_repo_with_origin().await;
        let data = tempfile::tempdir().unwrap();
        let wt = Worktree::discover(work.path(), data.path()).await.unwrap();

        // An arbitrary directory that is NOT a worktree `wt` manages (not
        // returned by `wt.list()`). Plant a sentinel untracked file so we
        // can prove `git clean -ffdx` never ran against it.
        let unmanaged = tempfile::tempdir().unwrap();
        let sentinel = unmanaged.path().join("sentinel.env");
        std::fs::write(&sentinel, "SECRET=1").unwrap();

        let err = wt
            .reset(ResetInput {
                directory: unmanaged.path().to_string_lossy().into_owned(),
            })
            .await
            .unwrap_err();
        match err {
            VcsError::Other(msg) => assert!(
                msg.contains("not a managed worktree"),
                "unexpected message: {msg}"
            ),
            other => panic!("expected VcsError::Other, got {other:?}"),
        }

        // The destructive path must never have run.
        assert!(sentinel.exists(), "sentinel file was wiped; guard failed");
    }
}
