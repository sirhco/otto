use std::path::{Path, PathBuf};

pub struct ServerInfo {
    pub id: &'static str,
    pub extensions: &'static [&'static str],
    pub command: &'static [&'static str],
    pub root_markers: &'static [&'static str],
}

pub fn builtin_servers() -> Vec<ServerInfo> {
    vec![
        ServerInfo {
            id: "typescript",
            extensions: &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"],
            command: &["typescript-language-server", "--stdio"],
            root_markers: &["package.json", "tsconfig.json"],
        },
        ServerInfo {
            id: "rust",
            extensions: &[".rs"],
            command: &["rust-analyzer"],
            root_markers: &["Cargo.toml"],
        },
        ServerInfo {
            id: "pyright",
            extensions: &[".py", ".pyi"],
            command: &["pyright-langserver", "--stdio"],
            root_markers: &["pyproject.toml", "setup.py", "requirements.txt"],
        },
        ServerInfo {
            id: "gopls",
            extensions: &[".go"],
            command: &["gopls"],
            root_markers: &["go.mod", "go.work"],
        },
    ]
}

/// ext → LSP languageId (subset of language.ts).
pub fn language_id(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "go" => "go",
        _ => "plaintext",
    }
}

/// Resolve `command[0]` against `$PATH` (which). No auto-install. None if missing.
pub fn resolve_command(command: &[&str]) -> Option<Vec<String>> {
    let bin = command.first()?;
    let found = which(bin)?;
    let mut out = vec![found.to_string_lossy().into_owned()];
    out.extend(command[1..].iter().map(|s| s.to_string()));
    Some(out)
}

fn which(bin: &str) -> Option<PathBuf> {
    // Absolute or explicitly-relative command: use as-is if it exists.
    let p = Path::new(bin);
    if p.is_absolute() || bin.contains('/') {
        return p.is_file().then(|| p.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Walk up from the file's dir to `ceiling`, returning the nearest dir containing
/// any marker. Falls back to `ceiling`. Port of server.ts NearestRoot.
pub fn nearest_root(file: &Path, ceiling: &Path, markers: &[&str]) -> PathBuf {
    let mut dir = file.parent().unwrap_or(ceiling);
    loop {
        for m in markers {
            if dir.join(m).exists() {
                return dir.to_path_buf();
            }
        }
        if dir == ceiling {
            break;
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }
    ceiling.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn matches_by_extension() {
        let servers = builtin_servers();
        let rust = servers.iter().find(|s| s.id == "rust").unwrap();
        assert!(rust.extensions.contains(&".rs"));
    }

    #[test]
    fn language_id_for_known_exts() {
        assert_eq!(language_id(Path::new("a/b.rs")), "rust");
        assert_eq!(language_id(Path::new("a/b.ts")), "typescript");
        assert_eq!(language_id(Path::new("a/b.py")), "python");
        assert_eq!(language_id(Path::new("a/b.unknown")), "plaintext");
    }

    #[test]
    fn nearest_root_finds_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("Cargo.toml"), "[package]").unwrap();
        let sub = root.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("main.rs");
        let found = nearest_root(&file, root, &["Cargo.toml"]);
        assert_eq!(found, root);
    }

    #[test]
    fn nearest_root_falls_back_to_ceiling() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("x/y.rs");
        assert_eq!(nearest_root(&file, tmp.path(), &["Cargo.toml"]), tmp.path());
    }
}
