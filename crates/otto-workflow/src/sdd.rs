//! Native subagent-driven-development: parse a plan into tasks, fan the
//! implementers out through `spawn_many` (parallel), then run a bounded
//! per-task review→fix loop, recording every status to the ledger.

use std::sync::Arc;

use otto_tools::{SubagentRequest, SubagentSpawner};
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
    found.unwrap_or(TaskStatus::NeedsContext)
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
/// # v1 concurrency constraint (SHARED working tree)
///
/// In v1 the implementers fan out in parallel (`spawn_many`) into the SINGLE
/// SHARED working tree — there is NO per-task worktree isolation. Two
/// consequences follow, and callers MUST respect them:
///
/// 1. Tasks MUST touch disjoint files. Concurrent implementers editing the same
///    file will clobber one another.
/// 2. Implementers do NOT commit or stage. The engine leaves ALL changes in the
///    working tree for the review→fix phase; nothing is version-controlled by
///    the agents. Run `otto workflow sdd` on a dedicated feature branch so the
///    accumulated working-tree changes are easy to inspect and commit yourself.
///
/// Per-task worktree isolation (so implementers can safely commit in parallel)
/// is a deferred (P5) seam.
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

    /// Drive the full SDD loop against explicit collaborators.
    ///
    /// # Errors
    /// Returns [`WfError`] on a ledger write failure or an unparseable review
    /// verdict.
    pub async fn drive(
        &self,
        spawner: &Arc<dyn SubagentSpawner>,
        store: otto_storage::Store,
        parent: &str,
        abort: CancellationToken,
        progress: Option<crate::ProgressSink>,
        subagent: Option<crate::SubagentSink>,
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

        // --- Phase A: fan out ALL implementers in one parallel batch ---
        // Announce every task as running BEFORE the batch await. `spawn_many`
        // blocks until the whole batch finishes, and the per-task emits below
        // only fire afterward — so without this, an observer (the TUI status
        // panel) sees nothing for the entire implementer phase (the longest,
        // most permission-heavy part of a run). Emitting up front populates the
        // panel immediately; the `IMPLEMENTED` emit overwrites each as it lands.
        for t in &self.tasks {
            crate::emit(
                &progress,
                Some(t.index),
                "RUNNING",
                "implementer dispatched",
            );
        }
        let reqs: Vec<SubagentRequest> = self
            .tasks
            .iter()
            .map(|t| self.implementer_req(t, parent, &abort, &subagent))
            .collect();
        let results = spawner.spawn_many(reqs).await;

        // Parse each implementer status; record to the ledger.
        let mut statuses = Vec::with_capacity(self.tasks.len());
        for (t, res) in self.tasks.iter().zip(results) {
            let status = match res {
                Ok(text) => parse_status(&text),
                Err(_) => TaskStatus::Blocked,
            };
            ledger.record(t.index, status, "implemented").await?;
            // Happy-path tasks proceed into the review loop (which re-emits
            // REVIEWING→DONE), so "IMPLEMENTED" is just a phase marker. A task
            // that BLOCKED or NEEDS_CONTEXT skips the review loop below, making
            // this its ONLY progress event — surface its real terminal status.
            let phase = if matches!(status, TaskStatus::Done | TaskStatus::DoneWithConcerns) {
                "IMPLEMENTED"
            } else {
                status.as_wire()
            };
            crate::emit(&progress, Some(t.index), phase, status.as_wire());
            statuses.push(status);
        }

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
            result.reviewed = true;
            let mut round = 0u32;
            loop {
                crate::emit(&progress, Some(t.index), "REVIEWING", "");
                let verdict = self.review(spawner, t, parent, &abort, &subagent).await?;
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
                self.fix(spawner, t, &verdict.findings, parent, &abort, &subagent)
                    .await?;
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
    ) -> SubagentRequest {
        SubagentRequest {
            subagent_type: "general".to_string(),
            description: format!("sdd task {}", t.index),
            prompt: format!(
                "Implement this task. Write the code, add the tests, and run the \
                 test suite to confirm they pass. DO NOT run any git commands \
                 (no add / stage / commit) — the workflow manages version \
                 control and other implementers are editing the same working \
                 tree concurrently. Leave your changes in the working tree.\n\n\
                 ## Task {}: {}\n{}\n\n\
                 End your reply with one JSON line: {{\"status\": \"DONE\"}} \
                 (or DONE_WITH_CONCERNS / NEEDS_CONTEXT / BLOCKED).",
                t.index, t.title, t.body
            ),
            parent_session_id: parent.to_string(),
            parent_message_id: String::new(),
            task_id: None,
            event_tx: crate::tap_subagent(t.index, subagent),
            command: None,
            abort: abort.clone(),
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
        parent_session_id: parent.to_string(),
        parent_message_id: String::new(),
        task_id: None,
        command: None,
        abort: abort.clone(),
        event_tx: crate::tap_subagent(task_index, subagent),
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
            CancellationToken::new(),
            cx.progress.clone(),
            cx.subagent.clone(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TaskStatus;
    use otto_tools::ToolError;
    use std::sync::Mutex;

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
    /// implementer prompts and an "approved" verdict for review prompts.
    /// Also records how many spawn_many BATCH calls happened.
    struct BatchSpawner {
        prompts: Mutex<Vec<String>>,
        batches: Mutex<u32>,
    }
    #[async_trait::async_trait]
    impl SubagentSpawner for BatchSpawner {
        async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
            self.prompts.lock().unwrap().push(req.prompt.clone());
            // If the request was tapped (Task 3), forward one canned tool call
            // so the tap→sink path is exercised end-to-end. Untapped requests
            // (event_tx = None) send nothing → byte-identical to before.
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
    async fn sdd_fans_out_once_and_records_ledger() {
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
            )
            .await
            .unwrap();
        assert_eq!(report.tasks.len(), 2);
        assert!(report.tasks.iter().all(|t| t.status == TaskStatus::Done));
        assert!(report.tasks.iter().all(|t| t.approved));
        // The implementers were dispatched in exactly ONE spawn_many batch.
        assert_eq!(
            *concrete.batches.lock().unwrap(),
            1,
            "implementers must fan out in ONE batch"
        );
        // Ledger has both tasks recorded as DONE.
        let led = Ledger::new(store, "ses_1", "sdd");
        let recs = led.tasks().await.unwrap();
        assert_eq!(recs.len(), 2);
        assert!(recs.iter().all(|r| r.status == TaskStatus::Done));
    }

    #[tokio::test]
    async fn drive_emits_progress_when_sink_present() {
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
        )
        .await
        .unwrap();
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            got.push(ev);
        }
        // Every task is announced RUNNING up front (before the batch await), so
        // the status panel populates during the implementer phase.
        assert!(got.iter().any(|e| e.status == "RUNNING"));
        // At least one IMPLEMENTED and one DONE per task streamed.
        assert!(got.iter().any(|e| e.status == "IMPLEMENTED"));
        assert!(got.iter().any(|e| e.status == "DONE"));
        assert!(got.iter().any(|e| e.task_index == Some(1)));
    }

    #[tokio::test]
    async fn drive_taps_subagent_activity() {
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
        let err = wf
            .drive(
                &spawner,
                store,
                "ses_dup",
                CancellationToken::new(),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WfError::Gate(_)), "got {err:?}");
        // Nothing was dispatched.
        assert_eq!(*concrete.batches.lock().unwrap(), 0);
    }
}
