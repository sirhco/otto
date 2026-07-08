//! The `bash` tool — a port of the foreground path of opencode
//! `packages/opencode/src/tool/shell.ts`.
//!
//! Runs `sh -c <command>` in `ctx.directory`, honoring a millisecond `timeout`
//! and cooperative cancellation via `ctx.abort` (killing the child on either),
//! capturing combined stdout+stderr and the exit code. Asks the `bash`
//! permission first (shell.ts:283-291), then scans the command string for
//! tokens that look like paths escaping `ctx.directory` and asks
//! `external_directory` for each ([`external_path_candidates`]) — the same
//! guard the filesystem tools apply via
//! [`crate::tools::assert_external_directory`], before the command spawns.
//!
//! This is a conservative **mitigation, not containment**: it is a
//! whitespace/token-level string scan, not a shell parser, so it does not see
//! through `$VAR` expansion, command substitution (`` $(...) ``/backticks),
//! or quoting/escaping tricks. It only catches literal paths typed on the
//! command line.
//!
//! TODO(phase-2+): string-scan ported; tree-sitter upgrade pending.
//! Background shells and PowerShell handling from shell.ts are still not
//! ported; only the foreground path is here.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use super::assert_external_directory;
use crate::tool::{ExecuteResult, PermissionRequest, Tool, ToolContext, ToolError, decode_args};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

/// How long to wait for the stdout/stderr pipes to reach EOF once the child
/// has exited (or been killed). A background/daemon grandchild can inherit
/// the pipe write ends and hold them open long after the shell is gone —
/// without this bound, `execute` blocked until that orphan exited.
const PIPE_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Incrementally drain `pipe` into `buf` until EOF. Incremental (rather than
/// `read_to_end` into a task-local Vec) so output captured before a drain
/// deadline is not lost when the task is abandoned.
fn drain_pipe(
    mut pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    buf: Arc<Mutex<Vec<u8>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
    })
}

/// Kill the child's entire process group, falling back to the direct child.
///
/// The shell frequently FORKS the command instead of exec'ing it (dash does,
/// bash does for compound commands), so signalling only `sh` leaves the real
/// work running as an orphan — the spawn puts the child in its own group
/// precisely so this can take the whole tree down. Group kill goes through
/// `/bin/kill` (a negated pid signals the group) because this crate forbids
/// `unsafe` and there is no safe killpg in std.
///
/// The `--` separator is load-bearing: without it, procps kill (Linux) parses
/// a bare `-<pid>` argument as an option, not a negative pid. The signal is
/// spelled `-KILL` (not `-9`) for the same portability reason.
fn kill_child_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .status();
    }
    let _ = child.start_kill();
}

/// The user's home directory, read fresh from `$HOME` at each call site — the
/// same `cfg(unix)`-for-v1 stance as [`kill_child_tree`]. `None` on other
/// platforms (and if `$HOME` is unset), which simply means nothing is ever
/// "under home" and [`external_path_candidates`] flags nothing.
#[cfg(unix)]
fn bash_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(not(unix))]
fn bash_home() -> Option<PathBuf> {
    None
}

/// Collapse `.` and lexical `..` components of `path` without touching the
/// filesystem — the same lexical stance as `contains_path` in
/// `crate::tools`. A `..` with nothing precedable to pop (already at the
/// root, or more `..`s than preceding components) is kept as-is rather than
/// erroring.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut stack: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(stack.last(), Some(Component::Normal(_))) {
                    stack.pop();
                } else {
                    stack.push(component);
                }
            }
            other => stack.push(other),
        }
    }
    stack.into_iter().collect()
}

/// Whether `resolved` sits under `home`, or is an ancestor `home` itself sits
/// under. The ancestor direction matters because a `..` escape from a
/// directory nested under `home` can climb *past* `home` towards the root
/// while still being a "close to home" escape worth flagging (e.g. two `..`
/// from a project two levels under home lands one level above home). A bare
/// root (`/`) is excluded from the ancestor check — every absolute path is
/// an ancestor of the root, which would defeat the whole point of the check.
fn under_home(resolved: &Path, home: Option<&Path>) -> bool {
    let Some(home) = home else { return false };
    resolved.starts_with(home) || (resolved.components().count() > 1 && home.starts_with(resolved))
}

/// Strip one layer of matching surrounding quotes and trailing punctuation
/// from a whitespace-delimited token.
fn clean_token(token: &str) -> &str {
    let mut token = token;
    if token.len() >= 2 {
        let bytes = token.as_bytes();
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            token = &token[1..token.len() - 1];
        }
    }
    token.trim_end_matches([',', ';', ':', ')', ']', '}'])
}

/// Scan `command` for tokens that look like paths escaping `directory` — the
/// pure-fn core of the bash sandbox check (see the module docs for why this
/// is a mitigation, not containment).
///
/// A token is a *candidate* when it is absolute (`/...`), tilde-relative
/// (`~`/`~/...`, expanded via `home`), or contains a lexical `..` escape
/// (resolved against `directory`; no filesystem access). Candidates are only
/// **flagged** when they resolve outside `directory` *and* [`under_home`] —
/// `/usr/bin/env`, `/etc/hosts`, `/opt/...` stay unflagged, avoiding prompt
/// fatigue for system-wide paths unrelated to the user's projects. Flag-shaped
/// (`-...`) tokens are split on `=` so `--flag=/x` values are checked. Results
/// are deduped by parent directory and capped at 5.
fn external_path_candidates(command: &str, directory: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    const MAX_CANDIDATES: usize = 5;
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen_parents: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for raw in command.split_whitespace() {
        // Split `--flag=/x` so the value is checked — but only for
        // flag-shaped (`-...`) tokens: a *path* whose name contains a
        // literal `=` (`../notes=v2.txt`) must keep its `/`/`..` prefix.
        let token = match raw.split_once('=') {
            Some((_, value)) if raw.starts_with('-') && !value.is_empty() => value,
            _ => raw,
        };
        let token = clean_token(token);
        if token.is_empty() || token.contains("://") || (token.contains('@') && token.contains(':'))
        {
            continue;
        }

        let resolved = if token == "~" {
            home.map(Path::to_path_buf)
        } else if let Some(rest) = token.strip_prefix("~/") {
            home.map(|h| lexical_normalize(&h.join(rest)))
        } else if token.starts_with('/') {
            Some(lexical_normalize(Path::new(token)))
        } else if Path::new(token)
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            Some(lexical_normalize(&directory.join(token)))
        } else {
            None
        };

        let Some(resolved) = resolved else { continue };
        if resolved.starts_with(directory) || !under_home(&resolved, home) {
            continue;
        }

        let parent = resolved
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| resolved.clone());
        if seen_parents.insert(parent) {
            out.push(resolved);
            if out.len() >= MAX_CANDIDATES {
                break;
            }
        }
    }

    out
}

#[derive(Debug, Deserialize)]
struct BashParams {
    command: String,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
}

/// The `bash` tool.
#[derive(Debug, Default, Clone, Copy)]
pub struct BashTool;

#[async_trait::async_trait]
impl Tool for BashTool {
    fn id(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/bash.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute" },
                "description": { "type": "string", "description": "A short (5-10 word) description of what the command does" },
                "timeout": { "type": "number", "description": "Optional timeout in milliseconds (max 600000)" }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: BashParams = decode_args(self.id(), args)?;
        let timeout = params
            .timeout
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        ctx.permission
            .ask(PermissionRequest {
                permission: "bash".to_string(),
                patterns: vec![params.command.clone()],
                always: vec![format!("{} *", params.command)],
                metadata: serde_json::json!({ "command": params.command }),
            })
            .await?;

        let home = bash_home();
        for candidate in external_path_candidates(&params.command, &ctx.directory, home.as_deref())
        {
            // "file" kind (the write/edit convention for unknown targets):
            // the always-allow glob and parentDir derive from the token's
            // parent directory. Scanned tokens skew heavily toward file
            // paths; the "directory" kind would glob `{file}/*`.
            assert_external_directory(ctx, &candidate, "file").await?;
        }

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(&params.command)
            .current_dir(&ctx.directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Own process group so timeout/abort can kill the child's whole tree
        // (see `kill_child_tree`), not just the `sh` wrapper.
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn()?;

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let out_buf = Arc::new(Mutex::new(Vec::new()));
        let err_buf = Arc::new(Mutex::new(Vec::new()));
        let out_task = drain_pipe(stdout, out_buf.clone());
        let err_task = drain_pipe(stderr, err_buf.clone());

        let mut exit_code: Option<i32> = None;
        let mut expired = false;
        let mut aborted = false;

        tokio::select! {
            status = child.wait() => {
                exit_code = status.ok().and_then(|s| s.code());
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(timeout)) => {
                expired = true;
                kill_child_tree(&mut child);
                let _ = child.wait().await;
            }
            _ = ctx.abort.cancelled() => {
                aborted = true;
                kill_child_tree(&mut child);
                let _ = child.wait().await;
            }
        }

        // The child is gone; give the pipes a bounded grace period to reach
        // EOF. A surviving background/daemon grandchild (spawned into a new
        // session, or left behind on the non-kill path) can hold the write
        // ends open indefinitely — output captured so far is kept either way.
        let _ = tokio::time::timeout(PIPE_DRAIN_GRACE, out_task).await;
        let _ = tokio::time::timeout(PIPE_DRAIN_GRACE, err_task).await;

        let out = std::mem::take(&mut *out_buf.lock().unwrap());
        let err = std::mem::take(&mut *err_buf.lock().unwrap());
        let mut output = String::new();
        output.push_str(&String::from_utf8_lossy(&out));
        output.push_str(&String::from_utf8_lossy(&err));
        if output.is_empty() {
            output = "(no output)".to_string();
        }

        // Surface RTK wrapping (see `crate::hooks::rtk`): when the command was
        // rewritten to run through `rtk`, its output is compacted — flag that so
        // the model does not read the trimmed output as the raw, complete result.
        if params.command.starts_with("rtk ") {
            output = format!("[rtk] output compacted by rtk\n\n{output}");
        }

        let mut meta_notes: Vec<String> = Vec::new();
        if expired {
            meta_notes.push(format!(
                "shell tool terminated command after exceeding timeout {timeout} ms. If this command is expected to take longer and is not waiting for interactive input, retry with a larger timeout value in milliseconds."
            ));
        }
        if aborted {
            meta_notes.push("User aborted the command".to_string());
        }
        if !meta_notes.is_empty() {
            output.push_str(&format!(
                "\n\n<shell_metadata>\n{}\n</shell_metadata>",
                meta_notes.join("\n")
            ));
        }

        Ok(
            ExecuteResult::new(params.command.clone(), output).with_metadata(serde_json::json!({
                "exit": exit_code,
                "aborted": aborted,
                "expired": expired,
            })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::RecordingGate;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    // -- external_path_candidates (pure fn) ---------------------------------

    #[test]
    fn simple_commands_have_no_candidates() {
        let dir = Path::new("/Users/x/proj");
        let home = Some(Path::new("/Users/x"));
        for cmd in ["echo hello", "ls -la", "git status"] {
            assert!(
                external_path_candidates(cmd, dir, home).is_empty(),
                "expected no candidates for {cmd:?}"
            );
        }
    }

    #[test]
    fn urls_and_scp_remotes_and_unrelated_absolute_paths_are_not_flagged() {
        let dir = Path::new("/Users/x/proj");
        let home = Some(Path::new("/Users/x"));
        for cmd in [
            "git clone https://github.com/a/b",
            "git@github.com:a/b.git",
            "/usr/bin/env python3",
        ] {
            assert!(
                external_path_candidates(cmd, dir, home).is_empty(),
                "expected no candidates for {cmd:?}"
            );
        }
    }

    #[test]
    fn absolute_path_outside_dir_under_home_is_flagged() {
        let dir = Path::new("/Users/x/proj");
        let home = Some(Path::new("/Users/x"));
        assert_eq!(
            external_path_candidates("cat /Users/x/other-repo/secret", dir, home),
            vec![PathBuf::from("/Users/x/other-repo/secret")]
        );
    }

    #[test]
    fn tilde_paths_are_flagged() {
        let dir = Path::new("/Users/x/proj");
        let home = Some(Path::new("/Users/x"));
        assert_eq!(
            external_path_candidates("cat ~/notes.txt", dir, home),
            vec![PathBuf::from("/Users/x/notes.txt")]
        );
        assert_eq!(
            external_path_candidates("cat ~", dir, home),
            vec![PathBuf::from("/Users/x")]
        );
    }

    #[test]
    fn dotdot_escapes_landing_under_home_are_flagged() {
        let dir = Path::new("/Users/x/proj");
        let home = Some(Path::new("/Users/x"));
        assert!(!external_path_candidates("cat ../sibling/file", dir, home).is_empty());
        assert!(!external_path_candidates("cd ..", dir, home).is_empty());
        assert!(!external_path_candidates("grep -r foo ../../", dir, home).is_empty());
    }

    #[test]
    fn flag_equals_value_is_split_and_flagged() {
        let dir = Path::new("/Users/x/proj");
        let home = Some(Path::new("/Users/x"));
        assert_eq!(
            external_path_candidates("--output=/Users/x/elsewhere/f", dir, home),
            vec![PathBuf::from("/Users/x/elsewhere/f")]
        );
    }

    #[test]
    fn escaping_path_containing_equals_is_still_flagged() {
        // The `=`-split must apply only to flag-shaped (`-...`) tokens: a
        // path whose *name* contains a literal `=` must not lose its leading
        // `/` or `..` and slip through unflagged.
        let dir = Path::new("/Users/x/proj");
        let home = Some(Path::new("/Users/x"));
        assert_eq!(
            external_path_candidates("cat /Users/x/other-repo/notes=v2.txt", dir, home),
            vec![PathBuf::from("/Users/x/other-repo/notes=v2.txt")]
        );
        assert_eq!(
            external_path_candidates("cat ../notes=v2.txt", dir, home),
            vec![PathBuf::from("/Users/x/notes=v2.txt")]
        );
    }

    #[test]
    fn paths_that_resolve_inside_directory_are_not_flagged() {
        let dir = Path::new("/Users/x/proj");
        let home = Some(Path::new("/Users/x"));
        assert!(external_path_candidates("cat /Users/x/proj/src/main.rs", dir, home).is_empty());
        assert!(external_path_candidates("cat ./src/../src/main.rs", dir, home).is_empty());
    }

    #[test]
    fn candidates_are_deduped_by_parent_and_capped() {
        let dir = Path::new("/Users/x/proj");
        let home = Some(Path::new("/Users/x"));
        let cmd = "cat /Users/x/a/1 /Users/x/a/2 /Users/x/b/1 /Users/x/c/1 /Users/x/d/1 /Users/x/e/1 /Users/x/f/1";
        let got = external_path_candidates(cmd, dir, home);
        // Six distinct parents (a-f) in the command; the cap must actually
        // bite at 5 and drop the sixth (/Users/x/f/1).
        assert_eq!(
            got,
            vec![
                PathBuf::from("/Users/x/a/1"),
                PathBuf::from("/Users/x/b/1"),
                PathBuf::from("/Users/x/c/1"),
                PathBuf::from("/Users/x/d/1"),
                PathBuf::from("/Users/x/e/1"),
            ]
        );
    }

    #[test]
    fn no_home_means_nothing_is_flagged() {
        let dir = Path::new("/Users/x/proj");
        assert!(external_path_candidates("cat /Users/x/other-repo/secret", dir, None).is_empty());
        assert!(external_path_candidates("cat ~/notes.txt", dir, None).is_empty());
    }

    // -- integration: BashTool::execute wiring -------------------------------

    #[tokio::test]
    async fn echo_round_trip_and_permission() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(RecordingGate::allow());
        let ctx = ToolContext::builder(dir.path())
            .permission(gate.clone())
            .build();
        let res = BashTool
            .execute(serde_json::json!({ "command": "echo hello" }), &ctx)
            .await
            .unwrap();
        assert!(res.output.contains("hello"));
        assert!(gate.asked_for("bash"));
        assert_eq!(res.metadata["exit"], serde_json::json!(0));
    }

    #[tokio::test]
    async fn rtk_wrapped_command_gets_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        // Runs `sh -c "rtk ..."`; whether rtk is installed or not, the prefix is
        // driven by the command string, so the marker must be present.
        let res = BashTool
            .execute(serde_json::json!({ "command": "rtk echo hello" }), &ctx)
            .await
            .unwrap();
        assert!(res.output.starts_with("[rtk] output compacted by rtk"));
    }

    #[tokio::test]
    async fn unwrapped_command_has_no_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let res = BashTool
            .execute(serde_json::json!({ "command": "echo hello" }), &ctx)
            .await
            .unwrap();
        assert!(!res.output.contains("[rtk]"));
    }

    #[tokio::test]
    async fn non_zero_exit_captured() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let res = BashTool
            .execute(serde_json::json!({ "command": "exit 3" }), &ctx)
            .await
            .unwrap();
        assert_eq!(res.metadata["exit"], serde_json::json!(3));
    }

    // Timing margins: the sleep is 30s and the "was killed" ceilings are 15s,
    // so a kill/abort (sub-second locally) and a run-to-completion stay
    // clearly distinguishable even on badly loaded CI runners, where process
    // spawn alone has been observed to blow a 3s budget.

    #[tokio::test]
    async fn timeout_kills_command() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let start = std::time::Instant::now();
        let res = BashTool
            .execute(
                serde_json::json!({ "command": "sleep 30", "timeout": 100 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(15),
            "command was not killed by the timeout (ran {:?})",
            start.elapsed()
        );
        assert_eq!(res.metadata["expired"], serde_json::json!(true));
    }

    /// The timeout must kill the whole process GROUP, not just the `sh`
    /// wrapper: a compound command makes the shell fork the child instead of
    /// exec'ing it (dash on Linux does this even for simple commands), and
    /// killing only `sh` leaves an orphan running — and holding the stdout
    /// pipe open, which blocked `execute` until the orphan exited.
    #[tokio::test]
    async fn timeout_kills_forked_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let start = std::time::Instant::now();
        // `sleep 30; echo done` forces the shell to fork `sleep` (no exec
        // optimization), reproducing the Linux CI behavior everywhere.
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            BashTool.execute(
                serde_json::json!({ "command": "sleep 30; echo done", "timeout": 100 }),
                &ctx,
            ),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "execute hung on the orphaned grandchild ({:?})",
                start.elapsed()
            )
        })
        .unwrap();
        assert_eq!(res.metadata["expired"], serde_json::json!(true));
    }

    /// A command that leaves a BACKGROUND child behind must not hold
    /// `execute` hostage: the shell exits immediately, but the background
    /// child inherits the stdout pipe, and reading to EOF would block until
    /// it dies.
    #[tokio::test]
    async fn background_child_does_not_hold_pipes_open() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            BashTool.execute(
                serde_json::json!({ "command": "sleep 30 & echo started" }),
                &ctx,
            ),
        )
        .await
        .expect("execute must return when the shell exits, not when its background child does")
        .unwrap();
        assert!(res.output.contains("started"));
        assert_eq!(res.metadata["exit"], serde_json::json!(0));
    }

    #[tokio::test]
    async fn abort_via_cancellation_token() {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let ctx = ToolContext::builder(dir.path())
            .abort(token.clone())
            .build();
        let handle = tokio::spawn(async move {
            BashTool
                .execute(serde_json::json!({ "command": "sleep 30" }), &ctx)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        token.cancel();
        let res = tokio::time::timeout(std::time::Duration::from_secs(15), handle)
            .await
            .expect("did not hang")
            .unwrap()
            .unwrap();
        assert_eq!(res.metadata["aborted"], serde_json::json!(true));
    }

    // -- integration: external_directory wiring ------------------------------
    //
    // These tests need a real path that is genuinely "under home" per
    // `bash_home()` (which reads the real `$HOME`), so `ctx.directory` is a
    // tempdir created *inside* `$HOME` rather than the usual
    // `tempfile::tempdir()` (which lands outside `$HOME`, e.g.
    // `/var/folders/...` or `/tmp` — exactly why the rest of this file's
    // tests stay green: none of their commands reference an outside-dir path
    // that is also under home).

    #[tokio::test]
    async fn command_touching_path_under_home_asks_external_directory_and_still_runs() {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME must be set"));
        let dir = tempfile::Builder::new()
            .prefix("otto-bash-test-")
            .tempdir_in(&home)
            .unwrap();
        let gate = Arc::new(RecordingGate::allow());
        let ctx = ToolContext::builder(dir.path())
            .permission(gate.clone())
            .build();
        let outside = home.join("otto-bash-test-outside-marker");

        let res = BashTool
            .execute(
                serde_json::json!({ "command": format!("echo hi {}", outside.display()) }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(gate.asked_for("external_directory"));
        assert!(res.output.contains("hi"));

        // The scanned token is a *file*-shaped path, so the ask must use the
        // "file" kind: the always-allow glob and parentDir come from the
        // token's PARENT directory ({home}/*), not from the token itself
        // ({file}/* — which would corrupt the persisted "always allow this
        // directory" flow).
        let reqs = gate.requests_for("external_directory");
        assert_eq!(reqs.len(), 1);
        let want_glob = format!("{}/*", home.display());
        assert_eq!(reqs[0].patterns, vec![want_glob.clone()]);
        assert_eq!(reqs[0].always, vec![want_glob]);
        assert_eq!(
            reqs[0].metadata["parentDir"],
            serde_json::json!(home.display().to_string())
        );
    }

    #[tokio::test]
    async fn echo_hello_does_not_ask_external_directory() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(RecordingGate::allow());
        let ctx = ToolContext::builder(dir.path())
            .permission(gate.clone())
            .build();
        BashTool
            .execute(serde_json::json!({ "command": "echo hello" }), &ctx)
            .await
            .unwrap();
        assert!(!gate.asked_for("external_directory"));
    }

    #[tokio::test]
    async fn denied_external_directory_blocks_command_before_it_runs() {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME must be set"));
        let dir = tempfile::Builder::new()
            .prefix("otto-bash-test-")
            .tempdir_in(&home)
            .unwrap();
        let gate = Arc::new(RecordingGate::deny("external_directory"));
        let ctx = ToolContext::builder(dir.path())
            .permission(gate.clone())
            .build();
        let marker = home.join(format!("otto-bash-test-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        let result = BashTool
            .execute(
                serde_json::json!({ "command": format!("touch {}", marker.display()) }),
                &ctx,
            )
            .await;

        assert!(
            result.is_err(),
            "expected the denial to propagate as an error"
        );
        assert!(
            !marker.exists(),
            "command must not have run — the denied path was never touched"
        );
        let _ = std::fs::remove_file(&marker);
    }
}
