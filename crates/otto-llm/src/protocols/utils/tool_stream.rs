//! The streaming tool-call accumulator.
//!
//! Direct port of opencode `packages/llm/src/protocols/utils/tool-stream.ts`.
//! Providers stream tool-call input incrementally in provider-specific ways;
//! this accumulator normalizes them into `tool-input-start` →
//! `tool-input-delta*` → `tool-input-end` + `tool-call`.
//!
//! Two registration styles are supported:
//! - Anthropic: the tool id/name arrive up front ([`State::start`]) and input
//!   arrives as `input_json_delta` ([`State::append_existing`]).
//! - OpenAI: the id/name may arrive with the first delta, keyed by index
//!   ([`State::append_or_start`]).

use std::collections::HashMap;
use std::hash::Hash;

use otto_events::{Json, LLMEvent};

use crate::error::LLMError;

/// Parse an accumulated tool-input string into JSON.
///
/// Port of the `parseToolInput` helper: an empty string becomes `{}`.
///
/// # Errors
/// Returns [`LLMError::EventDecode`] if a non-empty string is not valid JSON.
pub fn parse_tool_input(input: &str) -> Result<Json, LLMError> {
    if input.trim().is_empty() {
        return Ok(Json::Object(serde_json::Map::new()));
    }
    serde_json::from_str(input)
        .map_err(|e| LLMError::EventDecode(format!("invalid tool input JSON: {e}")))
}

/// A partially-streamed tool call.
///
/// Port of the pending-tool record in `tool-stream.ts`.
#[derive(Debug, Clone)]
pub struct PendingTool {
    /// Tool-call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Accumulated raw input (partial JSON text).
    pub input: String,
    /// Whether the provider executed the tool itself.
    pub provider_executed: Option<bool>,
    /// Provider metadata escape hatch.
    pub provider_metadata: Option<Json>,
    /// Monotonic registration order, for deterministic [`State::finish_all`].
    seq: u64,
}

/// Per-stream tool accumulator, keyed by `K` (a content-block or tool-call
/// index).
///
/// Port of `ToolStream.State` in `tool-stream.ts`.
#[derive(Debug, Clone)]
pub struct State<K: Hash + Eq> {
    pending: HashMap<K, PendingTool>,
    next_seq: u64,
}

impl<K: Hash + Eq> Default for State<K> {
    fn default() -> Self {
        State {
            pending: HashMap::new(),
            next_seq: 0,
        }
    }
}

impl<K: Hash + Eq + Clone> State<K> {
    /// The initial (empty) accumulator.
    #[must_use]
    pub fn initial() -> Self {
        State::default()
    }

    fn register(
        &mut self,
        key: K,
        id: String,
        name: String,
        provider_executed: Option<bool>,
        provider_metadata: Option<Json>,
    ) -> LLMEvent {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.insert(
            key,
            PendingTool {
                id: id.clone(),
                name: name.clone(),
                input: String::new(),
                provider_executed,
                provider_metadata: provider_metadata.clone(),
                seq,
            },
        );
        LLMEvent::ToolInputStart {
            id,
            name,
            provider_metadata,
        }
    }

    /// Register a tool before any input deltas (Anthropic
    /// `content_block_start`). Emits `tool-input-start`. Port of
    /// `ToolStream.start`.
    pub fn start(
        &mut self,
        key: K,
        id: impl Into<String>,
        name: impl Into<String>,
        provider_executed: Option<bool>,
        provider_metadata: Option<Json>,
    ) -> Vec<LLMEvent> {
        vec![self.register(
            key,
            id.into(),
            name.into(),
            provider_executed,
            provider_metadata,
        )]
    }

    /// Append an input delta, registering the tool on first sight (OpenAI:
    /// id/name arrive with the first delta, keyed by index). Emits
    /// `tool-input-start` (only on first registration) then `tool-input-delta`.
    /// Port of `ToolStream.appendOrStart`.
    ///
    /// # Errors
    /// Returns [`LLMError::EventDecode`] if the tool is new but no `id`/`name`
    /// was provided to register it.
    pub fn append_or_start(
        &mut self,
        key: K,
        id: Option<String>,
        name: Option<String>,
        delta: &str,
    ) -> Result<Vec<LLMEvent>, LLMError> {
        let mut out = Vec::new();
        if !self.pending.contains_key(&key) {
            let id = id.ok_or_else(|| {
                LLMError::EventDecode("tool delta for new tool is missing id".to_string())
            })?;
            let name = name.ok_or_else(|| {
                LLMError::EventDecode("tool delta for new tool is missing name".to_string())
            })?;
            out.push(self.register(key.clone(), id, name, None, None));
        }
        let pending = self
            .pending
            .get_mut(&key)
            .expect("pending tool registered above");
        pending.input.push_str(delta);
        out.push(LLMEvent::ToolInputDelta {
            id: pending.id.clone(),
            name: pending.name.clone(),
            text: delta.to_string(),
        });
        Ok(out)
    }

    /// Append an input delta to an already-registered tool (Anthropic
    /// `input_json_delta`). Emits `tool-input-delta`. Port of
    /// `ToolStream.appendExisting`.
    ///
    /// # Errors
    /// Returns [`LLMError::EventDecode`] if no tool is registered under `key`.
    pub fn append_existing(&mut self, key: &K, delta: &str) -> Result<Vec<LLMEvent>, LLMError> {
        let pending = self.pending.get_mut(key).ok_or_else(|| {
            LLMError::EventDecode("tool input delta for unknown tool".to_string())
        })?;
        pending.input.push_str(delta);
        Ok(vec![LLMEvent::ToolInputDelta {
            id: pending.id.clone(),
            name: pending.name.clone(),
            text: delta.to_string(),
        }])
    }

    fn emit_finish(pending: PendingTool, input: Json) -> Vec<LLMEvent> {
        vec![
            LLMEvent::ToolInputEnd {
                id: pending.id.clone(),
                name: pending.name.clone(),
                provider_metadata: pending.provider_metadata.clone(),
            },
            LLMEvent::ToolCall {
                id: pending.id,
                name: pending.name,
                input,
                provider_executed: pending.provider_executed,
                provider_metadata: pending.provider_metadata,
            },
        ]
    }

    /// Finish the tool under `key`, parsing its accumulated input. Emits
    /// `tool-input-end` + `tool-call`. Port of `ToolStream.finish`.
    ///
    /// # Errors
    /// Returns [`LLMError::EventDecode`] if `key` is unknown or its accumulated
    /// input is not valid JSON.
    pub fn finish(&mut self, key: &K) -> Result<Vec<LLMEvent>, LLMError> {
        let pending = self
            .pending
            .remove(key)
            .ok_or_else(|| LLMError::EventDecode("finish for unknown tool".to_string()))?;
        let input = parse_tool_input(&pending.input)?;
        Ok(Self::emit_finish(pending, input))
    }

    /// Finish the tool under `key` using an externally-supplied parsed `input`
    /// (ignoring the accumulated text). Emits `tool-input-end` + `tool-call`.
    /// Port of `ToolStream.finishWithInput`.
    ///
    /// # Errors
    /// Returns [`LLMError::EventDecode`] if `key` is unknown.
    pub fn finish_with_input(&mut self, key: &K, input: Json) -> Result<Vec<LLMEvent>, LLMError> {
        let pending = self
            .pending
            .remove(key)
            .ok_or_else(|| LLMError::EventDecode("finish for unknown tool".to_string()))?;
        Ok(Self::emit_finish(pending, input))
    }

    /// Finish every pending tool in registration order. Port of
    /// `ToolStream.finishAll`.
    ///
    /// # Errors
    /// Returns [`LLMError::EventDecode`] if any accumulated input is invalid.
    pub fn finish_all(&mut self) -> Result<Vec<LLMEvent>, LLMError> {
        let mut drained: Vec<(K, PendingTool)> = self.pending.drain().collect();
        drained.sort_by_key(|(_, p)| p.seq);
        let mut out = Vec::new();
        for (_, pending) in drained {
            let input = parse_tool_input(&pending.input)?;
            out.extend(Self::emit_finish(pending, input));
        }
        Ok(out)
    }

    /// Whether any tool is still pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Whether a tool is currently registered under `key`. Lets a caller
    /// avoid re-registering (and losing accumulated input for) a tool that
    /// was already started, when a provider's stream grammar allows a
    /// `start`-equivalent event to arrive redundantly (e.g. OpenAI Responses'
    /// `output_item.done` falls back to registering a tool that should
    /// already exist from `output_item.added`).
    #[must_use]
    pub fn contains(&self, key: &K) -> bool {
        self.pending.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(events: &[LLMEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                LLMEvent::ToolInputStart { .. } => "tool-input-start",
                LLMEvent::ToolInputDelta { .. } => "tool-input-delta",
                LLMEvent::ToolInputEnd { .. } => "tool-input-end",
                LLMEvent::ToolCall { .. } => "tool-call",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn parse_empty_input_is_object() {
        assert_eq!(
            parse_tool_input("").unwrap(),
            Json::Object(Default::default())
        );
        assert_eq!(
            parse_tool_input("   ").unwrap(),
            Json::Object(Default::default())
        );
    }

    #[test]
    fn anthropic_start_delta_finish() {
        let mut s = State::<u64>::initial();
        let mut all = Vec::new();
        all.extend(s.start(0, "call_1", "get_weather", None, None));
        all.extend(s.append_existing(&0, "{\"city\":").unwrap());
        all.extend(s.append_existing(&0, "\"paris\"}").unwrap());
        all.extend(s.finish(&0).unwrap());
        assert_eq!(
            types(&all),
            [
                "tool-input-start",
                "tool-input-delta",
                "tool-input-delta",
                "tool-input-end",
                "tool-call"
            ]
        );
        // final tool-call carries the parsed input.
        match all.last().unwrap() {
            LLMEvent::ToolCall { input, name, .. } => {
                assert_eq!(name, "get_weather");
                assert_eq!(input["city"], "paris");
            }
            _ => panic!("expected tool-call"),
        }
    }

    #[test]
    fn openai_append_or_start_registers_on_first_delta() {
        let mut s = State::<u64>::initial();
        let mut all = Vec::new();
        all.extend(
            s.append_or_start(0, Some("c1".into()), Some("f".into()), "{\"a\":1}")
                .unwrap(),
        );
        all.extend(s.append_or_start(0, None, None, "").unwrap());
        all.extend(s.finish_all().unwrap());
        assert_eq!(
            types(&all),
            [
                "tool-input-start",
                "tool-input-delta",
                "tool-input-delta",
                "tool-input-end",
                "tool-call"
            ]
        );
    }

    #[test]
    fn append_existing_unknown_errors() {
        let mut s = State::<u64>::initial();
        assert!(s.append_existing(&7, "x").is_err());
    }
}
