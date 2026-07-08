//! The `bash` tool — a port of the foreground path of opencode
//! `packages/opencode/src/tool/shell.ts`.
//!
//! Runs `sh -c <command>` in `ctx.directory`, honoring a millisecond `timeout`
//! and cooperative cancellation via `ctx.abort` (killing the child on either),
//! capturing combined stdout+stderr and the exit code. Asks the `bash`
//! permission first (shell.ts:283-291).
//!
//! TODO(phase-2+): background shells and the tree-sitter path-scan / PowerShell
//! handling from shell.ts are not ported; only the foreground path is here.

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;

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
fn kill_child_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .status();
    }
    let _ = child.start_kill();
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
}
