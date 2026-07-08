//! The non-`run` subcommands: `serve`, `models`, `providers`/`auth`, `agent`,
//! and `mcp`.
//!
//! Each listing renderer writes to an arbitrary [`Write`] so tests can assert
//! on the produced text against an in-memory runtime, while the login/logout
//! and serve paths (which touch the network, the credential store, or block on
//! a listener) are exercised manually.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Write};
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use otto_agent::AgentMode;
use otto_app::Runtime;
use otto_auth::providers::{anthropic, api_key, copilot};
use otto_auth::{Credential, Pkce};
use otto_mcp::{McpClient, McpServerConfig};
use otto_server::ServeOptions;

use crate::cli::{AuthCommand, ProvidersCommand};

/// The server version reported to MCP servers on connect.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Fallback env var for the serve password (opencode compatibility).
const OPENCODE_PASSWORD_ENV: &str = "OPENCODE_SERVER_PASSWORD";

/// The known provider ids surfaced by `otto providers`/`otto auth`, drawn
/// from the currently installed [`otto_llm::registry`] snapshot.
fn known_provider_ids() -> BTreeSet<String> {
    otto_llm::registry::current()
        .providers()
        .iter()
        .map(|p| p.id.clone())
        .collect()
}

// -- serve -------------------------------------------------------------------

/// Run the HTTP + SSE server (`otto serve`).
///
/// Loads the runtime, resolves the bind address (`hostname:port`, `port 0` for
/// a random port), prints the URL, and hands off to [`otto_server::serve`].
/// The password falls back from `--password`/`otto_SERVER_PASSWORD` to
/// `OPENCODE_SERVER_PASSWORD` for opencode compatibility.
///
/// # Errors
/// Propagates runtime-load, address-resolution, and server failures.
pub async fn cmd_serve(
    cwd: &std::path::Path,
    port: u16,
    hostname: &str,
    password: Option<String>,
    cors: bool,
) -> Result<()> {
    let runtime = Arc::new(Runtime::load(cwd).await.context("failed to load runtime")?);

    let addr = (hostname, port)
        .to_socket_addrs()
        .with_context(|| format!("could not resolve {hostname}:{port}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("no address resolved for {hostname}:{port}"))?;

    let password = password.or_else(|| std::env::var(OPENCODE_PASSWORD_ENV).ok());

    println!("otto server listening on http://{addr}");
    if password.is_some() {
        println!("(basic-auth enabled)");
    }
    otto_server::serve(runtime, addr, ServeOptions { password, cors })
        .await
        .context("server error")?;
    Ok(())
}

// -- models ------------------------------------------------------------------

/// Write the model listing (optionally filtered by `provider`) to `out`.
///
/// Enumerates the currently installed [`otto_llm::registry`] snapshot (see
/// [`otto_llm::registry::current`]; refreshed beforehand by the `--refresh`
/// dispatch path in `lib.rs`). Each line carries the context window (in
/// thousands of tokens), the comma-joined capability tags, and — when the
/// model publishes pricing — a `$in/$out` per-Mtok cost hint.
///
/// # Errors
/// Propagates writer errors.
pub fn render_models(provider: Option<&str>, out: &mut dyn Write) -> io::Result<()> {
    let registry = otto_llm::registry::current();
    let mut count = 0usize;
    for m in registry.all_models() {
        if let Some(filter) = provider
            && filter != m.provider.0
        {
            continue;
        }
        let context = m
            .limits
            .context
            .map(|c| format!("{}k", c / 1000))
            .unwrap_or_else(|| "?".to_string());
        let mut caps = Vec::new();
        if m.capabilities.tool_call {
            caps.push("tools");
        }
        if m.capabilities.reasoning {
            caps.push("reasoning");
        }
        if m.capabilities.attachment {
            caps.push("vision");
        }
        let caps = caps.join(",");
        let cost = m
            .cost
            .as_ref()
            .map(|c| format!("  ${}/${}", format_cost(c.input), format_cost(c.output)))
            .unwrap_or_default();
        writeln!(
            out,
            "{}/{}  context={context}  [{caps}]{cost}",
            m.provider.0, m.id.0
        )?;
        count += 1;
    }
    if count == 0 {
        writeln!(out, "no models found for provider filter")?;
    }
    Ok(())
}

/// Format a per-Mtok cost value, dropping a trailing `.0` for whole numbers.
fn format_cost(value: Option<f64>) -> String {
    match value {
        None => "?".to_string(),
        Some(v) if v.fract() == 0.0 => format!("{v:.0}"),
        Some(v) => format!("{v}"),
    }
}

// -- providers / auth --------------------------------------------------------

/// Dispatch `otto providers <cmd>`.
///
/// # Errors
/// Propagates runtime-load and credential-store failures.
pub async fn cmd_providers(cwd: &std::path::Path, command: ProvidersCommand) -> Result<()> {
    let runtime = Runtime::load(cwd).await.context("failed to load runtime")?;
    match command {
        ProvidersCommand::List => {
            let mut stdout = io::stdout();
            render_providers(&runtime, &mut stdout)?;
            Ok(())
        }
        ProvidersCommand::Login { provider } => login(&runtime, &provider).await,
        ProvidersCommand::Logout { provider } => logout(&runtime, &provider),
    }
}

/// Dispatch `otto auth <cmd>` — the same implementation as `providers`.
///
/// # Errors
/// Propagates runtime-load and credential-store failures.
pub async fn cmd_auth(cwd: &std::path::Path, command: AuthCommand) -> Result<()> {
    let runtime = Runtime::load(cwd).await.context("failed to load runtime")?;
    match command {
        AuthCommand::List => {
            let mut stdout = io::stdout();
            render_providers(&runtime, &mut stdout)?;
            Ok(())
        }
        AuthCommand::Login { provider } => login(&runtime, &provider).await,
        AuthCommand::Logout { provider } => logout(&runtime, &provider),
    }
}

/// Write the provider listing (each with a redacted credential status) to
/// `out`.
///
/// The provider set is the union of the known model providers and any provider
/// with a stored credential.
///
/// # Errors
/// Propagates writer errors.
pub fn render_providers(runtime: &Runtime, out: &mut dyn Write) -> io::Result<()> {
    let auth = runtime.auth().all().unwrap_or_default();

    let mut providers: BTreeSet<String> = known_provider_ids();
    for key in auth.keys() {
        providers.insert(key.clone());
    }

    for provider in providers {
        let status = match auth.get(&provider) {
            Some(Credential::Api { .. }) => "api key",
            Some(Credential::Oauth { .. }) => "oauth",
            Some(Credential::WellKnown { .. }) => "wellknown",
            None => "not logged in",
        };
        writeln!(out, "{provider}  [{status}]")?;
    }
    Ok(())
}

/// Interactive login for `provider`.
///
/// For `anthropic` the user chooses an API key or the OAuth (PKCE) flow: the
/// authorize URL is printed, the pasted code is exchanged for tokens, and the
/// resulting credential is stored. For `github-copilot` the device-code flow
/// is driven end to end: the verification URL + user code are printed, and
/// the access-token endpoint is polled until authorised. Every other provider
/// stores a plain API key. Requires an interactive terminal.
async fn login(runtime: &Runtime, provider: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("login requires an interactive terminal");
    }
    let store = runtime.auth();

    if provider == "anthropic" {
        println!("anthropic login:");
        println!("  [1] API key");
        println!("  [2] OAuth (Claude Pro/Max)");
        let choice = prompt("choice [1/2]: ")?;
        if choice.trim() == "2" {
            let oauth = anthropic::AnthropicOAuth::new();
            let pkce = Pkce::generate();
            let (url, verifier) = oauth.authorize_url(&pkce);
            println!("\nOpen this URL in your browser to authorize:\n{url}\n");
            let code = prompt("paste the code here: ")?;
            let credential = oauth
                .exchange(code.trim(), &verifier)
                .await
                .context("OAuth code exchange failed")?;
            store.set("anthropic", credential)?;
            println!("stored anthropic OAuth credential");
            return Ok(());
        }
    }

    if provider == "github-copilot" {
        let flow = copilot::CopilotOAuth::new();
        let start = flow
            .start_device()
            .await
            .context("copilot device start failed")?;
        println!(
            "\nOpen {} in your browser and enter code: {}\n",
            start.verification_uri, start.user_code
        );
        let mut interval = start.interval;
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            match flow.poll(&start.device_code).await? {
                copilot::DevicePoll::Complete(cred) => {
                    store.set("github-copilot", *cred)?;
                    println!("stored github-copilot credential");
                    return Ok(());
                }
                copilot::DevicePoll::Pending => {}
                copilot::DevicePoll::SlowDown => interval += 5,
            }
        }
    }

    let key = prompt(&format!("{provider} API key: "))?;
    let key = key.trim();
    if key.is_empty() {
        bail!("no API key entered");
    }
    store.set(provider, api_key::credential(key))?;
    println!("stored {provider} API key");
    Ok(())
}

/// Remove any stored credential for `provider`.
fn logout(runtime: &Runtime, provider: &str) -> Result<()> {
    runtime.auth().remove(provider)?;
    println!("removed credentials for {provider}");
    Ok(())
}

/// Print `label` to stdout and read a line from stdin.
fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("failed to read input")?;
    Ok(line)
}

// -- agent -------------------------------------------------------------------

/// Write the resolved agent listing (name, mode, description) to `out`.
///
/// # Errors
/// Propagates writer errors.
pub fn render_agents(runtime: &Runtime, out: &mut dyn Write) -> io::Result<()> {
    for agent in runtime.agents() {
        let mode = match agent.mode {
            AgentMode::Subagent => "subagent",
            AgentMode::Primary => "primary",
            AgentMode::All => "all",
        };
        let description = agent.description.as_deref().unwrap_or("");
        writeln!(out, "{}  [{mode}]  {description}", agent.name)?;
    }
    Ok(())
}

/// Dispatch `otto agent list`.
///
/// # Errors
/// Propagates runtime-load and writer failures.
pub async fn cmd_agent_list(cwd: &std::path::Path) -> Result<()> {
    let runtime = Runtime::load(cwd).await.context("failed to load runtime")?;
    let mut stdout = io::stdout();
    render_agents(&runtime, &mut stdout)?;
    Ok(())
}

// -- mcp ---------------------------------------------------------------------

/// Write the configured MCP servers and their (best-effort) connection status
/// to `out`.
///
/// # Errors
/// Propagates writer errors. Connection failures are reported inline per
/// server, never fatal.
pub async fn render_mcp(runtime: &Runtime, out: &mut dyn Write) -> Result<()> {
    let servers: BTreeMap<String, McpServerConfig> = runtime
        .config()
        .mcp
        .as_ref()
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default();

    if servers.is_empty() {
        writeln!(out, "no MCP servers configured")?;
        return Ok(());
    }

    let client = McpClient::new(VERSION);
    for (name, config) in &servers {
        let status = match client.connect(name.clone(), config).await {
            Ok(status) => format!("{status:?}"),
            Err(err) => format!("Failed({err})"),
        };
        writeln!(out, "{name}  [{status}]")?;
    }
    Ok(())
}

/// Dispatch `otto mcp list`.
///
/// # Errors
/// Propagates runtime-load and writer failures.
pub async fn cmd_mcp_list(cwd: &std::path::Path) -> Result<()> {
    let runtime = Runtime::load(cwd).await.context("failed to load runtime")?;
    let mut stdout = io::stdout();
    render_mcp(&runtime, &mut stdout).await
}

// -- worktree ----------------------------------------------------------------

/// Build a worktree manager rooted at `cwd`.
async fn worktree_service(cwd: &std::path::Path) -> Result<otto_vcs::worktree::Worktree> {
    let data_base = otto_config::paths::global_data_dir().join("worktree");
    otto_vcs::worktree::Worktree::discover(cwd, &data_base)
        .await
        .context("not a git repository (worktree requires git)")
}

/// Write the worktree listing to `out`.
///
/// # Errors
/// Propagates writer errors.
pub fn render_worktrees(
    list: &[otto_vcs::worktree::WorktreeInfo],
    out: &mut dyn Write,
) -> io::Result<()> {
    if list.is_empty() {
        writeln!(out, "no worktrees")?;
        return Ok(());
    }
    for w in list {
        let branch = w.branch.as_deref().unwrap_or("(detached)");
        writeln!(out, "{}  [{branch}]  {}", w.name, w.directory)?;
    }
    Ok(())
}

/// Dispatch `otto worktree <cmd>`.
///
/// # Errors
/// Propagates git and writer failures.
pub async fn cmd_worktree(
    cwd: &std::path::Path,
    command: crate::cli::WorktreeCommand,
) -> Result<()> {
    use crate::cli::WorktreeCommand;
    let svc = worktree_service(cwd).await?;
    match command {
        WorktreeCommand::List => {
            let list = svc.list().await.context("failed to list worktrees")?;
            let mut stdout = io::stdout();
            render_worktrees(&list, &mut stdout)?;
        }
        WorktreeCommand::Create { name } => {
            let info = svc
                .create(otto_vcs::worktree::CreateInput { name })
                .await
                .context("failed to create worktree")?;
            println!(
                "created {}  [{}]  {}",
                info.name,
                info.branch.as_deref().unwrap_or(""),
                info.directory
            );
        }
        WorktreeCommand::Remove { directory } => {
            svc.remove(otto_vcs::worktree::RemoveInput {
                directory: directory.clone(),
            })
            .await
            .context("failed to remove worktree")?;
            println!("removed {directory}");
        }
        WorktreeCommand::Reset { directory } => {
            svc.reset(otto_vcs::worktree::ResetInput {
                directory: directory.clone(),
            })
            .await
            .context("failed to reset worktree")?;
            println!("reset {directory}");
        }
    }
    Ok(())
}

// -- tui -----------------------------------------------------------------

/// `otto tui` — launch the terminal UI (attach or auto-spawn).
///
/// # Errors
/// Propagates runtime-spawn, HTTP, and terminal failures.
pub async fn cmd_tui(
    cwd: &std::path::Path,
    server: Option<String>,
    password: Option<String>,
    no_splash: bool,
) -> Result<()> {
    otto_tui::run(otto_tui::TuiOptions {
        server,
        password,
        cwd: cwd.to_path_buf(),
        no_splash,
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `otto_llm::registry::install` is a process-global shared across all
    /// tests in this binary, which run in parallel, so every
    /// install-dependent assertion is consolidated into this one test
    /// rather than spread across several (see `otto_llm::registry`'s test
    /// module for the same convention).
    #[test]
    fn render_models_lists_installed_registry() {
        otto_llm::registry::install(
            otto_llm::models_dev::Registry::from_json(include_str!(
                "../../otto-llm/tests/fixtures/models_dev_sample.json"
            ))
            .unwrap(),
        );
        let mut buf = Vec::new();
        render_models(Some("openai"), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("openai/o3"));
        assert!(s.contains("openai/gpt-4o"));
        assert!(!s.contains("anthropic/")); // filtered out
        assert!(s.contains("reasoning")); // o3 caps
        assert!(s.contains("context=200k")); // o3 limits
        assert!(s.contains("$2/$8")); // o3 cost hint
        // gpt-4o has no cost block: no trailing cost hint on its line.
        let gpt4o_line = s.lines().find(|l| l.starts_with("openai/gpt-4o")).unwrap();
        assert!(!gpt4o_line.contains('$'));

        let mut all = Vec::new();
        render_models(None, &mut all).unwrap();
        let all = String::from_utf8(all).unwrap();
        assert!(all.contains("anthropic/claude-opus-4-8"));
        assert!(all.contains("openai/o3"));
        assert!(all.contains("openai/gpt-4o"));

        // A model with no `limit.context` (e.g. a future models.dev entry
        // missing the `limit` key entirely, per `RawModel`'s
        // `#[serde(default)]`) renders `context=?` rather than a misleading
        // `context=0k`.
        let json = r#"{
            "noctx": {
                "id": "noctx", "name": "No Context Provider", "env": [],
                "models": {
                    "mystery": { "id": "mystery", "name": "Mystery Model" }
                }
            }
        }"#;
        otto_llm::registry::install(otto_llm::models_dev::Registry::from_json(json).unwrap());
        let mut buf = Vec::new();
        render_models(Some("noctx"), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("context=?"), "expected context=?, got: {s}");
        assert!(!s.contains("context=0k"));
    }
}
