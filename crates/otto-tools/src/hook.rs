//! A pre-execute hook seam for the [`ToolRegistry`](crate::registry::ToolRegistry).
//!
//! Otto has no PreToolUse layer of its own — [`Tool::execute`](crate::Tool::execute)
//! is called directly by the registry. A [`ToolHook`] runs *before* a tool
//! executes, so it can rewrite the tool's arguments (e.g. prefixing a shell
//! command with a proxy) or block the call outright. Hooks are registered on
//! the registry via
//! [`register_hook`](crate::registry::ToolRegistry::register_hook) and run in
//! registration order, each seeing the args produced by the previous one.

use serde_json::Value;

use crate::tool::ToolContext;

/// The outcome of a [`ToolHook::before_execute`] call.
pub enum HookOutcome {
    /// Proceed to the next hook / the tool with these (possibly rewritten) args.
    Continue(Value),
    /// Block the tool call; the reason surfaces as a
    /// [`ToolError::Execution`](crate::ToolError::Execution).
    Deny(String),
}

/// A transformation applied to a tool call before it executes.
///
/// Implementors inspect `tool_id` and `args` and return a [`HookOutcome`].
/// Returning `Continue(args)` unchanged is a no-op; this is the expected path
/// for tools a hook does not care about.
#[async_trait::async_trait]
pub trait ToolHook: Send + Sync {
    /// Run before the identified tool executes.
    async fn before_execute(&self, tool_id: &str, args: Value, ctx: &ToolContext) -> HookOutcome;
}
