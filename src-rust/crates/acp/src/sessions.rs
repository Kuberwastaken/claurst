//! Per-session state for the ACP server.

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol_schema as acp;
use claurst_core::types::Message;
use claurst_tools::PendingPermissionStore;
use dashmap::DashMap;
use tokio_util::sync::CancellationToken;

/// One ACP session — a logical conversation with its own cwd, transcript,
/// MCP server roster, and cancellation token.
pub struct SessionState {
    pub session_id: acp::SessionId,
    pub cwd: PathBuf,
    pub messages: parking_lot::Mutex<Vec<Message>>,
    pub cancel_token: CancellationToken,
    pub pending_permissions: Arc<parking_lot::Mutex<PendingPermissionStore>>,
    pub file_history: Arc<parking_lot::Mutex<claurst_core::file_history::FileHistory>>,
    pub current_turn: Arc<std::sync::atomic::AtomicUsize>,
}

impl SessionState {
    pub fn new(session_id: acp::SessionId, cwd: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            cwd,
            messages: parking_lot::Mutex::new(Vec::new()),
            cancel_token: CancellationToken::new(),
            pending_permissions: Arc::new(parking_lot::Mutex::new(
                PendingPermissionStore::default(),
            )),
            file_history: Arc::new(parking_lot::Mutex::new(
                claurst_core::file_history::FileHistory::new(),
            )),
            current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
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

    #[test]
    fn new_session_starts_empty_with_a_fresh_cancel_token() {
        let id = acp::SessionId::new("session-1");
        let cwd = PathBuf::from("/tmp/claurst-test");
        let state = SessionState::new(id.clone(), cwd.clone());

        assert_eq!(state.session_id, id);
        assert_eq!(state.cwd, cwd);
        assert!(state.messages.lock().is_empty());
        assert_eq!(
            state.current_turn.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(!state.cancel_token.is_cancelled());
    }

    #[test]
    fn registry_insert_get_remove_round_trip() {
        let registry = SessionRegistry::new();
        let id = acp::SessionId::new("session-2");
        let state = SessionState::new(id.clone(), PathBuf::from("/tmp"));

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
