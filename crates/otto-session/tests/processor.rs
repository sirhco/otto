//! Integration tests for the session [`Processor`] state machine.
//!
//! Each test feeds a canned [`LLMEvent`] stream (via `futures::stream::iter`)
//! into a processor backed by an in-memory store, then asserts on the parts and
//! assistant message the processor persisted.

use std::sync::Arc;
use std::sync::Mutex;

use futures::stream;
use otto_events::{FinishReason, LLMEvent, ToolResultValue, Usage};
use otto_llm::model::{ModelCost, ModelLimits};
use otto_llm::{LLMError, Model};
use otto_session::{ProcessOutcome, Processor};
use otto_storage::model::{
    Assistant, AssistantPath, AssistantTime, Info, InfoBody, PartKind, TokenCache, Tokens,
    ToolState,
};
use otto_storage::{Session, SessionTokens, Store};
use otto_tools::tool::{PermissionDenied, PermissionGate, PermissionRequest};
use serde_json::json;

const SES: &str = "ses_1";
const MSG: &str = "msg_assistant_0001";

// -- fixtures ---------------------------------------------------------------

async fn store_with_message() -> Store {
    let store = Store::open_in_memory().await.expect("open");
    store
        .create_session(&Session {
            id: SES.into(),
            project_id: "prj_1".into(),
            parent_id: None,
            directory: "/work".into(),
            title: "Test".into(),
            version: "1.0.0".into(),
            cost: 0.0,
            tokens: SessionTokens::default(),
            metadata: None,
            time_created: 1,
            time_updated: 1,
        })
        .await
        .expect("session");
    store
        .insert_message(&assistant(false))
        .await
        .expect("message");
    store
}

fn assistant(summary: bool) -> Info {
    Info {
        id: MSG.into(),
        session_id: SES.into(),
        body: InfoBody::Assistant(Assistant {
            time: AssistantTime {
                created: 100,
                completed: None,
            },
            error: None,
            parent_id: "msg_user_0000".into(),
            model_id: "opus".into(),
            provider_id: "anthropic".into(),
            mode: "build".into(),
            agent: "build".into(),
            path: AssistantPath {
                cwd: "/work".into(),
                root: "/work".into(),
            },
            summary: summary.then_some(true),
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
            finish: None,
        }),
    }
}

fn model_with_context(context: Option<u64>) -> Model {
    Model {
        limits: ModelLimits {
            context,
            input: None,
            output: None,
        },
        cost: Some(ModelCost {
            input: Some(3.0),
            output: Some(15.0),
            cache_read: Some(0.3),
            cache_write: Some(3.75),
        }),
        ..Model::new("anthropic", "opus", "route_anthropic")
    }
}

fn model() -> Model {
    model_with_context(None)
}

/// A permission gate that records every request and returns a fixed answer.
#[derive(Default)]
struct RecordingGate {
    asked: Mutex<Vec<PermissionRequest>>,
    deny: bool,
}

#[async_trait::async_trait]
impl PermissionGate for RecordingGate {
    async fn ask(&self, req: PermissionRequest) -> Result<(), PermissionDenied> {
        let permission = req.permission.clone();
        self.asked.lock().unwrap().push(req);
        if self.deny {
            Err(PermissionDenied { permission })
        } else {
            Ok(())
        }
    }
}

fn ok_stream(
    events: Vec<LLMEvent>,
) -> impl futures::Stream<Item = Result<LLMEvent, LLMError>> + Unpin {
    stream::iter(events.into_iter().map(Ok))
}

async fn parts(store: &Store) -> Vec<otto_storage::model::Part> {
    store.list_parts(MSG).await.expect("parts")
}

async fn get_assistant(store: &Store) -> Assistant {
    store
        .get_message(SES, MSG)
        .await
        .expect("get")
        .expect("some")
        .as_assistant()
        .expect("assistant")
        .clone()
}

// -- tests ------------------------------------------------------------------

#[tokio::test]
async fn text_stream_records_text_tokens_and_continues() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    let events = vec![
        LLMEvent::StepStart { index: 0 },
        LLMEvent::TextStart {
            id: "t1".into(),
            provider_metadata: None,
        },
        LLMEvent::TextDelta {
            id: "t1".into(),
            text: "Hello, ".into(),
            provider_metadata: None,
        },
        LLMEvent::TextDelta {
            id: "t1".into(),
            text: "world".into(),
            provider_metadata: None,
        },
        LLMEvent::TextEnd {
            id: "t1".into(),
            provider_metadata: None,
        },
        LLMEvent::StepFinish {
            index: 0,
            reason: FinishReason::Stop,
            usage: Some(Usage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                cache_read_input_tokens: Some(10),
                total_tokens: Some(150),
                ..Usage::default()
            }),
            provider_metadata: None,
        },
        LLMEvent::Finish {
            reason: FinishReason::Stop,
            usage: None,
            provider_metadata: None,
        },
    ];

    let outcome = proc.process(ok_stream(events)).await.expect("process");
    assert_eq!(outcome, ProcessOutcome::Continue);

    let text = parts(&store)
        .await
        .into_iter()
        .find_map(|p| match p.kind {
            PartKind::Text { text, .. } => Some(text),
            _ => None,
        })
        .expect("a text part");
    assert_eq!(text, "Hello, world");

    let a = get_assistant(&store).await;
    assert_eq!(a.finish.as_deref(), Some("stop"));
    // input adjusted for cache read: 100 - 10 = 90; output 50; cache read 10.
    assert_eq!(a.tokens.input, 90.0);
    assert_eq!(a.tokens.output, 50.0);
    assert_eq!(a.tokens.cache.read, 10.0);
    // cost = 90*3/1e6 + 50*15/1e6 + 10*0.3/1e6.
    let expected = 90.0 * 3.0 / 1e6 + 50.0 * 15.0 / 1e6 + 10.0 * 0.3 / 1e6;
    assert!(
        (a.cost - expected).abs() < 1e-12,
        "cost {} != {}",
        a.cost,
        expected
    );
    assert!(a.time.completed.is_some(), "cleanup stamps completion");
}

#[tokio::test]
async fn tool_stream_completes_with_output() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    let events = vec![
        LLMEvent::ToolInputStart {
            id: "call_1".into(),
            name: "read".into(),
            provider_metadata: None,
        },
        LLMEvent::ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            input: json!({ "filePath": "/x.txt" }),
            provider_executed: None,
            provider_metadata: None,
        },
        LLMEvent::ToolResult {
            id: "call_1".into(),
            name: "read".into(),
            result: ToolResultValue::Json {
                value: json!({ "title": "x.txt", "output": "file contents", "metadata": { "lines": 1 } }),
            },
            output: None,
            provider_executed: None,
            provider_metadata: None,
        },
        LLMEvent::StepFinish {
            index: 0,
            reason: FinishReason::ToolCalls,
            usage: None,
            provider_metadata: None,
        },
        LLMEvent::Finish {
            reason: FinishReason::ToolCalls,
            usage: None,
            provider_metadata: None,
        },
    ];

    let outcome = proc.process(ok_stream(events)).await.expect("process");
    assert_eq!(outcome, ProcessOutcome::Continue);

    let tool = parts(&store)
        .await
        .into_iter()
        .find_map(|p| match p.kind {
            PartKind::Tool { state, tool, .. } => Some((tool, state)),
            _ => None,
        })
        .expect("a tool part");
    assert_eq!(tool.0, "read");
    match tool.1 {
        ToolState::Completed {
            output,
            title,
            input,
            ..
        } => {
            assert_eq!(output, "file contents");
            assert_eq!(title, "x.txt");
            assert_eq!(input, json!({ "filePath": "/x.txt" }));
        }
        other => panic!("expected completed, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_error_rejection_blocks_and_stops() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    let events = vec![
        LLMEvent::ToolInputStart {
            id: "call_1".into(),
            name: "edit".into(),
            provider_metadata: None,
        },
        LLMEvent::ToolCall {
            id: "call_1".into(),
            name: "edit".into(),
            input: json!({ "path": "/x" }),
            provider_executed: None,
            provider_metadata: None,
        },
        LLMEvent::ToolError {
            id: "call_1".into(),
            name: "edit".into(),
            message: "permission 'edit' denied".into(),
            error: None,
            provider_metadata: None,
        },
        LLMEvent::StepFinish {
            index: 0,
            reason: FinishReason::ToolCalls,
            usage: None,
            provider_metadata: None,
        },
    ];

    let outcome = proc.process(ok_stream(events)).await.expect("process");
    assert_eq!(outcome, ProcessOutcome::Stop, "a rejection blocks the loop");

    let tool = parts(&store)
        .await
        .into_iter()
        .find_map(|p| match p.kind {
            PartKind::Tool { state, .. } => Some(state),
            _ => None,
        })
        .expect("a tool part");
    match tool {
        ToolState::Error { error, .. } => assert!(error.contains("denied")),
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_error_non_rejection_continues() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    let events = vec![
        LLMEvent::ToolInputStart {
            id: "call_1".into(),
            name: "bash".into(),
            provider_metadata: None,
        },
        LLMEvent::ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            input: json!({ "command": "false" }),
            provider_executed: None,
            provider_metadata: None,
        },
        LLMEvent::ToolError {
            id: "call_1".into(),
            name: "bash".into(),
            message: "exited with code 1".into(),
            error: None,
            provider_metadata: None,
        },
        LLMEvent::StepFinish {
            index: 0,
            reason: FinishReason::ToolCalls,
            usage: None,
            provider_metadata: None,
        },
        LLMEvent::Finish {
            reason: FinishReason::ToolCalls,
            usage: None,
            provider_metadata: None,
        },
    ];

    let outcome = proc.process(ok_stream(events)).await.expect("process");
    assert_eq!(outcome, ProcessOutcome::Continue);
}

#[tokio::test]
async fn doom_loop_asks_permission() {
    let store = store_with_message().await;
    let gate = Arc::new(RecordingGate::default());
    let gate_dyn: Arc<dyn PermissionGate> = gate.clone();
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate_dyn);

    // Three identical, fully-executed tool calls in a row.
    let mut events = Vec::new();
    for i in 0..3 {
        let id = format!("call_{i}");
        events.push(LLMEvent::ToolInputStart {
            id: id.clone(),
            name: "list".into(),
            provider_metadata: None,
        });
        events.push(LLMEvent::ToolCall {
            id: id.clone(),
            name: "list".into(),
            input: json!({ "path": "/" }),
            provider_executed: None,
            provider_metadata: None,
        });
        events.push(LLMEvent::ToolResult {
            id: id.clone(),
            name: "list".into(),
            result: ToolResultValue::Json {
                value: json!({ "output": "a\nb" }),
            },
            output: None,
            provider_executed: None,
            provider_metadata: None,
        });
    }
    events.push(LLMEvent::StepFinish {
        index: 0,
        reason: FinishReason::ToolCalls,
        usage: None,
        provider_metadata: None,
    });
    events.push(LLMEvent::Finish {
        reason: FinishReason::ToolCalls,
        usage: None,
        provider_metadata: None,
    });

    proc.process(ok_stream(events)).await.expect("process");

    let asked = gate.asked.lock().unwrap();
    assert_eq!(asked.len(), 1, "exactly one doom-loop ask");
    assert_eq!(asked[0].permission, "doom_loop");
    assert_eq!(asked[0].patterns, vec!["list".to_string()]);
    assert_eq!(asked[0].always, vec!["list".to_string()]);
}

#[tokio::test]
async fn cleanup_marks_dangling_tool_interrupted() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    // A tool call that never produces a result — the provider finished its
    // turn (terminal `Finish`) with the tool still pending, so cleanup must
    // interrupt it.
    let events = vec![
        LLMEvent::ToolInputStart {
            id: "call_1".into(),
            name: "bash".into(),
            provider_metadata: None,
        },
        LLMEvent::ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            input: json!({ "command": "sleep 999" }),
            provider_executed: None,
            provider_metadata: None,
        },
        LLMEvent::Finish {
            reason: FinishReason::ToolCalls,
            usage: None,
            provider_metadata: None,
        },
    ];

    let outcome = proc.process(ok_stream(events)).await.expect("process");
    assert_eq!(outcome, ProcessOutcome::Continue);

    let tool = parts(&store)
        .await
        .into_iter()
        .find_map(|p| match p.kind {
            PartKind::Tool { state, .. } => Some(state),
            _ => None,
        })
        .expect("a tool part");
    match tool {
        ToolState::Error {
            error, metadata, ..
        } => {
            assert_eq!(error, "Tool execution aborted");
            let meta = metadata.expect("metadata");
            assert_eq!(meta.get("interrupted"), Some(&json!(true)));
        }
        other => panic!("expected interrupted error, got {other:?}"),
    }
}

#[tokio::test]
async fn step_finish_overflow_triggers_compaction() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    // Small context window so a big step overflows.
    let mut proc = Processor::new(
        store.clone(),
        SES,
        MSG,
        model_with_context(Some(1000)),
        "build",
        gate,
    );

    let events = vec![
        LLMEvent::StepStart { index: 0 },
        LLMEvent::StepFinish {
            index: 0,
            reason: FinishReason::Length,
            usage: Some(Usage {
                input_tokens: Some(5000),
                output_tokens: Some(100),
                total_tokens: Some(5100),
                ..Usage::default()
            }),
            provider_metadata: None,
        },
        // A trailing event that must NOT be processed once compaction flips.
        LLMEvent::TextStart {
            id: "late".into(),
            provider_metadata: None,
        },
    ];

    let outcome = proc.process(ok_stream(events)).await.expect("process");
    assert_eq!(outcome, ProcessOutcome::Compact);

    // takeUntil semantics: the trailing text-start after overflow is dropped.
    let has_text = parts(&store)
        .await
        .iter()
        .any(|p| matches!(p.kind, PartKind::Text { .. }));
    assert!(!has_text, "events after overflow are not processed");
}

#[tokio::test]
async fn tool_call_during_summary_errors() {
    let store = Store::open_in_memory().await.expect("open");
    store
        .create_session(&Session {
            id: SES.into(),
            project_id: "prj_1".into(),
            parent_id: None,
            directory: "/work".into(),
            title: "Test".into(),
            version: "1.0.0".into(),
            cost: 0.0,
            tokens: SessionTokens::default(),
            metadata: None,
            time_created: 1,
            time_updated: 1,
        })
        .await
        .expect("session");
    store.insert_message(&assistant(true)).await.expect("msg");

    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    let events = vec![LLMEvent::ToolInputStart {
        id: "call_1".into(),
        name: "read".into(),
        provider_metadata: None,
    }];

    let err = proc.process(ok_stream(events)).await.unwrap_err();
    assert!(
        matches!(err, otto_session::ProcessorError::ToolWhileSummary(ref n) if n == "read"),
        "got {err:?}"
    );
    // cleanup still ran: the message is stamped completed.
    let a = get_assistant(&store).await;
    assert!(a.time.completed.is_some());
}

// -- Fix 2: mid-stream provider-error classification ------------------------

#[tokio::test]
async fn provider_error_retryable_true_is_retryable_llm_error() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    let events = vec![
        LLMEvent::StepStart { index: 0 },
        LLMEvent::ProviderError {
            message: "transient boom".into(),
            classification: None,
            retryable: Some(true),
            provider_metadata: None,
        },
    ];

    let err = proc.process(ok_stream(events)).await.unwrap_err();
    assert!(
        matches!(
            err,
            otto_session::ProcessorError::Llm(LLMError::ProviderRetryable(ref m))
                if m == "transient boom"
        ),
        "got {err:?}"
    );
    // cleanup still ran.
    let a = get_assistant(&store).await;
    assert!(a.time.completed.is_some());
}

#[tokio::test]
async fn provider_error_retryable_false_plain_is_terminal_provider_error() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    let events = vec![LLMEvent::ProviderError {
        message: "invalid request body".into(),
        classification: None,
        retryable: Some(false),
        provider_metadata: None,
    }];

    let err = proc.process(ok_stream(events)).await.unwrap_err();
    assert!(
        matches!(
            err,
            otto_session::ProcessorError::Provider(ref m) if m == "invalid request body"
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn provider_error_none_with_rate_limit_pattern_is_retryable() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    let events = vec![LLMEvent::ProviderError {
        message: "the model is overloaded, try again".into(),
        classification: None,
        retryable: None,
        provider_metadata: None,
    }];

    let err = proc.process(ok_stream(events)).await.unwrap_err();
    assert!(
        matches!(
            err,
            otto_session::ProcessorError::Llm(LLMError::ProviderRetryable(_))
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn provider_error_context_overflow_routes_to_compaction() {
    use otto_events::ProviderFailureClassification;
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    let events = vec![LLMEvent::ProviderError {
        message: "context length exceeded".into(),
        classification: Some(ProviderFailureClassification::ContextOverflow),
        retryable: None,
        provider_metadata: None,
    }];

    let outcome = proc.process(ok_stream(events)).await.expect("process");
    assert_eq!(outcome, ProcessOutcome::Compact);
}

// -- Fix 4: mid-stream terminal-finish guard --------------------------------

#[tokio::test]
async fn content_without_terminal_finish_is_retryable_no_terminal_finish() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    let events = vec![
        LLMEvent::StepStart { index: 0 },
        LLMEvent::TextStart {
            id: "t1".into(),
            provider_metadata: None,
        },
        LLMEvent::TextDelta {
            id: "t1".into(),
            text: "partial answer".into(),
            provider_metadata: None,
        },
        // clean EOF, NO Finish.
    ];

    let err = proc.process(ok_stream(events)).await.unwrap_err();
    assert!(
        matches!(
            err,
            otto_session::ProcessorError::Llm(LLMError::NoTerminalFinish)
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn content_with_terminal_finish_continues() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    let events = vec![
        LLMEvent::StepStart { index: 0 },
        LLMEvent::TextStart {
            id: "t1".into(),
            provider_metadata: None,
        },
        LLMEvent::TextDelta {
            id: "t1".into(),
            text: "full answer".into(),
            provider_metadata: None,
        },
        LLMEvent::TextEnd {
            id: "t1".into(),
            provider_metadata: None,
        },
        LLMEvent::Finish {
            reason: FinishReason::Stop,
            usage: None,
            provider_metadata: None,
        },
    ];

    let outcome = proc.process(ok_stream(events)).await.expect("process");
    assert_eq!(outcome, ProcessOutcome::Continue);
}

#[tokio::test]
async fn empty_stream_is_retryable_empty_stream_error() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    // A step frame but no real content and no Finish. Returning `Continue`
    // here made the run loop re-request immediately with no backoff, forever
    // (the zero-event loop against OpenAI-compatible gateways). It must
    // surface as `EmptyStream` — retryable with backoff — and stay distinct
    // from `NoTerminalFinish` (which implies content was streamed).
    let events = vec![LLMEvent::StepStart { index: 0 }];

    let err = proc.process(ok_stream(events)).await.unwrap_err();
    assert!(
        matches!(err, otto_session::ProcessorError::Llm(LLMError::EmptyStream)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn truly_empty_stream_is_also_empty_stream_error() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    // Zero events at all — not even a step frame (gateway returned 200 and
    // closed, or every frame was in an unrecognized shape).
    let err = proc.process(ok_stream(vec![])).await.unwrap_err();
    assert!(
        matches!(err, otto_session::ProcessorError::Llm(LLMError::EmptyStream)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn text_deltas_are_debounced_into_few_part_writes() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    // A fast burst of 200 one-char deltas. Un-debounced, each delta rewrites
    // the whole accumulated blob (200 row changes); debounced, only the start
    // row, sparse interval flushes, and the end/step/finish writes remain.
    let mut events = vec![
        LLMEvent::StepStart { index: 0 },
        LLMEvent::TextStart {
            id: "t1".into(),
            provider_metadata: None,
        },
    ];
    for _ in 0..200 {
        events.push(LLMEvent::TextDelta {
            id: "t1".into(),
            text: "x".into(),
            provider_metadata: None,
        });
    }
    events.push(LLMEvent::TextEnd {
        id: "t1".into(),
        provider_metadata: None,
    });
    events.push(LLMEvent::StepFinish {
        index: 0,
        reason: FinishReason::Stop,
        usage: None,
        provider_metadata: None,
    });
    events.push(LLMEvent::Finish {
        reason: FinishReason::Stop,
        usage: None,
        provider_metadata: None,
    });

    // The in-memory store lives on a single connection, so `total_changes()`
    // counts every row change the processor makes.
    let before: i64 = sqlx::query_scalar("SELECT total_changes()")
        .fetch_one(store.pool())
        .await
        .expect("before");
    let outcome = proc.process(ok_stream(events)).await.expect("process");
    let after: i64 = sqlx::query_scalar("SELECT total_changes()")
        .fetch_one(store.pool())
        .await
        .expect("after");
    assert_eq!(outcome, ProcessOutcome::Continue);

    let writes = after - before;
    assert!(
        writes < 30,
        "per-delta persistence must be debounced: {writes} row changes for 200 deltas"
    );

    // Debouncing must not lose text: the final part holds the full blob.
    let text = parts(&store)
        .await
        .into_iter()
        .find_map(|p| match p.kind {
            PartKind::Text { text, .. } => Some(text),
            _ => None,
        })
        .expect("a text part");
    assert_eq!(text, "x".repeat(200));
}

#[tokio::test]
async fn reasoning_deltas_are_debounced_into_few_part_writes() {
    let store = store_with_message().await;
    let gate: Arc<dyn PermissionGate> = Arc::new(RecordingGate::default());
    let mut proc = Processor::new(store.clone(), SES, MSG, model(), "build", gate);

    let mut events = vec![
        LLMEvent::StepStart { index: 0 },
        LLMEvent::ReasoningStart {
            id: "r1".into(),
            provider_metadata: None,
        },
    ];
    for _ in 0..200 {
        events.push(LLMEvent::ReasoningDelta {
            id: "r1".into(),
            text: "y".into(),
            provider_metadata: None,
        });
    }
    events.push(LLMEvent::ReasoningEnd {
        id: "r1".into(),
        provider_metadata: None,
    });
    events.push(LLMEvent::StepFinish {
        index: 0,
        reason: FinishReason::Stop,
        usage: None,
        provider_metadata: None,
    });
    events.push(LLMEvent::Finish {
        reason: FinishReason::Stop,
        usage: None,
        provider_metadata: None,
    });

    let before: i64 = sqlx::query_scalar("SELECT total_changes()")
        .fetch_one(store.pool())
        .await
        .expect("before");
    let outcome = proc.process(ok_stream(events)).await.expect("process");
    let after: i64 = sqlx::query_scalar("SELECT total_changes()")
        .fetch_one(store.pool())
        .await
        .expect("after");
    assert_eq!(outcome, ProcessOutcome::Continue);

    let writes = after - before;
    assert!(
        writes < 30,
        "per-delta persistence must be debounced: {writes} row changes for 200 deltas"
    );

    let text = parts(&store)
        .await
        .into_iter()
        .find_map(|p| match p.kind {
            PartKind::Reasoning { text, .. } => Some(text),
            _ => None,
        })
        .expect("a reasoning part");
    assert_eq!(text, "y".repeat(200));
}
