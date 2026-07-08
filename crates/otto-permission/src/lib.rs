//! Permission ruleset + interactive ask/reply gate — a Rust port of opencode
//! `packages/opencode/src/permission/index.ts` together with the schemas in
//! `packages/core/src/v1/permission.ts` and
//! `packages/core/src/v1/config/permission.ts`.
//!
//! * [`ruleset`] — [`Rule`]/[`Ruleset`] types and the pure functions
//!   [`evaluate`], [`merge`], [`expand`], and [`Ruleset::from_config`].
//! * [`permission`] — the [`Permission`] service implementing `ask`/`reply`
//!   with per-session approvals, reject cascade, and always auto-resolve.
//! * [`gate`] — [`SessionGate`], the [`otto_tools::PermissionGate`] the
//!   session loop injects into `ToolContext`.
//! * [`disabled`] — the hard-deny tool filter (`index.ts:204`).
//!
//! `otto_tools::AllowAll` is re-exported for tests and pre-gate call sites.

#![forbid(unsafe_code)]

pub mod gate;
pub mod permission;
pub mod ruleset;
mod wildcard;

use std::collections::HashSet;

pub use gate::SessionGate;
pub use otto_tools::AllowAll;
pub use permission::{Asked, PendingInfo, Permission, Reply, RequestId, SessionId};
pub use ruleset::{Action, ResolvedRule, Rule, Ruleset, evaluate, expand, merge};

use crate::ruleset::Action as RsAction;
use crate::wildcard::wildcard_match;

/// Tools that map to the `edit` permission (`index.ts:205`).
const EDIT_TOOLS: [&str; 3] = ["edit", "write", "apply_patch"];
/// Tools that map to the `read` permission (MCP resource reads, `index.ts:206`).
const READ_TOOLS: [&str; 3] = [
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
];

/// Compute the set of tools that are hard-denied and should be removed from
/// the toolset — port of `disabled` (`index.ts:204`).
///
/// Each tool is mapped to a permission (`edit`/`write`/`apply_patch` →
/// `edit`; MCP resource reads → `read`; otherwise the tool name itself). A
/// tool is disabled when the **last** rule whose `permission` glob matches the
/// mapped permission has pattern exactly `"*"` and action `Deny` — i.e. a
/// blanket deny for that permission.
#[must_use]
pub fn disabled(tools: &[String], rulesets: &[&Ruleset]) -> HashSet<String> {
    let mut out = HashSet::new();
    for tool in tools {
        let permission: &str = if EDIT_TOOLS.contains(&tool.as_str()) {
            "edit"
        } else if READ_TOOLS.contains(&tool.as_str()) {
            "read"
        } else {
            tool
        };

        // findLast across the flattened rulesets by permission match only.
        let mut winner = None;
        for ruleset in rulesets {
            for rule in ruleset.rules() {
                if wildcard_match(permission, &rule.permission) {
                    winner = Some(rule);
                }
            }
        }

        if let Some(rule) = winner
            && rule.pattern == "*"
            && rule.action == RsAction::Deny
        {
            out.insert(tool.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny(permission: &str, pattern: &str) -> Rule {
        Rule {
            permission: permission.to_string(),
            pattern: pattern.to_string(),
            action: Action::Deny,
        }
    }

    fn tools() -> Vec<String> {
        ["edit", "write", "apply_patch", "read", "bash"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn disabled_edit_deny_removes_edit_family() {
        let rs = Ruleset(vec![deny("edit", "*")]);
        let got = disabled(&tools(), &[&rs]);
        assert!(got.contains("edit"));
        assert!(got.contains("write"));
        assert!(got.contains("apply_patch"));
        assert!(!got.contains("read"));
        assert!(!got.contains("bash"));
    }

    #[test]
    fn disabled_requires_star_pattern() {
        // A scoped deny (pattern != "*") does not disable the tool.
        let rs = Ruleset(vec![deny("edit", "*.rs")]);
        let got = disabled(&tools(), &[&rs]);
        assert!(got.is_empty());
    }

    #[test]
    fn disabled_read_mcp_maps_to_read() {
        let rs = Ruleset(vec![deny("read", "*")]);
        let toolset = vec!["read_mcp_resource".to_string(), "bash".to_string()];
        let got = disabled(&toolset, &[&rs]);
        assert!(got.contains("read_mcp_resource"));
        assert!(!got.contains("bash"));
    }
}
