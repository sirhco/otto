//! Process-wide `tracing` initialization for the `otto` binary.
//!
//! Until now the `--log-level` / `--print-logs` flags were parsed but dead —
//! no subscriber was ever installed, so every `tracing::warn!` in the
//! workspace (skipped stream frames, retry/salvage decisions) vanished and a
//! stalled turn left no forensics. [`init`] wires them up:
//!
//! * default sink: a daily-rolling file `{data_dir}/logs/otto.log.YYYY-MM-DD`
//!   (e.g. `~/Library/Application Support/otto/logs/` on macOS) — safe for the
//!   TUI, whose alternate screen would garble stderr output;
//! * `--print-logs`: log to stderr instead (useful for `otto run` / `serve`);
//! * filter precedence: `OTTO_LOG` env (full `EnvFilter` directive syntax,
//!   e.g. `otto_session=debug,otto_llm=trace`) > `--log-level` > config
//!   `logLevel` > `warn`.

use std::path::Path;
use std::sync::OnceLock;

use otto_config::LogLevel;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Keeps the non-blocking writer's flush thread alive for the process
/// lifetime (dropping the guard would silently stop writing).
static GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// The env var accepted as a full `EnvFilter` directive string.
const ENV: &str = "OTTO_LOG";

/// Install the global `tracing` subscriber. Idempotent and infallible by
/// design: logging must never block a command from running, so any failure
/// (unwritable log dir, double init in tests) degrades to no logging.
pub fn init(cli_level: Option<&str>, print_logs: bool, cwd: &Path) {
    let filter = resolve_filter(cli_level, cwd);

    if print_logs {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_ansi(true)
            .try_init();
        return;
    }

    let dir = otto_config::paths::global_data_dir().join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let appender = tracing_appender::rolling::daily(&dir, "otto.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    if tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .try_init()
        .is_ok()
    {
        let _ = GUARD.set(guard);
    }
}

/// Resolve the filter: `OTTO_LOG` > `--log-level` > config `logLevel` > warn.
fn resolve_filter(cli_level: Option<&str>, cwd: &Path) -> EnvFilter {
    if let Ok(directive) = std::env::var(ENV)
        && !directive.is_empty()
        && let Ok(filter) = EnvFilter::try_new(&directive)
    {
        return filter;
    }
    let level = cli_level
        .map(str::to_string)
        .or_else(|| {
            // Tolerant config load (mirrors the TUI's theme load): a broken
            // config must not block startup, and logging least of all.
            otto_config::load(cwd)
                .ok()
                .and_then(|c| c.log_level)
                .map(|l| {
                    match l {
                        LogLevel::Debug => "debug",
                        LogLevel::Info => "info",
                        LogLevel::Warn => "warn",
                        LogLevel::Error => "error",
                    }
                    .to_string()
                })
        })
        .unwrap_or_else(|| "warn".to_string());
    // Validate as a plain level: `EnvFilter` would happily accept any string
    // as a *target* directive ("garbage" → "garbage=trace"), silencing all
    // logging instead of falling back.
    if level.parse::<tracing::Level>().is_ok() {
        EnvFilter::new(level)
    } else {
        EnvFilter::new("warn")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_level_beats_default() {
        let f = resolve_filter(Some("debug"), Path::new("/nonexistent"));
        assert_eq!(f.to_string(), "debug");
    }

    #[test]
    fn default_is_warn() {
        let f = resolve_filter(None, Path::new("/nonexistent"));
        assert_eq!(f.to_string(), "warn");
    }

    #[test]
    fn garbage_cli_level_degrades_to_warn() {
        let f = resolve_filter(Some("not a level!!"), Path::new("/nonexistent"));
        assert_eq!(f.to_string(), "warn");
    }
}
