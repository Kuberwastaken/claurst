// cursor_acp.rs — Cursor CLI ACP client provider.
//
// Connects to Cursor's `agent --force --trust acp` subprocess via
// newline-delimited JSON-RPC 2.0 over stdio. Implements LlmProvider trait.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use claurst_core::provider_id::{ModelId, ProviderId};
use claurst_core::types::{ContentBlock, MessageContent, Role, UsageInfo};
use futures::{Stream, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, RwLock};
use tracing::info;

use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StopReason,
    StreamEvent, SystemPromptStyle,
};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for spawning the Cursor CLI subprocess.
#[derive(Debug, Clone)]
pub struct CursorAcpCommand {
    /// Path to the executable (default: "agent").
    pub executable: String,
    /// Arguments before "acp" (default: ["--force", "--trust"]).
    pub permission_args: Vec<String>,
    /// The "acp" subcommand argument (default: "acp").
    pub acp_arg: String,
    /// Extra args after "acp" (default: []).
    pub extra_args: Vec<String>,
}

impl Default for CursorAcpCommand {
    fn default() -> Self {
        Self {
            executable: std::env::var("CLAURST_CURSOR_ACP_PATH")
                .unwrap_or_else(|_| "agent".to_string()),
            permission_args: std::env::var("CLAURST_CURSOR_ACP_EXTRA_ARGS")
                .unwrap_or_else(|_| "--force --trust".to_string())
                .split_whitespace()
                .map(String::from)
                .collect(),
            acp_arg: std::env::var("CLAURST_CURSOR_ACP_ARGS")
                .unwrap_or_else(|_| "acp".to_string())
                .split_whitespace()
                .next()
                .unwrap_or("acp")
                .to_string(),
            extra_args: Vec::new(),
        }
    }
}

impl CursorAcpCommand {
    /// Build the full command-line argument vector.
    fn args(&self) -> Vec<String> {
        let mut args = self.permission_args.clone();
        args.push(self.acp_arg.clone());
        args.extend(self.extra_args.iter().cloned());
        args
    }

    /// Check whether the executable is available on PATH.
    pub fn configured(&self) -> bool {
        std::process::Command::new(&self.executable)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }
}

// ---------------------------------------------------------------------------
// Model catalog
// ---------------------------------------------------------------------------

/// Model catalog discovered from the Cursor CLI's initialize response.
#[derive(Debug, Clone, Default)]
pub struct ModelCatalog {
    pub models: Vec<String>,
    pub current: Option<String>,
    pub names: HashMap<String, String>,
}

impl ModelCatalog {
    /// Parse model info from the initialize response params.
    pub fn from_initialize(params: &serde_json::Value) -> Self {
        let mut models = Vec::new();
        let mut current = None;

        // Parse availableModels array.
        if let Some(arr) = params.get("availableModels").and_then(|v| v.as_array()) {
            for m in arr {
                if let Some(id) = m.as_str() {
                    models.push(id.to_string());
                }
            }
        }

        // Parse currentModelId.
        if let Some(id) = params.get("currentModelId").and_then(|v| v.as_str()) {
            current = Some(id.to_string());
        }

        // Parse configOptions for model category.
        if let Some(options) = params.get("configOptions").and_then(|v| v.as_array()) {
            for opt in options {
                if let Some(cat) = opt.get("category").and_then(|c| c.as_str()) {
                    if cat == "model" {
                        if let Some(id) = opt.get("id").and_then(|i| i.as_str()) {
                            if !models.contains(&id.to_string()) {
                                models.push(id.to_string());
                            }
                        }
                    }
                }
            }
        }

        Self {
            models,
            current,
            names: HashMap::new(),
        }
    }

    /// Parse model catalog from the session/new response params.
    pub fn from_session_new(params: &serde_json::Value) -> Self {
        let mut models = Vec::new();
        let mut current = None;
        let mut names = HashMap::new();

        // Parse models.availableModels — array of {modelId, name} objects.
        if let Some(models_obj) = params.get("models") {
            if let Some(arr) = models_obj.get("availableModels").and_then(|v| v.as_array()) {
                for m in arr {
                    // Each entry is {"modelId": "...", "name": "..."}
                    if let Some(id) = m.get("modelId").and_then(|v| v.as_str()) {
                        if !models.contains(&id.to_string()) {
                            models.push(id.to_string());
                        }
                        if let Some(name) = m.get("name").and_then(|v| v.as_str()) {
                            names.insert(id.to_string(), name.to_string());
                        }
                    }
                    // Also try plain string format (backward compat).
                    if let Some(id) = m.as_str() {
                        if !models.contains(&id.to_string()) {
                            models.push(id.to_string());
                        }
                    }
                }
            }
            if let Some(id) = models_obj.get("currentModelId").and_then(|v| v.as_str()) {
                current = Some(id.to_string());
            }
        }

        // Also parse configOptions for model category.
        if let Some(options) = params.get("configOptions").and_then(|v| v.as_array()) {
            for opt in options {
                if let Some(cat) = opt.get("category").and_then(|c| c.as_str()) {
                    if cat == "model" {
                        // The configOption itself has an id (current value).
                        if let Some(id) = opt.get("id").and_then(|i| i.as_str()) {
                            if !models.contains(&id.to_string()) {
                                models.push(id.to_string());
                            }
                        }
                        // Also extract from options array.
                        if let Some(opts) = opt.get("options").and_then(|o| o.as_array()) {
                            for option in opts {
                                if let Some(value) = option.get("value").and_then(|v| v.as_str()) {
                                    if !models.contains(&value.to_string()) {
                                        models.push(value.to_string());
                                    }
                                    if let Some(name) = option.get("name").and_then(|v| v.as_str()) {
                                        names.insert(value.to_string(), name.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Self {
            models,
            current,
            names,
        }
    }
    /// Resolve a model ID. Exact match wins; bare ID resolves when exactly
    /// one bracketed variant exists; ambiguous → None.
    pub fn resolve_model(&self, requested: &str) -> Option<String> {
        // Exact match.
        if self.models.iter().any(|m| m == requested) {
            return Some(requested.to_string());
        }
        // Bare ID match (when model has bracketed variants like "model [variant]").
        let matches: Vec<&String> = self
            .models
            .iter()
            .filter(|m| m.starts_with(requested) && m.contains('['))
            .collect();
        if matches.len() == 1 {
            return Some(matches[0].clone());
        }
        if matches.len() > 1 {
            // Ambiguous — multiple bracketed variants.
            return None;
        }
        // If no exact match but we have models, pass through the requested id.
        if !self.models.is_empty() {
            return Some(requested.to_string());
        }
        None
    }
}

// ---------------------------------------------------------------------------
// ACP process wrapper
// ---------------------------------------------------------------------------

/// Owns the Cursor subprocess and its stdio handles.
struct AcpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Bounded stderr tail for debugging (last 8KB).
    stderr_tail: Arc<RwLock<String>>,
    next_id: u64,
    session_id: String,
    catalog: ModelCatalog,
}

impl Drop for AcpProcess {
    fn drop(&mut self) {
        // Kill the Cursor subprocess to prevent orphaned processes.
        let _ = self.child.start_kill();
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Cursor ACP provider — connects to Cursor CLI via subprocess JSON-RPC.
pub struct CursorAcpProvider {
    /// Provider identifier.
    id: ProviderId,
    /// Process handle — None when not connected (lazy reconnection).
    /// tokio::sync::Mutex because create_message_stream() is async and
    /// holds the lock across .await points.
    process: Mutex<Option<AcpProcess>>,
    /// Current model ID.
    model: RwLock<String>,
    /// Command configuration.
    command: CursorAcpCommand,
}

impl CursorAcpProvider {
    /// Create a new provider with the given model and default command.
    pub fn new(model: String) -> Self {
        Self {
            id: ProviderId::new("cursor-acp"),
            process: Mutex::new(None),
            model: RwLock::new(model),
            command: CursorAcpCommand::default(),
        }
    }

    /// Create from environment — used by runtime_provider_for().
    pub fn from_env() -> Self {
        let model = std::env::var("CLAURST_CURSOR_ACP_MODEL").unwrap_or_default();
        Self::new(model)
    }

    /// Spawn the Cursor subprocess and set up stdio handles.
    /// Does NOT run initialize or create a session — callers do that.
    async fn spawn_subprocess(&self) -> Result<AcpProcess, ProviderError> {
        let mut child = tokio::process::Command::new(&self.command.executable)
            .args(self.command.args())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("failed to spawn agent: {}", e),
                status: None,
                body: None,
            })?;

        let stdin = child.stdin.take().ok_or_else(|| ProviderError::Other {
            provider: self.id.clone(),
            message: "failed to capture stdin".to_string(),
            status: None,
            body: None,
        })?;
        let stdout = child.stdout.take().ok_or_else(|| ProviderError::Other {
            provider: self.id.clone(),
            message: "failed to capture stdout".to_string(),
            status: None,
            body: None,
        })?;
        let stderr = child.stderr.take();

        // Spawn stderr reader to maintain a bounded tail.
        let stderr_tail = Arc::new(RwLock::new(String::new()));
        if let Some(stderr) = stderr {
            let tail = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break, // EOF
                        Ok(_) => {
                            let mut buf = tail.write().await;
                            buf.push_str(&line);
                            // Keep last 8KB.
                            if buf.len() > 8192 {
                                let start = buf.len() - 8192;
                                *buf = buf[start..].to_string();
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        Ok(AcpProcess {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_tail,
            next_id: 1,
            session_id: String::new(),
            catalog: ModelCatalog::default(),
        })
    }

    /// Spawn the Cursor subprocess and run initialize + session/new.
    async fn connect(&self) -> Result<AcpProcess, ProviderError> {
        let mut process = self.spawn_subprocess().await?;
        self.initialize(&mut process).await?;
        self.create_session(&mut process).await?;
        Ok(process)
    }

    /// Send initialize request and parse model catalog.
    async fn initialize(&self, process: &mut AcpProcess) -> Result<(), ProviderError> {
        let id = self.next_id(process);
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.to_string(),
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {
                    "streaming": true,
                },
            },
        });

        self.send_json(process, &request).await?;

        // Read response.
        let response = self.read_response(process, id).await?;

        // Parse model catalog.
        if let Some(result) = response.get("result") {
            process.catalog = ModelCatalog::from_initialize(result);
            info!(
                models = process.catalog.models.len(),
                "Cursor ACP initialized"
            );
        }

        Ok(())
    }

    /// Send session/new request.
    async fn create_session(&self, process: &mut AcpProcess) -> Result<(), ProviderError> {
        let id = self.next_id(process);
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.to_string(),
            "method": "session/new",
            "params": {
                "cwd": cwd,
                "mcpServers": [],
            },
        });

        self.send_json(process, &request).await?;

        let response = self.read_response(process, id).await?;

        if let Some(result) = response.get("result") {
            if let Some(sid) = result.get("sessionId").and_then(|s| s.as_str()) {
                process.session_id = sid.to_string();
                info!(session_id = sid, "Cursor ACP session created");
            }
            // Parse model catalog from session/new response.
            process.catalog = ModelCatalog::from_session_new(result);
            info!(
                models = process.catalog.models.len(),
                current = process.catalog.current.as_deref().unwrap_or("none"),
                "Cursor ACP model catalog loaded"
            );
        }

        Ok(())
    }

    /// Get the next JSON-RPC id and increment the counter.
    fn next_id(&self, process: &mut AcpProcess) -> u64 {
        let id = process.next_id;
        process.next_id += 1;
        id
    }

    /// Send a JSON-RPC message as a newline-delimited line.
    async fn send_json(
        &self,
        process: &mut AcpProcess,
        value: &serde_json::Value,
    ) -> Result<(), ProviderError> {
        let mut line = serde_json::to_string(value).map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("JSON serialize error: {}", e),
            status: None,
            body: None,
        })?;
        line.push('\n');
        process
            .stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("stdin write error: {}", e),
                status: None,
                body: None,
            })?;
        Ok(())
    }

    /// Read a JSON-RPC response with the given id.
    async fn read_response(
        &self,
        process: &mut AcpProcess,
        expected_id: u64,
    ) -> Result<serde_json::Value, ProviderError> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = process
                .stdout
                .read_line(&mut line)
                .await
                .map_err(|e| ProviderError::Other {
                    provider: self.id.clone(),
                    message: format!("stdout read error: {}", e),
                    status: None,
                    body: None,
                })?;
            if n == 0 {
                let stderr = process.stderr_tail.read().await;
                return Err(ProviderError::Other {
                    provider: self.id.clone(),
                    message: format!("Cursor CLI closed stdout. stderr tail:\n{}", stderr),
                    status: None,
                    body: None,
                });
            }
            // Parse JSON.
            let msg: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue, // Skip non-JSON lines
            };
            // Check if this is a response with the expected id.
            if let Some(id) = msg.get("id").and_then(|i| i.as_str()) {
                if id == expected_id.to_string() {
                    if let Some(error) = msg.get("error") {
                        return Err(ProviderError::Other {
                            provider: self.id.clone(),
                            message: format!("JSON-RPC error: {}", error),
                            status: None,
                            body: None,
                        });
                    }
                    return Ok(msg);
                }
            }
            // Skip notifications — they're not responses.
        }
    }

    /// Ensure a process is connected. Reconnects if process is None.
    async fn ensure_connected(&self) -> Result<(), ProviderError> {
        let mut guard = self.process.lock().await;
        if guard.is_none() {
            let proc = self.connect().await?;
            *guard = Some(proc);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// LlmProvider implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmProvider for CursorAcpProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "Cursor ACP"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        // Non-streaming: collect all stream events into a response.
        let mut stream = self.create_message_stream(request).await?;

        let mut id = String::from("unknown");
        let mut model = self.model.read().await.clone();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = UsageInfo::default();
        let mut text_parts = Vec::new();

        while let Some(result) = stream.next().await {
            match result {
                Err(e) => return Err(e),
                Ok(evt) => match evt {
                    StreamEvent::MessageStart {
                        id: msg_id,
                        model: msg_model,
                        usage: msg_usage,
                    } => {
                        id = msg_id;
                        model = msg_model;
                        usage = msg_usage;
                    }
                    StreamEvent::TextDelta { text, .. } => {
                        text_parts.push(text);
                    }
                    StreamEvent::MessageDelta {
                        stop_reason: sr, ..
                    } => {
                        if let Some(r) = sr {
                            stop_reason = r;
                        }
                    }
                    _ => {}
                },
            }
        }

        let text = text_parts.join("");
        Ok(ProviderResponse {
            id,
            content: vec![ContentBlock::Text { text }],
            stop_reason,
            usage,
            model,
        })
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        self.ensure_connected().await?;

        let model = request.model.clone();
        let mut guard = self.process.lock().await;
        let process = guard.as_mut().ok_or_else(|| ProviderError::Other {
            provider: self.id.clone(),
            message: "not connected".to_string(),
            status: None,
            body: None,
        })?;

        // Build session/prompt request.
        let id = self.next_id(process);
        let resolved_model = process.catalog.resolve_model(&model).unwrap_or(model);

        // ACP session/prompt: send only the latest user message as a flat
        // content-block array. The session maintains conversation history.
        let prompt_json: Vec<serde_json::Value> = request
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .last()
            .map(|m| match &m.content {
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text { text } => serde_json::json!({
                            "type": "text",
                            "text": text,
                        }),
                        _ => serde_json::json!({
                            "type": "text",
                            "text": m.get_all_text(),
                        }),
                    })
                    .collect::<Vec<_>>(),
                MessageContent::Text(text) => vec![serde_json::json!({
                    "type": "text",
                    "text": text,
                })],
            })
            .unwrap_or_default();

        let prompt_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.to_string(),
            "method": "session/prompt",
            "params": {
                "sessionId": process.session_id,
                "model": resolved_model,
                "prompt": prompt_json,
            },
        });

        self.send_json(process, &prompt_request).await?;

        // Collect events into a vec and return as a stream.
        let mut events: Vec<Result<StreamEvent, ProviderError>> = Vec::new();

        events.push(Ok(StreamEvent::MessageStart {
            id: id.to_string(),
            model: resolved_model.clone(),
            usage: UsageInfo::default(),
        }));

        let mut line = String::new();
        loop {
            line.clear();
            let n = process
                .stdout
                .read_line(&mut line)
                .await
                .map_err(|e| ProviderError::Other {
                    provider: self.id.clone(),
                    message: format!("read error: {}", e),
                    status: None,
                    body: None,
                })?;
            if n == 0 {
                break;
            }
            let msg: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Check for error.
            if let Some(error) = msg.get("error") {
                return Err(ProviderError::Other {
                    provider: self.id.clone(),
                    message: format!("session/prompt error: {}", error),
                    status: None,
                    body: None,
                });
            }

            // Check for notification (session/update).
            if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                if method == "session/update" {
                    if let Some(update) =
                        msg.get("params").and_then(|p| p.get("update"))
                    {
                        let update_type = update
                            .get("sessionUpdate")
                            .and_then(|u| u.as_str())
                            .unwrap_or("");
                        if update_type == "agent_message_chunk" {
                            if let Some(text) = update
                                .get("content")
                                .and_then(|c| c.get("text"))
                                .and_then(|t| t.as_str())
                            {
                                events.push(Ok(StreamEvent::TextDelta {
                                    index: 0,
                                    text: text.to_string(),
                                }));
                            }
                        }
                    }
                }
            }

            // Check for response (id matches).
            if let Some(resp_id) = msg.get("id").and_then(|i| i.as_str()) {
                if resp_id == id.to_string() {
                    // Parse stop reason from the response.
                    let stop_reason = msg
                        .get("result")
                        .and_then(|r| r.get("stopReason"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("end_turn");
                    let sr = match stop_reason {
                        "end_turn" => StopReason::EndTurn,
                        "max_tokens" => StopReason::MaxTokens,
                        "tool_use" => StopReason::ToolUse,
                        _ => StopReason::EndTurn,
                    };
                    events.push(Ok(StreamEvent::MessageDelta {
                        stop_reason: Some(sr),
                        usage: None,
                    }));
                    break;
                }
            }
        }

        // Return collected events as a stream.
        let s = stream! {
            for event in events {
                yield event;
            }
        };

        Ok(Box::pin(s))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        // First, try the existing connected process.
        {
            let guard = self.process.lock().await;
            if let Some(proc) = guard.as_ref() {
                return Ok(proc
                    .catalog
                    .models
                    .iter()
                    .map(|m| ModelInfo {
                        id: ModelId::new(m),
                        provider_id: self.id.clone(),
                        name: proc
                            .catalog
                            .names
                            .get(m)
                            .cloned()
                            .unwrap_or_else(|| m.clone()),
                        context_window: 0,
                        max_output_tokens: 0,
                        ..Default::default()
                    })
                    .collect());
            }
        }
        // No existing process — spawn a temporary one for discovery.
        // The subprocess is killed when the process handle is dropped.
        let proc = self.connect().await?;
        Ok(proc
            .catalog
            .models
            .iter()
            .map(|m| ModelInfo {
                id: ModelId::new(m),
                provider_id: self.id.clone(),
                name: proc
                    .catalog
                    .names
                    .get(m)
                    .cloned()
                    .unwrap_or_else(|| m.clone()),
                context_window: 0,
                max_output_tokens: 0,
                ..Default::default()
            })
            .collect())
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        if self.command.configured() {
            Ok(ProviderStatus::Healthy)
        } else {
            Ok(ProviderStatus::Unavailable {
                reason: "agent (Cursor CLI) not found on PATH".to_string(),
            })
        }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            thinking: false,
            image_input: false,
            pdf_input: false,
            audio_input: false,
            video_input: false,
            caching: false,
            structured_output: false,
            system_prompt_style: SystemPromptStyle::SystemMessage,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_acp_command_defaults() {
        let cmd = CursorAcpCommand::default();
        assert_eq!(cmd.executable, "agent");
        assert!(cmd.permission_args.contains(&"--force".to_string()));
        assert!(cmd.permission_args.contains(&"--trust".to_string()));
        assert_eq!(cmd.acp_arg, "acp");
    }

    #[test]
    fn cursor_acp_command_args() {
        let cmd = CursorAcpCommand::default();
        let args = cmd.args();
        assert!(args.contains(&"--force".to_string()));
        assert!(args.contains(&"--trust".to_string()));
        assert!(args.contains(&"acp".to_string()));
    }

    #[test]
    fn model_catalog_from_initialize() {
        let params = serde_json::json!({
            "availableModels": ["model-a", "model-b"],
            "currentModelId": "model-a",
        });
        let catalog = ModelCatalog::from_initialize(&params);
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.current.as_deref(), Some("model-a"));
    }

    #[test]
    fn model_catalog_from_initialize_with_config_options() {
        let params = serde_json::json!({
            "availableModels": ["model-a"],
            "currentModelId": "model-a",
            "configOptions": [
                {"category": "model", "id": "model-b"},
                {"category": "other", "id": "not-a-model"},
                {"category": "model", "id": "model-c"},
            ],
        });
        let catalog = ModelCatalog::from_initialize(&params);
        assert_eq!(catalog.models.len(), 3);
        assert!(catalog.models.contains(&"model-b".to_string()));
        assert!(catalog.models.contains(&"model-c".to_string()));
        assert_eq!(catalog.current.as_deref(), Some("model-a"));
    }

    #[test]
    fn model_catalog_resolve_exact() {
        let catalog = ModelCatalog {
            models: vec!["model-a".to_string(), "model-b".to_string()],
            current: None,
            names: HashMap::new(),
        };
        assert_eq!(
            catalog.resolve_model("model-a").as_deref(),
            Some("model-a")
        );
    }

    #[test]
    fn model_catalog_resolve_bare_id() {
        let catalog = ModelCatalog {
            models: vec!["model-a [variant]".to_string()],
            current: None,
            names: HashMap::new(),
        };
        assert_eq!(
            catalog.resolve_model("model-a").as_deref(),
            Some("model-a [variant]")
        );
    }

    #[test]
    fn model_catalog_resolve_ambiguous() {
        let catalog = ModelCatalog {
            models: vec!["model-a [v1]".to_string(), "model-a [v2]".to_string()],
            current: None,
            names: HashMap::new(),
        };
        // Ambiguous — returns None.
        assert!(catalog.resolve_model("model-a").is_none());
    }

    #[test]
    fn model_catalog_resolve_empty_catalog() {
        let catalog = ModelCatalog::default();
        assert!(catalog.resolve_model("anything").is_none());
    }

    #[test]
    fn model_catalog_from_session_new() {
        let params = serde_json::json!({
            "sessionId": "test-session",
            "modes": {"currentModeId": "agent", "availableModes": []},
            "models": {
                "currentModelId": "composer-2.5[fast=true]",
                "availableModels": [
                    {"modelId": "auto-smart[optimize_for=cost]", "name": "Auto"},
                    {"modelId": "composer-2.5[fast=true]", "name": "Composer 2.5"},
                    {"modelId": "claude-opus-4-8[thinking=true]", "name": "Opus 4.8 (MAX mode)"},
                ],
            },
            "configOptions": [
                {"category": "model", "id": "composer-2.5[fast=true]", "options": [
                    {"value": "auto-smart[optimize_for=cost]", "name": "Auto"},
                    {"value": "composer-2.5[fast=true]", "name": "Composer 2.5"},
                    {"value": "claude-opus-4-8[thinking=true]", "name": "Opus 4.8 (MAX mode)"},
                    {"value": "gpt-5.5[context=272k]", "name": "GPT-5.5 (MAX mode)"},
                ]},
            ],
        });
        let catalog = ModelCatalog::from_session_new(&params);
        assert_eq!(catalog.models.len(), 4); // 3 from availableModels + 1 new from configOptions
        assert!(catalog.models.contains(&"auto-smart[optimize_for=cost]".to_string()));
        assert!(catalog.models.contains(&"gpt-5.5[context=272k]".to_string()));
        assert_eq!(catalog.current.as_deref(), Some("composer-2.5[fast=true]"));
        assert_eq!(
            catalog.names.get("composer-2.5[fast=true]"),
            Some(&"Composer 2.5".to_string())
        );
        assert_eq!(
            catalog.names.get("claude-opus-4-8[thinking=true]"),
            Some(&"Opus 4.8 (MAX mode)".to_string())
        );
    }
}