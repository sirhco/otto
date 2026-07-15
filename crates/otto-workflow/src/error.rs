//! Core workflow result types: errors, the step/gate outcome, and the
//! subagent task-status contract.

/// A deterministic guard that a workflow step failed to satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateFail {
    pub reason: String,
}

/// Outcome of one workflow step: advance to the next state, stop at a failed
/// deterministic gate, or finish.
#[derive(Debug)]
pub enum Step<S> {
    Next(S),
    Gate(GateFail),
    Done,
}

/// Errors from running a workflow node.
#[derive(Debug, thiserror::Error)]
pub enum WfError {
    #[error("subagent run failed: {0}")]
    Run(String),
    #[error("failed to parse judgment JSON: {0}")]
    Parse(String),
    #[error("gate failed: {0}")]
    Gate(String),
    #[error(transparent)]
    Tool(#[from] otto_tools::ToolError),
}

/// The status an implementer/judgment subagent reports (SDD's 4-status
/// machine). Wire format is SCREAMING_SNAKE_CASE JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskStatus {
    Done,
    DoneWithConcerns,
    NeedsContext,
    Blocked,
    /// Engine-assigned only — never expected as a status a subagent reports
    /// for itself. Set when `drive()` detects the run was cancelled before a
    /// task's review/fix could start.
    Cancelled,
}

impl TaskStatus {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            TaskStatus::Done => "DONE",
            TaskStatus::DoneWithConcerns => "DONE_WITH_CONCERNS",
            TaskStatus::NeedsContext => "NEEDS_CONTEXT",
            TaskStatus::Blocked => "BLOCKED",
            TaskStatus::Cancelled => "CANCELLED",
        }
    }
    #[must_use]
    pub fn from_wire(s: &str) -> Option<TaskStatus> {
        match s {
            "DONE" => Some(TaskStatus::Done),
            "DONE_WITH_CONCERNS" => Some(TaskStatus::DoneWithConcerns),
            "NEEDS_CONTEXT" => Some(TaskStatus::NeedsContext),
            "BLOCKED" => Some(TaskStatus::Blocked),
            "CANCELLED" => Some(TaskStatus::Cancelled),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_deserializes_screaming_snake() {
        assert_eq!(
            serde_json::from_str::<TaskStatus>("\"DONE\"").unwrap(),
            TaskStatus::Done
        );
        assert_eq!(
            serde_json::from_str::<TaskStatus>("\"DONE_WITH_CONCERNS\"").unwrap(),
            TaskStatus::DoneWithConcerns
        );
        assert_eq!(
            serde_json::from_str::<TaskStatus>("\"NEEDS_CONTEXT\"").unwrap(),
            TaskStatus::NeedsContext
        );
        assert_eq!(
            serde_json::from_str::<TaskStatus>("\"BLOCKED\"").unwrap(),
            TaskStatus::Blocked
        );
    }

    #[test]
    fn task_status_rejects_unknown() {
        assert!(serde_json::from_str::<TaskStatus>("\"WAT\"").is_err());
    }

    #[test]
    fn task_status_wire_round_trips() {
        assert_eq!(
            TaskStatus::from_wire(TaskStatus::Done.as_wire()),
            Some(TaskStatus::Done)
        );
        assert_eq!(
            TaskStatus::from_wire(TaskStatus::DoneWithConcerns.as_wire()),
            Some(TaskStatus::DoneWithConcerns)
        );
        assert_eq!(
            TaskStatus::from_wire(TaskStatus::NeedsContext.as_wire()),
            Some(TaskStatus::NeedsContext)
        );
        assert_eq!(
            TaskStatus::from_wire(TaskStatus::Blocked.as_wire()),
            Some(TaskStatus::Blocked)
        );
        assert_eq!(TaskStatus::from_wire("WAT"), None);
    }

    #[test]
    fn wf_error_display_and_gatefail() {
        let e = WfError::Parse("bad".to_string());
        assert!(e.to_string().contains("bad"));
        let g = GateFail {
            reason: "no red".to_string(),
        };
        assert_eq!(g.reason, "no red");
    }

    #[test]
    fn step_variants_construct() {
        let s: Step<u8> = Step::Next(1);
        assert!(matches!(s, Step::Next(1)));
        let d: Step<u8> = Step::Done;
        assert!(matches!(d, Step::Done));
        let g: Step<u8> = Step::Gate(GateFail { reason: "x".into() });
        assert!(matches!(g, Step::Gate(_)));
    }

    #[test]
    fn task_status_cancelled_round_trips() {
        assert_eq!(
            serde_json::from_str::<TaskStatus>("\"CANCELLED\"").unwrap(),
            TaskStatus::Cancelled
        );
        assert_eq!(
            TaskStatus::from_wire(TaskStatus::Cancelled.as_wire()),
            Some(TaskStatus::Cancelled)
        );
        assert_eq!(TaskStatus::Cancelled.as_wire(), "CANCELLED");
    }
}
