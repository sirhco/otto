//! Project test-runner abstraction. `AutoRunner` detects the native test
//! command from marker files and shells it via `tokio::process`.

use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Result of running the project test suite.
#[derive(Debug, Clone)]
pub struct TestOutcome {
    pub passed: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub failures: Vec<String>,
}

/// Runs the project's test suite, optionally filtered to a subset.
#[async_trait::async_trait]
pub trait TestRunner: Send + Sync {
    async fn run(&self, filter: Option<&str>) -> TestOutcome;
}

/// Detects and runs the native test command for a directory.
pub struct AutoRunner {
    pub directory: PathBuf,
    pub timeout_ms: u64,
    pub abort: CancellationToken,
}

impl AutoRunner {
    #[must_use]
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            timeout_ms: 600_000,
            abort: CancellationToken::new(),
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }

    #[must_use]
    pub fn with_abort(mut self, token: CancellationToken) -> Self {
        self.abort = token;
        self
    }

    /// The base test command implied by the marker files in `dir`, or `None`
    /// if no known toolchain is detected. First match wins in this order:
    /// Cargo → npm → pytest → go.
    #[must_use]
    pub fn command_for(dir: &Path) -> Option<Vec<String>> {
        if dir.join("Cargo.toml").exists() {
            Some(vec!["cargo".into(), "test".into()])
        } else if dir.join("package.json").exists() {
            Some(vec!["npm".into(), "test".into()])
        } else if dir.join("pyproject.toml").exists() || dir.join("pytest.ini").exists() {
            Some(vec!["pytest".into()])
        } else if dir.join("go.mod").exists() {
            Some(vec!["go".into(), "test".into(), "./...".into()])
        } else {
            None
        }
    }
}

/// Extract failing-test lines from test stdout (cargo/generic `... FAILED`
/// lines and `---- name ----` failure headers).
#[must_use]
pub fn parse_failures(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|l| l.contains("FAILED") || l.trim_start().starts_with("---- "))
        .map(|l| l.trim().to_string())
        .collect()
}

/// Run `program args` in `cwd`, racing completion against a timeout and an
/// abort token (mirrors otto-tools bash.rs). Captures stdout+stderr.
pub async fn run_command(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout_ms: u64,
    abort: CancellationToken,
) -> TestOutcome {
    use tokio::io::AsyncReadExt;

    let child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return TestOutcome {
                passed: false,
                exit_code: -1,
                stdout: format!("failed to spawn test command: {e}"),
                failures: Vec::new(),
            };
        }
    };

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    // `drain` owns `child` (and the pipes) outright so the timeout/abort
    // branches can simply drop the future — `kill_on_drop` then kills the
    // child without needing to reach back into a value already borrowed
    // elsewhere.
    let drain = async move {
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        let mut stdout_pipe = stdout_pipe;
        let mut stderr_pipe = stderr_pipe;
        // Read both pipes concurrently: if the child fills the stderr pipe
        // buffer while we're still blocked reading stdout to EOF (or vice
        // versa), a sequential read-then-read deadlocks the child on its
        // write() call, and it never exits. `tokio::join!` polls both
        // futures together so neither read can starve the other.
        let out_fut = async {
            if let Some(p) = stdout_pipe.as_mut() {
                let _ = p.read_to_end(&mut out_buf).await;
            }
        };
        let err_fut = async {
            if let Some(p) = stderr_pipe.as_mut() {
                let _ = p.read_to_end(&mut err_buf).await;
            }
        };
        tokio::join!(out_fut, err_fut);
        let status = child.wait().await;
        (status, out_buf, err_buf)
    };
    tokio::pin!(drain);

    tokio::select! {
        (status, out_buf, err_buf) = &mut drain => {
            let stdout = String::from_utf8_lossy(&out_buf).to_string();
            let stderr = String::from_utf8_lossy(&err_buf).to_string();
            let (passed, code) = match status {
                Ok(s) => (s.success(), s.code().unwrap_or(-1)),
                Err(_) => (false, -1),
            };
            TestOutcome {
                passed,
                exit_code: code,
                failures: parse_failures(&stdout),
                stdout: format!("{stdout}{stderr}"),
            }
        }
        _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => {
            TestOutcome {
                passed: false,
                exit_code: -1,
                stdout: format!("test command timed out after {timeout_ms}ms"),
                failures: Vec::new(),
            }
        }
        _ = abort.cancelled() => {
            TestOutcome {
                passed: false,
                exit_code: -1,
                stdout: "test command aborted".to_string(),
                failures: Vec::new(),
            }
        }
    }
}

#[async_trait::async_trait]
impl TestRunner for AutoRunner {
    async fn run(&self, filter: Option<&str>) -> TestOutcome {
        let Some(mut cmd) = Self::command_for(&self.directory) else {
            return TestOutcome {
                passed: false,
                exit_code: -1,
                stdout: "no known test runner (no Cargo.toml/package.json/pyproject.toml/go.mod)"
                    .to_string(),
                failures: Vec::new(),
            };
        };
        if let Some(f) = filter {
            cmd.push(f.to_string());
        }
        run_command(
            &cmd[0],
            &cmd[1..],
            &self.directory,
            self.timeout_ms,
            self.abort.clone(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), "").unwrap();
    }

    #[test]
    fn detects_cargo() {
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), "Cargo.toml");
        assert_eq!(
            AutoRunner::command_for(d.path()),
            Some(vec!["cargo".to_string(), "test".to_string()])
        );
    }

    #[test]
    fn detects_npm() {
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), "package.json");
        assert_eq!(
            AutoRunner::command_for(d.path()),
            Some(vec!["npm".to_string(), "test".to_string()])
        );
    }

    #[test]
    fn detects_pytest_via_pyproject() {
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), "pyproject.toml");
        assert_eq!(
            AutoRunner::command_for(d.path()),
            Some(vec!["pytest".to_string()])
        );
    }

    #[test]
    fn detects_go() {
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), "go.mod");
        assert_eq!(
            AutoRunner::command_for(d.path()),
            Some(vec![
                "go".to_string(),
                "test".to_string(),
                "./...".to_string()
            ])
        );
    }

    #[test]
    fn detects_none_for_empty_dir() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(AutoRunner::command_for(d.path()), None);
    }

    #[test]
    fn cargo_wins_over_go_when_both_present() {
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), "Cargo.toml");
        touch(d.path(), "go.mod");
        assert_eq!(
            AutoRunner::command_for(d.path()),
            Some(vec!["cargo".to_string(), "test".to_string()])
        );
    }

    #[test]
    fn parse_failures_finds_failed_lines() {
        let stdout = "test alpha ... ok\ntest beta ... FAILED\ntest gamma ... ok\n\nfailures:\n---- beta ----\n";
        let f = parse_failures(stdout);
        assert!(f.iter().any(|l| l.contains("beta") && l.contains("FAILED")));
        assert!(f.iter().any(|l| l.starts_with("---- beta")));
    }

    #[test]
    fn parse_failures_empty_when_all_pass() {
        assert!(parse_failures("test a ... ok\ntest result: ok. 1 passed").is_empty());
    }

    #[tokio::test]
    async fn run_command_times_out() {
        use tokio_util::sync::CancellationToken;
        let out = super::run_command(
            "sleep",
            &["5".to_string()],
            std::path::Path::new("."),
            150, // ms
            CancellationToken::new(),
        )
        .await;
        assert!(!out.passed);
        assert!(out.stdout.contains("timed out"), "stdout={}", out.stdout);
    }

    #[tokio::test]
    async fn run_command_captures_success() {
        use tokio_util::sync::CancellationToken;
        let out = super::run_command(
            "true",
            &[],
            std::path::Path::new("."),
            5_000,
            CancellationToken::new(),
        )
        .await;
        assert!(out.passed);
        assert_eq!(out.exit_code, 0);
    }

    /// Regression test for a full-duplex pipe deadlock: reading stdout to
    /// EOF, then stderr to EOF (sequentially) can hang forever if the child
    /// fills the *other* pipe's OS buffer (~64KB) while the parent is still
    /// blocked on the first read — the child blocks on write() and never
    /// exits, so the read the parent is waiting on never EOFs either. Emit
    /// large output on both streams and confirm it completes quickly and
    /// captures both.
    #[tokio::test]
    async fn run_command_drains_both_streams_without_deadlock() {
        use tokio_util::sync::CancellationToken;
        let out = super::run_command(
            "sh",
            &[
                "-c".to_string(),
                "yes X | head -c 200000; yes Y | head -c 200000 1>&2".to_string(),
            ],
            std::path::Path::new("."),
            30_000,
            CancellationToken::new(),
        )
        .await;
        assert!(
            out.passed,
            "should complete, stdout_len={}",
            out.stdout.len()
        );
        // both streams captured (stdout+stderr concatenated in `stdout` field)
        assert!(
            out.stdout.len() >= 400_000,
            "captured {} bytes",
            out.stdout.len()
        );
    }
}
