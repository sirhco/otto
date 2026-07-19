//! The step/text/reasoning lifecycle state machine.
//!
//! Direct port of opencode `packages/llm/src/protocols/utils/lifecycle.ts`.
//! Both concrete protocols depend on this for correct [`LLMEvent`] ordering:
//!
//! - `step-start` is emitted exactly once.
//! - the first `text-delta` for an id emits `text-start` before it; likewise
//!   `reasoning-start` before the first `reasoning-delta`.
//! - `finish` closes every still-open text/reasoning block (emitting the
//!   matching `*-end`) and *then* emits `step-finish` followed by `finish`.

use std::collections::HashSet;

use otto_events::{FinishReason, LLMEvent, ProviderMetadata, Usage};

/// Per-stream lifecycle accumulator.
///
/// Port of `Lifecycle.State` in `lifecycle.ts`.
#[derive(Debug, Clone, Default)]
pub struct State {
    /// Whether `step-start` has already been emitted.
    step_started: bool,
    /// Ids of currently-open text blocks.
    text: HashSet<String>,
    /// Ids of currently-open reasoning blocks.
    reasoning: HashSet<String>,
}

impl State {
    /// The initial lifecycle state (`Lifecycle.initial`).
    #[must_use]
    pub fn initial() -> Self {
        State::default()
    }

    /// Emit `step-start` the first time it is called; subsequent calls emit
    /// nothing. Port of `Lifecycle.stepStart`.
    pub fn step_start(&mut self, index: u64) -> Vec<LLMEvent> {
        if self.step_started {
            return Vec::new();
        }
        self.step_started = true;
        vec![LLMEvent::StepStart { index }]
    }

    /// Whether `step_start` has ever been called (i.e. the stream produced at
    /// least one observable chunk). Used by reducers whose `on_halt` must
    /// distinguish "stream produced nothing at all" (emit nothing) from "the
    /// stream opened a step but ended before a terminal event" (still emit a
    /// closing finish).
    #[must_use]
    pub fn is_started(&self) -> bool {
        self.step_started
    }

    /// Emit a text delta for `id`, opening the block with `text-start` on the
    /// first delta. Port of `Lifecycle.textDelta`.
    pub fn text_delta(&mut self, id: &str, text: impl Into<String>) -> Vec<LLMEvent> {
        let mut out = Vec::new();
        if self.text.insert(id.to_string()) {
            out.push(LLMEvent::TextStart {
                id: id.to_string(),
                provider_metadata: None,
            });
        }
        out.push(LLMEvent::TextDelta {
            id: id.to_string(),
            text: text.into(),
            provider_metadata: None,
        });
        out
    }

    /// Close the text block `id` with `text-end` if it is open. Port of
    /// `Lifecycle.textEnd`.
    pub fn text_end(&mut self, id: &str) -> Vec<LLMEvent> {
        if self.text.remove(id) {
            vec![LLMEvent::TextEnd {
                id: id.to_string(),
                provider_metadata: None,
            }]
        } else {
            Vec::new()
        }
    }

    /// Emit a reasoning delta for `id`, opening the block with
    /// `reasoning-start` on the first delta. Port of `Lifecycle.reasoningDelta`.
    pub fn reasoning_delta(&mut self, id: &str, text: impl Into<String>) -> Vec<LLMEvent> {
        let mut out = Vec::new();
        if self.reasoning.insert(id.to_string()) {
            out.push(LLMEvent::ReasoningStart {
                id: id.to_string(),
                provider_metadata: None,
            });
        }
        out.push(LLMEvent::ReasoningDelta {
            id: id.to_string(),
            text: text.into(),
            provider_metadata: None,
        });
        out
    }

    /// Idempotently open a reasoning block with no delta text yet, attaching
    /// optional provider metadata to the `reasoning-start` event.
    ///
    /// Port of `Lifecycle.reasoningStart` (`lifecycle.ts:27-37`). Unlike
    /// [`State::reasoning_delta`] this exists standalone because OpenAI
    /// Responses opens reasoning-summary blocks (and per-summary-index
    /// blocks) before any text arrives. Mirrors opencode exactly: a no-op
    /// (including no `step-start`) when `id` is already open — callers that
    /// want a `step-start` first must call [`State::step_start`] themselves,
    /// per otto's explicit-step-start convention (see module docs).
    pub fn reasoning_start(
        &mut self,
        id: &str,
        metadata: Option<ProviderMetadata>,
    ) -> Vec<LLMEvent> {
        if self.reasoning.contains(id) {
            return Vec::new();
        }
        self.reasoning.insert(id.to_string());
        vec![LLMEvent::ReasoningStart {
            id: id.to_string(),
            provider_metadata: metadata,
        }]
    }

    /// Whether a reasoning block `id` is currently open.
    #[must_use]
    pub fn is_reasoning_open(&self, id: &str) -> bool {
        self.reasoning.contains(id)
    }

    /// Close the reasoning block `id` with `reasoning-end` if it is open. Port
    /// of `Lifecycle.reasoningEnd`.
    pub fn reasoning_end(&mut self, id: &str) -> Vec<LLMEvent> {
        self.reasoning_end_with_metadata(id, None)
    }

    /// Like [`State::reasoning_end`], but attaches `metadata` to the
    /// `reasoning-end` event. Port of `Lifecycle.reasoningEnd`'s
    /// `providerMetadata` parameter (`lifecycle.ts:51-63`).
    pub fn reasoning_end_with_metadata(
        &mut self,
        id: &str,
        metadata: Option<ProviderMetadata>,
    ) -> Vec<LLMEvent> {
        if self.reasoning.remove(id) {
            vec![LLMEvent::ReasoningEnd {
                id: id.to_string(),
                provider_metadata: metadata,
            }]
        } else {
            Vec::new()
        }
    }

    /// Close every still-open text/reasoning block, then emit `step-finish`
    /// and `finish`. Port of `Lifecycle.finish`.
    pub fn finish(
        &mut self,
        reason: FinishReason,
        usage: Option<Usage>,
        index: u64,
    ) -> Vec<LLMEvent> {
        self.finish_with_metadata(reason, usage, index, None)
    }

    /// Like [`State::finish`], but attaches `metadata` to both the
    /// `step-finish` and `finish` events. Port of `Lifecycle.finish`'s
    /// `providerMetadata` input field (`lifecycle.ts:80-100`) — otto's base
    /// [`State::finish`] hard-codes `None` for callers (anthropic/openai-chat)
    /// that never carry per-finish metadata; OpenAI Responses does
    /// (`responseId`/`serviceTier`).
    pub fn finish_with_metadata(
        &mut self,
        reason: FinishReason,
        usage: Option<Usage>,
        index: u64,
        metadata: Option<ProviderMetadata>,
    ) -> Vec<LLMEvent> {
        let mut out = Vec::new();

        // Close dangling text blocks.
        for id in std::mem::take(&mut self.text) {
            out.push(LLMEvent::TextEnd {
                id,
                provider_metadata: None,
            });
        }
        // Close dangling reasoning blocks.
        for id in std::mem::take(&mut self.reasoning) {
            out.push(LLMEvent::ReasoningEnd {
                id,
                provider_metadata: None,
            });
        }

        out.push(LLMEvent::StepFinish {
            index,
            reason,
            usage: usage.clone(),
            provider_metadata: metadata.clone(),
        });
        out.push(LLMEvent::Finish {
            reason,
            usage,
            provider_metadata: metadata,
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(events: &[LLMEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                LLMEvent::StepStart { .. } => "step-start",
                LLMEvent::TextStart { .. } => "text-start",
                LLMEvent::TextDelta { .. } => "text-delta",
                LLMEvent::TextEnd { .. } => "text-end",
                LLMEvent::ReasoningStart { .. } => "reasoning-start",
                LLMEvent::ReasoningDelta { .. } => "reasoning-delta",
                LLMEvent::ReasoningEnd { .. } => "reasoning-end",
                LLMEvent::StepFinish { .. } => "step-finish",
                LLMEvent::Finish { .. } => "finish",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn step_start_emitted_once() {
        let mut s = State::initial();
        assert_eq!(types(&s.step_start(0)), ["step-start"]);
        assert!(s.step_start(0).is_empty());
    }

    #[test]
    fn text_start_precedes_first_delta_only() {
        let mut s = State::initial();
        assert_eq!(
            types(&s.text_delta("t1", "a")),
            ["text-start", "text-delta"]
        );
        assert_eq!(types(&s.text_delta("t1", "b")), ["text-delta"]);
        assert_eq!(types(&s.text_end("t1")), ["text-end"]);
        // ending an unknown/closed block is a no-op.
        assert!(s.text_end("t1").is_empty());
    }

    #[test]
    fn reasoning_ordering() {
        let mut s = State::initial();
        assert_eq!(
            types(&s.reasoning_delta("r1", "x")),
            ["reasoning-start", "reasoning-delta"]
        );
        assert_eq!(types(&s.reasoning_delta("r1", "y")), ["reasoning-delta"]);
        assert_eq!(types(&s.reasoning_end("r1")), ["reasoning-end"]);
    }

    #[test]
    fn finish_closes_open_blocks_then_step_finish_then_finish() {
        let mut s = State::initial();
        s.step_start(0);
        s.text_delta("t1", "hi");
        s.reasoning_delta("r1", "think");
        let out = s.finish(FinishReason::Stop, Some(Usage::default()), 0);
        let t = types(&out);
        // last two must be step-finish then finish.
        assert_eq!(&t[t.len() - 2..], ["step-finish", "finish"]);
        // the open blocks are closed before step-finish.
        assert!(t.contains(&"text-end"));
        assert!(t.contains(&"reasoning-end"));
        let end_max = t
            .iter()
            .rposition(|x| *x == "text-end" || *x == "reasoning-end")
            .unwrap();
        let step_finish = t.iter().position(|x| *x == "step-finish").unwrap();
        assert!(end_max < step_finish);
    }
}
