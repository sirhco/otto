//! The agent loop — port of opencode `session/prompt.ts` + `processor.ts`.
//!
//! Ties [`otto_llm`] (streaming), [`otto_tools`] (execution), and
//! [`otto_storage`] (persistence) into the while-loop that drives an agent
//! turn. Filled in during Phase 3.

#![forbid(unsafe_code)]

pub mod compaction;
pub mod convert;
pub mod overflow;
pub mod processor;
pub mod retry;
pub mod run;
pub mod run_state;
pub mod runtime;
pub mod subagent;
pub mod system;
mod warm;

pub use compaction::{select, CompactionError, SelectResult};
pub use convert::{to_model_messages, ConvertOptions};
pub use overflow::is_overflow;
pub use processor::{ProcessOutcome, Processor, ProcessorError};
pub use retry::{retryable, with_retry};
pub use run::{run_loop, tap_events, RunConfig, RunError};
pub use run_state::RunnerRegistry;
pub use runtime::augment_with_tools;
pub use subagent::{RouteFor, SessionSubagentSpawner};
pub use system::{assemble, base_prompt, build_system, environment, instructions};
pub use warm::WarmCache;
