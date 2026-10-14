//! End-to-end tests: a real `AgentServer` over an in-memory JSON-RPC pipe,
//! driven by a scripted provider and a fake ACP client.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol_schema as acp;
use claurst_api::{ProviderError, ProviderRequest, StreamEvent};
use claurst_core::types::{ContentBlock, UsageInfo};
use dashmap::DashMap;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{mpsc, oneshot, Notify};

use crate::connection::{self, Connection};
use crate::runtime::AgentRuntime;
use crate::server::AgentServer;

/// One scripted provider response.
pub(crate) enum Step {
    /// Never produce a response; only a cancel ends the turn.
    Hang,
    /// End the turn with this text.
    Text(&'static str),
    /// Call one tool with this input.
    Tool(&'static str, Value),
    /// Fail the request with a provider error.
    Fail(&'static str),
}

/// Provider double that replays `Step`s and records every request.
pub(crate) struct ScriptedProvider {
    id: claurst_core::provider_id::ProviderId,
    steps: parking_lot::Mutex<VecDeque<Step>>,
    pub requests: parking_lot::Mutex<Vec<ProviderRequest>>,
    pub image_input: bool,
    started: Notify,
}

impl ScriptedProvider {
    pub fn new(steps: Vec<Step>) -> Arc<Self> {
        Self::build(steps, false)
    }

    /// A provider that reports image input support.
    pub fn with_images(steps: Vec<Step>) -> Arc<Self> {
        Self::build(steps, true)
    }

    fn build(steps: Vec<Step>, image_input: bool) -> Arc<Self> {
        Arc::new(Self {
            id: claurst_core::provider_id::ProviderId::new("scripted"),
            steps: parking_lot::Mutex::new(steps.into()),
            requests: parking_lot::Mutex::new(Vec::new()),
            image_input,
            started: Notify::new(),
        })
    }

    /// Wait until the provider has received a request.
    pub async fn wait_started(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.started.notified())
            .await
            .expect("provider never received a request");
    }
}

type EventStream = std::pin::Pin<
    Box<dyn futures::Stream<Item = Result<StreamEvent, ProviderError>> + Send>,
>;

#[async_trait::async_trait]
impl claurst_api::LlmProvider for ScriptedProvider {
    fn id(&self) -> &claurst_core::provider_id::ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "scripted"
    }

    async fn create_message(
        &self,
        _request: ProviderRequest,
    ) -> Result<claurst_api::ProviderResponse, ProviderError> {
        unimplemented!("the query loop streams")
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<EventStream, ProviderError> {
        use claurst_api::provider_types::StopReason;

        self.requests.lock().push(request);
        self.started.notify_one();
        let step = self.steps.lock().pop_front().unwrap_or(Step::Text("done"));
        let start = Ok(StreamEvent::MessageStart {
            id: uuid::Uuid::new_v4().to_string(),
            model: "scripted-model".to_string(),
            usage: UsageInfo::default(),
        });
        let events = match step {
            Step::Hang => return Ok(Box::pin(futures::stream::pending())),
            Step::Fail(message) => {
                return Err(ProviderError::AuthFailed {
                    provider: self.id.clone(),
                    message: message.to_string(),
                })
            }
            Step::Text(text) => vec![
                start,
                Ok(StreamEvent::TextDelta {
                    index: 0,
                    text: text.to_string(),
                }),
                Ok(StreamEvent::MessageDelta {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Some(UsageInfo::default()),
                }),
                Ok(StreamEvent::MessageStop),
            ],
            Step::Tool(name, input) => vec![
                start,
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    content_block: ContentBlock::ToolUse {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: name.to_string(),
                        input: json!({}),
                        thought_signature: None,
                    },
                }),
                Ok(StreamEvent::InputJsonDelta {
                    index: 0,
                    partial_json: input.to_string(),
                }),
                Ok(StreamEvent::MessageDelta {
                    stop_reason: Some(StopReason::ToolUse),
                    usage: Some(UsageInfo::default()),
                }),
                Ok(StreamEvent::MessageStop),
            ],
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn health_check(&self) -> Result<claurst_api::ProviderStatus, ProviderError> {
        Ok(claurst_api::ProviderStatus::Healthy)
    }

    fn capabilities(&self) -> claurst_api::ProviderCapabilities {
        claurst_api::ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            thinking: false,
            image_input: self.image_input,
            pdf_input: false,
            audio_input: false,
            video_input: false,
            caching: false,
            structured_output: false,
            system_prompt_style: claurst_api::SystemPromptStyle::TopLevel,
        }
    }
}

/// A runtime for `cwd` that routes every request to `provider`.
pub(crate) fn scripted_runtime(cwd: &Path, provider: Arc<ScriptedProvider>) -> AgentRuntime {
    let settings = claurst_core::config::Settings::default();
    let mut config = settings.effective_config();
    config.provider = Some("scripted".to_string());
    config.project_dir = Some(cwd.to_path_buf());

    let mut registry = claurst_api::ProviderRegistry::new();
    registry.register(provider);
    let provider_registry = Arc::new(registry);

    let api_client = Arc::new(
        claurst_api::AnthropicClient::new(claurst_api::client::ClientConfig {
            api_key: "test-key".to_string(),
            ..Default::default()
        })
        .expect("build test client"),
    );

    let mut query_config = claurst_query::QueryConfig::from_config(&config);
    query_config.model = "scripted-model".to_string();
    query_config.max_turns = 10;
    query_config.provider_registry = Some(provider_registry.clone());
    let supports_images = crate::runtime::provider_accepts_images(&config, &provider_registry);

    AgentRuntime {
        config,
        settings,
        api_client,
        provider_registry,
        tools: Arc::new(claurst_tools::all_tools()),
        cost_tracker: claurst_core::CostTracker::new(),
        query_config,
        mcp_manager: None,
        working_dir: cwd.to_path_buf(),
        supports_images,
    }
}

/// Fake ACP client wired to a running `AgentServer`.
pub(crate) struct Client {
    writer: tokio::sync::Mutex<DuplexStream>,
    next_id: AtomicI64,
    responses: Arc<DashMap<i64, oneshot::Sender<Value>>>,
    /// Every `session/request_permission` the agent sent.
    pub permission_requests: Arc<parking_lot::Mutex<Vec<Value>>>,
    /// Option ids to answer permission requests with, in order. When empty
    /// the client rejects.
    pub permission_answers: Arc<parking_lot::Mutex<VecDeque<&'static str>>>,
    /// Every `session/update` notification the agent sent.
    pub updates: Arc<parking_lot::Mutex<Vec<Value>>>,
}

impl Client {
    /// Start a server with `runtime` and connect a client to it.
    pub fn start(runtime: AgentRuntime) -> Arc<Self> {
        let (client_write, server_read) = tokio::io::duplex(1 << 20);
        let (server_write, client_read) = tokio::io::duplex(1 << 20);

        let connection = Connection::new(server_write);
        let server = AgentServer::new(connection.clone());
        server
            .runtimes
            .try_lock()
            .expect("a new server's runtime cache is free")
            .insert(runtime.working_dir.clone(), Arc::new(runtime));
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(connection::run_reader(connection, server_read, tx));
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                server.dispatch(msg);
            }
        });

        let client = Arc::new(Self {
            writer: tokio::sync::Mutex::new(client_write),
            next_id: AtomicI64::new(1),
            responses: Arc::new(DashMap::new()),
            permission_requests: Arc::new(parking_lot::Mutex::new(Vec::new())),
            permission_answers: Arc::new(parking_lot::Mutex::new(VecDeque::new())),
            updates: Arc::new(parking_lot::Mutex::new(Vec::new())),
        });
        tokio::spawn(Self::read_loop(client.clone(), client_read));
        client
    }

    async fn read_loop(self: Arc<Self>, reader: DuplexStream) {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&line).expect("agent wrote invalid JSON");
            match msg.get("method").and_then(Value::as_str) {
                Some("session/request_permission") => {
                    self.permission_requests.lock().push(msg["params"].clone());
                    let answer = self.permission_answers.lock().pop_front().unwrap_or("reject_once");
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": msg["id"],
                        "result": { "outcome": { "outcome": "selected", "optionId": answer } },
                    });
                    self.write(&reply).await;
                }
                Some("session/update") => self.updates.lock().push(msg["params"].clone()),
                Some(other) => panic!("unexpected agent message: {other}"),
                None => {
                    let id = msg["id"].as_i64().expect("response id");
                    if let Some((_, tx)) = self.responses.remove(&id) {
                        let _ = tx.send(msg);
                    }
                }
            }
        }
    }

    async fn write(&self, msg: &Value) {
        let mut bytes = serde_json::to_vec(msg).expect("serialize");
        bytes.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(&bytes).await.expect("write to agent");
        writer.flush().await.expect("flush to agent");
    }

    /// Send a request and return the full response envelope.
    pub async fn call(&self, method: &str, params: impl serde::Serialize) -> Value {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.responses.insert(id, tx);
        let params = serde_json::to_value(params).expect("serialize params");
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        tokio::time::timeout(Duration::from_secs(20), rx)
            .await
            .unwrap_or_else(|_| panic!("no response to {method}"))
            .expect("response channel closed")
    }

    pub async fn notify(&self, method: &str, params: impl serde::Serialize) {
        let params = serde_json::to_value(params).expect("serialize params");
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await;
    }

    pub async fn new_session(&self, cwd: &Path) -> acp::SessionId {
        let response = self
            .call("session/new", acp::NewSessionRequest::new(cwd.to_path_buf()))
            .await;
        let result: acp::NewSessionResponse =
            serde_json::from_value(response["result"].clone()).expect("session/new result");
        result.session_id
    }

    pub async fn prompt(&self, session_id: &acp::SessionId, text: &str) -> Value {
        self.call(
            "session/prompt",
            acp::PromptRequest::new(
                session_id.clone(),
                vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
            ),
        )
        .await
    }
}

/// Parse the stop reason out of a `session/prompt` response.
pub(crate) fn stop_reason(response: &Value) -> acp::StopReason {
    let result: acp::PromptResponse = serde_json::from_value(response["result"].clone())
        .unwrap_or_else(|e| panic!("expected a PromptResponse, got {response}: {e}"));
    result.stop_reason
}

/// Serialises the tests that read or write the global settings, and points
/// the settings directory at a fresh scratch directory for each of them so
/// an "Allow always" rule saved by one test never leaks into another (or
/// into the real user settings).
pub(crate) async fn isolated_home() -> tokio::sync::MutexGuard<'static, ()> {
    static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let guard = HOME_LOCK.lock().await;
    std::env::set_var("CLAURST_HOME", scratch_dir("home"));
    guard
}

/// Tool-call updates the agent sent, as `(status, tool title)` pairs.
pub(crate) fn tool_statuses(client: &Client) -> Vec<String> {
    client
        .updates
        .lock()
        .iter()
        .filter(|u| u["update"]["sessionUpdate"] == "tool_call_update")
        .map(|u| u["update"]["status"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A fresh, canonical scratch directory.
pub(crate) fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("claurst-acp-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::canonicalize(&dir).expect("canonicalize scratch dir")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompt_after_cancel_runs_normally() {
    let _home = isolated_home().await;
    let cwd = scratch_dir("cancel");
    let provider = ScriptedProvider::new(vec![Step::Hang, Step::Text("second answer")]);
    let client = Client::start(scripted_runtime(&cwd, provider.clone()));
    let session_id = client.new_session(&cwd).await;

    let first = {
        let client = client.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move { client.prompt(&session_id, "first").await })
    };
    provider.wait_started().await;
    client
        .notify("session/cancel", acp::CancelNotification::new(session_id.clone()))
        .await;
    let first = first.await.expect("first prompt task");
    assert_eq!(stop_reason(&first), acp::StopReason::Cancelled);

    let second = client.prompt(&session_id, "second").await;
    assert_eq!(
        stop_reason(&second),
        acp::StopReason::EndTurn,
        "a cancel must not poison later prompts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_prompt_on_one_session_is_rejected() {
    let _home = isolated_home().await;
    let cwd = scratch_dir("busy");
    let provider = ScriptedProvider::new(vec![Step::Hang]);
    let client = Client::start(scripted_runtime(&cwd, provider.clone()));
    let session_id = client.new_session(&cwd).await;

    let first = {
        let client = client.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move { client.prompt(&session_id, "first").await })
    };
    provider.wait_started().await;

    let second = client.prompt(&session_id, "second").await;
    let error: acp::Error =
        serde_json::from_value(second["error"].clone()).expect("busy prompt must be an error");
    assert_eq!(error.code, acp::ErrorCode::InvalidRequest);
    assert!(error.message.contains("already has a prompt running"), "{}", error.message);

    client
        .notify("session/cancel", acp::CancelNotification::new(session_id.clone()))
        .await;
    let first = first.await.expect("first prompt task");
    assert_eq!(stop_reason(&first), acp::StopReason::Cancelled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_inside_workspace_runs_without_asking_the_client() {
    let _home = isolated_home().await;
    let cwd = scratch_dir("read");
    let file = cwd.join("notes.txt");
    std::fs::write(&file, "hello").expect("seed file");
    let provider = ScriptedProvider::new(vec![
        Step::Tool("Read", json!({ "file_path": file })),
        Step::Text("read it"),
    ]);
    let client = Client::start(scripted_runtime(&cwd, provider));
    let session_id = client.new_session(&cwd).await;

    let response = client.prompt(&session_id, "read the notes").await;
    assert_eq!(stop_reason(&response), acp::StopReason::EndTurn);
    assert!(
        client.permission_requests.lock().is_empty(),
        "a read inside the workspace is allowed by policy and must not reach the client"
    );
    assert_eq!(tool_statuses(&client), vec!["completed"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_outside_workspace_asks_the_client() {
    let _home = isolated_home().await;
    let cwd = scratch_dir("workspace");
    let outside = scratch_dir("outside").join("secret.txt");
    std::fs::write(&outside, "secret").expect("seed file");
    let provider = ScriptedProvider::new(vec![
        Step::Tool("Read", json!({ "file_path": outside })),
        Step::Text("read it"),
    ]);
    let client = Client::start(scripted_runtime(&cwd, provider));
    client.permission_answers.lock().push_back("allow_once");
    let session_id = client.new_session(&cwd).await;

    let response = client.prompt(&session_id, "read the secret").await;
    assert_eq!(stop_reason(&response), acp::StopReason::EndTurn);
    let requests = client.permission_requests.lock().clone();
    assert_eq!(requests.len(), 1, "{requests:?}");
    let request: acp::RequestPermissionRequest =
        serde_json::from_value(requests[0].clone()).expect("request_permission params");
    assert_eq!(request.session_id, session_id);
    assert_eq!(tool_statuses(&client), vec!["completed"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_asks_once_then_allow_always_is_remembered_and_saved() {
    let _home = isolated_home().await;
    let cwd = scratch_dir("write");
    let target = cwd.join("out.txt");
    let provider = ScriptedProvider::new(vec![
        Step::Tool("Write", json!({ "file_path": target, "content": "one" })),
        Step::Text("wrote one"),
        Step::Tool("Write", json!({ "file_path": target, "content": "two" })),
        Step::Text("wrote two"),
    ]);
    let client = Client::start(scripted_runtime(&cwd, provider));
    client.permission_answers.lock().push_back("allow_always");
    let session_id = client.new_session(&cwd).await;

    let first = client.prompt(&session_id, "write one").await;
    assert_eq!(stop_reason(&first), acp::StopReason::EndTurn);
    assert_eq!(client.permission_requests.lock().len(), 1);
    assert_eq!(std::fs::read_to_string(&target).expect("written"), "one");

    let second = client.prompt(&session_id, "write two").await;
    assert_eq!(stop_reason(&second), acp::StopReason::EndTurn);
    assert_eq!(
        client.permission_requests.lock().len(),
        1,
        "allow-always must not ask again in the same session"
    );
    assert_eq!(std::fs::read_to_string(&target).expect("written"), "two");

    let saved = claurst_core::config::Settings::load_sync().expect("settings readable");
    assert!(
        saved
            .permission_rules
            .iter()
            .any(|rule| rule.tool_name.as_deref() == Some("Write")),
        "allow-always must be saved like the TUI does: {:?}",
        saved.permission_rules
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_write_fails_the_tool_and_asks_again_next_time() {
    let _home = isolated_home().await;
    let cwd = scratch_dir("reject");
    let target = cwd.join("out.txt");
    let provider = ScriptedProvider::new(vec![
        Step::Tool("Write", json!({ "file_path": target, "content": "one" })),
        Step::Text("could not write"),
        Step::Tool("Write", json!({ "file_path": target, "content": "two" })),
        Step::Text("could not write"),
    ]);
    let client = Client::start(scripted_runtime(&cwd, provider));
    client
        .permission_answers
        .lock()
        .extend(["reject_once", "reject_once"]);
    let session_id = client.new_session(&cwd).await;

    client.prompt(&session_id, "write one").await;
    client.prompt(&session_id, "write two").await;
    assert_eq!(client.permission_requests.lock().len(), 2);
    assert!(!target.exists(), "a rejected write must not touch the file");
    assert_eq!(tool_statuses(&client), vec!["failed", "failed"]);
}

/// A minimal stdio MCP server: answers `initialize`, lists one `ping` tool,
/// and returns "pong" for every call.
#[cfg(unix)]
const FAKE_MCP_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -n 1 | cut -d: -f2)
  [ -z "$id" ] && continue
  case "$line" in
    *'"method":"initialize"'*)
      version=$(printf '%s' "$line" | grep -o '"protocolVersion":"[^"]*"' | head -n 1 | cut -d'"' -f4)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"%s","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1.0.0"}}}\n' "$id" "$version" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"ping","description":"Reply with pong","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}\n' "$id" ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"method not found"}}\n' "$id" ;;
  esac
done
"#;

#[cfg(unix)]
fn sh_server(
    name: &str,
    script: &str,
    origin: claurst_core::config::McpServerOrigin,
) -> claurst_core::config::McpServerConfig {
    claurst_core::config::McpServerConfig {
        name: name.to_string(),
        command: Some("sh".to_string()),
        args: vec!["-c".to_string(), script.to_string()],
        env: std::collections::HashMap::new(),
        url: None,
        server_type: "stdio".to_string(),
        origin,
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trusted_mcp_tools_are_attached_and_callable() {
    use claurst_core::config::{McpServerOrigin, Settings};

    let _home = isolated_home().await;
    let cwd = scratch_dir("mcp");
    let marker = cwd.join("project-server-ran");
    let settings = Settings::default();
    let mut config = settings.effective_config();
    config.mcp_servers = vec![
        sh_server("fake", FAKE_MCP_SERVER, McpServerOrigin::User),
        sh_server(
            "repo",
            &format!("touch '{}'", marker.display()),
            McpServerOrigin::Project,
        ),
    ];

    let manager = crate::runtime::build_mcp_manager(&config, &settings, &cwd)
        .await
        .expect("the user-defined server is trusted");
    let names: Vec<String> = claurst_tools::mcp_tools(&manager)
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    assert_eq!(names, vec!["fake_ping"]);
    assert!(
        !marker.exists(),
        "an untrusted project server must not be launched"
    );

    let provider = ScriptedProvider::new(vec![
        Step::Tool("fake_ping", json!({})),
        Step::Text("got pong"),
    ]);
    let mut runtime = scripted_runtime(&cwd, provider);
    let mut tools = claurst_tools::all_tools();
    tools.extend(claurst_tools::mcp_tools(&manager));
    runtime.tools = Arc::new(tools);
    runtime.mcp_manager = Some(manager);

    let client = Client::start(runtime);
    client.permission_answers.lock().push_back("allow_once");
    let session_id = client.new_session(&cwd).await;
    let response = client.prompt(&session_id, "ping the server").await;
    assert_eq!(stop_reason(&response), acp::StopReason::EndTurn);
    assert_eq!(
        client.permission_requests.lock().len(),
        1,
        "an MCP tool asks once (no double prompt from the central backstop)"
    );
    let outputs: Vec<Value> = client
        .updates
        .lock()
        .iter()
        .filter(|u| u["update"]["sessionUpdate"] == "tool_call_update")
        .map(|u| u["update"]["rawOutput"].clone())
        .collect();
    assert_eq!(outputs, vec![json!("pong")]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_turn_is_a_jsonrpc_error_not_a_refusal() {
    let _home = isolated_home().await;
    let cwd = scratch_dir("error");
    let provider = ScriptedProvider::new(vec![Step::Fail("invalid api key")]);
    let client = Client::start(scripted_runtime(&cwd, provider));
    let session_id = client.new_session(&cwd).await;

    let response = client.prompt(&session_id, "hello").await;
    assert!(response.get("result").is_none(), "{response}");
    let error: acp::Error =
        serde_json::from_value(response["error"].clone()).expect("an ACP error object");
    assert_eq!(error.code, acp::ErrorCode::InternalError);
    assert!(error.message.contains("invalid api key"), "{}", error.message);

    let retry = client.prompt(&session_id, "hello again").await;
    assert_eq!(
        stop_reason(&retry),
        acp::StopReason::EndTurn,
        "a failed turn must leave the session usable"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompt_images_reach_a_provider_that_accepts_them() {
    let _home = isolated_home().await;
    let cwd = scratch_dir("image");
    let provider = ScriptedProvider::with_images(vec![Step::Text("a cat")]);
    let client = Client::start(scripted_runtime(&cwd, provider.clone()));
    let session_id = client.new_session(&cwd).await;

    let response = client
        .call(
            "session/prompt",
            acp::PromptRequest::new(
                session_id.clone(),
                vec![
                    acp::ContentBlock::Text(acp::TextContent::new("what is this?")),
                    acp::ContentBlock::Image(acp::ImageContent::new("aGVsbG8=", "image/png")),
                ],
            ),
        )
        .await;
    assert_eq!(stop_reason(&response), acp::StopReason::EndTurn);

    let requests = provider.requests.lock();
    let user = requests[0]
        .messages
        .iter()
        .find(|m| m.role == claurst_core::types::Role::User)
        .expect("the request carries the user turn");
    let has_image = user.content_blocks().iter().any(|block| {
        matches!(block, ContentBlock::Image { source } if source.data.as_deref() == Some("aGVsbG8="))
    });
    assert!(has_image, "the image must be passed to the provider: {user:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runtime_uses_settings_and_agents_md_of_the_session_cwd() {
    let _home = isolated_home().await;
    let cwd = scratch_dir("project");
    std::fs::create_dir_all(cwd.join(".claurst")).expect("create .claurst");
    let mut project = claurst_core::config::Settings::default();
    project.config.model = Some("project-pinned-model".to_string());
    std::fs::write(
        cwd.join(".claurst").join("settings.json"),
        serde_json::to_string(&project).expect("serialize project settings"),
    )
    .expect("write project settings");
    std::fs::write(cwd.join("AGENTS.md"), "Project rule: PROJECT-RULE-MARKER").expect("write AGENTS.md");

    let runtime = AgentRuntime::build(cwd.clone()).await.expect("runtime builds");
    assert_eq!(runtime.working_dir, cwd);
    assert_eq!(runtime.query_config.model, "project-pinned-model");
    let system_prompt = runtime.query_config.system_prompt.as_deref().unwrap_or_default();
    assert!(
        system_prompt.contains("PROJECT-RULE-MARKER"),
        "AGENTS.md from the session cwd must reach the system prompt"
    );
}
