//! End-to-end proof over real HTTP (localhost): a built-in provider's
//! `Protocol` + real `HttpTransport` (reqwest) + SSE framing + `LLMClient`,
//! driven against a `wiremock` server that replays a canned `text/event-stream`
//! body. No outbound network — the mock listens on 127.0.0.1.
//!
//! This is the Phase 1 acceptance test: it exercises the whole stack the way a
//! real provider call would, short of hitting the vendor API (see the
//! `#[ignore]`d live smoke test at the bottom for that).

use std::sync::Arc;

use otto_llm::providers::{Anthropic, Bedrock, Google, OpenAI, Provider};
use otto_llm::transport::HttpTransport;
use otto_llm::transport::bedrock::BedrockTransport;
use otto_llm::{AwsCredentials, ContentPart, FinishReason, LLMRequest, Message};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Collect the assembled assistant text from a response's content parts.
fn assistant_text(resp: &otto_llm::LLMResponse) -> String {
    resp.message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Serve `body` as a Server-Sent-Events stream for a single POST to `route`.
async fn sse_server(route: &str, body: &'static str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;
    server
}

fn user_request(model: otto_llm::Model) -> LLMRequest {
    LLMRequest::new(model, vec![Message::user(vec![ContentPart::text("hi")])])
}

#[tokio::test]
async fn anthropic_end_to_end_over_http() {
    // A minimal-but-realistic Anthropic Messages SSE stream.
    const BODY: &str = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let server = sse_server("/messages", BODY).await;

    let provider = Anthropic::new(
        Some(otto_llm::Secret::literal("sk-test")),
        Arc::new(HttpTransport::new()),
    )
    .with_base_url(server.uri());
    let client = provider.client("claude-sonnet-4");

    let resp = client
        .generate(user_request(provider.model("claude-sonnet-4")))
        .await
        .expect("generate over http");

    assert_eq!(assistant_text(&resp), "Hello world");
    assert_eq!(resp.finish_reason, Some(FinishReason::Stop));
    let usage = resp.usage.expect("usage reported");
    // Anthropic sums the breakdown into input_tokens; here only non-cached.
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(2));
}

#[tokio::test]
async fn openai_end_to_end_over_http() {
    // A minimal OpenAI chat.completion.chunk SSE stream, ending with [DONE].
    const BODY: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    let server = sse_server("/chat/completions", BODY).await;

    let provider = OpenAI::new(
        Some(otto_llm::Secret::literal("sk-test")),
        Arc::new(HttpTransport::new()),
    )
    .with_base_url(server.uri());
    let client = provider.client("gpt-4o");

    let resp = client
        .generate(user_request(provider.model("gpt-4o")))
        .await
        .expect("generate over http");

    assert_eq!(assistant_text(&resp), "Hello world");
    assert_eq!(resp.finish_reason, Some(FinishReason::Stop));
    let usage = resp.usage.expect("usage reported");
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(2));
}

#[tokio::test]
async fn google_end_to_end_over_http() {
    // A minimal Gemini streamGenerateContent SSE stream: one text chunk, then
    // a final chunk carrying finishReason + usageMetadata (Gemini never
    // emits finish in-band; it's synthesized by `Gemini::on_halt` once the
    // frame stream ends).
    const BODY: &str = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hi\"}]}}]}\n\n",
        "data: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1,\"totalTokenCount\":2}}\n\n",
    );
    let server = sse_server("/models/gemini-2.0-flash:streamGenerateContent", BODY).await;

    let provider = Google::new(
        Some(otto_llm::Secret::literal("gk-test")),
        Arc::new(HttpTransport::new()),
    )
    .with_base_url(server.uri());
    let client = provider.client("gemini-2.0-flash");

    let resp = client
        .generate(user_request(provider.model("gemini-2.0-flash")))
        .await
        .expect("generate over http");

    assert_eq!(assistant_text(&resp), "hi");
    assert_eq!(resp.finish_reason, Some(FinishReason::Stop));
    let usage = resp.usage.expect("usage reported");
    assert_eq!(usage.input_tokens, Some(1));
    assert_eq!(usage.output_tokens, Some(1));
}

/// Build a single AWS binary event-stream frame carrying a `:event-type`
/// string header and a payload. Local copy of
/// `otto_llm::transport::aws_event_stream`'s `#[cfg(test)] pub(crate)`
/// `make_frame` helper — not visible from an external integration test crate,
/// so the framing logic is duplicated here rather than exposed publicly just
/// for tests.
fn make_frame(event_type: &str, payload: &str) -> Vec<u8> {
    let name = ":event-type";
    let mut headers = Vec::new();
    headers.push(name.len() as u8);
    headers.extend_from_slice(name.as_bytes());
    headers.push(7u8); // string type
    headers.extend_from_slice(&(event_type.len() as u16).to_be_bytes());
    headers.extend_from_slice(event_type.as_bytes());
    let payload_bytes = payload.as_bytes();
    let total = 4 + 4 + 4 + headers.len() + payload_bytes.len() + 4;
    let mut f = Vec::new();
    f.extend_from_slice(&(total as u32).to_be_bytes());
    f.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    f.extend_from_slice(&0u32.to_be_bytes()); // prelude crc (unvalidated)
    f.extend_from_slice(&headers);
    f.extend_from_slice(payload_bytes);
    f.extend_from_slice(&0u32.to_be_bytes()); // message crc (unvalidated)
    f
}

/// Serve `body` as a binary AWS event-stream for a single POST to `route`.
async fn eventstream_server(route: &str, body: Vec<u8>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_raw(body, "application/vnd.amazon.eventstream"),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn bedrock_end_to_end_over_http() {
    // A minimal-but-realistic Converse event-stream: messageStart, two text
    // deltas, contentBlockStop, messageStop (end_turn), then a separate
    // metadata chunk carrying usage (Bedrock splits finish across two
    // frames; `BedrockConverse::on_halt` combines them once the stream
    // ends).
    let mut body = Vec::new();
    body.extend(make_frame("messageStart", r#"{"role":"assistant"}"#));
    body.extend(make_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"text":"Hello"}}"#,
    ));
    body.extend(make_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"text":" world"}}"#,
    ));
    body.extend(make_frame("contentBlockStop", r#"{"contentBlockIndex":0}"#));
    body.extend(make_frame("messageStop", r#"{"stopReason":"end_turn"}"#));
    body.extend(make_frame(
        "metadata",
        r#"{"usage":{"inputTokens":10,"outputTokens":2}}"#,
    ));

    let model_id = "anthropic.claude-3-sonnet";
    let server = eventstream_server(&format!("/model/{model_id}/converse-stream"), body).await;

    let creds = AwsCredentials {
        access_key_id: "AKIDEXAMPLE".into(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
        session_token: None,
        region: "us-east-1".into(),
    };
    let provider = Bedrock::new(creds.clone())
        .with_base_url(server.uri())
        .with_transport(BedrockTransport::with_fixed_clock(creds, 1_700_000_000));
    let client = provider.client(model_id);

    let resp = client
        .generate(user_request(provider.model(model_id)))
        .await
        .expect("generate over http");

    assert_eq!(assistant_text(&resp), "Hello world");
    assert_eq!(resp.finish_reason, Some(FinishReason::Stop));
    let usage = resp.usage.expect("usage reported");
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(2));

    let received = server.received_requests().await.expect("recording enabled");
    assert_eq!(received.len(), 1);
    let authorization = received[0]
        .headers
        .get("authorization")
        .expect("authorization header present")
        .to_str()
        .unwrap();
    assert!(
        authorization.starts_with("AWS4-HMAC-SHA256"),
        "unexpected authorization header: {authorization}"
    );
}

/// Live smoke test against the real Anthropic API. Ignored by default; run with
/// `ANTHROPIC_API_KEY=... cargo test -p otto-llm -- --ignored live_anthropic`.
#[tokio::test]
#[ignore = "hits the real Anthropic API; needs ANTHROPIC_API_KEY"]
async fn live_anthropic_smoke() {
    let Ok(_key) = std::env::var("ANTHROPIC_API_KEY") else {
        eprintln!("ANTHROPIC_API_KEY unset; skipping");
        return;
    };
    // Key is read from the env by the provider's auth fallback.
    let provider = Anthropic::new(None, Arc::new(HttpTransport::new()));
    let client = provider.client("claude-sonnet-4");

    let req = LLMRequest::new(
        provider.model("claude-sonnet-4"),
        vec![Message::user(vec![ContentPart::text(
            "Reply with exactly one word: hi",
        )])],
    );
    let resp = client.generate(req).await.expect("live generate");
    assert!(
        !assistant_text(&resp).trim().is_empty(),
        "expected non-empty completion"
    );
}
