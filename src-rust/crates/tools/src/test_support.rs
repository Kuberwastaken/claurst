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
    context_with_handler(working_dir, Arc::new(AllowAllHandler))
}

/// Permission handler that runs the real `PermissionManager` (workspace fence
/// included, `Ask` turned into `Deny` as in headless mode) and, right after
/// its first decision, replaces `link` with a symlink to `target`: the swap
/// from #415, landed in the window between the permission check and the open.
#[cfg(unix)]
pub(crate) struct SwapAfterCheckHandler {
    inner: claurst_core::permissions::ManagedAutoPermissionHandler,
    swap: parking_lot::Mutex<Option<(PathBuf, PathBuf)>>,
}

#[cfg(unix)]
impl SwapAfterCheckHandler {
    /// `swap` is `(link, target)`; `None` runs the real fence with no swap.
    pub(crate) fn new(
        manager: claurst_core::permissions::PermissionManager,
        swap: Option<(PathBuf, PathBuf)>,
    ) -> Self {
        Self {
            inner: claurst_core::permissions::ManagedAutoPermissionHandler::new(Arc::new(
                std::sync::Mutex::new(manager),
            )),
            swap: parking_lot::Mutex::new(swap),
        }
    }
}

#[cfg(unix)]
impl claurst_core::permissions::PermissionHandler for SwapAfterCheckHandler {
    fn check_permission(
        &self,
        request: &claurst_core::permissions::PermissionRequest,
    ) -> claurst_core::permissions::PermissionDecision {
        claurst_core::permissions::PermissionHandler::request_permission(self, request)
    }

    fn request_permission(
        &self,
        request: &claurst_core::permissions::PermissionRequest,
    ) -> claurst_core::permissions::PermissionDecision {
        let decision =
            claurst_core::permissions::PermissionHandler::request_permission(&self.inner, request);
        if let Some((link, target)) = self.swap.lock().take() {
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
        }
        decision
    }
}

/// Build a non-interactive `ToolContext` rooted at `working_dir` that asks
/// `handler` for every permission decision.
pub(crate) fn context_with_handler(
    working_dir: PathBuf,
    handler: Arc<dyn claurst_core::permissions::PermissionHandler>,
) -> ToolContext {
    ToolContext {
        working_dir,
        permission_mode: claurst_core::config::PermissionMode::Default,
        permission_handler: handler,
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
