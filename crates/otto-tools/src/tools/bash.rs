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

use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::tool::{ExecuteResult, PermissionRequest, Tool, ToolContext, ToolError, decode_args};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

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

        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&params.command)
            .current_dir(&ctx.directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let out_task = tokio::spawn(async move {
            let mut b = Vec::new();
            let _ = stdout.read_to_end(&mut b).await;
            b
        });
        let err_task = tokio::spawn(async move {
            let mut b = Vec::new();
            let _ = stderr.read_to_end(&mut b).await;
            b
        });

        let mut exit_code: Option<i32> = None;
        let mut expired = false;
        let mut aborted = false;

        tokio::select! {
            status = child.wait() => {
                exit_code = status.ok().and_then(|s| s.code());
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(timeout)) => {
                expired = true;
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            _ = ctx.abort.cancelled() => {
                aborted = true;
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }

        let out = out_task.await.unwrap_or_default();
        let err = err_task.await.unwrap_or_default();
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

    #[tokio::test]
    async fn timeout_kills_command() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let start = std::time::Instant::now();
        let res = BashTool
            .execute(
                serde_json::json!({ "command": "sleep 5", "timeout": 100 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
        assert_eq!(res.metadata["expired"], serde_json::json!(true));
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
                .execute(serde_json::json!({ "command": "sleep 5" }), &ctx)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        token.cancel();
        let res = tokio::time::timeout(std::time::Duration::from_secs(3), handle)
            .await
            .expect("did not hang")
            .unwrap()
            .unwrap();
        assert_eq!(res.metadata["aborted"], serde_json::json!(true));
    }
}
