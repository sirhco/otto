//! `otto workflow` — native dev-loop workflow driver.
//!
//! `tdd` runs the native [`TddWorkflow`] engine end to end (Phase 3), `sdd`
//! runs the native [`SddWorkflow`] engine + renders its ledger (Phase 4), and
//! `plan` runs the native [`PlanWorkflow`] engine (plan execution + verification
//! gate) + renders its ledger (Phase 5).

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use otto_app::Runtime;
use otto_workflow::{
    AutoRunner, Claim, Ledger, PlanTask, PlanWorkflow, SddWorkflow, TddWorkflow, WfCtx, Workflow,
    command_for_claim, parse_plan_tasks,
};

use crate::cli::WorkflowCommand;

pub async fn cmd_workflow(cwd: &Path, command: WorkflowCommand) -> Result<()> {
    match command {
        WorkflowCommand::Tdd { feature, dry_run } => {
            if dry_run {
                print!("{}", render_tdd_dry_run(&feature));
                return Ok(());
            }
            run_tdd(cwd, feature).await
        }
        WorkflowCommand::Sdd { plan, dry_run } => {
            if dry_run {
                return dry_run_plan_file("sdd", cwd, &plan);
            }
            run_sdd(cwd, plan).await
        }
        WorkflowCommand::Plan { plan, dry_run } => {
            if dry_run {
                return dry_run_plan_file("plan", cwd, &plan);
            }
            run_plan(cwd, plan).await
        }
    }
}

/// Preview a plan-file-driven workflow (`sdd`/`plan`) without touching the LLM
/// or the working tree: parse the plan, print the task list, and (for `plan`)
/// the verification commands that would run after each task.
fn dry_run_plan_file(kind: &str, cwd: &Path, plan_path: &str) -> Result<()> {
    let md = std::fs::read_to_string(plan_path)
        .with_context(|| format!("failed to read plan {plan_path}"))?;
    let tasks = parse_plan_tasks(&md);
    if tasks.is_empty() {
        anyhow::bail!("no `### Task N` sections found in {plan_path}");
    }
    // `plan` verifies build + tests after each task; `sdd` runs no gate.
    let verify: Vec<Vec<String>> = if kind == "plan" {
        [Claim::Builds, Claim::TestsPass]
            .iter()
            .filter_map(|c| command_for_claim(*c, cwd))
            .collect()
    } else {
        Vec::new()
    };
    print!("{}", render_dry_run(kind, plan_path, &tasks, &verify));
    Ok(())
}

/// The `tdd` dry-run notice (there is no plan file to preview).
fn render_tdd_dry_run(feature: &str) -> String {
    format!(
        "DRY RUN — tdd: feature {feature:?}\n\
         would drive the TDD cycle: write a failing test → verify RED → \
         implement → verify GREEN → regression check.\n\n\
         (dry run — no subagents dispatched, working tree untouched)\n"
    )
}

/// Render the `sdd`/`plan` dry-run preview.
fn render_dry_run(kind: &str, source: &str, tasks: &[PlanTask], verify: &[Vec<String>]) -> String {
    let mut out = format!(
        "DRY RUN — {kind}: {source}\nparsed {} task(s):\n",
        tasks.len()
    );
    for t in tasks {
        let body_lines = t.body.lines().count();
        out.push_str(&format!(
            "  task {}: {}  [{body_lines} body line(s)]\n",
            t.index, t.title
        ));
        let first = t
            .body
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        if !first.is_empty() {
            // char-safe truncation to ~80 cols
            let preview: String = first.chars().take(80).collect();
            let ellipsis = if first.chars().count() > 80 {
                "…"
            } else {
                ""
            };
            out.push_str(&format!("      {preview}{ellipsis}\n"));
        }
    }
    if kind == "plan" {
        out.push_str("\nverification after each task (must pass to accept the task):\n");
        if verify.is_empty() {
            out.push_str("  (none — no cargo/known toolchain detected in this directory)\n");
        } else {
            for cmd in verify {
                out.push_str(&format!("  $ {}\n", cmd.join(" ")));
            }
        }
    }
    out.push_str("\n(dry run — no subagents dispatched, working tree untouched)\n");
    out
}

/// Drive the native TDD cycle for `feature` against a real [`Runtime`].
async fn run_tdd(cwd: &Path, feature: String) -> Result<()> {
    let runtime = Runtime::load(cwd).await.context("failed to load runtime")?;
    let agent = runtime.default_agent().clone();
    let model = runtime.default_model();
    let session_id = runtime
        .create_session(format!("workflow tdd: {feature}"), &agent, None)
        .await?;
    let spawner = runtime
        .subagent_spawner(&agent, &model)
        .map_err(|e| anyhow::anyhow!("spawner: {e}"))?;
    let worktree = Arc::new(
        otto_vcs::worktree::Worktree::discover(
            cwd,
            &otto_config::paths::global_data_dir().join("worktree"),
        )
        .await
        .context("not a git repository")?,
    );
    let runner = Arc::new(AutoRunner::new(runtime.directory().to_path_buf()));
    let cx = WfCtx {
        spawner,
        worktree,
        runner,
        store: runtime.store().clone(),
        directory: runtime.directory().to_path_buf(),
        parent_session_id: session_id,
        permission: std::sync::Arc::new(otto_permission::Ruleset::default()),
        progress: None,
        subagent: None,
    };
    let report = TddWorkflow::new(feature).run(&cx).await?;
    println!("TDD complete: {report:?}");
    Ok(())
}

/// Drive the native SDD engine over the tasks in `plan_path`.
async fn run_sdd(cwd: &Path, plan_path: String) -> Result<()> {
    let md = std::fs::read_to_string(&plan_path)
        .with_context(|| format!("failed to read plan {plan_path}"))?;
    let tasks = parse_plan_tasks(&md);
    if tasks.is_empty() {
        anyhow::bail!("no `### Task N` sections found in {plan_path}");
    }
    println!("parsed {} task(s) from {plan_path}", tasks.len());

    let runtime = Runtime::load(cwd).await.context("failed to load runtime")?;
    let agent = runtime.default_agent().clone();
    let model = runtime.default_model();
    let session_id = runtime
        .create_session("workflow sdd".to_string(), &agent, None)
        .await?;
    let spawner = runtime
        .subagent_spawner(&agent, &model)
        .map_err(|e| anyhow::anyhow!("spawner: {e}"))?;
    let worktree = Arc::new(
        otto_vcs::worktree::Worktree::discover(
            cwd,
            &otto_config::paths::global_data_dir().join("worktree"),
        )
        .await
        .context("not a git repository")?,
    );
    let runner = Arc::new(AutoRunner::new(runtime.directory().to_path_buf()));
    let cx = WfCtx {
        spawner,
        worktree,
        runner,
        store: runtime.store().clone(),
        directory: runtime.directory().to_path_buf(),
        parent_session_id: session_id.clone(),
        permission: std::sync::Arc::new(otto_permission::Ruleset::default()),
        progress: None,
        subagent: None,
    };
    let report = SddWorkflow::new(tasks).run(&cx).await?;

    // Render the ledger.
    let led = Ledger::new(runtime.store().clone(), session_id, "sdd");
    println!("\nSDD ledger:");
    for rec in led.tasks().await? {
        println!(
            "  task {}: {:?} — {}",
            rec.task_index, rec.status, rec.notes
        );
    }
    println!("\n{} task(s) processed.", report.tasks.len());
    Ok(())
}

/// Drive the native plan-execution engine over the tasks in `plan_path`.
async fn run_plan(cwd: &Path, plan_path: String) -> Result<()> {
    let md = std::fs::read_to_string(&plan_path)
        .with_context(|| format!("failed to read plan {plan_path}"))?;
    let tasks = parse_plan_tasks(&md);
    if tasks.is_empty() {
        anyhow::bail!("no `### Task N` sections found in {plan_path}");
    }
    println!("executing {} task(s) from {plan_path}", tasks.len());

    let runtime = Runtime::load(cwd).await.context("failed to load runtime")?;
    let agent = runtime.default_agent().clone();
    let model = runtime.default_model();
    let session_id = runtime
        .create_session("workflow plan".to_string(), &agent, None)
        .await?;
    let spawner = runtime
        .subagent_spawner(&agent, &model)
        .map_err(|e| anyhow::anyhow!("spawner: {e}"))?;
    let worktree = Arc::new(
        otto_vcs::worktree::Worktree::discover(
            cwd,
            &otto_config::paths::global_data_dir().join("worktree"),
        )
        .await
        .context("not a git repository")?,
    );
    let runner = Arc::new(AutoRunner::new(runtime.directory().to_path_buf()));
    let cx = WfCtx {
        spawner,
        worktree,
        runner,
        store: runtime.store().clone(),
        directory: runtime.directory().to_path_buf(),
        parent_session_id: session_id.clone(),
        permission: std::sync::Arc::new(otto_permission::Ruleset::default()),
        progress: None,
        subagent: None,
    };
    let report = PlanWorkflow::new(tasks).run(&cx).await?;

    // Render the ledger.
    let led = Ledger::new(runtime.store().clone(), session_id, "plan");
    println!("\nplan ledger:");
    for rec in led.tasks().await? {
        println!(
            "  task {}: {:?} — {}",
            rec.task_index, rec.status, rec.notes
        );
    }
    println!("\ncompleted: {}", report.completed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tasks() -> Vec<PlanTask> {
        parse_plan_tasks("### Task 1: Alpha\nfirst line\nmore\n### Task 2: Beta\nbeta body\n")
    }

    #[test]
    fn sdd_dry_run_lists_tasks_no_verify() {
        let out = render_dry_run("sdd", "p.md", &tasks(), &[]);
        assert!(out.contains("DRY RUN — sdd: p.md"));
        assert!(out.contains("parsed 2 task(s)"));
        assert!(out.contains("task 1: Alpha"));
        assert!(out.contains("first line"));
        assert!(out.contains("task 2: Beta"));
        assert!(!out.contains("verification after each task")); // sdd has no gate
        assert!(out.contains("no subagents dispatched"));
    }

    #[test]
    fn plan_dry_run_shows_verify_commands() {
        let verify = vec![
            vec!["cargo".to_string(), "build".to_string()],
            vec!["cargo".to_string(), "test".to_string()],
        ];
        let out = render_dry_run("plan", "p.md", &tasks(), &verify);
        assert!(out.contains("verification after each task"));
        assert!(out.contains("$ cargo build"));
        assert!(out.contains("$ cargo test"));
    }

    #[test]
    fn plan_dry_run_notes_empty_gate() {
        let out = render_dry_run("plan", "p.md", &tasks(), &[]);
        assert!(out.contains("no cargo/known toolchain detected"));
    }

    #[test]
    fn tdd_dry_run_mentions_feature_and_cycle() {
        let out = render_tdd_dry_run("add(a,b)");
        assert!(out.contains("tdd: feature \"add(a,b)\""));
        assert!(out.contains("verify RED"));
        assert!(out.contains("no subagents dispatched"));
    }
}
