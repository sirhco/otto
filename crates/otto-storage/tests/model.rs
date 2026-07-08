//! Serde round-trip and opencode JSON-compat tests for the data model.

use otto_storage::model::{
    AgentSource, ApiError, ApiErrorData, Assistant, AssistantError, AssistantPath, AssistantTime,
    CompletedTime, FilePartSource, FilePartSourceText, Info, InfoBody, ModelRef, OutputFormat,
    Part, PartKind, Position, Range, RetryTime, StartEndReqTime, StartEndTime, StartTime,
    TokenCache, Tokens, ToolState, User, UserModel, UserSummary, UserTime,
};
use serde_json::json;

/// Round-trips a [`Part`] through JSON and asserts structural equality.
fn part_roundtrip(part: &Part) {
    let text = serde_json::to_string(part).expect("serialize part");
    let back: Part = serde_json::from_str(&text).expect("deserialize part");
    assert_eq!(part, &back, "part round-trip mismatch: {text}");

    // The persisted `data` blob must exclude id/sessionID/messageID.
    let data = part.data_json().expect("data json");
    let value: serde_json::Value = serde_json::from_str(&data).expect("parse data");
    let obj = value.as_object().expect("data is object");
    assert!(!obj.contains_key("id"), "data leaked id: {data}");
    assert!(
        !obj.contains_key("sessionID"),
        "data leaked sessionID: {data}"
    );
    assert!(
        !obj.contains_key("messageID"),
        "data leaked messageID: {data}"
    );
    assert!(obj.contains_key("type"), "data missing type: {data}");

    // Reconstructing from the blob + columns yields the original.
    let rebuilt = Part::from_row(
        part.id.clone(),
        part.session_id.clone(),
        part.message_id.clone(),
        &data,
    )
    .expect("from_row");
    assert_eq!(part, &rebuilt, "part hydration mismatch");
}

fn part(kind: PartKind) -> Part {
    Part {
        id: "prt_test".into(),
        session_id: "ses_test".into(),
        message_id: "msg_test".into(),
        kind,
    }
}

fn tokens() -> Tokens {
    Tokens {
        total: Some(30.0),
        input: 10.0,
        output: 20.0,
        reasoning: 0.0,
        cache: TokenCache {
            read: 1.0,
            write: 2.0,
        },
    }
}

#[test]
fn part_text_roundtrip() {
    part_roundtrip(&part(PartKind::Text {
        text: "hello".into(),
        synthetic: Some(true),
        ignored: None,
        time: Some(StartEndTime {
            start: 1,
            end: Some(2),
        }),
        metadata: Some(json!({"k": "v"})),
    }));
    // Minimal text part (only required field).
    part_roundtrip(&part(PartKind::Text {
        text: String::new(),
        synthetic: None,
        ignored: None,
        time: None,
        metadata: None,
    }));
}

#[test]
fn part_reasoning_roundtrip() {
    part_roundtrip(&part(PartKind::Reasoning {
        text: "thinking".into(),
        time: StartEndTime {
            start: 5,
            end: None,
        },
        metadata: Some(json!({"anthropic": {"signature": "sig"}})),
    }));
}

#[test]
fn part_file_roundtrip() {
    part_roundtrip(&part(PartKind::File {
        mime: "image/png".into(),
        filename: Some("a.png".into()),
        url: "data:image/png;base64,AAA".into(),
        source: Some(FilePartSource::Symbol {
            text: FilePartSourceText {
                value: "sym".into(),
                start: 0.0,
                end: 3.0,
            },
            path: "src/lib.rs".into(),
            range: Range {
                start: Position {
                    line: 1,
                    character: 2,
                },
                end: Position {
                    line: 3,
                    character: 4,
                },
            },
            name: "foo".into(),
            kind: 12,
        }),
    }));
    part_roundtrip(&part(PartKind::File {
        mime: "text/plain".into(),
        filename: None,
        url: "file:///x".into(),
        source: Some(FilePartSource::Resource {
            text: FilePartSourceText {
                value: "r".into(),
                start: 0.0,
                end: 1.0,
            },
            client_name: "mcp".into(),
            uri: "res://x".into(),
        }),
    }));
    part_roundtrip(&part(PartKind::File {
        mime: "text/plain".into(),
        filename: None,
        url: "file:///y".into(),
        source: Some(FilePartSource::File {
            text: FilePartSourceText {
                value: "f".into(),
                start: 0.0,
                end: 1.0,
            },
            path: "y".into(),
        }),
    }));
}

#[test]
fn part_tool_states_roundtrip() {
    let states = vec![
        ToolState::Pending {
            input: json!({"a": 1}),
            raw: "{\"a\":1}".into(),
        },
        ToolState::Running {
            input: json!({"a": 1}),
            title: Some("running".into()),
            metadata: Some(json!({"x": 1})),
            time: StartTime { start: 10 },
        },
        ToolState::Completed {
            input: json!({"a": 1}),
            output: "done".into(),
            title: "Read".into(),
            metadata: json!({"lines": 3}),
            time: CompletedTime {
                start: 10,
                end: 20,
                compacted: Some(25),
            },
            attachments: None,
        },
        ToolState::Error {
            input: json!({"a": 1}),
            error: "boom".into(),
            metadata: None,
            time: StartEndReqTime { start: 10, end: 20 },
        },
    ];
    for state in states {
        part_roundtrip(&part(PartKind::Tool {
            call_id: "call_1".into(),
            tool: "read".into(),
            metadata: Some(json!({"providerExecuted": false})),
            state,
        }));
    }
}

#[test]
fn part_step_and_misc_roundtrip() {
    part_roundtrip(&part(PartKind::StepStart {
        snapshot: Some("snap".into()),
    }));
    part_roundtrip(&part(PartKind::StepFinish {
        reason: "stop".into(),
        snapshot: None,
        cost: 0.5,
        tokens: tokens(),
    }));
    part_roundtrip(&part(PartKind::Snapshot {
        snapshot: "s".into(),
    }));
    part_roundtrip(&part(PartKind::Patch {
        hash: "abc".into(),
        files: vec!["a".into(), "b".into()],
    }));
    part_roundtrip(&part(PartKind::Agent {
        name: "reviewer".into(),
        source: Some(AgentSource {
            value: "@reviewer".into(),
            start: 0,
            end: 9,
        }),
    }));
    part_roundtrip(&part(PartKind::Subtask {
        prompt: "do it".into(),
        description: "task".into(),
        agent: "worker".into(),
        model: Some(ModelRef {
            provider_id: "anthropic".into(),
            model_id: "opus".into(),
        }),
        command: Some("/run".into()),
    }));
    part_roundtrip(&part(PartKind::Compaction {
        auto: true,
        overflow: Some(false),
        tail_start_id: Some("msg_tail".into()),
    }));
    part_roundtrip(&part(PartKind::Retry {
        attempt: 2,
        error: ApiError::ApiError(ApiErrorData {
            message: "rate limited".into(),
            status_code: Some(429),
            is_retryable: true,
            response_headers: None,
            response_body: None,
            metadata: None,
        }),
        time: RetryTime { created: 99 },
    }));
}

#[test]
fn tool_completed_with_attachment_roundtrip() {
    let attachment = part(PartKind::File {
        mime: "image/png".into(),
        filename: Some("shot.png".into()),
        url: "data:image/png;base64,ZZ".into(),
        source: None,
    });
    part_roundtrip(&part(PartKind::Tool {
        call_id: "c".into(),
        tool: "screenshot".into(),
        metadata: None,
        state: ToolState::Completed {
            input: json!({}),
            output: "ok".into(),
            title: "Shot".into(),
            metadata: json!({}),
            time: CompletedTime {
                start: 1,
                end: 2,
                compacted: None,
            },
            attachments: Some(vec![attachment]),
        },
    }));
}

fn info_roundtrip(info: &Info) {
    let text = serde_json::to_string(info).expect("serialize info");
    let back: Info = serde_json::from_str(&text).expect("deserialize info");
    assert_eq!(info, &back, "info round-trip mismatch: {text}");

    let data = info.data_json().expect("data json");
    let value: serde_json::Value = serde_json::from_str(&data).expect("parse data");
    let obj = value.as_object().expect("data is object");
    assert!(!obj.contains_key("id"), "data leaked id: {data}");
    assert!(
        !obj.contains_key("sessionID"),
        "data leaked sessionID: {data}"
    );
    assert!(obj.contains_key("role"), "data missing role: {data}");

    let rebuilt =
        Info::from_row(info.id.clone(), info.session_id.clone(), &data).expect("from_row");
    assert_eq!(info, &rebuilt, "info hydration mismatch");
}

#[test]
fn info_user_roundtrip() {
    let mut tools = std::collections::HashMap::new();
    tools.insert("read".to_string(), true);
    info_roundtrip(&Info {
        id: "msg_u".into(),
        session_id: "ses_1".into(),
        body: InfoBody::User(User {
            time: UserTime { created: 1000 },
            format: Some(OutputFormat::JsonSchema {
                schema: json!({"type": "object"}),
                retry_count: Some(2),
            }),
            summary: Some(UserSummary {
                title: Some("t".into()),
                body: None,
                diffs: vec![json!({"file": "a"})],
            }),
            agent: "build".into(),
            model: UserModel {
                provider_id: "anthropic".into(),
                model_id: "opus".into(),
                variant: Some("thinking".into()),
            },
            system: Some("sys".into()),
            tools: Some(tools),
        }),
    });
    // Minimal user with text format.
    info_roundtrip(&Info {
        id: "msg_u2".into(),
        session_id: "ses_1".into(),
        body: InfoBody::User(User {
            time: UserTime { created: 1 },
            format: Some(OutputFormat::Text),
            summary: None,
            agent: "build".into(),
            model: UserModel {
                provider_id: "anthropic".into(),
                model_id: "sonnet".into(),
                variant: None,
            },
            system: None,
            tools: None,
        }),
    });
}

#[test]
fn info_assistant_roundtrip() {
    info_roundtrip(&Info {
        id: "msg_a".into(),
        session_id: "ses_1".into(),
        body: InfoBody::Assistant(assistant_body(Some("stop"), None)),
    });
}

#[test]
fn assistant_error_variants_roundtrip() {
    let errors = vec![
        AssistantError::ProviderAuthError {
            provider_id: "anthropic".into(),
            message: "no key".into(),
        },
        AssistantError::UnknownError {
            message: "?".into(),
            r#ref: Some("ref_1".into()),
        },
        AssistantError::MessageOutputLengthError {},
        AssistantError::MessageAbortedError {
            message: "stopped".into(),
        },
        AssistantError::StructuredOutputError {
            message: "bad".into(),
            retries: 3,
        },
        AssistantError::ContextOverflowError {
            message: "too big".into(),
            response_body: Some("body".into()),
        },
        AssistantError::ContentFilterError {
            message: "blocked".into(),
        },
        AssistantError::ApiError(ApiErrorData {
            message: "500".into(),
            status_code: Some(500),
            is_retryable: false,
            response_headers: None,
            response_body: None,
            metadata: None,
        }),
    ];
    for err in errors {
        let info = Info {
            id: "msg_e".into(),
            session_id: "ses_1".into(),
            body: InfoBody::Assistant(assistant_body(None, Some(err))),
        };
        info_roundtrip(&info);
    }
}

fn assistant_body(finish: Option<&str>, error: Option<AssistantError>) -> Assistant {
    Assistant {
        time: AssistantTime {
            created: 2000,
            completed: Some(2100),
        },
        error,
        parent_id: "msg_u".into(),
        model_id: "opus".into(),
        provider_id: "anthropic".into(),
        mode: "build".into(),
        agent: "build".into(),
        path: AssistantPath {
            cwd: "/work".into(),
            root: "/work".into(),
        },
        summary: Some(false),
        cost: 0.25,
        tokens: tokens(),
        structured: None,
        variant: None,
        finish: finish.map(str::to_string),
    }
}

/// Guards against field drift: a literal opencode-shaped `tool` part with a
/// `completed` state must deserialize into the typed model.
#[test]
fn json_compat_tool_part_completed() {
    let raw = json!({
        "id": "prt_abc",
        "sessionID": "ses_abc",
        "messageID": "msg_abc",
        "type": "tool",
        "callID": "toolu_1",
        "tool": "bash",
        "state": {
            "status": "completed",
            "input": {"command": "ls"},
            "output": "a\nb\n",
            "title": "ls",
            "metadata": {"exit": 0},
            "time": {"start": 1000, "end": 1050}
        }
    });
    let parsed: Part = serde_json::from_value(raw).expect("deserialize opencode tool part");
    assert_eq!(parsed.id, "prt_abc");
    assert_eq!(parsed.session_id, "ses_abc");
    assert_eq!(parsed.message_id, "msg_abc");
    match parsed.kind {
        PartKind::Tool {
            call_id,
            tool,
            state,
            ..
        } => {
            assert_eq!(call_id, "toolu_1");
            assert_eq!(tool, "bash");
            match state {
                ToolState::Completed {
                    output,
                    title,
                    time,
                    ..
                } => {
                    assert_eq!(output, "a\nb\n");
                    assert_eq!(title, "ls");
                    assert_eq!(time.start, 1000);
                    assert_eq!(time.end, 1050);
                    assert_eq!(time.compacted, None);
                }
                other => panic!("expected completed, got {other:?}"),
            }
        }
        other => panic!("expected tool part, got {other:?}"),
    }
}

/// Guards against field drift: a literal opencode-shaped assistant message.
#[test]
fn json_compat_assistant_message() {
    let raw = json!({
        "id": "msg_1",
        "sessionID": "ses_1",
        "role": "assistant",
        "time": {"created": 1700000000000i64},
        "parentID": "msg_0",
        "modelID": "claude-opus-4",
        "providerID": "anthropic",
        "mode": "build",
        "agent": "build",
        "path": {"cwd": "/w", "root": "/w"},
        "cost": 0.01,
        "tokens": {
            "input": 100, "output": 50, "reasoning": 0,
            "cache": {"read": 10, "write": 5}
        },
        "finish": "stop"
    });
    let parsed: Info = serde_json::from_value(raw).expect("deserialize opencode assistant");
    assert_eq!(parsed.id, "msg_1");
    let a = parsed.as_assistant().expect("assistant");
    assert_eq!(a.parent_id, "msg_0");
    assert_eq!(a.provider_id, "anthropic");
    assert_eq!(a.finish.as_deref(), Some("stop"));
    assert_eq!(a.tokens.input, 100.0);
    assert_eq!(a.tokens.cache.read, 10.0);
}
