//! The `clap` command tree for the `otto` binary.
//!
//! [`Cli`] carries the global flags and the [`Commands`] subcommand union; each
//! subcommand's arguments live in its own `Args` struct. The types are public
//! so the dispatcher and the parsing tests can construct and match on them.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// `otto` — Rust agentic coding harness.
#[derive(Debug, Parser)]
#[command(
    name = "otto",
    version,
    about = "otto — Rust agentic coding harness",
    long_about = "otto is a Rust agentic coding harness: run an agent turn, serve the \
                  HTTP/SSE API, and manage models, providers, agents, and MCP servers."
)]
pub struct Cli {
    /// Working directory the runtime is rooted at.
    #[arg(long, global = true, default_value = ".")]
    pub cwd: PathBuf,

    /// Log verbosity (e.g. `debug`, `info`, `warn`, `error`).
    #[arg(long, global = true)]
    pub log_level: Option<String>,

    /// Print logs to stderr instead of a log file.
    #[arg(long, global = true)]
    pub print_logs: bool,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Commands,
}

/// The top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Run a single agent turn against a prompt.
    Run(RunArgs),
    /// Serve the HTTP + SSE API.
    Serve(ServeArgs),
    /// List available models.
    Models(ModelsArgs),
    /// Manage providers and their credentials.
    Providers(ProvidersArgs),
    /// Manage authentication credentials (alias of `providers`).
    Auth(AuthArgs),
    /// Inspect agents.
    Agent(AgentArgs),
    /// Inspect MCP servers.
    Mcp(McpArgs),
    /// Manage git worktrees (isolated agent workspaces).
    Worktree(WorktreeArgs),
    /// Launch the terminal UI.
    Tui(TuiArgs),
    /// Run a native dev-loop workflow (TDD / SDD / plan execution).
    Workflow(WorkflowArgs),
}

/// Arguments for `otto run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The prompt. Multiple words are joined; if omitted, read from stdin.
    pub message: Vec<String>,

    /// Run as this agent (defaults to the configured default agent).
    #[arg(long)]
    pub agent: Option<String>,

    /// Generate with this `provider/model` (defaults to the configured model).
    #[arg(long)]
    pub model: Option<String>,

    /// Continue the session with this id.
    #[arg(long)]
    pub session: Option<String>,

    /// Continue the most recent session in this directory.
    #[arg(long = "continue")]
    pub continue_: bool,

    /// Auto-allow permission requests when running non-interactively.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for `otto serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Port to bind (0 selects a random free port).
    #[arg(long, default_value_t = 4096)]
    pub port: u16,

    /// Hostname / interface to bind.
    #[arg(long, default_value = "127.0.0.1")]
    pub hostname: String,

    /// Basic-auth password gate (also read from `otto_SERVER_PASSWORD`).
    #[arg(long, env = "otto_SERVER_PASSWORD")]
    pub password: Option<String>,

    /// Enable permissive CORS.
    #[arg(long)]
    pub cors: bool,
}

/// Arguments for `otto models`.
#[derive(Debug, Args)]
pub struct ModelsArgs {
    /// Only list models from this provider.
    pub provider: Option<String>,

    /// Force a fresh fetch of the models.dev registry before listing.
    #[arg(long)]
    pub refresh: bool,
}

/// Arguments for `otto providers`.
#[derive(Debug, Args)]
pub struct ProvidersArgs {
    /// The providers subcommand.
    #[command(subcommand)]
    pub command: ProvidersCommand,
}

/// `otto providers` subcommands.
#[derive(Debug, Subcommand)]
pub enum ProvidersCommand {
    /// List configured providers and whether credentials are present.
    List,
    /// Log in to a provider (API key or, for anthropic, OAuth).
    Login {
        /// The provider id (e.g. `anthropic`, `openai`).
        provider: String,
    },
    /// Remove a provider's stored credentials.
    Logout {
        /// The provider id.
        provider: String,
    },
}

/// Arguments for `otto auth`.
#[derive(Debug, Args)]
pub struct AuthArgs {
    /// The auth subcommand.
    #[command(subcommand)]
    pub command: AuthCommand,
}

/// `otto auth` subcommands (shared with `providers`).
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// List stored credentials (redacted).
    List,
    /// Log in to a provider.
    Login {
        /// The provider id.
        provider: String,
    },
    /// Remove a provider's stored credentials.
    Logout {
        /// The provider id.
        provider: String,
    },
}

/// Arguments for `otto agent`.
#[derive(Debug, Args)]
pub struct AgentArgs {
    /// The agent subcommand.
    #[command(subcommand)]
    pub command: AgentCommand,
}

/// `otto agent` subcommands.
#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// List resolved agents.
    List,
}

/// Arguments for `otto mcp`.
#[derive(Debug, Args)]
pub struct McpArgs {
    /// The mcp subcommand.
    #[command(subcommand)]
    pub command: McpCommand,
}

/// `otto mcp` subcommands.
#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// List configured MCP servers and their connection status.
    List,
}

/// Arguments for `otto worktree`.
#[derive(Debug, Args)]
pub struct WorktreeArgs {
    /// The worktree subcommand.
    #[command(subcommand)]
    pub command: WorktreeCommand,
}

/// Arguments for `otto tui`.
#[derive(Debug, Args)]
pub struct TuiArgs {
    /// Attach to an already-running server instead of auto-spawning one.
    #[arg(long)]
    pub server: Option<String>,
    /// Basic-auth password for `--server`.
    #[arg(long)]
    pub password: Option<String>,
    /// Skip the startup splash screen.
    #[arg(long)]
    pub no_splash: bool,
}

/// `otto worktree` subcommands.
#[derive(Debug, Subcommand)]
pub enum WorktreeCommand {
    /// List managed worktrees.
    List,
    /// Create a new worktree on a `otto/<name>` branch.
    Create {
        /// A name for the worktree (slugified). Defaults to `workspace`.
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove a worktree and delete its branch.
    Remove {
        /// Absolute path of the worktree directory.
        directory: String,
    },
    /// Hard-reset a worktree to the origin default branch.
    Reset {
        /// Absolute path of the worktree directory.
        directory: String,
    },
}

/// Arguments for `otto workflow`.
#[derive(Debug, Args)]
pub struct WorkflowArgs {
    /// The workflow subcommand.
    #[command(subcommand)]
    pub command: WorkflowCommand,
}

/// `otto workflow` subcommands.
#[derive(Debug, Subcommand)]
pub enum WorkflowCommand {
    /// Native test-driven-development cycle (Phase 3).
    Tdd {
        /// The feature to drive a TDD cycle for.
        #[arg(long)]
        feature: String,
        /// Preview only: print what would run, dispatch no subagents, leave the
        /// working tree untouched.
        #[arg(long)]
        dry_run: bool,
    },
    /// Native subagent-driven-development orchestration (Phase 4).
    Sdd {
        /// Path to the plan file whose `### Task N` sections drive the run.
        #[arg(long)]
        plan: String,
        /// Preview only: parse the plan + print the task list, dispatch no
        /// subagents, leave the working tree untouched.
        #[arg(long)]
        dry_run: bool,
    },
    /// Plan-execution + verification gate (Phase 5).
    Plan {
        /// Path to the plan file whose `### Task N` sections are executed in order.
        #[arg(long)]
        plan: String,
        /// Preview only: parse the plan + print the task list and the
        /// verification commands, dispatch no subagents, leave the tree untouched.
        #[arg(long)]
        dry_run: bool,
    },
}

#[cfg(test)]
mod workflow_cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_workflow_tdd() {
        let cli = Cli::try_parse_from(["otto", "workflow", "tdd", "--feature", "x"]).unwrap();
        match cli.command {
            Commands::Workflow(args) => {
                assert!(matches!(args.command, WorkflowCommand::Tdd { .. }));
            }
            _ => panic!("expected Workflow command"),
        }
    }

    #[test]
    fn workflow_tdd_takes_a_feature() {
        let cli =
            Cli::try_parse_from(["otto", "workflow", "tdd", "--feature", "add(a,b)"]).unwrap();
        match cli.command {
            Commands::Workflow(a) => match a.command {
                WorkflowCommand::Tdd { feature, dry_run } => {
                    assert_eq!(feature, "add(a,b)");
                    assert!(!dry_run);
                }
                _ => panic!("expected Tdd"),
            },
            _ => panic!("expected Workflow"),
        }
    }

    #[test]
    fn parses_workflow_sdd_with_plan() {
        let sdd = Cli::try_parse_from(["otto", "workflow", "sdd", "--plan", "p.md"]).unwrap();
        match sdd.command {
            Commands::Workflow(a) => match a.command {
                WorkflowCommand::Sdd { plan, dry_run } => {
                    assert_eq!(plan, "p.md");
                    assert!(!dry_run);
                }
                _ => panic!("expected Sdd"),
            },
            _ => panic!("expected Workflow"),
        }
    }

    #[test]
    fn parses_workflow_plan_with_plan_file() {
        let cli = Cli::try_parse_from(["otto", "workflow", "plan", "--plan", "p.md"]).unwrap();
        match cli.command {
            Commands::Workflow(a) => match a.command {
                WorkflowCommand::Plan { plan, dry_run } => {
                    assert_eq!(plan, "p.md");
                    assert!(!dry_run);
                }
                _ => panic!("expected Plan"),
            },
            _ => panic!("expected Workflow"),
        }
    }

    #[test]
    fn workflow_dry_run_flag_parses() {
        let cli = Cli::try_parse_from(["otto", "workflow", "sdd", "--plan", "p.md", "--dry-run"])
            .unwrap();
        match cli.command {
            Commands::Workflow(a) => match a.command {
                WorkflowCommand::Sdd { dry_run, .. } => assert!(dry_run),
                _ => panic!("expected Sdd"),
            },
            _ => panic!("expected Workflow"),
        }
    }

    #[test]
    fn rejects_unknown_workflow_kind() {
        assert!(Cli::try_parse_from(["otto", "workflow", "bogus"]).is_err());
    }
}
