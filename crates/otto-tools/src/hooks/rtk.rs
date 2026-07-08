//! [`RtkHook`] — routes shell commands through the RTK (Rust Token Killer)
//! proxy, which compacts noisy dev-command output (`git status` →
//! `rtk git status`, 60–90% fewer tokens).
//!
//! The hook only touches the `bash` tool and is conservative: it prefixes
//! `rtk ` onto *simple single commands* only. A command containing a shell
//! control operator (a pipe, `&&`, redirection, subshell, …) is left untouched,
//! because `rtk` wraps one command — prefixing a whole pipeline would run
//! `rtk` against only the first stage and reshape the rest. RTK is auto-detected
//! on `PATH`; if it is missing (or the feature is disabled) the hook is inert
//! and commands run raw.

use std::path::Path;

use serde_json::Value;

use crate::hook::{HookOutcome, ToolHook};
use crate::tool::ToolContext;

/// Shell metacharacters that mean the command is more than a single program
/// invocation. Any of these present → do not wrap.
const SHELL_OPERATORS: &[&str] = &["|", "&", ";", ">", "<", "`", "$(", "\n", "\\", "(", ")"];

/// A [`ToolHook`] that rewrites `bash` commands to run through `rtk`.
pub struct RtkHook {
    active: bool,
}

impl RtkHook {
    /// Build the hook. When `enabled`, probe `PATH` for an `rtk` executable;
    /// the hook is only `active` when both are true. When enabled but `rtk` is
    /// not found, log a one-time hint and stay inert.
    pub fn new(enabled: bool) -> Self {
        if !enabled {
            return Self { active: false };
        }
        let found = rtk_on_path();
        if !found {
            eprintln!(
                "otto: rtk.enabled is set but `rtk` was not found on PATH; shell commands run unwrapped"
            );
        }
        Self { active: found }
    }

    /// Construct with an explicit `active` state, bypassing the `PATH` probe.
    /// For tests.
    pub fn with_active(active: bool) -> Self {
        Self { active }
    }
}

#[async_trait::async_trait]
impl ToolHook for RtkHook {
    async fn before_execute(
        &self,
        tool_id: &str,
        mut args: Value,
        _ctx: &ToolContext,
    ) -> HookOutcome {
        if !self.active || tool_id != "bash" {
            return HookOutcome::Continue(args);
        }
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return HookOutcome::Continue(args);
        };
        if let Some(wrapped) = wrap_command(command) {
            args["command"] = Value::String(wrapped);
        }
        HookOutcome::Continue(args)
    }
}

/// Return `Some(rewritten)` when `command` should be prefixed with `rtk `, or
/// `None` to leave it as-is. Idempotent (already-`rtk ` commands pass through)
/// and conservative (compound shell commands pass through).
fn wrap_command(command: &str) -> Option<String> {
    let trimmed = command.trim_start();
    if trimmed.is_empty() || trimmed.starts_with("rtk ") {
        return None;
    }
    if SHELL_OPERATORS.iter().any(|op| command.contains(op)) {
        return None;
    }
    Some(format!("rtk {command}"))
}

/// Whether an executable named `rtk` exists on `PATH`. Splits `PATH` on the
/// platform separator and checks each directory for an `rtk` file — no `which`
/// crate, no subprocess.
fn rtk_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable_file(&dir.join(rtk_bin_name())))
}

#[cfg(windows)]
fn rtk_bin_name() -> &'static str {
    "rtk.exe"
}

#[cfg(not(windows))]
fn rtk_bin_name() -> &'static str {
    "rtk"
}

fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext::builder(std::env::temp_dir()).build()
    }

    async fn run(hook: &RtkHook, tool: &str, command: &str) -> String {
        let args = serde_json::json!({ "command": command });
        match hook.before_execute(tool, args, &ctx()).await {
            HookOutcome::Continue(v) => v["command"].as_str().unwrap_or_default().to_string(),
            HookOutcome::Deny(_) => panic!("RtkHook never denies"),
        }
    }

    #[tokio::test]
    async fn wraps_simple_bash_command_when_active() {
        let hook = RtkHook::with_active(true);
        assert_eq!(run(&hook, "bash", "git status").await, "rtk git status");
    }

    #[tokio::test]
    async fn leaves_compound_command_raw() {
        let hook = RtkHook::with_active(true);
        assert_eq!(run(&hook, "bash", "git status && npm test").await, "git status && npm test");
        assert_eq!(run(&hook, "bash", "cat a | grep b").await, "cat a | grep b");
        assert_eq!(run(&hook, "bash", "echo hi > out.txt").await, "echo hi > out.txt");
    }

    #[tokio::test]
    async fn idempotent_on_already_wrapped() {
        let hook = RtkHook::with_active(true);
        assert_eq!(run(&hook, "bash", "rtk git status").await, "rtk git status");
    }

    #[tokio::test]
    async fn ignores_non_bash_tool() {
        let hook = RtkHook::with_active(true);
        assert_eq!(run(&hook, "read", "git status").await, "git status");
    }

    #[tokio::test]
    async fn inert_when_not_active() {
        let hook = RtkHook::with_active(false);
        assert_eq!(run(&hook, "bash", "git status").await, "git status");
    }
}
