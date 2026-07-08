//! The SDD/TDD status ledger: a typed write/read log over the sqlite
//! `workflow_task` table. The engine records every per-task status transition
//! here; `otto workflow` renders it.

use otto_storage::{Store, WorkflowTaskRow};

use crate::error::{TaskStatus, WfError};

/// One task's latest recorded state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub task_index: u32,
    pub status: TaskStatus,
    pub notes: String,
}

/// A status log scoped to one workflow run (session + kind).
pub struct Ledger {
    store: Store,
    session_id: String,
    kind: String,
}

impl Ledger {
    #[must_use]
    pub fn new(store: Store, session_id: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            store,
            session_id: session_id.into(),
            kind: kind.into(),
        }
    }

    /// Record (upsert) task `task_index`'s current status + notes.
    ///
    /// # Errors
    /// Returns [`WfError`] on a storage failure.
    pub async fn record(
        &self,
        task_index: u32,
        status: TaskStatus,
        notes: &str,
    ) -> Result<(), WfError> {
        let row = WorkflowTaskRow {
            // Deterministic id: one row per (session, kind, index).
            id: format!("{}:{}:{}", self.session_id, self.kind, task_index),
            session_id: self.session_id.clone(),
            workflow_kind: self.kind.clone(),
            task_index: i64::from(task_index),
            status: status.as_wire().to_string(),
            notes: if notes.is_empty() {
                None
            } else {
                Some(notes.to_string())
            },
            updated_at: now_millis(),
        };
        self.store
            .upsert_workflow_task(&row)
            .await
            .map_err(|e| WfError::Run(format!("ledger: {e}")))
    }

    /// All recorded tasks for this session, ordered by index.
    ///
    /// # Errors
    /// Returns [`WfError`] on a storage failure or an unknown status string.
    pub async fn tasks(&self) -> Result<Vec<TaskRecord>, WfError> {
        let rows = self
            .store
            .list_workflow_tasks(&self.session_id)
            .await
            .map_err(|e| WfError::Run(format!("ledger: {e}")))?;
        rows.into_iter()
            .filter(|r| r.workflow_kind == self.kind)
            .map(|r| {
                let status = TaskStatus::from_wire(&r.status)
                    .ok_or_else(|| WfError::Parse(format!("unknown status {:?}", r.status)))?;
                Ok(TaskRecord {
                    task_index: u32::try_from(r.task_index).unwrap_or(0),
                    status,
                    notes: r.notes.unwrap_or_default(),
                })
            })
            .collect()
    }
}

/// Wall-clock millis since the epoch for `updated_at`. Monotonicity is not
/// required — this column is a human-facing audit timestamp only.
fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TaskStatus;

    #[tokio::test]
    async fn records_and_reads_back_in_order() {
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let led = Ledger::new(store, "ses_1", "sdd");
        led.record(1, TaskStatus::NeedsContext, "starting")
            .await
            .unwrap();
        led.record(0, TaskStatus::Done, "done").await.unwrap();
        // advance task 1
        led.record(1, TaskStatus::Done, "finished").await.unwrap();

        let tasks = led.tasks().await.unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].task_index, 0);
        assert_eq!(tasks[1].task_index, 1);
        assert_eq!(tasks[1].status, TaskStatus::Done);
        assert_eq!(tasks[1].notes, "finished");
    }
}
