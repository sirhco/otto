//! The per-session permission mode and its serde/cycle helpers.

use serde::{Deserialize, Serialize};
use crate::ruleset::{Action, Rule, Ruleset};

/// How aggressively tool calls are auto-approved for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Prompt for every tool action. The safe default.
    #[default]
    ApproveEach,
    /// Auto-approve file edits; still prompt for shell commands.
    AcceptEdits,
    /// Auto-approve everything except built-in dangerous operations.
    FullAuto,
}

impl PermissionMode {
    /// The next mode in the cycle order approve-each → accept-edits → full-auto → …
    #[must_use]
    pub fn cycle(self) -> Self {
        match self {
            Self::ApproveEach => Self::AcceptEdits,
            Self::AcceptEdits => Self::FullAuto,
            Self::FullAuto => Self::ApproveEach,
        }
    }

    /// The stable lowercase-kebab wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApproveEach => "approve-each",
            Self::AcceptEdits => "accept-edits",
            Self::FullAuto => "full-auto",
        }
    }

    /// Parse the wire string; unknown → `None`.
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "approve-each" => Some(Self::ApproveEach),
            "accept-edits" => Some(Self::AcceptEdits),
            "full-auto" => Some(Self::FullAuto),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_in_order_and_wraps() {
        assert_eq!(PermissionMode::ApproveEach.cycle(), PermissionMode::AcceptEdits);
        assert_eq!(PermissionMode::AcceptEdits.cycle(), PermissionMode::FullAuto);
        assert_eq!(PermissionMode::FullAuto.cycle(), PermissionMode::ApproveEach);
    }

    #[test]
    fn default_is_approve_each() {
        assert_eq!(PermissionMode::default(), PermissionMode::ApproveEach);
    }

    #[test]
    fn wire_string_round_trips() {
        for m in [
            PermissionMode::ApproveEach,
            PermissionMode::AcceptEdits,
            PermissionMode::FullAuto,
        ] {
            assert_eq!(PermissionMode::from_str_opt(m.as_str()), Some(m));
        }
        assert_eq!(PermissionMode::from_str_opt("nonsense"), None);
    }

    #[test]
    fn serde_uses_kebab_case() {
        let j = serde_json::to_string(&PermissionMode::FullAuto).unwrap();
        assert_eq!(j, "\"full-auto\"");
        let m: PermissionMode = serde_json::from_str("\"accept-edits\"").unwrap();
        assert_eq!(m, PermissionMode::AcceptEdits);
    }
}

fn rule(permission: &str, pattern: &str, action: Action) -> Rule {
    Rule { permission: permission.into(), pattern: pattern.into(), action }
}

/// The baseline ruleset a mode installs as the lowest-precedence layer.
#[must_use]
pub fn mode_overlay(mode: PermissionMode) -> Ruleset {
    match mode {
        PermissionMode::ApproveEach => Ruleset(vec![rule("*", "*", Action::Ask)]),
        PermissionMode::AcceptEdits => Ruleset(vec![
            rule("*", "*", Action::Ask),
            rule("edit", "*", Action::Allow),
            rule("write", "*", Action::Allow),
            // Note: the `apply_patch` tool requests under the `edit` permission,
            // so the `edit` rule above already covers it — no separate rule needed.
        ]),
        PermissionMode::FullAuto => Ruleset(vec![rule("*", "*", Action::Allow)]),
    }
}

/// Built-in dangerous patterns that always prompt (highest-precedence layer).
/// Conservative first pass; refine in review.
#[must_use]
pub fn danger_ruleset() -> Ruleset {
    let bash = [
        "*rm -rf*", "*rm -fr*",
        "*git push*--force*", "*git push*-f*",
        "*mkfs*", "*dd *of=/dev*", "*> /dev/sd*",
        "*chmod *777*", "*chmod -R *777*",
        "*curl*|*sh*", "*wget*|*sh*",
        "*sudo *",
    ];
    let files = ["*.env", "**/.env", "*id_rsa*", "**/.ssh/*", "*credentials*", "*.pem"];
    let mut rules = Vec::new();
    for p in bash {
        rules.push(rule("bash", p, Action::Ask));
    }
    // The `apply_patch` tool requests under the `edit` permission, so `edit`
    // covers its file writes too — no separate `apply_patch` name needed here.
    for p in files {
        for perm in ["edit", "write"] {
            rules.push(rule(perm, p, Action::Ask));
        }
    }
    Ruleset(rules)
}

#[cfg(test)]
mod overlay_tests {
    use super::*;
    use crate::ruleset::evaluate;

    #[test]
    fn full_auto_allows_normal_but_danger_layer_asks() {
        let overlay = mode_overlay(PermissionMode::FullAuto);
        let danger = danger_ruleset();
        let layers: [&Ruleset; 2] = [&overlay, &danger];
        // normal command: overlay allow, no danger match → Allow
        assert_eq!(evaluate(&layers, "bash", "cargo test").action, Action::Allow);
        // dangerous command: danger (last) wins → Ask
        assert_eq!(evaluate(&layers, "bash", "rm -rf build/").action, Action::Ask);
    }

    #[test]
    fn accept_edits_allows_edits_asks_bash() {
        let overlay = mode_overlay(PermissionMode::AcceptEdits);
        let layers: [&Ruleset; 1] = [&overlay];
        assert_eq!(evaluate(&layers, "edit", "src/main.rs").action, Action::Allow);
        assert_eq!(evaluate(&layers, "write", "a.txt").action, Action::Allow);
        assert_eq!(evaluate(&layers, "bash", "ls").action, Action::Ask);
    }

    #[test]
    fn approve_each_asks_everything() {
        let overlay = mode_overlay(PermissionMode::ApproveEach);
        let layers: [&Ruleset; 1] = [&overlay];
        assert_eq!(evaluate(&layers, "edit", "x").action, Action::Ask);
        assert_eq!(evaluate(&layers, "bash", "ls").action, Action::Ask);
    }

    #[test]
    fn config_deny_beats_full_auto() {
        // full layer order: [mode, config, approved, danger]
        let overlay = mode_overlay(PermissionMode::FullAuto);
        let config = Ruleset(vec![rule("bash", "*git push*", Action::Deny)]);
        let approved = Ruleset::new();
        let danger = danger_ruleset();
        let layers: [&Ruleset; 4] = [&overlay, &config, &approved, &danger];
        assert_eq!(evaluate(&layers, "bash", "git push origin").action, Action::Deny);
        // non-denied normal command still auto-allows
        assert_eq!(evaluate(&layers, "bash", "cargo build").action, Action::Allow);
    }
}
