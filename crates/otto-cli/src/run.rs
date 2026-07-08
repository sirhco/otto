//! The `run` command: drive a single agent turn and render it to the terminal.
//!
//! The provider-facing flow is factored into [`run_session`], which is
//! deliberately decoupled from any real terminal or provider: the output sink
//! is an injectable [`Write`], permission decisions come from an injectable
//! [`PermissionResponder`], and the model route comes from whatever
//! [`RouteFactory`](otto_app::RouteFactory) the [`Runtime`] was built with.
//! Tests drive it headless with [`Runtime::in_memory`], a scripted route
//! factory, and a scripted responder; the `otto run` CLI wires the same
//! function to real stdout, a TTY prompt, and a Ctrl-C cancellation token.

use std::io::{IsTerminal, Read, Write};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use otto_agent::{AgentInfo, ModelRef};
use otto_app::{RunHandle, Runtime};
use otto_permission::{Asked, Reply};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

use crate::cli::RunArgs;
use crate::render::{Renderer, color_enabled};

/// Decides how to answer an interactive permission [`Asked`] request.
///
/// The CLI implementation ([`TtyResponder`]) prompts the user on a TTY (or
/// applies a non-interactive policy); tests inject a scripted responder so the
/// permission path is exercised with no real terminal.
pub trait PermissionResponder: Send + Sync {
    /// Answer a permission request.
    fn respond(&self, asked: &Asked) -> Reply;
}

/// Everything [`run_session`] needs to drive one turn.
pub struct RunRequest {
    /// The user prompt to send.
    pub prompt: String,
    /// The resolved agent to run as.
    pub agent: AgentInfo,
    /// The resolved model to generate with.
    pub model: ModelRef,
    /// An existing session to continue, or `None` to create a fresh one.
    pub session_id: Option<String>,
}

/// Drive a single agent turn on `runtime`, rendering the streamed events to
/// `out`.
///
/// Creates (or continues) a session, spawns a background task that answers
/// permission asks via `responder`, kicks off [`Runtime::run`], renders the
/// live [`LLMEvent`](otto_events::LLMEvent) stream with a [`Renderer`], and
/// awaits the final assistant message. `abort` cancels the run (wire it to
/// Ctrl-C); it is also used to tear the permission task down once the turn
/// ends.
///
/// # Errors
/// Returns an error if the session cannot be created/found, if the run task
/// fails ([`RunError`](otto_app::RunError)) or panics, or if writing to `out`
/// fails. A run error makes the process exit non-zero.
pub async fn run_session<W: Write + Send>(
    runtime: &Runtime,
    req: RunRequest,
    out: W,
    color: bool,
    responder: Arc<dyn PermissionResponder>,
    abort: CancellationToken,
) -> Result<()> {
    let RunRequest {
        prompt,
        agent,
        model,
        session_id,
    } = req;

    // Resolve the target session up front so its id is stable for the run.
    let session_id = match session_id {
        Some(id) => id,
        None => runtime
            .create_session(title_from(&prompt), &agent, None)
            .await
            .context("failed to create session")?,
    };

    // Answer permission asks off-thread. Subscribe *before* the run so no ask
    // is missed. Each ask is resolved on a blocking thread (the TTY responder
    // reads stdin), so asks are handled one at a time.
    let permission = runtime.permission().clone();
    let mut asks = permission.subscribe();
    let responder = responder.clone();
    let perm_abort = abort.clone();
    let perm_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = perm_abort.cancelled() => break,
                received = asks.recv() => match received {
                    Ok(asked) => {
                        let request_id = asked.request_id.clone();
                        let responder = responder.clone();
                        let reply = tokio::task::spawn_blocking(move || responder.respond(&asked))
                            .await
                            .unwrap_or(Reply::Reject { message: None });
                        permission.reply(&request_id, reply);
                    }
                    Err(RecvError::Closed) => break,
                    Err(RecvError::Lagged(_)) => continue,
                },
            }
        }
    });

    // Drive the turn and render the live event stream.
    let RunHandle { mut events, join } =
        runtime.run(&session_id, prompt, &agent, &model, abort.clone());
    let mut renderer = Renderer::new(out, color);
    while let Some(event) = events.recv().await {
        renderer.handle(&event)?;
    }
    let outcome = join.await;
    renderer.finish()?;

    // The turn is done: tear the permission task down.
    abort.cancel();
    let _ = perm_task.await;

    match outcome {
        Ok(Ok(_info)) => Ok(()),
        Ok(Err(run_err)) => Err(anyhow::anyhow!(run_err.to_string())).context("run failed"),
        Err(join_err) => bail!("run task panicked: {join_err}"),
    }
}

/// A short session title derived from the first line of the prompt.
fn title_from(prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "New Session".to_string();
    }
    let title: String = first.chars().take(50).collect();
    title
}

/// The interactive CLI permission responder.
///
/// On a TTY it prompts `y`/`n`/`a` (allow once / reject / always) on stderr and
/// reads the answer from stdin. When stdin is not a TTY it applies a fixed
/// policy: allow-once if `--yes` was passed, otherwise reject with a message.
pub struct TtyResponder {
    /// Auto-allow non-interactive asks (`--yes`).
    pub yes: bool,
    /// Whether stdin is an interactive terminal.
    pub interactive: bool,
}

impl PermissionResponder for TtyResponder {
    fn respond(&self, asked: &Asked) -> Reply {
        if !self.interactive {
            return if self.yes {
                Reply::Once
            } else {
                Reply::Reject {
                    message: Some(
                        "permission auto-rejected (non-interactive; pass --yes to allow)".into(),
                    ),
                }
            };
        }

        let patterns = if asked.patterns.is_empty() {
            String::new()
        } else {
            format!(" {}", asked.patterns.join(", "))
        };
        eprint!(
            "\npermission: {}{patterns} — allow? [y]es / [n]o / [a]lways: ",
            asked.permission
        );
        let _ = std::io::stderr().flush();

        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return Reply::Reject { message: None };
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "a" | "always" => Reply::Always,
            "y" | "yes" => Reply::Once,
            _ => Reply::Reject {
                message: Some("rejected by user".into()),
            },
        }
    }
}

/// Entry point for `otto run`: load the runtime, resolve the agent/model/
/// prompt/session, wire Ctrl-C to a cancellation token, and call
/// [`run_session`] against real stdout.
///
/// # Errors
/// Propagates runtime-load, resolution, and run failures.
pub async fn cmd_run(cwd: &std::path::Path, args: RunArgs) -> Result<()> {
    let runtime = Runtime::load(cwd).await.context("failed to load runtime")?;

    // Resolve the agent: `--agent` by name, else the runtime default.
    let agent = match &args.agent {
        Some(name) => runtime
            .agents()
            .iter()
            .find(|a| &a.name == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown agent: {name}"))?,
        None => runtime.default_agent().clone(),
    };

    // Resolve the model: `--model provider/model`, else the runtime default.
    let model = match &args.model {
        Some(spec) => ModelRef::parse(spec),
        None => runtime.default_model(),
    };

    // Resolve the prompt: joined message args, else piped stdin.
    let prompt = read_prompt(args.message)?;
    if prompt.trim().is_empty() {
        bail!("empty prompt: pass a message or pipe one on stdin");
    }

    // Resolve the session to continue, if any.
    let session_id = resolve_session(&runtime, args.session, args.continue_).await?;

    // Cancellation: Ctrl-C cancels the run.
    let abort = CancellationToken::new();
    let ctrl_abort = abort.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            ctrl_abort.cancel();
        }
    });

    let interactive = std::io::stdin().is_terminal();
    let responder: Arc<dyn PermissionResponder> = Arc::new(TtyResponder {
        yes: args.yes,
        interactive,
    });

    let color = color_enabled() && std::io::stdout().is_terminal();
    run_session(
        &runtime,
        RunRequest {
            prompt,
            agent,
            model,
            session_id,
        },
        std::io::stdout(),
        color,
        responder,
        abort,
    )
    .await
}

/// The prompt text: joined `message` args, or all of stdin when the args are
/// empty and stdin is piped (non-TTY). A TTY with no args yields an empty
/// string (the caller errors).
fn read_prompt(message: Vec<String>) -> Result<String> {
    if !message.is_empty() {
        return Ok(message.join(" "));
    }
    if !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("failed to read prompt from stdin")?;
        return Ok(buf);
    }
    Ok(String::new())
}

/// Resolve the session to run against: an explicit `--session <id>` (validated
/// to exist), the latest session in this directory for `--continue`, or `None`
/// for a fresh session.
async fn resolve_session(
    runtime: &Runtime,
    session: Option<String>,
    continue_latest: bool,
) -> Result<Option<String>> {
    if let Some(id) = session {
        if runtime.store().get_session(&id).await?.is_none() {
            bail!("session not found: {id}");
        }
        return Ok(Some(id));
    }
    if continue_latest {
        // `list_sessions` is ordered oldest-first; prefer the newest session
        // rooted in this working directory, else the newest overall.
        let sessions = runtime.store().list_sessions().await?;
        let dir = runtime.directory().display().to_string();
        let id = sessions
            .iter()
            .rev()
            .find(|s| s.directory == dir)
            .or_else(|| sessions.last())
            .map(|s| s.id.clone());
        if id.is_none() {
            bail!("no previous session to continue");
        }
        return Ok(id);
    }
    Ok(None)
}
