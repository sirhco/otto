use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// Enumerate workspace files under `root`, repo-relative, sorted, deduped, capped at `limit`.
/// Prefers `git ls-files` (tracked + untracked-not-ignored, .gitignore-respecting, .git
/// excluded); falls back to a recursive walk (skipping `.git`) for non-git dirs.
pub fn find_files(root: &Path, limit: usize) -> Vec<String> {
    find_entries(root, limit).0
}

/// Enumerate workspace files *and* directories under `root`, repo-relative,
/// each list sorted/deduped and capped at `limit`. Directories are derived
/// from every ancestor (`Path::ancestors()`) of the *full* file list — before
/// truncation — so a directory isn't dropped just because the file that
/// justified it fell past the cap; the two lists are then truncated
/// independently. Because directories are derived from the file list, they
/// automatically inherit its gitignore/`.git` exclusions: a directory whose
/// only contents are ignored never appears.
///
/// Wire contract: the HTTP layer sends `files` and `dirs` as separate JSON
/// keys; the TUI's `@`-mention picker merges `dirs` into its candidate list
/// as trailing-`/` strings (`is_dir == ends_with('/')`).
pub fn find_entries(root: &Path, limit: usize) -> (Vec<String>, Vec<String>) {
    let mut files = git_ls_files(root).unwrap_or_else(|| walk_files(root));
    files.sort();
    files.dedup();

    let mut dir_set: BTreeSet<String> = BTreeSet::new();
    for file in &files {
        for ancestor in Path::new(file).ancestors().skip(1) {
            match ancestor.to_str() {
                Some(s) if !s.is_empty() => {
                    dir_set.insert(s.to_string());
                }
                _ => {}
            }
        }
    }
    let mut dirs: Vec<String> = dir_set.into_iter().collect();

    files.truncate(limit);
    dirs.truncate(limit);
    (files, dirs)
}

fn git_ls_files(root: &Path) -> Option<Vec<String>> {
    let out = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let list: Vec<String> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    Some(list)
}

fn walk_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk_dir(root, root, &mut out);
    out
}

fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        if path.is_dir() {
            walk_dir(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root)
            && let Some(s) = rel.to_str()
        {
            out.push(s.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = Command::new("git").args(args).current_dir(dir).status();
        assert!(
            ok.map(|s| s.success()).unwrap_or(false),
            "git {args:?} failed"
        );
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn find_files_lists_tracked_and_untracked_excludes_gitignored() {
        if !git_available() {
            eprintln!("skip: git absent");
            return;
        }
        let tmp = std::env::temp_dir().join(format!("otto-ff-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        git(&tmp, &["init", "-q"]);
        fs::write(tmp.join("a.txt"), "a").unwrap();
        fs::write(tmp.join("b.rs"), "b").unwrap();
        fs::write(tmp.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(tmp.join("ignored.txt"), "x").unwrap();
        git(&tmp, &["add", "a.txt", ".gitignore"]);

        let files = find_files(&tmp, 100);
        assert!(files.iter().any(|f| f == "a.txt"), "tracked file listed");
        assert!(
            files.iter().any(|f| f == "b.rs"),
            "untracked-not-ignored listed"
        );
        assert!(
            !files.iter().any(|f| f == "ignored.txt"),
            "gitignored excluded"
        );
        assert!(
            !files.iter().any(|f| f.starts_with(".git/")),
            ".git excluded"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_files_respects_limit() {
        if !git_available() {
            eprintln!("skip: git absent");
            return;
        }
        let tmp = std::env::temp_dir().join(format!("otto-ff-lim-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        git(&tmp, &["init", "-q"]);
        for i in 0..10 {
            fs::write(tmp.join(format!("f{i}.txt")), "x").unwrap();
        }
        let files = find_files(&tmp, 3);
        assert_eq!(files.len(), 3, "capped at limit");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_files_non_git_falls_back_to_walk() {
        let tmp = std::env::temp_dir().join(format!("otto-ff-nogit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".git")).unwrap(); // a bare ".git" dir name to skip, but not a repo
        fs::remove_dir_all(tmp.join(".git")).unwrap();
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("only.txt"), "x").unwrap();
        let files = find_files(&tmp, 100);
        assert!(
            files.iter().any(|f| f == "only.txt"),
            "walk fallback lists file"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_entries_lists_nested_dirs() {
        if !git_available() {
            eprintln!("skip: git absent");
            return;
        }
        let tmp = std::env::temp_dir().join(format!("otto-fe-nested-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src/deep")).unwrap();
        git(&tmp, &["init", "-q"]);
        fs::write(tmp.join("src/deep/a.rs"), "a").unwrap();
        git(&tmp, &["add", "src/deep/a.rs"]);

        let (files, dirs) = find_entries(&tmp, 100);
        assert!(files.iter().any(|f| f == "src/deep/a.rs"), "file listed");
        assert!(dirs.iter().any(|d| d == "src"), "top dir listed: {dirs:?}");
        assert!(
            dirs.iter().any(|d| d == "src/deep"),
            "nested dir listed: {dirs:?}"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_entries_excludes_dot_git_from_dirs() {
        if !git_available() {
            eprintln!("skip: git absent");
            return;
        }
        let tmp = std::env::temp_dir().join(format!("otto-fe-dotgit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        git(&tmp, &["init", "-q"]);
        fs::write(tmp.join("a.txt"), "a").unwrap();
        git(&tmp, &["add", "a.txt"]);

        let (_, dirs) = find_entries(&tmp, 100);
        assert!(
            !dirs.iter().any(|d| d == ".git" || d.starts_with(".git/")),
            ".git excluded from dirs: {dirs:?}"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_entries_excludes_gitignored_only_dir() {
        if !git_available() {
            eprintln!("skip: git absent");
            return;
        }
        let tmp = std::env::temp_dir().join(format!("otto-fe-ignoredir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("secret")).unwrap();
        git(&tmp, &["init", "-q"]);
        fs::write(tmp.join("secret/x.txt"), "x").unwrap();
        fs::write(tmp.join(".gitignore"), "secret/\n").unwrap();
        git(&tmp, &["add", ".gitignore"]);

        let (files, dirs) = find_entries(&tmp, 100);
        assert!(
            !files.iter().any(|f| f.starts_with("secret/")),
            "gitignored file excluded"
        );
        assert!(
            !dirs.iter().any(|d| d == "secret"),
            "gitignored-only dir excluded: {dirs:?}"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_entries_non_git_fallback_lists_dirs() {
        let tmp = std::env::temp_dir().join(format!("otto-fe-nogit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("nested/deep")).unwrap();
        fs::write(tmp.join("nested/deep/f.txt"), "x").unwrap();

        let (files, dirs) = find_entries(&tmp, 100);
        assert!(
            files.iter().any(|f| f == "nested/deep/f.txt"),
            "file listed"
        );
        assert!(
            dirs.iter().any(|d| d == "nested"),
            "top dir listed: {dirs:?}"
        );
        assert!(
            dirs.iter().any(|d| d == "nested/deep"),
            "nested dir listed: {dirs:?}"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_entries_dir_present_even_when_owning_file_truncated() {
        if !git_available() {
            eprintln!("skip: git absent");
            return;
        }
        let tmp = std::env::temp_dir().join(format!("otto-fe-trunc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("zzz")).unwrap();
        git(&tmp, &["init", "-q"]);
        for i in 0..9 {
            fs::write(tmp.join(format!("a{i}.txt")), "x").unwrap();
        }
        fs::write(tmp.join("zzz/deep.txt"), "x").unwrap();
        git(&tmp, &["add", "-A"]);

        // files sort alphabetically: a0..a8 come before zzz/deep.txt, so a
        // limit of 3 truncates the file list well before "zzz" is reached.
        let (files, dirs) = find_entries(&tmp, 3);
        assert_eq!(files.len(), 3, "files capped at limit");
        assert!(
            !files.iter().any(|f| f.starts_with("zzz")),
            "owning file truncated away: {files:?}"
        );
        assert!(
            dirs.iter().any(|d| d == "zzz"),
            "dir derived from full list survives file truncation: {dirs:?}"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_entries_caps_dirs_at_limit() {
        if !git_available() {
            eprintln!("skip: git absent");
            return;
        }
        let tmp = std::env::temp_dir().join(format!("otto-fe-dirlim-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        git(&tmp, &["init", "-q"]);
        for i in 0..5 {
            fs::create_dir_all(tmp.join(format!("dir{i}"))).unwrap();
            fs::write(tmp.join(format!("dir{i}/f.txt")), "x").unwrap();
        }
        git(&tmp, &["add", "-A"]);

        let (_, dirs) = find_entries(&tmp, 2);
        assert_eq!(
            dirs.len(),
            2,
            "dirs capped at limit independently: {dirs:?}"
        );
        let _ = fs::remove_dir_all(&tmp);
    }
}
