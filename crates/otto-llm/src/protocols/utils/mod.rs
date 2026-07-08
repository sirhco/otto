//! Shared protocol utilities.
//!
//! Port of opencode `packages/llm/src/protocols/utils/`. These are the
//! building blocks concrete protocols compose to guarantee correct
//! [`otto_events::LLMEvent`] ordering.

pub mod gemini_tool_schema;
pub mod lifecycle;
pub mod sigv4;
pub mod tool_stream;
