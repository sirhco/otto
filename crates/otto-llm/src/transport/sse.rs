//! Server-Sent Events framing.
//!
//! Port of the `sseFraming` helper in opencode
//! `packages/llm/src/route/transport/shared.ts`: split a byte stream on blank
//! lines, extract `data:` fields (concatenating multi-line data with `\n`), and
//! yield each payload — skipping empty payloads and the `[DONE]` sentinel.

use bytes::Bytes;
use futures::stream::{Stream, StreamExt};

use crate::error::LLMError;

/// Incremental SSE line decoder over UTF-8 byte chunks.
///
/// Port of the framing state in `shared.ts`. Feed chunks with [`SseDecoder::push`]
/// and drain any trailing event with [`SseDecoder::flush`].
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: String,
}

impl SseDecoder {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> Self {
        SseDecoder::default()
    }

    /// Push a byte chunk, returning any completed `data:` payloads.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let block: String = self.buf.drain(..pos + 2).collect();
            if let Some(payload) = parse_block(&block) {
                out.push(payload);
            }
        }
        out
    }

    /// Drain any trailing event that was not terminated by a blank line.
    pub fn flush(&mut self) -> Vec<String> {
        if self.buf.trim().is_empty() {
            self.buf.clear();
            return Vec::new();
        }
        let block = std::mem::take(&mut self.buf);
        parse_block(&block).into_iter().collect()
    }
}

/// Extract the `data:` payload from a single SSE event block, or `None` if the
/// block carries no usable data. Multi-line `data:` fields are joined with
/// `\n`; empty payloads and `[DONE]` are filtered out (`sseFraming`).
fn parse_block(block: &str) -> Option<String> {
    let mut data_lines: Vec<&str> = Vec::new();
    for raw in block.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    let data = data_lines.join("\n");
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    Some(data)
}

/// Decode an SSE byte stream into a stream of `data:` payload strings.
///
/// Port of `sseFraming` in `shared.ts`. Empty payloads and `[DONE]` are
/// dropped.
pub fn decode_sse<S>(stream: S) -> impl Stream<Item = Result<String, LLMError>> + Send
where
    S: Stream<Item = Result<Bytes, LLMError>> + Send + 'static,
{
    async_stream::try_stream! {
        // Opt-in wire diagnostic: `otto_DEBUG_STREAM=1` dumps every raw SSE
        // chunk (verbatim, including the finish_reason chunk, `[DONE]`, and any
        // provider `{"error":...}` frame) plus stream-termination markers to
        // stderr. Off by default (single env lookup, no overhead).
        let debug = std::env::var_os("otto_DEBUG_STREAM").is_some();
        let mut decoder = SseDecoder::new();
        futures::pin_mut!(stream);
        while let Some(chunk) = stream.next().await {
            if debug {
                match &chunk {
                    Ok(b) => eprint!("[otto-stream] {}", String::from_utf8_lossy(b)),
                    Err(e) => eprintln!("\n[otto-stream] TRANSPORT ERROR: {e}"),
                }
            }
            let bytes = chunk?;
            for payload in decoder.push(&bytes) {
                yield payload;
            }
        }
        for payload in decoder.flush() {
            yield payload;
        }
        if debug {
            eprintln!("\n[otto-stream] <stream closed>");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    #[test]
    fn decoder_handles_multiple_events_multiline_and_done() {
        let mut d = SseDecoder::new();
        let mut out = d.push(b"data: {\"a\":1}\n\n");
        out.extend(d.push(b"data: line1\ndata: line2\n\n"));
        out.extend(d.push(b"data: [DONE]\n\n"));
        assert_eq!(
            out,
            vec!["{\"a\":1}".to_string(), "line1\nline2".to_string()]
        );
    }

    #[test]
    fn decoder_handles_split_chunks() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: {\"partial\":").is_empty());
        let out = d.push(b"true}\n\n");
        assert_eq!(out, vec!["{\"partial\":true}".to_string()]);
    }

    #[test]
    fn decoder_flushes_unterminated_event() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: tail").is_empty());
        assert_eq!(d.flush(), vec!["tail".to_string()]);
    }

    #[tokio::test]
    async fn decode_sse_stream() {
        let chunks: Vec<Result<Bytes, LLMError>> = vec![
            Ok(Bytes::from_static(b"data: a\n\ndata: b\n")),
            Ok(Bytes::from_static(b"\ndata: [DONE]\n\n")),
        ];
        let frames: Vec<String> = decode_sse(stream::iter(chunks))
            .map(Result::unwrap)
            .collect()
            .await;
        assert_eq!(frames, vec!["a".to_string(), "b".to_string()]);
    }
}
