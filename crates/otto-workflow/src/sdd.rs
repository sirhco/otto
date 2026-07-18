//! Native subagent-driven-development: parse a plan into tasks, dispatch the
//! implementers in parallel into isolated per-task worktrees, merge each
//! success back into the shared working tree, then run a bounded per-task
//! review→fix loop, recording every status to the ledger.

use std::path::PathBuf;
use std::sync::Arc;

use otto_storage::model::MessageId;
use otto_tools::{SubagentOrigin, SubagentRequest, SubagentSpawner};
use otto_vcs::worktree::{CreateInput, RemoveInput, Worktree};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::error::{TaskStatus, WfError};
use crate::ledger::Ledger;

/// One task extracted from a plan's `### Task N: Title` sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTask {
    pub index: u32,
    pub title: String,
    pub body: String,
}

/// Split a plan on `### Task N[: Title]` headings. Everything up to the first
/// such heading (preamble) is ignored; each task's body is the text until the
/// next task heading.
#[must_use]
pub fn parse_plan_tasks(md: &str) -> Vec<PlanTask> {
    let mut tasks: Vec<PlanTask> = Vec::new();
    let mut cur: Option<PlanTask> = None;
    for line in md.lines() {
        if let Some((index, title)) = parse_task_heading(line) {
            if let Some(t) = cur.take() {
                tasks.push(t);
            }
            cur = Some(PlanTask {
                index,
                title,
                body: String::new(),
            });
        } else if let Some(t) = cur.as_mut() {
            t.body.push_str(line);
            t.body.push('\n');
        }
    }
    if let Some(t) = cur.take() {
        tasks.push(t);
    }
    for t in &mut tasks {
        t.body = t.body.trim().to_string();
    }
    tasks
}

/// Parse a `### Task 3: Name` heading → `(3, "Name")`. Title is optional.
fn parse_task_heading(line: &str) -> Option<(u32, String)> {
    let rest = line.strip_prefix("### Task ")?;
    // rest = "3: Name" or "3"
    let (num, title) = match rest.split_once(':') {
        Some((n, t)) => (n.trim(), t.trim().to_string()),
        None => (rest.trim(), String::new()),
    };
    let index: u32 = num.parse().ok()?;
    Some((index, title))
}

/// Find the LAST `{...}` object in `text` that has a `"status"` field and
/// return its `TaskStatus`; default to `NeedsContext` when none parses.
#[must_use]
pub fn parse_status(text: &str) -> TaskStatus {
    try_parse_status(text).unwrap_or(TaskStatus::NeedsContext)
}

/// Like [`parse_status`], but `None` when the text carries no status marker at
/// all — letting callers distinguish "reported NEEDS_CONTEXT" from "the turn
/// ended without reporting anything" (e.g. cut short by a rejected permission
/// ask).
#[must_use]
pub fn try_parse_status(text: &str) -> Option<TaskStatus> {
    #[derive(Deserialize)]
    struct StatusOnly {
        status: TaskStatus,
    }
    let bytes = text.as_bytes();
    // Scan every '{'..matching-'}' candidate; keep the last that parses.
    let mut found: Option<TaskStatus> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end) = matching_brace(bytes, i)
        {
            if let Ok(v) = serde_json::from_str::<StatusOnly>(&text[i..=end]) {
                found = Some(v.status);
            }
            i = end + 1;
            continue;
        }
        i += 1;
    }
    found
}

/// Index of the `}` matching the `{` at `open`, honoring nesting AND JSON
/// string values. Braces inside a `"..."` string are ignored, so a finding
/// like `["missing } here"]` does not close the object early. `{`, `}`, `"`,
/// and `\` are all single-byte ASCII, so byte-walking stays unicode-safe.
fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    for (k, b) in bytes.iter().enumerate().skip(open) {
        if in_string {
            if *b == b'"' && !is_escaped(bytes, k) {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

/// True when the byte at `k` is preceded by an odd run of backslashes (i.e. it
/// is escaped). Used to tell a literal `\"` from a string-terminating `"`.
fn is_escaped(bytes: &[u8], k: usize) -> bool {
    let mut backslashes = 0usize;
    let mut j = k;
    while j > 0 && bytes[j - 1] == b'\\' {
        backslashes += 1;
        j -= 1;
    }
    backslashes % 2 == 1
}

/// Result of running the SDD engine over a plan.
#[derive(Debug)]
pub struct SddReport {
    pub tasks: Vec<TaskResult>,
}

/// One task's terminal state after the implement→review→fix loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResult {
    pub index: u32,
    pub status: TaskStatus,
    pub reviewed: bool,
    pub approved: bool,
}

#[derive(Deserialize)]
struct Verdict {
    approved: bool,
    #[serde(default)]
    findings: Vec<String>,
}

/// The SDD engine over a list of plan tasks.
///
/// # Working tree (isolated per task, parallel)
///
/// Phase A dispatches every task's implementer at once, each into its OWN
/// git worktree (`otto/sdd-task-<index>`, created off the shared tree's
/// current `HEAD`) — no two implementers ever see each other's uncommitted
/// changes while they work. Once an implementer reports a terminal status,
/// its worktree's changes (including new/untracked files) are folded back
/// into the shared working tree one at a time via
/// [`otto_vcs::worktree::Worktree::merge_working_tree`] — sequential,
/// because applying a patch against one shared tree can't itself be
/// parallelized. A task whose changes fail to merge (e.g. two tasks touched
/// overlapping lines — plans are still expected, unenforced, to keep tasks
/// on disjoint files) degrades to `Blocked` with the git failure recorded,
/// rather than corrupting the shared tree or failing the whole run. Every
/// worktree is removed once its task is done, win or lose. Implementers
/// still do NOT commit or stage: the engine leaves the shared tree's
/// changes unstaged for the review→fix phase. Run `otto workflow sdd` on a
/// dedicated feature branch so the accumulated working-tree changes are easy
/// to inspect and commit yourself.
///
/// Phase B (review→fix) is unaffected — it runs sequentially, per task,
/// directly against the shared working tree (worktrees only exist during
/// Phase A).
pub struct SddWorkflow {
    pub tasks: Vec<PlanTask>,
    pub max_fix_rounds: u32,
}

impl SddWorkflow {
    #[must_use]
    pub fn new(tasks: Vec<PlanTask>) -> Self {
        Self {
            tasks,
            max_fix_rounds: 2,
        }
    }

    /// Drive the full SDD loop against explicit collaborators. An
    /// unparseable review verdict or a mid-run cancellation degrades the
    /// affected task's status rather than failing the whole run — this
    /// always returns `Ok` unless a genuine infrastructure failure (ledger
    /// write, duplicate task index) occurs.
    ///
    /// `worktree` roots Phase A's per-task isolation — every implementer
    /// gets its own worktree under `worktree.data_root`, merged back into
    /// `worktree.git_root` on success.
    ///
    /// # Errors
    /// Returns [`WfError`] on a ledger write failure or a duplicate task
    /// index.
    #[allow(clippy::too_many_arguments)]
    pub async fn drive(
        &self,
        spawner: &Arc<dyn SubagentSpawner>,
        store: otto_storage::Store,
        parent: &str,
        abort: CancellationToken,
        progress: Option<crate::ProgressSink>,
        subagent: Option<crate::SubagentSink>,
        worktree: &Arc<Worktree>,
    ) -> Result<SddReport, WfError> {
        // Guard: the ledger keys rows on `session:kind:index`, so duplicate
        // task indices would silently overwrite one another. Reject up front,
        // before spawning anything.
        for (a, t) in self.tasks.iter().enumerate() {
            if self.tasks[..a].iter().any(|p| p.index == t.index) {
                return Err(WfError::Gate(format!(
                    "plan has duplicate task index {}",
                    t.index
                )));
            }
        }

        let ledger = Ledger::new(store, parent, "sdd");

        // Nothing to salvage if we were already cancelled before starting —
        // mark every task Cancelled and dispatch nothing.
        if abort.is_cancelled() {
            let mut out = Vec::with_capacity(self.tasks.len());
            for t in &self.tasks {
                ledger
                    .record(t.index, TaskStatus::Cancelled, "cancelled before start")
                    .await?;
                crate::emit(
                    &progress,
                    Some(t.index),
                    "CANCELLED",
                    "cancelled before start",
                );
                out.push(TaskResult {
                    index: t.index,
                    status: TaskStatus::Cancelled,
                    reviewed: false,
                    approved: false,
                });
            }
            return Ok(SddReport { tasks: out });
        }

        // --- Phase A: create isolated worktrees, dispatch implementers in
        // parallel, merge each success back into the shared directory ---
        //
        // Worktree creation is cheap, local git plumbing and stays
        // sequential — concurrent `git worktree add` against the same repo
        // is an unnecessary correctness risk for no real speedup. The
        // expensive, latency-bound part (the implementer LLM turns) is what
        // actually benefits from running in parallel, via `spawn_many`.
        struct Dispatched {
            task_index: usize,
            worktree_dir: String,
        }
        let mut reqs = Vec::new();
        let mut dispatched = Vec::new();
        let mut statuses: Vec<Option<TaskStatus>> = vec![None; self.tasks.len()];

        for (i, t) in self.tasks.iter().enumerate() {
            if abort.is_cancelled() {
                ledger
                    .record(t.index, TaskStatus::Cancelled, "cancelled before start")
                    .await?;
                crate::emit(
                    &progress,
                    Some(t.index),
                    "CANCELLED",
                    "cancelled before start",
                );
                statuses[i] = Some(TaskStatus::Cancelled);
                continue;
            }
            match worktree
                .create(CreateInput {
                    name: Some(format!("sdd-task-{}", t.index)),
                })
                .await
            {
                Ok(info) => {
                    crate::emit(
                        &progress,
                        Some(t.index),
                        "RUNNING",
                        "implementer dispatched",
                    );
                    reqs.push(self.implementer_req(
                        t,
                        parent,
                        &abort,
                        &subagent,
                        PathBuf::from(&info.directory),
                    ));
                    dispatched.push(Dispatched {
                        task_index: i,
                        worktree_dir: info.directory,
                    });
                }
                Err(e) => {
                    let note = format!("failed to create an isolated worktree: {e}");
                    ledger.record(t.index, TaskStatus::Blocked, &note).await?;
                    crate::emit(&progress, Some(t.index), "BLOCKED", &note);
                    statuses[i] = Some(TaskStatus::Blocked);
                }
            }
        }

        let results = if dispatched.is_empty() {
            Vec::new()
        } else {
            spawner.spawn_many(reqs).await
        };

        for (
            Dispatched {
                task_index,
                worktree_dir,
            },
            res,
        ) in dispatched.into_iter().zip(results)
        {
            let t = &self.tasks[task_index];
            let (status, note) = match res {
                Ok(text) => match try_parse_status(&text) {
                    Some(s) => (s, "implemented".to_string()),
                    None => (
                        TaskStatus::NeedsContext,
                        "no status marker in output (possibly a rejected permission ask ended the turn early)".to_string(),
                    ),
                },
                Err(_) => (TaskStatus::Blocked, "implementer failed to spawn/run".to_string()),
            };

            let (status, note) = if matches!(
                status,
                TaskStatus::Done | TaskStatus::DoneWithConcerns
            ) {
                match worktree.merge_working_tree(&worktree_dir).await {
                    Ok(_) => (status, note),
                    Err(e) => (
                        TaskStatus::Blocked,
                        format!(
                            "implementer succeeded but its changes failed to merge into the shared working tree: {e}"
                        ),
                    ),
                }
            } else {
                (status, note)
            };

            if let Err(e) = worktree
                .remove(RemoveInput {
                    directory: worktree_dir.clone(),
                })
                .await
            {
                tracing::warn!(
                    task = t.index,
                    worktree = %worktree_dir,
                    error = %e,
                    "sdd: failed to remove isolated worktree (leaked, harmless)"
                );
            }

            ledger.record(t.index, status, &note).await?;
            let phase = if matches!(status, TaskStatus::Done | TaskStatus::DoneWithConcerns) {
                "IMPLEMENTED"
            } else {
                status.as_wire()
            };
            crate::emit(&progress, Some(t.index), phase, &note);
            statuses[task_index] = Some(status);
        }

        let statuses: Vec<TaskStatus> = statuses
            .into_iter()
            .map(|s| s.expect("every task index is assigned a status exactly once above"))
            .collect();

        // --- Phase B: per-task review→fix (only for completed tasks) ---
        let mut out = Vec::with_capacity(self.tasks.len());
        for (t, status) in self.tasks.iter().zip(statuses) {
            let mut result = TaskResult {
                index: t.index,
                status,
                reviewed: false,
                approved: false,
            };
            if !matches!(status, TaskStatus::Done | TaskStatus::DoneWithConcerns) {
                // NEEDS_CONTEXT / BLOCKED: leave for the human; do not review.
                out.push(result);
                continue;
            }
            if abort.is_cancelled() {
                result.status = TaskStatus::Cancelled;
                ledger
                    .record(t.index, TaskStatus::Cancelled, "cancelled before review")
                    .await?;
                crate::emit(
                    &progress,
                    Some(t.index),
                    "CANCELLED",
                    "cancelled before review",
                );
                out.push(result);
                continue;
            }
            result.reviewed = true;
            let mut round = 0u32;
            loop {
                crate::emit(&progress, Some(t.index), "REVIEWING", "");
                let verdict = match self.review(spawner, t, parent, &abort, &subagent).await {
                    Ok(v) => v,
                    Err(e) => {
                        // An unparseable verdict (bad JSON, or a turn cut
                        // short by cancellation) must not abort the whole
                        // run — degrade this ONE task and let the others
                        // finish. Mirrors the implementer path's existing
                        // "unclear output -> NeedsContext" philosophy; a
                        // genuine spawn/infra failure inside review() gets
                        // Blocked instead, matching the implementer path's
                        // Ok/Err distinction.
                        let degraded = if matches!(e, WfError::Parse(_)) {
                            TaskStatus::NeedsContext
                        } else {
                            TaskStatus::Blocked
                        };
                        let note = format!("review verdict unusable: {e}");
                        result.status = degraded;
                        ledger.record(t.index, degraded, &note).await?;
                        crate::emit(&progress, Some(t.index), degraded.as_wire(), &note);
                        break;
                    }
                };
                if verdict.approved {
                    result.approved = true;
                    result.status = TaskStatus::Done;
                    ledger
                        .record(t.index, TaskStatus::Done, "review clean")
                        .await?;
                    crate::emit(&progress, Some(t.index), "DONE", "review clean");
                    break;
                }
                round += 1;
                if round > self.max_fix_rounds {
                    ledger
                        .record(
                            t.index,
                            TaskStatus::DoneWithConcerns,
                            "unresolved review findings",
                        )
                        .await?;
                    crate::emit(
                        &progress,
                        Some(t.index),
                        "DONE_WITH_CONCERNS",
                        "unresolved review findings",
                    );
                    break;
                }
                crate::emit(
                    &progress,
                    Some(t.index),
                    "FIXING",
                    &format!("round {round}"),
                );
                if let Err(e) = self
                    .fix(spawner, t, &verdict.findings, parent, &abort, &subagent)
                    .await
                {
                    let note = format!("fix failed: {e}");
                    result.status = TaskStatus::Blocked;
                    ledger.record(t.index, TaskStatus::Blocked, &note).await?;
                    crate::emit(&progress, Some(t.index), "BLOCKED", &note);
                    break;
                }
            }
            out.push(result);
        }

        Ok(SddReport { tasks: out })
    }

    fn implementer_req(
        &self,
        t: &PlanTask,
        parent: &str,
        abort: &CancellationToken,
        subagent: &Option<crate::SubagentSink>,
        directory: PathBuf,
    ) -> SubagentRequest {
        SubagentRequest {
            subagent_type: "general".to_string(),
            description: format!("sdd task {}", t.index),
            prompt: format!(
                "Implement this task. Write the code, add the tests, and run the \
                 test suite to confirm they pass. DO NOT run any git commands \
                 (no add / stage / commit) — the workflow manages version \
                 control. You have your own isolated working tree for this \
                 task; nothing you do here is visible to other implementers \
                 until the workflow merges your changes back. Leave your \
                 changes in the working tree.\n\n\
                 ## Task {}: {}\n{}\n\n\
                 End your reply with one JSON line: {{\"status\": \"DONE\"}} \
                 (or DONE_WITH_CONCERNS / NEEDS_CONTEXT / BLOCKED).",
                t.index, t.title, t.body
            ),
            parent_session_id: parent.into(),
            parent_message_id: MessageId::default(),
            task_id: None,
            event_tx: crate::tap_subagent(t.index, subagent),
            command: None,
            abort: abort.clone(),
            directory: Some(directory),
            origin: SubagentOrigin::Workflow {
                kind: "sdd".to_string(),
            },
        }
    }

    async fn review(
        &self,
        spawner: &Arc<dyn SubagentSpawner>,
        t: &PlanTask,
        parent: &str,
        abort: &CancellationToken,
        subagent: &Option<crate::SubagentSink>,
    ) -> Result<Verdict, WfError> {
        let prompt = format!(
            "Review the task implementation for spec compliance and code quality.\n\n\
             ## Task {}: {}\n{}\n\n\
             Return ONLY JSON: {{\"approved\": bool, \"findings\": [string]}}.",
            t.index, t.title, t.body
        );
        let text = spawn_one(
            spawner, "general", parent, &prompt, abort, t.index, subagent,
        )
        .await?;
        parse_verdict(&text).ok_or_else(|| WfError::Parse("review verdict".to_string()))
    }

    async fn fix(
        &self,
        spawner: &Arc<dyn SubagentSpawner>,
        t: &PlanTask,
        findings: &[String],
        parent: &str,
        abort: &CancellationToken,
        subagent: &Option<crate::SubagentSink>,
    ) -> Result<(), WfError> {
        let prompt = format!(
            "Fix these review findings for task {}: {}\n- {}\n\n\
             Re-run the covering tests. DO NOT run any git commands — the \
             workflow manages version control.",
            t.index,
            t.title,
            findings.join("\n- ")
        );
        spawn_one(
            spawner, "general", parent, &prompt, abort, t.index, subagent,
        )
        .await?;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn spawn_one(
    spawner: &Arc<dyn SubagentSpawner>,
    agent: &str,
    parent: &str,
    prompt: &str,
    abort: &CancellationToken,
    task_index: u32,
    subagent: &Option<crate::SubagentSink>,
) -> Result<String, WfError> {
    let req = SubagentRequest {
        subagent_type: agent.to_string(),
        description: "sdd node".to_string(),
        prompt: prompt.to_string(),
        parent_session_id: parent.into(),
        parent_message_id: MessageId::default(),
        task_id: None,
        command: None,
        abort: abort.clone(),
        event_tx: crate::tap_subagent(task_index, subagent),
        directory: None,
        origin: SubagentOrigin::Workflow {
            kind: "sdd".to_string(),
        },
    };
    spawner.spawn(req).await.map_err(WfError::from)
}

/// Parse the LAST `{...}` with an `approved` field as a `Verdict`.
fn parse_verdict(text: &str) -> Option<Verdict> {
    let bytes = text.as_bytes();
    let mut found = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end) = matching_brace(bytes, i)
        {
            if let Ok(v) = serde_json::from_str::<Verdict>(&text[i..=end]) {
                found = Some(v);
            }
            i = end + 1;
            continue;
        }
        i += 1;
    }
    found
}

#[async_trait::async_trait]
impl crate::Workflow for SddWorkflow {
    type Output = SddReport;
    async fn run(&self, cx: &crate::WfCtx) -> Result<Self::Output, WfError> {
        self.drive(
            &cx.spawner,
            cx.store.clone(),
            &cx.parent_session_id,
            cx.abort.clone(),
            cx.progress.clone(),
            cx.subagent.clone(),
            &cx.worktree,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TaskStatus;
    use otto_tools::ToolError;
    use otto_vcs::worktree::Worktree;
    use std::sync::Mutex;

    /// Build a temp git repo with one commit and a `Worktree` rooted there.
    /// Mirrors `otto_vcs::worktree::tests::init_repo` — duplicated locally
    /// since that helper is private to `otto-vcs`'s own test module and
    /// otto-vcs exposes no cross-crate test-support surface (not worth
    /// adding for one ~10-line helper).
    async fn init_repo() -> (tempfile::TempDir, tempfile::TempDir, Arc<Worktree>) {
        let repo = tempfile::tempdir().unwrap();
        let p = repo.path();
        otto_vcs::git::run_git(p, &["init", "-q", "-b", "main"])
            .await
            .unwrap();
        otto_vcs::git::run_git(p, &["config", "user.email", "t@t.t"])
            .await
            .unwrap();
        otto_vcs::git::run_git(p, &["config", "user.name", "t"])
            .await
            .unwrap();
        otto_vcs::git::run_git(p, &["config", "commit.gpgsign", "false"])
            .await
            .unwrap();
        std::fs::write(p.join("f.txt"), "hello").unwrap();
        otto_vcs::git::run_git(p, &["add", "."]).await.unwrap();
        otto_vcs::git::run_git(p, &["commit", "-q", "-m", "init"])
            .await
            .unwrap();
        let data = tempfile::tempdir().unwrap();
        let worktree = Arc::new(Worktree::new(p.to_path_buf(), data.path().to_path_buf()));
        (repo, data, worktree)
    }

    #[test]
    fn parse_plan_splits_on_task_headings() {
        let md = "# Plan\nintro\n### Task 1: Alpha\nbody a\n### Task 2: Beta\nbody b\n";
        let tasks = parse_plan_tasks(md);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].index, 1);
        assert_eq!(tasks[0].title, "Alpha");
        assert!(tasks[0].body.contains("body a"));
        assert_eq!(tasks[1].index, 2);
        assert_eq!(tasks[1].title, "Beta");
    }

    #[test]
    fn parse_plan_no_tasks_is_empty() {
        assert!(parse_plan_tasks("# just a heading\ntext").is_empty());
    }

    #[test]
    fn parse_status_reads_trailing_json() {
        let t = "did the work.\n{\"status\": \"DONE\"}";
        assert_eq!(parse_status(t), TaskStatus::Done);
    }

    #[test]
    fn parse_status_reads_last_json_object() {
        let t = "{\"status\":\"BLOCKED\"} then more\n{\"status\":\"DONE_WITH_CONCERNS\"}";
        assert_eq!(parse_status(t), TaskStatus::DoneWithConcerns);
    }

    #[test]
    fn parse_status_defaults_to_needs_context() {
        assert_eq!(parse_status("no json here"), TaskStatus::NeedsContext);
    }

    #[test]
    fn parse_verdict_recovers_with_unbalanced_brace_in_string() {
        // A finding string contains an unbalanced `}`. A byte-only brace scan
        // would match that `}` as the object close, yield invalid JSON, and
        // return None. The string-aware scan skips braces inside strings and
        // recovers the correct verdict.
        let v = parse_verdict("prose\n{\"approved\": false, \"findings\": [\"missing } brace\"]}");
        assert!(v.is_some(), "string-aware scan must recover the verdict");
        assert!(!v.unwrap().approved);
    }

    #[test]
    fn parse_status_finds_trailing_object_past_brace_in_string() {
        // A brace inside a string in an earlier object must not desync the scan
        // so that the trailing status object is still found.
        let t = "note {\"findings\": [\"unbalanced { here\"]}\n{\"status\": \"DONE\"}";
        assert_eq!(parse_status(t), TaskStatus::Done);
    }

    #[test]
    fn parse_verdict_handles_escaped_quote_before_brace() {
        // An escaped quote (\") inside the string must NOT be read as a string
        // terminator, so the following `}` stays inside the string.
        let v = parse_verdict(r#"{"approved": false, "findings": ["say \"hi} there\""]}"#);
        assert!(v.is_some());
        assert!(!v.unwrap().approved);
    }

    /// Records every prompt; returns a DONE implementer status for all
    /// implementer prompts and an "approved" verdict for review prompts. If
    /// tapped (Task 3's event_tx), forwards one canned tool call so the
    /// tap→sink path is exercised end-to-end. If given a real `directory`
    /// (Phase A implementer dispatch), writes a file there so Phase A's
    /// merge-back has something real to merge. Also records how many
    /// spawn_many BATCH calls happened.
    struct BatchSpawner {
        prompts: Mutex<Vec<String>>,
        batches: Mutex<u32>,
    }
    #[async_trait::async_trait]
    impl SubagentSpawner for BatchSpawner {
        async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
            self.prompts.lock().unwrap().push(req.prompt.clone());
            if let Some(tx) = &req.event_tx {
                let _ = tx.send(otto_events::LLMEvent::ToolCall {
                    id: "1".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": "x"}),
                    provider_executed: None,
                    provider_metadata: None,
                });
            }
            if req.prompt.contains("Review the task") {
                Ok("looks good\n{\"approved\": true, \"findings\": []}".to_string())
            } else {
                if let Some(dir) = &req.directory {
                    let file = req.description.replace(' ', "_").replace(':', "");
                    std::fs::write(dir.join(format!("{file}.txt")), "implemented").unwrap();
                }
                Ok("implemented it\n{\"status\": \"DONE\"}".to_string())
            }
        }
        async fn spawn_many(&self, reqs: Vec<SubagentRequest>) -> Vec<Result<String, ToolError>> {
            *self.batches.lock().unwrap() += 1;
            let mut out = Vec::with_capacity(reqs.len());
            for r in reqs {
                out.push(self.spawn(r).await);
            }
            out
        }
    }

    #[tokio::test]
    async fn sdd_dispatches_implementers_in_parallel_with_isolated_worktrees() {
        if !otto_vcs::git::git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (repo, _data, worktree) = init_repo().await;
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let concrete = Arc::new(BatchSpawner {
            prompts: Mutex::new(vec![]),
            batches: Mutex::new(0),
        });
        let spawner: Arc<dyn SubagentSpawner> = concrete.clone();
        let tasks = parse_plan_tasks("### Task 1: A\nbuild a\n### Task 2: B\nbuild b\n");
        let wf = SddWorkflow::new(tasks);
        let report = wf
            .drive(
                &spawner,
                store.clone(),
                "ses_1",
                CancellationToken::new(),
                None,
                None,
                &worktree,
            )
            .await
            .unwrap();
        assert_eq!(report.tasks.len(), 2);
        assert!(report.tasks.iter().all(|t| t.status == TaskStatus::Done));
        assert!(report.tasks.iter().all(|t| t.approved));
        // Implementers must dispatch as ONE parallel batch, not one-at-a-time.
        assert_eq!(
            *concrete.batches.lock().unwrap(),
            1,
            "spawn_many must be called exactly once for the implementer phase"
        );
        // Each implementer wrote into ITS OWN isolated worktree; both files
        // are now present in the shared repo root after merge-back.
        assert!(repo.path().join("sdd_task_1.txt").exists());
        assert!(repo.path().join("sdd_task_2.txt").exists());
        // No worktrees are left behind after a successful run.
        assert!(worktree.list().await.unwrap().is_empty());
        let led = Ledger::new(store, "ses_1", "sdd");
        let recs = led.tasks().await.unwrap();
        assert_eq!(recs.len(), 2);
        assert!(recs.iter().all(|r| r.status == TaskStatus::Done));
    }

    #[tokio::test]
    async fn implementer_dispatch_order_matches_task_order() {
        if !otto_vcs::git::git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (_repo, _data, worktree) = init_repo().await;
        struct OrderRecordingSpawner {
            order: Mutex<Vec<String>>,
        }
        #[async_trait::async_trait]
        impl SubagentSpawner for OrderRecordingSpawner {
            async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
                self.order.lock().unwrap().push(req.description.clone());
                Ok("done\n{\"status\": \"DONE\"}".to_string())
            }
        }
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let concrete = Arc::new(OrderRecordingSpawner {
            order: Mutex::new(vec![]),
        });
        let spawner: Arc<dyn SubagentSpawner> = concrete.clone();
        let tasks = parse_plan_tasks("### Task 1: A\na\n### Task 2: B\nb\n### Task 3: C\nc\n");
        let wf = SddWorkflow::new(tasks);
        wf.drive(
            &spawner,
            store,
            "ses_order",
            CancellationToken::new(),
            None,
            None,
            &worktree,
        )
        .await
        .unwrap();
        // Implementer descriptions are "sdd task {index}" (see implementer_req)
        // — the recorded order must match task order exactly, proving
        // sequential (not concurrent/reordered) dispatch. Phase B's
        // review/fix dispatches also land in `order` (description
        // "sdd node", from spawn_one) since this mock doesn't distinguish
        // implementer vs. review prompts — filter down to just the
        // implementer entries before asserting order.
        let order = concrete.order.lock().unwrap().clone();
        let implementer_order: Vec<&String> =
            order.iter().filter(|d| d.starts_with("sdd task")).collect();
        assert_eq!(
            implementer_order,
            vec!["sdd task 1", "sdd task 2", "sdd task 3"]
        );
    }

    #[tokio::test]
    async fn drive_emits_progress_when_sink_present() {
        if !otto_vcs::git::git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (_repo, _data, worktree) = init_repo().await;
        use tokio::sync::mpsc;
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let concrete = std::sync::Arc::new(BatchSpawner {
            prompts: std::sync::Mutex::new(vec![]),
            batches: std::sync::Mutex::new(0),
        });
        let spawner: Arc<dyn SubagentSpawner> = concrete.clone();
        let tasks = parse_plan_tasks("### Task 1: A\na\n### Task 2: B\nb\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let wf = SddWorkflow::new(tasks);
        wf.drive(
            &spawner,
            store,
            "ses_e",
            CancellationToken::new(),
            Some(tx),
            None,
            &worktree,
        )
        .await
        .unwrap();
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            got.push(ev);
        }
        // Each task is announced RUNNING immediately before its own
        // (sequential) dispatch, so the status panel updates continuously
        // through the implementer phase.
        assert!(got.iter().any(|e| e.status == "RUNNING"));
        // At least one IMPLEMENTED and one DONE per task streamed.
        assert!(got.iter().any(|e| e.status == "IMPLEMENTED"));
        assert!(got.iter().any(|e| e.status == "DONE"));
        assert!(got.iter().any(|e| e.task_index == Some(1)));
    }

    #[tokio::test]
    async fn drive_taps_subagent_activity() {
        if !otto_vcs::git::git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (_repo, _data, worktree) = init_repo().await;
        use tokio::sync::mpsc;
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let concrete = Arc::new(BatchSpawner {
            prompts: Mutex::new(vec![]),
            batches: Mutex::new(0),
        });
        let spawner: Arc<dyn SubagentSpawner> = concrete.clone();
        let tasks = parse_plan_tasks("### Task 1: A\nbuild a\n");
        let (act_tx, mut act_rx) = mpsc::unbounded_channel();
        let wf = SddWorkflow::new(tasks);
        wf.drive(
            &spawner,
            store,
            "ses_tap",
            CancellationToken::new(),
            None,
            Some(act_tx),
            &worktree,
        )
        .await
        .unwrap();
        // The mock forwarded a ToolCall on the tapped request's event_tx; the
        // tap's forwarder task summarizes it and sends a SubagentActivity tagged
        // with the task index.
        let act = act_rx.recv().await.expect("tapped activity");
        assert_eq!(act.task_index, 1);
        assert_eq!(act.verb, "bash");
        assert_eq!(act.detail, "x");
    }

    #[tokio::test]
    async fn drive_rejects_duplicate_task_index() {
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let concrete = Arc::new(BatchSpawner {
            prompts: Mutex::new(vec![]),
            batches: Mutex::new(0),
        });
        let spawner: Arc<dyn SubagentSpawner> = concrete.clone();
        // Two tasks share index 2 → must error before spawning anything.
        let tasks = vec![
            PlanTask {
                index: 2,
                title: "A".to_string(),
                body: "build a".to_string(),
            },
            PlanTask {
                index: 2,
                title: "B".to_string(),
                body: "build b".to_string(),
            },
        ];
        let wf = SddWorkflow::new(tasks);
        let worktree = Arc::new(Worktree::new(PathBuf::new(), PathBuf::new()));
        let err = wf
            .drive(
                &spawner,
                store,
                "ses_dup",
                CancellationToken::new(),
                None,
                None,
                &worktree,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WfError::Gate(_)), "got {err:?}");
        // Nothing was dispatched.
        assert_eq!(*concrete.batches.lock().unwrap(), 0);
    }

    /// A spawner whose review response is configurable per task: task 1's
    /// review returns unparseable text, task 2's review approves normally.
    /// Implementer prompts always succeed.
    struct FlakyReviewSpawner {
        batches: Mutex<u32>,
    }
    #[async_trait::async_trait]
    impl SubagentSpawner for FlakyReviewSpawner {
        async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
            if req.prompt.contains("Review the task") {
                if req.prompt.contains("Task 1:") {
                    Ok("I couldn't finish reviewing this.".to_string())
                } else {
                    Ok("looks good\n{\"approved\": true, \"findings\": []}".to_string())
                }
            } else {
                Ok("implemented it\n{\"status\": \"DONE\"}".to_string())
            }
        }
        async fn spawn_many(&self, reqs: Vec<SubagentRequest>) -> Vec<Result<String, ToolError>> {
            *self.batches.lock().unwrap() += 1;
            let mut out = Vec::with_capacity(reqs.len());
            for r in reqs {
                out.push(self.spawn(r).await);
            }
            out
        }
    }

    #[tokio::test]
    async fn unparseable_verdict_degrades_one_task_not_the_whole_run() {
        if !otto_vcs::git::git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (_repo, _data, worktree) = init_repo().await;
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(FlakyReviewSpawner {
            batches: Mutex::new(0),
        });
        let tasks = parse_plan_tasks("### Task 1: A\nbuild a\n### Task 2: B\nbuild b\n");
        let wf = SddWorkflow::new(tasks);
        let report = wf
            .drive(
                &spawner,
                store,
                "ses_flaky",
                CancellationToken::new(),
                None,
                None,
                &worktree,
            )
            .await
            .expect("drive must not error on a bad verdict");
        assert_eq!(report.tasks.len(), 2, "both tasks must still be reported");
        let t1 = report.tasks.iter().find(|t| t.index == 1).unwrap();
        assert_eq!(t1.status, TaskStatus::NeedsContext);
        assert!(!t1.approved);
        let t2 = report.tasks.iter().find(|t| t.index == 2).unwrap();
        assert_eq!(t2.status, TaskStatus::Done);
        assert!(t2.approved);
    }

    #[tokio::test]
    async fn already_cancelled_marks_every_task_cancelled_and_dispatches_nothing() {
        if !otto_vcs::git::git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (_repo, _data, worktree) = init_repo().await;
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let concrete = Arc::new(BatchSpawner {
            prompts: Mutex::new(vec![]),
            batches: Mutex::new(0),
        });
        let spawner: Arc<dyn SubagentSpawner> = concrete.clone();
        let tasks = parse_plan_tasks("### Task 1: A\na\n### Task 2: B\nb\n");
        let abort = CancellationToken::new();
        abort.cancel();
        let wf = SddWorkflow::new(tasks);
        let report = wf
            .drive(&spawner, store, "ses_cancel", abort, None, None, &worktree)
            .await
            .expect("drive must not error on cancellation");
        assert_eq!(report.tasks.len(), 2);
        assert!(
            report
                .tasks
                .iter()
                .all(|t| t.status == TaskStatus::Cancelled)
        );
        assert_eq!(
            *concrete.batches.lock().unwrap(),
            0,
            "nothing should be dispatched once already cancelled"
        );
        assert!(
            worktree.list().await.unwrap().is_empty(),
            "already-cancelled must create zero worktrees"
        );
    }

    #[tokio::test]
    async fn cancellation_during_batch_still_completes_and_merges_dispatched_tasks() {
        // Once Phase A's batch dispatch has started, cancellation can no
        // longer stop already-in-flight implementers (spawn_many can't be
        // interrupted mid-batch) — their work still completes and merges
        // back rather than being discarded. Phase B's existing, unchanged
        // cancellation check then sees the run as cancelled and skips
        // review for every task.
        if !otto_vcs::git::git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (repo, _data, worktree) = init_repo().await;
        struct CancelDuringSpawner {
            abort: CancellationToken,
            calls: Mutex<u32>,
        }
        #[async_trait::async_trait]
        impl SubagentSpawner for CancelDuringSpawner {
            async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
                *self.calls.lock().unwrap() += 1;
                self.abort.cancel();
                if let Some(dir) = &req.directory {
                    let name = req.description.replace(' ', "_");
                    std::fs::write(dir.join(format!("{name}.txt")), "x").unwrap();
                }
                Ok("done\n{\"status\": \"DONE\"}".to_string())
            }
        }
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let abort = CancellationToken::new();
        let concrete = Arc::new(CancelDuringSpawner {
            abort: abort.clone(),
            calls: Mutex::new(0),
        });
        let spawner: Arc<dyn SubagentSpawner> = concrete.clone();
        let tasks = parse_plan_tasks("### Task 1: A\na\n### Task 2: B\nb\n");
        let wf = SddWorkflow::new(tasks);
        let report = wf
            .drive(
                &spawner,
                store,
                "ses_cancel_mid",
                abort,
                None,
                None,
                &worktree,
            )
            .await
            .expect("drive must not error");
        // Both tasks were already batched before cancellation fired inside
        // the first spawn() call — the default serial spawn_many still
        // dispatches task 2 too (no per-item abort check mid-batch).
        assert_eq!(*concrete.calls.lock().unwrap(), 2);
        // Both implementers' work landed in the shared repo root...
        assert!(repo.path().join("sdd_task_1.txt").exists());
        assert!(repo.path().join("sdd_task_2.txt").exists());
        // ...but Phase B's pre-existing cancellation check still demotes
        // both to Cancelled — dispatched-but-unreviewed is treated as
        // Cancelled, same as before this feature.
        assert_eq!(report.tasks[0].status, TaskStatus::Cancelled);
        assert_eq!(report.tasks[1].status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn worktree_creation_failure_blocks_the_task_without_dispatching_it() {
        if !otto_vcs::git::git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (repo, data, _worktree) = init_repo().await;
        // Make data_root an ordinary FILE so `create_dir_all` (inside
        // Worktree::create) fails deterministically.
        let bad_data_root = data.path().join("not-a-dir");
        std::fs::write(&bad_data_root, "x").unwrap();
        let worktree = Arc::new(Worktree::new(repo.path().to_path_buf(), bad_data_root));

        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let concrete = Arc::new(BatchSpawner {
            prompts: Mutex::new(vec![]),
            batches: Mutex::new(0),
        });
        let spawner: Arc<dyn SubagentSpawner> = concrete.clone();
        let tasks = parse_plan_tasks("### Task 1: A\na\n");
        let wf = SddWorkflow::new(tasks);
        let report = wf
            .drive(
                &spawner,
                store,
                "ses_wt_fail",
                CancellationToken::new(),
                None,
                None,
                &worktree,
            )
            .await
            .unwrap();
        assert_eq!(report.tasks[0].status, TaskStatus::Blocked);
        // Nothing was ever dispatched — the implementer never ran.
        assert_eq!(*concrete.batches.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn merge_conflict_blocks_only_the_conflicting_task() {
        if !otto_vcs::git::git_available().await {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let (repo, _data, worktree) = init_repo().await;
        struct ConflictingSpawner;
        #[async_trait::async_trait]
        impl SubagentSpawner for ConflictingSpawner {
            async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
                if req.prompt.contains("Review the task") {
                    return Ok("looks good\n{\"approved\": true, \"findings\": []}".to_string());
                }
                if let Some(dir) = &req.directory {
                    // Both tasks rewrite the SAME line of the SAME tracked
                    // file, based on the same original content ("hello") —
                    // an unavoidable conflict once one side's change has
                    // already been merged.
                    let content = if req.description == "sdd task 1" {
                        "AAA"
                    } else {
                        "BBB"
                    };
                    std::fs::write(dir.join("f.txt"), content).unwrap();
                }
                Ok("implemented\n{\"status\": \"DONE\"}".to_string())
            }
        }
        let store = otto_storage::Store::open_in_memory().await.unwrap();
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(ConflictingSpawner);
        let tasks = parse_plan_tasks("### Task 1: A\na\n### Task 2: B\nb\n");
        let wf = SddWorkflow::new(tasks);
        let report = wf
            .drive(
                &spawner,
                store,
                "ses_conflict",
                CancellationToken::new(),
                None,
                None,
                &worktree,
            )
            .await
            .unwrap();
        let t1 = report.tasks.iter().find(|t| t.index == 1).unwrap();
        let t2 = report.tasks.iter().find(|t| t.index == 2).unwrap();
        // Task 1 merges first (dispatched/merged in task order) and
        // succeeds; task 2's patch no longer applies cleanly once task 1's
        // change has already landed, so it degrades to Blocked instead of
        // corrupting the tree.
        assert_eq!(t1.status, TaskStatus::Done);
        assert_eq!(t2.status, TaskStatus::Blocked);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("f.txt")).unwrap(),
            "AAA"
        );
    }
}
