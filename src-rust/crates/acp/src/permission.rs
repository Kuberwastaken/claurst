//! Bridge between Claurst's synchronous permission checks and the
//! asynchronous `session/request_permission` JSON-RPC round-trip used by ACP.
//!
//! Every tool check runs through the session's `PermissionManager` first
//! (via `ManagedInteractivePermissionHandler`), exactly like the TUI: reads
//! inside the workspace, allow rules and bypass mode resolve without asking
//! anyone. Only a real `Ask` decision makes `ToolContext` enqueue the request
//! onto the session's `PendingPermissionStore` and block on a oneshot. A
//! background task — spawned by `prompt::handle` — drains the queue, sends
//! each request to the client as `session/request_permission`, records an
//! "Allow always" answer on the manager, and forwards the decision back
//! through the oneshot to unblock the tool.

use std::sync::{Arc, Mutex};

use agent_client_protocol_schema as acp;
use claurst_core::config::Settings;
use claurst_core::permissions::{PermissionDecision, PermissionManager, PermissionRequest};
use claurst_tools::{PendingPermissionStore, PendingPermissionRequest};
use tracing::{debug, warn};

use crate::connection::Connection;

/// Drain a single pending permission request, route it through the
/// connection as `session/request_permission`, and fire the oneshot with
/// the resulting decision. A cancel of the prompt denies the request
/// without waiting for the client, so the blocked tool unwinds.
pub async fn forward_pending(
    connection: Arc<Connection>,
    session_id: acp::SessionId,
    manager: Arc<Mutex<PermissionManager>>,
    pending: PendingPermissionRequest,
    turn_cancel: tokio_util::sync::CancellationToken,
) {
    let PendingPermissionRequest {
        tool_use_id,
        request,
        reason,
        decision_tx,
    } = pending;

    let Some(decision_tx) = decision_tx else {
        warn!(tool_use_id, "ACP permission: pending request had no decision_tx");
        return;
    };

    let title = if reason.is_empty() {
        format!("Approve {}", request.tool_name)
    } else {
        reason.clone()
    };

    let fields = acp::ToolCallUpdateFields::new()
        .kind(Some(infer_tool_kind(&request)))
        .status(Some(acp::ToolCallStatus::Pending))
        .title(Some(title));
    let tool_call = acp::ToolCallUpdate::new(acp::ToolCallId::new(tool_use_id.as_str()), fields);

    let options = vec![
        acp::PermissionOption::new(
            acp::PermissionOptionId::new("allow_once"),
            "Allow once",
            acp::PermissionOptionKind::AllowOnce,
        ),
        acp::PermissionOption::new(
            acp::PermissionOptionId::new("allow_always"),
            "Allow always",
            acp::PermissionOptionKind::AllowAlways,
        ),
        acp::PermissionOption::new(
            acp::PermissionOptionId::new("reject_once"),
            "Reject",
            acp::PermissionOptionKind::RejectOnce,
        ),
    ];

    let request_params = acp::RequestPermissionRequest::new(session_id, tool_call, options);

    // The drainer does not watch the turn token, so the turn may already be
    // cancelled here; never show the client a request for a dead turn.
    if turn_cancel.is_cancelled() {
        debug!(tool = %request.tool_name, "ACP permission: prompt already cancelled, denying");
        let _ = decision_tx.send(PermissionDecision::Deny);
        return;
    }

    debug!(tool = %request.tool_name, "ACP permission: requesting from client");
    let result = tokio::select! {
        // Checked first so a cancel always wins over a ready send.
        biased;
        _ = turn_cancel.cancelled() => {
            debug!(tool = %request.tool_name, "ACP permission: prompt cancelled, denying");
            let _ = decision_tx.send(PermissionDecision::Deny);
            return;
        }
        result = connection.send_request::<_, acp::RequestPermissionResponse>(
            "session/request_permission",
            request_params,
        ) => result,
    };

    let decision = match result {
        Ok(Ok(response)) => match response.outcome {
            acp::RequestPermissionOutcome::Selected(sel) => match sel.option_id.0.as_ref() {
                "allow_once" => PermissionDecision::Allow,
                "allow_always" => {
                    remember_allow_always(&manager, &request);
                    PermissionDecision::Allow
                }
                _ => PermissionDecision::Deny,
            },
            acp::RequestPermissionOutcome::Cancelled => PermissionDecision::Deny,
            _ => PermissionDecision::Deny,
        },
        Ok(Err(err)) => {
            warn!(?err, "ACP permission: client returned error, denying");
            PermissionDecision::Deny
        }
        Err(err) => {
            warn!(?err, "ACP permission: send_request failed, denying");
            PermissionDecision::Deny
        }
    };

    let _ = decision_tx.send(decision);
}

/// Record an "Allow always" answer the way the TUI does: a persistent allow
/// rule saved to the global settings, scoped to the requested path when there
/// is one. When the settings cannot be saved the rule is kept for the rest of
/// the session instead.
fn remember_allow_always(manager: &Mutex<PermissionManager>, request: &PermissionRequest) {
    let Ok(mut manager) = manager.lock() else {
        warn!(tool = %request.tool_name, "ACP permission: manager lock poisoned, allow-always not remembered");
        return;
    };
    let pattern = request.path.as_deref().map(|path| format!("{path}*"));
    let persisted = Settings::load_sync().and_then(|mut settings| {
        match pattern.as_deref() {
            Some(pattern) => {
                manager.add_persistent_allow_path(&request.tool_name, pattern, &mut settings)
            }
            None => manager.add_persistent_allow(&request.tool_name, &mut settings),
        }
        .map_err(|e| anyhow::anyhow!("{e}"))
    });
    if let Err(e) = persisted {
        warn!(error = %e, tool = %request.tool_name, "ACP permission: could not save allow-always, keeping it for this session");
        match pattern.as_deref() {
            Some(pattern) => manager.add_session_allow_path(&request.tool_name, pattern),
            None => manager.add_session_allow(&request.tool_name),
        }
    }
}

/// Classify a Claurst tool name into an ACP `ToolKind` for client UI hints.
fn infer_tool_kind(request: &PermissionRequest) -> acp::ToolKind {
    if request.is_read_only {
        return acp::ToolKind::Read;
    }
    match request.tool_name.as_str() {
        "Edit" | "FileEdit" | "Write" | "FileWrite" | "BatchEdit" | "ApplyPatch" => acp::ToolKind::Edit,
        "Bash" | "Shell" | "Execute" => acp::ToolKind::Execute,
        "WebFetch" | "WebSearch" => acp::ToolKind::Fetch,
        "Glob" | "Grep" | "GlobTool" => acp::ToolKind::Search,
        "Delete" | "Rm" => acp::ToolKind::Delete,
        "Move" | "Rename" => acp::ToolKind::Move,
        "Think" | "Sequential" => acp::ToolKind::Think,
        _ => acp::ToolKind::Other,
    }
}

/// Spawn a task that watches the shared `PendingPermissionStore` and
/// forwards each enqueued request through the ACP connection. The task
/// exits when `cancel` is fired or the connection drops; `turn_cancel` is
/// the prompt's token, which denies requests still waiting on the client.
pub fn spawn_drainer(
    connection: Arc<Connection>,
    session_id: acp::SessionId,
    manager: Arc<Mutex<PermissionManager>>,
    store: Arc<parking_lot::Mutex<PendingPermissionStore>>,
    cancel: tokio_util::sync::CancellationToken,
    turn_cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(50));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let popped: Vec<PendingPermissionRequest> = {
                let mut guard = store.lock();
                guard.queue.drain(..).collect()
            };
            for pending in popped {
                tokio::spawn(forward_pending(
                    connection.clone(),
                    session_id.clone(),
                    manager.clone(),
                    pending,
                    turn_cancel.clone(),
                ));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(tool_name: &str, is_read_only: bool) -> PermissionRequest {
        PermissionRequest {
            tool_name: tool_name.to_string(),
            description: String::new(),
            details: None,
            is_read_only,
            path: None,
            working_dir: None,
            allowed_roots: Vec::new(),
            context_description: None,
        }
    }

    #[test]
    fn read_only_requests_are_always_classified_as_read() {
        // Even a tool whose name would otherwise map to Execute/Delete/etc.
        // should report Read once the caller marks it read-only.
        assert_eq!(infer_tool_kind(&request("Bash", true)), acp::ToolKind::Read);
        assert_eq!(infer_tool_kind(&request("Rm", true)), acp::ToolKind::Read);
    }

    #[test]
    fn tool_names_map_to_expected_kinds() {
        let cases = [
            ("Edit", acp::ToolKind::Edit),
            ("FileWrite", acp::ToolKind::Edit),
            ("ApplyPatch", acp::ToolKind::Edit),
            ("Bash", acp::ToolKind::Execute),
            ("Shell", acp::ToolKind::Execute),
            ("WebFetch", acp::ToolKind::Fetch),
            ("WebSearch", acp::ToolKind::Fetch),
            ("Glob", acp::ToolKind::Search),
            ("Grep", acp::ToolKind::Search),
            ("Delete", acp::ToolKind::Delete),
            ("Rm", acp::ToolKind::Delete),
            ("Move", acp::ToolKind::Move),
            ("Rename", acp::ToolKind::Move),
            ("Think", acp::ToolKind::Think),
            ("Sequential", acp::ToolKind::Think),
        ];
        for (name, expected) in cases {
            assert_eq!(infer_tool_kind(&request(name, false)), expected, "tool: {name}");
        }
    }

    #[test]
    fn unknown_tool_names_map_to_other() {
        assert_eq!(
            infer_tool_kind(&request("SomeCustomMcpTool", false)),
            acp::ToolKind::Other
        );
    }
}
