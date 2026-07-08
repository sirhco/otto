//! Deterministic permission gate for destructive workflow steps (worktree
//! reset/remove). In a headless engine only `Allow` proceeds; `Ask` and `Deny`
//! both stop the step with a `WfError::Gate` — there is no interactive prompt
//! to satisfy an `Ask`.

use otto_permission::{Action, Ruleset, evaluate};

use crate::error::WfError;

/// Gate a destructive step: proceed only if the rulesets resolve to
/// `Action::Allow` for `(permission, pattern)`.
///
/// # Errors
/// Returns [`WfError::Gate`] when the resolved action is `Ask` or `Deny`.
pub fn gate_destructive(
    rulesets: &[&Ruleset],
    permission: &str,
    pattern: &str,
) -> Result<(), WfError> {
    let resolved = evaluate(rulesets, permission, pattern);
    match resolved.action {
        Action::Allow => Ok(()),
        Action::Ask => Err(WfError::Gate(format!(
            "destructive step '{permission} {pattern}' requires confirmation (ask) — not run in headless workflow"
        ))),
        Action::Deny => Err(WfError::Gate(format!(
            "destructive step '{permission} {pattern}' denied by permission ruleset"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_permission::{Action, Rule, Ruleset};

    fn rs(action: Action) -> Ruleset {
        Ruleset(vec![Rule {
            permission: "bash".to_string(),
            pattern: "*".to_string(),
            action,
        }])
    }

    #[test]
    fn allow_passes() {
        let r = rs(Action::Allow);
        assert!(gate_destructive(&[&r], "bash", "git worktree remove").is_ok());
    }

    #[test]
    fn deny_gates() {
        let r = rs(Action::Deny);
        let out = gate_destructive(&[&r], "bash", "git worktree remove");
        assert!(matches!(out, Err(WfError::Gate(_))));
    }

    #[test]
    fn ask_gates_in_headless_engine() {
        let r = rs(Action::Ask);
        assert!(matches!(
            gate_destructive(&[&r], "bash", "x"),
            Err(WfError::Gate(_))
        ));
    }
}
