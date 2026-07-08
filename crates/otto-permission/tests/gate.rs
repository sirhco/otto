//! Integration tests for the interactive ask/reply gate and [`SessionGate`].

use std::sync::Arc;

use otto_permission::{Action, Permission, Reply, Rule, Ruleset, SessionGate};
use otto_tools::{PermissionGate, PermissionRequest};

fn req(permission: &str, patterns: &[&str]) -> PermissionRequest {
    PermissionRequest {
        permission: permission.to_string(),
        patterns: patterns.iter().map(|s| s.to_string()).collect(),
        always: patterns.iter().map(|s| s.to_string()).collect(),
        metadata: serde_json::json!({}),
    }
}

fn rule(permission: &str, pattern: &str, action: Action) -> Rule {
    Rule {
        permission: permission.to_string(),
        pattern: pattern.to_string(),
        action,
    }
}

#[tokio::test]
async fn allow_returns_ok_without_prompt() {
    let perm = Permission::new(Ruleset(vec![rule("edit", "*", Action::Allow)]));
    let out = perm.ask("ses_1", req("edit", &["src/main.rs"])).await;
    assert!(out.is_ok());
    assert!(perm.list_pending().is_empty());
}

#[tokio::test]
async fn deny_returns_err_immediately() {
    let perm = Permission::new(Ruleset(vec![rule("edit", "*", Action::Deny)]));
    let out = perm.ask("ses_1", req("edit", &["src/main.rs"])).await;
    assert!(out.is_err());
    assert!(perm.list_pending().is_empty());
}

#[tokio::test]
async fn ask_blocks_then_once_ok() {
    let perm = Arc::new(Permission::new(Ruleset::new())); // default -> Ask
    let mut events = perm.subscribe();

    let p = perm.clone();
    let handle = tokio::spawn(async move { p.ask("ses_1", req("edit", &["a.rs"])).await });

    let asked = events.recv().await.unwrap();
    assert_eq!(asked.permission, "edit");
    assert!(perm.reply(&asked.request_id, Reply::Once));

    assert!(handle.await.unwrap().is_ok());
    assert!(perm.list_pending().is_empty());
}

#[tokio::test]
async fn ask_reject_with_feedback_errs() {
    let perm = Arc::new(Permission::new(Ruleset::new()));
    let mut events = perm.subscribe();

    let p = perm.clone();
    let handle = tokio::spawn(async move { p.ask("ses_1", req("edit", &["a.rs"])).await });

    let asked = events.recv().await.unwrap();
    assert!(perm.reply(
        &asked.request_id,
        Reply::Reject {
            message: Some("no".to_string())
        }
    ));

    assert!(handle.await.unwrap().is_err());
}

#[tokio::test]
async fn always_grants_session_so_later_ask_is_ok() {
    let perm = Arc::new(Permission::new(Ruleset::new()));
    let mut events = perm.subscribe();

    let p = perm.clone();
    let first = tokio::spawn(async move { p.ask("ses_1", req("edit", &["a.rs"])).await });

    let asked = events.recv().await.unwrap();
    assert!(perm.reply(&asked.request_id, Reply::Always));
    assert!(first.await.unwrap().is_ok());

    // A later ask for the same pattern is approved without prompting.
    let out = perm.ask("ses_1", req("edit", &["a.rs"])).await;
    assert!(out.is_ok());
    assert!(perm.list_pending().is_empty());
}

#[tokio::test]
async fn always_auto_resolves_concurrent_matching_request() {
    let perm = Arc::new(Permission::new(Ruleset::new()));
    let mut events = perm.subscribe();

    // Two concurrent asks for the SAME pattern in the same session.
    let p1 = perm.clone();
    let a = tokio::spawn(async move { p1.ask("ses_1", req("edit", &["a.rs"])).await });
    let p2 = perm.clone();
    let b = tokio::spawn(async move { p2.ask("ses_1", req("edit", &["a.rs"])).await });

    // Collect both request ids.
    let e1 = events.recv().await.unwrap();
    let e2 = events.recv().await.unwrap();
    assert_ne!(e1.request_id, e2.request_id);

    // Reply Always to the first; the second must auto-resolve to Ok.
    assert!(perm.reply(&e1.request_id, Reply::Always));

    assert!(a.await.unwrap().is_ok());
    assert!(b.await.unwrap().is_ok());
    assert!(perm.list_pending().is_empty());
}

#[tokio::test]
async fn reject_cascades_to_session() {
    let perm = Arc::new(Permission::new(Ruleset::new()));
    let mut events = perm.subscribe();

    // Two pending requests (different patterns) in the SAME session.
    let p1 = perm.clone();
    let a = tokio::spawn(async move { p1.ask("ses_1", req("edit", &["a.rs"])).await });
    let p2 = perm.clone();
    let b = tokio::spawn(async move { p2.ask("ses_1", req("bash", &["ls"])).await });

    let e1 = events.recv().await.unwrap();
    let e2 = events.recv().await.unwrap();
    assert_ne!(e1.request_id, e2.request_id);

    // Reject one request; the cascade rejects the other in the same session.
    assert!(perm.reply(&e1.request_id, Reply::Reject { message: None }));

    assert!(a.await.unwrap().is_err());
    assert!(b.await.unwrap().is_err());
    assert!(perm.list_pending().is_empty());
}

#[tokio::test]
async fn session_gate_delegates() {
    let perm = Arc::new(Permission::new(Ruleset(vec![rule(
        "edit",
        "*",
        Action::Allow,
    )])));
    let gate = SessionGate::new(perm.clone(), "ses_1");
    let out = gate.ask(req("edit", &["a.rs"])).await;
    assert!(out.is_ok());

    let deny = SessionGate::new(
        Arc::new(Permission::new(Ruleset(vec![rule(
            "edit",
            "*",
            Action::Deny,
        )]))),
        "ses_2",
    );
    assert!(deny.ask(req("edit", &["a.rs"])).await.is_err());
}
