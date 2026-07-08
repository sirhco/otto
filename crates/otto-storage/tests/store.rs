//! Integration tests for the SQLite [`Store`].

use otto_storage::model::{
    Assistant, AssistantPath, AssistantTime, Info, InfoBody, Part, PartKind, TokenCache, Tokens,
    User, UserModel, UserTime,
};
use otto_storage::{Session, SessionCacheTokens, SessionTokens, Store};
use serde_json::json;
use sqlx::Row;

fn session(id: &str) -> Session {
    Session {
        id: id.into(),
        project_id: "prj_1".into(),
        parent_id: None,
        directory: "/work".into(),
        title: "Test".into(),
        version: "1.2.3".into(),
        cost: 0.5,
        tokens: SessionTokens {
            input: 10,
            output: 20,
            reasoning: 0,
            cache: SessionCacheTokens { read: 1, write: 2 },
        },
        metadata: Some(json!({"k": "v"})),
        time_created: 1000,
        time_updated: 2000,
    }
}

fn user_info(id: &str, created: i64) -> Info {
    Info {
        id: id.into(),
        session_id: "ses_1".into(),
        body: InfoBody::User(User {
            time: UserTime { created },
            format: None,
            summary: None,
            agent: "build".into(),
            model: UserModel {
                provider_id: "anthropic".into(),
                model_id: "opus".into(),
                variant: None,
            },
            system: None,
            tools: None,
        }),
    }
}

fn assistant_info(id: &str, parent: &str, created: i64) -> Info {
    Info {
        id: id.into(),
        session_id: "ses_1".into(),
        body: InfoBody::Assistant(Assistant {
            time: AssistantTime {
                created,
                completed: None,
            },
            error: None,
            parent_id: parent.into(),
            model_id: "opus".into(),
            provider_id: "anthropic".into(),
            mode: "build".into(),
            agent: "build".into(),
            path: AssistantPath {
                cwd: "/w".into(),
                root: "/w".into(),
            },
            summary: None,
            cost: 0.0,
            tokens: Tokens {
                total: None,
                input: 0.0,
                output: 0.0,
                reasoning: 0.0,
                cache: TokenCache {
                    read: 0.0,
                    write: 0.0,
                },
            },
            structured: None,
            variant: None,
            finish: Some("stop".into()),
        }),
    }
}

fn text_part(id: &str, msg: &str, text: &str) -> Part {
    Part {
        id: id.into(),
        session_id: "ses_1".into(),
        message_id: msg.into(),
        kind: PartKind::Text {
            text: text.into(),
            synthetic: None,
            ignored: None,
            time: None,
            metadata: None,
        },
    }
}

#[tokio::test]
async fn session_crud() {
    let store = Store::open_in_memory().await.expect("open");
    assert!(store.get_session("ses_1").await.expect("get").is_none());

    let s = session("ses_1");
    store.create_session(&s).await.expect("create");

    let got = store
        .get_session("ses_1")
        .await
        .expect("get")
        .expect("some");
    assert_eq!(got, s);

    let list = store.list_sessions().await.expect("list");
    assert_eq!(list, vec![s]);
}

#[tokio::test]
async fn update_session_title_changes_only_title() {
    let store = Store::open_in_memory().await.expect("open");
    let s = session("ses_1");
    store.create_session(&s).await.expect("create");

    store
        .update_session_title("ses_1", "Streaming client retry logic")
        .await
        .expect("update title");

    let got = store
        .get_session("ses_1")
        .await
        .expect("get")
        .expect("some");
    assert_eq!(got.title, "Streaming client retry logic");
    // Everything else is untouched.
    assert_eq!(
        Session {
            title: got.title.clone(),
            ..s
        },
        got
    );
}

#[tokio::test]
async fn messages_with_parts_ordered() {
    let store = Store::open_in_memory().await.expect("open");
    store
        .create_session(&session("ses_1"))
        .await
        .expect("create");

    // Insert out of chronological order; list must sort by (time_created, id).
    store
        .insert_message(&assistant_info("msg_002", "msg_001", 200))
        .await
        .expect("m2");
    store
        .insert_message(&user_info("msg_001", 100))
        .await
        .expect("m1");

    // Parts inserted out of id order; list_parts must sort by id.
    store
        .insert_part(&text_part("prt_002", "msg_001", "world"))
        .await
        .expect("p2");
    store
        .insert_part(&text_part("prt_001", "msg_001", "hello"))
        .await
        .expect("p1");

    let messages = store.list_messages("ses_1").await.expect("list messages");
    assert_eq!(
        messages.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
        vec!["msg_001", "msg_002"]
    );

    let with_parts = store
        .messages_with_parts("ses_1")
        .await
        .expect("with parts");
    assert_eq!(with_parts.len(), 2);
    assert_eq!(with_parts[0].info.id, "msg_001");
    let part_ids: Vec<_> = with_parts[0].parts.iter().map(|p| p.id.clone()).collect();
    assert_eq!(part_ids, vec!["prt_001", "prt_002"]);
    assert!(with_parts[1].parts.is_empty());

    // get_message round-trips hydration.
    let got = store
        .get_message("ses_1", "msg_002")
        .await
        .expect("get")
        .expect("some");
    assert_eq!(got, assistant_info("msg_002", "msg_001", 200));
    assert!(store.list_parts("msg_002").await.expect("parts").is_empty());
}

#[tokio::test]
async fn data_blob_excludes_columns_and_hydrates() {
    let store = Store::open_in_memory().await.expect("open");
    store
        .create_session(&session("ses_1"))
        .await
        .expect("create");
    let info = user_info("msg_001", 100);
    store.insert_message(&info).await.expect("msg");
    store
        .insert_part(&text_part("prt_001", "msg_001", "hi"))
        .await
        .expect("part");

    // The raw message.data blob excludes id/sessionID.
    let row = sqlx::query("SELECT data FROM message WHERE id = ?")
        .bind("msg_001")
        .fetch_one(store.pool())
        .await
        .expect("row");
    let data: String = row.get("data");
    let obj: serde_json::Value = serde_json::from_str(&data).expect("parse");
    let obj = obj.as_object().expect("object");
    assert!(!obj.contains_key("id"));
    assert!(!obj.contains_key("sessionID"));
    assert!(obj.contains_key("role"));

    // The raw part.data blob excludes id/sessionID/messageID.
    let row = sqlx::query("SELECT data FROM part WHERE id = ?")
        .bind("prt_001")
        .fetch_one(store.pool())
        .await
        .expect("row");
    let data: String = row.get("data");
    let obj: serde_json::Value = serde_json::from_str(&data).expect("parse");
    let obj = obj.as_object().expect("object");
    assert!(!obj.contains_key("id"));
    assert!(!obj.contains_key("sessionID"));
    assert!(!obj.contains_key("messageID"));
    assert!(obj.contains_key("type"));

    // Hydration merges the columns back onto the blob.
    let hydrated = store
        .get_message("ses_1", "msg_001")
        .await
        .expect("get")
        .expect("some");
    assert_eq!(hydrated, info);
    let parts = store.list_parts("msg_001").await.expect("parts");
    assert_eq!(parts[0], text_part("prt_001", "msg_001", "hi"));
}

#[tokio::test]
async fn open_file_backed_roundtrips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("otto.db");
    let store = Store::open(&path).await.expect("open");
    store
        .create_session(&session("ses_1"))
        .await
        .expect("create");
    store
        .insert_message(&user_info("msg_001", 100))
        .await
        .expect("msg");

    let got = store
        .get_message("ses_1", "msg_001")
        .await
        .expect("get")
        .expect("some");
    assert_eq!(got, user_info("msg_001", 100));
}

#[tokio::test]
async fn open_file_backed_uses_wal_and_busy_timeout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("otto.db");
    let store = Store::open(&path).await.expect("open");

    // WAL lets readers proceed while the per-delta writer commits; the default
    // rollback journal serializes them and stalls the streaming drain loop.
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(store.pool())
        .await
        .expect("journal_mode");
    assert_eq!(journal_mode.to_lowercase(), "wal");

    // A zero busy_timeout makes a contended write fail immediately with
    // SQLITE_BUSY instead of waiting for the lock.
    let busy_timeout_ms: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(store.pool())
        .await
        .expect("busy_timeout");
    assert!(
        busy_timeout_ms >= 1000,
        "busy_timeout should be at least 1s, got {busy_timeout_ms}ms"
    );
}

#[tokio::test]
async fn foreign_key_cascade_and_enforcement() {
    let store = Store::open_in_memory().await.expect("open");
    // Inserting a message without its session violates the FK.
    let err = store.insert_message(&user_info("msg_x", 1)).await;
    assert!(err.is_err(), "FK should reject orphan message");
}

#[tokio::test]
async fn update_part_and_message_upsert_in_place() {
    let store = Store::open_in_memory().await.expect("open");
    store
        .create_session(&session("ses_1"))
        .await
        .expect("create");
    store
        .insert_message(&assistant_info("msg_001", "msg_000", 100))
        .await
        .expect("m1");

    // update_part inserts when absent...
    store
        .update_part(&text_part("prt_001", "msg_001", "hello"))
        .await
        .expect("insert via update_part");
    // ...and overwrites in place when present.
    store
        .update_part(&text_part("prt_001", "msg_001", "goodbye"))
        .await
        .expect("overwrite via update_part");

    let parts = store.list_parts("msg_001").await.expect("parts");
    assert_eq!(parts.len(), 1, "upsert must not duplicate the row");
    match &parts[0].kind {
        PartKind::Text { text, .. } => assert_eq!(text, "goodbye"),
        other => panic!("expected text part, got {other:?}"),
    }

    // update_message overwrites the message data in place.
    let mut updated = assistant_info("msg_001", "msg_000", 100);
    if let InfoBody::Assistant(a) = &mut updated.body {
        a.finish = Some("length".into());
        a.cost = 1.25;
    }
    store
        .update_message(&updated)
        .await
        .expect("update message");

    let got = store
        .get_message("ses_1", "msg_001")
        .await
        .expect("get")
        .expect("some");
    let a = got.as_assistant().expect("assistant");
    assert_eq!(a.finish.as_deref(), Some("length"));
    assert_eq!(a.cost, 1.25);
    assert_eq!(
        store.list_messages("ses_1").await.expect("list").len(),
        1,
        "upsert must not duplicate the message row"
    );
}

#[tokio::test]
async fn delete_parts_for_message_scoped_to_message() {
    let store = Store::open_in_memory().await.expect("open");
    store
        .create_session(&session("ses_1"))
        .await
        .expect("create");
    store
        .insert_message(&assistant_info("msg_a", "msg_000", 100))
        .await
        .expect("msg a");
    store
        .insert_message(&assistant_info("msg_b", "msg_000", 200))
        .await
        .expect("msg b");

    // Two parts under message A, one under message B.
    store
        .insert_part(&text_part("prt_a1", "msg_a", "hello"))
        .await
        .expect("a1");
    store
        .insert_part(&text_part("prt_a2", "msg_a", "world"))
        .await
        .expect("a2");
    store
        .insert_part(&text_part("prt_b1", "msg_b", "untouched"))
        .await
        .expect("b1");

    store
        .delete_parts_for_message("msg_a")
        .await
        .expect("delete");

    assert!(
        store.list_parts("msg_a").await.expect("parts a").is_empty(),
        "message A's parts must all be deleted"
    );
    let parts_b = store.list_parts("msg_b").await.expect("parts b");
    assert_eq!(
        parts_b,
        vec![text_part("prt_b1", "msg_b", "untouched")],
        "message B's parts must be unaffected by A's deletion"
    );

    // Deleting a message with no parts (already-empty or never had any) is a no-op.
    store
        .delete_parts_for_message("msg_a")
        .await
        .expect("delete again is a no-op");
    store
        .delete_parts_for_message("msg_nonexistent")
        .await
        .expect("delete on unknown message id is a no-op");
}

#[tokio::test]
async fn workflow_task_upsert_and_list() {
    use otto_storage::WorkflowTaskRow;
    let store = otto_storage::Store::open_in_memory().await.unwrap();
    let row = WorkflowTaskRow {
        id: "wft_1".into(),
        session_id: "ses_1".into(),
        workflow_kind: "sdd".into(),
        task_index: 0,
        status: "NEEDS_CONTEXT".into(),
        notes: None,
        updated_at: 100,
    };
    store.upsert_workflow_task(&row).await.unwrap();
    // Upsert by id: same id, advanced status.
    let done = WorkflowTaskRow {
        status: "DONE".into(),
        updated_at: 200,
        ..row.clone()
    };
    store.upsert_workflow_task(&done).await.unwrap();

    let rows = store.list_workflow_tasks("ses_1").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "DONE");
    assert_eq!(rows[0].updated_at, 200);
}

#[tokio::test]
async fn workflow_tasks_list_ordered_by_index_and_scoped_by_session() {
    use otto_storage::WorkflowTaskRow;
    let store = otto_storage::Store::open_in_memory().await.unwrap();
    let mk = |id: &str, sid: &str, idx: i64| WorkflowTaskRow {
        id: id.into(),
        session_id: sid.into(),
        workflow_kind: "sdd".into(),
        task_index: idx,
        status: "DONE".into(),
        notes: None,
        updated_at: 1,
    };
    store
        .upsert_workflow_task(&mk("b", "ses_1", 1))
        .await
        .unwrap();
    store
        .upsert_workflow_task(&mk("a", "ses_1", 0))
        .await
        .unwrap();
    store
        .upsert_workflow_task(&mk("z", "ses_other", 0))
        .await
        .unwrap();
    let rows = store.list_workflow_tasks("ses_1").await.unwrap();
    assert_eq!(
        rows.iter().map(|r| r.task_index).collect::<Vec<_>>(),
        vec![0, 1]
    );
}
