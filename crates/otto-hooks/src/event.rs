//! The lifecycle event payloads sent to a hook command on stdin, and the
//! verdict shape read back from stdout.

use otto_id::SessionId;
use serde::Deserialize;
use serde_json::{Value, json};

/// One of otto's 8 lifecycle hook points.
#[derive(Debug, Clone)]
pub enum HookEvent {
    SessionStart {
        session_id: SessionId,
        source: SessionStartSource,
    },
    UserPromptSubmit {
        session_id: SessionId,
        prompt: String,
        cwd: std::path::PathBuf,
    },
    PreToolUse {
        session_id: SessionId,
        tool_id: String,
        args: Value,
        cwd: std::path::PathBuf,
    },
    PostToolUse {
        session_id: SessionId,
        tool_id: String,
        args: Value,
        success: bool,
        cwd: std::path::PathBuf,
    },
    PreCompact {
        session_id: SessionId,
        trigger: CompactTrigger,
    },
    Stop {
        session_id: SessionId,
    },
    SubagentStop {
        session_id: SessionId,
        parent_session_id: SessionId,
    },
    Notification {
        session_id: SessionId,
        message: String,
    },
}

/// Discriminator matching `HooksConfig`'s 8 fields — lets `HookRunner` look
/// up the right `Vec<HookMatcherGroup>` for an event without a match on
/// `HookEvent` itself (which also carries payload data).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PreCompact,
    Stop,
    SubagentStop,
    Notification,
}

impl HookKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HookKind::SessionStart => "SessionStart",
            HookKind::UserPromptSubmit => "UserPromptSubmit",
            HookKind::PreToolUse => "PreToolUse",
            HookKind::PostToolUse => "PostToolUse",
            HookKind::PreCompact => "PreCompact",
            HookKind::Stop => "Stop",
            HookKind::SubagentStop => "SubagentStop",
            HookKind::Notification => "Notification",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStartSource {
    New,
    Resumed,
}

impl SessionStartSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStartSource::New => "new",
            SessionStartSource::Resumed => "resumed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactTrigger {
    Manual,
    Auto,
}

impl CompactTrigger {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CompactTrigger::Manual => "manual",
            CompactTrigger::Auto => "auto",
        }
    }
}

impl HookEvent {
    #[must_use]
    pub fn kind(&self) -> HookKind {
        match self {
            HookEvent::SessionStart { .. } => HookKind::SessionStart,
            HookEvent::UserPromptSubmit { .. } => HookKind::UserPromptSubmit,
            HookEvent::PreToolUse { .. } => HookKind::PreToolUse,
            HookEvent::PostToolUse { .. } => HookKind::PostToolUse,
            HookEvent::PreCompact { .. } => HookKind::PreCompact,
            HookEvent::Stop { .. } => HookKind::Stop,
            HookEvent::SubagentStop { .. } => HookKind::SubagentStop,
            HookEvent::Notification { .. } => HookKind::Notification,
        }
    }

    /// The tool id, for the two tool-scoped events — `None` for every other
    /// event (and therefore never matcher-filtered).
    #[must_use]
    pub fn tool_id(&self) -> Option<&str> {
        match self {
            HookEvent::PreToolUse { tool_id, .. } | HookEvent::PostToolUse { tool_id, .. } => {
                Some(tool_id.as_str())
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn to_stdin_json(&self) -> Value {
        match self {
            HookEvent::SessionStart { session_id, source } => json!({
                "event": "SessionStart",
                "session_id": session_id.as_str(),
                "source": source.as_str(),
            }),
            HookEvent::UserPromptSubmit {
                session_id,
                prompt,
                cwd,
            } => json!({
                "event": "UserPromptSubmit",
                "session_id": session_id.as_str(),
                "prompt": prompt,
                "cwd": cwd.display().to_string(),
            }),
            HookEvent::PreToolUse {
                session_id,
                tool_id,
                args,
                cwd,
            } => json!({
                "event": "PreToolUse",
                "session_id": session_id.as_str(),
                "tool_id": tool_id,
                "args": args,
                "cwd": cwd.display().to_string(),
            }),
            HookEvent::PostToolUse {
                session_id,
                tool_id,
                args,
                success,
                cwd,
            } => json!({
                "event": "PostToolUse",
                "session_id": session_id.as_str(),
                "tool_id": tool_id,
                "args": args,
                "success": success,
                "cwd": cwd.display().to_string(),
            }),
            HookEvent::PreCompact {
                session_id,
                trigger,
            } => json!({
                "event": "PreCompact",
                "session_id": session_id.as_str(),
                "trigger": trigger.as_str(),
            }),
            HookEvent::Stop { session_id } => json!({
                "event": "Stop",
                "session_id": session_id.as_str(),
            }),
            HookEvent::SubagentStop {
                session_id,
                parent_session_id,
            } => json!({
                "event": "SubagentStop",
                "session_id": session_id.as_str(),
                "parent_session_id": parent_session_id.as_str(),
            }),
            HookEvent::Notification {
                session_id,
                message,
            } => json!({
                "event": "Notification",
                "session_id": session_id.as_str(),
                "message": message,
            }),
        }
    }
}

/// The verdict a hook command's stdout is parsed into. Missing/unparseable
/// `decision` defaults to `Allow` — see `HookRunner`'s fail-open rule for the
/// crash/timeout case (a *parseable* `HookVerdict` with an explicit
/// non-allow `decision` is not a failure and is honored normally).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    #[default]
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookVerdict {
    #[serde(default)]
    pub decision: Decision,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub additional_context: Option<String>,
    #[serde(default)]
    pub system_message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pre_tool_use() -> HookEvent {
        HookEvent::PreToolUse {
            session_id: SessionId::from("ses_test"),
            tool_id: "bash".to_string(),
            args: json!({"command": "echo hi"}),
            cwd: std::path::PathBuf::from("/repo"),
        }
    }

    #[test]
    fn pre_tool_use_serializes_expected_fields() {
        let json = pre_tool_use().to_stdin_json();
        assert_eq!(json["event"], "PreToolUse");
        assert_eq!(json["session_id"], "ses_test");
        assert_eq!(json["tool_id"], "bash");
        assert_eq!(json["args"]["command"], "echo hi");
        assert_eq!(json["cwd"], "/repo");
    }

    #[test]
    fn kind_matches_variant() {
        let event = HookEvent::Stop {
            session_id: SessionId::from("ses_a"),
        };
        assert_eq!(event.kind(), HookKind::Stop);
        assert_eq!(event.tool_id(), None);
    }

    #[test]
    fn tool_id_present_only_for_tool_events() {
        assert_eq!(pre_tool_use().tool_id(), Some("bash"));
        let notification = HookEvent::Notification {
            session_id: SessionId::from("ses_a"),
            message: "waiting".to_string(),
        };
        assert_eq!(notification.tool_id(), None);
    }

    #[test]
    fn verdict_defaults_to_allow_on_missing_decision() {
        let v: HookVerdict = serde_json::from_str("{}").unwrap();
        assert_eq!(v.decision, Decision::Allow);
        assert!(v.reason.is_none());
    }

    #[test]
    fn verdict_parses_deny_with_reason() {
        let v: HookVerdict =
            serde_json::from_str(r#"{"decision":"deny","reason":"nope"}"#).unwrap();
        assert_eq!(v.decision, Decision::Deny);
        assert_eq!(v.reason.as_deref(), Some("nope"));
    }

    #[test]
    fn verdict_rejects_unknown_decision_value() {
        let result: Result<HookVerdict, _> = serde_json::from_str(r#"{"decision":"bogus"}"#);
        assert!(result.is_err());
    }
}
