//! Test-only helpers for exercising a tool's `execute` against a permissive
//! `ToolContext` rooted at a caller-supplied (usually temp) directory.

use crate::ToolContext;
use std::path::PathBuf;
use std::sync::Arc;

/// Permission handler that approves everything, so `execute` runs unattended.
pub(crate) struct AllowAllHandler;

impl claurst_core::permissions::PermissionHandler for AllowAllHandler {
    fn check_permission(
        &self,
        _request: &claurst_core::permissions::PermissionRequest,
    ) -> claurst_core::permissions::PermissionDecision {
        claurst_core::permissions::PermissionDecision::Allow
    }

    fn request_permission(
        &self,
        _request: &claurst_core::permissions::PermissionRequest,
    ) -> claurst_core::permissions::PermissionDecision {
        claurst_core::permissions::PermissionDecision::Allow
    }
}

/// Build a permissive, non-interactive `ToolContext` rooted at `working_dir`.
pub(crate) fn allow_all_context(working_dir: PathBuf) -> ToolContext {
    ToolContext {
        working_dir,
        permission_mode: claurst_core::config::PermissionMode::Default,
        permission_handler: Arc::new(AllowAllHandler),
        cost_tracker: claurst_core::cost::CostTracker::new(),
        session_id: "eol-test".to_string(),
        file_history: Arc::new(parking_lot::Mutex::new(
            claurst_core::file_history::FileHistory::new(),
        )),
        current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        non_interactive: true,
        mcp_manager: None,
        config: claurst_core::config::Config::default(),
        managed_agent_config: None,
        completion_notifier: None,
        pending_permissions: None,
        permission_manager: None,
        user_question_tx: None,
        cancel_token: tokio_util::sync::CancellationToken::new(),
    }
}
