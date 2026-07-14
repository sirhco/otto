//! Spawns configured hook commands and resolves their verdict — fail-open on
//! any crash, bad JSON, or timeout (see the design doc's Failure Handling
//! section for the rationale).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use regex::Regex;
use tokio::io::AsyncWriteExt;

use crate::config::{HookCommand, HookMatcherGroup, HooksConfig};
use crate::event::{Decision, HookEvent, HookVerdict};

/// Default per-command timeout, overridable via `HookCommand::timeout_ms`.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, thiserror::Error)]
enum HookError {
    #[error("spawning hook process: {0}")]
    Io(#[from] std::io::Error),
    #[error("decoding hook stdout as JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hook exited nonzero (code {code:?}) with no output")]
    NonzeroExit { code: Option<i32> },
}

/// Fires configured lifecycle hooks and resolves the combined verdict.
pub struct HookRunner {
    config: HooksConfig,
    /// Compiled-regex cache keyed by matcher pattern string, populated
    /// lazily in [`Self::group_matches`]. `HookMatcherGroup` can't hold a
    /// compiled `Regex` directly without breaking its
    /// `Clone`/`PartialEq`/`Serialize`/`Deserialize` derives, so the cache
    /// lives here instead — avoids recompiling every configured matcher's
    /// regex on every `fire` call.
    regex_cache: Mutex<HashMap<String, Regex>>,
}

impl HookRunner {
    #[must_use]
    pub fn new(config: HooksConfig) -> Self {
        Self {
            config,
            regex_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `group` should run for `tool_id`, using (and populating) the
    /// compiled-regex cache. Same semantics as
    /// [`HookMatcherGroup::matches`] (`None` matcher/tool_id always matches,
    /// invalid regex never matches) — this is the cached hot-path
    /// equivalent used by [`Self::fire`].
    fn group_matches(&self, group: &HookMatcherGroup, tool_id: Option<&str>) -> bool {
        let (Some(pattern), Some(id)) = (&group.matcher, tool_id) else {
            return true;
        };
        let mut cache = self.regex_cache.lock().expect("regex cache mutex poisoned");
        if let Some(re) = cache.get(pattern) {
            return re.is_match(id);
        }
        match Regex::new(pattern) {
            Ok(re) => {
                let matched = re.is_match(id);
                cache.insert(pattern.clone(), re);
                matched
            }
            Err(_) => false,
        }
    }

    /// Run every matching hook for `event` in config order. The first
    /// non-`Allow` decision short-circuits the remaining hooks;
    /// `additional_context`/`system_message` from every hook run before that
    /// point are newline-concatenated onto the returned verdict.
    pub async fn fire(&self, event: HookEvent) -> HookVerdict {
        let mut verdict = HookVerdict::default();
        for group in self.config.groups_for(event.kind()) {
            if !self.group_matches(group, event.tool_id()) {
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
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().expect("stdin was piped");

        // Run stdin write and stdout read concurrently to avoid deadlock on large payloads.
        // If we write all stdin first, then read stdout, both sides can block on filled pipe buffers.
        let write_fut = async move {
            stdin.write_all(&payload).await?;
            drop(stdin); // close stdin so the child sees EOF
            Ok::<(), std::io::Error>(())
        };
        let output_fut = child.wait_with_output();
        let (_, output) = tokio::try_join!(write_fut, output_fut)?;

        if output.stdout.is_empty() {
            if !output.status.success() {
                return Err(HookError::NonzeroExit {
                    code: output.status.code(),
                });
            }
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

    #[tokio::test]
    async fn timeout_kills_process_quickly() {
        // Create a unique marker file path to prove process termination.
        let marker = std::env::temp_dir()
            .join(format!("otto-hooks-test-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker); // clean slate

        let cfg = cfg_with_pre_tool_use(vec![HookCommand {
            // Sleep 1s, then try to create a marker file.
            // If the timeout kills the process, the marker never appears.
            command: format!("sleep 1 && touch {}", marker.display()),
            timeout_ms: Some(50),
        }]);
        let start = std::time::Instant::now();
        let verdict = HookRunner::new(cfg).fire(pre_tool_use_event()).await;
        let elapsed = start.elapsed();
        assert_eq!(verdict.decision, Decision::Allow);
        // Should return quickly (well under the 1s sleep), proving the timeout fired.
        assert!(
            elapsed.as_millis() < 500,
            "timeout should return quickly after 50ms, got {elapsed:?}"
        );

        // Give the (supposedly killed) process ample time to have finished
        // if it had NOT been terminated. If the marker exists, the process
        // wasn't actually killed and this test fails.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            !marker.exists(),
            "marker file should not exist — proves process was killed, not just that timeout fired"
        );
        let _ = std::fs::remove_file(&marker); // cleanup
    }

    #[tokio::test]
    async fn nonzero_exit_with_valid_json_stdout_is_honored() {
        let cfg = cfg_with_pre_tool_use(vec![HookCommand {
            command: "echo '{\"decision\":\"deny\",\"reason\":\"hook failed\"}'; exit 1".to_string(),
            timeout_ms: None,
        }]);
        let verdict = HookRunner::new(cfg).fire(pre_tool_use_event()).await;
        // Even though exit code is 1, the JSON on stdout should be honored
        assert_eq!(verdict.decision, Decision::Deny);
        assert_eq!(verdict.reason.as_deref(), Some("hook failed"));
    }

    #[tokio::test]
    async fn concurrent_stdin_stdout_handles_large_payloads() {
        // Test that stdin write and stdout read are concurrent.
        // Without concurrency, a hook that outputs early and then drains stdin
        // would deadlock on large payloads (when payload > pipe buffer size).
        // The hook outputs immediately, then consumes stdin.
        // Wrapped in a timeout to ensure it doesn't hang.
        let large_args = serde_json::json!({"command": "echo hi; large_payload_here ".repeat(1000)});
        let event = HookEvent::PreToolUse {
            session_id: SessionId::from("ses_test"),
            tool_id: "bash".to_string(),
            args: large_args,
            cwd: std::path::PathBuf::from("/tmp"),
        };
        let cfg = cfg_with_pre_tool_use(vec![HookCommand {
            // Command outputs immediately, then reads stdin until EOF.
            // Without concurrent read, the large stdin payload would fill the
            // pipe buffer, blocking the parent's write, while the parent is blocked
            // waiting for this command to finish (which is blocked reading stdin).
            command: "sh -c 'echo \"{\\\"decision\\\":\\\"allow\\\"}\" >&1; cat > /dev/null'".to_string(),
            timeout_ms: Some(5000), // generous timeout
        }]);
        let start = std::time::Instant::now();
        let verdict = HookRunner::new(cfg).fire(event).await;
        let elapsed = start.elapsed();

        // Should succeed and be fast (well under the 5s timeout).
        assert_eq!(verdict.decision, Decision::Allow);
        assert!(
            elapsed.as_secs() < 2,
            "concurrent stdin/stdout should handle large payloads without deadlock, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn regex_is_cached_after_first_match() {
        let cfg = HooksConfig {
            pre_tool_use: vec![HookMatcherGroup {
                matcher: Some("^bash$".to_string()),
                hooks: vec![HookCommand {
                    command: "echo '{}'".to_string(),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        };
        let runner = HookRunner::new(cfg);

        let _ = runner.fire(pre_tool_use_event()).await;
        assert_eq!(
            runner.regex_cache.lock().unwrap().len(),
            1,
            "first fire compiles and caches the pattern"
        );

        let _ = runner.fire(pre_tool_use_event()).await;
        assert_eq!(
            runner.regex_cache.lock().unwrap().len(),
            1,
            "second fire reuses the cached regex rather than adding a new entry"
        );
    }

    #[tokio::test]
    async fn invalid_regex_is_not_cached_and_never_matches() {
        let cfg = HooksConfig {
            pre_tool_use: vec![HookMatcherGroup {
                matcher: Some("(unterminated".to_string()),
                hooks: vec![HookCommand {
                    command: "echo '{\"decision\":\"deny\"}'".to_string(),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        };
        let runner = HookRunner::new(cfg);
        let verdict = runner.fire(pre_tool_use_event()).await;
        assert_eq!(
            verdict.decision,
            Decision::Allow,
            "invalid regex never matches, so the denying hook never runs"
        );
        assert!(runner.regex_cache.lock().unwrap().is_empty());
    }
}
