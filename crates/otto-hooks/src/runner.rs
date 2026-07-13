//! Spawns configured hook commands and resolves their verdict — fail-open on
//! any crash, bad JSON, or timeout (see the design doc's Failure Handling
//! section for the rationale).

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use crate::config::{HookCommand, HooksConfig};
use crate::event::{Decision, HookEvent, HookVerdict};

/// Default per-command timeout, overridable via `HookCommand::timeout_ms`.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, thiserror::Error)]
enum HookError {
    #[error("spawning hook process: {0}")]
    Io(#[from] std::io::Error),
    #[error("decoding hook stdout as JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Fires configured lifecycle hooks and resolves the combined verdict.
pub struct HookRunner {
    config: HooksConfig,
}

impl HookRunner {
    #[must_use]
    pub fn new(config: HooksConfig) -> Self {
        Self { config }
    }

    /// Run every matching hook for `event` in config order. The first
    /// non-`Allow` decision short-circuits the remaining hooks;
    /// `additional_context`/`system_message` from every hook run before that
    /// point are newline-concatenated onto the returned verdict.
    pub async fn fire(&self, event: HookEvent) -> HookVerdict {
        let mut verdict = HookVerdict::default();
        for group in self.config.groups_for(event.kind()) {
            if !group.matches(event.tool_id()) {
                continue;
            }
            for cmd in &group.hooks {
                let v = self.run_one(cmd, &event).await;
                if let Some(ctx) = v.additional_context {
                    verdict.additional_context = Some(match verdict.additional_context.take() {
                        Some(existing) => format!("{existing}\n{ctx}"),
                        None => ctx,
                    });
                }
                if let Some(msg) = v.system_message {
                    verdict.system_message = Some(match verdict.system_message.take() {
                        Some(existing) => format!("{existing}\n{msg}"),
                        None => msg,
                    });
                }
                if v.decision != Decision::Allow {
                    verdict.decision = v.decision;
                    verdict.reason = v.reason;
                    return verdict;
                }
            }
        }
        verdict
    }

    async fn run_one(&self, cmd: &HookCommand, event: &HookEvent) -> HookVerdict {
        let timeout = Duration::from_millis(cmd.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        match tokio::time::timeout(timeout, Self::spawn_and_read(cmd, event)).await {
            Ok(Ok(verdict)) => verdict,
            Ok(Err(err)) => {
                tracing::warn!(
                    command = %cmd.command,
                    event = event.kind().as_str(),
                    error = %err,
                    "hook failed; failing open (allow)"
                );
                HookVerdict::default()
            }
            Err(_elapsed) => {
                tracing::warn!(
                    command = %cmd.command,
                    event = event.kind().as_str(),
                    timeout_ms = timeout.as_millis() as u64,
                    "hook timed out; failing open (allow)"
                );
                HookVerdict::default()
            }
        }
    }

    async fn spawn_and_read(cmd: &HookCommand, event: &HookEvent) -> Result<HookVerdict, HookError> {
        let payload = serde_json::to_vec(&event.to_stdin_json())?;
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&payload).await?;
        }
        let output = child.wait_with_output().await?;
        if output.stdout.is_empty() {
            return Ok(HookVerdict::default());
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HookMatcherGroup;
    use otto_id::SessionId;

    fn cfg_with_pre_tool_use(hooks: Vec<HookCommand>) -> HooksConfig {
        HooksConfig {
            pre_tool_use: vec![HookMatcherGroup {
                matcher: None,
                hooks,
            }],
            ..Default::default()
        }
    }

    fn pre_tool_use_event() -> HookEvent {
        HookEvent::PreToolUse {
            session_id: SessionId::from("ses_test"),
            tool_id: "bash".to_string(),
            args: serde_json::json!({"command": "echo hi"}),
            cwd: std::path::PathBuf::from("/tmp"),
        }
    }

    #[tokio::test]
    async fn allows_when_no_hooks_configured() {
        let runner = HookRunner::new(HooksConfig::default());
        let verdict = runner.fire(pre_tool_use_event()).await;
        assert_eq!(verdict.decision, Decision::Allow);
    }

    #[tokio::test]
    async fn deny_from_hook_stdout_short_circuits() {
        let cfg = cfg_with_pre_tool_use(vec![HookCommand {
            command: "echo '{\"decision\":\"deny\",\"reason\":\"blocked\"}'".to_string(),
            timeout_ms: None,
        }]);
        let verdict = HookRunner::new(cfg).fire(pre_tool_use_event()).await;
        assert_eq!(verdict.decision, Decision::Deny);
        assert_eq!(verdict.reason.as_deref(), Some("blocked"));
    }

    #[tokio::test]
    async fn timeout_fails_open() {
        let cfg = cfg_with_pre_tool_use(vec![HookCommand {
            command: "sleep 5".to_string(),
            timeout_ms: Some(50),
        }]);
        let verdict = HookRunner::new(cfg).fire(pre_tool_use_event()).await;
        assert_eq!(verdict.decision, Decision::Allow);
    }

    #[tokio::test]
    async fn nonzero_exit_with_no_stdout_fails_open() {
        let cfg = cfg_with_pre_tool_use(vec![HookCommand {
            command: "exit 1".to_string(),
            timeout_ms: None,
        }]);
        let verdict = HookRunner::new(cfg).fire(pre_tool_use_event()).await;
        assert_eq!(verdict.decision, Decision::Allow);
    }

    #[tokio::test]
    async fn unparseable_stdout_fails_open() {
        let cfg = cfg_with_pre_tool_use(vec![HookCommand {
            command: "echo 'not json'".to_string(),
            timeout_ms: None,
        }]);
        let verdict = HookRunner::new(cfg).fire(pre_tool_use_event()).await;
        assert_eq!(verdict.decision, Decision::Allow);
    }

    #[tokio::test]
    async fn non_matching_tool_id_skips_the_hook() {
        let cfg = HooksConfig {
            pre_tool_use: vec![HookMatcherGroup {
                matcher: Some("^edit$".to_string()),
                hooks: vec![HookCommand {
                    command: "echo '{\"decision\":\"deny\"}'".to_string(),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        };
        // event's tool_id is "bash", matcher is "^edit$" — must not match.
        let verdict = HookRunner::new(cfg).fire(pre_tool_use_event()).await;
        assert_eq!(verdict.decision, Decision::Allow);
    }

    #[tokio::test]
    async fn multiple_hooks_concatenate_system_message() {
        let cfg = cfg_with_pre_tool_use(vec![
            HookCommand {
                command: "echo '{\"system_message\":\"first\"}'".to_string(),
                timeout_ms: None,
            },
            HookCommand {
                command: "echo '{\"system_message\":\"second\"}'".to_string(),
                timeout_ms: None,
            },
        ]);
        let verdict = HookRunner::new(cfg).fire(pre_tool_use_event()).await;
        assert_eq!(verdict.system_message.as_deref(), Some("first\nsecond"));
    }

    #[tokio::test]
    async fn first_deny_short_circuits_remaining_hooks() {
        let cfg = cfg_with_pre_tool_use(vec![
            HookCommand {
                command: "echo '{\"decision\":\"deny\",\"reason\":\"first\"}'".to_string(),
                timeout_ms: None,
            },
            HookCommand {
                command: "echo '{\"decision\":\"deny\",\"reason\":\"second\"}'".to_string(),
                timeout_ms: None,
            },
        ]);
        let verdict = HookRunner::new(cfg).fire(pre_tool_use_event()).await;
        assert_eq!(verdict.reason.as_deref(), Some("first"));
    }
}
