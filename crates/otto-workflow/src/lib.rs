//! otto-workflow — a deterministic driver above `otto_session::run_loop`.
//!
//! Phase 2 ships the skeleton: error/step/status types, the test-runner
//! abstraction, the `WfCtx`/`Workflow` shapes, and the `judge()` judgment
//! node. Engine logic (TDD, SDD, plan execution) lands in later phases.

mod classify;
mod ctx;
mod error;
mod gate;
mod judge;
mod ledger;
mod plan;
mod regression;
mod runner;
mod sdd;
mod tdd;
mod verify;

pub use classify::{RedKind, classify_red};
pub use ctx::{
    ProgressSink, SubagentActivity, SubagentSink, WfCtx, WfProgress, Workflow, emit, tap_subagent,
};
pub use error::{GateFail, Step, TaskStatus, WfError};
pub use gate::gate_destructive;
pub use judge::judge;
pub use ledger::{Ledger, TaskRecord};
pub use plan::{PlanReport, PlanTaskResult, PlanWorkflow};
pub use regression::{RegressionOutcome, git_changed_files, regression_check};
pub use runner::{AutoRunner, TestOutcome, TestRunner, parse_failures};
pub use sdd::{PlanTask, SddReport, SddWorkflow, TaskResult, parse_plan_tasks, parse_status};
pub use tdd::{TddPhase, TddReport, TddWorkflow};
pub use verify::{CheckResult, Claim, VerificationGate, VerifyReport, command_for_claim};
