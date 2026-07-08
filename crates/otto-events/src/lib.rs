//! `LLMEvent` union, `Usage` accounting, and the app event bus.
//!
//! Direct port of opencode `packages/llm/src/schema/events.ts` (plus the
//! literal unions it references from `ids.ts`, `errors.ts`, and `messages.ts`).
//!
//! - [`LLMEvent`] — the universal, provider-neutral LLM streaming event union,
//!   tagged by a `type` field with kebab-case variant strings.
//! - [`Usage`] — token accounting with the documented inclusive-vs-breakdown
//!   invariant.
//! - [`EventBus`] — a small pub/sub over [`tokio::sync::broadcast`], the Rust
//!   analog of opencode's Node `EventEmitter` app bus.

#![forbid(unsafe_code)]

mod bus;
mod event;
mod usage;

pub use bus::{DEFAULT_CAPACITY, EventBus};
pub use event::{
    FinishReason, Json, LLMEvent, ProviderFailureClassification, ToolOutput, ToolResultValue,
};
pub use usage::{ProviderMetadata, Usage};
