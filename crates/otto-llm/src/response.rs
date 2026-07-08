//! Response assembly.
//!
//! Port of `LLMResponse.empty` / `reduce` / `complete` in opencode
//! `packages/llm/src/schema/events.ts` (lines ~382, ~561-618): fold a stream of
//! [`LLMEvent`]s into a single assembled assistant [`Message`] plus the raw
//! event log, usage, and finish reason.

use otto_events::{FinishReason, LLMEvent, ToolResultValue, Usage};

use crate::message::{ContentPart, Message, Role};

/// The accumulated result of an LLM stream.
///
/// Port of the `LLMResponse` shape in `events.ts`. [`LLMResponse::complete`]
/// returns `None` until a terminal `finish` event sets [`finish_reason`].
///
/// [`finish_reason`]: LLMResponse::finish_reason
#[derive(Debug, Clone, PartialEq)]
pub struct LLMResponse {
    /// The assembled assistant message.
    pub message: Message,
    /// The raw event log, in arrival order.
    pub events: Vec<LLMEvent>,
    /// Aggregate token usage, if reported.
    pub usage: Option<Usage>,
    /// The terminal finish reason, if the stream finished.
    pub finish_reason: Option<FinishReason>,
    // Index of the in-progress text content part, keyed by block id.
    #[doc(hidden)]
    text_blocks: Vec<(String, usize)>,
    // Index of the in-progress reasoning content part, keyed by block id.
    #[doc(hidden)]
    reasoning_blocks: Vec<(String, usize)>,
}

impl LLMResponse {
    /// An empty response accumulator (`LLMResponse.empty`).
    #[must_use]
    pub fn empty() -> Self {
        LLMResponse {
            message: Message {
                id: None,
                role: Role::Assistant,
                content: Vec::new(),
                native: None,
            },
            events: Vec::new(),
            usage: None,
            finish_reason: None,
            text_blocks: Vec::new(),
            reasoning_blocks: Vec::new(),
        }
    }

    fn text_index(&mut self, id: &str) -> usize {
        if let Some((_, idx)) = self.text_blocks.iter().find(|(bid, _)| bid == id) {
            return *idx;
        }
        let idx = self.message.content.len();
        self.message.content.push(ContentPart::Text {
            text: String::new(),
            cache: None,
        });
        self.text_blocks.push((id.to_string(), idx));
        idx
    }

    fn reasoning_index(&mut self, id: &str) -> usize {
        if let Some((_, idx)) = self.reasoning_blocks.iter().find(|(bid, _)| bid == id) {
            return *idx;
        }
        let idx = self.message.content.len();
        self.message.content.push(ContentPart::Reasoning {
            text: String::new(),
            encrypted: None,
        });
        self.reasoning_blocks.push((id.to_string(), idx));
        idx
    }

    /// Fold one event into the response (`LLMResponse.reduce`).
    ///
    /// Text/reasoning deltas are collapsed into their assembled content parts;
    /// tool calls/results/errors are appended as parts; `finish` records the
    /// terminal reason and usage.
    #[must_use]
    pub fn reduce(mut self, event: LLMEvent) -> Self {
        match &event {
            LLMEvent::TextStart { id, .. } => {
                self.text_index(id);
            }
            LLMEvent::TextDelta { id, text, .. } => {
                let idx = self.text_index(id);
                if let ContentPart::Text { text: acc, .. } = &mut self.message.content[idx] {
                    acc.push_str(text);
                }
            }
            LLMEvent::ReasoningStart { id, .. } => {
                self.reasoning_index(id);
            }
            LLMEvent::ReasoningDelta { id, text, .. } => {
                let idx = self.reasoning_index(id);
                if let ContentPart::Reasoning { text: acc, .. } = &mut self.message.content[idx] {
                    acc.push_str(text);
                }
            }
            LLMEvent::ToolCall {
                id,
                name,
                input,
                provider_executed,
                ..
            } => {
                self.message.content.push(ContentPart::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    provider_executed: *provider_executed,
                });
            }
            LLMEvent::ToolResult {
                id,
                name,
                result,
                provider_executed,
                ..
            } => {
                self.message.content.push(ContentPart::ToolResult {
                    id: id.clone(),
                    name: name.clone(),
                    result: result.clone(),
                    provider_executed: *provider_executed,
                    cache: None,
                });
            }
            LLMEvent::ToolError {
                id, name, message, ..
            } => {
                self.message.content.push(ContentPart::ToolResult {
                    id: id.clone(),
                    name: name.clone(),
                    result: ToolResultValue::Error {
                        value: serde_json::Value::String(message.clone()),
                    },
                    provider_executed: None,
                    cache: None,
                });
            }
            LLMEvent::StepFinish {
                usage: Some(usage), ..
            } => {
                self.usage = Some(usage.clone());
            }
            LLMEvent::Finish { reason, usage, .. } => {
                self.finish_reason = Some(*reason);
                if let Some(usage) = usage {
                    self.usage = Some(usage.clone());
                }
            }
            // text-end / reasoning-end / tool-input-* / step-start /
            // provider-error carry no message content to assemble.
            _ => {}
        }
        self.events.push(event);
        self
    }

    /// The response if it has terminated (`LLMResponse.complete`), else `None`.
    #[must_use]
    pub fn complete(&self) -> Option<&LLMResponse> {
        if self.finish_reason.is_some() {
            Some(self)
        } else {
            None
        }
    }
}

impl Default for LLMResponse {
    fn default() -> Self {
        LLMResponse::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_scripted_stream_into_message() {
        let events = vec![
            LLMEvent::StepStart { index: 0 },
            LLMEvent::TextStart {
                id: "t1".into(),
                provider_metadata: None,
            },
            LLMEvent::TextDelta {
                id: "t1".into(),
                text: "Hello".into(),
                provider_metadata: None,
            },
            LLMEvent::TextDelta {
                id: "t1".into(),
                text: " world".into(),
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
                    output_tokens: Some(2),
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

        // incomplete until the finish event.
        let partial = LLMResponse::empty().reduce(events[0].clone());
        assert!(partial.complete().is_none());

        let response = events
            .into_iter()
            .fold(LLMResponse::empty(), LLMResponse::reduce);
        let done = response.complete().expect("finished");
        assert_eq!(done.finish_reason, Some(FinishReason::Stop));
        assert_eq!(
            done.message.content,
            vec![ContentPart::Text {
                text: "Hello world".into(),
                cache: None
            }]
        );
        assert_eq!(done.usage.as_ref().unwrap().output_tokens, Some(2));
    }
}
