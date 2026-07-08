//! MCP client — a Rust port of opencode's MCP integration
//! (`packages/opencode/src/mcp/`).
//!
//! - [`McpServerConfig`] ports the `local` / `remote` config union
//!   (`core/src/v1/config/mcp.ts`).
//! - [`McpClient`] ports the connection lifecycle Service (`mcp/index.ts`):
//!   local stdio servers, remote streamable-HTTP servers with SSE fallback,
//!   per-server [`McpStatus`], and the [`ToolsChanged`] event.
//! - [`McpTool`] ports `McpCatalog` (`mcp/catalog.ts`): each server tool is
//!   surfaced as a namespaced [`otto_tools::Tool`]; servers advertising
//!   resources also get [`ListMcpResourcesTool`] / [`ReadMcpResourceTool`].
//!
//! OAuth (opencode's `mcp/oauth-*`) is not ported yet — see the TODO on
//! [`McpClient::connect`].

#![forbid(unsafe_code)]

mod client;
mod config;
mod tool;

pub use client::{Connection, McpClient, McpError, McpStatus, ToolsChanged};
pub use config::{
    DEFAULT_CALLBACK_PORT, DEFAULT_REDIRECT_URI, DEFAULT_TIMEOUT, McpServerConfig, OAuthConfig,
    OAuthSetting,
};
pub use tool::{ListMcpResourcesTool, McpTool, ReadMcpResourceTool, sanitize, tool_name};
