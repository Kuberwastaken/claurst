//! Bridge between Claurst's synchronous `PermissionHandler` trait and the
//! asynchronous `session/request_permission` JSON-RPC round-trip used by ACP.
//!
//! The handler itself simply returns `Ask { reason }` for every permission
//! check. That causes `ToolContext::request_permission_inner` to enqueue the
//! request onto a shared `PendingPermissionStore` and block on a oneshot.
//! A background task — spawned by `prompt::handle_prompt` — drains the queue,
//! converts each pending request into a `session/request_permission` call to
//! the connected client, and forwards the client's decision back through the
//! oneshot to unblock the tool.

use std::sync::Arc;

use agent_client_protocol_schema as acp;
use claurst_core::permissions::{PermissionDecision, PermissionRequest};
use claurst_core::PermissionHandler;
use claurst_tools::{PendingPermissionStore, PendingPermissionRequest};
use tracing::{debug, warn};

use crate::connection::Connection;

/// Permission handler that defers every decision to the ACP client.
pub struct AcpPermissionHandler;

impl PermissionHandler for AcpPermissionHandler {
    fn check_permission(&self, _request: &PermissionRequest) -> PermissionDecision {
        // Defer everything to interactive resolution.
        PermissionDecision::Ask {
            reason: String::new(),
        }
    }

    fn request_permission(&self, request: &PermissionRequest) -> PermissionDecision {
        let mut reason = format!("Tool '{}' requires approval", request.tool_name);
        if let Some(detail) = &request.details {
            reason.push_str(": ");
            reason.push_str(detail);
        }
        PermissionDecision::Ask { reason }
    }
}

/// Drain a single pending permission request, route it through the
/// connection as `session/request_permission`, and fire the oneshot with
/// the resulting decision.
pub async fn forward_pending(
    connection: Arc<Connection>,
    session_id: acp::SessionId,
    pending: PendingPermissionRequest,
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

    debug!(tool = %request.tool_name, "ACP permission: requesting from client");
    let result = connection
        .send_request::<_, acp::RequestPermissionResponse>(
            "session/request_permission",
            request_params,
        )
        .await;

    let decision = match result {
        Ok(Ok(response)) => match response.outcome {
            acp::RequestPermissionOutcome::Selected(sel) => match sel.option_id.0.as_ref() {
                "allow_once" => PermissionDecision::Allow,
                "allow_always" => PermissionDecision::AllowPermanently,
                "reject_always" => PermissionDecision::DenyPermanently,
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
/// exits when `cancel` is fired or the connection drops.
pub fn spawn_drainer(
    connection: Arc<Connection>,
    session_id: acp::SessionId,
    store: Arc<parking_lot::Mutex<PendingPermissionStore>>,
    cancel: tokio_util::sync::CancellationToken,
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
                let conn = connection.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    forward_pending(conn, sid, pending).await;
                });
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

    #[test]
    fn handler_check_permission_always_asks_with_empty_reason() {
        let handler = AcpPermissionHandler;
        let decision = handler.check_permission(&request("Bash", false));
        assert!(matches!(decision, PermissionDecision::Ask { reason } if reason.is_empty()));
    }

    #[test]
    fn handler_request_permission_includes_tool_name_and_detail() {
        let handler = AcpPermissionHandler;
        let mut req = request("Bash", false);
        req.details = Some("rm -rf /tmp/x".to_string());

        let decision = handler.request_permission(&req);
        match decision {
            PermissionDecision::Ask { reason } => {
                assert!(reason.contains("Bash"));
                assert!(reason.contains("rm -rf /tmp/x"));
            }
            other => panic!("expected Ask decision, got {other:?}"),
        }
    }
}
