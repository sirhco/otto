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

pub use classify::{classify_red, RedKind};
pub use ctx::{
    emit, tap_subagent, ProgressSink, SubagentActivity, SubagentSink, WfCtx, WfProgress, Workflow,
};
pub use error::{GateFail, Step, TaskStatus, WfError};
pub use gate::gate_destructive;
pub use judge::judge;
pub use ledger::{Ledger, TaskRecord};
pub use plan::{PlanReport, PlanTaskResult, PlanWorkflow};
pub use regression::{git_changed_files, regression_check, RegressionOutcome};
pub use runner::{parse_failures, AutoRunner, TestOutcome, TestRunner};
pub use sdd::{parse_plan_tasks, parse_status, PlanTask, SddReport, SddWorkflow, TaskResult};
pub use tdd::{TddPhase, TddReport, TddWorkflow};
pub use verify::{command_for_claim, CheckResult, Claim, VerificationGate, VerifyReport};
