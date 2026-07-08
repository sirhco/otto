//! Session message/part model + SQLite persistence — a Rust port of opencode's
//! `packages/schema/src/v1/session.ts` (the data model),
//! `packages/opencode/src/session/message-v2.ts` (the [`latest`] /
//! [`filter_compacted`] derivations and row hydration), and
//! `packages/core/src/session/sql.ts` (the SQLite schema).
//!
//! * [`model`] — the [`Part`] / [`ToolState`] / [`Info`] / [`AssistantError`]
//!   unions plus their value objects.
//! * [`message`] — [`latest`] and [`filter_compacted`].
//! * [`store`] — the async SQLite [`Store`] over `sqlx`.

#![forbid(unsafe_code)]

pub mod message;
pub mod model;
pub mod store;

pub use message::{Latest, filter_compacted, latest};
pub use model::{
    AgentSource, ApiError, ApiErrorData, Assistant, AssistantError, AssistantPath, AssistantTime,
    CompletedTime, FilePartSource, FilePartSourceText, Info, InfoBody, Json, MessageId, ModelRef,
    OutputFormat, Part, PartId, PartKind, Position, Range, RetryTime, SessionId, StartEndReqTime,
    StartEndTime, StartTime, TokenCache, Tokens, ToolState, User, UserModel, UserSummary, UserTime,
    WithParts, new_message_id, new_part_id,
};
pub use store::{Session, SessionCacheTokens, SessionTokens, StorageError, Store, WorkflowTaskRow};
