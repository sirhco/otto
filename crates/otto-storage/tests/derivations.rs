//! Scripted tests for `latest()` and `filter_compacted()`
//! (`message-v2.ts:585-601`, `521-572`).

use otto_storage::model::{
    Assistant, AssistantPath, AssistantTime, Info, InfoBody, Part, PartKind, TokenCache, Tokens,
    User, UserModel, UserTime, WithParts,
};
use otto_storage::{filter_compacted, latest};

fn tokens() -> Tokens {
    Tokens {
        total: None,
        input: 0.0,
        output: 0.0,
        reasoning: 0.0,
        cache: TokenCache {
            read: 0.0,
            write: 0.0,
        },
    }
}

fn user_msg(id: &str, parts: Vec<Part>) -> WithParts {
    WithParts {
        info: Info {
            id: id.into(),
            session_id: "ses_1".into(),
            body: InfoBody::User(User {
                time: UserTime { created: 0 },
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
        },
        parts,
    }
}

fn assistant_msg(id: &str, parent: &str, finish: Option<&str>, summary: bool) -> WithParts {
    WithParts {
        info: Info {
            id: id.into(),
            session_id: "ses_1".into(),
            body: InfoBody::Assistant(Assistant {
                time: AssistantTime {
                    created: 0,
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
                summary: Some(summary),
                cost: 0.0,
                tokens: tokens(),
                structured: None,
                variant: None,
                finish: finish.map(str::to_string),
            }),
        },
        parts: vec![],
    }
}

fn compaction_part(id: &str, msg_id: &str, tail: Option<&str>) -> Part {
    Part {
        id: id.into(),
        session_id: "ses_1".into(),
        message_id: msg_id.into(),
        kind: PartKind::Compaction {
            auto: true,
            overflow: None,
            tail_start_id: tail.map(str::to_string),
        },
    }
}

fn subtask_part(id: &str, msg_id: &str) -> Part {
    Part {
        id: id.into(),
        session_id: "ses_1".into(),
        message_id: msg_id.into(),
        kind: PartKind::Subtask {
            prompt: "p".into(),
            description: "d".into(),
            agent: "worker".into(),
            model: None,
            command: None,
        },
    }
}

fn ids(msgs: &[WithParts]) -> Vec<String> {
    msgs.iter().map(|m| m.info.id.clone()).collect()
}

/// `latest()` must pick bindings by MAX id, not array order, and collect
/// compaction/subtask parts newer than the latest finished assistant.
#[test]
fn latest_by_max_id() {
    // Deliberately out of array order to prove id-based selection.
    let msgs = vec![
        user_msg(
            "msg_005",
            vec![
                subtask_part("prt_1", "msg_005"),
                compaction_part("prt_2", "msg_005", Some("msg_003")),
            ],
        ),
        assistant_msg("msg_002", "msg_001", Some("stop"), false),
        user_msg("msg_001", vec![]),
        assistant_msg("msg_004", "msg_003", None, false),
        user_msg("msg_003", vec![]),
    ];

    let out = latest(&msgs);
    assert_eq!(
        out.user.as_ref().map(|i| i.id.clone()),
        Some("msg_005".into())
    );
    assert_eq!(
        out.assistant.as_ref().map(|i| i.id.clone()),
        Some("msg_004".into())
    );
    assert_eq!(
        out.finished.as_ref().map(|i| i.id.clone()),
        Some("msg_002".into())
    );
    // finished is msg_002; msg_005 > msg_002 so both its parts count.
    assert_eq!(out.tasks.len(), 2);
    assert!(out.tasks.iter().any(Part::is_subtask));
    assert!(out.tasks.iter().any(Part::is_compaction));
}

/// No finished assistant → every compaction/subtask part is a task.
#[test]
fn latest_no_finished_collects_all_tasks() {
    let msgs = vec![
        user_msg("msg_001", vec![subtask_part("prt_1", "msg_001")]),
        assistant_msg("msg_002", "msg_001", None, false),
    ];
    let out = latest(&msgs);
    assert!(out.finished.is_none());
    assert_eq!(
        out.assistant.as_ref().map(|i| i.id.clone()),
        Some("msg_002".into())
    );
    assert_eq!(out.tasks.len(), 1);
}

/// `filter_compacted()` must drop pre-tail messages (the cut) and splice the
/// compaction+summary ahead of the retained tail (the reorder).
///
/// Input is newest-first, as produced by opencode's `stream()`.
#[test]
fn filter_compacted_cut_and_reorder() {
    // Chronological (oldest → newest):
    //   m1 user, m2 assistant       (older than tail → dropped)
    //   m3 user                     (tail_start)
    //   m4 assistant
    //   m5 user + compaction(tail=m3)   (the compaction message, C)
    //   m6 assistant summary(parent=m5) (the summary, S)
    //   m7 user, m8 assistant       (after summary → continue)
    let chronological = vec![
        user_msg("msg_001", vec![]),
        assistant_msg("msg_002", "msg_001", Some("stop"), false),
        user_msg("msg_003", vec![]),
        assistant_msg("msg_004", "msg_003", Some("stop"), false),
        user_msg(
            "msg_005",
            vec![compaction_part("prt_c", "msg_005", Some("msg_003"))],
        ),
        assistant_msg("msg_006", "msg_005", Some("stop"), true),
        user_msg("msg_007", vec![]),
        assistant_msg("msg_008", "msg_007", Some("stop"), false),
    ];
    // stream() yields newest-first.
    let mut newest_first = chronological;
    newest_first.reverse();

    let out = filter_compacted(newest_first);

    // Cut: the pre-tail messages m1, m2 are gone.
    let out_ids = ids(&out);
    assert!(
        !out_ids.contains(&"msg_001".to_string()),
        "m1 should be dropped"
    );
    assert!(
        !out_ids.contains(&"msg_002".to_string()),
        "m2 should be dropped"
    );

    // Reorder: compaction+summary spliced ahead of the retained tail.
    assert_eq!(
        out_ids,
        vec![
            "msg_005".to_string(), // compaction user (C)
            "msg_006".into(),      // summary (S)
            "msg_003".into(),      // retained tail start
            "msg_004".into(),
            "msg_007".into(), // after summary
            "msg_008".into(),
        ]
    );
}

/// With no compaction, `filter_compacted()` just reverses newest-first input
/// back to chronological order.
#[test]
fn filter_compacted_no_compaction_reverses() {
    let newest_first = vec![
        assistant_msg("msg_003", "msg_001", Some("stop"), false),
        user_msg("msg_002", vec![]),
        user_msg("msg_001", vec![]),
    ];
    let out = filter_compacted(newest_first);
    assert_eq!(ids(&out), vec!["msg_001", "msg_002", "msg_003"]);
}
