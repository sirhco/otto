//! MCP connection lifecycle — a Rust port of opencode's MCP `Service`
//! (`packages/opencode/src/mcp/index.ts`).
//!
//! [`McpClient`] owns a set of named server connections. It ports:
//! - `connectLocal` (`index.ts:332`): spawn a child process over stdio.
//! - `connectRemote` (`index.ts:228`): dial streamable-HTTP first, then fall
//!   back to SSE (`index.ts:261-281`).
//! - the `status` / `connect` / `disconnect` service surface
//!   (`index.ts:157-169`).
//! - the `ToolsChanged` event published on connect/disconnect
//!   (`index.ts:462`).
//!
//! OAuth (`index.ts:startAuth`/`authenticate`/`finishAuth`) is intentionally
//! **not** ported here — see the TODO in [`McpClient::connect`].

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use otto_events::EventBus;
use rmcp::ServiceExt;
use rmcp::model::{
    ClientCapabilities, ClientInfo, Implementation, ProtocolVersion, Resource as McpResource,
    Tool as McpToolDef,
};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::sse_client::{SseClientConfig, SseClientTransport};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::transport::{ConfigureCommandExt, IntoTransport, TokioChildProcess};
use tokio::process::Command;
use tokio::sync::broadcast;

use crate::config::McpServerConfig;
use crate::tool::tool_name;

/// Errors raised while connecting to or managing MCP servers.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// A connection (transport creation or the MCP handshake) failed. Mirrors
    /// the `{ status: "failed", error }` branch in `index.ts:317`/`index.ts:359`.
    #[error("MCP server '{name}' failed to connect: {message}")]
    Connect {
        /// The server name.
        name: String,
        /// The underlying failure message (model/log facing).
        message: String,
    },

    /// No server is registered under this name (`NotFoundError`, `index.ts:70`).
    #[error("MCP server '{0}' not found")]
    NotFound(String),

    /// A local server config had an empty `command` array.
    #[error("MCP server '{0}' has an empty command")]
    EmptyCommand(String),

    /// A configured HTTP header was not a valid header name/value.
    #[error("invalid HTTP header for MCP server '{server}': {message}")]
    InvalidHeader {
        /// The server name.
        server: String,
        /// Why the header was rejected.
        message: String,
    },

    /// The child process could not be spawned.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Per-server connection status. Ports opencode's `Status` union
/// (`index.ts:101-108`); the OAuth-specific `needs_auth` /
/// `needs_client_registration` states are omitted because OAuth is not ported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpStatus {
    /// A connection attempt is in flight (handshake pending).
    Connecting,
    /// Connected and tools/resources have been listed.
    Connected,
    /// Disabled via config (`enabled: false`) or after an explicit disconnect.
    Disabled,
    /// The last connection attempt failed; carries the error message.
    Failed(String),
}

/// Emitted whenever a server connects or disconnects. Ports
/// `McpEvent.ToolsChanged` (`index.ts:62`), signalling that the aggregate
/// tool list changed and should be re-collected via [`McpClient::tools`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolsChanged {
    /// The server whose tools changed.
    pub server: String,
}

/// A live connection to one MCP server. Shared (`Arc`) into every [`McpTool`]
/// so tool execution can reach the server's [`Peer`]. Holds the server's
/// advertised tool/resource catalog (opencode caches these in `State.defs`,
/// `index.ts:147`).
///
/// [`McpTool`]: crate::tool::McpTool
pub struct Connection {
    name: String,
    peer: Peer<RoleClient>,
    tools: Vec<McpToolDef>,
    resources: Vec<McpResource>,
}

impl Connection {
    /// The server name (the map key / namespace prefix).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The rmcp client peer used to issue `call_tool` / `read_resource`.
    #[must_use]
    pub fn peer(&self) -> &Peer<RoleClient> {
        &self.peer
    }

    /// The server's advertised tools (raw MCP names, un-namespaced).
    #[must_use]
    pub fn tools(&self) -> &[McpToolDef] {
        &self.tools
    }

    /// The server's advertised resources.
    #[must_use]
    pub fn resources(&self) -> &[McpResource] {
        &self.resources
    }

    /// Whether the server advertised any resources (gates the resource tools).
    #[must_use]
    pub fn has_resources(&self) -> bool {
        !self.resources.is_empty()
    }
}

struct ServerEntry {
    status: McpStatus,
    /// Owns the background task + transport lifecycle. Dropped/cancelled on
    /// disconnect. `None` for entries that never connected (disabled/failed).
    running: Option<RunningService<RoleClient, ClientInfo>>,
    connection: Option<Arc<Connection>>,
}

/// Manages a set of named MCP server connections and exposes their tools.
/// The Rust analogue of opencode's MCP `Service` (`index.ts:193`).
pub struct McpClient {
    version: String,
    servers: Mutex<HashMap<String, ServerEntry>>,
    events: EventBus<ToolsChanged>,
}

impl Default for McpClient {
    fn default() -> Self {
        Self::new(env!("CARGO_PKG_VERSION"))
    }
}

impl McpClient {
    /// Create a client. `version` is reported to servers as the client
    /// implementation version (`Client { name: "otto", version }`,
    /// `index.ts:77`).
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            servers: Mutex::new(HashMap::new()),
            events: EventBus::new(),
        }
    }

    /// The client handshake info sent on `initialize` (`createClient`,
    /// `index.ts:76-82`). The name is always `"otto"`.
    fn client_info(&self) -> ClientInfo {
        ClientInfo {
            protocol_version: ProtocolVersion::default(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "otto".into(),
                title: None,
                version: self.version.clone(),
                icons: None,
                website_url: None,
            },
        }
    }

    /// Subscribe to [`ToolsChanged`] events. Each subscriber sees every event
    /// published after it subscribed.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ToolsChanged>> {
        self.events.subscribe()
    }

    /// The current status of `name`, or `None` if the server was never seen.
    #[must_use]
    pub fn status(&self, name: &str) -> Option<McpStatus> {
        self.servers
            .lock()
            .unwrap()
            .get(name)
            .map(|e| e.status.clone())
    }

    /// Snapshot of all known server statuses (ports `MCP.status`,
    /// `index.ts:583`).
    #[must_use]
    pub fn statuses(&self) -> HashMap<String, McpStatus> {
        self.servers
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.status.clone()))
            .collect()
    }

    /// The connected servers' [`Connection`]s, for building the tool list.
    #[must_use]
    pub fn connections(&self) -> Vec<Arc<Connection>> {
        self.servers
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.status == McpStatus::Connected)
            .filter_map(|e| e.connection.clone())
            .collect()
    }

    /// Build the `<mcp_instructions>` system-prompt block advertising every
    /// connected server and its namespaced tools, or `None` when no server is
    /// connected.
    ///
    /// Port of `SystemPrompt.mcp` (`session/system.ts:110-126`) combined with
    /// `MCP.instructions` (`mcp/index.ts:607-617`): one `<server name="…">`
    /// element per connected server (sorted by name for stable output), whose
    /// body lists the namespaced tool ids (`{server}_{tool}`, `catalog.ts:119`)
    /// the model may call — plus the resource tools for any server advertising
    /// resources — each body line indented four spaces to match opencode's
    /// nesting.
    ///
    /// otto does not yet capture a server's own `getInstructions()` text
    /// (`index.ts:391`), so the body is synthesized from the tool catalog; the
    /// wrapper format stays faithful to opencode.
    #[must_use]
    pub fn instructions(&self) -> Option<String> {
        let mut connections = self.connections();
        if connections.is_empty() {
            return None;
        }
        connections.sort_by(|a, b| a.name().cmp(b.name()));

        let mut out = String::from("<mcp_instructions>\n");
        for conn in &connections {
            let server = conn.name();
            let _ = writeln!(out, "  <server name=\"{server}\">");
            let _ = writeln!(
                out,
                "    The following tools are available from this MCP server:"
            );
            for tool in conn.tools() {
                let _ = writeln!(out, "    - {}", tool_name(server, &tool.name));
            }
            if conn.has_resources() {
                let _ = writeln!(out, "    - {}", tool_name(server, "list_mcp_resources"));
                let _ = writeln!(out, "    - {}", tool_name(server, "read_mcp_resource"));
            }
            let _ = writeln!(out, "  </server>");
        }
        out.push_str("</mcp_instructions>");
        Some(out)
    }

    fn set_status(&self, name: &str, status: McpStatus) {
        let mut servers = self.servers.lock().unwrap();
        servers
            .entry(name.to_string())
            .and_modify(|e| e.status = status.clone())
            .or_insert_with(|| ServerEntry {
                status,
                running: None,
                connection: None,
            });
    }

    /// Connect a named server from its config. Ports `MCP.create`
    /// (`index.ts:364`): disabled servers short-circuit, otherwise dial local
    /// (stdio) or remote (streamable-HTTP → SSE).
    ///
    /// TODO(confirm): OAuth is not ported. opencode's `connectRemote`
    /// (`index.ts:243-259`) installs an OAuth provider and, on a 401, registers
    /// a pending auth flow (`index.ts:304-313`). Here a remote 401 simply
    /// surfaces as [`McpStatus::Failed`]; wiring the auth flow is a follow-up.
    pub async fn connect(
        &self,
        name: impl Into<String>,
        config: &McpServerConfig,
    ) -> Result<McpStatus, McpError> {
        let name = name.into();
        if !config.is_enabled() {
            self.set_status(&name, McpStatus::Disabled);
            return Ok(McpStatus::Disabled);
        }
        match config {
            McpServerConfig::Local {
                command,
                cwd,
                environment,
                ..
            } => {
                self.connect_local(name, command, cwd.as_deref(), environment)
                    .await
            }
            McpServerConfig::Remote { url, headers, .. } => {
                self.connect_remote(name, url, headers).await
            }
        }
    }

    /// Spawn a local server over stdio (`connectLocal`, `index.ts:332`).
    async fn connect_local(
        &self,
        name: String,
        command: &[String],
        cwd: Option<&str>,
        environment: &Option<BTreeMap<String, String>>,
    ) -> Result<McpStatus, McpError> {
        let Some((program, args)) = command.split_first() else {
            self.set_status(&name, McpStatus::Failed("empty command".into()));
            return Err(McpError::EmptyCommand(name));
        };
        let cwd = cwd.map(str::to_string);
        let args = args.to_vec();
        let env = environment.clone();
        let command = Command::new(program).configure(|cmd| {
            cmd.args(&args);
            if let Some(cwd) = &cwd {
                cmd.current_dir(cwd);
            }
            if let Some(env) = &env {
                for (k, v) in env {
                    cmd.env(k, v);
                }
            }
        });
        let transport = match TokioChildProcess::new(command) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(&name, McpStatus::Failed(e.to_string()));
                return Err(McpError::Io(e));
            }
        };
        self.connect_transport(name, transport).await
    }

    /// Dial a remote server, streamable-HTTP first then SSE. Ports the
    /// transport loop of `connectRemote` (`index.ts:261-324`).
    async fn connect_remote(
        &self,
        name: String,
        url: &str,
        headers: &Option<BTreeMap<String, String>>,
    ) -> Result<McpStatus, McpError> {
        let http = self.build_http_client(&name, headers)?;

        // 1. StreamableHTTP.
        let streamable = StreamableHttpClientTransport::with_client(
            http.clone(),
            StreamableHttpClientTransportConfig::with_uri(url.to_string()),
        );
        if let Ok(status) = self.connect_transport(name.clone(), streamable).await {
            return Ok(status);
        }

        // 2. SSE fallback.
        let sse = match SseClientTransport::start_with_client(
            http,
            SseClientConfig {
                sse_endpoint: url.to_string().into(),
                ..Default::default()
            },
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                let message = e.to_string();
                self.set_status(&name, McpStatus::Failed(message.clone()));
                return Err(McpError::Connect { name, message });
            }
        };
        self.connect_transport(name, sse).await
    }

    fn build_http_client(
        &self,
        name: &str,
        headers: &Option<BTreeMap<String, String>>,
    ) -> Result<reqwest::Client, McpError> {
        let mut builder = reqwest::Client::builder();
        if let Some(map) = headers {
            let mut header_map = reqwest::header::HeaderMap::new();
            for (key, value) in map {
                let header_name =
                    reqwest::header::HeaderName::from_bytes(key.as_bytes()).map_err(|e| {
                        McpError::InvalidHeader {
                            server: name.to_string(),
                            message: e.to_string(),
                        }
                    })?;
                let header_value = reqwest::header::HeaderValue::from_str(value).map_err(|e| {
                    McpError::InvalidHeader {
                        server: name.to_string(),
                        message: e.to_string(),
                    }
                })?;
                header_map.insert(header_name, header_value);
            }
            builder = builder.default_headers(header_map);
        }
        builder.build().map_err(|e| McpError::Connect {
            name: name.to_string(),
            message: e.to_string(),
        })
    }

    /// Serve the MCP handshake over an arbitrary transport and register the
    /// resulting connection. This is the shared tail of every connect path and
    /// is also the seam the in-process tests use (feeding an in-memory duplex
    /// transport). Ports `connectTransport` + the `create` catalog listing
    /// (`index.ts:210`/`index.ts:382-392`).
    pub async fn connect_transport<T, E, A>(
        &self,
        name: impl Into<String>,
        transport: T,
    ) -> Result<McpStatus, McpError>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let name = name.into();
        self.set_status(&name, McpStatus::Connecting);

        let running = match self.client_info().serve(transport).await {
            Ok(running) => running,
            Err(e) => {
                let message = e.to_string();
                self.set_status(&name, McpStatus::Failed(message.clone()));
                return Err(McpError::Connect { name, message });
            }
        };

        self.store(name, running).await
    }

    /// List the server's tools/resources (guarded by advertised capabilities,
    /// like `getServerCapabilities()?.tools` in `index.ts:383`) and store the
    /// connection, then publish [`ToolsChanged`].
    async fn store(
        &self,
        name: String,
        running: RunningService<RoleClient, ClientInfo>,
    ) -> Result<McpStatus, McpError> {
        let caps = running.peer_info().map(|info| info.capabilities.clone());
        let supports_tools = caps.as_ref().is_some_and(|c| c.tools.is_some());
        let supports_resources = caps.as_ref().is_some_and(|c| c.resources.is_some());

        let tools = if supports_tools {
            match running.list_all_tools().await {
                Ok(tools) => tools,
                Err(e) => {
                    let message = e.to_string();
                    self.set_status(&name, McpStatus::Failed(message.clone()));
                    let _ = running.cancel().await;
                    return Err(McpError::Connect { name, message });
                }
            }
        } else {
            Vec::new()
        };

        let resources = if supports_resources {
            running.list_all_resources().await.unwrap_or_default()
        } else {
            Vec::new()
        };

        let connection = Arc::new(Connection {
            name: name.clone(),
            peer: running.peer().clone(),
            tools,
            resources,
        });

        {
            let mut servers = self.servers.lock().unwrap();
            servers.insert(
                name.clone(),
                ServerEntry {
                    status: McpStatus::Connected,
                    running: Some(running),
                    connection: Some(connection),
                },
            );
        }

        self.events.publish(ToolsChanged { server: name });
        Ok(McpStatus::Connected)
    }

    /// Disconnect a server: cancel its background service, mark it disabled,
    /// and publish [`ToolsChanged`]. Ports `MCP.disconnect` (`index.ts:645`).
    pub async fn disconnect(&self, name: &str) -> Result<(), McpError> {
        let running = {
            let mut servers = self.servers.lock().unwrap();
            let entry = servers
                .get_mut(name)
                .ok_or_else(|| McpError::NotFound(name.to_string()))?;
            entry.status = McpStatus::Disabled;
            entry.connection = None;
            entry.running.take()
        };
        if let Some(running) = running {
            let _ = running.cancel().await;
        }
        self.events.publish(ToolsChanged {
            server: name.to_string(),
        });
        Ok(())
    }
}
