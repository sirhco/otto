//! Integration tests for `otto_tui::client::Client` against a scripted server.

mod harness;

use harness::{no_auth, plain_runtime, spawn};
use otto_tui::client::Client;

#[tokio::test]
async fn sessions_create_list_roundtrip() {
    let base = spawn(plain_runtime().await, no_auth()).await;
    let client = Client::new(base, None);

    assert!(client.sessions().await.unwrap().is_empty());
    let made = client.create_session("Hello").await.unwrap();
    assert!(made.id.starts_with("ses_"), "id: {}", made.id);

    let listed = client.sessions().await.unwrap();
    assert!(listed.iter().any(|s| s.id == made.id), "list has created");
}

#[tokio::test]
async fn agents_include_build() {
    let base = spawn(plain_runtime().await, no_auth()).await;
    let client = Client::new(base, None);
    let agents = client.agents().await.unwrap();
    assert!(
        agents.iter().any(|a| a.name == "build"),
        "agents: {agents:?}"
    );
}

use futures::StreamExt;
use harness::{GuardTool, ScriptedRouteFactory, text_turn};
use otto_events::LLMEvent;
use otto_tui::sse::ServerEvent;

#[tokio::test]
async fn prompt_streams_llm_events() {
    let (factory, _calls) = ScriptedRouteFactory::new(vec![text_turn("t1", "done")]);
    let runtime = std::sync::Arc::new(
        otto_app::Runtime::in_memory(otto_config::Config::default())
            .await
            .unwrap()
            .with_route_factory(factory),
    );
    let base = spawn(runtime, no_auth()).await;
    let client = Client::new(base, None);
    let ses = client.create_session("Chat").await.unwrap();

    let mut got_text = String::new();
    let mut finished = false;
    let mut stream = client.prompt(&ses.id, "hi", None, None, &[]).await.unwrap();
    while let Some(ev) = stream.next().await {
        match ev {
            LLMEvent::TextDelta { text, .. } => got_text.push_str(&text),
            LLMEvent::Finish { .. } => finished = true,
            _ => {}
        }
    }
    assert_eq!(got_text, "done");
    assert!(finished, "stream ended with a finish event");
}

#[tokio::test]
async fn events_stream_surfaces_permission_and_reply_unblocks() {
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

    // Subscribe to /event before prompting.
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
            if let LLMEvent::TextDelta { text: t, .. } = ev {
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

    client
        .reply_permission(&asked.id, "once", None)
        .await
        .unwrap();
    let text = prompt_task.await.unwrap();
    assert_eq!(text, "done");
}
