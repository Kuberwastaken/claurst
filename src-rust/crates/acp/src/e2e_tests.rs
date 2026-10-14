//! End-to-end tests: a real `AgentServer` over an in-memory JSON-RPC pipe,
//! driven by a scripted provider and a fake ACP client.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol_schema as acp;
use claurst_api::{ProviderError, ProviderRequest, StreamEvent};
use claurst_core::types::UsageInfo;
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

    let permission_manager = Arc::new(std::sync::Mutex::new(
        claurst_core::permissions::PermissionManager::new(config.permission_mode.clone(), &settings),
    ));

    AgentRuntime {
        config,
        settings,
        api_client,
        provider_registry,
        tools: Arc::new(claurst_tools::all_tools()),
        cost_tracker: claurst_core::CostTracker::new(),
        query_config,
        mcp_manager: None,
        permission_manager,
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

/// A fresh, canonical scratch directory.
pub(crate) fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("claurst-acp-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::canonicalize(&dir).expect("canonicalize scratch dir")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompt_after_cancel_runs_normally() {
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
