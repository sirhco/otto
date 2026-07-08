//! Classifies a failing test run as a genuine RED (assertion / feature
//! missing) vs a compile error / typo that must NOT be accepted as the TDD
//! red step. Cargo-first; other runners are a follow-up.

use crate::runner::TestOutcome;

/// The nature of a test run for the TDD `VerifyRed` gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedKind {
    /// The suite passed — not a red at all.
    Passed,
    /// The build failed before tests ran (compile error / typo). NOT a valid red.
    CompileError,
    /// Tests ran and a test failed for a real reason (assertion / missing feature).
    GenuineRed,
}

/// Classify a `TestOutcome`. A genuine red requires that the test binary
/// actually ran and reported a failure; a build that never compiled is a
/// `CompileError`, which the TDD engine bounces back to WriteTest.
#[must_use]
pub fn classify_red(outcome: &TestOutcome) -> RedKind {
    if outcome.passed {
        return RedKind::Passed;
    }
    let s = &outcome.stdout;
    let lower = s.to_lowercase();
    let compile_error = s.contains("error[E")
        || lower.contains("could not compile")
        || lower.contains("syntaxerror")
        || lower.contains("error: expected")
        || lower.contains("cannot find");
    let tests_ran =
        s.contains("test result:") || s.contains("running ") || lower.contains("panicked");
    if compile_error && !tests_ran {
        return RedKind::CompileError;
    }
    // Failed, and either the test binary ran (test result / panic) or there is
    // no compile-error signature — treat as a genuine assertion/feature red.
    RedKind::GenuineRed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(passed: bool, stdout: &str) -> TestOutcome {
        TestOutcome {
            passed,
            exit_code: if passed { 0 } else { 101 },
            stdout: stdout.to_string(),
            failures: Vec::new(),
        }
    }

    #[test]
    fn passing_suite_is_passed() {
        let o = outcome(true, "test result: ok. 5 passed; 0 failed");
        assert_eq!(classify_red(&o), RedKind::Passed);
    }

    #[test]
    fn compile_error_is_not_a_red() {
        let stdout = "error[E0425]: cannot find value `foo` in this scope\nerror: could not compile `demo` (bin \"demo\") due to 1 previous error";
        assert_eq!(classify_red(&outcome(false, stdout)), RedKind::CompileError);
    }

    #[test]
    fn assertion_failure_is_genuine_red() {
        let stdout = "running 1 test\ntest checks_answer ... FAILED\n\nfailures:\n---- checks_answer ----\nthread 'checks_answer' panicked at src/lib.rs:10:5:\nassertion `left == right` failed\n\ntest result: FAILED. 0 passed; 1 failed";
        assert_eq!(classify_red(&outcome(false, stdout)), RedKind::GenuineRed);
    }

    #[test]
    fn feature_missing_panic_is_genuine_red() {
        // Test calls a function that exists but returns wrong value -> panic in test.
        let stdout = "running 1 test\ntest new_feature ... FAILED\nthread 'new_feature' panicked at 'not yet implemented'\ntest result: FAILED. 0 passed; 1 failed";
        assert_eq!(classify_red(&outcome(false, stdout)), RedKind::GenuineRed);
    }

    #[test]
    fn syntax_error_is_compile_error() {
        let stdout = "error: expected `;`, found `}`\nerror: could not compile `demo`";
        assert_eq!(classify_red(&outcome(false, stdout)), RedKind::CompileError);
    }
}
