//! otto TUI — a ratatui terminal client for `otto serve`.

pub mod appearance;
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
use crate::state::{App, DashboardPeek, LoopAction, Msg, Overlay};

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
    app.color_depth = crate::appearance::detect_color_depth();

    let mut ssh_detected = false;
    if cfg_theme
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case("auto"))
        && !no_color
    {
        app.dark_theme = crate::appearance::quantize(&crate::theme::Theme::dark(), app.color_depth);
        app.light_theme =
            crate::appearance::quantize(&crate::theme::Theme::preset("light"), app.color_depth);

        let mode = match crate::appearance::os_theme::detect_os_theme().await {
            Some(m) => m,
            None if crate::appearance::os_theme::is_ssh_session() => {
                ssh_detected = true;
                crate::appearance::os_theme::detect_os_theme_ssh()
                    .await
                    .unwrap_or(crate::appearance::ThemeMode::Dark)
            }
            None => crate::appearance::ThemeMode::Dark,
        };
        app.theme_mode = Some(mode);
        app.theme = match mode {
            crate::appearance::ThemeMode::Light => app.light_theme.clone(),
            crate::appearance::ThemeMode::Dark => app.dark_theme.clone(),
        };
    } else {
        let selected = crate::theme::Theme::select_with(no_color, cfg_theme.as_deref());
        app.theme = crate::appearance::quantize(&selected, app.color_depth);
    }
    let should_poll = should_poll_os_theme(cfg_theme.as_deref(), no_color, ssh_detected);
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
                    Event::FocusGained => {
                        let _ = tx.send(Msg::FocusChanged(true));
                    }
                    Event::FocusLost => {
                        let _ = tx.send(Msg::FocusChanged(false));
                    }
                    _ => {}
                }
            }
        });
    }
    // Event (permission) pump. Reconnects with backoff: the stream ending (or
    // failing to open) would otherwise silently kill permission prompts and
    // workflow progress for the rest of the session.
    {
        let tx = tx.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let mut attempt: u32 = 0;
            loop {
                if let Ok(mut stream) = client.events().await {
                    attempt = 0;
                    while let Some(ev) = stream.next().await {
                        // `permission.mode_changed` is translated straight into
                        // the dedicated `Msg::PermissionModeChanged` here (the
                        // one place `ServerEvent`s become `Msg`s) rather than
                        // routed through `Msg::Event`, so `App::update` can
                        // handle it as a plain, independently-testable
                        // top-level variant.
                        let msg = match ev {
                            crate::sse::ServerEvent::PermissionModeChanged { mode, .. } => {
                                Msg::PermissionModeChanged(mode)
                            }
                            other => Msg::Event(other),
                        };
                        if tx.send(msg).is_err() {
                            return; // UI gone — stop for good
                        }
                    }
                }
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(event_reconnect_delay(attempt)).await;
                if tx.is_closed() {
                    return;
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
    // OS-appearance live-poll pump — only spawned in `theme = "auto"` mode
    // with a live-pollable detection method (see `should_poll_os_theme`).
    if should_poll {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let mode = crate::appearance::os_theme::detect_os_theme().await;
                if tx.send(Msg::OsThemeChanged(mode)).is_err() {
                    break;
                }
            }
        });
    }

    let mut terminal = enter_terminal()?;
    if let Some(hex) = app.theme.accent_hex() {
        let _ = set_cursor_color(&hex);
    }

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
                LoopAction::Notify => {
                    let _ = notify_turn_finished();
                }
                LoopAction::ResetTitle => {
                    let _ = reset_title();
                }
                LoopAction::CursorColor => {
                    if let Some(hex) = app.theme.accent_hex() {
                        let _ = set_cursor_color(&hex);
                    } else {
                        let _ = reset_cursor_color();
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
            // Fires for the ctrl+f picker AND the `@`-mention pickers (chat +
            // text-input), all of which flag `loading` when they open.
            let was_loading_files = app.file_fetch_pending();
            let was_sessions = matches!(app.overlay, Overlay::Sessions);
            let was_dashboard = matches!(app.overlay, Overlay::Dashboard);
            let dashboard_selected_before = app.dashboard.selected;
            let next = app.on_key(k);
            let now_loading_files = app.file_fetch_pending();
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
            // Fetch once immediately on opening the dashboard.
            if matches!(app.overlay, Overlay::Dashboard) && !was_dashboard {
                let client = client.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Some(msg) = fetch_dashboard(&client).await {
                        let _ = tx.send(msg);
                    }
                });
            }
            // A row-selection change (arrow keys) needs its own peek fetch
            // for idle/busy rows — `dashboard_move` already set
            // `app.dashboard.peek` to `Loading` synchronously (Task 6);
            // this spawns the async fetch that resolves it.
            if matches!(app.overlay, Overlay::Dashboard)
                && app.dashboard.selected != dashboard_selected_before
                && matches!(app.dashboard.peek, DashboardPeek::Loading)
                && let Some(row) = app.dashboard.rows.get(app.dashboard.selected)
            {
                let sid = row.session.id.clone();
                let client = client.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Ok(rows) = client.history(&sid).await {
                        let text = crate::state::latest_message_text(&rows);
                        let _ = tx.send(Msg::DashboardPeekLoaded {
                            session_id: sid,
                            text,
                        });
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
        // Adopting a different session: the Σ session-token totals belong to
        // the old one. (A same-session history reconcile goes through
        // HistoryLoaded directly and keeps them.)
        app.reset_session_counters();
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
        app.update(Msg::PermissionReply {
            id: id.clone(),
            reply: reply.clone(),
        });
        let client = client.clone();
        tokio::spawn(async move {
            let _ = client.reply_permission(&id, &reply, None).await;
        });
        return;
    }
    if let Msg::QuestionReply { id, reply } = &msg {
        let id = id.clone();
        let reply = reply.clone();
        app.update(Msg::QuestionReply {
            id: id.clone(),
            reply: reply.clone(),
        });
        let client = client.clone();
        tokio::spawn(async move {
            let _ = client.reply_question(&id, &reply).await;
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
        // ctrl+f attachments + surviving `@`-mention paths (mentions whose
        // token was edited back out are dropped by the substring check).
        let files = app.take_files_for_submit(&text);
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
    if let Msg::PromptEnded = &msg {
        // An errored turn may have dropped deltas the server persisted after
        // the connection broke — reconcile the transcript from history. The
        // stream's ProviderError event is processed before PromptEnded (same
        // FIFO channel), so `app.status` already reflects the failure here.
        if should_reconcile_history(app)
            && let Some(id) = app.session_id.clone()
        {
            let client = client.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Ok(rows) = client.history(&id).await {
                    let _ = tx.send(Msg::HistoryLoaded(rows));
                }
            });
        }
        app.update(msg);
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
        let parent = app.session_id.clone();
        tokio::spawn(async move {
            if let Err(e) = client.workflow(&kind, &arg, parent.as_deref()).await {
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
    if let Msg::Tick = &msg
        && matches!(app.overlay, Overlay::Dashboard)
        && dashboard_poll_due(app.tick)
    {
        let client = client.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Some(msg) = fetch_dashboard(&client).await {
                let _ = tx.send(msg);
            }
        });
    }
    app.update(msg);
}

/// How often (in ticks of the existing 125ms tick) the dashboard polls
/// `GET /session`/`GET /permission`/`GET /question` while open: 16 ticks =
/// 2s.
const DASHBOARD_POLL_TICKS: u32 = 16;

/// Whether a dashboard poll is due on this tick.
fn dashboard_poll_due(tick: u32) -> bool {
    tick.is_multiple_of(DASHBOARD_POLL_TICKS)
}

/// Fetch the three dashboard endpoints and fold them into a `DashboardLoaded`
/// message, or `None` if any of the three fails. Per the design spec's
/// error-handling section, a poll failure must keep the last-known
/// `DashboardState` on screen rather than clearing it — so a partial
/// failure here is deliberately all-or-nothing (never sends a message built
/// from only the endpoints that happened to succeed), leaving
/// `app.dashboard.rows` untouched until the next successful poll.
async fn fetch_dashboard(client: &Client) -> Option<Msg> {
    let sessions = client.sessions().await.ok()?;
    let permissions = client.permission_list().await.ok()?;
    let questions = client.question_list().await.ok()?;
    Some(Msg::DashboardLoaded {
        sessions,
        permissions,
        questions,
    })
}

/// Backoff before `/event` reconnect attempt `attempt` (1-based):
/// 1s, 2s, 4s, 8s, then capped at 15s.
fn event_reconnect_delay(attempt: u32) -> std::time::Duration {
    let secs = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX)
        .min(15);
    std::time::Duration::from_secs(secs)
}

/// Whether a just-ended turn warrants a history refetch: a turn that ended in
/// a transport/provider error may have dropped deltas the server persisted
/// after the connection broke — reloading history reconciles the transcript.
fn should_reconcile_history(app: &App) -> bool {
    app.session_id.is_some() && app.status.starts_with("error:")
}

fn enter_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    // No `EnableMouseCapture`: capturing the mouse steals the terminal's
    // native drag-select, which is how users copy transcript/code text.
    // Wheel-scroll is not worth that trade-off — keyboard PageUp/PageDown/End
    // (input.rs) cover scrolling instead.
    execute!(
        out,
        EnterAlternateScreen,
        crossterm::event::EnableFocusChange
    )?;
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
    let _ = reset_cursor_color();
    let _ = reset_title();
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableFocusChange,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Re-enter the alt screen after a [`LoopAction::Suspend`] resumes in the
/// foreground. Mirrors [`enter_terminal`]'s body but reuses the existing
/// `Terminal`/backend rather than constructing a new one.
#[cfg(unix)]
fn reenter_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        crossterm::event::EnableFocusChange
    )?;
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

/// Build the OSC 9 "system notification" escape sequence. Broadly supported
/// (iTerm2, WezTerm, Warp, Ghostty); terminals that don't recognize OSC 9
/// safely ignore it.
fn notify_sequence(text: &str) -> String {
    format!("\x1b]9;{text}\x07")
}

/// Build a terminal-title-set (OSC 0) escape sequence.
fn title_sequence(text: &str) -> String {
    format!("\x1b]0;{text}\x07")
}

/// Write the turn-finished OS notification and set a "done" terminal title.
fn notify_turn_finished() -> io::Result<()> {
    use std::io::Write;
    let mut out = io::stdout();
    write!(
        out,
        "{}{}",
        notify_sequence("otto: turn finished"),
        title_sequence("otto — done")
    )?;
    out.flush()
}

/// Reset the terminal title to the plain `otto` title.
fn reset_title() -> io::Result<()> {
    use std::io::Write;
    let mut out = io::stdout();
    write!(out, "{}", title_sequence("otto"))?;
    out.flush()
}

/// Build the OSC 12 "set cursor color" escape sequence for an uppercase
/// 6-digit hex string (no `#`), e.g. `"88C0D0"`.
fn cursor_color_sequence(hex: &str) -> String {
    format!("\x1b]12;#{hex}\x07")
}

/// Reset the terminal cursor color to its default (OSC 112).
const CURSOR_COLOR_RESET_SEQUENCE: &str = "\x1b]112\x07";

/// Set the terminal cursor color (OSC 12) to `hex` (e.g. `"88C0D0"`, no `#`).
fn set_cursor_color(hex: &str) -> io::Result<()> {
    use std::io::Write;
    let mut out = io::stdout();
    write!(out, "{}", cursor_color_sequence(hex))?;
    out.flush()
}

/// Reset the terminal cursor color to its default.
fn reset_cursor_color() -> io::Result<()> {
    use std::io::Write;
    let mut out = io::stdout();
    write!(out, "{CURSOR_COLOR_RESET_SEQUENCE}")?;
    out.flush()
}

/// Whether `run()` should spawn the live OS-appearance poll pump: only in
/// `theme = "auto"` mode, only when `NO_COLOR` isn't forcing `mono()`
/// regardless of detection, and only when startup detection did NOT fall
/// back to the SSH one-shot path (a live re-poll there would race the
/// `crossterm::EventStream` read).
fn should_poll_os_theme(cfg_theme: Option<&str>, no_color: bool, ssh_detected: bool) -> bool {
    cfg_theme.is_some_and(|t| t.eq_ignore_ascii_case("auto")) && !no_color && !ssh_detected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_reconnect_delay_backs_off_and_caps() {
        use std::time::Duration;
        assert_eq!(event_reconnect_delay(1), Duration::from_secs(1));
        assert_eq!(event_reconnect_delay(2), Duration::from_secs(2));
        assert_eq!(event_reconnect_delay(3), Duration::from_secs(4));
        assert_eq!(event_reconnect_delay(5), Duration::from_secs(15), "capped");
        assert_eq!(
            event_reconnect_delay(50),
            Duration::from_secs(15),
            "still capped"
        );
    }

    #[test]
    fn reconcile_only_after_errored_turn() {
        let mut app = App::new();
        app.session_id = Some("ses_1".into());
        app.status = "ready".into();
        assert!(
            !should_reconcile_history(&app),
            "clean turn end needs no history refetch"
        );
        app.status = "error: lost connection to otto server: timed out".into();
        assert!(
            should_reconcile_history(&app),
            "errored turn may have dropped persisted deltas — refetch"
        );
        app.session_id = None;
        assert!(
            !should_reconcile_history(&app),
            "no session — nothing to refetch"
        );
    }

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

    #[test]
    fn notify_sequence_is_osc9() {
        let seq = notify_sequence("otto: turn finished");
        assert_eq!(seq, "\x1b]9;otto: turn finished\x07");
    }

    #[test]
    fn title_sequence_is_osc0() {
        let seq = title_sequence("otto — done");
        assert_eq!(seq, "\x1b]0;otto — done\x07");
    }

    #[test]
    fn cursor_color_sequence_is_osc12() {
        assert_eq!(cursor_color_sequence("88C0D0"), "\x1b]12;#88C0D0\x07");
    }

    #[test]
    fn cursor_color_reset_sequence_is_osc112() {
        assert_eq!(CURSOR_COLOR_RESET_SEQUENCE, "\x1b]112\x07");
    }

    #[test]
    fn dashboard_poll_due_every_16_ticks() {
        assert!(dashboard_poll_due(0));
        assert!(!dashboard_poll_due(1));
        assert!(!dashboard_poll_due(15));
        assert!(dashboard_poll_due(16));
        assert!(dashboard_poll_due(32));
    }

    #[test]
    fn should_poll_os_theme_only_when_auto_and_not_ssh_detected() {
        assert!(should_poll_os_theme(Some("auto"), false, false));
        assert!(
            !should_poll_os_theme(Some("auto"), false, true),
            "ssh one-shot only"
        );
        assert!(
            !should_poll_os_theme(Some("nord"), false, false),
            "not auto mode"
        );
        assert!(
            !should_poll_os_theme(Some("auto"), true, false),
            "NO_COLOR wins"
        );
        assert!(!should_poll_os_theme(None, false, false));
        assert!(
            should_poll_os_theme(Some("Auto"), false, false),
            "case-insensitive"
        );
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
