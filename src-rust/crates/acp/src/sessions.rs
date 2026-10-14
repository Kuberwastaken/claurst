//! Per-session state for the ACP server.

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol_schema as acp;
use claurst_core::permissions::PermissionManager;
use claurst_core::types::Message;
use claurst_tools::PendingPermissionStore;
use dashmap::DashMap;
use tokio_util::sync::CancellationToken;

use crate::runtime::AgentRuntime;

/// One ACP session — a logical conversation with its own cwd, transcript,
/// MCP server roster, and the cancellation token of its running prompt.
pub struct SessionState {
    pub session_id: acp::SessionId,
    pub cwd: PathBuf,
    /// Runtime built for `cwd`: settings, context, tools and MCP servers.
    pub runtime: Arc<AgentRuntime>,
    pub messages: parking_lot::Mutex<Vec<Message>>,
    /// Cancellation token of the prompt currently running on this session,
    /// `None` while the session is idle. Each prompt gets a fresh token, so a
    /// `session/cancel` only ever affects the turn that was in flight.
    active_turn: parking_lot::Mutex<Option<CancellationToken>>,
    pub pending_permissions: Arc<parking_lot::Mutex<PendingPermissionStore>>,
    /// Decides every tool check before anything is sent to the client, and
    /// holds this session's "Allow always" rules.
    pub permission_manager: Arc<std::sync::Mutex<PermissionManager>>,
    pub file_history: Arc<parking_lot::Mutex<claurst_core::file_history::FileHistory>>,
    pub current_turn: Arc<std::sync::atomic::AtomicUsize>,
}

impl SessionState {
    pub fn new(
        session_id: acp::SessionId,
        runtime: Arc<AgentRuntime>,
        permission_manager: PermissionManager,
    ) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            cwd: runtime.working_dir.clone(),
            runtime,
            messages: parking_lot::Mutex::new(Vec::new()),
            active_turn: parking_lot::Mutex::new(None),
            pending_permissions: Arc::new(parking_lot::Mutex::new(
                PendingPermissionStore::default(),
            )),
            permission_manager: Arc::new(std::sync::Mutex::new(permission_manager)),
            file_history: Arc::new(parking_lot::Mutex::new(
                claurst_core::file_history::FileHistory::new(),
            )),
            current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    /// Start a prompt turn with a fresh cancellation token. Returns `None`
    /// when another prompt is already running on this session.
    pub fn begin_turn(&self) -> Option<ActiveTurn<'_>> {
        let mut slot = self.active_turn.lock();
        if slot.is_some() {
            return None;
        }
        let token = CancellationToken::new();
        *slot = Some(token.clone());
        Some(ActiveTurn {
            slot: &self.active_turn,
            token,
        })
    }

    /// Cancel the running prompt, if any. Returns whether a turn was running.
    pub fn cancel_turn(&self) -> bool {
        match self.active_turn.lock().as_ref() {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }
}

/// A running prompt turn. Dropping it marks the session idle again.
pub struct ActiveTurn<'a> {
    slot: &'a parking_lot::Mutex<Option<CancellationToken>>,
    pub token: CancellationToken,
}

impl Drop for ActiveTurn<'_> {
    fn drop(&mut self) {
        *self.slot.lock() = None;
    }
}

/// Map of active sessions keyed by ACP session id.
#[derive(Default)]
pub struct SessionRegistry {
    inner: DashMap<acp::SessionId, Arc<SessionState>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, state: Arc<SessionState>) {
        self.inner.insert(state.session_id.clone(), state);
    }

    pub fn get(&self, id: &acp::SessionId) -> Option<Arc<SessionState>> {
        self.inner.get(id).map(|r| r.value().clone())
    }

    pub fn remove(&self, id: &acp::SessionId) -> Option<Arc<SessionState>> {
        self.inner.remove(id).map(|(_, v)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, cwd: &str) -> Arc<SessionState> {
        let manager = PermissionManager::new(
            claurst_core::PermissionMode::Default,
            &claurst_core::config::Settings::default(),
        );
        let provider = crate::e2e_tests::ScriptedProvider::new(Vec::new());
        let runtime = crate::e2e_tests::scripted_runtime(std::path::Path::new(cwd), provider);
        SessionState::new(acp::SessionId::new(id), Arc::new(runtime), manager)
    }

    #[test]
    fn new_session_starts_empty_and_idle() {
        let id = acp::SessionId::new("session-1");
        let cwd = PathBuf::from("/tmp/claurst-test");
        let state = session("session-1", "/tmp/claurst-test");

        assert_eq!(state.session_id, id);
        assert_eq!(state.cwd, cwd);
        assert!(state.messages.lock().is_empty());
        assert_eq!(
            state.current_turn.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(!state.cancel_turn(), "a new session has no turn to cancel");
    }

    #[test]
    fn second_concurrent_turn_is_rejected() {
        let state = session("busy", "/tmp");
        let first = state.begin_turn().expect("idle session accepts a turn");
        assert!(state.begin_turn().is_none(), "a running turn must block a second one");
        drop(first);
        assert!(state.begin_turn().is_some(), "the session is idle again once the turn ends");
    }

    #[test]
    fn cancel_only_affects_the_running_turn() {
        let state = session("cancel", "/tmp");
        let first = state.begin_turn().expect("idle session accepts a turn");
        assert!(state.cancel_turn());
        assert!(first.token.is_cancelled());
        drop(first);

        let second = state.begin_turn().expect("idle session accepts a turn");
        assert!(
            !second.token.is_cancelled(),
            "a prompt after a cancel must start with a live token"
        );
    }

    #[test]
    fn registry_insert_get_remove_round_trip() {
        let registry = SessionRegistry::new();
        let id = acp::SessionId::new("session-2");
        let state = session("session-2", "/tmp");

        assert!(registry.get(&id).is_none());

        registry.insert(Arc::clone(&state));
        let fetched = registry.get(&id).expect("session should be present after insert");
        assert!(Arc::ptr_eq(&fetched, &state));

        let removed = registry.remove(&id).expect("session should be present to remove");
        assert!(Arc::ptr_eq(&removed, &state));
        assert!(registry.get(&id).is_none());
    }

    #[test]
    fn registry_remove_unknown_id_returns_none() {
        let registry = SessionRegistry::new();
        let id = acp::SessionId::new("missing");
        assert!(registry.remove(&id).is_none());
    }
}
