//! The tool-augmented event stream — a Rust port of the tool-execution seam in
//! opencode `session/llm/native-runtime.ts:103-140` and the per-call dispatch
//! in `packages/llm/src/tool-runtime.ts:23-76`.
//!
//! [`augment_with_tools`] wraps a provider event stream: every provider event
//! passes through unchanged, but each `tool-call` that the provider did *not*
//! execute itself ALSO triggers a concurrent tool execution. The provider
//! stream is drained first; then the in-flight tool results are appended to the
//! tail (as `tool-result` / `tool-error` events), exactly like native-runtime's
//! `Stream.concat(Stream.fromQueue(results))`. The [`crate::processor::Processor`]
//! pairs the tail results back to their calls by id.

use std::sync::Arc;

use futures::stream::{BoxStream, StreamExt};
use otto_events::{Json, LLMEvent, ToolResultValue};
use otto_llm::LLMError;
use otto_tools::{ToolContext, ToolRegistry};
use serde_json::json;
use tokio::sync::mpsc;

/// Extract the base64 payload of a `data:` URL (the part after the first comma).
/// Mirrors `convert::data_url_payload` (`message-v2.ts:183-186`); duplicated
/// here to keep the module dependency-free.
fn data_url_payload(url: &str) -> &str {
    match url.find(',') {
        Some(idx) => &url[idx + 1..],
        None => url,
    }
}

/// Wrap a provider event stream so that non-provider-executed `tool-call`
/// events are dispatched to the [`ToolRegistry`] concurrently, with their
/// results appended to the tail of the stream.
///
/// Port of the tool-augmentation in `native-runtime.ts:103-140`:
///
/// * Every provider event is forwarded unchanged and immediately (so the
///   processor records the `tool-call` at once — `native-runtime.ts:116-118`).
/// * Each forwarded `tool-call` with `provider_executed != Some(true)` spawns a
///   concurrent [`dispatch`] task (`native-runtime.ts:119-129`).
/// * When the provider stream drains, the runtime awaits every in-flight tool
///   task and appends its `tool-result` / `tool-error` events
///   (`native-runtime.ts:131-137`).
/// * [`ToolContext::abort`] is honored: once cancelled the runtime stops
///   spawning, aborts pending tasks, and appends nothing — leaving the running
///   tool parts for the processor's cleanup to mark interrupted.
///
/// `model_id` is accepted for parity with native-runtime's `StreamInput`; the
/// otto registry gates by model earlier (in `run_loop`) so it is unused here.
#[must_use]
pub fn augment_with_tools(
    provider_stream: BoxStream<'static, Result<LLMEvent, LLMError>>,
    tools: Arc<ToolRegistry>,
    ctx: ToolContext,
    model_id: String,
) -> BoxStream<'static, Result<LLMEvent, LLMError>> {
    let _ = model_id;
    async_stream::stream! {
        let mut provider = provider_stream;
        // Each spawned task sends its result events here; handles let us abort
        // them on cancellation.
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<LLMEvent>>();
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        // -- pass-through phase: forward provider events, spawn tool tasks -----
        loop {
            let item = tokio::select! {
                biased;
                _ = ctx.abort.cancelled() => break,
                item = provider.next() => item,
            };
            let Some(item) = item else { break };

            if let Ok(LLMEvent::ToolCall {
                id,
                name,
                input,
                provider_executed,
                ..
            }) = &item
                && *provider_executed != Some(true)
                && !ctx.abort.is_cancelled()
            {
                let tools = tools.clone();
                let ctx = ctx.clone();
                let tx = tx.clone();
                let id = id.clone();
                let name = name.clone();
                let input = input.clone();
                handles.push(tokio::spawn(async move {
                    let events = dispatch(&tools, &ctx, id, name, input).await;
                    let _ = tx.send(events);
                }));
            }

            yield item;
        }

        // Drop our sender so `rx.recv()` completes once every task's clone drops.
        drop(tx);

        // On cancellation, abort pending tasks and append nothing — the
        // processor's cleanup marks the still-running tool parts interrupted.
        if ctx.abort.is_cancelled() {
            for handle in &handles {
                handle.abort();
            }
            return;
        }

        // -- tail phase: append tool results as they settle -------------------
        loop {
            let events = tokio::select! {
                biased;
                _ = ctx.abort.cancelled() => {
                    for handle in &handles {
                        handle.abort();
                    }
                    return;
                }
                msg = rx.recv() => match msg {
                    Some(events) => events,
                    None => break,
                },
            };
            for event in events {
                yield Ok(event);
            }
        }
    }
    .boxed()
}

/// Execute one canonical tool call and produce its terminal events — port of
/// `ToolRuntime.dispatch` (`tool-runtime.ts:23-76`).
///
/// * Unknown tool → a `tool-error` plus an error `tool-result`
///   (`tool-runtime.ts:25,68-75`).
/// * Success → a single `tool-result` carrying the tool output (with title /
///   metadata folded into a JSON object the processor unpacks, or rich content
///   blocks when the result has attachments) (`tool-runtime.ts:74`).
/// * [`ToolError`](otto_tools::ToolError) → a `tool-error` plus an error
///   `tool-result` (`tool-runtime.ts:31-33,68-73`).
async fn dispatch(
    tools: &ToolRegistry,
    ctx: &ToolContext,
    id: String,
    name: String,
    input: Json,
) -> Vec<LLMEvent> {
    if tools.get(&name).is_none() {
        return error_events(&id, &name, format!("Unknown tool: {name}"));
    }
    match tools.execute(&name, input, ctx).await {
        Ok(result) => {
            let value = if result.attachments.is_empty() {
                // Fold title/metadata/output into an object; the processor's
                // `tool_result_output` extracts them (`processor.ts:255-274`).
                ToolResultValue::Json {
                    value: json!({
                        "title": result.title,
                        "output": result.output,
                        "metadata": result.metadata,
                    }),
                }
            } else {
                let mut blocks: Vec<Json> = Vec::new();
                if !result.output.is_empty() {
                    blocks.push(json!({ "type": "text", "text": result.output }));
                }
                for att in &result.attachments {
                    blocks.push(json!({
                        "type": "media",
                        "mediaType": att.mime,
                        "data": data_url_payload(&att.url),
                    }));
                }
                ToolResultValue::Content { value: blocks }
            };
            vec![LLMEvent::ToolResult {
                id,
                name,
                result: value,
                output: None,
                provider_executed: None,
                provider_metadata: None,
            }]
        }
        Err(err) => error_events(&id, &name, err.to_string()),
    }
}

/// Build the `tool-error` + error `tool-result` pair emitted for a failed or
/// unknown tool call (`tool-runtime.ts:68-73`).
fn error_events(id: &str, name: &str, message: String) -> Vec<LLMEvent> {
    vec![
        LLMEvent::ToolError {
            id: id.to_string(),
            name: name.to_string(),
            message: message.clone(),
            error: None,
            provider_metadata: None,
        },
        LLMEvent::ToolResult {
            id: id.to_string(),
            name: name.to_string(),
            result: ToolResultValue::Error {
                value: Json::String(message),
            },
            output: None,
            provider_executed: None,
            provider_metadata: None,
        },
    ]
}
