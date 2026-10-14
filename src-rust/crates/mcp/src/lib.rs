// cc-mcp: Model Context Protocol (MCP) client implementation.
//
// MCP is a JSON-RPC 2.0 based protocol for connecting Claude to external
// tool/resource servers. This crate implements:
//
// - JSON-RPC 2.0 client primitives
// - MCP protocol handshake (initialize, initialized)
// - Tool discovery (tools/list)
// - Tool execution (tools/call)
// - Resource management (resources/list, resources/read)
// - Prompt templates (prompts/list, prompts/get)
// - Transport: stdio (subprocess) and HTTP/SSE
// - Environment variable expansion in server configs
// - Concurrent startup with per-server connect/catalog timeouts and a bounded
//   retry for remote transports

use async_trait::async_trait;
use claurst_core::config::McpServerConfig;
use claurst_core::mcp_templates::TemplateRenderer;
use claurst_core::types::{ContentBlock, ImageSource, ToolDefinition};
use dashmap::DashMap;
use futures::stream::{BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

pub use client::McpClient;
pub use types::*;

pub mod backend;
pub mod registry;
pub mod oauth;
pub mod rmcp_backend;

// ---------------------------------------------------------------------------
// Environment variable expansion
// ---------------------------------------------------------------------------

/// Expand `${VAR_NAME}` and `${VAR_NAME:-default}` patterns in `input` using
/// the process environment.  Unknown variables without a default are left as-is
/// (matching the TS behaviour: report missing but don't crash).
pub fn expand_env_vars(input: &str) -> String {
    let mut result = input.to_string();
    // We iterate from left to right, always restarting the search after each
    // substitution so that replaced values are not re-scanned.
    let mut search_from = 0;
    loop {
        match result[search_from..].find("${") {
            None => break,
            Some(rel_start) => {
                let start = search_from + rel_start;
                match result[start..].find('}') {
                    None => break, // unclosed brace — stop
                    Some(rel_end) => {
                        let end = start + rel_end; // index of '}'
                        let inner = &result[start + 2..end]; // content between ${ and }

                        // Support ${VAR:-default} syntax
                        let (var_name, default_value) = if let Some(pos) = inner.find(":-") {
                            (&inner[..pos], Some(&inner[pos + 2..]))
                        } else {
                            (inner, None)
                        };

                        let replacement = match std::env::var(var_name) {
                            Ok(val) => val,
                            Err(_) => match default_value {
                                Some(def) => def.to_string(),
                                None => {
                                    // Leave the original text in place; advance past it
                                    search_from = end + 1;
                                    continue;
                                }
                            },
                        };

                        result = format!("{}{}{}", &result[..start], replacement, &result[end + 1..]);
                        // Continue scanning from where the replacement ends
                        search_from = start + replacement.len();
                    }
                }
            }
        }
    }
    result
}

/// Expand env vars in every string field of a `McpServerConfig`.
/// Returns a new owned config; the original is not modified.
pub fn expand_server_config(config: &McpServerConfig) -> McpServerConfig {
    McpServerConfig {
        name: config.name.clone(),
        command: config.command.as_deref().map(expand_env_vars),
        args: config.args.iter().map(|a| expand_env_vars(a)).collect(),
        env: config
            .env
            .iter()
            .map(|(k, v)| (k.clone(), expand_env_vars(v)))
            .collect(),
        url: config.url.as_deref().map(expand_env_vars),
        server_type: config.server_type.clone(),
        headers: config
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), expand_env_vars(v)))
            .collect(),
        connect_timeout_ms: config.connect_timeout_ms,
        catalog_timeout_ms: config.catalog_timeout_ms,
        // Preserve origin: expansion must not launder a project server into
        // a trusted one.
        origin: config.origin,
    }
}

// ---------------------------------------------------------------------------
// Timeouts and connection status
// ---------------------------------------------------------------------------

/// Per-server bounds on startup work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpTimeouts {
    /// Spawning/opening the transport plus the `initialize` handshake. Every
    /// connect attempt for a server (including protocol-version fallbacks)
    /// shares this one deadline.
    pub connect: Duration,
    /// Each catalog request (`tools/list`, `resources/list`, `prompts/list`).
    pub catalog: Duration,
}

impl McpTimeouts {
    pub const DEFAULT_CONNECT: Duration = Duration::from_secs(10);
    pub const DEFAULT_CATALOG: Duration = Duration::from_secs(30);

    pub fn for_config(config: &McpServerConfig) -> Self {
        Self {
            connect: config
                .connect_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(Self::DEFAULT_CONNECT),
            catalog: config
                .catalog_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(Self::DEFAULT_CATALOG),
        }
    }
}

impl Default for McpTimeouts {
    fn default() -> Self {
        Self {
            connect: Self::DEFAULT_CONNECT,
            catalog: Self::DEFAULT_CATALOG,
        }
    }
}

/// A startup step that did not finish within its [`McpTimeouts`] bound.
#[derive(Debug, thiserror::Error)]
#[error("MCP server '{server}': {phase} timed out after {}s", .after.as_secs_f32())]
pub struct McpTimeoutError {
    pub server: String,
    pub phase: &'static str,
    pub after: Duration,
}

/// Live connection status for a single MCP server.
#[derive(Debug, Clone)]
pub enum McpServerStatus {
    /// Successfully connected; reports how many tools were discovered.
    Connected { tool_count: usize },
    /// Connection attempt in progress.
    Connecting,
    /// Not connected. `last_error` is set when the final connect attempt failed.
    Disconnected { last_error: Option<String> },
    /// A connect attempt failed and another attempt is scheduled at `retry_at`.
    Failed { error: String, retry_at: Instant },
}

impl McpServerStatus {
    /// Human-readable one-liner for display in `/mcp status`.
    pub fn display(&self) -> String {
        match self {
            McpServerStatus::Connected { tool_count } => {
                format!("connected ({} tool{})", tool_count, if *tool_count == 1 { "" } else { "s" })
            }
            McpServerStatus::Connecting => "connecting…".to_string(),
            McpServerStatus::Disconnected { last_error: None } => "disconnected".to_string(),
            McpServerStatus::Disconnected { last_error: Some(e) } => {
                format!("failed ({})", e)
            }
            McpServerStatus::Failed { error, retry_at } => {
                let secs = retry_at.saturating_duration_since(Instant::now()).as_secs();
                format!("failed – {} (retry in {}s)", error, secs)
            }
        }
    }

    /// `true` while the server's startup outcome is still open.
    pub fn is_pending(&self) -> bool {
        matches!(self, McpServerStatus::Connecting | McpServerStatus::Failed { .. })
    }
}

/// Counts of servers still starting and servers that failed for good.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpStatusSummary {
    pub connecting: usize,
    pub failed: usize,
}

// ---------------------------------------------------------------------------
// Tool naming and routing
// ---------------------------------------------------------------------------

/// Prefix of every MCP tool name exposed to the model: `mcp__<server>__<tool>`.
pub const MCP_TOOL_PREFIX: &str = "mcp__";

/// Longest exposed tool name. OpenAI-compatible and Gemini APIs cap function
/// names at 64 characters; Anthropic allows 128.
pub const MAX_MCP_TOOL_NAME_LEN: usize = 64;

/// One MCP tool as exposed to the model.
#[derive(Debug, Clone)]
pub struct McpToolEntry {
    pub server_name: String,
    /// The tool's own name on its server.
    pub tool_name: String,
    /// Definition under the exposed `mcp__<server>__<tool>` name.
    pub definition: ToolDefinition,
}

/// Replace every character outside `[A-Za-z0-9_-]` (the set every provider
/// accepts in a tool name) with `_`. An empty input becomes `_`.
pub fn sanitize_tool_name_part(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if sanitized.is_empty() {
        "_".to_string()
    } else {
        sanitized
    }
}

/// The exposed name for `tool` on `server`, unique within `used`.
///
/// Normally `mcp__<server>__<tool>` with both parts sanitized. When that is
/// longer than [`MAX_MCP_TOOL_NAME_LEN`], already taken, or `force_hash` is
/// set (the server's sanitized name is shared with another configured
/// server), it is truncated and suffixed with a hash of the raw
/// `(server, tool)` pair. Calls are routed through the manager's map from
/// exposed name to `(server, tool)`, never by parsing the name back.
fn exposed_tool_name(
    server: &str,
    tool: &str,
    force_hash: bool,
    used: &mut std::collections::HashSet<String>,
) -> String {
    use sha2::{Digest, Sha256};

    let base = format!(
        "{}{}__{}",
        MCP_TOOL_PREFIX,
        sanitize_tool_name_part(server),
        sanitize_tool_name_part(tool)
    );
    if !force_hash && base.len() <= MAX_MCP_TOOL_NAME_LEN && used.insert(base.clone()) {
        return base;
    }
    let mut attempt: u32 = 0;
    loop {
        let mut hasher = Sha256::new();
        hasher.update(server.as_bytes());
        hasher.update([0u8]);
        hasher.update(tool.as_bytes());
        if attempt > 0 {
            hasher.update(attempt.to_le_bytes());
        }
        let digest = hasher.finalize();
        let suffix: String = digest[..4].iter().map(|b| format!("{:02x}", b)).collect();
        // `base` is ASCII after sanitization, so byte truncation is char-safe.
        let keep = (MAX_MCP_TOOL_NAME_LEN - suffix.len() - 1).min(base.len());
        let candidate = format!("{}_{}", &base[..keep], suffix);
        if used.insert(candidate.clone()) {
            return candidate;
        }
        attempt += 1;
    }
}

/// Exposed tool entries for `clients` (sorted by server name), in a stable
/// order: servers by name, tools in server order. Servers in
/// `ambiguous_servers` always get hashed names.
fn build_tool_entries(
    clients: &[(String, Arc<McpClient>)],
    ambiguous_servers: &std::collections::HashSet<String>,
) -> Vec<McpToolEntry> {
    let mut used = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for (server_name, client) in clients {
        let force_hash = ambiguous_servers.contains(server_name);
        for tool in &client.tools {
            let name = exposed_tool_name(server_name, &tool.name, force_hash, &mut used);
            entries.push(McpToolEntry {
                server_name: server_name.clone(),
                tool_name: tool.name.clone(),
                definition: ToolDefinition {
                    name,
                    description: tool.description.clone().unwrap_or_default(),
                    input_schema: tool.input_schema.clone(),
                },
            });
        }
    }
    entries
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 Types
// ---------------------------------------------------------------------------

pub mod types {
    use super::*;

    /// A JSON-RPC 2.0 request.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct JsonRpcRequest {
        pub jsonrpc: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub id: Option<Value>,
        pub method: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub params: Option<Value>,
    }

    impl JsonRpcRequest {
        pub fn new(id: impl Into<Value>, method: impl Into<String>, params: Option<Value>) -> Self {
            Self {
                jsonrpc: "2.0".to_string(),
                id: Some(id.into()),
                method: method.into(),
                params,
            }
        }

        pub fn notification(method: impl Into<String>, params: Option<Value>) -> Self {
            Self {
                jsonrpc: "2.0".to_string(),
                id: None,
                method: method.into(),
                params,
            }
        }
    }


    /// A JSON-RPC 2.0 response.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct JsonRpcResponse {
        pub jsonrpc: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub id: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub error: Option<JsonRpcError>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct JsonRpcError {
        pub code: i64,
        pub message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub data: Option<Value>,
    }

    // ---- MCP protocol types ------------------------------------------------

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    pub struct ServerCapabilities {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub tools: Option<ToolsCapability>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub resources: Option<ResourcesCapability>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub prompts: Option<PromptsCapability>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub logging: Option<Value>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ToolsCapability {
        #[serde(default)]
        pub list_changed: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ResourcesCapability {
        #[serde(default)]
        pub subscribe: bool,
        #[serde(default)]
        pub list_changed: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PromptsCapability {
        #[serde(default)]
        pub list_changed: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ServerInfo {
        pub name: String,
        pub version: String,
    }

    /// An MCP tool definition.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct McpTool {
        pub name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
        pub input_schema: Value,
    }

    impl From<&McpTool> for ToolDefinition {
        fn from(t: &McpTool) -> Self {
            ToolDefinition {
                name: t.name.clone(),
                description: t.description.clone().unwrap_or_default(),
                input_schema: t.input_schema.clone(),
            }
        }
    }

    /// tools/list response.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ListToolsResult {
        pub tools: Vec<McpTool>,
        #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
        pub next_cursor: Option<String>,
    }

    /// tools/call params.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct CallToolParams {
        pub name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub arguments: Option<Value>,
    }

    /// tools/call response.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct CallToolResult {
        pub content: Vec<McpContent>,
        /// Structured result (`structuredContent`). Per the MCP spec, any text
        /// content alongside it is a serialization of the same value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub structured_content: Option<Value>,
        #[serde(default)]
        pub is_error: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "lowercase")]
    pub enum McpContent {
        Text { text: String },
        Image {
            data: String,
            #[serde(rename = "mimeType")]
            mime_type: String,
        },
        Resource { resource: ResourceContents },
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ResourceContents {
        pub uri: String,
        #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
        pub mime_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub blob: Option<String>,
    }

    /// An MCP resource.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct McpResource {
        pub uri: String,
        pub name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub mime_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub annotations: Option<Value>,
    }

    /// resources/list response.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ListResourcesResult {
        pub resources: Vec<McpResource>,
        #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
        pub next_cursor: Option<String>,
    }

    /// An MCP prompt template.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct McpPrompt {
        pub name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
        #[serde(default)]
        pub arguments: Vec<McpPromptArgument>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct McpPromptArgument {
        pub name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
        #[serde(default)]
        pub required: bool,
    }

    /// prompts/list response.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ListPromptsResult {
        pub prompts: Vec<McpPrompt>,
    }

    /// A single message returned by prompts/get.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PromptMessage {
        /// "user" or "assistant"
        pub role: String,
        pub content: PromptMessageContent,
    }

    /// Content inside a PromptMessage.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "lowercase")]
    pub enum PromptMessageContent {
        Text { text: String },
        Image { data: String, mime_type: String },
        Resource { resource: serde_json::Value },
    }

    /// prompts/get response.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct GetPromptResult {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
        pub messages: Vec<PromptMessage>,
    }
}

// ---------------------------------------------------------------------------
// Transport layer
// ---------------------------------------------------------------------------

pub mod transport {
    use super::*;
    use reqwest::header::{CONTENT_TYPE, HeaderValue};
    #[cfg(test)]
    use tokio::sync::mpsc;

    pub const LEGACY_PROTOCOL_VERSION: &str = "2024-11-05";
    pub const STREAMABLE_HTTP_PROTOCOL_VERSION: &str = "2025-11-25";
    pub const STREAMABLE_HTTP_PROTOCOL_VERSIONS: &[&str] = &[
        STREAMABLE_HTTP_PROTOCOL_VERSION,
        "2025-06-18",
        "2025-03-26",
        LEGACY_PROTOCOL_VERSION,
    ];

    /// A transport can send requests and receive responses.
    #[async_trait]
    pub trait McpTransport: Send + Sync {
        async fn send(&self, message: &JsonRpcRequest) -> anyhow::Result<()>;
        async fn recv(&self) -> anyhow::Result<Option<JsonRpcResponse>>;
        async fn close(&self) -> anyhow::Result<()>;
        /// Non-blocking poll: return the next raw JSON message if one is
        /// immediately available, or `Ok(None)` if the queue is empty.
        /// Used by the notification dispatch loop to drain server-initiated
        /// notifications without blocking an async task.
        async fn try_receive_raw(&self) -> anyhow::Result<Option<serde_json::Value>>;
        /// Subscribe to raw JSON notifications from the transport.
        /// Returns an async stream of notification messages.
        ///
        /// For transports that natively support push notifications (e.g., WebSocket),
        /// this returns a stream that yields messages directly from the transport.
        /// For transports without native push support (e.g., stdio), this returns
        /// a stream that polls periodically.
        fn subscribe_to_notifications(
            &self,
        ) -> BoxStream<'static, anyhow::Result<serde_json::Value>>;

        fn protocol_version(&self) -> &'static str {
            LEGACY_PROTOCOL_VERSION
        }
    }

    pub(crate) fn bearer_header_value(token: &str) -> anyhow::Result<HeaderValue> {
        HeaderValue::from_str(&format!("Bearer {}", token))
            .map_err(|e| anyhow::anyhow!("invalid bearer token header: {}", e))
    }

    pub(crate) fn is_event_stream_response(response: &reqwest::Response) -> bool {
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.contains("text/event-stream"))
            .unwrap_or(false)
    }

    pub(crate) fn resolve_legacy_endpoint(base_url: &str, endpoint: &str) -> anyhow::Result<String> {
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            anyhow::bail!("legacy SSE endpoint event did not include a POST endpoint");
        }
        if let Ok(url) = url::Url::parse(endpoint) {
            return Ok(url.to_string());
        }
        let base = url::Url::parse(base_url)
            .map_err(|e| anyhow::anyhow!("invalid legacy SSE base URL '{}': {}", base_url, e))?;
        base.join(endpoint)
            .map(|url| url.to_string())
            .map_err(|e| anyhow::anyhow!("failed to resolve legacy SSE endpoint '{}': {}", endpoint, e))
    }

    #[cfg(test)]
    pub(super) fn route_incoming_value(
        server_name: &str,
        value: serde_json::Value,
        response_tx: &mpsc::UnboundedSender<JsonRpcResponse>,
        notification_tx: &mpsc::UnboundedSender<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let is_response = value.get("id").map(|id| !id.is_null()).unwrap_or(false)
            || value.get("result").is_some()
            || value.get("error").is_some();

        if is_response {
            let response: JsonRpcResponse = serde_json::from_value(value).map_err(|e| {
                anyhow::anyhow!(
                    "MCP server '{}': failed to parse JSON-RPC response from HTTP transport: {}",
                    server_name,
                    e
                )
            })?;
            let _ = response_tx.send(response);
            return Ok(());
        }

        if value.get("method").is_some() {
            let _ = notification_tx.send(value);
        }

        Ok(())
    }

    fn dispatch_sse_event<F>(
        event_name: &mut Option<String>,
        data_lines: &mut Vec<String>,
        on_event: &mut F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(Option<&str>, &str) -> anyhow::Result<()>,
    {
        if event_name.is_none() && data_lines.is_empty() {
            return Ok(());
        }
        let data = data_lines.join("\n");
        on_event(event_name.as_deref(), &data)?;
        *event_name = None;
        data_lines.clear();
        Ok(())
    }

    pub(super) fn process_sse_line(
        line: &str,
        event_name: &mut Option<String>,
        data_lines: &mut Vec<String>,
    ) {
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => *event_name = Some(value.to_string()),
            "data" => data_lines.push(value.to_string()),
            _ => {}
        }
    }

    pub(crate) async fn process_sse_response<F>(
        response: reqwest::Response,
        mut on_event: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(Option<&str>, &str) -> anyhow::Result<()>,
    {
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut event_name: Option<String> = None;
        let mut data_lines: Vec<String> = Vec::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| anyhow::anyhow!("failed reading SSE stream: {}", e))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find('\n') {
                let mut line: String = buffer.drain(..=pos).collect();
                if line.ends_with('\n') {
                    line.pop();
                }
                if line.ends_with('\r') {
                    line.pop();
                }
                if line.is_empty() {
                    dispatch_sse_event(&mut event_name, &mut data_lines, &mut on_event)?;
                } else {
                    process_sse_line(&line, &mut event_name, &mut data_lines);
                }
            }
        }

        if !buffer.is_empty() {
            if buffer.ends_with('\r') {
                buffer.pop();
            }
            if buffer.is_empty() {
                dispatch_sse_event(&mut event_name, &mut data_lines, &mut on_event)?;
            } else {
                process_sse_line(&buffer, &mut event_name, &mut data_lines);
            }
        }

        dispatch_sse_event(&mut event_name, &mut data_lines, &mut on_event)?;
        Ok(())
    }

}

// ---------------------------------------------------------------------------
// MCP Client
// ---------------------------------------------------------------------------

pub mod client {
    use super::*;

    /// A fully initialized MCP client connected to a single server.
    pub struct McpClient {
        pub server_name: String,
        pub server_info: Option<ServerInfo>,
        pub capabilities: ServerCapabilities,
        pub tools: Vec<McpTool>,
        pub resources: Vec<McpResource>,
        pub prompts: Vec<McpPrompt>,
        pub instructions: Option<String>,
        backend: Option<Arc<dyn backend::McpClientBackend>>,
    }

    impl McpClient {
        fn from_snapshot(snapshot: backend::McpClientSnapshot) -> Self {
            Self {
                server_name: snapshot.server_name,
                server_info: snapshot.server_info,
                capabilities: snapshot.capabilities,
                tools: snapshot.tools,
                resources: snapshot.resources,
                prompts: snapshot.prompts,
                instructions: snapshot.instructions,
                backend: None,
            }
        }

        fn from_backend(backend: Arc<dyn backend::McpClientBackend>) -> Self {
            let snapshot = backend.snapshot();
            let mut client = Self::from_snapshot(snapshot);
            client.backend = Some(backend);
            client
        }

        fn backend(&self) -> anyhow::Result<&Arc<dyn backend::McpClientBackend>> {
            self.backend
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("MCP client backend missing"))
        }

        /// Connect and load the catalog. The transport and handshake share one
        /// deadline of `timeouts.connect`; each catalog request is bounded by
        /// `timeouts.catalog`. The `config` should already have env vars
        /// expanded via `expand_server_config`.
        pub async fn connect(
            config: &McpServerConfig,
            auth_token: Option<String>,
            timeouts: McpTimeouts,
        ) -> anyhow::Result<Self> {
            let deadline = tokio::time::Instant::now() + timeouts.connect;
            // An explicit `Authorization` header wins over the stored token.
            let auth_token = auth_token.filter(|_| {
                !config
                    .headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("authorization"))
            });
            match config.server_type.as_str() {
                "stdio" => {
                    let backend = crate::rmcp_backend::RmcpClientBackend::connect_stdio(
                        config, deadline, timeouts,
                    )
                    .await?;
                    Ok(Self::from_backend(Arc::new(backend)))
                }
                "sse" => {
                    let backend = crate::rmcp_backend::RmcpClientBackend::connect_legacy_sse(
                        config,
                        auth_token,
                        deadline,
                        timeouts,
                    )
                    .await?;
                    Ok(Self::from_backend(Arc::new(backend)))
                }
                "http" => {
                    let mut last_error = None;
                    for &protocol_version in transport::STREAMABLE_HTTP_PROTOCOL_VERSIONS {
                        let protocol_version = serde_json::from_value::<rmcp::model::ProtocolVersion>(
                            Value::String(protocol_version.to_string()),
                        )
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "MCP server '{}': invalid streamable HTTP protocol version '{}': {}",
                                config.name,
                                protocol_version,
                                e
                            )
                        })?;
                        let protocol_version_label = protocol_version.as_str().to_string();
                        match crate::rmcp_backend::RmcpClientBackend::connect_http(
                            config,
                            auth_token.clone(),
                            protocol_version,
                            deadline,
                            timeouts,
                        )
                        .await
                        {
                            Ok(backend) => return Ok(Self::from_backend(Arc::new(backend))),
                            Err(e) => {
                                let message = e.to_string();
                                if Self::is_unsupported_protocol_error(&message) {
                                    debug!(
                                        server = %config.name,
                                        protocol_version = %protocol_version_label,
                                        error = %message,
                                        "Streamable HTTP protocol version unsupported; trying fallback"
                                    );
                                    last_error = Some(e);
                                    continue;
                                }
                                return Err(e);
                            }
                        }
                    }
                    Err(last_error.unwrap_or_else(|| {
                        anyhow::anyhow!(
                            "MCP server '{}': no supported streamable HTTP protocol version found",
                            config.name
                        )
                    }))
                }
                other => Err(anyhow::anyhow!(
                    "MCP server '{}': unsupported transport type '{}'",
                    config.name,
                    other
                )),
            }
        }

        pub(crate) fn is_unsupported_protocol_error(message: &str) -> bool {
            // rmcp, servers, and gateways do not emit a single stable
            // protocol-negotiation error string, so match the known variants
            // conservatively to keep downgrade fallback working.
            let lower = message.to_ascii_lowercase();
            lower.contains("unsupported protocol version")
                || lower.contains("unsupported mcp-protocol-version")
                || (lower.contains("protocol version") && lower.contains("unsupported"))
                || (lower.contains("mcp-protocol-version") && lower.contains("bad request"))
        }

        // ---- High-level API -----------------------------------------------

        pub async fn list_tools(&self) -> anyhow::Result<Vec<McpTool>> {
            self.backend()?.list_tools().await
        }

        pub async fn call_tool(
            &self,
            name: &str,
            arguments: Option<Value>,
        ) -> anyhow::Result<CallToolResult> {
            self.backend()?.call_tool(name, arguments).await.map_err(|e| {
                anyhow::anyhow!(
                    "MCP server '{}': tool '{}' call failed: {}",
                    self.server_name,
                    name,
                    e
                )
            })
        }

        pub async fn list_resources(&self) -> anyhow::Result<Vec<McpResource>> {
            let mut resources = self.backend()?.list_resources().await?;
            apply_resource_templates(&mut resources);
            Ok(resources)
        }

        pub async fn read_resource(&self, uri: &str) -> anyhow::Result<ResourceContents> {
            self.backend()?.read_resource(uri).await
        }

        pub async fn list_prompts(&self) -> anyhow::Result<Vec<McpPrompt>> {
            self.backend()?.list_prompts().await
        }

        /// Invoke `prompts/get` with the given name and optional arguments map.
        ///
        /// Returns the expanded prompt messages that should be injected into the
        /// conversation as-is (mirrors TS `getMCPPrompt`).
        pub async fn get_prompt(
            &self,
            name: &str,
            arguments: Option<std::collections::HashMap<String, String>>,
        ) -> anyhow::Result<GetPromptResult> {
            self.backend()?.get_prompt(name, arguments).await
        }

        pub async fn subscribe_resource(&self, uri: &str) -> anyhow::Result<()> {
            self.backend()?.subscribe_resource(uri).await
        }

        pub async fn unsubscribe_resource(&self, uri: &str) -> anyhow::Result<()> {
            self.backend()?.unsubscribe_resource(uri).await
        }

        /// Get all tools as `ToolDefinition` objects suitable for the API.
        pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
            self.tools.iter().map(|t| t.into()).collect()
        }

        /// Subscribe to raw JSON notifications from the active backend.
        pub fn subscribe_to_notifications(
            &self,
        ) -> anyhow::Result<BoxStream<'static, anyhow::Result<serde_json::Value>>> {
            Ok(self.backend()?.subscribe_to_notifications())
        }

        /// A copy of this client with a refreshed tool list, sharing the same
        /// live backend session.
        pub(crate) fn with_tools(&self, tools: Vec<McpTool>) -> Self {
            Self {
                server_name: self.server_name.clone(),
                server_info: self.server_info.clone(),
                capabilities: self.capabilities.clone(),
                tools,
                resources: self.resources.clone(),
                prompts: self.prompts.clone(),
                instructions: self.instructions.clone(),
                backend: self.backend.clone(),
            }
        }

        #[cfg(test)]
        pub fn new_for_test(server_name: impl Into<String>) -> Self {
            Self {
                server_name: server_name.into(),
                server_info: None,
                capabilities: ServerCapabilities::default(),
                tools: vec![],
                resources: vec![],
                prompts: vec![],
                instructions: None,
                backend: None,
            }
        }
    }

    fn apply_resource_templates(resources: &mut [McpResource]) {
        for resource in resources {
            if let Some(annotations) = &resource.annotations {
                if let Some(prompt_template) = annotations.get("prompt") {
                    if let Some(template_str) = prompt_template.as_str() {
                        let context = serde_json::json!({
                            "uri": resource.uri,
                            "name": resource.name,
                            "description": resource.description,
                            "mimeType": resource.mime_type,
                        });
                        let rendered = TemplateRenderer::render(template_str, &context);
                        resource.description = Some(rendered);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MCP Auth State
// ---------------------------------------------------------------------------

/// Authentication state for a single MCP server.
#[derive(Debug, Clone)]
pub enum McpAuthState {
    /// Server does not require OAuth authentication.
    NotRequired,
    /// OAuth required; `auth_url` is where the user should go.
    Required { auth_url: String },
    /// Successfully authenticated; token may have an expiry.
    Authenticated { token_expiry: Option<chrono::DateTime<chrono::Utc>> },
    /// An error occurred reading / initiating auth.
    Error(String),
}

// ---------------------------------------------------------------------------
// MCP Manager: manages multiple server connections
// ---------------------------------------------------------------------------

/// Manages a pool of MCP server connections.
///
/// [`McpManager::start`] registers every server as connecting and connects
/// them concurrently in background tasks, so callers (the TUI) never wait on
/// a slow or hung server. Status and catalog changes bump a generation
/// counter that callers poll ([`McpManager::generation`]) to rebuild the
/// tool list; headless callers await [`McpManager::wait_until_settled`].
pub struct McpManager {
    /// Original (unexpanded) server configs — needed for OAuth initiation.
    server_configs: HashMap<String, McpServerConfig>,
    state: parking_lot::RwLock<ManagerState>,
    /// Generation counter, bumped on every status or catalog change.
    changes: tokio::sync::watch::Sender<u64>,
    /// Active resource subscriptions: (server_name, uri) → change event sender.
    pub resource_subscriptions: DashMap<(String, String), tokio::sync::mpsc::Sender<ResourceChangedEvent>>,
}

/// Mutable connection state. The lock is never held across an `.await`.
#[derive(Default)]
struct ManagerState {
    clients: HashMap<String, Arc<McpClient>>,
    statuses: HashMap<String, McpServerStatus>,
    /// Exposed tools, rebuilt whenever a client or its tool list changes.
    tools: Vec<McpToolEntry>,
    /// Exposed tool name → index into `tools`.
    routes: HashMap<String, usize>,
    /// Configured servers whose sanitized name collides with another
    /// configured server's. Fixed at start so exposed names do not depend on
    /// which of them connected first.
    ambiguous_servers: std::collections::HashSet<String>,
}

impl ManagerState {
    fn rebuild_tools(&mut self) {
        let mut clients: Vec<(String, Arc<McpClient>)> = self
            .clients
            .iter()
            .map(|(name, client)| (name.clone(), Arc::clone(client)))
            .collect();
        clients.sort_by(|a, b| a.0.cmp(&b.0));
        self.tools = build_tool_entries(&clients, &self.ambiguous_servers);
        self.routes = self
            .tools
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.definition.name.clone(), index))
            .collect();
    }
}

#[derive(Debug, Clone)]
pub struct McpServerCatalog {
    pub tool_count: usize,
    pub resource_count: usize,
    pub prompt_count: usize,
    pub resources: Vec<String>,
    pub prompts: Vec<String>,
}

/// Remote (`http`/`sse`) connects get this many attempts in total. Stdio
/// servers get one: a process that fails to start is not a transient fault.
const MAX_REMOTE_CONNECT_ATTEMPTS: u32 = 3;
/// Delay before the second remote attempt; doubled for each later attempt.
const REMOTE_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

impl McpManager {
    pub fn new() -> Self {
        Self {
            server_configs: HashMap::new(),
            state: parking_lot::RwLock::new(ManagerState::default()),
            changes: tokio::sync::watch::Sender::new(0),
            resource_subscriptions: DashMap::new(),
        }
    }

    /// Register `configs` and connect to every server concurrently in the
    /// background. Returns immediately; every supported server starts out
    /// [`McpServerStatus::Connecting`]. Must be called inside a Tokio runtime.
    pub fn start(configs: &[McpServerConfig]) -> Arc<Self> {
        let mut manager = Self::new();
        for config in configs {
            let status = match config.server_type.as_str() {
                "stdio" | "sse" | "http" => McpServerStatus::Connecting,
                other => {
                    warn!(
                        server = %config.name,
                        transport = other,
                        "Unsupported MCP transport type; skipping server"
                    );
                    McpServerStatus::Disconnected {
                        last_error: Some(format!("unsupported transport: {}", other)),
                    }
                }
            };
            manager.state.get_mut().statuses.insert(config.name.clone(), status);
            manager.server_configs.insert(config.name.clone(), config.clone());
        }
        let mut by_sanitized: HashMap<String, Vec<&String>> = HashMap::new();
        for name in manager.server_configs.keys() {
            by_sanitized.entry(sanitize_tool_name_part(name)).or_default().push(name);
        }
        let ambiguous: std::collections::HashSet<String> = by_sanitized
            .into_values()
            .filter(|names| names.len() > 1)
            .flatten()
            .cloned()
            .collect();
        manager.state.get_mut().ambiguous_servers = ambiguous;

        let manager = Arc::new(manager);
        let pending: Vec<McpServerConfig> = manager
            .server_configs
            .values()
            .filter(|config| matches!(config.server_type.as_str(), "stdio" | "sse" | "http"))
            .cloned()
            .collect();
        for config in pending {
            tokio::spawn(Self::connect_server(Arc::downgrade(&manager), config));
        }
        manager
    }

    /// Connect to all configured MCP servers and wait until each one has
    /// either connected or failed (bounded by the per-server timeouts).
    pub async fn connect_all(configs: &[McpServerConfig]) -> Arc<Self> {
        let manager = Self::start(configs);
        manager.wait_until_settled().await;
        manager
    }

    /// Connect one server: env expansion, connect/catalog timeouts and, for
    /// remote transports, a bounded retry with backoff. Holds only a weak
    /// reference so dropping the manager is never delayed by a slow server.
    async fn connect_server(manager: Weak<Self>, config: McpServerConfig) {
        let name = config.name.clone();
        let expanded = expand_server_config(&config);
        for (header, value) in &expanded.headers {
            if value.contains("${") {
                warn!(
                    server = %name,
                    header = %header,
                    "MCP header value still contains an unexpanded ${{VAR}} after expansion"
                );
            }
        }
        let timeouts = McpTimeouts::for_config(&expanded);
        let remote = matches!(expanded.server_type.as_str(), "sse" | "http");
        debug!(server = %name, transport = %expanded.server_type, "Connecting to MCP server");

        let mut attempt = 1;
        let result = loop {
            let auth_token = match config.url.as_deref() {
                Some(server_url) if remote => {
                    oauth::get_valid_mcp_access_token(&name, server_url)
                        .await
                        .ok()
                        .flatten()
                }
                _ => None,
            };
            let error = match client::McpClient::connect(&expanded, auth_token, timeouts).await {
                Ok(client) => break Ok(client),
                Err(error) => error,
            };
            // A timed-out attempt already spent the whole connect budget.
            let retry = remote
                && attempt < MAX_REMOTE_CONNECT_ATTEMPTS
                && error.downcast_ref::<McpTimeoutError>().is_none();
            if !retry {
                break Err(error);
            }
            let delay = REMOTE_RETRY_BASE_DELAY * 2u32.pow(attempt - 1);
            warn!(
                server = %name,
                attempt,
                retry_in_ms = delay.as_millis() as u64,
                error = %error,
                "MCP connect attempt failed; retrying"
            );
            let Some(strong) = manager.upgrade() else { return };
            strong.set_status(
                &name,
                McpServerStatus::Failed {
                    error: error.to_string(),
                    retry_at: Instant::now() + delay,
                },
            );
            drop(strong);
            tokio::time::sleep(delay).await;
            let Some(strong) = manager.upgrade() else { return };
            strong.set_status(&name, McpServerStatus::Connecting);
            attempt += 1;
        };

        let Some(manager) = manager.upgrade() else { return };
        match result {
            Ok(client) => {
                info!(
                    server = %name,
                    transport = %expanded.server_type,
                    tools = client.tools.len(),
                    resources = client.resources.len(),
                    "MCP server connected"
                );
                manager.install_client(client);
            }
            Err(e) => {
                error!(
                    server = %name,
                    transport = %expanded.server_type,
                    error = %e,
                    "Failed to connect to MCP server"
                );
                manager.set_status(
                    &name,
                    McpServerStatus::Disconnected {
                        last_error: Some(e.to_string()),
                    },
                );
            }
        }
    }

    /// Record a connected client and start listening for its notifications.
    fn install_client(self: &Arc<Self>, client: McpClient) {
        let name = client.server_name.clone();
        let notifications = client.subscribe_to_notifications();
        let tool_count = client.tools.len();
        {
            let mut state = self.state.write();
            state.clients.insert(name.clone(), Arc::new(client));
            state
                .statuses
                .insert(name.clone(), McpServerStatus::Connected { tool_count });
            state.rebuild_tools();
        }
        self.bump();
        match notifications {
            Ok(stream) => {
                tokio::spawn(Self::listen_for_notifications(Arc::downgrade(self), name, stream));
            }
            Err(e) => warn!(server = %name, error = %e, "MCP notifications unavailable"),
        }
    }

    /// Route server-initiated notifications until the transport closes or
    /// the manager is dropped.
    async fn listen_for_notifications(
        manager: Weak<Self>,
        server_name: String,
        mut stream: BoxStream<'static, anyhow::Result<serde_json::Value>>,
    ) {
        while let Some(item) = stream.next().await {
            let Some(manager) = manager.upgrade() else { break };
            match item {
                Ok(raw) => {
                    let effect =
                        process_notification(&server_name, raw, &manager.resource_subscriptions).await;
                    if effect == NotificationEffect::ToolsChanged {
                        manager.refresh_tools(&server_name).await;
                    }
                }
                Err(e) => {
                    debug!(server = %server_name, error = %e, "notification stream error");
                    break;
                }
            }
        }
    }

    /// Re-list a server's tools after `notifications/tools/list_changed`.
    pub async fn refresh_tools(&self, server_name: &str) {
        let Some(client) = self.client(server_name) else { return };
        let catalog = self
            .server_configs
            .get(server_name)
            .map(McpTimeouts::for_config)
            .unwrap_or_default()
            .catalog;
        let tools = match tokio::time::timeout(catalog, client.list_tools()).await {
            Ok(Ok(tools)) => tools,
            Ok(Err(e)) => {
                warn!(server = %server_name, error = %e, "MCP tools refresh failed");
                return;
            }
            Err(_) => {
                warn!(
                    server = %server_name,
                    timeout_secs = catalog.as_secs_f32(),
                    "MCP tools refresh timed out"
                );
                return;
            }
        };
        let tool_count = tools.len();
        let updated = Arc::new(client.with_tools(tools));
        {
            let mut state = self.state.write();
            // A reconnect may have replaced the client while we were listing.
            match state.clients.get(server_name) {
                Some(current) if Arc::ptr_eq(current, &client) => {}
                _ => return,
            }
            state.clients.insert(server_name.to_string(), updated);
            state
                .statuses
                .insert(server_name.to_string(), McpServerStatus::Connected { tool_count });
            state.rebuild_tools();
        }
        self.bump();
        info!(server = %server_name, tools = tool_count, "MCP tool list refreshed");
    }

    fn set_status(&self, name: &str, status: McpServerStatus) {
        self.state.write().statuses.insert(name.to_string(), status);
        self.bump();
    }

    fn bump(&self) {
        self.changes.send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    fn client(&self, name: &str) -> Option<Arc<McpClient>> {
        self.state.read().clients.get(name).cloned()
    }

    /// Connected clients, sorted by server name.
    fn clients_sorted(&self) -> Vec<(String, Arc<McpClient>)> {
        let mut clients: Vec<(String, Arc<McpClient>)> = self
            .state
            .read()
            .clients
            .iter()
            .map(|(name, client)| (name.clone(), Arc::clone(client)))
            .collect();
        clients.sort_by(|a, b| a.0.cmp(&b.0));
        clients
    }

    /// Changes whenever a server's status or tool catalog changes. Callers
    /// that cache the tool list rebuild it when this differs from the value
    /// they last saw.
    pub fn generation(&self) -> u64 {
        *self.changes.borrow()
    }

    /// Wait until no server is connecting or waiting to retry. Bounded by
    /// the per-server connect and catalog timeouts.
    pub async fn wait_until_settled(&self) {
        let mut changes = self.changes.subscribe();
        loop {
            if !self.state.read().statuses.values().any(McpServerStatus::is_pending) {
                return;
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    }

    /// Number of servers still starting and servers whose startup failed.
    pub fn status_summary(&self) -> McpStatusSummary {
        let state = self.state.read();
        let mut summary = McpStatusSummary::default();
        for status in state.statuses.values() {
            match status {
                McpServerStatus::Connecting | McpServerStatus::Failed { .. } => {
                    summary.connecting += 1
                }
                McpServerStatus::Disconnected { last_error: Some(_) } => summary.failed += 1,
                _ => {}
            }
        }
        summary
    }

    // -----------------------------------------------------------------------
    // Status / query API (used by /mcp command and the TUI)
    // -----------------------------------------------------------------------

    /// Return all connected server names, sorted.
    pub fn server_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.state.read().clients.keys().cloned().collect();
        names.sort();
        names
    }

    /// Return status for a single server.
    pub fn server_status(&self, name: &str) -> McpServerStatus {
        self.state
            .read()
            .statuses
            .get(name)
            .cloned()
            .unwrap_or(McpServerStatus::Disconnected { last_error: None })
    }

    /// Return status for every configured server.
    pub fn all_statuses(&self) -> HashMap<String, McpServerStatus> {
        self.state.read().statuses.clone()
    }

    /// Servers whose startup failed for good, as `(server_name, error)`.
    pub fn failed_servers(&self) -> Vec<(String, String)> {
        let mut failed: Vec<(String, String)> = self
            .state
            .read()
            .statuses
            .iter()
            .filter_map(|(name, status)| match status {
                McpServerStatus::Disconnected { last_error: Some(e) } => {
                    Some((name.clone(), e.clone()))
                }
                _ => None,
            })
            .collect();
        failed.sort();
        failed
    }

    /// Return counts and names for tools/resources/prompts on connected servers.
    pub fn server_catalog(&self, name: &str) -> Option<McpServerCatalog> {
        let client = self.client(name)?;
        Some(McpServerCatalog {
            tool_count: client.tools.len(),
            resource_count: client.resources.len(),
            prompt_count: client.prompts.len(),
            resources: client.resources.iter().map(|r| r.name.clone()).collect(),
            prompts: client.prompts.iter().map(|p| p.name.clone()).collect(),
        })
    }

    // -----------------------------------------------------------------------
    // Tool / resource helpers
    // -----------------------------------------------------------------------

    /// Every tool of every connected server under its exposed
    /// `mcp__<server>__<tool>` name, in a stable order.
    pub fn all_tool_definitions(&self) -> Vec<McpToolEntry> {
        self.state.read().tools.clone()
    }

    /// Execute a tool call by its exposed name, routed through the
    /// exposed-name map to the owning server.
    pub async fn call_tool(
        &self,
        exposed_name: &str,
        arguments: Option<Value>,
    ) -> anyhow::Result<CallToolResult> {
        let (client, tool_name) = {
            let state = self.state.read();
            let entry = state
                .routes
                .get(exposed_name)
                .and_then(|&index| state.tools.get(index))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Unknown MCP tool '{}' (its server may have disconnected or removed it)",
                        exposed_name
                    )
                })?;
            let client = state.clients.get(&entry.server_name).cloned().ok_or_else(|| {
                anyhow::anyhow!("MCP server '{}' is not connected", entry.server_name)
            })?;
            (client, entry.tool_name.clone())
        };
        client.call_tool(&tool_name, arguments).await
    }

    /// Number of connected servers.
    pub fn server_count(&self) -> usize {
        self.state.read().clients.len()
    }

    /// The system-prompt section carrying server instructions (from the
    /// `initialize` response) for every connected server with at least one
    /// tool for which `is_active` returns `true` (checked against exposed
    /// `mcp__<server>__<tool>` names). `None` when no server qualifies.
    ///
    /// Recomputed per request, so it belongs in the dynamic (uncached) part
    /// of the system prompt: servers connect, disconnect and change tools
    /// during a session.
    pub fn instructions_prompt_section(&self, is_active: impl Fn(&str) -> bool) -> Option<String> {
        let state = self.state.read();
        let mut servers: Vec<(&String, &Arc<McpClient>)> = state.clients.iter().collect();
        servers.sort_by(|a, b| a.0.cmp(b.0));
        let sections: Vec<String> = servers
            .into_iter()
            .filter_map(|(name, client)| {
                let instructions = client.instructions.as_deref()?.trim();
                if instructions.is_empty() {
                    return None;
                }
                let has_active_tool = state
                    .tools
                    .iter()
                    .any(|entry| &entry.server_name == name && is_active(&entry.definition.name));
                has_active_tool.then(|| format!("## {}\n{}", name, instructions))
            })
            .collect();
        if sections.is_empty() {
            return None;
        }
        Some(format!(
            "# MCP Server Instructions\n\nThe following MCP servers have provided instructions for how to use their tools and resources:\n\n{}",
            sections.join("\n\n")
        ))
    }

    /// Get server instructions (from initialize response).
    pub fn server_instructions(&self) -> Vec<(String, String)> {
        self.clients_sorted()
            .into_iter()
            .filter_map(|(name, client)| client.instructions.clone().map(|instr| (name, instr)))
            .collect()
    }

    /// List all resources from all (or a specific) connected server.
    pub async fn list_all_resources(
        &self,
        server_filter: Option<&str>,
    ) -> Vec<serde_json::Value> {
        let mut all = vec![];
        for (name, client) in self.clients_sorted() {
            if let Some(filter) = server_filter {
                if name != filter {
                    continue;
                }
            }
            match client.list_resources().await {
                Ok(resources) => {
                    for r in resources {
                        all.push(serde_json::json!({
                            "uri": r.uri,
                            "name": r.name,
                            "description": r.description,
                            "mimeType": r.mime_type,
                            "server": name,
                        }));
                    }
                }
                Err(e) => {
                    warn!(server = %name, error = %e, "Failed to list resources");
                }
            }
        }
        all
    }

    /// Read a specific resource from a named server.
    pub async fn read_resource(
        &self,
        server_name: &str,
        uri: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let client = self
            .client(server_name)
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not found or not connected", server_name))?;

        let contents = client.read_resource(uri).await?;
        Ok(serde_json::to_value(&contents)?)
    }

    /// List all prompts from all (or a specific) connected server.
    pub async fn list_all_prompts(
        &self,
        server_filter: Option<&str>,
    ) -> Vec<serde_json::Value> {
        let mut all = vec![];
        for (name, client) in self.clients_sorted() {
            if let Some(filter) = server_filter {
                if name != filter {
                    continue;
                }
            }
            match client.list_prompts().await {
                Ok(prompts) => {
                    for p in prompts {
                        all.push(serde_json::json!({
                            "name": p.name,
                            "description": p.description,
                            "arguments": p.arguments,
                            "server": name,
                        }));
                    }
                }
                Err(e) => {
                    warn!(server = %name, error = %e, "Failed to list prompts");
                }
            }
        }
        all
    }

    /// Get an expanded prompt from a named server by prompt name and arguments.
    ///
    /// Returns the `GetPromptResult` with fully-rendered messages suitable for
    /// injection into the conversation (mirrors TS `getMCPPrompt`).
    pub async fn get_prompt(
        &self,
        server_name: &str,
        prompt_name: &str,
        arguments: Option<std::collections::HashMap<String, String>>,
    ) -> anyhow::Result<GetPromptResult> {
        let client = self
            .client(server_name)
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not found or not connected", server_name))?;
        client.get_prompt(prompt_name, arguments).await
    }

    // -----------------------------------------------------------------------
    // OAuth / authentication helpers
    // -----------------------------------------------------------------------

    /// Return the current authentication state for a server.
    ///
    /// - Returns `Authenticated` if a valid (non-expired) token exists on disk.
    /// - Returns `NotRequired` for stdio servers.
    /// - Returns `Required` for remote `http` / `sse` servers that lack a valid token.
    pub fn auth_state(&self, server_name: &str) -> McpAuthState {
        let config = match self.server_configs.get(server_name) {
            Some(c) => c,
            None => return McpAuthState::NotRequired,
        };

        if !matches!(config.server_type.as_str(), "http" | "sse") {
            return McpAuthState::NotRequired;
        }

        if let Some(token) = oauth::get_mcp_token(server_name) {
            if !token.is_expired(60) {
                return McpAuthState::Authenticated {
                    token_expiry: token.expiry_datetime(),
                };
            }

            if token.refresh_token.is_some() {
                return McpAuthState::Required {
                    auth_url: format!(
                        "{} (refreshable token detected; connection will try to refresh automatically)",
                        config.url
                            .clone()
                            .unwrap_or_else(|| "(unknown URL)".to_string())
                    ),
                };
            }
        }

        McpAuthState::Required {
            auth_url: config
                .url
                .clone()
                .unwrap_or_else(|| "(unknown URL)".to_string()),
        }
    }

    /// Initiate OAuth 2.0 + PKCE for an HTTP MCP server.
    pub async fn initiate_auth(&self, server_name: &str) -> anyhow::Result<String> {
        Ok(self.begin_auth(server_name).await?.auth_url)
    }

    /// Build a full OAuth authorization session for an HTTP/SSE MCP server.
    pub async fn begin_auth(&self, server_name: &str) -> anyhow::Result<oauth::McpAuthSession> {
        let config = self
            .server_configs
            .get(server_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown MCP server: {}", server_name))?;

        let base_url = config
            .url
            .as_deref()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "MCP server '{}' has no URL configured (required for OAuth)",
                    server_name
                )
            })?;

        oauth::begin_mcp_auth(server_name, base_url).await
    }

    /// Run the browser-based OAuth flow and persist the resulting token.
    pub async fn authenticate(&self, server_name: &str) -> anyhow::Result<oauth::McpAuthResult> {
        let config = self
            .server_configs
            .get(server_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown MCP server: {}", server_name))?;

        let base_url = config
            .url
            .as_deref()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "MCP server '{}' has no URL configured (required for OAuth)",
                    server_name
                )
            })?;

        oauth::run_mcp_auth_flow(server_name, base_url).await
    }

    /// Store an OAuth access token for an MCP server.
    ///
    /// `expires_in` is the lifetime in seconds (as returned by the token endpoint).
    pub fn store_token(
        &self,
        server_name: &str,
        token: &str,
        expires_in: Option<u64>,
    ) -> anyhow::Result<()> {
        let expires_at = expires_in.map(|secs| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                + secs
        });
        let mcp_token = oauth::McpToken {
            access_token: token.to_string(),
            refresh_token: None,
            expires_at,
            scope: None,
            server_name: server_name.to_string(),
        };
        oauth::store_mcp_token(&mcp_token)
            .map_err(|e| anyhow::anyhow!("Failed to store MCP token for '{}': {}", server_name, e))
    }

    /// Load the stored OAuth access token for an MCP server, if any.
    ///
    /// Returns `None` if no token is stored or the token cannot be refreshed.
    pub async fn load_token(&self, server_name: &str) -> Option<String> {
        let config = self.server_configs.get(server_name)?;
        let server_url = config.url.as_deref()?;
        oauth::get_valid_mcp_access_token(server_name, server_url)
            .await
            .ok()
            .flatten()
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// MCP result → model output conversion
// ---------------------------------------------------------------------------

/// Image types every image-capable provider accepts.
const ATTACHABLE_IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Largest decoded image passed through to the model (the Anthropic API's
/// per-image limit).
const MAX_ATTACHED_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// An MCP tool result converted for the model.
#[derive(Debug, Clone, Default)]
pub struct McpToolOutput {
    /// Text of the tool result. Each image is represented by a placeholder
    /// line with its type and size, so a provider that drops images from
    /// tool results still tells the model what came back.
    pub text: String,
    /// Image blocks to send after `text` where the provider supports images
    /// in tool results.
    pub images: Vec<ContentBlock>,
}

/// Decoded size of a base64 payload, without decoding it.
fn base64_decoded_len(data: &str) -> usize {
    let trimmed = data.trim_end_matches('=');
    trimmed.len() * 3 / 4
}

fn format_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{} B", bytes)
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{:.1} MB", b / (KB * KB))
    }
}

/// Describe an image and, when it can be attached, push it to `images`.
fn image_part(data: &str, mime_type: &str, images: &mut Vec<ContentBlock>) -> String {
    let size = base64_decoded_len(data);
    let size_label = format_size(size);
    if !ATTACHABLE_IMAGE_TYPES.contains(&mime_type) {
        return format!("[Image: {}, {}; not attached: unsupported image type]", mime_type, size_label);
    }
    if size > MAX_ATTACHED_IMAGE_BYTES {
        return format!(
            "[Image: {}, {}; not attached: larger than {}]",
            mime_type,
            size_label,
            format_size(MAX_ATTACHED_IMAGE_BYTES)
        );
    }
    // An image block the provider rejects stays in the history and fails
    // every later request, so only attach data that decodes and whose
    // signature matches the declared type.
    use base64::Engine as _;
    let decoded = match base64::engine::general_purpose::STANDARD.decode(data) {
        Ok(decoded) => decoded,
        Err(_) => {
            return format!("[Image: {}, {}; not attached: invalid base64 data]", mime_type, size_label)
        }
    };
    let signature_matches = match mime_type {
        "image/png" => decoded.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => decoded.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/gif" => decoded.starts_with(b"GIF87a") || decoded.starts_with(b"GIF89a"),
        "image/webp" => decoded.len() >= 12 && decoded.starts_with(b"RIFF") && &decoded[8..12] == b"WEBP",
        _ => false,
    };
    if !signature_matches {
        return format!(
            "[Image: {}, {}; not attached: data is not a valid {} image]",
            mime_type, size_label, mime_type
        );
    }
    images.push(ContentBlock::Image {
        source: ImageSource {
            source_type: "base64".to_string(),
            media_type: Some(mime_type.to_string()),
            data: Some(data.to_string()),
            url: None,
        },
    });
    format!("[Image: {}, {}]", mime_type, size_label)
}

/// Convert an MCP tool call result into model output.
///
/// - `structuredContent`, when present, is used as the text (compact JSON)
///   and plain text blocks are skipped: the spec makes them a serialization
///   of the same value.
/// - `text` → the text itself.
/// - `image` → a `[Image: <mime>, <size>]` placeholder line plus an image
///   block (PNG/JPEG/GIF/WebP up to 5 MB; otherwise the placeholder says why
///   it was not attached).
/// - `resource` → `[Resource: <uri>]` plus its text; image blobs are handled
///   like images, other blobs become `[Binary resource: <mime>, <size>]`.
///
/// Parts are joined with newlines.
pub fn mcp_result_to_output(result: &CallToolResult) -> McpToolOutput {
    let mut images = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    if let Some(structured) = &result.structured_content {
        parts.push(structured.to_string());
    }
    for content in &result.content {
        match content {
            McpContent::Text { text } => {
                if result.structured_content.is_none() {
                    parts.push(text.clone());
                }
            }
            McpContent::Image { data, mime_type } => {
                parts.push(image_part(data, mime_type, &mut images));
            }
            McpContent::Resource { resource } => {
                parts.push(format!("[Resource: {}]", resource.uri));
                if let Some(text) = &resource.text {
                    parts.push(text.clone());
                } else if let Some(blob) = &resource.blob {
                    let mime = resource
                        .mime_type
                        .as_deref()
                        .unwrap_or("application/octet-stream");
                    if mime.starts_with("image/") {
                        parts.push(image_part(blob, mime, &mut images));
                    } else {
                        parts.push(format!(
                            "[Binary resource: {}, {}]",
                            mime,
                            format_size(base64_decoded_len(blob))
                        ));
                    }
                }
            }
        }
    }

    McpToolOutput {
        text: parts.join("\n"),
        images,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- env expansion -----------------------------------------------------

    #[test]
    fn test_expand_env_vars_no_vars() {
        assert_eq!(expand_env_vars("hello world"), "hello world");
    }

    #[test]
    fn test_expand_env_vars_known_var() {
        std::env::set_var("_CC_TEST_VAR", "rustacean");
        let out = expand_env_vars("hello ${_CC_TEST_VAR}!");
        assert_eq!(out, "hello rustacean!");
        std::env::remove_var("_CC_TEST_VAR");
    }

    #[test]
    fn test_expand_env_vars_default_value() {
        std::env::remove_var("_CC_MISSING_VAR");
        let out = expand_env_vars("val=${_CC_MISSING_VAR:-fallback}");
        assert_eq!(out, "val=fallback");
    }

    #[test]
    fn test_expand_env_vars_missing_no_default() {
        std::env::remove_var("_CC_REALLY_MISSING");
        // Missing with no default → keep original
        let out = expand_env_vars("${_CC_REALLY_MISSING}");
        assert_eq!(out, "${_CC_REALLY_MISSING}");
    }

    #[test]
    fn test_expand_env_vars_multiple() {
        std::env::set_var("_CC_A", "foo");
        std::env::set_var("_CC_B", "bar");
        let out = expand_env_vars("${_CC_A}/${_CC_B}");
        assert_eq!(out, "foo/bar");
        std::env::remove_var("_CC_A");
        std::env::remove_var("_CC_B");
    }

    #[test]
    fn test_expand_server_config() {
        std::env::set_var("_CC_TEST_HOME", "/home/user");
        let cfg = McpServerConfig {
            name: "test".to_string(),
            command: Some("${_CC_TEST_HOME}/bin/server".to_string()),
            args: vec!["--root".to_string(), "${_CC_TEST_HOME}".to_string()],
            env: {
                let mut m = HashMap::new();
                m.insert("PATH".to_string(), "${_CC_TEST_HOME}/bin".to_string());
                m
            },
            url: None,
            server_type: "stdio".to_string(),
            headers: {
                let mut m = HashMap::new();
                m.insert("X-Home".to_string(), "${_CC_TEST_HOME}".to_string());
                m
            },
            connect_timeout_ms: None,
            catalog_timeout_ms: None,
            origin: Default::default(),
        };
        let expanded = expand_server_config(&cfg);
        assert_eq!(expanded.command.as_deref(), Some("/home/user/bin/server"));
        assert_eq!(expanded.args[1], "/home/user");
        assert_eq!(expanded.env.get("PATH").map(|s| s.as_str()), Some("/home/user/bin"));
        assert_eq!(expanded.headers.get("X-Home").map(|s| s.as_str()), Some("/home/user"));
        std::env::remove_var("_CC_TEST_HOME");
    }

    // ---- JSON-RPC -----------------------------------------------------------

    #[test]
    fn test_json_rpc_request_serialization() {
        let req = JsonRpcRequest::new(1u64, "tools/list", None);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"method\":\"tools/list\""));
        assert!(json.contains("\"id\":1"));
    }

    #[test]
    fn test_json_rpc_notification_omits_id() {
        let req = JsonRpcRequest::notification("notifications/initialized", None);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"method\":\"notifications/initialized\""));
        assert!(!json.contains("\"id\""));
    }

    // ---- McpTool → ToolDefinition ------------------------------------------

    #[test]
    fn test_mcp_tool_to_definition() {
        let tool = McpTool {
            name: "search".to_string(),
            description: Some("Search the web".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } }
            }),
        };
        let def: ToolDefinition = (&tool).into();
        assert_eq!(def.name, "search");
        assert_eq!(def.description, "Search the web");
    }

    // ---- mcp_result_to_output ----------------------------------------------

    fn result(content: Vec<McpContent>) -> CallToolResult {
        CallToolResult {
            content,
            structured_content: None,
            is_error: false,
        }
    }

    #[test]
    fn test_result_output_text() {
        let out = mcp_result_to_output(&result(vec![McpContent::Text {
            text: "hello".to_string(),
        }]));
        assert_eq!(out.text, "hello");
        assert!(out.images.is_empty());
    }

    #[test]
    fn test_result_output_image_is_attached_with_placeholder() {
        // The 8-byte PNG signature.
        let out = mcp_result_to_output(&result(vec![McpContent::Image {
            data: "iVBORw0KGgo=".to_string(),
            mime_type: "image/png".to_string(),
        }]));
        assert_eq!(out.text, "[Image: image/png, 8 B]");
        assert_eq!(out.images.len(), 1);
        match &out.images[0] {
            ContentBlock::Image { source } => {
                assert_eq!(source.source_type, "base64");
                assert_eq!(source.media_type.as_deref(), Some("image/png"));
                assert_eq!(source.data.as_deref(), Some("iVBORw0KGgo="));
            }
            other => panic!("expected an image block, got {:?}", other),
        }
    }

    #[test]
    fn test_result_output_unsupported_or_huge_image_is_not_attached() {
        let svg = mcp_result_to_output(&result(vec![McpContent::Image {
            data: "PHN2Zz4=".to_string(),
            mime_type: "image/svg+xml".to_string(),
        }]));
        assert!(svg.images.is_empty());
        assert!(svg.text.contains("not attached: unsupported image type"), "{}", svg.text);

        let huge = "A".repeat(8 * 1024 * 1024);
        let big = mcp_result_to_output(&result(vec![McpContent::Image {
            data: huge,
            mime_type: "image/png".to_string(),
        }]));
        assert!(big.images.is_empty());
        assert!(big.text.contains("6.0 MB; not attached: larger than 5.0 MB"), "{}", big.text);
    }

    #[test]
    fn test_result_output_invalid_image_data_is_not_attached() {
        // Not base64 at all: attaching it would get every later request
        // rejected by the provider.
        let garbage = mcp_result_to_output(&result(vec![McpContent::Image {
            data: "not base64!!".to_string(),
            mime_type: "image/png".to_string(),
        }]));
        assert!(garbage.images.is_empty());
        assert!(garbage.text.contains("not attached: invalid base64 data"), "{}", garbage.text);

        // Valid base64, but a JPEG signature declared as PNG.
        let mismatched = mcp_result_to_output(&result(vec![McpContent::Image {
            data: "/9j/4AAQ".to_string(),
            mime_type: "image/png".to_string(),
        }]));
        assert!(mismatched.images.is_empty());
        assert!(
            mismatched.text.contains("not attached: data is not a valid image/png image"),
            "{}",
            mismatched.text
        );

        // The same bytes declared as JPEG are attached.
        let jpeg = mcp_result_to_output(&result(vec![McpContent::Image {
            data: "/9j/4AAQ".to_string(),
            mime_type: "image/jpeg".to_string(),
        }]));
        assert_eq!(jpeg.images.len(), 1);
    }

    #[test]
    fn test_result_output_prefers_structured_content() {
        let mut r = result(vec![
            McpContent::Text {
                text: "{\"temperature\": 22}".to_string(),
            },
            McpContent::Image {
                data: "iVBORw0KGgo=".to_string(),
                mime_type: "image/png".to_string(),
            },
        ]);
        r.structured_content = Some(serde_json::json!({ "temperature": 22 }));
        let out = mcp_result_to_output(&r);
        assert_eq!(out.text, "{\"temperature\":22}\n[Image: image/png, 8 B]");
        assert_eq!(out.images.len(), 1);
    }

    #[test]
    fn test_result_output_resource_with_text() {
        let out = mcp_result_to_output(&result(vec![McpContent::Resource {
            resource: ResourceContents {
                uri: "file:///foo.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: Some("file contents".to_string()),
                blob: None,
            },
        }]));
        assert_eq!(out.text, "[Resource: file:///foo.txt]\nfile contents");
    }

    #[test]
    fn test_result_output_resource_blobs() {
        let out = mcp_result_to_output(&result(vec![
            McpContent::Resource {
                resource: ResourceContents {
                    uri: "file:///img.png".to_string(),
                    mime_type: Some("image/png".to_string()),
                    text: None,
                    blob: Some("iVBORw0KGgo=".to_string()),
                },
            },
            McpContent::Resource {
                resource: ResourceContents {
                    uri: "file:///data.bin".to_string(),
                    mime_type: None,
                    text: None,
                    blob: Some("AAAA".to_string()),
                },
            },
        ]));
        assert!(out.text.contains("[Resource: file:///img.png]\n[Image: image/png, 8 B]"), "{}", out.text);
        assert!(out.text.contains("[Binary resource: application/octet-stream, 3 B]"), "{}", out.text);
        assert_eq!(out.images.len(), 1);
    }

    #[test]
    fn test_result_output_mixed_text() {
        let out = mcp_result_to_output(&result(vec![
            McpContent::Text {
                text: "line one".to_string(),
            },
            McpContent::Text {
                text: "line two".to_string(),
            },
        ]));
        assert_eq!(out.text, "line one\nline two");
    }

    // ---- McpManager --------------------------------------------------------

    #[test]
    fn test_manager_server_names_empty() {
        let mgr = McpManager::new();
        assert!(mgr.server_names().is_empty());
    }

    #[test]
    fn test_manager_all_statuses_empty() {
        let mgr = McpManager::new();
        assert!(mgr.all_statuses().is_empty());
    }

    #[test]
    fn test_manager_failed_servers_empty() {
        let mgr = McpManager::new();
        assert!(mgr.failed_servers().is_empty());
    }

    #[tokio::test]
    async fn test_start_reports_unsupported_transport_without_connecting() {
        let cfg = McpServerConfig {
            name: "odd".to_string(),
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: None,
            server_type: "carrier-pigeon".to_string(),
            headers: Default::default(),
            connect_timeout_ms: None,
            catalog_timeout_ms: None,
            origin: Default::default(),
        };
        let mgr = McpManager::start(&[cfg]);
        mgr.wait_until_settled().await;
        assert_eq!(mgr.status_summary(), McpStatusSummary { connecting: 0, failed: 1 });
        let failed = mgr.failed_servers();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].1.contains("unsupported transport"));
    }

    fn test_client(server: &str, tools: &[&str]) -> McpClient {
        let mut client = client::McpClient::new_for_test(server);
        client.tools = tools
            .iter()
            .map(|name| McpTool {
                name: (*name).to_string(),
                description: Some(format!("{} tool", name)),
                input_schema: serde_json::json!({ "type": "object" }),
            })
            .collect();
        client
    }

    // ---- tool naming / routing ---------------------------------------------

    #[test]
    fn test_sanitize_tool_name_part() {
        assert_eq!(sanitize_tool_name_part("github"), "github");
        assert_eq!(sanitize_tool_name_part("my.server/v2"), "my_server_v2");
        assert_eq!(sanitize_tool_name_part("kebab-case_ok"), "kebab-case_ok");
        assert_eq!(sanitize_tool_name_part("über tool"), "_ber_tool");
        assert_eq!(sanitize_tool_name_part(""), "_");
    }

    #[test]
    fn test_exposed_names_do_not_collide_across_prefix_servers() {
        // `{server}_{tool}` mapped both of these to `git_lab_status`.
        let clients = vec![
            ("git".to_string(), Arc::new(test_client("git", &["lab_status"]))),
            ("git_lab".to_string(), Arc::new(test_client("git_lab", &["status"]))),
        ];
        let entries = build_tool_entries(&clients, &std::collections::HashSet::new());
        let names: Vec<&str> = entries.iter().map(|e| e.definition.name.as_str()).collect();
        assert_eq!(names, vec!["mcp__git__lab_status", "mcp__git_lab__status"]);
        assert_eq!(entries[0].server_name, "git");
        assert_eq!(entries[0].tool_name, "lab_status");
        assert_eq!(entries[1].server_name, "git_lab");
        assert_eq!(entries[1].tool_name, "status");
    }

    #[test]
    fn test_exposed_names_disambiguate_sanitization_collisions() {
        let clients = vec![
            ("a.b".to_string(), Arc::new(test_client("a.b", &["t"]))),
            ("a_b".to_string(), Arc::new(test_client("a_b", &["t"]))),
        ];
        let entries = build_tool_entries(&clients, &std::collections::HashSet::new());
        assert_eq!(entries[0].definition.name, "mcp__a_b__t");
        assert_ne!(entries[1].definition.name, entries[0].definition.name);
        assert!(entries[1].definition.name.starts_with("mcp__a_b__t_"));
        assert!(entries
            .iter()
            .all(|e| e.definition.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')));
    }

    #[test]
    fn test_ambiguous_servers_keep_their_names_across_connect_order() {
        let ambiguous: std::collections::HashSet<String> =
            ["a.b".to_string(), "a_b".to_string()].into_iter().collect();
        let a_dot = ("a.b".to_string(), Arc::new(test_client("a.b", &["t"])));
        let a_under = ("a_b".to_string(), Arc::new(test_client("a_b", &["t"])));

        // Only `a_b` connected so far.
        let early = build_tool_entries(std::slice::from_ref(&a_under), &ambiguous);
        // Both connected.
        let late = build_tool_entries(&[a_dot, a_under], &ambiguous);
        let name_of = |entries: &[McpToolEntry], server: &str| {
            entries
                .iter()
                .find(|e| e.server_name == server)
                .map(|e| e.definition.name.clone())
                .expect("server entry")
        };
        assert_eq!(name_of(&early, "a_b"), name_of(&late, "a_b"));
        // Neither gets the clean name, so no exact rule can switch servers.
        assert_ne!(name_of(&late, "a_b"), "mcp__a_b__t");
        assert_ne!(name_of(&late, "a.b"), "mcp__a_b__t");
        assert_ne!(name_of(&late, "a.b"), name_of(&late, "a_b"));
    }

    #[test]
    fn test_exposed_names_are_capped_and_stable() {
        let long_tool = "x".repeat(100);
        let mut used = std::collections::HashSet::new();
        let first = exposed_tool_name("server", &long_tool, false, &mut used);
        assert!(first.len() <= MAX_MCP_TOOL_NAME_LEN, "{} chars", first.len());
        assert!(first.starts_with("mcp__server__xxx"));
        // Same raw identity → same name on a rebuild.
        let again = exposed_tool_name("server", &long_tool, false, &mut std::collections::HashSet::new());
        assert_eq!(first, again);
    }

    #[tokio::test]
    async fn test_manager_routes_by_exposed_name() {
        let mgr = Arc::new(McpManager::new());
        mgr.install_client(test_client("git", &["lab_status"]));
        mgr.install_client(test_client("git_lab", &["status"]));
        let entries = mgr.all_tool_definitions();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].definition.name, "mcp__git_lab__status");
        assert_eq!(entries[1].definition.description, "status tool");

        // The legacy `{server}_{tool}` form is not routed.
        let err = mgr
            .call_tool("git_lab_status", None)
            .await
            .expect_err("legacy names are not routed");
        assert!(err.to_string().contains("Unknown MCP tool"), "{}", err);
        // A routed name reaches the owning client (whose test backend is absent).
        let err = mgr
            .call_tool("mcp__git_lab__status", None)
            .await
            .expect_err("test clients have no backend");
        assert!(err.to_string().contains("backend missing"), "{}", err);
    }

    #[tokio::test]
    async fn test_instructions_section_only_for_servers_with_active_tools() {
        let mgr = Arc::new(McpManager::new());
        let mut docs = test_client("docs", &["search"]);
        docs.instructions = Some("  Always cite the page URL.  ".to_string());
        mgr.install_client(docs);
        let mut quiet = test_client("quiet", &["noop"]);
        quiet.instructions = Some("Never shown.".to_string());
        mgr.install_client(quiet);
        let mut blank = test_client("blank", &["noop"]);
        blank.instructions = Some("   ".to_string());
        mgr.install_client(blank);

        let section = mgr
            .instructions_prompt_section(|name| name != "mcp__quiet__noop")
            .expect("docs has an active tool and instructions");
        assert!(section.starts_with("# MCP Server Instructions"));
        assert!(section.contains("## docs\nAlways cite the page URL."), "{}", section);
        assert!(!section.contains("quiet"), "{}", section);
        assert!(!section.contains("## blank"), "{}", section);

        assert!(mgr.instructions_prompt_section(|_| false).is_none());
    }

    #[tokio::test]
    async fn test_install_client_marks_connected_and_bumps_generation() {
        let mgr = Arc::new(McpManager::new());
        let before = mgr.generation();
        let mut client = client::McpClient::new_for_test("srv");
        client.tools.push(McpTool {
            name: "ping".to_string(),
            description: None,
            input_schema: serde_json::json!({ "type": "object" }),
        });
        mgr.install_client(client);
        assert_ne!(mgr.generation(), before);
        assert!(matches!(
            mgr.server_status("srv"),
            McpServerStatus::Connected { tool_count: 1 }
        ));
        assert_eq!(mgr.server_names(), vec!["srv".to_string()]);
        // Nothing is pending, so waiting returns immediately.
        mgr.wait_until_settled().await;
    }

    #[test]
    fn test_auth_state_uses_token_expiry_datetime() {
        // Redirect the on-disk token store into a tempdir so this test never
        // touches (or requires a writable) ~/.claurst. Sandboxed builds run
        // with no HOME and disallow writes outside the build tree.
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("CLAURST_MCP_TOKENS_DIR");
        std::env::set_var("CLAURST_MCP_TOKENS_DIR", tmp.path());

        let mut mgr = McpManager::new();
        mgr.server_configs.insert(
            "remote".to_string(),
            McpServerConfig {
                name: "remote".to_string(),
                command: None,
                args: vec![],
                env: HashMap::new(),
                url: Some("https://example.com/mcp".to_string()),
                server_type: "http".to_string(),
                headers: Default::default(),
                connect_timeout_ms: None,
                catalog_timeout_ms: None,
                origin: Default::default(),
            },
        );

        let expires_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 3600;
        let token = oauth::McpToken {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: Some(expires_at),
            scope: None,
            server_name: "remote".to_string(),
        };
        oauth::store_mcp_token(&token).expect("store token");

        match mgr.auth_state("remote") {
            McpAuthState::Authenticated { token_expiry } => {
                assert_eq!(token_expiry, token.expiry_datetime());
            }
            other => panic!("expected authenticated, got {:?}", other),
        }

        oauth::remove_mcp_token("remote").ok();

        match prev {
            Some(v) => std::env::set_var("CLAURST_MCP_TOKENS_DIR", v),
            None => std::env::remove_var("CLAURST_MCP_TOKENS_DIR"),
        }
    }
}

// ---------------------------------------------------------------------------
// Resource subscriptions (T2-12)
// ---------------------------------------------------------------------------

use tokio::sync::mpsc as tokio_mpsc;

/// Notification that a resource has changed.
#[derive(Debug, Clone)]
pub struct ResourceChangedEvent {
    pub server_name: String,
    pub uri: String,
}

/// What the manager must do after a notification was processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationEffect {
    None,
    /// `notifications/tools/list_changed`: re-list the server's tools.
    ToolsChanged,
}

/// Process a single server-initiated notification from `server_name`.
/// Routes resource updates to subscribers and reports tool-list changes.
pub(crate) async fn process_notification(
    server_name: &str,
    raw: serde_json::Value,
    resource_subscriptions: &DashMap<(String, String), tokio_mpsc::Sender<ResourceChangedEvent>>,
) -> NotificationEffect {
    // Only process server-initiated notifications (have "method", no non-null "id")
    let has_method = raw.get("method").is_some();
    let has_id = raw.get("id").map(|v| !v.is_null()).unwrap_or(false);
    if !has_method || has_id {
        debug!(
            server = %server_name,
            "process_notification: skipping non-notification message"
        );
        return NotificationEffect::None;
    }

    let method = raw["method"].as_str().unwrap_or("");
    match method {
        "notifications/resources/updated" => {
            let uri = raw["params"]["uri"].as_str().unwrap_or("").to_string();
            let key = (server_name.to_string(), uri);
            // Clone the sender out so no map guard is held across the send.
            let sender = resource_subscriptions.get(&key).map(|tx| tx.value().clone());
            match sender {
                Some(tx) => {
                    let event = ResourceChangedEvent {
                        server_name: key.0,
                        uri: key.1,
                    };
                    if let Err(e) = tx.send(event).await {
                        debug!(
                            server = %server_name,
                            error = %e,
                            "process_notification: resource subscription receiver dropped"
                        );
                    }
                }
                None => debug!(
                    server = %server_name,
                    uri = %key.1,
                    "process_notification: no subscriber for resource update"
                ),
            }
            NotificationEffect::None
        }
        "notifications/tools/list_changed" => {
            info!(server = %server_name, "MCP tools list changed");
            NotificationEffect::ToolsChanged
        }
        other => {
            debug!(
                server = %server_name,
                method = %other,
                "Unhandled MCP notification"
            );
            NotificationEffect::None
        }
    }
}

/// Subscription handle for a single MCP resource URI.
pub struct ResourceSubscription {
    pub server_name: String,
    pub uri: String,
}

/// Subscribe to resource changes on an MCP server.
///
/// Sends the `resources/subscribe` JSON-RPC request to the named server and
/// returns a channel receiver that will deliver [`ResourceChangedEvent`] values
/// whenever the server fires a `notifications/resources/updated` notification.
/// The notification dispatch loop (elsewhere) looks up the tx in
/// `manager.resource_subscriptions` and forwards events.
///
/// If the server is not connected or the RPC fails, a dead receiver is returned
/// (no events will ever be delivered).
pub async fn subscribe_resource(
    manager: &McpManager,
    server_name: &str,
    uri: &str,
) -> tokio_mpsc::Receiver<ResourceChangedEvent> {
    let make_dead = || {
        let (_tx, rx) = tokio_mpsc::channel::<ResourceChangedEvent>(1);
        rx
    };

    let client = match manager.client(server_name) {
        Some(c) => c,
        None => {
            tracing::warn!(server_name, uri, "subscribe_resource: server not connected");
            return make_dead();
        }
    };

    if let Err(e) = client.subscribe_resource(uri).await {
        tracing::warn!(server_name, uri, error = %e, "subscribe_resource RPC failed");
        return make_dead();
    }

    let (tx, rx) = tokio_mpsc::channel(32);
    manager
        .resource_subscriptions
        .insert((server_name.to_string(), uri.to_string()), tx);
    tracing::info!(server_name, uri, "MCP resource subscription registered");
    rx
}

/// Unsubscribe from resource change notifications.
///
/// Sends `resources/unsubscribe` JSON-RPC request to the named server via
/// `McpManager`.  Returns an error if the server is not connected or the
/// request fails.
pub async fn unsubscribe_resource(
    manager: &McpManager,
    server_name: &str,
    uri: &str,
) -> Result<(), String> {
    let client = manager
        .client(server_name)
        .ok_or_else(|| format!("unsubscribe_resource: server '{}' not connected", server_name))?;

    client
        .unsubscribe_resource(uri)
        .await
        .map_err(|e| format!("unsubscribe_resource failed: {e}"))
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Transport tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod transport_tests {
    use super::transport::*;
    use tokio::sync::mpsc as tokio_mpsc;

    #[tokio::test]
    async fn test_route_incoming_value_sends_response_to_response_queue() {
        let (response_tx, mut response_rx) = tokio_mpsc::unbounded_channel();
        let (notification_tx, mut notification_rx) = tokio_mpsc::unbounded_channel();
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": {"ok": true}
        });

        route_incoming_value("srv", value, &response_tx, &notification_tx).unwrap();

        let response = response_rx.recv().await.expect("expected response");
        assert_eq!(response.id, Some(serde_json::json!(7)));
        assert!(notification_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_route_incoming_value_sends_notification_to_notification_queue() {
        let (response_tx, mut response_rx) = tokio_mpsc::unbounded_channel();
        let (notification_tx, mut notification_rx) = tokio_mpsc::unbounded_channel();
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/resources/updated",
            "params": {"uri": "file:///foo.txt"}
        });

        route_incoming_value("srv", value.clone(), &response_tx, &notification_tx).unwrap();

        let notification = notification_rx.recv().await.expect("expected notification");
        assert_eq!(notification, value);
        assert!(response_rx.try_recv().is_err());
    }

    #[test]
    fn test_resolve_legacy_endpoint_supports_relative_path() {
        let endpoint = resolve_legacy_endpoint("https://example.com/mcp", "/messages").unwrap();
        assert_eq!(endpoint, "https://example.com/messages");
    }

    #[test]
    fn test_process_sse_line_collects_event_and_data() {
        let mut event_name = None;
        let mut data_lines = Vec::new();
        process_sse_line("event: endpoint", &mut event_name, &mut data_lines);
        process_sse_line("data: /messages", &mut event_name, &mut data_lines);
        assert_eq!(event_name.as_deref(), Some("endpoint"));
        assert_eq!(data_lines, vec!["/messages".to_string()]);
    }

    #[test]
    fn test_is_unsupported_protocol_error_matches_rmcp_variants() {
        assert!(crate::McpClient::is_unsupported_protocol_error(
            "unsupported protocol version: 2025-11-25"
        ));
        assert!(crate::McpClient::is_unsupported_protocol_error(
            "Bad Request: Unsupported MCP-Protocol-Version: 2025-11-25"
        ));
        assert!(!crate::McpClient::is_unsupported_protocol_error("connection reset by peer"));
    }

}

// ---------------------------------------------------------------------------
// Notification dispatch tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod notification_tests {
    use super::*;

    #[tokio::test]
    async fn test_process_notification_routes_resource_updated() {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/resources/updated",
            "params": { "uri": "file:///foo.txt" }
        });


        let subscriptions: DashMap<
            (String, String),
            tokio::sync::mpsc::Sender<ResourceChangedEvent>,
        > = DashMap::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ResourceChangedEvent>(4);
        subscriptions.insert(("myserver".to_string(), "file:///foo.txt".to_string()), tx);

        process_notification("myserver", notification, &subscriptions).await;

        let event = rx.try_recv().expect("expected a ResourceChangedEvent");
        assert_eq!(event.server_name, "myserver");
        assert_eq!(event.uri, "file:///foo.txt");
        assert!(rx.try_recv().is_err(), "channel should be empty after one event");
    }

    #[tokio::test]
    async fn test_process_notification_no_subscriber_does_not_panic() {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/resources/updated",
            "params": { "uri": "file:///unsubscribed.txt" }
        });

        let subscriptions: DashMap<
            (String, String),
            tokio::sync::mpsc::Sender<ResourceChangedEvent>,
        > = DashMap::new();
        process_notification("myserver", notification, &subscriptions).await;
    }

    #[tokio::test]
    async fn test_process_notification_tools_list_changed() {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/tools/list_changed",
            "params": {}
        });

        let subscriptions: DashMap<
            (String, String),
            tokio::sync::mpsc::Sender<ResourceChangedEvent>,
        > = DashMap::new();
        let effect = process_notification("myserver", notification, &subscriptions).await;
        assert_eq!(effect, NotificationEffect::ToolsChanged);
    }

    #[tokio::test]
    async fn test_process_notification_multiple_events() {
        let n1 = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/resources/updated",
            "params": { "uri": "file:///a.txt" }
        });
        let n2 = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/resources/updated",
            "params": { "uri": "file:///b.txt" }
        });


        let subscriptions: DashMap<
            (String, String),
            tokio::sync::mpsc::Sender<ResourceChangedEvent>,
        > = DashMap::new();
        let (tx_a, mut rx_a) = tokio::sync::mpsc::channel::<ResourceChangedEvent>(4);
        let (tx_b, mut rx_b) = tokio::sync::mpsc::channel::<ResourceChangedEvent>(4);
        subscriptions.insert(("s1".to_string(), "file:///a.txt".to_string()), tx_a);
        subscriptions.insert(("s1".to_string(), "file:///b.txt".to_string()), tx_b);

        process_notification("s1", n1, &subscriptions).await;
        process_notification("s1", n2, &subscriptions).await;

        let ev_a = rx_a.try_recv().expect("expected event for a.txt");
        assert_eq!(ev_a.uri, "file:///a.txt");

        let ev_b = rx_b.try_recv().expect("expected event for b.txt");
        assert_eq!(ev_b.uri, "file:///b.txt");
    }

    #[tokio::test]
    async fn test_process_notification_skips_rpc_responses() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "notifications/resources/updated",
            "params": { "uri": "file:///foo.txt" }
        });


        let subscriptions: DashMap<
            (String, String),
            tokio::sync::mpsc::Sender<ResourceChangedEvent>,
        > = DashMap::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ResourceChangedEvent>(4);
        subscriptions.insert(("myserver".to_string(), "file:///foo.txt".to_string()), tx);

        process_notification("myserver", response, &subscriptions).await;

        assert!(
            rx.try_recv().is_err(),
            "RPC response must not be dispatched as a notification"
        );
    }
}
