//! Escalates a hook `Decision::Ask` verdict into a real interactive
//! permission ask via [`crate::tool::PermissionGate::ask`], instead of the
//! deny-equivalent fallback used for `Decision::Deny`. otto extension — no
//! opencode analog. See
//! `docs/superpowers/specs/2026-07-14-hook-ask-escalation-design.md`.

use otto_hooks::HookVerdict;

use crate::tool::{PermissionDenied, PermissionRequest};

/// The result of escalating a hook `Ask` verdict through
/// [`crate::tool::PermissionGate::ask`].
pub struct HookAskOutcome {
    /// `true` when the human approved (`Reply::Once`/`Reply::Always`).
    pub approved: bool,
    /// Rejection text to surface back to the model/turn: the human's typed
    /// `Reply::Reject` message when they gave one, else the hook's own
    /// `verdict.reason`. `None` only when neither was set.
    pub message: Option<String>,
}

/// Build the [`PermissionRequest`] for a hook `Ask` verdict.
///
/// `event` is one of `"pre_tool_use"`, `"user_prompt_submit"`, `"stop"`,
/// `"subagent_stop"`. `tool_id` is `Some` only for `pre_tool_use`, giving it
/// a per-tool pattern (`"pre_tool_use:{tool_id}"`) so an "Always" reply
/// scopes to that one tool rather than every hook-originated ask.
#[must_use]
pub fn build_hook_permission_request(
    event: &str,
    verdict: &HookVerdict,
    tool_id: Option<&str>,
) -> PermissionRequest {
    let pattern = match tool_id {
        Some(id) => format!("{event}:{id}"),
        None => event.to_string(),
    };
    PermissionRequest {
        permission: "hook".to_string(),
        patterns: vec![pattern.clone()],
        always: vec![pattern],
        metadata: serde_json::json!({
            "event": event,
            "tool_id": tool_id,
            "reason": verdict.reason.clone(),
        }),
    }
}

/// Interpret a [`crate::tool::PermissionGate::ask`] result for a hook `Ask`
/// escalation, falling back to the hook's own `reason` when the human didn't
/// type a correction.
#[must_use]
pub fn interpret_hook_ask_result(
    result: Result<(), PermissionDenied>,
    verdict: &HookVerdict,
) -> HookAskOutcome {
    match result {
        Ok(()) => HookAskOutcome {
            approved: true,
            message: None,
        },
        Err(denied) => HookAskOutcome {
            approved: false,
            message: denied.message.or_else(|| verdict.reason.clone()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(reason: Option<&str>) -> HookVerdict {
        HookVerdict {
            decision: otto_hooks::Decision::Ask,
            reason: reason.map(str::to_string),
            additional_context: None,
            system_message: None,
        }
    }

    #[test]
    fn pattern_for_pre_tool_use_includes_tool_id() {
        let req = build_hook_permission_request("pre_tool_use", &verdict(None), Some("bash"));
        assert_eq!(req.permission, "hook");
        assert_eq!(req.patterns, vec!["pre_tool_use:bash".to_string()]);
        assert_eq!(req.always, vec!["pre_tool_use:bash".to_string()]);
    }

    #[test]
    fn pattern_for_non_tool_event_is_bare_event_name() {
        let req = build_hook_permission_request("stop", &verdict(None), None);
        assert_eq!(req.patterns, vec!["stop".to_string()]);
        assert_eq!(req.always, vec!["stop".to_string()]);
    }

    #[test]
    fn approved_result_carries_no_message() {
        let outcome = interpret_hook_ask_result(Ok(()), &verdict(Some("needs review")));
        assert!(outcome.approved);
        assert_eq!(outcome.message, None);
    }

    #[test]
    fn rejected_result_prefers_human_message_over_hook_reason() {
        let denied = PermissionDenied {
            permission: "hook".to_string(),
            by_user: true,
            message: Some("human says no".to_string()),
        };
        let outcome = interpret_hook_ask_result(Err(denied), &verdict(Some("hook reason")));
        assert!(!outcome.approved);
        assert_eq!(outcome.message.as_deref(), Some("human says no"));
    }

    #[test]
    fn rejected_result_falls_back_to_hook_reason_with_no_human_message() {
        let denied = PermissionDenied {
            permission: "hook".to_string(),
            by_user: true,
            message: None,
        };
        let outcome = interpret_hook_ask_result(Err(denied), &verdict(Some("hook reason")));
        assert_eq!(outcome.message.as_deref(), Some("hook reason"));
    }
}
