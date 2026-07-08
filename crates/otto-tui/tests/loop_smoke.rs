//! End-to-end piece-integration test for the event-loop pipeline: a scripted
//! server, the real HTTP client, and the `App`/`Msg` fold — no real terminal.

mod harness;

use futures::StreamExt;
use harness::{GuardTool, ScriptedRouteFactory, no_auth, spawn, text_turn};
use otto_tui::client::Client;
use otto_tui::sse::ServerEvent;
use otto_tui::state::{App, Msg, Overlay, TranscriptItem};

#[tokio::test]
async fn prompt_pipeline_folds_into_transcript() {
    let (factory, _) = ScriptedRouteFactory::new(vec![text_turn("t1", "hello from otto")]);
    let runtime = std::sync::Arc::new(
        otto_app::Runtime::in_memory(otto_config::Config::default())
            .await
            .unwrap()
            .with_route_factory(factory),
    );
    let base = spawn(runtime, no_auth()).await;
    let client = Client::new(base, None);
    let ses = client.create_session("Chat").await.unwrap();

    let mut app = App::new();
    app.session_id = Some(ses.id.clone());
    app.update(Msg::Submitted("hi".into())); // user echo, as the loop does on submit

    let mut stream = client.prompt(&ses.id, "hi", None, None, &[]).await.unwrap();
    while let Some(ev) = stream.next().await {
        app.update(Msg::Server(ev));
    }

    let assistant = app.transcript.iter().find_map(|i| match i {
        TranscriptItem::Assistant(s) => Some(s.clone()),
        _ => None,
    });
    assert_eq!(assistant.as_deref(), Some("hello from otto"));
    assert!(matches!(app.transcript.first(), Some(TranscriptItem::User(s)) if s == "hi"));
    assert_eq!(
        app.transcript
            .iter()
            .filter(|i| matches!(i, TranscriptItem::User(_)))
            .count(),
        1,
        "expected exactly one User transcript item (no double echo)"
    );
}

/// Key half of the permission-reply routing bug: pressing `y` while a
/// `Overlay::Permission` is open must produce `Msg::PermissionReply` (and
/// close the overlay) — this is the message `event_loop`'s `Msg::Key` arm
/// hands to `dispatch`.
#[test]
fn on_key_permission_reply_produces_msg() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use otto_tui::sse::PermissionAsked;

    let mut app = App::new();
    app.overlay = Overlay::Permission(PermissionAsked {
        id: "p1".into(),
        session_id: "s1".into(),
        permission: "danger".into(),
        patterns: vec![],
    });

    let msg = app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    match msg {
        Some(Msg::PermissionReply { id, reply }) => {
            assert_eq!(id, "p1");
            assert_eq!(reply, "once");
        }
        other => panic!("expected Some(Msg::PermissionReply {{ .. }}), got {other:?}"),
    }
    assert!(
        matches!(app.overlay, Overlay::None),
        "overlay must close once the reply intent is produced"
    );
}

/// Reproduces the dead-wiring bug end to end at the HTTP layer: drives the
/// exact `Client::reply_permission` call that `dispatch` now performs for
/// `Msg::PermissionReply`, and asserts it actually unblocks the agent turn
/// that is parked on the permission ask. Before the `dispatch` fix, nothing
/// ever sent `Msg::PermissionReply` onto `rx`, so the server-side call this
/// test makes directly is precisely the call that used to never happen.
#[tokio::test]
async fn permission_reply_unblocks_agent() {
    // Turn 1 calls the guard tool (asks "danger"); turn 2 finishes after grant.
    let mut turn1 = vec![harness::step_start()];
    turn1.push(harness::tool_call("call_1", "guard", serde_json::json!({})));
    turn1.push(harness::step_finish(otto_events::FinishReason::ToolCalls));
    turn1.push(harness::finish(otto_events::FinishReason::ToolCalls));
    let (factory, _) = ScriptedRouteFactory::new(vec![turn1, text_turn("t2", "done")]);

    let config = otto_config::Config {
        permission: Some(serde_json::json!({ "danger": "ask" })),
        ..otto_config::Config::default()
    };
    let mut registry = otto_tools::ToolRegistry::new();
    registry.register(std::sync::Arc::new(GuardTool));
    let runtime = std::sync::Arc::new(
        otto_app::Runtime::in_memory(config)
            .await
            .unwrap()
            .with_route_factory(factory)
            .with_tools(std::sync::Arc::new(registry)),
    );
    let base = spawn(runtime, no_auth()).await;
    let client = Client::new(base, None);
    let ses = client.create_session("Perm").await.unwrap();

    // Subscribe to /event before prompting, exactly like the loop's event pump.
    let mut events = client.events().await.unwrap();

    // Fire the prompt in the background; it blocks on the permission ask.
    let prompt_client = client.clone();
    let ses_id = ses.id.clone();
    let prompt_task = tokio::spawn(async move {
        let mut s = prompt_client
            .prompt(&ses_id, "do danger", None, None, &[])
            .await
            .unwrap();
        let mut text = String::new();
        while let Some(ev) = s.next().await {
            if let otto_events::LLMEvent::TextDelta { text: t, .. } = ev {
                text.push_str(&t);
            }
        }
        text
    });

    // Wait for the permission.asked event.
    let asked = loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), events.next()).await {
            Ok(Some(ServerEvent::PermissionAsked(p))) => break p,
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => panic!("no permission.asked within 5s"),
        }
    };
    assert_eq!(asked.permission, "danger");

    // This is precisely what the fixed `dispatch` does for `Msg::PermissionReply`.
    client
        .reply_permission(&asked.id, "once", None)
        .await
        .unwrap();

    let text = tokio::time::timeout(std::time::Duration::from_secs(5), prompt_task)
        .await
        .expect("prompt task timed out — permission reply did not unblock the agent")
        .unwrap();
    assert_eq!(text, "done");
}

/// Drives the REAL routing path (rx -> route_message -> on_key -> field), not
/// `on_key` in isolation — this is the wiring the old test missed. `y` on an
/// empty input must set `App.pending_action`, not return a dead `Msg::YankLast`
/// that `route_message`/`App::update` silently swallow.
#[test]
fn y_key_sets_pending_yank_through_routing() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use otto_tui::state::LoopAction;

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let client = Client::new("http://127.0.0.1:0", None);
    let mut app = App::new();
    app.session_id = Some("ses_1".into());
    app.transcript
        .push(TranscriptItem::Assistant("hello world".into()));
    // input is empty by default
    otto_tui::route_message(
        &mut app,
        &client,
        &tx,
        Msg::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
    );
    assert_eq!(app.pending_action, Some(LoopAction::Yank));
}

/// Regression for the new-session (ctrl+n) bug: the flow's `Msg::SwitchSession`
/// arrives on the channel from the spawned `create_session` task — i.e. as a
/// NON-key message. `event_loop` must route non-key messages through `dispatch`
/// (which adopts the session id + kicks off history load), NOT through
/// `App::update`, where `Msg::SwitchSession` is a no-op. Before the fix, ctrl+n
/// left the app stuck on "new session…" forever because the created session's
/// id was silently dropped.
#[tokio::test]
async fn channel_switch_session_is_adopted_not_dropped() {
    let (factory, _) = ScriptedRouteFactory::new(vec![text_turn("t1", "hi")]);
    let runtime = std::sync::Arc::new(
        otto_app::Runtime::in_memory(otto_config::Config::default())
            .await
            .unwrap()
            .with_route_factory(factory),
    );
    let base = spawn(runtime, no_auth()).await;
    let client = Client::new(base, None);
    let ses = client.create_session("New").await.unwrap();

    let mut app = App::new(); // session_id is None, as right after a NewSession
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();

    // A SwitchSession delivered as a non-key channel message — exactly what the
    // spawned create_session task sends on ctrl+n.
    otto_tui::route_message(&mut app, &client, &tx, Msg::SwitchSession(ses.id.clone()));

    assert_eq!(
        app.session_id.as_deref(),
        Some(ses.id.as_str()),
        "channel-delivered SwitchSession must adopt the new session id"
    );
    assert!(
        app.status.contains("loading"),
        "status should advance to loading, not stay stuck busy; got {:?}",
        app.status
    );
}
