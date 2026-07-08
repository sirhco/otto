//! Verification-before-completion: a compiled table mapping a completion
//! `Claim` to the toolchain command that must pass before the claim is
//! accepted. Cargo-first (mirroring the crate's cargo-only classification
//! scope); non-cargo toolchains map to `None` in v1 and are skipped.

use std::path::Path;

/// A completion claim whose truth is decided by a command's exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// The project compiles.
    Builds,
    /// The test suite passes.
    TestsPass,
    /// The linter is clean.
    LintClean,
    /// The code is formatted.
    Formatted,
}

/// The compiled claim→command table. Returns the command that must succeed to
/// back `claim` in `dir`, or `None` if the toolchain is unmapped (v1: only
/// cargo). Cargo-first: presence of `Cargo.toml` selects the cargo commands.
#[must_use]
pub fn command_for_claim(claim: Claim, dir: &Path) -> Option<Vec<String>> {
    if dir.join("Cargo.toml").exists() {
        let cmd: Vec<String> = match claim {
            Claim::Builds => vec!["cargo", "build"],
            Claim::TestsPass => vec!["cargo", "test"],
            Claim::LintClean => vec!["cargo", "clippy", "--", "-D", "warnings"],
            Claim::Formatted => vec!["cargo", "fmt", "--", "--check"],
        }
        .into_iter()
        .map(String::from)
        .collect();
        Some(cmd)
    } else {
        // v1: non-cargo toolchains are not yet mapped (see Global Constraints).
        None
    }
}

use tokio_util::sync::CancellationToken;

use crate::runner::run_command;

/// One claim's verification outcome.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub claim: Claim,
    pub command: Vec<String>,
    pub passed: bool,
    pub exit_code: i32,
    pub output: String,
}

/// The outcome of running a `VerificationGate`.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub results: Vec<CheckResult>,
}

impl VerifyReport {
    /// True if every check that ran passed. An empty report (no mapped checks)
    /// is vacuously passed — the caller decides whether "nothing to verify" is
    /// acceptable.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.results.iter().all(|r| r.passed)
    }

    /// The checks that failed.
    #[must_use]
    pub fn failures(&self) -> Vec<&CheckResult> {
        self.results.iter().filter(|r| !r.passed).collect()
    }
}

/// A set of claim→command checks to run before accepting a completion.
pub struct VerificationGate {
    checks: Vec<(Claim, Vec<String>)>,
}

impl VerificationGate {
    /// Build a gate for `claims`, dropping any that map to `None` for `dir`.
    #[must_use]
    pub fn for_claims(claims: &[Claim], dir: &Path) -> Self {
        let checks = claims
            .iter()
            .filter_map(|&c| command_for_claim(c, dir).map(|cmd| (c, cmd)))
            .collect();
        Self { checks }
    }

    /// Number of mapped checks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.checks.len()
    }

    /// Whether the gate has no checks (all claims were unmapped).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }

    /// Run each check via `run_command`, collecting a `VerifyReport`. Never
    /// errors: a failed command is a failed `CheckResult`.
    pub async fn verify(
        &self,
        dir: &Path,
        timeout_ms: u64,
        abort: CancellationToken,
    ) -> VerifyReport {
        let mut results = Vec::with_capacity(self.checks.len());
        for (claim, cmd) in &self.checks {
            let outcome = run_command(&cmd[0], &cmd[1..], dir, timeout_ms, abort.clone()).await;
            results.push(CheckResult {
                claim: *claim,
                command: cmd.clone(),
                passed: outcome.passed,
                exit_code: outcome.exit_code,
                output: outcome.stdout,
            });
        }
        VerifyReport { results }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cargo_dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        d
    }

    #[test]
    fn cargo_claims_map_to_cargo_commands() {
        let d = cargo_dir();
        assert_eq!(
            command_for_claim(Claim::Builds, d.path()),
            Some(vec!["cargo".into(), "build".into()])
        );
        assert_eq!(
            command_for_claim(Claim::TestsPass, d.path()),
            Some(vec!["cargo".into(), "test".into()])
        );
        assert_eq!(
            command_for_claim(Claim::LintClean, d.path()),
            Some(vec![
                "cargo".into(),
                "clippy".into(),
                "--".into(),
                "-D".into(),
                "warnings".into()
            ])
        );
        assert_eq!(
            command_for_claim(Claim::Formatted, d.path()),
            Some(vec![
                "cargo".into(),
                "fmt".into(),
                "--".into(),
                "--check".into()
            ])
        );
    }

    #[test]
    fn non_cargo_dir_maps_to_none() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(command_for_claim(Claim::Builds, d.path()), None);
        assert_eq!(command_for_claim(Claim::TestsPass, d.path()), None);
    }

    use tokio_util::sync::CancellationToken;

    /// A real, compiling cargo project so `cargo build` + `cargo test` pass.
    async fn buildable_cargo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        std::fs::write(
            p.join("Cargo.toml"),
            "[package]\nname=\"vg\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(
            p.join("src/lib.rs"),
            "pub fn f() -> i32 { 1 }\n#[cfg(test)]\nmod t{#[test]fn a(){assert_eq!(super::f(),1);}}\n",
        )
        .unwrap();
        d
    }

    #[tokio::test]
    async fn gate_passes_on_a_good_build() {
        let d = buildable_cargo().await;
        let gate = VerificationGate::for_claims(&[Claim::Builds], d.path());
        assert_eq!(gate.len(), 1);
        let report = gate
            .verify(d.path(), 120_000, CancellationToken::new())
            .await;
        assert!(
            report.all_passed(),
            "expected build to pass: {:?}",
            report.failures()
        );
    }

    #[tokio::test]
    async fn gate_fails_on_a_broken_build() {
        let d = buildable_cargo().await;
        // Break compilation.
        std::fs::write(d.path().join("src/lib.rs"), "pub fn f() -> i32 { \n").unwrap();
        let gate = VerificationGate::for_claims(&[Claim::Builds], d.path());
        let report = gate
            .verify(d.path(), 120_000, CancellationToken::new())
            .await;
        assert!(!report.all_passed());
        assert_eq!(report.failures().len(), 1);
        assert_eq!(report.failures()[0].claim, Claim::Builds);
    }

    #[tokio::test]
    async fn unmapped_claims_are_skipped_not_failed() {
        let d = tempfile::tempdir().unwrap(); // no Cargo.toml
        let gate = VerificationGate::for_claims(&[Claim::Builds, Claim::TestsPass], d.path());
        assert!(gate.is_empty(), "unmapped claims must not produce checks");
        let report = gate.verify(d.path(), 5_000, CancellationToken::new()).await;
        assert!(report.all_passed(), "no checks = vacuously passed");
        assert!(report.results.is_empty());
    }
}
