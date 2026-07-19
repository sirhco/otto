//! Proof the core seam composes: a trivial `MockProtocol` wired through
//! `GenericRoute` + a `MockTransport` (canned SSE frames, no network), folded
//! by `LLMClient::generate` into an `LLMResponse`.
//!
//! This exercises the whole pipeline — `build_body` → transport frames →
//! `decode_event` → `step` (via `Lifecycle` + `ToolStream`) → inclusive
//! `terminal` take-until → `on_halt` flush → `LLMResponse::reduce` — before any
//! real protocol exists.

use std::sync::Arc;

use futures::stream::{self, BoxStream};
use otto_events::{FinishReason, LLMEvent, Usage};
use otto_llm::lifecycle;
use otto_llm::tool_stream;
use otto_llm::{AuthDef, PreparedHttp};
use otto_llm::{
    ContentPart, Endpoint, GenericRoute, LLMClient, LLMError, LLMRequest, Message, Model, Protocol,
    Transport,
};

/// Per-stream state: the lifecycle machine plus a tool accumulator keyed by
/// index.
#[derive(Default)]
struct MockState {
    life: lifecycle::State,
    tools: tool_stream::State<u64>,
}

/// A trivial protocol whose events are plain JSON objects tagged by `type`.
struct MockProtocol;

impl Protocol for MockProtocol {
    type Body = serde_json::Value;
    type Event = serde_json::Value;
    type State = MockState;

    fn id(&self) -> &'static str {
        "mock"
    }

    fn build_body(&self, req: &LLMRequest) -> Result<Self::Body, LLMError> {
        Ok(serde_json::json!({
            "model": req.model.id.0,
            "messages": req.messages.len(),
        }))
    }

    fn decode_event(&self, frame: &str) -> Result<Self::Event, LLMError> {
        serde_json::from_str(frame).map_err(|e| LLMError::EventDecode(e.to_string()))
    }

    fn initial(&self, _req: &LLMRequest) -> Self::State {
        MockState::default()
    }

    fn step(&self, state: &mut Self::State, event: Self::Event) -> Result<Vec<LLMEvent>, LLMError> {
        let kind = event["type"].as_str().unwrap_or_default();
        let mut out = Vec::new();
        match kind {
            "text" => {
                out.extend(state.life.step_start(0));
                let id = event["id"].as_str().unwrap_or("t");
                let text = event["text"].as_str().unwrap_or_default();
                out.extend(state.life.text_delta(id, text));
            }
            "tool_start" => {
                out.extend(state.life.step_start(0));
                let index = event["index"].as_u64().unwrap_or(0);
                let id = event["id"].as_str().unwrap_or_default().to_string();
                let name = event["name"].as_str().unwrap_or_default().to_string();
                out.extend(state.tools.start(index, id, name, None, None));
            }
            "tool_delta" => {
                let index = event["index"].as_u64().unwrap_or(0);
                let text = event["text"].as_str().unwrap_or_default();
                out.extend(state.tools.append_existing(&index, text)?);
            }
            "tool_finish" => {
                let index = event["index"].as_u64().unwrap_or(0);
                out.extend(state.tools.finish(&index)?);
            }
            "finish" => {
                out.extend(state.life.finish(
                    FinishReason::Stop,
                    Some(Usage {
                        output_tokens: Some(3),
                        ..Usage::default()
                    }),
                    0,
                ));
            }
            other => {
                return Err(LLMError::EventDecode(format!(
                    "unknown mock event: {other}"
                )));
            }
        }
        Ok(out)
    }

    fn terminal(&self, event: &Self::Event) -> bool {
        event["type"] == "finish"
    }
}

/// A transport that replays canned SSE `data:` payloads from memory.
struct MockTransport {
    frames: Vec<String>,
}

impl Transport for MockTransport {
    fn frames(&self, _req: PreparedHttp) -> BoxStream<'static, Result<String, LLMError>> {
        let frames = self.frames.clone();
        Box::pin(stream::iter(frames.into_iter().map(Ok)))
    }
}

fn client(frames: Vec<String>) -> LLMClient {
    let route = GenericRoute::new(
        Arc::new(MockProtocol),
        Endpoint::new("https://example.test", "/v1/stream"),
        AuthDef::none(),
        Arc::new(MockTransport { frames }),
    );
    LLMClient::new(Arc::new(route))
}

fn request() -> LLMRequest {
    LLMRequest::new(
        Model::new("mock", "mock-1", "mock"),
        vec![Message::user(vec![ContentPart::text("hi")])],
    )
}

#[tokio::test]
async fn generate_folds_text_stream() {
    let frames = vec![
        r#"{"type":"text","id":"t1","text":"Hello"}"#.to_string(),
        r#"{"type":"text","id":"t1","text":" world"}"#.to_string(),
        r#"{"type":"finish"}"#.to_string(),
    ];
    let response = client(frames).generate(request()).await.expect("generate");

    assert_eq!(response.finish_reason, Some(FinishReason::Stop));
    assert_eq!(
        response.message.content,
        vec![ContentPart::Text {
            text: "Hello world".into(),
            cache: None
        }]
    );
    assert_eq!(response.usage.as_ref().unwrap().output_tokens, Some(3));
}

#[tokio::test]
async fn generate_folds_tool_call_stream() {
    let frames = vec![
        r#"{"type":"tool_start","index":0,"id":"call_1","name":"get_weather"}"#.to_string(),
        r#"{"type":"tool_delta","index":0,"text":"{\"city\":\"paris\"}"}"#.to_string(),
        r#"{"type":"tool_finish","index":0}"#.to_string(),
        r#"{"type":"finish"}"#.to_string(),
    ];
    let response = client(frames).generate(request()).await.expect("generate");

    assert_eq!(response.finish_reason, Some(FinishReason::Stop));
    let tool_call = response
        .message
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
            _ => None,
        })
        .expect("tool call assembled");
    assert_eq!(tool_call.0, "get_weather");
    assert_eq!(tool_call.1["city"], "paris");
}

#[tokio::test]
async fn generate_errors_without_terminal_finish() {
    let frames = vec![r#"{"type":"text","id":"t1","text":"unterminated"}"#.to_string()];
    let err = client(frames).generate(request()).await.unwrap_err();
    assert!(matches!(err, LLMError::NoTerminalFinish));
}

#[tokio::test]
async fn undecodable_frames_are_skipped_not_fatal() {
    // A gateway interleaving garbage (an HTML error page fragment, a comment
    // frame that leaked through) with valid frames: the good events must
    // survive and the turn completes.
    let frames = vec![
        r#"{"type":"text","id":"t1","text":"Hello"}"#.to_string(),
        "<html>502 Bad Gateway</html>".to_string(),
        r#"{"type":"text","id":"t1","text":" world"}"#.to_string(),
        r#"{"type":"finish"}"#.to_string(),
    ];
    let response = client(frames).generate(request()).await.expect("generate");
    assert_eq!(
        response.message.content,
        vec![ContentPart::Text {
            text: "Hello world".into(),
            cache: None
        }]
    );
}

#[tokio::test]
async fn frame_with_two_undelimited_events_is_recovered() {
    // Observed against real Vertex AI traffic: the SSE proxy sometimes omits
    // the blank-line event terminator between two consecutive chunks, so the
    // (spec-compliant) framer joins them into one `data:` frame containing
    // two complete, independently-valid JSON events separated by a bare `\n`
    // — `{"a":1}\n{"b":2}` instead of two separate frames. Both events must
    // still be processed, not dropped as one undecodable frame.
    let frames = vec![
        r#"{"type":"text","id":"t1","text":"Hello"}"#.to_string(),
        format!(
            "{}\n{}",
            r#"{"type":"text","id":"t1","text":" wor"}"#,
            r#"{"type":"text","id":"t1","text":"ld"}"#
        ),
        r#"{"type":"finish"}"#.to_string(),
    ];
    let response = client(frames).generate(request()).await.expect("generate");
    assert_eq!(
        response.message.content,
        vec![ContentPart::Text {
            text: "Hello world".into(),
            cache: None
        }]
    );
}

#[tokio::test]
async fn all_garbage_stream_fails_retryably() {
    // Zero decodable frames must fail as ProviderRetryable (retry with
    // backoff), not as a fatal EventDecode and not as a silent empty attempt.
    let frames = vec![
        "<html>502</html>".to_string(),
        "not json either".to_string(),
    ];
    let err = client(frames).generate(request()).await.unwrap_err();
    assert!(matches!(err, LLMError::ProviderRetryable(_)), "got {err:?}");
}
