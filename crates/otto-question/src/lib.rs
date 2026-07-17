//! The question-tool interactive ask/reply gate — a simpler sibling of
//! `otto-permission`'s `Permission` service, with no ruleset/mode/policy
//! dimension: every ask reaches a human (or auto-cancels non-interactively).
//!
//! * [`question`] — the [`Question`] service implementing `ask`/`reply`.
//! * [`gate`] — [`SessionQuestionGate`], the [`otto_tools::tool::QuestionGate`]
//!   the session loop injects into `ToolContext`.

#![forbid(unsafe_code)]

pub mod gate;
pub mod question;

pub use gate::SessionQuestionGate;
pub use question::{Asked, PendingInfo, Question, RequestId, SessionId};
