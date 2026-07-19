//! Concrete wire protocols and their shared utilities.
//!
//! Port of opencode `packages/llm/src/protocols/`. The [`utils`] module holds
//! the shared lifecycle / tool-stream state machines every protocol composes;
//! each concrete protocol implements [`crate::protocol::Protocol`].
pub mod anthropic_messages;
pub mod copilot_cache;
pub mod gemini;
pub mod openai_chat;
pub mod openai_compatible;
pub mod openai_responses;
pub mod utils;

pub use copilot_cache::{BodyShape, CopilotCache};
