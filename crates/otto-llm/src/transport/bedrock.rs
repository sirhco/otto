//! Amazon Bedrock transport: SigV4-signed POST + AWS event-stream decode.
//!
//! Unlike [`HttpTransport`](super::HttpTransport), which speaks plain SSE,
//! [`BedrockTransport`] signs each outgoing request with AWS SigV4
//! ([`sigv4::sign`]) and decodes the AWS binary event-stream framing
//! ([`aws_event_stream::decode`]) instead of SSE framing.
//!
//! # Wire contract
//!
//! Each decoded [`aws_event_stream::DecodedEvent`] is re-wrapped as a
//! single-key JSON string `{"<event_type>":<payload>}` before being handed to
//! the [`Transport`] caller — port of opencode's
//! `bedrock-event-stream.ts:70`. This lets the Bedrock protocol's
//! `BedrockEvent` enum (Task 5) `#[serde(...)]`-tag on that wrapper key
//! directly, the same way other protocols tag on a JSON `type` field.
//! Callers of [`BedrockTransport::frames`] must NOT expect a bare payload
//! string — always a single-key wrapper object.

use std::time::{SystemTime, UNIX_EPOCH};

use futures::stream::{BoxStream, StreamExt, TryStreamExt};

use super::{PreparedHttp, Transport, aws_event_stream};
use crate::error::LLMError;
use crate::protocols::utils::sigv4::{self, AwsCredentials, SignInput};

/// An injectable timestamp source for SigV4 signing.
///
/// SigV4 signatures are bound to the request time (`x-amz-date` / the
/// credential-scope date), so tests need a way to pin it rather than racing
/// the real clock.
#[derive(Debug, Clone, Copy)]
pub enum Clock {
    /// The real wall clock, via `SystemTime::now()`.
    System,
    /// A pinned Unix-seconds timestamp, for deterministic tests.
    Fixed(u64),
}

impl Clock {
    /// The current time as Unix seconds.
    #[must_use]
    pub fn now_unix_secs(&self) -> u64 {
        match self {
            Clock::System => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after the Unix epoch")
                .as_secs(),
            Clock::Fixed(secs) => *secs,
        }
    }
}

/// A `reqwest`-backed transport that SigV4-signs requests to Amazon Bedrock
/// and decodes its binary event-stream response framing.
///
/// Holds concrete [`AwsCredentials`], so — unlike [`HttpTransport`](super::HttpTransport)
/// — it can't be shared as a single provider-wide `Arc`; the route factory
/// builds one per Bedrock route (Task 6).
#[derive(Debug, Clone)]
pub struct BedrockTransport {
    client: reqwest::Client,
    creds: AwsCredentials,
    clock: Clock,
}

impl BedrockTransport {
    /// A transport using a [`reqwest::Client`] configured with the shared
    /// idle/connect timeouts (see [`super::build_client`]) and the real clock.
    #[must_use]
    pub fn new(creds: AwsCredentials) -> Self {
        BedrockTransport {
            client: super::build_client(),
            creds,
            clock: Clock::System,
        }
    }

    /// A transport pinned to a fixed timestamp, for deterministic tests.
    #[must_use]
    pub fn with_fixed_clock(creds: AwsCredentials, secs: u64) -> Self {
        BedrockTransport {
            client: super::build_client(),
            creds,
            clock: Clock::Fixed(secs),
        }
    }
}

/// Derive the `host[:port]` authority from a full request URL.
///
/// Hand-rolled to match [`sigv4::sign`]'s own URL parsing (Bedrock URLs are
/// always `scheme://host/path`): strip the scheme, then take everything up to
/// the first `/`.
fn host_from_url(url: &str) -> Result<String, LLMError> {
    let after_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or_else(|| LLMError::Validation(format!("invalid URL (missing scheme): {url}")))?;
    let host = after_scheme.split('/').next().unwrap_or(after_scheme);
    Ok(host.to_string())
}

impl Transport for BedrockTransport {
    fn frames(&self, req: PreparedHttp) -> BoxStream<'static, Result<String, LLMError>> {
        let client = self.client.clone();
        let creds = self.creds.clone();
        let timestamp_unix_secs = self.clock.now_unix_secs();
        let stream = async_stream::try_stream! {
            let host = host_from_url(&req.url)?;
            let mut headers = req.headers.clone();
            headers.insert("host".to_string(), host);

            let signed = sigv4::sign(&creds, &SignInput {
                method: "POST",
                url: &req.url,
                headers: &headers,
                body: &req.body,
                service: "bedrock",
                timestamp_unix_secs,
            })?;
            headers.extend(signed);

            let mut builder = client.post(&req.url).body(req.body);
            for (name, value) in &headers {
                builder = builder.header(name, value);
            }
            let resp = builder
                .send()
                .await
                .map_err(|e| LLMError::Transport(e.to_string()))?;

            let status = resp.status();
            if !status.is_success() {
                let message = resp.text().await.unwrap_or_default();
                Err(LLMError::Http {
                    status: status.as_u16(),
                    message,
                    retry_after: None,
                })?;
            } else {
                let byte_stream = resp
                    .bytes_stream()
                    .map_err(|e| LLMError::Transport(e.to_string()));
                let events = aws_event_stream::decode(byte_stream);
                futures::pin_mut!(events);
                while let Some(event) = events.next().await {
                    let event = event?;
                    yield format!("{{\"{}\":{}}}", event.event_type, event.payload);
                }
            }
        };
        stream.boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::aws_event_stream::make_frame;
    use std::collections::BTreeMap;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_creds() -> AwsCredentials {
        AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
            region: "us-east-1".into(),
        }
    }

    #[test]
    fn bedrock_transport_new_builds_without_panic() {
        let _transport = BedrockTransport::new(test_creds());
    }

    #[test]
    fn bedrock_transport_with_fixed_clock_builds_without_panic() {
        let _transport = BedrockTransport::with_fixed_clock(test_creds(), 1_700_000_000);
    }

    #[tokio::test]
    async fn signs_posts_and_decodes_event_stream_frames() {
        let f1 = make_frame("contentBlockDelta", r#"{"delta":{"text":"hi"}}"#);
        let f2 = make_frame("messageStop", r#"{"stopReason":"end_turn"}"#);
        let mut body = f1;
        body.extend_from_slice(&f2);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model/x/converse-stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_raw(body, "application/vnd.amazon.eventstream"),
            )
            .mount(&server)
            .await;

        let transport = BedrockTransport::with_fixed_clock(test_creds(), 1_700_000_000);
        let req = PreparedHttp {
            url: format!("{}/model/x/converse-stream", server.uri()),
            headers: BTreeMap::new(),
            body: b"{}".to_vec(),
        };

        let frames: Vec<String> = transport
            .frames(req)
            .map(|frame| frame.expect("frame decodes"))
            .collect()
            .await;

        assert_eq!(frames.len(), 2);
        assert!(
            frames[0].starts_with(r#"{"contentBlockDelta":"#),
            "unexpected wrapper: {}",
            frames[0]
        );
        assert!(
            frames[1].starts_with(r#"{"messageStop":"#),
            "unexpected wrapper: {}",
            frames[1]
        );

        let received = server.received_requests().await.expect("recording enabled");
        assert_eq!(received.len(), 1);
        let request = &received[0];
        let authorization = request
            .headers
            .get("authorization")
            .expect("authorization header present")
            .to_str()
            .unwrap();
        assert!(
            authorization.starts_with("AWS4-HMAC-SHA256"),
            "unexpected authorization header: {authorization}"
        );
        assert!(
            request.headers.contains_key("x-amz-date"),
            "x-amz-date header missing"
        );
    }
}
