use std::path::Path;
use std::process::Command;

/// Enumerate workspace files under `root`, repo-relative, sorted, deduped, capped at `limit`.
/// Prefers `git ls-files` (tracked + untracked-not-ignored, .gitignore-respecting, .git
/// excluded); falls back to a recursive walk (skipping `.git`) for non-git dirs.
pub fn find_files(root: &Path, limit: usize) -> Vec<String> {
    let mut files = git_ls_files(root).unwrap_or_else(|| walk_files(root));
    files.sort();
    files.dedup();
    files.truncate(limit);
    files
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
}
