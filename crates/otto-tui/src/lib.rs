//! otto TUI — a ratatui terminal client for `otto serve`.

pub mod client;
pub mod input;
pub mod narration;
pub mod render;
pub mod spawn;
pub mod splash;
pub mod sse;
pub mod state;
pub mod theme;
pub mod view;

use std::io::{self, IsTerminal, Stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use base64::Engine;
use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::client::Client;
use crate::spawn::LocalServer;
use crate::state::{App, LoopAction, Msg, Overlay};

/// Whether we successfully pushed Kitty keyboard-enhancement flags at startup.
/// Gates the conditional pops so unsupported terminals (and the panic/suspend
/// seams) never emit a stray pop.
static KBD_ENHANCED: AtomicBool = AtomicBool::new(false);

#[must_use]
fn should_pop_kbd() -> bool {
    KBD_ENHANCED.load(Ordering::SeqCst)
}

/// Options for [`run`].
#[derive(Debug, Clone)]
pub struct TuiOptions {
    pub server: Option<String>,
    pub password: Option<String>,
    pub cwd: PathBuf,
    /// Suppress the startup splash (also honours `otto_NO_SPLASH`).
    pub no_splash: bool,
}

/// Headless test seam: apply a sequence of [`Msg`]s through [`App::update`] in
/// order.
pub fn drive(app: &mut App, msgs: Vec<Msg>) {
    for msg in msgs {
        app.update(msg);
    }
}

/// Run the TUI to completion, restoring the terminal on exit.
///
/// # Errors
/// Propagates terminal, runtime-spawn, and IO failures.
pub async fn run(opts: TuiOptions) -> Result<()> {
    // Attach or auto-spawn.
    let (base, _server): (String, Option<LocalServer>) = match opts.server.clone() {
        Some(url) => (url, None),
        None => {
            let s = spawn::spawn_local_server(&opts.cwd).await?;
            (s.base_url.clone(), Some(s))
        }
    };
    let client = Client::new(base, opts.password.clone());

    let mut app = App::new();
    // Config-driven theme: tolerant load (a missing/broken config must not
    // block startup). NO_COLOR always wins over a configured preset.
    let cfg_theme = otto_config::load(&opts.cwd).ok().and_then(|c| c.theme);
    let no_color = std::env::var_os("NO_COLOR").is_some();
    app.theme = crate::theme::Theme::select_with(no_color, cfg_theme.as_deref());
    crate::render::highlight::select_syntect_theme(cfg_theme.as_deref());
    // Load catalogs + sessions up front (best-effort).
    if let Ok(agents) = client.agents().await {
        app.agents = agents;
    }
    if let Ok(models) = client.models().await {
        app.models = models;
    }
    if let Ok(sessions) = client.sessions().await {
        app.sessions = sessions;
    }
    // Ensure an active session — reopen the most-recently-active one (not
    // `sessions.first()`, which is the OLDEST, since `GET /session` is ordered
    // oldest-created first).
    let session = match crate::client::most_recent_session(&app.sessions) {
        Some(s) => s.clone(),
        None => client.create_session("New session").await?,
    };
    app.session_id = Some(session.id.clone());
    if let Ok(rows) = client.history(&session.id).await {
        app.update(Msg::HistoryLoaded(rows));
    }
    app.status = "ready".into();

    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();

    // Input pump.
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut events = EventStream::new();
            while let Some(Ok(ev)) = events.next().await {
                match ev {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        let _ = tx.send(Msg::Key(k));
                    }
                    Event::Resize(_, _) => {
                        let _ = tx.send(Msg::Resize);
                    }
                    _ => {}
                }
            }
        });
    }
    // Event (permission) pump.
    {
        let tx = tx.clone();
        let client = client.clone();
        tokio::spawn(async move {
            if let Ok(mut stream) = client.events().await {
                while let Some(ev) = stream.next().await {
                    // `permission.mode_changed` is translated straight into the
                    // dedicated `Msg::PermissionModeChanged` here (the one place
                    // `ServerEvent`s become `Msg`s) rather than routed through
                    // `Msg::Event`, so `App::update` can handle it as a plain,
                    // independently-testable top-level variant.
                    let msg = match ev {
                        crate::sse::ServerEvent::PermissionModeChanged { mode, .. } => {
                            Msg::PermissionModeChanged(mode)
                        }
                        other => Msg::Event(other),
                    };
                    let _ = tx.send(msg);
                }
            }
        });
    }
    // Tick pump — drives the header spinner + elapsed-seconds liveness
    // indicator while a prompt streams. 125ms = 8/s; must match
    // `view::TICKS_PER_SEC`.
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(125));
            loop {
                ticker.tick().await;
                if tx.send(Msg::Tick).is_err() {
                    break;
                }
            }
        });
    }

    let mut terminal = enter_terminal()?;

    // Arm the startup splash (an `App::splash` tick countdown drained by the
    // event loop) unless opted out or the terminal can't fit even the banner.
    if splash::should_show_splash(
        opts.no_splash,
        std::env::var_os("otto_NO_SPLASH").is_some(),
        io::stdout().is_terminal(),
    ) && let Ok(size) = terminal.size()
        && splash::splash_variant(size.width, size.height) != splash::SplashVariant::None
    {
        app.splash = Some(splash::SPLASH_TICKS);
    }

    // If we panic mid-loop/mid-draw, restore the terminal (disable raw mode,
    // leave the alt screen) before the default panic handler prints, so the
    // user isn't left with a wrecked terminal.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        if should_pop_kbd() {
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::event::PopKeyboardEnhancementFlags
            );
        }
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        prev_hook(info);
    }));

    let result = event_loop(&mut terminal, &mut app, &client, &tx, &mut rx).await;
    leave_terminal(&mut terminal)?;
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    client: &Client,
    tx: &mpsc::UnboundedSender<Msg>,
    rx: &mut mpsc::UnboundedReceiver<Msg>,
) -> Result<()> {
    terminal.draw(|f| view::view(app, f))?;
    while let Some(msg) = rx.recv().await {
        let is_tick = matches!(msg, Msg::Tick);
        let had_flash = app.flash.is_some();
        let had_splash = app.splash.is_some();
        route_message(app, client, tx, msg);
        if let Some(action) = app.pending_action.take() {
            match action {
                LoopAction::Yank => {
                    // OSC-52 write goes straight to stdout, which the loop owns.
                    // `last_assistant_text` borrows `app` immutably and returns
                    // `&str`; copy to an owned `String` first so the borrow ends
                    // before the subsequent `app.flash(...)` call.
                    let text = app.last_assistant_text().map(str::to_string);
                    if let Some(text) = text
                        && osc52_copy(&text).is_ok()
                    {
                        app.flash("copied");
                    }
                }
                LoopAction::Suspend => {
                    #[cfg(unix)]
                    {
                        leave_terminal(terminal)?;
                        // SAFETY: raise() only sends SIGTSTP to this process; it
                        // is async-signal-safe. Returns when we're foregrounded.
                        unsafe {
                            libc::raise(libc::SIGTSTP);
                        }
                        reenter_terminal(terminal)?;
                        terminal.draw(|f| view::view(app, f))?;
                    }
                }
            }
        }
        if app.should_quit {
            break;
        }
        // Guard the redraw: an idle tick must not repaint, or the terminal
        // burns CPU redrawing an unchanged screen 8x/sec forever. A live flash
        // is an exception — it must keep painting so it can fade, and the
        // tick that finally clears it needs one more repaint to erase it.
        // A splash that just auto-dismissed on a tick needs one repaint to
        // erase itself and reveal the real UI (mirrors the `had_flash` case).
        let splash_cleared = had_splash && app.splash.is_none();
        if !is_tick || app.is_busy() || app.flash.is_some() || had_flash || splash_cleared {
            terminal.draw(|f| view::view(app, f))?;
        }
    }
    Ok(())
}

/// Route one incoming message. Key events flow through `on_key` (and trigger a
/// one-shot `/file/list` fetch when the picker just opened); **every other
/// message — including ones injected onto the channel by spawned tasks (e.g. the
/// `Msg::SwitchSession` a `create_session` task sends on ctrl+n) — flows through
/// `dispatch`**, which performs the side effects (`SwitchSession` adopts the id
/// and loads history). Routing non-key messages to `App::update` instead drops
/// them, since `update`'s `SwitchSession`/`NewSession` arms are no-ops.
///
/// Extracted from `event_loop` so the routing decision is unit-testable.
pub fn route_message(app: &mut App, client: &Client, tx: &mpsc::UnboundedSender<Msg>, msg: Msg) {
    match msg {
        Msg::Key(k) => {
            let was_loading_files = matches!(app.overlay, Overlay::Files(ref s) if s.loading);
            let was_sessions = matches!(app.overlay, Overlay::Sessions);
            let next = app.on_key(k);
            let now_loading_files = matches!(app.overlay, Overlay::Files(ref s) if s.loading);
            if now_loading_files && !was_loading_files {
                let client = client.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Ok((files, truncated)) = client.list_files(1000).await {
                        let _ = tx.send(Msg::FilesLoaded(files, truncated));
                    }
                });
            }
            // Refresh the session list when the picker just opened, so
            // auto-generated titles show without restarting the TUI.
            if matches!(app.overlay, Overlay::Sessions) && !was_sessions {
                let client = client.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Ok(sessions) = client.sessions().await {
                        let _ = tx.send(Msg::SessionsLoaded(sessions));
                    }
                });
            }
            if let Some(next) = next {
                dispatch(app, client, tx, next);
            }
        }
        other => dispatch(app, client, tx, other),
    }
}

/// Apply a `Msg` that may kick off async work (prompt streaming, session load).
fn dispatch(app: &mut App, client: &Client, tx: &mpsc::UnboundedSender<Msg>, msg: Msg) {
    if let Msg::SwitchSession(id) = &msg {
        let id = id.clone();
        app.session_id = Some(id.clone());
        app.transcript.clear();
        app.status = "loading…".into();
        let client = client.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Ok(rows) = client.history(&id).await {
                let _ = tx.send(Msg::HistoryLoaded(rows));
            }
        });
        return;
    }
    if let Msg::NewSession = &msg {
        let client = client.clone();
        let tx = tx.clone();
        app.status = "new session…".into();
        tokio::spawn(async move {
            match client.create_session("New session").await {
                Ok(s) => {
                    let _ = tx.send(Msg::SwitchSession(s.id));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(e.to_string()));
                }
            }
        });
        return;
    }
    if let Msg::PermissionReply { id, reply } = &msg {
        let id = id.clone();
        let reply = reply.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let _ = client.reply_permission(&id, &reply, None).await;
        });
        return;
    }
    if let Msg::CyclePermissionMode = &msg {
        // compute the next mode from the current one
        let next = match app.permission_mode.as_str() {
            "approve-each" => "accept-edits",
            "accept-edits" => "full-auto",
            _ => "approve-each",
        };
        app.permission_mode = next.to_string(); // optimistic; server confirms via SSE
        if let Some(id) = app.session_id.clone() {
            let client = client.clone();
            let next = next.to_string();
            tokio::spawn(async move {
                let _ = client.set_permission_mode(&id, &next).await;
            });
        }
        return;
    }
    if let Msg::Submitted(text) = &msg {
        let text = text.clone();
        app.update(Msg::Submitted(text.clone())); // user echo + reset open blocks
        let files = std::mem::take(&mut app.attachments);
        if let Some(id) = app.session_id.clone() {
            let client = client.clone();
            let tx = tx.clone();
            let agent = app.agent.clone();
            let model = app.model.clone();
            app.status = "…thinking".into();
            tokio::spawn(async move {
                match client
                    .prompt(&id, &text, agent.as_deref(), model.as_deref(), &files)
                    .await
                {
                    Ok(mut s) => {
                        while let Some(ev) = s.next().await {
                            let _ = tx.send(Msg::Server(ev));
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Error(e.to_string()));
                    }
                }
                // Stream closed — unstick "…thinking" if nothing terminal did.
                let _ = tx.send(Msg::PromptEnded);
            });
        }
        return;
    }
    if let Msg::StartWorkflow { kind, arg } = &msg {
        let kind = kind.clone();
        let arg = arg.clone();
        // Let App::update fold its (currently no-op; Task 5 = a launch line) state.
        app.update(Msg::StartWorkflow {
            kind: kind.clone(),
            arg: arg.clone(),
        });
        let client = client.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = client.workflow(&kind, &arg).await {
                let _ = tx.send(Msg::Error(format!("workflow {kind} failed to start: {e}")));
            }
            // Progress + completion arrive on the /event pump (already running).
        });
        return;
    }
    if let Msg::CancelWorkflow(session) = &msg {
        let session = session.clone();
        let client = client.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = client.cancel_workflow(&session).await {
                let _ = tx.send(Msg::Error(format!("cancel failed: {e}")));
            }
            // The cancel surfaces as a `workflow.done{ok:false}` on the /event pump.
        });
        return;
    }
    if let Msg::InterruptTurn(session) = &msg {
        let session = session.clone();
        let client = client.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = client.cancel_run(&session).await {
                let _ = tx.send(Msg::Error(format!("interrupt failed: {e}")));
            }
            // The abort settles the run; the stream closes and busy clears.
        });
        return;
    }
    app.update(msg);
}

fn enter_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    // No `EnableMouseCapture`: capturing the mouse steals the terminal's
    // native drag-select, which is how users copy transcript/code text.
    // Wheel-scroll is not worth that trade-off — keyboard PageUp/PageDown/End
    // (input.rs) cover scrolling instead.
    execute!(out, EnterAlternateScreen)?;
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
        if execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok()
        {
            KBD_ENHANCED.store(true, Ordering::SeqCst);
        }
    }
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn leave_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    if should_pop_kbd() {
        use crossterm::event::PopKeyboardEnhancementFlags;
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Re-enter the alt screen after a [`LoopAction::Suspend`] resumes in the
/// foreground. Mirrors [`enter_terminal`]'s body but reuses the existing
/// `Terminal`/backend rather than constructing a new one.
#[cfg(unix)]
fn reenter_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    if should_pop_kbd() {
        use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
        let _ = execute!(
            terminal.backend_mut(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    terminal.clear()?;
    Ok(())
}

/// Build (but do not write) the OSC-52 "set clipboard" escape sequence for
/// `s`. Split out from [`osc52_copy`] so the exact byte format is unit
/// testable without capturing stdout.
///
/// Out-of-band: the terminal intercepts this sequence directly (no visible
/// effect in the alt screen). Works in iTerm2, kitty, wezterm, and xterm
/// directly; under tmux it additionally needs DCS passthrough wrapping,
/// which is out of scope here.
fn osc52_sequence(s: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

/// Write the OSC-52 yank sequence for `s` to stdout and flush.
fn osc52_copy(s: &str) -> io::Result<()> {
    use std::io::Write;
    let mut out = io::stdout();
    write!(out, "{}", osc52_sequence(s))?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_sequence_encodes_rfc4648_vectors() {
        // Standard base64 test vectors, wrapped in the OSC-52 envelope.
        assert_eq!(osc52_sequence(""), "\x1b]52;c;\x07");
        assert_eq!(osc52_sequence("f"), "\x1b]52;c;Zg==\x07");
        assert_eq!(osc52_sequence("fo"), "\x1b]52;c;Zm8=\x07");
        assert_eq!(osc52_sequence("foo"), "\x1b]52;c;Zm9v\x07");
        assert_eq!(osc52_sequence("foob"), "\x1b]52;c;Zm9vYg==\x07");
    }

    #[test]
    fn osc52_sequence_format_is_escape_bracket_52_c_payload_bell() {
        let seq = osc52_sequence("hello");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
    }
}

#[cfg(test)]
mod kbd_tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    // `KBD_ENHANCED` is a process-global static. `cargo test` runs tests in
    // parallel by default, so without serializing access the two tests below
    // race and intermittently clobber each other's assertion. This lock is
    // test-only scaffolding, not part of the production gate.
    static KBD_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn kbd_gate_defaults_false_and_pop_is_noop() {
        let _guard = KBD_TEST_LOCK.lock().unwrap();
        // Fresh process: the flag starts false, so should_pop() is false and
        // the pop seams do nothing.
        KBD_ENHANCED.store(false, Ordering::SeqCst);
        assert!(!should_pop_kbd());
    }

    #[test]
    fn kbd_gate_true_after_set() {
        let _guard = KBD_TEST_LOCK.lock().unwrap();
        KBD_ENHANCED.store(true, Ordering::SeqCst);
        assert!(should_pop_kbd());
        KBD_ENHANCED.store(false, Ordering::SeqCst); // restore for other tests
    }
}
