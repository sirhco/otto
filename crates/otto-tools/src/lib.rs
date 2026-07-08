//! Tool contract + built-in filesystem tools — a Rust port of opencode
//! `packages/opencode/src/tool`.
//!
//! The crate is organized around three pieces:
//!
//! * [`tool`] — the [`Tool`] trait, the [`ToolContext`] execution seam
//!   (permission + metadata + cancellation), [`ExecuteResult`], and the
//!   [`ToolError`] taxonomy including [`decode_args`].
//! * [`truncate`] — output truncation ([`truncate_output`]).
//! * [`registry`] — the [`ToolRegistry`] with model gating and post-execute
//!   truncation.
//!
//! The built-in tools live in [`tools`]: `read`, `write`, `edit`, `glob`,
//! `grep`, `bash`, `apply_patch`, `webfetch`, `todowrite`, `websearch`,
//! `skill`, `question`, `invalid`, and `task`. The `apply_patch` parser/applier
//! lives in [`patch`]. `task`/`question` are Phase-later stubs and `websearch`
//! errors until a [`WebSearchProvider`] is injected.

#![forbid(unsafe_code)]

pub mod hook;
pub mod hooks;
pub mod lsp;
pub mod patch;
pub mod registry;
pub mod subagent;
pub mod tool;
pub mod tools;
pub mod truncate;

pub use hook::{HookOutcome, ToolHook};
pub use hooks::RtkHook;
pub use lsp::LspHandle;
pub use registry::ToolRegistry;
pub use subagent::{SubagentRequest, SubagentSpawner};
pub use tool::{
    AllowAll, Attachment, ExecuteResult, MetadataSink, NoopSink, PermissionDenied, PermissionGate,
    PermissionRequest, Tool, ToolContext, ToolContextBuilder, ToolError, decode_args,
};
pub use tools::skill::{SkillMeta, skill_index_block, skill_roots};
pub use tools::{
    ApplyPatchTool, BashTool, EditTool, GlobTool, GrepTool, InvalidTool, QuestionTool, ReadTool,
    SkillTool, TaskTool, Todo, TodoStatus, TodoWriteTool, WebFetchTool, WebSearchProvider,
    WebSearchQuery, WebSearchTool, WriteTool,
};
pub use truncate::{MAX_BYTES, MAX_LINES, Truncated, truncate_output};

#[cfg(test)]
pub(crate) mod testing {
    //! Test-only support: a recording permission gate.

    use std::sync::Mutex;

    use crate::tool::{PermissionDenied, PermissionGate, PermissionRequest};

    /// A [`PermissionGate`] that records every request and can be configured to
    /// deny a specific permission.
    pub struct RecordingGate {
        requests: Mutex<Vec<PermissionRequest>>,
        deny: Option<String>,
    }

    impl RecordingGate {
        /// Gate that approves everything and records requests.
        pub fn allow() -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                deny: None,
            }
        }

        /// Gate that denies exactly `permission` (approving everything else)
        /// and records requests.
        pub fn deny(permission: impl Into<String>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                deny: Some(permission.into()),
            }
        }

        /// Whether any recorded request used `permission`.
        pub fn asked_for(&self, permission: &str) -> bool {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.permission == permission)
        }

        /// All recorded requests for `permission`, in ask order.
        pub fn requests_for(&self, permission: &str) -> Vec<PermissionRequest> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.permission == permission)
                .cloned()
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl PermissionGate for RecordingGate {
        async fn ask(&self, req: PermissionRequest) -> Result<(), PermissionDenied> {
            let deny = self.deny.as_deref() == Some(req.permission.as_str());
            self.requests.lock().unwrap().push(req.clone());
            if deny {
                return Err(PermissionDenied {
                    permission: req.permission,
                });
            }
            Ok(())
        }
    }
}
