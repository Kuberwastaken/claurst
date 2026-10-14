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
        Arc::new(Self {
            id: claurst_core::provider_id::ProviderId::new("scripted"),
            steps: parking_lot::Mutex::new(steps.into()),
            requests: parking_lot::Mutex::new(Vec::new()),
            image_input: false,
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
        let server = AgentServer::new(connection.clone(), Arc::new(runtime));
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
