//! MCP tools exposed as [`otto_tools::Tool`] — a Rust port of opencode's
//! `McpCatalog` (`packages/opencode/src/mcp/catalog.ts`).
//!
//! Each server tool becomes an [`McpTool`] whose id is namespaced
//! `"{server}_{tool}"` (port of `McpCatalog.toolName`, `catalog.ts:117-119`),
//! and whose `execute` calls `peer().call_tool(...)` and maps the MCP
//! `CallToolResult` back into a [`otto_tools::ExecuteResult`]
//! (port of `convertTool`, `catalog.ts:42-83`). Servers that advertise
//! resources additionally expose `list_mcp_resources` / `read_mcp_resource`
//! tools.

use std::sync::Arc;

use async_trait::async_trait;
use otto_tools::{Attachment, ExecuteResult, Tool, ToolContext, ToolError, decode_args};
use rmcp::model::{CallToolRequestParam, CallToolResult, RawContent, ReadResourceRequestParam};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::client::Connection;

/// Replace every character outside `[A-Za-z0-9_-]` with `_`. Port of
/// `McpCatalog.sanitize` (`catalog.ts:117`).
#[must_use]
pub fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The namespaced tool id `"{server}_{tool}"`. Port of `McpCatalog.toolName`
/// (`catalog.ts:119`).
#[must_use]
pub fn tool_name(server: &str, name: &str) -> String {
    format!("{}_{}", sanitize(server), sanitize(name))
}

/// Concatenate the text blocks of a [`CallToolResult`] with blank-line
/// separators (mirrors the error/text joining in `catalog.ts:70-73`).
fn collect_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match &content.raw {
            RawContent::Text(text) => Some(text.text.clone()),
            RawContent::Resource(embedded) => match &embedded.resource {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                    Some(text.clone())
                }
                rmcp::model::ResourceContents::BlobResourceContents { .. } => None,
            },
            _ => None,
        })
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Map image blocks of a [`CallToolResult`] to [`Attachment`]s as `data:` URIs.
fn collect_attachments(result: &CallToolResult) -> Vec<Attachment> {
    result
        .content
        .iter()
        .filter_map(|content| match &content.raw {
            RawContent::Image(image) => Some(Attachment {
                mime: image.mime_type.clone(),
                filename: None,
                url: format!("data:{};base64,{}", image.mime_type, image.data),
            }),
            _ => None,
        })
        .collect()
}

/// A single MCP server tool presented as a [`otto_tools::Tool`]. Port of the
/// `dynamicTool` produced by `McpCatalog.convertTool` (`catalog.ts:42`).
pub struct McpTool {
    connection: Arc<Connection>,
    /// Raw (un-namespaced) MCP tool name, sent in `call_tool`.
    tool_name: String,
    /// Namespaced id (`"{server}_{tool}"`).
    id: String,
    description: String,
    input_schema: Value,
}

impl McpTool {
    /// Build an [`McpTool`] for `tool` on `connection`.
    #[must_use]
    pub fn new(connection: Arc<Connection>, tool: &rmcp::model::Tool) -> Self {
        let id = tool_name(connection.name(), &tool.name);
        // Port `catalog.ts:43-48`: normalize to an object schema with the
        // server-provided properties.
        let input_schema = Value::Object((*tool.input_schema).clone());
        Self {
            connection,
            tool_name: tool.name.to_string(),
            id,
            description: tool.description.as_deref().unwrap_or("").to_string(),
            input_schema,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let arguments: Option<Map<String, Value>> = match args {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return Err(ToolError::InvalidArguments {
                    tool: self.id.clone(),
                    detail: format!("expected a JSON object, got {other}"),
                });
            }
        };

        let result = self
            .connection
            .peer()
            .call_tool(CallToolRequestParam {
                name: self.tool_name.clone().into(),
                arguments,
            })
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        // Port `catalog.ts:68-73`: isError → throw with the joined text.
        if result.is_error == Some(true) {
            let message = collect_text(&result);
            let message = if message.is_empty() {
                "MCP tool returned an error".to_string()
            } else {
                message
            };
            return Err(ToolError::Execution(message));
        }

        let mut output = collect_text(&result);
        // Port `catalog.ts:75-80`: fall back to structuredContent when there is
        // no textual content.
        if output.is_empty()
            && let Some(structured) = &result.structured_content
        {
            output = structured.to_string();
        }
        let attachments = collect_attachments(&result);
        let metadata = result
            .structured_content
            .clone()
            .unwrap_or_else(|| json!({}));

        Ok(ExecuteResult {
            title: self.id.clone(),
            metadata,
            output,
            attachments,
        })
    }
}

/// `list_mcp_resources` for one server — lists the resources it advertises.
/// Ports the MCP-resource listing surface (`index.ts:713`).
pub struct ListMcpResourcesTool {
    connection: Arc<Connection>,
    id: String,
}

impl ListMcpResourcesTool {
    /// Build the tool for `connection`.
    #[must_use]
    pub fn new(connection: Arc<Connection>) -> Self {
        let id = tool_name(connection.name(), "list_mcp_resources");
        Self { connection, id }
    }
}

#[async_trait]
impl Tool for ListMcpResourcesTool {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        "List the resources advertised by this MCP server."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let resources = self
            .connection
            .peer()
            .list_all_resources()
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let lines: Vec<String> = resources
            .iter()
            .map(|r| match &r.description {
                Some(desc) => format!("{} ({}) — {}", r.name, r.uri, desc),
                None => format!("{} ({})", r.name, r.uri),
            })
            .collect();
        let metadata = serde_json::to_value(
            resources
                .iter()
                .map(|r| json!({ "uri": r.uri, "name": r.name, "mimeType": r.mime_type }))
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| json!([]));

        Ok(ExecuteResult {
            title: self.id.clone(),
            metadata,
            output: lines.join("\n"),
            attachments: Vec::new(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct ReadResourceArgs {
    uri: String,
}

/// `read_mcp_resource` for one server — reads a resource by URI via
/// `read_resource` (`index.ts:774`).
pub struct ReadMcpResourceTool {
    connection: Arc<Connection>,
    id: String,
}

impl ReadMcpResourceTool {
    /// Build the tool for `connection`.
    #[must_use]
    pub fn new(connection: Arc<Connection>) -> Self {
        let id = tool_name(connection.name(), "read_mcp_resource");
        Self { connection, id }
    }
}

#[async_trait]
impl Tool for ReadMcpResourceTool {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        "Read a resource from this MCP server by its URI."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "uri": { "type": "string", "description": "The URI of the resource to read." }
            },
            "required": ["uri"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let ReadResourceArgs { uri } = decode_args(&self.id, args)?;

        let result = self
            .connection
            .peer()
            .read_resource(ReadResourceRequestParam { uri: uri.clone() })
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let mut output = Vec::new();
        let mut attachments = Vec::new();
        for content in &result.contents {
            match content {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                    output.push(text.clone());
                }
                rmcp::model::ResourceContents::BlobResourceContents {
                    blob,
                    mime_type,
                    uri,
                    ..
                } => {
                    let mime = mime_type
                        .clone()
                        .unwrap_or_else(|| "application/octet-stream".into());
                    attachments.push(Attachment {
                        mime: mime.clone(),
                        filename: Some(uri.clone()),
                        url: format!("data:{mime};base64,{blob}"),
                    });
                }
            }
        }

        Ok(ExecuteResult {
            title: self.id.clone(),
            metadata: json!({ "uri": uri }),
            output: output.join("\n\n"),
            attachments,
        })
    }
}

impl crate::client::McpClient {
    /// Collect every connected server's tools as [`otto_tools::Tool`]s, ready
    /// to register into a `ToolRegistry`. Ports `MCP.tools` (`index.ts:658`):
    /// one namespaced [`McpTool`] per advertised tool, plus the
    /// `list_mcp_resources` / `read_mcp_resource` pair for any server that
    /// advertises resources.
    #[must_use]
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for connection in self.connections() {
            for tool in connection.tools() {
                tools.push(Arc::new(McpTool::new(connection.clone(), tool)));
            }
            if connection.has_resources() {
                tools.push(Arc::new(ListMcpResourcesTool::new(connection.clone())));
                tools.push(Arc::new(ReadMcpResourceTool::new(connection.clone())));
            }
        }
        tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_namespaces_and_sanitizes() {
        assert_eq!(tool_name("test-server", "echo"), "test-server_echo");
        assert_eq!(tool_name("my server", "do:it"), "my_server_do_it");
    }
}
