//! Top-level ACP request / notification dispatcher.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol_schema as acp;
use claurst_core::config::Settings;
use claurst_core::permissions::PermissionManager;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::connection::{Connection, Inbound};
use crate::runtime::AgentRuntime;
use crate::sessions::{ActiveTurn, SessionRegistry, SessionState};

/// One ACP session's prompt turn reserved in `dispatch`, before the handler
/// task for `session/prompt` is spawned (issue #467).
struct ReservedPrompt {
    session: Arc<SessionState>,
    request: acp::PromptRequest,
    turn: ActiveTurn,
}

/// The ACP agent: owns the connection, the per-directory runtimes, and the
/// session registry.
pub struct AgentServer {
    pub connection: Arc<Connection>,
    /// One runtime per session working directory, built on first use.
    pub runtimes: tokio::sync::Mutex<HashMap<PathBuf, Arc<AgentRuntime>>>,
    pub sessions: Arc<SessionRegistry>,
    pub client_capabilities: parking_lot::RwLock<acp::ClientCapabilities>,
}

impl AgentServer {
    pub fn new(connection: Arc<Connection>) -> Arc<Self> {
        Arc::new(Self {
            connection,
            runtimes: tokio::sync::Mutex::new(HashMap::new()),
            sessions: Arc::new(SessionRegistry::new()),
            client_capabilities: parking_lot::RwLock::new(acp::ClientCapabilities::default()),
        })
    }

    /// The runtime for `cwd`, building it (settings, context, MCP servers
    /// for that directory) the first time a session opens there.
    ///
    /// Keyed by the canonical cwd so the same directory reached under
    /// different spellings (`/a/b`, `/a/b/`) shares one runtime.
    pub(crate) async fn runtime_for(&self, cwd: &std::path::Path) -> Result<Arc<AgentRuntime>, acp::Error> {
        // Canonicalize before using the path as the cache key; fall back to
        // the given path when the directory does not exist yet.
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        self.evict_unused_runtimes(&cwd).await;
        if let Some(runtime) = {
            let runtimes = self.runtimes.lock().await;
            runtimes.get(&cwd).cloned()
        } {
            return Ok(runtime);
        }
        // Build OUTSIDE the cache lock: MCP connects can take seconds, and
        // holding the lock across `build().await` serialised `session/new`
        // for every directory behind the first one's connects.
        let runtime = Arc::new(AgentRuntime::build(cwd.clone()).await.map_err(|e| {
            acp::Error::new(
                i32::from(acp::ErrorCode::InternalError),
                format!("Failed to start the agent for {}: {e}", cwd.display()),
            )
        })?);
        let mut runtimes = self.runtimes.lock().await;
        // Another session/new may have raced us building the same directory;
        // one runtime wins and the loser's MCP connections close when its Arc
        // is dropped at the end of this call.
        match runtimes.get(&cwd) {
            Some(existing) => Ok(existing.clone()),
            None => {
                runtimes.insert(cwd, runtime.clone());
                Ok(runtime)
            }
        }
    }

    /// Evict the runtimes of directories that no session references (closing
    /// a workspace must not pin its MCP child processes for the life of the
    /// agent). `keep_cwd` — the directory a `session/new` is currently
    /// resolving, with no session yet — is never evicted whether it is cached
    /// or being built. Session state owns its runtime independently of this
    /// cache: concurrent session creation must never be discarded based on a
    /// stale snapshot of the cache keys.
    async fn evict_unused_runtimes(&self, keep_cwd: &std::path::Path) {
        let mut evicted: Vec<Arc<AgentRuntime>> = Vec::new();
        {
            let mut runtimes = self.runtimes.lock().await;
            let keep = self.sessions.live_cwds();
            let stale: Vec<PathBuf> = runtimes
                .keys()
                .filter(|dir| dir.as_path() != keep_cwd && !keep.contains(dir.as_path()))
                .cloned()
                .collect();
            for dir in stale {
                if let Some(stale_runtime) = runtimes.remove(&dir) {
                    info!(cwd = %dir.display(), "ACP: evicting runtime for unused directory");
                    evicted.push(stale_runtime);
                }
            }
        }
        // Release the evicted runtimes (and thus their McpManagers) outside
        // the cache lock, so their teardown cannot block another directory's
        // session/new.
        drop(evicted);
    }

    /// Dispatch a single inbound message. Spawns the actual handler on a
    /// background task so the reader loop stays responsive while a prompt
    /// is in flight. Returns the join handle so the caller can wait for
    /// in-flight work to finish before shutting down.
    pub fn dispatch(self: &Arc<Self>, msg: Inbound) -> tokio::task::JoinHandle<()> {
        // Reserve the prompt turn synchronously, before the handler task is
        // spawned (issue #467): that task is queued behind everything else on
        // the runtime, so a `session/cancel` that arrives before it runs must
        // still hit the turn and produce `stopReason: "cancelled"` as the
        // schema requires. Keep reservation failures too: a busy request must
        // not become accepted merely because the previous turn finishes before
        // its response task gets scheduled.
        let prompt_turn = match &msg {
            Inbound::Request { method, params, .. } if method == "session/prompt" => {
                Some(self.reserve_prompt_turn(params.as_ref()))
            }
            _ => None,
        };
        // Cancellation belongs to the turn reserved at this wire position,
        // not whichever turn exists when a background task eventually runs.
        if let Inbound::Notification { method, params } = &msg {
            self.handle_notification(method, params.clone());
        }
        let this = self.clone();
        tokio::spawn(async move {
            match msg {
                Inbound::Request { id, method, params } => match prompt_turn {
                    Some(Ok(ReservedPrompt {
                        session,
                        request,
                        turn,
                    })) => {
                        let response =
                            crate::prompt::handle(this.connection.clone(), session, request, turn)
                                .await;
                        this.send_prompt_response(id, response).await;
                    }
                    Some(Err(error)) => {
                        this.send_prompt_response(id, Err(error)).await;
                    }
                    None => {
                        let response = this.handle_request(&method, params).await;
                        let result = match response {
                            Ok(value) => this.connection.send_response(id, value).await,
                            Err(err) => this.connection.send_error_response(id, err).await,
                        };
                        if let Err(e) = result {
                            warn!(?e, method = %method, "ACP: failed to send response");
                        }
                    }
                },
                Inbound::Notification { .. } => {}
            }
        })
    }

    /// Send the response (or error) for a `session/prompt` request whose turn
    /// was reserved in `dispatch`.
    async fn send_prompt_response(
        self: &Arc<Self>,
        id: acp::RequestId,
        response: Result<acp::PromptResponse, acp::Error>,
    ) {
        let result = match response {
            Ok(value) => self.connection.send_response(id, value).await,
            Err(err) => self.connection.send_error_response(id, err).await,
        };
        if let Err(e) = result {
            warn!(?e, method = "session/prompt", "ACP: failed to send response");
        }
    }

    /// Reserve a prompt or retain its error synchronously at dispatch time.
    fn reserve_prompt_turn(
        self: &Arc<Self>,
        params: Option<&Value>,
    ) -> Result<ReservedPrompt, acp::Error> {
        let request: acp::PromptRequest = parse_params(params.cloned())?;
        let session = self.sessions.get(&request.session_id).ok_or_else(|| {
            acp::Error::invalid_params().data(Some(serde_json::json!({
                "reason": "unknown session",
                "sessionId": request.session_id,
            })))
        })?;
        let turn = session.begin_turn().ok_or_else(|| {
            acp::Error::new(
                i32::from(acp::ErrorCode::InvalidRequest),
                format!(
                    "Session {} already has a prompt running",
                    session.session_id
                ),
            )
        })?;
        Ok(ReservedPrompt {
            turn,
            session,
            request,
        })
    }

    async fn handle_request(
        self: &Arc<Self>,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, acp::Error> {
        debug!(method, "ACP: dispatch request");
        match method {
            "initialize" => {
                let req: acp::InitializeRequest = parse_params(params)?;
                let result = self.on_initialize(req).await?;
                serde_json::to_value(result).map_err(|_| acp::Error::internal_error())
            }
            "authenticate" => {
                let _req: acp::AuthenticateRequest = parse_params(params)?;
                // Claurst uses local credentials; clients don't need to authenticate.
                serde_json::to_value(acp::AuthenticateResponse::default())
                    .map_err(|_| acp::Error::internal_error())
            }
            "session/new" => {
                let req: acp::NewSessionRequest = parse_params(params)?;
                let result = self.on_new_session(req).await?;
                serde_json::to_value(result).map_err(|_| acp::Error::internal_error())
            }
            "session/load" => {
                // v1: not supported. Capability is advertised as false in
                // initialize so a well-behaved client never calls this.
                Err(acp::Error::method_not_found())
            }
            "session/prompt" => {
                let req: acp::PromptRequest = parse_params(params)?;
                let result = self.on_prompt(req).await?;
                serde_json::to_value(result).map_err(|_| acp::Error::internal_error())
            }
            other => {
                warn!(method = other, "ACP: method not found");
                Err(acp::Error::method_not_found())
            }
        }
    }

    fn handle_notification(self: &Arc<Self>, method: &str, params: Option<Value>) {
        debug!(method, "ACP: dispatch notification");
        match method {
            "session/cancel" => {
                let parsed: Result<acp::CancelNotification, _> =
                    params.map(serde_json::from_value).unwrap_or(Err(serde::de::Error::custom(
                        "missing params",
                    )));
                match parsed {
                    Ok(notif) => {
                        if let Some(session) = self.sessions.get(&notif.session_id) {
                            // Only the running prompt is cancelled; the next
                            // prompt starts with a fresh token.
                            if session.cancel_turn() {
                                info!(session_id = %notif.session_id, "ACP: cancelling prompt");
                            } else {
                                debug!(session_id = %notif.session_id, "ACP: cancel with no prompt running");
                            }
                        }
                    }
                    Err(e) => warn!(?e, "ACP: malformed session/cancel notification"),
                }
            }
            other => {
                warn!(method = other, "ACP: ignoring unknown notification");
            }
        }
    }

    async fn on_initialize(
        self: &Arc<Self>,
        req: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        info!(
            client_version = ?req.client_info.as_ref().map(|i| (&i.name, &i.version)),
            "ACP: initialize"
        );
        *self.client_capabilities.write() = req.client_capabilities.clone();

        let agent_info = acp::Implementation::new("claurst", env!("CARGO_PKG_VERSION"))
            .title(Some("Claurst".to_string()));

        let mut response = acp::InitializeResponse::new(acp::ProtocolVersion::V1)
            .agent_capabilities(
                acp::AgentCapabilities::new()
                    .load_session(false)
                    // Images pass through when the session's provider accepts
                    // them and are replaced by a short note otherwise.
                    .prompt_capabilities(acp::PromptCapabilities::new().image(true))
                    .mcp_capabilities(acp::McpCapabilities::new()),
            );
        response = response.agent_info(Some(agent_info));
        Ok(response)
    }

    async fn on_new_session(
        self: &Arc<Self>,
        req: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        if !req.cwd.is_absolute() {
            return Err(acp::Error::invalid_params()
                .data(Some(serde_json::json!({ "reason": "cwd must be absolute" }))));
        }
        let runtime = self.runtime_for(&req.cwd).await?;
        let session_id = acp::SessionId::new(format!("acp-{}", uuid::Uuid::new_v4()));
        // Read the global rules fresh so approvals saved by earlier sessions
        // apply here too.
        let mode = runtime.config.permission_mode.clone();
        let permission_manager = match Settings::load_sync() {
            Ok(settings) => PermissionManager::new(mode, &settings),
            Err(e) => {
                warn!(error = %e, "ACP: could not reload settings, using the runtime's permission rules");
                PermissionManager::new(mode, &runtime.settings)
            }
        };
        // Project approvals for this cwd, and the configured allow/deny lists.
        let mut permission_manager = permission_manager.with_project_store(
            claurst_core::permissions::project_approvals_path(
                &Settings::config_dir(),
                &claurst_core::permissions::approvals_project_root(&req.cwd),
            ),
        );
        permission_manager.sync_config(&runtime.config);
        let state = SessionState::new(session_id.clone(), runtime, permission_manager);
        info!(session_id = %session_id, cwd = %req.cwd.display(), "ACP: new session");

        // v1: ignore req.mcp_servers — agent uses settings.json MCP roster.
        if !req.mcp_servers.is_empty() {
            warn!(
                count = req.mcp_servers.len(),
                "ACP: session-specific MCP servers are not yet routed (v1) — using the configured servers for this directory"
            );
        }

        self.sessions.insert(state);
        Ok(acp::NewSessionResponse::new(session_id))
    }

    async fn on_prompt(
        self: &Arc<Self>,
        req: acp::PromptRequest,
    ) -> Result<acp::PromptResponse, acp::Error> {
        let session = match self.sessions.get(&req.session_id) {
            Some(s) => s,
            None => {
                return Err(acp::Error::invalid_params().data(Some(serde_json::json!({
                    "reason": "unknown session",
                    "sessionId": req.session_id,
                }))));
            }
        };
        // Direct handler path; normal dispatch already reserves synchronously.
        let Some(turn) = session.begin_turn() else {
            return Err(acp::Error::new(
                i32::from(acp::ErrorCode::InvalidRequest),
                format!("Session {} already has a prompt running", session.session_id),
            ));
        };
        crate::prompt::handle(self.connection.clone(), session, req, turn).await
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(params: Option<Value>) -> Result<T, acp::Error> {
    let value = params.ok_or_else(acp::Error::invalid_params)?;
    serde_json::from_value(value).map_err(|e| {
        acp::Error::invalid_params().data(Some(serde_json::json!({ "deserialize_error": e.to_string() })))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Params {
        name: String,
    }

    #[test]
    fn parse_params_missing_returns_invalid_params_error() {
        let result: Result<Params, acp::Error> = parse_params(None);
        let err = result.expect_err("None params should be rejected");
        assert_eq!(err.code, acp::ErrorCode::InvalidParams);
    }

    #[test]
    fn parse_params_deserializes_matching_shape() {
        let value = serde_json::json!({ "name": "claurst" });
        let result: Params = parse_params(Some(value)).expect("valid params should parse");
        assert_eq!(result, Params { name: "claurst".to_string() });
    }

    #[test]
    fn parse_params_mismatched_shape_returns_invalid_params_error_with_detail() {
        let value = serde_json::json!({ "wrong_field": 1 });
        let result: Result<Params, acp::Error> = parse_params(Some(value));
        let err = result.expect_err("mismatched shape should be rejected");
        assert_eq!(err.code, acp::ErrorCode::InvalidParams);
        assert!(err.data.is_some());
    }
}
