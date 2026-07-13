//! The native TDD cycle as a state machine. The Iron Law — no production code
//! accepted before a test has been seen to fail for a genuine reason — is a
//! property of this graph: `VerifyRed` sits on the only path to `GreenImpl`.

use std::path::Path;
use std::sync::Arc;

use otto_storage::model::MessageId;
use otto_tools::{SubagentRequest, SubagentSpawner};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::classify::{RedKind, classify_red};
use crate::error::WfError;
use crate::regression::{RegressionOutcome, regression_check};
use crate::runner::TestRunner;

/// Named states, recorded in the report as the machine advances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TddPhase {
    WriteTest,
    VerifyRed,
    GreenImpl,
    VerifyGreen,
    Regression,
    Refactor,
    Done,
}

/// What a run of the cycle produced.
#[derive(Debug)]
pub struct TddReport {
    pub phases: Vec<TddPhase>,
    pub attempts: u32,
    pub regression: RegressionOutcome,
}

#[derive(Debug, Deserialize)]
struct Meaningful {
    meaningful: bool,
    #[allow(dead_code)]
    reason: String,
}

/// The TDD cycle for one `feature`.
pub struct TddWorkflow {
    pub feature: String,
    pub max_attempts: u32,
}

impl TddWorkflow {
    #[must_use]
    pub fn new(feature: impl Into<String>) -> Self {
        Self {
            feature: feature.into(),
            max_attempts: 3,
        }
    }

    /// Drive the cycle against explicit collaborators (used by the CLI and by
    /// tests). `spawner`/`abort`/`parent` back the LLM nodes; `runner`/`cwd`
    /// back the mechanical gates.
    #[allow(clippy::too_many_arguments)]
    pub async fn drive(
        &self,
        spawner: &Arc<dyn SubagentSpawner>,
        runner: &dyn TestRunner,
        cwd: &Path,
        parent: &str,
        abort: CancellationToken,
        progress: Option<crate::ProgressSink>,
        subagent: Option<crate::SubagentSink>,
    ) -> Result<TddReport, WfError> {
        let mut phases = Vec::new();
        let mut attempts = 0u32;

        // --- WriteTest ↔ VerifyRed until a genuine red (bounded) ---
        loop {
            attempts += 1;
            if attempts > self.max_attempts {
                return Err(WfError::Gate(format!(
                    "no genuine failing test after {} attempts",
                    self.max_attempts
                )));
            }
            phases.push(TddPhase::WriteTest);
            crate::emit(&progress, None, "WRITE_TEST", "");
            let note = if attempts == 1 {
                String::new()
            } else {
                "\nThe previous test did not fail for a real reason (it had a compile error or passed). Fix it so it FAILS on a missing/incorrect implementation.".to_string()
            };
            self.spawn_node(
                spawner,
                &abort,
                parent,
                &format!(
                    "Write a single, minimal FAILING test for this feature: {}. Do not write the implementation.{note}",
                    self.feature
                ),
                &subagent,
            )
            .await?;

            // JudgeMeaningful (LLM judgment, structured)
            let m: Meaningful = crate::judge::judge(
                spawner,
                "general",
                parent,
                format!(
                    "You just wrote a test for: {}. Is it a meaningful test that asserts real behavior (not a tautology, not empty)? Return JSON {{\"meaningful\": bool, \"reason\": string}}.",
                    self.feature
                ),
                abort.clone(),
            )
            .await?;
            if !m.meaningful {
                continue;
            }

            // VerifyRed gate
            phases.push(TddPhase::VerifyRed);
            crate::emit(&progress, None, "VERIFY_RED", "");
            let red = runner.run(None).await;
            match classify_red(&red) {
                RedKind::GenuineRed => break, // Iron Law satisfied — proceed
                RedKind::CompileError | RedKind::Passed => continue, // bounce to WriteTest
            }
        }

        // Commit the failing test so the post-green stash removes only the
        // impl. Committing here (after a genuine red, before any production
        // code exists) means the sole working-tree delta at regression time is
        // the implementation — this works even when the test and the impl live
        // in the same file, which the old before/after file diff could not
        // handle.
        commit_all(
            cwd,
            &format!("test: failing test for {} (red)", self.feature),
        )
        .await?;

        // --- GreenImpl ↔ VerifyGreen until pass (bounded) ---
        let mut green_attempts = 0u32;
        loop {
            green_attempts += 1;
            if green_attempts > self.max_attempts {
                return Err(WfError::Gate(format!(
                    "implementation did not pass the test after {} attempts",
                    self.max_attempts
                )));
            }
            phases.push(TddPhase::GreenImpl);
            crate::emit(&progress, None, "GREEN_IMPL", "");
            self.spawn_node(
                spawner,
                &abort,
                parent,
                &format!(
                    "Write the MINIMAL production code to make the failing test pass for: {}. Do not modify the test.",
                    self.feature
                ),
                &subagent,
            )
            .await?;
            phases.push(TddPhase::VerifyGreen);
            crate::emit(&progress, None, "VERIFY_GREEN", "");
            if runner.run(None).await.passed {
                break;
            }
        }

        // --- Regression: prove the test fails without the new prod code ---
        phases.push(TddPhase::Regression);
        crate::emit(&progress, None, "REGRESSION", "");
        let regression = regression_check(runner, cwd).await?;
        if regression == RegressionOutcome::TestPassedWithoutProd {
            return Err(WfError::Gate(
                "regression: the test passes even without the implementation".to_string(),
            ));
        }

        phases.push(TddPhase::Done);
        crate::emit(&progress, None, "DONE", "");
        Ok(TddReport {
            phases,
            attempts,
            regression,
        })
    }

    async fn spawn_node(
        &self,
        spawner: &Arc<dyn SubagentSpawner>,
        abort: &CancellationToken,
        parent: &str,
        prompt: &str,
        subagent: &Option<crate::SubagentSink>,
    ) -> Result<String, WfError> {
        let req = SubagentRequest {
            subagent_type: "general".to_string(),
            description: "tdd node".to_string(),
            prompt: prompt.to_string(),
            parent_session_id: parent.into(),
            parent_message_id: MessageId::default(),
            task_id: None,
            command: None,
            abort: abort.clone(),
            // The TDD cycle is single-track (no per-task index), so all node
            // activity is tagged under task index 0.
            event_tx: crate::tap_subagent(0, subagent),
        };
        spawner.spawn(req).await.map_err(WfError::from)
    }
}

/// Stage and commit everything in `cwd`. Used to commit the failing test after
/// a genuine red so the post-green regression stash removes only the impl.
async fn commit_all(cwd: &Path, msg: &str) -> Result<(), WfError> {
    otto_vcs::git::run_git(cwd, &["add", "-A"])
        .await
        .map_err(|e| WfError::Run(format!("git add: {e}")))?;
    otto_vcs::git::run_git(cwd, &["commit", "-q", "-m", msg])
        .await
        .map_err(|e| WfError::Run(format!("git commit: {e}")))?;
    Ok(())
}

#[async_trait::async_trait]
impl crate::Workflow for TddWorkflow {
    type Output = TddReport;
    async fn run(&self, cx: &crate::WfCtx) -> Result<Self::Output, WfError> {
        self.drive(
            &cx.spawner,
            cx.runner.as_ref(),
            &cx.directory,
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
    use crate::runner::TestOutcome;
    use otto_tools::ToolError;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Spawner that returns canned text; JudgeMeaningful always gets
    /// meaningful=true. The WriteTest node writes a test file and the GreenImpl
    /// node writes a production file to `dir`, mirroring what real subagents
    /// do — the WriteTest file is what `drive`'s post-red `commit_all` commits,
    /// so the regression stash removes only the (untracked) GreenImpl file.
    struct MockSpawner {
        dir: PathBuf,
    }
    #[async_trait::async_trait]
    impl SubagentSpawner for MockSpawner {
        async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
            // The judge prompt asks for JSON; everything else is a plain node.
            if req.prompt.contains("Return JSON") {
                Ok("{\"meaningful\": true, \"reason\": \"asserts add()\"}".to_string())
            } else if req.prompt.contains("FAILING test") {
                // Give the post-red `commit_all` a real file to commit.
                std::fs::write(self.dir.join("test_file.rs"), "test").unwrap();
                Ok("done".to_string())
            } else if req.prompt.contains("production code") {
                std::fs::write(self.dir.join("prod.rs"), "impl").unwrap();
                Ok("done".to_string())
            } else {
                Ok("done".to_string())
            }
        }
    }

    /// Runner returning a scripted sequence of outcomes.
    struct ScriptRunner {
        seq: Mutex<std::collections::VecDeque<TestOutcome>>,
    }
    impl ScriptRunner {
        fn new(outcomes: Vec<TestOutcome>) -> Self {
            Self {
                seq: Mutex::new(outcomes.into_iter().collect()),
            }
        }
    }
    #[async_trait::async_trait]
    impl TestRunner for ScriptRunner {
        async fn run(&self, _filter: Option<&str>) -> TestOutcome {
            self.seq
                .lock()
                .unwrap()
                .pop_front()
                .expect("runner called more than scripted")
        }
    }

    fn red() -> TestOutcome {
        TestOutcome {
            passed: false,
            exit_code: 101,
            stdout: "running 1 test\ntest t ... FAILED\ntest result: FAILED".to_string(),
            failures: vec![],
        }
    }
    fn compile_err() -> TestOutcome {
        TestOutcome {
            passed: false,
            exit_code: 101,
            stdout: "error[E0425]: cannot find value\nerror: could not compile".to_string(),
            failures: vec![],
        }
    }
    fn pass() -> TestOutcome {
        TestOutcome {
            passed: true,
            exit_code: 0,
            stdout: "test result: ok. 1 passed".to_string(),
            failures: vec![],
        }
    }

    // NOTE: `drive` runs real git on `cwd` (the post-red `commit_all` and the
    // regression stash/pop). Tests run it in a temp git repo so the git calls
    // succeed; the regression node's suite outcomes are fed by the ScriptRunner
    // (fail-then-pass) so no real stashing semantics are needed for the
    // state-machine assertions. A committed `seed` file exists up front; the
    // WriteTest node writes `test_file.rs` (committed by `commit_all` after the
    // red), and the GreenImpl node writes `prod.rs` (the sole untracked delta
    // the regression stash then removes).
    async fn temp_git_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        run_git_ok(d.path(), &["init", "-q"]).await;
        run_git_ok(d.path(), &["config", "user.email", "t@t.t"]).await;
        run_git_ok(d.path(), &["config", "user.name", "t"]).await;
        // one committed file so status has a baseline; the working tree is
        // otherwise clean until GreenImpl writes "prod.rs".
        std::fs::write(d.path().join("seed"), "x").unwrap();
        run_git_ok(d.path(), &["add", "-A"]).await;
        run_git_ok(d.path(), &["commit", "-qm", "seed"]).await;
        d
    }
    async fn run_git_ok(dir: &Path, args: &[&str]) {
        otto_vcs::git::run_git(dir, args).await.unwrap();
    }

    #[tokio::test]
    async fn happy_path_reaches_done() {
        let d = temp_git_repo().await;
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(MockSpawner {
            dir: d.path().to_path_buf(),
        });
        // VerifyRed=red, VerifyGreen=pass, regression: without=fail, restore-with=pass
        let runner = ScriptRunner::new(vec![red(), pass(), red(), pass()]);
        let wf = TddWorkflow::new("add(a,b)");
        let report = wf
            .drive(
                &spawner,
                &runner,
                d.path(),
                "ses_1",
                CancellationToken::new(),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(report.regression, RegressionOutcome::Verified);
        assert!(report.phases.contains(&TddPhase::Done));
        // Iron Law: VerifyRed appears before the first GreenImpl.
        let vr = report
            .phases
            .iter()
            .position(|p| *p == TddPhase::VerifyRed)
            .unwrap();
        let gi = report
            .phases
            .iter()
            .position(|p| *p == TddPhase::GreenImpl)
            .unwrap();
        assert!(
            vr < gi,
            "VerifyRed must precede GreenImpl: {:?}",
            report.phases
        );
    }

    #[tokio::test]
    async fn compile_error_red_bounces_and_does_not_reach_green_early() {
        let d = temp_git_repo().await;
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(MockSpawner {
            dir: d.path().to_path_buf(),
        });
        // First VerifyRed = compile error (bounce), second VerifyRed = genuine red,
        // then VerifyGreen=pass, regression without=fail, with=pass.
        let runner = ScriptRunner::new(vec![compile_err(), red(), pass(), red(), pass()]);
        let wf = TddWorkflow::new("add(a,b)");
        let report = wf
            .drive(
                &spawner,
                &runner,
                d.path(),
                "ses_1",
                CancellationToken::new(),
                None,
                None,
            )
            .await
            .unwrap();
        // WriteTest occurred at least twice (bounce), and only one GreenImpl.
        let writes = report
            .phases
            .iter()
            .filter(|p| **p == TddPhase::WriteTest)
            .count();
        let greens = report
            .phases
            .iter()
            .filter(|p| **p == TddPhase::GreenImpl)
            .count();
        assert!(
            writes >= 2,
            "compile-error red must bounce to WriteTest: {:?}",
            report.phases
        );
        assert_eq!(greens, 1);
    }

    /// Spawner used to guard the fix for the misattribution bug: `spawn`
    /// writes a REAL test file on the WriteTest prompt (distinct from the
    /// production file GreenImpl writes), and a REAL production edit on the
    /// GreenImpl prompt. Paired with a real `AutoRunner` running actual cargo,
    /// this exercises the same file-attribution logic `drive` uses in
    /// production instead of scripting the runner's answers.
    struct GreenAfterRedSpawner {
        dir: PathBuf,
    }
    #[async_trait::async_trait]
    impl SubagentSpawner for GreenAfterRedSpawner {
        async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
            if req.prompt.contains("Return JSON") {
                Ok("{\"meaningful\": true, \"reason\": \"asserts add()\"}".to_string())
            } else if req.prompt.contains("FAILING test") {
                // Mirrors what a real WriteTest subagent does: creates a NEW
                // test file, separate from the eventual production file.
                std::fs::create_dir_all(self.dir.join("tests")).unwrap();
                std::fs::write(
                    self.dir.join("tests/feature.rs"),
                    "#[test]\nfn adds() {\n    assert_eq!(demo::add(1, 2), 3);\n}\n",
                )
                .unwrap();
                Ok("wrote test".to_string())
            } else if req.prompt.contains("production code") {
                std::fs::write(
                    self.dir.join("src/lib.rs"),
                    "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
                )
                .unwrap();
                Ok("wrote impl".to_string())
            } else {
                Ok("done".to_string())
            }
        }
    }

    /// Cargo project committed with a CLEAN baseline: `add` compiles and is
    /// `pub`, but there is no test yet and no bug to fix up front — the
    /// WriteTest node adds the failing test, GreenImpl node fixes `add`.
    async fn setup_clean_cargo_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        std::fs::write(
            p.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(p.join("src")).unwrap();
        // Ignore build artifacts so `commit_all`'s `git add -A` and the
        // regression `--include-untracked` stash never sweep `target/`.
        std::fs::write(p.join(".gitignore"), "/target\nCargo.lock\n").unwrap();
        std::fs::write(
            p.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { 0 }\n",
        )
        .unwrap();
        run_git_ok(p, &["init", "-q"]).await;
        run_git_ok(p, &["config", "user.email", "t@t.t"]).await;
        run_git_ok(p, &["config", "user.name", "t"]).await;
        run_git_ok(p, &["add", "-A"]).await;
        run_git_ok(p, &["commit", "-qm", "clean baseline"]).await;
        d
    }

    /// Real-runner integration test guarding the commit-the-test regression
    /// mechanism: WriteTest creates `tests/feature.rs`, GreenImpl edits
    /// `src/lib.rs`. After the genuine red, `drive` commits the test via
    /// `commit_all`, so the post-green regression stash removes only the
    /// (untracked/modified) `src/lib.rs` and leaves the committed test in
    /// place. The without-impl run then finds the test still present and
    /// failing (genuine red), and the cycle completes as `Verified`. Under the
    /// old file-diff scheme this same layout risked folding the test file into
    /// `prod_files` and stashing it away with the impl; committing the test
    /// removes that hazard entirely (and handles same-file test+impl layouts).
    #[tokio::test]
    async fn regression_does_not_stash_the_new_test_file() {
        let d = setup_clean_cargo_repo().await;
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(GreenAfterRedSpawner {
            dir: d.path().to_path_buf(),
        });
        let runner = crate::runner::AutoRunner::new(d.path().to_path_buf());
        let wf = TddWorkflow::new("add(a,b)");
        let report = wf
            .drive(
                &spawner,
                &runner,
                d.path(),
                "ses_1",
                CancellationToken::new(),
                None,
                None,
            )
            .await
            .expect("drive should succeed: the test file must not be misattributed as a prod file");
        assert_eq!(report.regression, RegressionOutcome::Verified);
        assert!(report.phases.contains(&TddPhase::Done));
    }

    /// Spawner whose judge rejects the first written test as not meaningful,
    /// then accepts the second — exercises the JudgeMeaningful bounce back to
    /// WriteTest (Finding 2).
    struct JudgeBounceSpawner {
        dir: PathBuf,
        judge_calls: std::sync::atomic::AtomicU32,
    }
    #[async_trait::async_trait]
    impl SubagentSpawner for JudgeBounceSpawner {
        async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
            if req.prompt.contains("Return JSON") {
                let n = self
                    .judge_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Ok("{\"meaningful\": false, \"reason\": \"tautology\"}".to_string())
                } else {
                    Ok("{\"meaningful\": true, \"reason\": \"asserts add()\"}".to_string())
                }
            } else if req.prompt.contains("FAILING test") {
                std::fs::write(self.dir.join("test_file.rs"), "test").unwrap();
                Ok("done".to_string())
            } else if req.prompt.contains("production code") {
                std::fs::write(self.dir.join("prod.rs"), "impl").unwrap();
                Ok("done".to_string())
            } else {
                Ok("done".to_string())
            }
        }
    }

    #[tokio::test]
    async fn judge_rejects_first_test_then_accepts_bounce() {
        let d = temp_git_repo().await;
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(JudgeBounceSpawner {
            dir: d.path().to_path_buf(),
            judge_calls: std::sync::atomic::AtomicU32::new(0),
        });
        // attempt1: judge rejects -> continue, no VerifyRed run consumed.
        // attempt2: judge accepts -> VerifyRed=red; then VerifyGreen=pass;
        // regression without=red (fails, as required), with=pass.
        let runner = ScriptRunner::new(vec![red(), pass(), red(), pass()]);
        let wf = TddWorkflow::new("add(a,b)");
        let report = wf
            .drive(
                &spawner,
                &runner,
                d.path(),
                "ses_1",
                CancellationToken::new(),
                None,
                None,
            )
            .await
            .unwrap();
        let writes = report
            .phases
            .iter()
            .filter(|p| **p == TddPhase::WriteTest)
            .count();
        assert!(
            writes >= 2,
            "an unmeaningful test judgment must bounce back to WriteTest: {:?}",
            report.phases
        );
        assert!(report.phases.contains(&TddPhase::Done));
    }

    #[tokio::test]
    async fn judge_never_meaningful_exhausts_attempts() {
        let d = temp_git_repo().await;
        struct AlwaysUnmeaningfulSpawner;
        #[async_trait::async_trait]
        impl SubagentSpawner for AlwaysUnmeaningfulSpawner {
            async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
                if req.prompt.contains("Return JSON") {
                    Ok("{\"meaningful\": false, \"reason\": \"tautology\"}".to_string())
                } else {
                    Ok("done".to_string())
                }
            }
        }
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(AlwaysUnmeaningfulSpawner);
        // VerifyRed is never reached because the judge never accepts.
        let runner = ScriptRunner::new(vec![]);
        let wf = TddWorkflow::new("add(a,b)");
        let r = wf
            .drive(
                &spawner,
                &runner,
                d.path(),
                "ses_1",
                CancellationToken::new(),
                None,
                None,
            )
            .await;
        assert!(matches!(r, Err(WfError::Gate(_))));
    }

    #[tokio::test]
    async fn never_reaches_genuine_red_errors_out() {
        let d = temp_git_repo().await;
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(MockSpawner {
            dir: d.path().to_path_buf(),
        });
        // Every VerifyRed is a compile error -> exhausts max_attempts (3).
        let runner = ScriptRunner::new(vec![compile_err(), compile_err(), compile_err()]);
        let wf = TddWorkflow::new("add(a,b)");
        let r = wf
            .drive(
                &spawner,
                &runner,
                d.path(),
                "ses_1",
                CancellationToken::new(),
                None,
                None,
            )
            .await;
        assert!(matches!(r, Err(WfError::Gate(_))));
    }

    /// With a sink present, `drive` emits at least a WRITE_TEST and a DONE event
    /// along the happy path.
    #[tokio::test]
    async fn drive_emits_progress() {
        let d = temp_git_repo().await;
        let spawner: Arc<dyn SubagentSpawner> = Arc::new(MockSpawner {
            dir: d.path().to_path_buf(),
        });
        let runner = ScriptRunner::new(vec![red(), pass(), red(), pass()]);
        let wf = TddWorkflow::new("add(a,b)");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        wf.drive(
            &spawner,
            &runner,
            d.path(),
            "ses_1",
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
            got.iter().any(|e| e.status == "WRITE_TEST"),
            "expected a WRITE_TEST event: {got:?}"
        );
        assert!(
            got.iter().any(|e| e.status == "DONE"),
            "expected a DONE event: {got:?}"
        );
    }
}
