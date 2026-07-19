//! Unit tests for the `clap` command tree.

use clap::Parser;
use otto_cli::cli::{AuthCommand, Cli, Commands, ProvidersCommand};

#[test]
fn parses_run_with_agent_and_model() {
    let cli = Cli::try_parse_from([
        "otto",
        "run",
        "hello",
        "--agent",
        "plan",
        "--model",
        "anthropic/claude-sonnet-4",
    ])
    .expect("parses");
    match cli.command {
        Commands::Run(args) => {
            assert_eq!(args.message, vec!["hello".to_string()]);
            assert_eq!(args.agent.as_deref(), Some("plan"));
            assert_eq!(args.model.as_deref(), Some("anthropic/claude-sonnet-4"));
            assert!(!args.continue_);
            assert!(!args.yes);
        }
        other => panic!("expected run, got {other:?}"),
    }
}

#[test]
fn run_joins_multiple_message_words() {
    let cli = Cli::try_parse_from(["otto", "run", "fix", "the", "bug"]).expect("parses");
    match cli.command {
        Commands::Run(args) => assert_eq!(args.message, vec!["fix", "the", "bug"]),
        other => panic!("expected run, got {other:?}"),
    }
}

#[test]
fn run_continue_and_yes_flags() {
    let cli = Cli::try_parse_from(["otto", "run", "--continue", "--yes", "go"]).expect("parses");
    match cli.command {
        Commands::Run(args) => {
            assert!(args.continue_);
            assert!(args.yes);
            assert_eq!(args.message, vec!["go"]);
        }
        other => panic!("expected run, got {other:?}"),
    }
}

#[test]
fn parses_serve_with_port() {
    let cli = Cli::try_parse_from(["otto", "serve", "--port", "8080"]).expect("parses");
    match cli.command {
        Commands::Serve(args) => {
            assert_eq!(args.port, 8080);
            assert_eq!(args.hostname, "127.0.0.1");
            assert!(!args.cors);
        }
        other => panic!("expected serve, got {other:?}"),
    }
}

#[test]
fn serve_defaults_to_4096() {
    let cli = Cli::try_parse_from(["otto", "serve"]).expect("parses");
    match cli.command {
        Commands::Serve(args) => assert_eq!(args.port, 4096),
        other => panic!("expected serve, got {other:?}"),
    }
}

#[test]
fn parses_global_cwd() {
    let cli = Cli::try_parse_from(["otto", "--cwd", "/tmp/x", "models"]).expect("parses");
    assert_eq!(cli.cwd, std::path::PathBuf::from("/tmp/x"));
    assert!(matches!(cli.command, Commands::Models(_)));
}

#[test]
fn parses_providers_login() {
    let cli = Cli::try_parse_from(["otto", "providers", "login", "anthropic"]).expect("parses");
    match cli.command {
        Commands::Providers(args) => match args.command {
            ProvidersCommand::Login {
                provider,
                enterprise,
            } => {
                assert_eq!(provider, "anthropic");
                assert_eq!(enterprise, None, "flag is opt-in");
            }
            other => panic!("expected login, got {other:?}"),
        },
        other => panic!("expected providers, got {other:?}"),
    }
}

/// `--enterprise` is what populates the credential's `enterprise_url`, which
/// is the only thing that switches Copilot off the public API host. Without
/// this flag there was no way to reach an enterprise Copilot endpoint at all.
#[test]
fn parses_copilot_login_with_enterprise_domain() {
    let cli = Cli::try_parse_from([
        "otto",
        "auth",
        "login",
        "github-copilot",
        "--enterprise",
        "acme.ghe.com",
    ])
    .expect("parses");
    match cli.command {
        Commands::Auth(args) => match args.command {
            AuthCommand::Login {
                provider,
                enterprise,
            } => {
                assert_eq!(provider, "github-copilot");
                assert_eq!(enterprise.as_deref(), Some("acme.ghe.com"));
            }
            other => panic!("expected login, got {other:?}"),
        },
        other => panic!("expected auth, got {other:?}"),
    }
}

#[test]
fn models_takes_optional_provider() {
    let cli = Cli::try_parse_from(["otto", "models", "openai"]).expect("parses");
    match cli.command {
        Commands::Models(args) => assert_eq!(args.provider.as_deref(), Some("openai")),
        other => panic!("expected models, got {other:?}"),
    }
}

#[test]
fn unknown_subcommand_errors() {
    let err = Cli::try_parse_from(["otto", "frobnicate"]);
    assert!(err.is_err(), "unknown subcommand must fail to parse");
}

#[test]
fn missing_subcommand_errors() {
    let err = Cli::try_parse_from(["otto"]);
    assert!(err.is_err(), "a subcommand is required");
}

#[test]
fn parses_tui_with_server() {
    let cli = Cli::try_parse_from(["otto", "tui", "--server", "http://localhost:4096"]).unwrap();
    assert!(matches!(cli.command, Commands::Tui(_)));
}

#[test]
fn parses_worktree_create_with_name() {
    use otto_cli::cli::WorktreeCommand;
    let cli = Cli::try_parse_from(["otto", "worktree", "create", "--name", "feat"]).unwrap();
    match cli.command {
        Commands::Worktree(a) => match a.command {
            WorktreeCommand::Create { name } => assert_eq!(name.as_deref(), Some("feat")),
            _ => panic!("wrong subcommand"),
        },
        _ => panic!("wrong command"),
    }
}
