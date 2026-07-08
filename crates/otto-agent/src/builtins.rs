//! Built-in agent definitions — port of the `agents` record in opencode
//! `agent/agent.ts` (agent.ts:140-265).
//!
//! Each built-in's permission is `merge(defaults, <agent-specific>)`, where
//! [`defaults`] ports the static portion of the base ruleset at agent.ts:119-136.
//!
//! Two families of the opencode base ruleset are **runtime-scoped** and are
//! therefore injected at session-build time rather than baked in here:
//! * the `external_directory` whitelist entries (`Truncate.GLOB`, the tmp dir,
//!   skill dirs, reference dirs — agent.ts:108-124), and
//! * the global user `cfg.permission` layer (`user`, applied at agent.ts).
//!
//! Prompt text is embedded from `assets/` via `include_str!`.

use otto_permission::{Ruleset, merge};
use serde_json::{Value, json};

use crate::agent::{AgentInfo, AgentMode};

/// Plan-mode system prompt. No opencode analogue file — opencode's plan agent
/// relies on the harness UI to surface plans; otto's TUI has no such surface,
/// so the prompt itself directs persisting the plan to `.opencode/plans/*.md`
/// (the one path plan mode may write) in the sdd-parseable `### Task N:` form.
pub const PROMPT_PLAN: &str = include_str!("../assets/plan.txt");
/// Explore agent system prompt (`agent/prompt/explore.txt`, agent.ts:214).
pub const PROMPT_EXPLORE: &str = include_str!("../assets/explore.txt");
/// Compaction agent system prompt (`agent/prompt/compaction.txt`, agent.ts:224).
pub const PROMPT_COMPACTION: &str = include_str!("../assets/compaction.txt");
/// Title agent system prompt (`agent/prompt/title.txt`, agent.ts:248).
pub const PROMPT_TITLE: &str = include_str!("../assets/title.txt");
/// Summary agent system prompt (`agent/prompt/summary.txt`, agent.ts:263).
pub const PROMPT_SUMMARY: &str = include_str!("../assets/summary.txt");

/// The static portion of the base permission ruleset — port of the
/// `Permission.fromConfig({...})` call at agent.ts:119-136.
///
/// The runtime `external_directory` whitelist (skill/tmp/reference dirs) is
/// reduced to its static base of `{ "*": "ask" }`; the whitelisted dirs are
/// merged in per-session downstream.
#[must_use]
pub fn defaults() -> Ruleset {
    Ruleset::from_config(&json!({
        "*": "allow",
        "doom_loop": "ask",
        "external_directory": { "*": "ask" },
        "question": "deny",
        "plan_enter": "deny",
        "plan_exit": "deny",
        // mirrors the Node.gitignore .env pattern (agent.ts:129-135)
        "read": {
            "*": "allow",
            "*.env": "ask",
            "*.env.*": "ask",
            "*.env.example": "allow"
        }
    }))
}

/// Convenience: `merge(defaults(), fromConfig(cfg))`.
fn with_defaults(cfg: &Value) -> Ruleset {
    merge(&[&defaults(), &Ruleset::from_config(cfg)])
}

/// Empty provider-options bag (`options: {}` on every built-in).
fn empty_options() -> Value {
    json!({})
}

/// The default primary coding agent — port of `build` (agent.ts:141-155).
///
/// Executes tools based on configured permissions; `defaults` allows `*`, so
/// with no user restrictions every tool is permitted.
#[must_use]
pub fn build() -> AgentInfo {
    AgentInfo {
        name: "build".into(),
        description: Some(
            "The default agent. Executes tools based on configured permissions.".into(),
        ),
        mode: AgentMode::Primary,
        native: true,
        hidden: false,
        top_p: None,
        temperature: None,
        color: None,
        permission: with_defaults(&json!({
            "question": "allow",
            "plan_enter": "allow"
        })),
        model: None,
        variant: None,
        prompt: None,
        options: empty_options(),
        steps: None,
    }
}

/// Plan-mode primary — port of `plan` (agent.ts:156-181).
///
/// Denies every edit (`edit: { "*": "deny" }`) except plan-mode markdown files.
/// The opencode ruleset also allows a worktree-relative path under
/// `Global.Path.data/plans` (agent.ts:174); that path is runtime-derived, so
/// only the static `.opencode/plans/*.md` exception is baked in here.
#[must_use]
pub fn plan() -> AgentInfo {
    AgentInfo {
        name: "plan".into(),
        description: Some("Plan mode. Disallows all edit tools.".into()),
        mode: AgentMode::Primary,
        native: true,
        hidden: false,
        top_p: None,
        temperature: None,
        color: None,
        permission: with_defaults(&json!({
            "question": "allow",
            "plan_exit": "allow",
            "task": { "general": "deny" },
            "edit": {
                "*": "deny",
                ".opencode/plans/*.md": "allow"
            }
        })),
        model: None,
        variant: None,
        prompt: Some(PROMPT_PLAN.into()),
        options: empty_options(),
        steps: None,
    }
}

/// General-purpose subagent — port of `general` (agent.ts:182-195).
///
/// Inherits `defaults` (all tools) but denies `todowrite`.
#[must_use]
pub fn general() -> AgentInfo {
    AgentInfo {
        name: "general".into(),
        description: Some(
            "General-purpose agent for researching complex questions and executing \
             multi-step tasks. Use this agent to execute multiple units of work in parallel."
                .into(),
        ),
        mode: AgentMode::Subagent,
        native: true,
        hidden: false,
        top_p: None,
        temperature: None,
        color: None,
        permission: with_defaults(&json!({ "todowrite": "deny" })),
        model: None,
        variant: None,
        prompt: None,
        options: empty_options(),
        steps: None,
    }
}

/// Read-only exploration subagent — port of `explore` (agent.ts:196-218).
///
/// Denies everything (`"*": "deny"`) then re-allows only the read-only tools
/// grep/glob/list/bash/webfetch/websearch/read. `external_directory` is reset
/// to its read-only base of `{ "*": "ask" }` (the `readonlyExternalDirectory`
/// of agent.ts:114-117, minus its runtime whitelist).
#[must_use]
pub fn explore() -> AgentInfo {
    AgentInfo {
        name: "explore".into(),
        description: Some(
            "Fast agent specialized for exploring codebases. Use this when you need to \
             quickly find files by patterns, search code for keywords, or answer questions \
             about the codebase. When calling this agent, specify the desired thoroughness \
             level: \"quick\", \"medium\", or \"very thorough\"."
                .into(),
        ),
        mode: AgentMode::Subagent,
        native: true,
        hidden: false,
        top_p: None,
        temperature: None,
        color: None,
        permission: with_defaults(&json!({
            "*": "deny",
            "grep": "allow",
            "glob": "allow",
            "list": "allow",
            "bash": "allow",
            "webfetch": "allow",
            "websearch": "allow",
            "read": "allow",
            "external_directory": { "*": "ask" }
        })),
        model: None,
        variant: None,
        prompt: Some(PROMPT_EXPLORE.into()),
        options: empty_options(),
        steps: None,
    }
}

/// Hidden compaction agent — port of `compaction` (agent.ts:219-233).
#[must_use]
pub fn compaction() -> AgentInfo {
    hidden_internal("compaction", PROMPT_COMPACTION, None)
}

/// Hidden title-generation agent — port of `title` (agent.ts:234-249).
#[must_use]
pub fn title() -> AgentInfo {
    hidden_internal("title", PROMPT_TITLE, Some(0.5))
}

/// Hidden summary agent — port of `summary` (agent.ts:250-264).
#[must_use]
pub fn summary() -> AgentInfo {
    hidden_internal("summary", PROMPT_SUMMARY, None)
}

/// Shared shape of the hidden internal primaries compaction/title/summary:
/// mode `primary`, `hidden: true`, permission `merge(defaults, { "*": "deny" })`.
fn hidden_internal(name: &str, prompt: &str, temperature: Option<f64>) -> AgentInfo {
    AgentInfo {
        name: name.into(),
        description: None,
        mode: AgentMode::Primary,
        native: true,
        hidden: true,
        top_p: None,
        temperature,
        color: None,
        permission: with_defaults(&json!({ "*": "deny" })),
        model: None,
        variant: None,
        prompt: Some(prompt.into()),
        options: empty_options(),
        steps: None,
    }
}

/// All built-in agents in declaration order — port of the `agents` record
/// key order (agent.ts:140-265): build, plan, general, explore, compaction,
/// title, summary.
#[must_use]
pub fn builtins() -> Vec<AgentInfo> {
    vec![
        build(),
        plan(),
        general(),
        explore(),
        compaction(),
        title(),
        summary(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_permission::{Action, Ruleset, evaluate};

    fn eval(rs: &Ruleset, permission: &str, pattern: &str) -> Action {
        evaluate(&[rs], permission, pattern).action
    }

    #[test]
    fn builtins_have_expected_modes_and_hidden() {
        let all = builtins();
        let by = |n: &str| all.iter().find(|a| a.name == n).unwrap().clone();

        assert_eq!(by("build").mode, AgentMode::Primary);
        assert!(!by("build").hidden);
        assert_eq!(by("plan").mode, AgentMode::Primary);
        assert_eq!(by("general").mode, AgentMode::Subagent);
        assert_eq!(by("explore").mode, AgentMode::Subagent);

        for name in ["compaction", "title", "summary"] {
            let a = by(name);
            assert_eq!(a.mode, AgentMode::Primary, "{name} mode");
            assert!(a.hidden, "{name} hidden");
        }
        assert!(all.iter().all(|a| a.native));
    }

    #[test]
    fn build_allows_everything() {
        let rs = build().permission;
        assert_eq!(eval(&rs, "edit", "src/main.rs"), Action::Allow);
        assert_eq!(eval(&rs, "bash", "ls"), Action::Allow);
        assert_eq!(eval(&rs, "question", "*"), Action::Allow);
    }

    #[test]
    fn explore_is_read_only() {
        let rs = explore().permission;
        // read-only tools allowed
        assert_eq!(eval(&rs, "read", "src/main.rs"), Action::Allow);
        assert_eq!(eval(&rs, "grep", "foo"), Action::Allow);
        assert_eq!(eval(&rs, "glob", "**/*.rs"), Action::Allow);
        assert_eq!(eval(&rs, "bash", "ls"), Action::Allow);
        // mutating tools denied
        assert_eq!(eval(&rs, "edit", "src/main.rs"), Action::Deny);
        assert_eq!(eval(&rs, "write", "src/main.rs"), Action::Deny);
        assert_eq!(eval(&rs, "apply_patch", "src/main.rs"), Action::Deny);
        assert_eq!(eval(&rs, "todowrite", "*"), Action::Deny);
        assert_eq!(eval(&rs, "task", "general"), Action::Deny);
    }

    #[test]
    fn plan_denies_edits_except_plan_files() {
        // The write/apply_patch tools map to the `edit` permission at gate
        // time; the ruleset only carries the `edit` key (agent.ts:171-175).
        let rs = plan().permission;
        assert_eq!(eval(&rs, "edit", "src/main.rs"), Action::Deny);
        assert_eq!(
            eval(&rs, "edit", ".opencode/plans/design.md"),
            Action::Allow
        );
        assert_eq!(eval(&rs, "plan_exit", "*"), Action::Allow);
    }

    /// The ruleset ALLOWS writing `.opencode/plans/*.md`, but without a system
    /// prompt nothing ever tells the model to do it — plan mode produced no
    /// plan file. The prompt must direct the agent to persist the final plan
    /// there, in the `### Task N:` shape the sdd workflow parses.
    #[test]
    fn plan_prompt_directs_writing_a_plan_file() {
        let prompt = plan().prompt.expect("plan agent carries a system prompt");
        assert!(
            prompt.contains(".opencode/plans/"),
            "prompt names the plans directory"
        );
        assert!(
            prompt.contains("### Task"),
            "prompt pins the sdd-parseable task heading format"
        );
    }

    #[test]
    fn general_denies_todowrite_but_allows_edit() {
        let rs = general().permission;
        assert_eq!(eval(&rs, "todowrite", "*"), Action::Deny);
        assert_eq!(eval(&rs, "edit", "src/main.rs"), Action::Allow);
    }

    #[test]
    fn hidden_internals_deny_all() {
        for a in [compaction(), title(), summary()] {
            assert_eq!(eval(&a.permission, "bash", "ls"), Action::Deny);
            assert_eq!(eval(&a.permission, "edit", "x"), Action::Deny);
        }
        assert_eq!(title().temperature, Some(0.5));
    }

    #[test]
    fn prompts_embed_non_empty() {
        assert!(!explore().prompt.unwrap().trim().is_empty());
        assert!(!compaction().prompt.unwrap().trim().is_empty());
        assert!(!title().prompt.unwrap().trim().is_empty());
        assert!(!summary().prompt.unwrap().trim().is_empty());
        // agents without a prompt override
        assert!(build().prompt.is_none());
        assert!(general().prompt.is_none());
    }
}
