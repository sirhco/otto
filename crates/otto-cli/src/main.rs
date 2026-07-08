//! otto — Rust agentic coding harness.
//!
//! Thin binary entrypoint: parse the [`Cli`](otto_cli::cli::Cli) command tree
//! and hand off to [`otto_cli::dispatch`]. A handler error is printed to
//! stderr and turned into a non-zero exit code.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use otto_cli::cli::Cli;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match otto_cli::dispatch(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
