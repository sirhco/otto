//! Sequential plan-execution driver (the native `executing-plans`): run each
//! task in order with a SINGLE implementer spawn (one at a time, so a task may
//! safely commit its own work — no shared-tree race, unlike the parallel SDD
//! engine), running the `VerificationGate` as a checkpoint after each task. A
//! failed gate halts the driver (executing-plans stops on a blocker). Every
//! status is recorded to the sqlite `Ledger` under kind `"plan"`.

use std::path::Path;
use std::sync::Arc;

use otto_storage::model::MessageId;
use otto_tools::{SubagentOrigin, SubagentRequest, SubagentSpawner};
use tokio_util::sync::CancellationToken;

use crate::error::{TaskStatus, WfError};
use crate::ledger::Ledger;
use crate::sdd::{PlanTask, parse_status};
use crate::verify::{Claim, VerificationGate};

/// One task's outcome under the plan driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTaskResult {
    pub index: u32,
    pub status: TaskStatus,
    pub verified: bool,
}

/// The result of running a plan to completion (or until a halt).
#[derive(Debug)]
pub struct PlanReport {
    pub tasks: Vec<PlanTaskResult>,
    pub completed: bool,
}

/// Sequential plan-execution driver.
pub struct PlanWorkflow {
    pub tasks: Vec<PlanTask>,
    pub claims: Vec<Claim>,
}

impl PlanWorkflow {
    /// New driver with the default verification claims (build + tests).
    #[must_use]
    pub fn new(tasks: Vec<PlanTask>) -> Self {
        Self {
            tasks,
            claims: vec![Claim::Builds, Claim::TestsPass],
        }
    }

    /// Execute every task in order; verify after each; halt on the first
    /// failed verification.
    ///
    /// # Errors
    /// Returns `WfError::Gate` when a task's post-execution verification fails,
    /// or `WfError` on a storage/ledger failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn drive(
        &self,
        spawner: &Arc<dyn SubagentSpawner>,
        store: otto_storage::Store,
        dir: &Path,
        parent: &str,
        abort: CancellationToken,
        progress: Option<crate::ProgressSink>,
        subagent: Option<crate::SubagentSink>,
    ) -> Result<PlanReport, WfError> {
        let ledger = Ledger::new(store, parent, "plan");
        let gate = VerificationGate::for_claims(&self.claims, dir);
        let mut out = Vec::with_capacity(self.tasks.len());

        for t in &self.tasks {
            if abort.is_cancelled() {
                crate::emit(
                    &progress,
                    Some(t.index),
                    "CANCELLED",
                    "cancelled before start",
                );
                return Ok(PlanReport {
                    tasks: out,
                    completed: false,
                });
            }
            // Execute the task (single spawn — sequential, so commits are safe).
            let text = self
                .spawn_task(spawner, t, parent, &abort, &subagent)
                .await?;
            let status = parse_status(&text);
            ledger.record(t.index, status, "executed").await?;
            crate::emit(&progress, Some(t.index), status.as_wire(), "executed");

            // Stop-on-blocker (executing-plans semantics): an implementer that
            // reports BLOCKED/NEEDS_CONTEXT explicitly could not finish, so the
            // driver must halt BEFORE dispatching a dependent next task.
            if matches!(status, TaskStatus::Blocked | TaskStatus::NeedsContext) {
                return Err(WfError::Gate(format!(
                    "task {} reported {} — halting (executing-plans stops on a blocker)",
                    t.index,
                    status.as_wire()
                )));
            }

            // Verify-before-completion: the mapped commands must pass.
            crate::emit(&progress, Some(t.index), "VERIFYING", "");
            let report = gate.verify(dir, 600_000, abort.clone()).await;
            let verified = report.all_passed();
            out.push(PlanTaskResult {
                index: t.index,
                status,
                verified,
            });

            if verified {
                crate::emit(&progress, Some(t.index), "VERIFIED", "");
            } else {
                // Ignore this halt-path ledger write's result so a sqlite error
                // here can't mask the intended Gate error below.
                let _ = ledger
                    .record(t.index, TaskStatus::Blocked, "verification failed")
                    .await;
                let claims: Vec<String> = report
                    .failures()
                    .iter()
                    .map(|f| format!("{:?}", f.claim))
                    .collect();
                crate::emit(&progress, Some(t.index), "FAILED", &claims.join(", "));
                return Err(WfError::Gate(format!(
                    "task {} failed verification: {}",
                    t.index,
                    claims.join(", ")
                )));
            }
        }

        Ok(PlanReport {
            tasks: out,
            completed: true,
        })
    }

    async fn spawn_task(
        &self,
        spawner: &Arc<dyn SubagentSpawner>,
        t: &PlanTask,
        parent: &str,
        abort: &CancellationToken,
        subagent: &Option<crate::SubagentSink>,
    ) -> Result<String, WfError> {
        let req = SubagentRequest {
            subagent_type: "general".to_string(),
            description: format!("plan task {}", t.index),
            prompt: format!(
                "Implement this task. Write the code, add tests, run them, and commit your work \
                 (you are the only agent working in this tree).\n\n\
                 ## Task {}: {}\n{}\n\n\
                 End your reply with one JSON line: {{\"status\": \"DONE\"}} \
                 (or DONE_WITH_CONCERNS / NEEDS_CONTEXT / BLOCKED).",
                t.index, t.title, t.body
            ),
            parent_session_id: parent.into(),
            parent_message_id: MessageId::default(),
            task_id: None,
            command: None,
            abort: abort.clone(),
            event_tx: crate::tap_subagent(t.index, subagent),
            directory: None,
            origin: SubagentOrigin::Workflow {
                kind: "plan".to_string(),
            },
        };
        spawner.spawn(req).await.map_err(WfError::from)
    }
}

#[async_trait::async_trait]
impl crate::Workflow for PlanWorkflow {
    type Output = PlanReport;
    async fn run(&self, cx: &crate::WfCtx) -> Result<Self::Output, WfError> {
        self.drive(
            &cx.spawner,
            cx.store.clone(),
            &cx.directory,
            &cx.parent_session_id,
            cx.abort.clone(),
            cx.progress.clone(),
            cx.subagent.clone(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TaskStatus, parse_plan_tasks};
    use otto_tools::{SubagentRequest, SubagentSpawner, ToolError};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    /// Records dispatch order; every implementer reports DONE.
    struct SeqSpawner {
        order: Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl SubagentSpawner for SeqSpawner {
        async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
            self.order.lock().unwrap().push(req.description.clone());
            Ok("did it\n{\"status\": \"DONE\"}".to_string())
        }
    }

    #[tokio::test]
    async fn executes_tasks_in_order_and_records_ledger() {
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(SeqSpawner {
            order: Mutex::new(vec![]),
        });
        // NON-cargo temp dir → gate is empty → vacuously verified, no real cargo.
        let dir = tempfile::tempdir().unwrap();
        let tasks = parse_plan_tasks("### Task 1: A\na\n### Task 2: B\nb\n");
        let wf = PlanWorkflow::new(tasks);
        let report = wf
            .drive(
                &spawner,
                store.clone(),
                dir.path(),
                "ses_p",
                CancellationToken::new(),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(report.completed);
        assert_eq!(report.tasks.len(), 2);
        assert_eq!(report.tasks[0].index, 1);
        assert_eq!(report.tasks[1].index, 2);
        assert!(report.tasks.iter().all(|t| t.status == TaskStatus::Done));
        // Ledger has both under kind "plan".
        let led = crate::Ledger::new(store, "ses_p", "plan");
        assert_eq!(led.tasks().await.unwrap().len(), 2);
    }

    /// A real broken-cargo dir: the gate fails after task 1, so the driver
    /// stops with a Gate error and task 2 is never dispatched.
    #[tokio::test]
    async fn failed_verification_stops_the_driver() {
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let order = Arc::new(Mutex::new(vec![]));
        struct CountSpawner {
            order: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl SubagentSpawner for CountSpawner {
            async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
                self.order.lock().unwrap().push(req.description.clone());
                Ok("{\"status\": \"DONE\"}".to_string())
            }
        }
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(CountSpawner {
            order: order.clone(),
        });
        // Broken cargo project → Claim::Builds fails.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname=\"b\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn f() -> i32 { \n").unwrap(); // broken
        let tasks = parse_plan_tasks("### Task 1: A\na\n### Task 2: B\nb\n");
        let wf = PlanWorkflow::new(tasks);
        let r = wf
            .drive(
                &spawner,
                store,
                dir.path(),
                "ses_p",
                CancellationToken::new(),
                None,
                None,
            )
            .await;
        assert!(
            matches!(r, Err(WfError::Gate(_))),
            "broken build must stop the driver: {r:?}"
        );
        // Only task 1 was dispatched before the gate halted.
        assert_eq!(order.lock().unwrap().len(), 1);
    }

    /// An implementer that reports BLOCKED must halt the driver (stop-on-blocker)
    /// before the next task is dispatched, even when the gate would pass.
    #[tokio::test]
    async fn blocked_status_halts_the_driver() {
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let order = Arc::new(Mutex::new(vec![]));
        struct BlockSpawner {
            order: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl SubagentSpawner for BlockSpawner {
            async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
                self.order.lock().unwrap().push(req.description.clone());
                Ok("{\"status\": \"BLOCKED\"}".to_string())
            }
        }
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(BlockSpawner {
            order: order.clone(),
        });
        // Non-cargo temp dir → gate empty (would otherwise pass); BLOCKED halts.
        let dir = tempfile::tempdir().unwrap();
        let tasks = parse_plan_tasks("### Task 1: A\na\n### Task 2: B\nb\n");
        let wf = PlanWorkflow::new(tasks);
        let r = wf
            .drive(
                &spawner,
                store,
                dir.path(),
                "ses_p",
                CancellationToken::new(),
                None,
                None,
            )
            .await;
        assert!(
            matches!(r, Err(WfError::Gate(_))),
            "BLOCKED must halt: {r:?}"
        );
        assert_eq!(order.lock().unwrap().len(), 1, "task 2 must not dispatch");
    }

    /// With a sink present, `drive` emits an EXECUTED-ish event and a VERIFYING
    /// event for task 1 (non-cargo temp dir → empty gate → vacuously verified).
    #[tokio::test]
    async fn drive_emits_progress() {
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(SeqSpawner {
            order: Mutex::new(vec![]),
        });
        let dir = tempfile::tempdir().unwrap();
        let tasks = parse_plan_tasks("### Task 1: A\na\n### Task 2: B\nb\n");
        let wf = PlanWorkflow::new(tasks);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        wf.drive(
            &spawner,
            store,
            dir.path(),
            "ses_p",
            CancellationToken::new(),
            Some(tx),
            None,
        )
        .await
        .unwrap();
        let mut got = Vec::new();
        while let Ok(e) = rx.try_recv() {
            got.push(e);
        }
        assert!(
            got.iter()
                .any(|e| e.task_index == Some(1) && e.detail == "executed"),
            "expected an executed event for task 1: {got:?}"
        );
        assert!(
            got.iter()
                .any(|e| e.task_index == Some(1) && e.status == "VERIFYING"),
            "expected a VERIFYING event for task 1: {got:?}"
        );
    }

    #[tokio::test]
    async fn cancellation_stops_before_the_next_task_and_reports_incomplete() {
        use otto_tools::{SubagentRequest, SubagentSpawner, ToolError};
        struct AlwaysDoneSpawner;
        #[async_trait::async_trait]
        impl SubagentSpawner for AlwaysDoneSpawner {
            async fn spawn(&self, _req: SubagentRequest) -> Result<String, ToolError> {
                Ok("done\n{\"status\": \"DONE\"}".to_string())
            }
        }
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(AlwaysDoneSpawner);
        let dir = tempfile::tempdir().unwrap();
        let tasks = parse_plan_tasks("### Task 1: A\na\n### Task 2: B\nb\n");
        let abort = CancellationToken::new();
        abort.cancel();
        let wf = PlanWorkflow::new(tasks);
        let report = wf
            .drive(
                &spawner,
                store,
                dir.path(),
                "ses_plancancel",
                abort,
                None,
                None,
            )
            .await
            .expect("cancellation must not be a hard Err");
        assert!(!report.completed);
        assert!(
            report.tasks.is_empty(),
            "no task should have been dispatched once already cancelled"
        );
    }
}
