//! Subagent session-permission derivation — port of opencode
//! `agent/subagent-permissions.ts` (`deriveSubagentSessionPermission`).

use otto_permission::{Action, Rule, Ruleset};

use crate::agent::AgentInfo;

/// Build the permission ruleset for a subagent's session when it is spawned
/// via the `task` tool — port of `deriveSubagentSessionPermission`
/// (subagent-permissions.ts:14-27).
///
/// The result narrows the child's capabilities by combining:
///
/// 1. The parent session's `external_directory` rules **and** every `deny`
///    rule (subagent-permissions.ts:21-23). Non-deny parent rules are dropped:
///    parent restrictions only govern the parent, while the subagent's own
///    ruleset (already baked into `subagent.permission`) determines what it may
///    do. Only the parent's denies and directory scoping are inherited.
/// 2. A default `todowrite: "*" -> deny` unless the subagent's own ruleset
///    already contains a `todowrite` rule (subagent-permissions.ts:19, 24).
/// 3. A default `task: "*" -> deny` unless the subagent's own ruleset already
///    contains a `task` rule (subagent-permissions.ts:18, 25).
///
/// `some((rule) => rule.permission === "task")` matches a rule of **any**
/// action, so a subagent that references `task`/`todowrite` at all — even to
/// deny a specific pattern — opts out of the blanket deny and keeps control.
#[must_use]
pub fn derive_subagent_permission(parent: &Ruleset, subagent: &AgentInfo) -> Ruleset {
    let can_task = subagent.permits_key("task");
    let can_todo = subagent.permits_key("todowrite");

    let mut rules: Vec<Rule> = parent
        .rules()
        .iter()
        .filter(|rule| rule.permission == "external_directory" || rule.action == Action::Deny)
        .cloned()
        .collect();

    if !can_todo {
        rules.push(Rule {
            permission: "todowrite".into(),
            pattern: "*".into(),
            action: Action::Deny,
        });
    }
    if !can_task {
        rules.push(Rule {
            permission: "task".into(),
            pattern: "*".into(),
            action: Action::Deny,
        });
    }

    Ruleset(rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::general;
    use otto_permission::{evaluate, Action};
    use serde_json::json;

    fn contains(rs: &Ruleset, permission: &str, action: Action) -> bool {
        rs.rules()
            .iter()
            .any(|r| r.permission == permission && r.action == action && r.pattern == "*")
    }

    #[test]
    fn parent_allow_all_still_denies_todo_and_task_by_default() {
        // A subagent that allows everything and references neither todowrite
        // nor task in its own ruleset.
        let mut subagent = general();
        subagent.permission = Ruleset::from_config(&json!({ "*": "allow" }));
        let parent = Ruleset::from_config(&json!({ "*": "allow" }));
        let derived = derive_subagent_permission(&parent, &subagent);

        // Both blanket denies are appended by default.
        assert!(contains(&derived, "task", Action::Deny));
        assert!(contains(&derived, "todowrite", Action::Deny));

        // The child session = subagent's own permission + derived. Even though
        // both parent and child allow everything, todowrite/task are denied.
        let session = [&subagent.permission, &derived];
        assert_eq!(evaluate(&session, "todowrite", "*").action, Action::Deny);
        assert_eq!(evaluate(&session, "task", "general").action, Action::Deny);
        // Other tools stay allowed.
        assert_eq!(evaluate(&session, "edit", "x").action, Action::Allow);
    }

    #[test]
    fn general_subagent_denies_todo_and_task() {
        // general's own ruleset already denies todowrite (so no extra blanket
        // todowrite deny is appended) but has no task rule (task appended).
        let parent = Ruleset::new();
        let subagent = general();
        let derived = derive_subagent_permission(&parent, &subagent);
        assert!(!contains(&derived, "todowrite", Action::Deny));
        assert!(contains(&derived, "task", Action::Deny));

        let session = [&subagent.permission, &derived];
        assert_eq!(evaluate(&session, "todowrite", "*").action, Action::Deny);
        assert_eq!(evaluate(&session, "task", "general").action, Action::Deny);
    }

    #[test]
    fn subagent_granting_todowrite_keeps_it() {
        let mut subagent = general();
        // Explicitly grant todowrite in the subagent's own ruleset.
        subagent.permission = Ruleset::from_config(&json!({ "todowrite": "allow" }));
        let parent = Ruleset::new();
        let derived = derive_subagent_permission(&parent, &subagent);
        // No blanket todowrite deny appended.
        assert!(!contains(&derived, "todowrite", Action::Deny));
        // task still denied (no task rule present).
        assert!(contains(&derived, "task", Action::Deny));

        let session = [&subagent.permission, &derived];
        assert_eq!(evaluate(&session, "todowrite", "*").action, Action::Allow);
        assert_eq!(evaluate(&session, "task", "general").action, Action::Deny);
    }

    #[test]
    fn inherits_parent_deny_and_external_directory_only() {
        let parent = Ruleset::from_config(&json!({
            "bash": "deny",
            "edit": "allow",
            "external_directory": { "/etc/*": "deny" }
        }));
        let subagent = general();
        let derived = derive_subagent_permission(&parent, &subagent);
        // parent deny inherited
        assert!(contains(&derived, "bash", Action::Deny));
        // parent allow dropped
        assert!(!derived.rules().iter().any(|r| r.permission == "edit"));
        // external_directory inherited regardless of action
        assert!(derived
            .rules()
            .iter()
            .any(|r| r.permission == "external_directory" && r.pattern == "/etc/*"));
    }
}
