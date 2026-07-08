//! The `Protocol` trait — the wire-shape half of a route.
//!
//! Port of the `Protocol` interface in opencode
//! `packages/llm/src/route/protocol.ts`. A protocol knows how to build a
//! provider request body, decode streamed frames into provider events, and
//! fold those events into the provider-neutral [`LLMEvent`] union via a small
//! per-stream state machine.
//!
//! Mapping from `protocol.ts`:
//! - [`Protocol::build_body`] ⇐ `body.from`
//! - [`Protocol::decode_event`] ⇐ frame → `stream.event`
//! - [`Protocol::initial`] ⇐ `stream.initial`
//! - [`Protocol::step`] ⇐ `stream.step`
//! - [`Protocol::terminal`] ⇐ `stream.terminal`
//! - [`Protocol::on_halt`] ⇐ `stream.onHalt`

use otto_events::LLMEvent;

use crate::error::LLMError;
use crate::request::LLMRequest;

/// A provider wire protocol (e.g. Anthropic Messages, OpenAI Chat
/// Completions).
///
/// Port of the `Protocol` interface in `route/protocol.ts`. `Frame` is the raw
/// SSE `data:` payload string; [`Protocol::decode_event`] parses it into the
/// protocol's `Event` type.
pub trait Protocol: Send + Sync {
    /// The provider request body type ([`serde::Serialize`]).
    type Body: serde::Serialize + Send;
    /// The decoded provider event type ([`serde::de::DeserializeOwned`]).
    type Event: serde::de::DeserializeOwned + Send;
    /// The per-stream accumulator state.
    type State: Send;

    /// Stable protocol id, e.g. `"anthropic"` / `"openai"`.
    fn id(&self) -> &'static str;

    /// Build the provider request body from a neutral request (`body.from`).
    ///
    /// # Errors
    /// Returns [`LLMError::Body`] / [`LLMError::Validation`] if the request
    /// cannot be represented for this provider.
    fn build_body(&self, req: &LLMRequest) -> Result<Self::Body, LLMError>;

    /// Decode one SSE `data:` frame into a provider event (`stream.event`).
    ///
    /// # Errors
    /// Returns [`LLMError::EventDecode`] if the frame is not valid.
    fn decode_event(&self, frame: &str) -> Result<Self::Event, LLMError>;

    /// The initial accumulator state for a new stream (`stream.initial`).
    fn initial(&self, req: &LLMRequest) -> Self::State;

    /// Fold one provider event into zero or more [`LLMEvent`]s, mutating the
    /// accumulator (`stream.step`).
    ///
    /// # Errors
    /// Returns an [`LLMError`] if the event is malformed for the current state.
    fn step(&self, state: &mut Self::State, event: Self::Event) -> Result<Vec<LLMEvent>, LLMError>;

    /// Whether `event` is the terminal event that ends the stream
    /// (`stream.terminal`, inclusive take-until). Defaults to `false`.
    fn terminal(&self, _event: &Self::Event) -> bool {
        false
    }

    /// Flush any dangling state when the stream halts (`stream.onHalt`).
    /// Defaults to emitting nothing.
    fn on_halt(&self, _state: &mut Self::State) -> Vec<LLMEvent> {
        Vec::new()
    }
}
