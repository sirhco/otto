//! otto — Rust agentic coding harness.
//!
//! The library surface behind the `otto` binary: the [`cli`] command tree, the
//! terminal [`render`]er, the testable [`run`] flow, and the remaining
//! [`commands`] (serve / models / providers / auth / agent / mcp). [`dispatch`]
//! maps a parsed [`Cli`](cli::Cli) onto those handlers.
//!
//! The provider-facing seams are injectable so the whole system runs headless
//! in tests: [`run::run_session`] takes an arbitrary output [`std::io::Write`],
//! an injectable [`run::PermissionResponder`], and whatever route factory the
//! [`Runtime`](otto_app::Runtime) was built with, while the listing renderers
//! ([`commands::render_models`], [`commands::render_agents`],
//! [`commands::render_providers`], [`commands::render_mcp`]) write to an
//! arbitrary sink.

#![forbid(unsafe_code)]

pub mod cli;
pub mod commands;
pub mod render;
pub mod run;
mod workflow;

use anyhow::Result;

use crate::cli::{AgentCommand, Cli, Commands, McpCommand};

/// Run the parsed [`Cli`] to completion, dispatching to the matching handler.
///
/// # Errors
/// Propagates any handler error (which the binary surfaces as a non-zero exit).
pub async fn dispatch(cli: Cli) -> Result<()> {
    let cwd = cli.cwd.as_path();
    match cli.command {
        Commands::Run(args) => run::cmd_run(cwd, args).await,
        Commands::Serve(args) => {
            commands::cmd_serve(cwd, args.port, &args.hostname, args.password, args.cors).await
        }
        Commands::Models(args) => {
            if args.refresh {
                let cache = otto_config::paths::global_cache_dir().join("models.json");
                let opts = otto_llm::models_dev::LoadOptions::from_env(cache);
                let reg = otto_llm::models_dev::refresh(&opts).await;
                let n = reg.len();
                otto_llm::registry::install(reg);
                println!("Refreshed {n} models.");
            }
            let mut stdout = std::io::stdout();
            commands::render_models(args.provider.as_deref(), &mut stdout)?;
            Ok(())
        }
        Commands::Providers(args) => commands::cmd_providers(cwd, args.command).await,
        Commands::Auth(args) => commands::cmd_auth(cwd, args.command).await,
        Commands::Agent(args) => match args.command {
            AgentCommand::List => commands::cmd_agent_list(cwd).await,
        },
        Commands::Mcp(args) => match args.command {
            McpCommand::List => commands::cmd_mcp_list(cwd).await,
        },
        Commands::Worktree(args) => commands::cmd_worktree(cwd, args.command).await,
        Commands::Tui(args) => {
            commands::cmd_tui(cwd, args.server, args.password, args.no_splash).await
        }
        Commands::Workflow(args) => workflow::cmd_workflow(cwd, args.command).await,
    }
}
