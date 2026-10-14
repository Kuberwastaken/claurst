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
/// included, `Ask` rejected by the non-interactive context as in headless
/// mode) and, right after
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
        batch_rejection: Default::default(),
    }
}

/// Build a headless `ToolContext` rooted at `working_dir` that runs the real
/// `PermissionManager` in `mode` (an `Ask` decision becomes a refusal).
pub(crate) fn managed_context(
    working_dir: PathBuf,
    mode: claurst_core::config::PermissionMode,
) -> ToolContext {
    let manager = Arc::new(std::sync::Mutex::new(
        claurst_core::permissions::PermissionManager::new(
            mode.clone(),
            &claurst_core::config::Settings::default(),
        ),
    ));
    ToolContext {
        permission_mode: mode,
        permission_manager: Some(manager.clone()),
        ..context_with_handler(
            working_dir,
            Arc::new(claurst_core::permissions::ManagedAutoPermissionHandler::new(manager)),
        )
    }
}

/// #445: AcceptEdits lets every write tool change files inside the
/// workspace without asking, and still refuses (headless) outside it.
mod accept_edits_tests {
    use super::managed_context;
    use crate::Tool;
    use claurst_core::config::PermissionMode;
    use serde_json::json;

    fn notebook() -> String {
        json!({
            "cells": [{
                "cell_type": "code", "id": "c1", "metadata": {},
                "source": ["print('hi')\n"], "outputs": [], "execution_count": null
            }],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        })
        .to_string()
    }

    /// Run every write tool against files under `dir` and report which
    /// succeeded, in the order Write, Edit, NotebookEdit, BatchEdit,
    /// ApplyPatch.
    async fn run_write_tools(ctx: &crate::ToolContext, dir: &std::path::Path) -> Vec<bool> {
        std::fs::write(dir.join("edit.txt"), "old\n").unwrap();
        std::fs::write(dir.join("batch.txt"), "old\n").unwrap();
        std::fs::write(dir.join("patch.txt"), "old\n").unwrap();
        std::fs::write(dir.join("nb.ipynb"), notebook()).unwrap();
        let path = |name: &str| dir.join(name).display().to_string();
        let patch = format!("--- {p}\n+++ {p}\n@@ -1 +1 @@\n-old\n+new\n", p = path("patch.txt"));
        let results = [
            crate::file_write::FileWriteTool
                .execute(json!({ "file_path": path("new.txt"), "content": "x" }), ctx)
                .await,
            crate::file_edit::FileEditTool
                .execute(
                    json!({ "file_path": path("edit.txt"), "old_string": "old", "new_string": "new" }),
                    ctx,
                )
                .await,
            crate::notebook_edit::NotebookEditTool
                .execute(
                    json!({
                        "notebook_path": path("nb.ipynb"), "cell_id": "cell-0",
                        "new_source": "print('bye')", "edit_mode": "replace"
                    }),
                    ctx,
                )
                .await,
            crate::batch_edit::BatchEditTool
                .execute(
                    json!({ "edits": [{ "file_path": path("batch.txt"), "old_string": "old", "new_string": "new" }] }),
                    ctx,
                )
                .await,
            crate::apply_patch::ApplyPatchTool.execute(json!({ "patch": patch }), ctx).await,
        ];
        results.iter().map(|r| !r.is_error).collect()
    }

    #[tokio::test]
    async fn accept_edits_allows_workspace_writes_for_every_write_tool() {
        let ws = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(ws.path()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_root = std::fs::canonicalize(outside.path()).unwrap();

        let accept = managed_context(root.clone(), PermissionMode::AcceptEdits);
        assert_eq!(run_write_tools(&accept, &root).await, vec![true; 5]);
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "x");
        assert_eq!(std::fs::read_to_string(root.join("patch.txt")).unwrap(), "new\n");

        // Outside the workspace AcceptEdits still asks, which headless refuses.
        assert_eq!(run_write_tools(&accept, &outside_root).await, vec![false; 5]);
        assert!(!outside_root.join("new.txt").exists());
        assert_eq!(std::fs::read_to_string(outside_root.join("patch.txt")).unwrap(), "old\n");

        // Default mode asks (refused headless) even inside the workspace.
        let default = managed_context(root.clone(), PermissionMode::Default);
        let fresh = root.join("fresh");
        std::fs::create_dir(&fresh).unwrap();
        assert_eq!(run_write_tools(&default, &fresh).await, vec![false; 5]);
    }
}
