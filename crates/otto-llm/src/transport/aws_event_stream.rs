//! AWS binary event-stream framing.
//!
//! Port of the `consumeFrames` helper referenced by opencode's Bedrock
//! transport (`bedrock-event-stream.ts:35-74`). Amazon Bedrock's
//! `ConverseStream` response is not SSE — it is AWS's binary
//! `application/vnd.amazon.eventstream` framing. Each frame is:
//!
//! ```text
//! [total_len: u32][headers_len: u32][prelude_crc: u32][headers][payload][message_crc: u32]
//! ```
//!
//! all big-endian. Headers are a repeated sequence of
//! `[name_len: u8][name][value_type: u8][value...]`; only string-typed
//! (`value_type == 7`) header values are `[value_len: u16][value bytes]` —
//! other value types are skipped over by their known width so header parsing
//! stays in sync even though this decoder only cares about the string
//! `:event-type` header (with `:exception-type` used as the fallback for
//! modeled streaming-exception frames).
//!
//! **CRC validation is intentionally skipped** for this MVP: `prelude_crc`
//! and `message_crc` are read but never checked against the frame bytes.
//! This is a known gap — a corrupted frame that still carries a plausible
//! `total_len` will be parsed as if valid.

use bytes::Bytes;
use futures::stream::{Stream, StreamExt};

use crate::error::LLMError;

/// A single decoded event-stream message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedEvent {
    /// The `:event-type` header value (falls back to `:exception-type` for
    /// modeled streaming-exception frames, which carry no `:event-type`).
    pub event_type: String,
    /// The UTF-8 decoded payload, with any top-level `"p"` padding field
    /// stripped from JSON payloads.
    pub payload: String,
}

/// Minimum bytes needed to read the frame prelude (`total_len` +
/// `headers_len` + `prelude_crc`).
const PRELUDE_LEN: usize = 12;
/// Trailing message CRC width.
const MESSAGE_CRC_LEN: usize = 4;

/// Incremental AWS event-stream frame decoder over binary byte chunks.
///
/// Port of the framing state in `consumeFrames` (`bedrock-event-stream.ts`).
/// Feed chunks with [`EventStreamDecoder::push`] and drain any trailing
/// partial state with [`EventStreamDecoder::finish`].
#[derive(Debug, Default)]
pub struct EventStreamDecoder {
    buf: Vec<u8>,
}

impl EventStreamDecoder {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> Self {
        EventStreamDecoder::default()
    }

    /// Push a byte chunk, returning any completed frames.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<DecodedEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < PRELUDE_LEN {
                break;
            }
            // `total_len` is trusted here: a hostile prelude could request an
            // unbounded buffer (no cap enforced). Acceptable for the trusted
            // AWS Bedrock endpoint; a size cap is a documented follow-up.
            let total_len = u32::from_be_bytes(self.buf[0..4].try_into().unwrap()) as usize;
            if total_len < PRELUDE_LEN + MESSAGE_CRC_LEN || self.buf.len() < total_len {
                // Either a malformed prelude or not enough bytes buffered yet;
                // wait for more data rather than desyncing.
                break;
            }
            let frame: Vec<u8> = self.buf.drain(..total_len).collect();
            if let Some(event) = parse_frame(&frame) {
                out.push(event);
            }
        }
        out
    }

    /// Drain any trailing, incomplete frame state.
    ///
    /// A truncated final frame carries no usable event, so this simply
    /// clears the buffer and returns nothing.
    pub fn finish(&mut self) -> Vec<DecodedEvent> {
        self.buf.clear();
        Vec::new()
    }
}

/// AWS event-stream header value types (`bedrock-event-stream.ts`'s
/// `HEADER_VALUE_TYPE` table). Only `String` (7) is extracted; the rest are
/// skipped by their known width.
#[allow(dead_code)]
enum HeaderValueType {
    True = 0,
    False = 1,
    Byte = 2,
    Short = 3,
    Integer = 4,
    Long = 5,
    ByteArray = 6,
    String = 7,
    Timestamp = 8,
    Uuid = 9,
}

/// Parse one complete frame (already sliced to exactly `total_len` bytes)
/// into a [`DecodedEvent`]. Returns `None` if the frame carries no usable
/// event type or payload.
fn parse_frame(frame: &[u8]) -> Option<DecodedEvent> {
    if frame.len() < PRELUDE_LEN + MESSAGE_CRC_LEN {
        return None;
    }
    let headers_len = u32::from_be_bytes(frame[4..8].try_into().ok()?) as usize;
    // prelude_crc at frame[8..12] — intentionally unvalidated.

    let headers_start = PRELUDE_LEN;
    let headers_end = headers_start.checked_add(headers_len)?;
    let payload_end = frame.len().checked_sub(MESSAGE_CRC_LEN)?;
    if headers_end > payload_end || payload_end > frame.len() {
        return None;
    }
    // message_crc at frame[payload_end..] — intentionally unvalidated.

    let headers = parse_headers(&frame[headers_start..headers_end]);
    let payload_bytes = &frame[headers_end..payload_end];

    // Resolve the event type from `:event-type`, falling back to
    // `:exception-type` for modeled streaming exceptions (which carry
    // `:message-type: exception` + `:exception-type: <name>` and NO
    // `:event-type`). `:message-type` is deliberately NOT in this fallback:
    // its only values are `"event"`/`"exception"`, never a real event name,
    // so using it would resolve exception frames to the literal `"exception"`
    // and the downstream Bedrock reducer could not match them (dropping the
    // retryable-throttling / context-overflow classification signals).
    let event_type = headers
        .iter()
        .find(|(name, _)| name == ":event-type")
        .or_else(|| headers.iter().find(|(name, _)| name == ":exception-type"))
        .map(|(_, value)| value.clone())?;

    let payload = String::from_utf8_lossy(payload_bytes).into_owned();
    let payload = strip_padding_field(&payload);

    Some(DecodedEvent {
        event_type,
        payload,
    })
}

/// Parse the header block into `(name, value)` pairs, extracting string
/// values and skipping over every other value type by its known width.
///
/// Never indexes past `bytes`: any header entry that would overrun the
/// buffer stops parsing and returns whatever headers were already
/// extracted, rather than panicking.
fn parse_headers(bytes: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        // name_len: u8
        let Some(&name_len) = bytes.get(pos) else {
            break;
        };
        let name_len = name_len as usize;
        pos += 1;

        let Some(name_bytes) = bytes.get(pos..pos + name_len) else {
            break;
        };
        pos += name_len;

        let Some(&value_type) = bytes.get(pos) else {
            break;
        };
        pos += 1;

        match value_type {
            0 | 1 => {
                // true/false: no value bytes.
            }
            2 => pos += 1,  // byte
            3 => pos += 2,  // short
            4 => pos += 4,  // integer
            5 => pos += 8,  // long
            8 => pos += 8,  // timestamp (i64 millis)
            9 => pos += 16, // uuid
            6 | 7 => {
                // byte-array / string: [len: u16][bytes]
                let Some(len_bytes) = bytes.get(pos..pos + 2) else {
                    break;
                };
                let value_len = u16::from_be_bytes(len_bytes.try_into().unwrap()) as usize;
                pos += 2;
                let Some(value_bytes) = bytes.get(pos..pos + value_len) else {
                    break;
                };
                pos += value_len;
                if value_type == 7 {
                    let name = String::from_utf8_lossy(name_bytes).into_owned();
                    let value = String::from_utf8_lossy(value_bytes).into_owned();
                    out.push((name, value));
                }
            }
            _ => {
                // Unknown value type: cannot safely determine width, stop
                // parsing headers rather than desyncing further.
                break;
            }
        }

        if pos > bytes.len() {
            break;
        }
    }
    out
}

/// Strip a top-level `"p"` padding field from a JSON payload, if present.
///
/// AWS occasionally pads Bedrock event payloads with a `"p"` field of
/// arbitrary filler characters (port of `bedrock-event-stream.ts:70`). Any
/// payload that is not a JSON object, or has no `"p"` field, is returned
/// unchanged.
fn strip_padding_field(payload: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return payload.to_string();
    };
    let Some(obj) = value.as_object_mut() else {
        return payload.to_string();
    };
    if obj.remove("p").is_none() {
        return payload.to_string();
    }
    serde_json::to_string(&value).unwrap_or_else(|_| payload.to_string())
}

/// Decode an AWS binary event-stream byte stream into a stream of
/// [`DecodedEvent`]s.
///
/// Port of `consumeFrames` (`bedrock-event-stream.ts`). Mirrors
/// [`crate::transport::sse::decode_sse`]'s shape.
pub fn decode<S>(stream: S) -> impl Stream<Item = Result<DecodedEvent, LLMError>> + Send
where
    S: Stream<Item = Result<Bytes, LLMError>> + Send + 'static,
{
    async_stream::try_stream! {
        let mut decoder = EventStreamDecoder::new();
        futures::pin_mut!(stream);
        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            for event in decoder.push(&bytes) {
                yield event;
            }
        }
        for event in decoder.finish() {
            yield event;
        }
    }
}

/// Build a single AWS event-stream frame carrying just a `:event-type`
/// string header and a payload.
///
/// Shared test helper: also reused by [`crate::transport::bedrock`]'s tests
/// to build a fake `converse-stream` response body.
#[cfg(test)]
pub(crate) fn make_frame(event_type: &str, payload: &str) -> Vec<u8> {
    // one string header ":event-type"
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    /// A frame with an extra non-string header (an `Integer` value) before
    /// the `:event-type` string header, to exercise skip-by-width parsing.
    fn make_frame_with_extra_headers(event_type: &str, payload: &str) -> Vec<u8> {
        let mut headers = Vec::new();
        // bool header
        let bname = ":bool-header";
        headers.push(bname.len() as u8);
        headers.extend_from_slice(bname.as_bytes());
        headers.push(0u8); // true, no value bytes

        // integer header
        let iname = ":int-header";
        headers.push(iname.len() as u8);
        headers.extend_from_slice(iname.as_bytes());
        headers.push(4u8); // integer type
        headers.extend_from_slice(&42i32.to_be_bytes());

        // byte-array header
        let baname = ":ba-header";
        headers.push(baname.len() as u8);
        headers.extend_from_slice(baname.as_bytes());
        headers.push(6u8); // byte array
        let ba_val = [1u8, 2, 3];
        headers.extend_from_slice(&(ba_val.len() as u16).to_be_bytes());
        headers.extend_from_slice(&ba_val);

        // string header
        let name = ":event-type";
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7u8);
        headers.extend_from_slice(&(event_type.len() as u16).to_be_bytes());
        headers.extend_from_slice(event_type.as_bytes());

        let payload_bytes = payload.as_bytes();
        let total = 4 + 4 + 4 + headers.len() + payload_bytes.len() + 4;
        let mut f = Vec::new();
        f.extend_from_slice(&(total as u32).to_be_bytes());
        f.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        f.extend_from_slice(&0u32.to_be_bytes());
        f.extend_from_slice(&headers);
        f.extend_from_slice(payload_bytes);
        f.extend_from_slice(&0u32.to_be_bytes());
        f
    }

    #[test]
    fn decodes_two_frames_across_chunk_boundary() {
        let f1 = make_frame("contentBlockDelta", r#"{"delta":{"text":"hi"}}"#);
        let f2 = make_frame("messageStop", r#"{"stopReason":"end_turn"}"#);
        let mut all = f1.clone();
        all.extend_from_slice(&f2);
        let mut dec = EventStreamDecoder::default();
        // split mid-first-frame to exercise buffering
        let split = f1.len() - 3;
        let mut out = dec.push(&all[..split]);
        out.extend(dec.push(&all[split..]));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event_type, "contentBlockDelta");
        assert!(out[0].payload.contains("hi"));
        assert_eq!(out[1].event_type, "messageStop");
    }

    #[test]
    fn decodes_partial_frame_delivered_in_two_pushes() {
        let frame = make_frame("messageStart", r#"{"role":"assistant"}"#);
        let mut dec = EventStreamDecoder::default();
        let split = frame.len() / 2;
        let out = dec.push(&frame[..split]);
        assert!(
            out.is_empty(),
            "no event should decode from a partial frame"
        );
        let out = dec.push(&frame[split..]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event_type, "messageStart");
        assert!(out[0].payload.contains("assistant"));
    }

    #[test]
    fn decodes_frame_one_byte_at_a_time() {
        let frame = make_frame("ping", r#"{"ok":true}"#);
        let mut dec = EventStreamDecoder::default();
        let mut out = Vec::new();
        for byte in &frame {
            out.extend(dec.push(std::slice::from_ref(byte)));
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event_type, "ping");
    }

    /// Build a frame from an arbitrary list of `(name, value)` string
    /// headers (all `value_type == 7`), for header-resolution tests.
    fn make_frame_with_string_headers(headers_kv: &[(&str, &str)], payload: &str) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in headers_kv {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7u8); // string type
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let payload_bytes = payload.as_bytes();
        let total = 4 + 4 + 4 + headers.len() + payload_bytes.len() + 4;
        let mut f = Vec::new();
        f.extend_from_slice(&(total as u32).to_be_bytes());
        f.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        f.extend_from_slice(&0u32.to_be_bytes());
        f.extend_from_slice(&headers);
        f.extend_from_slice(payload_bytes);
        f.extend_from_slice(&0u32.to_be_bytes());
        f
    }

    #[test]
    fn resolves_exception_frame_by_exception_type_not_message_type() {
        // A modeled streaming exception carries `:message-type: exception`
        // + `:exception-type: <name>` and NO `:event-type`. It must resolve
        // to the exception name (so the Bedrock reducer can match it and
        // preserve the retryable classification), NOT the literal
        // `"exception"` from `:message-type`.
        let frame = make_frame_with_string_headers(
            &[
                (":message-type", "exception"),
                (":exception-type", "throttlingException"),
            ],
            r#"{"message":"Too many requests"}"#,
        );
        let mut dec = EventStreamDecoder::default();
        let out = dec.push(&frame);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event_type, "throttlingException");
    }

    #[test]
    fn event_type_wins_over_message_type_for_normal_frame() {
        // A normal event carries both `:message-type: event` and
        // `:event-type: contentBlockDelta`; `:event-type` must win.
        let frame = make_frame_with_string_headers(
            &[
                (":message-type", "event"),
                (":event-type", "contentBlockDelta"),
            ],
            r#"{"delta":{"text":"hi"}}"#,
        );
        let mut dec = EventStreamDecoder::default();
        let out = dec.push(&frame);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event_type, "contentBlockDelta");
    }

    #[test]
    fn skips_non_string_headers_without_desyncing() {
        let frame = make_frame_with_extra_headers("contentBlockDelta", r#"{"delta":"x"}"#);
        let mut dec = EventStreamDecoder::default();
        let out = dec.push(&frame);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event_type, "contentBlockDelta");
        assert_eq!(out[0].payload, r#"{"delta":"x"}"#);
    }

    #[test]
    fn strips_top_level_padding_field() {
        let frame = make_frame("messageStop", r#"{"stopReason":"end_turn","p":"xxxxxx"}"#);
        let mut dec = EventStreamDecoder::default();
        let out = dec.push(&frame);
        assert_eq!(out.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&out[0].payload).unwrap();
        assert!(value.get("p").is_none());
        assert_eq!(value.get("stopReason").unwrap(), "end_turn");
    }

    #[test]
    fn finish_clears_incomplete_trailing_state() {
        let frame = make_frame("messageStop", r#"{"stopReason":"end_turn"}"#);
        let mut dec = EventStreamDecoder::default();
        dec.push(&frame[..frame.len() - 3]);
        let out = dec.finish();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn decode_stream_yields_events_in_order() {
        let f1 = make_frame("messageStart", r#"{"role":"assistant"}"#);
        let f2 = make_frame("messageStop", r#"{"stopReason":"end_turn"}"#);
        let chunks: Vec<Result<Bytes, LLMError>> = vec![Ok(Bytes::from(f1)), Ok(Bytes::from(f2))];
        let events: Vec<DecodedEvent> = decode(stream::iter(chunks))
            .map(Result::unwrap)
            .collect()
            .await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "messageStart");
        assert_eq!(events[1].event_type, "messageStop");
    }
}
