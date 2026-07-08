//! Golden tests for [`otto_session::to_model_messages`], porting the cases
//! exercised by opencode's `message-v2.ts` converter.

use otto_events::ToolResultValue;
use otto_llm::message::{ContentPart, Message, Role};
use otto_llm::model::{ModelId, ProviderId};
use otto_session::{to_model_messages, ConvertOptions};
use otto_storage::model::{
    Assistant, AssistantError, AssistantPath, AssistantTime, CompletedTime, Info, InfoBody, Part,
    PartKind, StartTime, TokenCache, Tokens, ToolState, User, UserModel, UserTime, WithParts,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

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

fn part(kind: PartKind) -> Part {
    Part {
        id: "prt_1".into(),
        session_id: "ses_1".into(),
        message_id: "msg_1".into(),
        kind,
    }
}

fn user_msg(parts: Vec<PartKind>) -> WithParts {
    WithParts {
        info: Info {
            id: "msg_u".into(),
            session_id: "ses_1".into(),
            body: InfoBody::User(User {
                time: UserTime { created: 0 },
                format: None,
                summary: None,
                agent: "build".into(),
                model: UserModel {
                    provider_id: "anthropic".into(),
                    model_id: "claude-sonnet-4".into(),
                    variant: None,
                },
                system: None,
                tools: None,
            }),
        },
        parts: parts.into_iter().map(part).collect(),
    }
}

fn assistant_msg_with(
    provider: &str,
    model: &str,
    error: Option<AssistantError>,
    parts: Vec<PartKind>,
) -> WithParts {
    WithParts {
        info: Info {
            id: "msg_a".into(),
            session_id: "ses_1".into(),
            body: InfoBody::Assistant(Assistant {
                time: AssistantTime {
                    created: 0,
                    completed: None,
                },
                error,
                parent_id: "msg_u".into(),
                model_id: model.into(),
                provider_id: provider.into(),
                mode: "primary".into(),
                agent: "build".into(),
                path: AssistantPath {
                    cwd: "/tmp".into(),
                    root: "/tmp".into(),
                },
                summary: None,
                cost: 0.0,
                tokens: tokens(),
                structured: None,
                variant: None,
                finish: None,
            }),
        },
        parts: parts.into_iter().map(part).collect(),
    }
}

fn assistant_msg(parts: Vec<PartKind>) -> WithParts {
    assistant_msg_with("anthropic", "claude-sonnet-4", None, parts)
}

fn text_part(text: &str) -> PartKind {
    PartKind::Text {
        text: text.into(),
        synthetic: None,
        ignored: None,
        time: None,
        metadata: None,
    }
}

fn completed_tool(call_id: &str, tool: &str, input: Value, output: &str) -> PartKind {
    PartKind::Tool {
        call_id: call_id.into(),
        tool: tool.into(),
        metadata: None,
        state: ToolState::Completed {
            input,
            output: output.into(),
            title: "t".into(),
            metadata: json!({}),
            time: CompletedTime {
                start: 0,
                end: 1,
                compacted: None,
            },
            attachments: None,
        },
    }
}

fn file_attachment(mime: &str, url: &str, filename: Option<&str>) -> Part {
    part(PartKind::File {
        mime: mime.into(),
        filename: filename.map(str::to_string),
        url: url.into(),
        source: None,
    })
}

fn anthropic() -> (ProviderId, ModelId) {
    (
        ProviderId::new("anthropic"),
        ModelId::new("claude-sonnet-4"),
    )
}

fn convert(msgs: &[WithParts]) -> Vec<Message> {
    let (p, m) = anthropic();
    to_model_messages(msgs, &p, &m, &ConvertOptions::default())
}

// ---------------------------------------------------------------------------
// User messages
// ---------------------------------------------------------------------------

#[test]
fn user_text_and_file() {
    let msg = user_msg(vec![
        text_part("hello"),
        PartKind::File {
            mime: "image/png".into(),
            filename: Some("a.png".into()),
            url: "data:image/png;base64,AAAA".into(),
            source: None,
        },
    ]);
    let out = convert(&[msg]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].role, Role::User);
    assert_eq!(out[0].content.len(), 2);
    assert_eq!(out[0].content[0], ContentPart::text("hello"));
    assert_eq!(
        out[0].content[1],
        ContentPart::Media {
            media_type: "image/png".into(),
            data: "AAAA".into(),
            filename: Some("a.png".into()),
        }
    );
}

#[test]
fn user_skips_ignored_empty_and_plaintext() {
    let msg = user_msg(vec![
        PartKind::Text {
            text: "ignored".into(),
            synthetic: None,
            ignored: Some(true),
            time: None,
            metadata: None,
        },
        text_part(""),
        PartKind::File {
            mime: "text/plain".into(),
            filename: Some("f.txt".into()),
            url: "data:text/plain,hi".into(),
            source: None,
        },
        text_part("kept"),
    ]);
    let out = convert(&[msg]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].content, vec![ContentPart::text("kept")]);
}

#[test]
fn user_strip_media_replaces_with_text() {
    let msg = user_msg(vec![PartKind::File {
        mime: "image/png".into(),
        filename: Some("a.png".into()),
        url: "data:image/png;base64,AAAA".into(),
        source: None,
    }]);
    let (p, m) = anthropic();
    let out = to_model_messages(
        &[msg],
        &p,
        &m,
        &ConvertOptions {
            strip_media: true,
            tool_output_max_chars: None,
        },
    );
    assert_eq!(
        out[0].content,
        vec![ContentPart::text("[Attached image/png: a.png]")]
    );
}

#[test]
fn user_compaction_and_subtask_substitution() {
    let msg = user_msg(vec![
        PartKind::Compaction {
            auto: false,
            overflow: None,
            tail_start_id: None,
        },
        PartKind::Subtask {
            prompt: "p".into(),
            description: "d".into(),
            agent: "build".into(),
            model: None,
            command: None,
        },
    ]);
    let out = convert(&[msg]);
    assert_eq!(
        out[0].content,
        vec![
            ContentPart::text("What did we do so far?"),
            ContentPart::text("The following tool was executed by the user"),
        ]
    );
}

#[test]
fn empty_message_dropped() {
    let msg = user_msg(vec![]);
    assert!(convert(&[msg]).is_empty());
}

// ---------------------------------------------------------------------------
// Assistant + tools
// ---------------------------------------------------------------------------

#[test]
fn assistant_text_and_completed_tool() {
    let msg = assistant_msg(vec![
        text_part("working"),
        completed_tool("call_1", "bash", json!({"cmd": "ls"}), "file listing"),
    ]);
    let out = convert(&[msg]);
    assert_eq!(out.len(), 2);

    // assistant: text + tool-call
    assert_eq!(out[0].role, Role::Assistant);
    assert_eq!(out[0].content[0], ContentPart::text("working"));
    assert_eq!(
        out[0].content[1],
        ContentPart::ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            input: json!({"cmd": "ls"}),
            provider_executed: None,
        }
    );

    // tool: result
    assert_eq!(out[1].role, Role::Tool);
    assert_eq!(
        out[1].content[0],
        ContentPart::ToolResult {
            id: "call_1".into(),
            name: "bash".into(),
            result: ToolResultValue::Text {
                value: Value::String("file listing".into())
            },
            provider_executed: None,
            cache: None,
        }
    );
}

#[test]
fn completed_tool_truncates_output() {
    let msg = assistant_msg(vec![completed_tool("c", "bash", json!({}), "abcdefghij")]);
    let (p, m) = anthropic();
    let out = to_model_messages(
        &[msg],
        &p,
        &m,
        &ConvertOptions {
            strip_media: false,
            tool_output_max_chars: Some(4),
        },
    );
    let ContentPart::ToolResult {
        result: ToolResultValue::Text { value },
        ..
    } = &out[1].content[0]
    else {
        panic!("expected text tool result");
    };
    assert_eq!(
        value.as_str().unwrap(),
        "abcd\n[Tool output truncated for compaction: omitted 6 chars]"
    );
}

#[test]
fn compacted_tool_output_cleared() {
    let msg = assistant_msg(vec![PartKind::Tool {
        call_id: "c".into(),
        tool: "bash".into(),
        metadata: None,
        state: ToolState::Completed {
            input: json!({}),
            output: "big".into(),
            title: "t".into(),
            metadata: json!({}),
            time: CompletedTime {
                start: 0,
                end: 1,
                compacted: Some(2),
            },
            attachments: None,
        },
    }]);
    let out = convert(&[msg]);
    let ContentPart::ToolResult {
        result: ToolResultValue::Text { value },
        ..
    } = &out[1].content[0]
    else {
        panic!("expected text");
    };
    assert_eq!(value.as_str().unwrap(), "[Old tool result content cleared]");
}

#[test]
fn dangling_running_tool_synthetic_error() {
    let msg = assistant_msg(vec![PartKind::Tool {
        call_id: "c".into(),
        tool: "bash".into(),
        metadata: None,
        state: ToolState::Running {
            input: json!({"x": 1}),
            title: None,
            metadata: None,
            time: StartTime { start: 0 },
        },
    }]);
    let out = convert(&[msg]);
    // still emits the tool-call
    assert_eq!(
        out[0].content[0],
        ContentPart::ToolCall {
            id: "c".into(),
            name: "bash".into(),
            input: json!({"x": 1}),
            provider_executed: None,
        }
    );
    assert_eq!(
        out[1].content[0],
        ContentPart::ToolResult {
            id: "c".into(),
            name: "bash".into(),
            result: ToolResultValue::Error {
                value: Value::String("[Tool execution was interrupted]".into())
            },
            provider_executed: None,
            cache: None,
        }
    );
}

#[test]
fn error_tool_emits_error_result() {
    let msg = assistant_msg(vec![PartKind::Tool {
        call_id: "c".into(),
        tool: "bash".into(),
        metadata: None,
        state: ToolState::Error {
            input: json!({}),
            error: "boom".into(),
            metadata: None,
            time: otto_storage::model::StartEndReqTime { start: 0, end: 1 },
        },
    }]);
    let out = convert(&[msg]);
    assert_eq!(
        out[1].content[0],
        ContentPart::ToolResult {
            id: "c".into(),
            name: "bash".into(),
            result: ToolResultValue::Error {
                value: Value::String("boom".into())
            },
            provider_executed: None,
            cache: None,
        }
    );
}

#[test]
fn interrupted_error_tool_emits_text_result() {
    let msg = assistant_msg(vec![PartKind::Tool {
        call_id: "c".into(),
        tool: "bash".into(),
        metadata: None,
        state: ToolState::Error {
            input: json!({}),
            error: "boom".into(),
            metadata: Some(json!({"interrupted": true, "output": "partial"})),
            time: otto_storage::model::StartEndReqTime { start: 0, end: 1 },
        },
    }]);
    let out = convert(&[msg]);
    assert_eq!(
        out[1].content[0],
        ContentPart::ToolResult {
            id: "c".into(),
            name: "bash".into(),
            result: ToolResultValue::Text {
                value: Value::String("partial".into())
            },
            provider_executed: None,
            cache: None,
        }
    );
}

// ---------------------------------------------------------------------------
// Reasoning
// ---------------------------------------------------------------------------

#[test]
fn reasoning_same_model_kept_with_signature() {
    let msg = assistant_msg(vec![PartKind::Reasoning {
        text: "thinking".into(),
        time: otto_storage::model::StartEndTime {
            start: 0,
            end: None,
        },
        metadata: Some(json!({"anthropic": {"signature": "sig123"}})),
    }]);
    let out = convert(&[msg]);
    assert_eq!(
        out[0].content[0],
        ContentPart::Reasoning {
            text: "thinking".into(),
            encrypted: Some("sig123".into()),
        }
    );
}

#[test]
fn reasoning_different_model_downgraded_to_text() {
    // stored on gpt, requested on claude → different model
    let msg = assistant_msg_with(
        "openai",
        "gpt-4o",
        None,
        vec![
            PartKind::Reasoning {
                text: "hidden thought".into(),
                time: otto_storage::model::StartEndTime {
                    start: 0,
                    end: None,
                },
                metadata: None,
            },
            PartKind::Reasoning {
                text: "   ".into(),
                time: otto_storage::model::StartEndTime {
                    start: 0,
                    end: None,
                },
                metadata: None,
            },
        ],
    );
    let out = convert(&[msg]);
    // empty reasoning dropped, non-empty becomes plain text
    assert_eq!(out[0].content, vec![ContentPart::text("hidden thought")]);
}

#[test]
fn signed_reasoning_empty_text_becomes_space() {
    let msg = assistant_msg(vec![
        PartKind::Reasoning {
            text: "r".into(),
            time: otto_storage::model::StartEndTime {
                start: 0,
                end: None,
            },
            metadata: Some(json!({"anthropic": {"signature": "s"}})),
        },
        text_part(""),
    ]);
    let out = convert(&[msg]);
    // reasoning then a single-space text separator
    assert_eq!(out[0].content[1], ContentPart::text(" "));
}

// ---------------------------------------------------------------------------
// Media gating
// ---------------------------------------------------------------------------

fn completed_tool_with_attachments(attachments: Vec<Part>) -> PartKind {
    PartKind::Tool {
        call_id: "c".into(),
        tool: "read".into(),
        metadata: None,
        state: ToolState::Completed {
            input: json!({}),
            output: "see image".into(),
            title: "t".into(),
            metadata: json!({}),
            time: CompletedTime {
                start: 0,
                end: 1,
                compacted: None,
            },
            attachments: Some(attachments),
        },
    }
}

#[test]
fn media_in_tool_result_inline_for_anthropic() {
    let msg = assistant_msg(vec![completed_tool_with_attachments(vec![
        file_attachment("image/png", "data:image/png;base64,IMG", Some("x.png")),
    ])]);
    let out = convert(&[msg]);
    // anthropic supports media inline → Content result, no follow-up user msg
    assert_eq!(out.len(), 2);
    let ContentPart::ToolResult {
        result: ToolResultValue::Content { value },
        ..
    } = &out[1].content[0]
    else {
        panic!("expected content result");
    };
    assert_eq!(value[0], json!({"type": "text", "text": "see image"}));
    assert_eq!(
        value[1],
        json!({"type": "media", "mediaType": "image/png", "data": "IMG"})
    );
}

#[test]
fn media_in_tool_result_extracted_for_gemini2() {
    // google + gemini-2 → not supported → media extracted to a user message
    let msg = assistant_msg_with(
        "google",
        "gemini-2.5-pro",
        None,
        vec![completed_tool_with_attachments(vec![file_attachment(
            "image/png",
            "data:image/png;base64,IMG",
            Some("x.png"),
        )])],
    );
    let (p, m) = (ProviderId::new("google"), ModelId::new("gemini-2.5-pro"));
    let out = to_model_messages(&[msg], &p, &m, &ConvertOptions::default());
    assert_eq!(out.len(), 3);
    // tool result is text-only (media extracted, no final attachments)
    assert_eq!(
        out[1].content[0],
        ContentPart::ToolResult {
            id: "c".into(),
            name: "read".into(),
            result: ToolResultValue::Text {
                value: Value::String("see image".into())
            },
            provider_executed: None,
            cache: None,
        }
    );
    // follow-up user message with the extracted media
    assert_eq!(out[2].role, Role::User);
    assert_eq!(
        out[2].content[0],
        ContentPart::text("Attached media from tool result:")
    );
    assert_eq!(
        out[2].content[1],
        ContentPart::Media {
            media_type: "image/png".into(),
            data: "IMG".into(),
            filename: Some("x.png".into()),
        }
    );
}

// ---------------------------------------------------------------------------
// Error-message skip rule
// ---------------------------------------------------------------------------

#[test]
fn errored_message_skipped() {
    let msg = assistant_msg_with(
        "anthropic",
        "claude-sonnet-4",
        Some(AssistantError::UnknownError {
            message: "fail".into(),
            r#ref: None,
        }),
        vec![text_part("partial")],
    );
    assert!(convert(&[msg]).is_empty());
}

#[test]
fn aborted_message_with_real_part_kept() {
    let msg = assistant_msg_with(
        "anthropic",
        "claude-sonnet-4",
        Some(AssistantError::MessageAbortedError {
            message: "aborted".into(),
        }),
        vec![text_part("done before abort")],
    );
    let out = convert(&[msg]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].content, vec![ContentPart::text("done before abort")]);
}

#[test]
fn aborted_message_only_stepstart_skipped() {
    let msg = assistant_msg_with(
        "anthropic",
        "claude-sonnet-4",
        Some(AssistantError::MessageAbortedError {
            message: "aborted".into(),
        }),
        vec![PartKind::StepStart { snapshot: None }],
    );
    assert!(convert(&[msg]).is_empty());
}
