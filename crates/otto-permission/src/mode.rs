//! The per-session permission mode and its serde/cycle helpers.

use serde::{Deserialize, Serialize};

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
