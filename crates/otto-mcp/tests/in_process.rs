//! Hermetic, in-process MCP integration tests.
//!
//! A tiny [`ServerHandler`] MCP server (an `echo` tool, a `boom` error tool,
//! and one text resource) is served over one end of a `tokio::io::duplex`
//! in-memory pipe; the [`McpClient`] connects to the other end via
//! [`McpClient::connect_transport`]. No network, no child process.

use std::sync::Arc;
use std::time::Duration;

use otto_mcp::{McpClient, McpStatus};
use otto_tools::{Tool, ToolContext, ToolError};
use rmcp::ErrorData as McpError;
use rmcp::ServiceExt;
use rmcp::model::{
    AnnotateAble, CallToolRequestParam, CallToolResult, Content, ListResourcesResult,
    ListToolsResult, PaginatedRequestParam, RawResource, ReadResourceRequestParam,
    ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo, Tool as McpToolDef,
};
use rmcp::service::{RequestContext, RoleServer, RunningService};
use serde_json::json;
use tokio::io::DuplexStream;

// ---------------------------------------------------------------------------
// Test MCP server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TestServer;

impl TestServer {
    fn echo_schema() -> Arc<serde_json::Map<String, serde_json::Value>> {
        let schema = json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        });
        Arc::new(schema.as_object().cloned().unwrap())
    }
}

impl rmcp::ServerHandler for TestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            ..Default::default()
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let echo = McpToolDef::new("echo", "Echo the text back", TestServer::echo_schema());
        let boom = McpToolDef::new("boom", "Always fails", TestServer::echo_schema());
        Ok(ListToolsResult::with_all_items(vec![echo, boom]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        match request.name.as_ref() {
            "echo" => {
                let text = request
                    .arguments
                    .as_ref()
                    .and_then(|m| m.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            "boom" => Ok(CallToolResult::error(vec![Content::text("boom exploded")])),
            other => Err(McpError::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParam>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let resource = RawResource::new("mem://greeting", "greeting").no_annotation();
        Ok(ListResourcesResult::with_all_items(vec![resource]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParam,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        Ok(ReadResourceResult {
            contents: vec![ResourceContents::text("hello from resource", request.uri)],
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Serve the test server on `server_io` in the background.
fn serve_test_server(
    server_io: DuplexStream,
) -> tokio::task::JoinHandle<RunningService<RoleServer, TestServer>> {
    tokio::spawn(async move { TestServer.serve(server_io).await.expect("server serve") })
}

/// Connect a fresh client to a fresh test server; returns both (the server
/// handle must be kept alive for the connection to stay up).
async fn connect() -> (McpClient, RunningService<RoleServer, TestServer>) {
    let (server_io, client_io) = tokio::io::duplex(8192);
    let server = serve_test_server(server_io);
    let client = McpClient::new("test");
    client
        .connect_transport("testserver", client_io)
        .await
        .expect("client connect");
    let server = server.await.expect("server join");
    (client, server)
}

fn find_tool(tools: &[Arc<dyn Tool>], id: &str) -> Arc<dyn Tool> {
    tools
        .iter()
        .find(|t| t.id() == id)
        .unwrap_or_else(|| panic!("tool '{id}' not found; have {:?}", ids(tools)))
        .clone()
}

fn ids(tools: &[Arc<dyn Tool>]) -> Vec<String> {
    tools.iter().map(|t| t.id().to_string()).collect()
}

fn ctx() -> ToolContext {
    ToolContext::builder(std::env::temp_dir()).build()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connect_lists_namespaced_tools() {
    let (client, _server) = connect().await;
    let tools = client.tools();
    let names = ids(&tools);
    assert!(names.contains(&"testserver_echo".to_string()), "{names:?}");
    assert!(names.contains(&"testserver_boom".to_string()), "{names:?}");
    // Resources advertised → resource tools present.
    assert!(
        names.contains(&"testserver_list_mcp_resources".to_string()),
        "{names:?}"
    );
    assert!(
        names.contains(&"testserver_read_mcp_resource".to_string()),
        "{names:?}"
    );

    let echo = find_tool(&tools, "testserver_echo");
    assert_eq!(echo.description(), "Echo the text back");
    assert_eq!(
        echo.parameters_schema()["properties"]["text"]["type"],
        "string"
    );
}

#[tokio::test]
async fn instructions_lists_server_and_namespaced_tools() {
    let (client, _server) = connect().await;
    let block = client.instructions().expect("instructions present");
    assert!(block.contains("<mcp_instructions>"), "{block}");
    assert!(block.contains("</mcp_instructions>"), "{block}");
    assert!(block.contains("<server name=\"testserver\">"), "{block}");
    // Namespaced tool ids the model can call.
    assert!(block.contains("testserver_echo"), "{block}");
    assert!(block.contains("testserver_boom"), "{block}");
    // Resource tools too, since the server advertises a resource.
    assert!(block.contains("testserver_read_mcp_resource"), "{block}");
}

#[tokio::test]
async fn instructions_none_when_no_servers() {
    let client = McpClient::new("test");
    assert!(client.instructions().is_none());
}

#[tokio::test]
async fn execute_echo_returns_output() {
    let (client, _server) = connect().await;
    let tools = client.tools();
    let echo = find_tool(&tools, "testserver_echo");

    let result = echo.execute(json!({ "text": "hi" }), &ctx()).await.unwrap();
    assert_eq!(result.output, "hi");
    assert_eq!(result.title, "testserver_echo");
}

#[tokio::test]
async fn tool_error_maps_to_tool_error() {
    let (client, _server) = connect().await;
    let tools = client.tools();
    let boom = find_tool(&tools, "testserver_boom");

    let err = boom
        .execute(json!({ "text": "x" }), &ctx())
        .await
        .unwrap_err();
    match err {
        ToolError::Execution(message) => assert!(message.contains("boom exploded"), "{message}"),
        other => panic!("expected Execution, got {other:?}"),
    }
}

#[tokio::test]
async fn read_mcp_resource_returns_content() {
    let (client, _server) = connect().await;
    let tools = client.tools();
    let read = find_tool(&tools, "testserver_read_mcp_resource");

    let result = read
        .execute(json!({ "uri": "mem://greeting" }), &ctx())
        .await
        .unwrap();
    assert!(
        result.output.contains("hello from resource"),
        "{}",
        result.output
    );
}

#[tokio::test]
async fn status_transitions_and_events_fire() {
    let (server_io, client_io) = tokio::io::duplex(8192);
    let server = serve_test_server(server_io);

    let client = McpClient::new("test");
    let mut events = client.subscribe();

    // Unknown before any connect.
    assert_eq!(client.status("testserver"), None);

    client
        .connect_transport("testserver", client_io)
        .await
        .expect("connect");
    let _server = server.await.expect("server join");

    assert_eq!(client.status("testserver"), Some(McpStatus::Connected));
    let event = events.recv().await.expect("tools changed on connect");
    assert_eq!(event.server, "testserver");

    client.disconnect("testserver").await.expect("disconnect");
    assert_eq!(client.status("testserver"), Some(McpStatus::Disabled));
    let event = events.recv().await.expect("tools changed on disconnect");
    assert_eq!(event.server, "testserver");

    // Tools are gone after disconnect.
    assert!(client.tools().is_empty());
}

#[tokio::test]
async fn connecting_precedes_connected() {
    let (server_io, client_io) = tokio::io::duplex(8192);
    let client = Arc::new(McpClient::new("test"));

    // Start connecting before the server is serving: the handshake blocks, so
    // the status observably sits at `Connecting`.
    let c = client.clone();
    let connect_task =
        tokio::spawn(async move { c.connect_transport("testserver", client_io).await });

    // Let the connect task set Connecting and block on the handshake.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(client.status("testserver"), Some(McpStatus::Connecting));

    // Now bring the server up; the handshake completes.
    let server = serve_test_server(server_io);
    connect_task.await.expect("join").expect("connect");
    let _server = server.await.expect("server join");

    assert_eq!(client.status("testserver"), Some(McpStatus::Connected));
}

#[tokio::test]
async fn disabled_config_does_not_connect() {
    let client = McpClient::new("test");
    let cfg = otto_mcp::McpServerConfig::Local {
        command: vec!["true".into()],
        cwd: None,
        environment: None,
        enabled: Some(false),
        timeout: None,
    };
    let status = client.connect("disabled", &cfg).await.unwrap();
    assert_eq!(status, McpStatus::Disabled);
    assert_eq!(client.status("disabled"), Some(McpStatus::Disabled));
    assert!(client.tools().is_empty());
}
