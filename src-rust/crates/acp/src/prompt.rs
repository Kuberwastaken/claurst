//! `session/prompt` handler — drives the Claurst query loop and forwards
//! every meaningful event back to the ACP client as a `session/update`
//! notification.

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol_schema as acp;
use claurst_api::streaming::{AnthropicStreamEvent, ContentDelta};
use claurst_core::permissions::ManagedInteractivePermissionHandler;
use claurst_core::types::{ContentBlock, ImageSource, Message};
use claurst_query::{QueryEvent, QueryOutcome};
use claurst_tools::ToolContext;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::connection::Connection;
use crate::sessions::SessionState;

/// Handle one `session/prompt` JSON-RPC call.
///
/// Drives the full Claurst query loop with the session runtime's tools, MCP
/// servers, and provider registry, while streaming every text delta,
/// thinking delta, and tool invocation back as `session/update`
/// notifications. Returns the final `PromptResponse` with the appropriate
/// `StopReason`, or a JSON-RPC error when the turn fails.
pub async fn handle(
    connection: Arc<Connection>,
    session: Arc<SessionState>,
    params: acp::PromptRequest,
) -> Result<acp::PromptResponse, acp::Error> {
    let runtime = &session.runtime;
    // Convert prompt content blocks → a single user message in Claurst's
    // internal format.
    let Some(message) = user_message(params.prompt, runtime.supports_images) else {
        return Err(acp::Error::invalid_params()
            .data(Some(serde_json::json!({ "reason": "prompt has no text or image content" }))));
    };

    // One prompt at a time per session, each with its own cancel token.
    let Some(turn) = session.begin_turn() else {
        return Err(acp::Error::new(
            i32::from(acp::ErrorCode::InvalidRequest),
            format!("Session {} already has a prompt running", session.session_id),
        ));
    };
    let cancel = turn.token.clone();

    // Append the user turn to the session transcript.
    let mut messages: Vec<Message> = {
        let guard = session.messages.lock();
        guard.clone()
    };
    messages.push(message);

    // Build per-session ToolContext.
    // The session's PermissionManager decides first; only its Ask decisions
    // reach the client through the pending-permission queue.
    let permission_handler: Arc<dyn claurst_core::PermissionHandler> = Arc::new(
        ManagedInteractivePermissionHandler::new(session.permission_manager.clone()),
    );
    let tool_ctx = ToolContext {
        working_dir: session.cwd.clone(),
        permission_mode: runtime.config.permission_mode.clone(),
        permission_handler,
        cost_tracker: runtime.cost_tracker.clone(),
        session_id: session.session_id.0.to_string(),
        file_history: session.file_history.clone(),
        current_turn: session.current_turn.clone(),
        non_interactive: false, // ACP routes permissions via the bridge
        mcp_manager: runtime.mcp_manager.clone(),
        config: runtime.config.clone(),
        managed_agent_config: runtime.config.managed_agents.clone(),
        completion_notifier: None,
        pending_permissions: Some(session.pending_permissions.clone()),
        permission_manager: Some(session.permission_manager.clone()),
        user_question_tx: None,
        // Bind to this turn's cancel token so the parallel tool executor and any
        // sub-agents observe cancellation (issue #218). `run_query_loop` also
        // rebinds this to the token it is driven by.
        cancel_token: cancel.clone(),
    };

    // Spawn the permission drainer for this turn.
    let drainer_cancel = CancellationToken::new();
    let drainer = crate::permission::spawn_drainer(
        connection.clone(),
        session.session_id.clone(),
        session.permission_manager.clone(),
        session.pending_permissions.clone(),
        drainer_cancel.clone(),
        cancel.clone(),
    );

    // Event channel + forwarder.
    let (ev_tx, ev_rx) = mpsc::unbounded_channel::<QueryEvent>();
    let forwarder = tokio::spawn(forward_events(
        connection.clone(),
        session.session_id.clone(),
        ev_rx,
    ));

    // Run the query loop.
    let outcome = claurst_query::run_query_loop(
        runtime.api_client.as_ref(),
        &mut messages,
        runtime.tools.as_slice(),
        &tool_ctx,
        &runtime.query_config,
        runtime.cost_tracker.clone(),
        Some(ev_tx),
        cancel,
        None,
    )
    .await;

    // Tear down forwarder + drainer.
    drainer_cancel.cancel();
    let _ = drainer.await;
    // Forwarder finishes when ev_tx is dropped at end of run_query_loop.
    let _ = forwarder.await;

    // Persist the updated transcript.
    {
        let mut guard = session.messages.lock();
        *guard = messages;
    }

    let stop_reason = match outcome {
        QueryOutcome::EndTurn { .. } => acp::StopReason::EndTurn,
        QueryOutcome::MaxTokens { .. } => acp::StopReason::MaxTokens,
        QueryOutcome::Cancelled => acp::StopReason::Cancelled,
        QueryOutcome::BudgetExceeded { .. } => acp::StopReason::MaxTurnRequests,
        QueryOutcome::Error(e) => {
            // A failed turn is a JSON-RPC error, not a model refusal.
            error!(error = ?e, "ACP: query loop errored");
            return Err(acp::Error::new(
                i32::from(acp::ErrorCode::InternalError),
                e.to_string(),
            ));
        }
    };

    Ok(acp::PromptResponse::new(stop_reason))
}

/// Convert prompt content blocks into one user message. Text, resource links
/// and embedded text resources form the text part; images are passed through
/// when the provider accepts them and replaced by a short note otherwise.
/// Returns `None` when the prompt has nothing to send.
fn user_message(blocks: Vec<acp::ContentBlock>, supports_images: bool) -> Option<Message> {
    let mut parts: Vec<String> = Vec::new();
    let mut images: Vec<ContentBlock> = Vec::new();
    for block in blocks {
        match block {
            acp::ContentBlock::Text(t) => parts.push(t.text),
            acp::ContentBlock::ResourceLink(link) => {
                parts.push(format!("[resource link: {}]", link.uri));
            }
            acp::ContentBlock::Resource(res) => {
                // Best-effort: emit any embedded text.
                let json = serde_json::to_value(&res).unwrap_or_default();
                if let Some(text) = json
                    .get("resource")
                    .and_then(|r| r.get("text"))
                    .and_then(|t| t.as_str())
                {
                    parts.push(text.to_string());
                }
            }
            acp::ContentBlock::Image(image) => {
                if !supports_images {
                    warn!("ACP: dropping image, the active provider does not accept image input");
                    parts.push(
                        "[image omitted: the active provider does not accept image input]"
                            .to_string(),
                    );
                } else if image.data.is_empty() {
                    parts.push(format!(
                        "[image: {}]",
                        image.uri.as_deref().unwrap_or("no data")
                    ));
                } else {
                    images.push(ContentBlock::Image {
                        source: ImageSource {
                            source_type: "base64".to_string(),
                            media_type: Some(image.mime_type),
                            data: Some(image.data),
                            url: None,
                        },
                    });
                }
            }
            acp::ContentBlock::Audio(_) => {
                warn!("ACP: ignoring audio content block (capability not advertised)");
            }
            _ => {
                warn!("ACP: ignoring unknown content block variant");
            }
        }
    }
    let text = parts.join("\n\n");
    if images.is_empty() {
        if text.trim().is_empty() {
            return None;
        }
        return Some(Message::user(text));
    }
    if !text.trim().is_empty() {
        images.push(ContentBlock::Text { text });
    }
    Some(Message::user_blocks(images))
}

/// Pump QueryEvents → `session/update` SessionNotifications.
async fn forward_events(
    connection: Arc<Connection>,
    session_id: acp::SessionId,
    mut rx: mpsc::UnboundedReceiver<QueryEvent>,
) {
    // Track tool calls so ToolEnd updates carry the right title and kind.
    let mut active_tools: HashMap<String, ToolMeta> = HashMap::new();

    while let Some(event) = rx.recv().await {
        match event {
            QueryEvent::Stream(AnthropicStreamEvent::ContentBlockDelta { delta, .. }) => {
                match delta {
                    ContentDelta::TextDelta { text } => {
                        send_text_chunk(&connection, &session_id, &text, false).await;
                    }
                    ContentDelta::ThinkingDelta { thinking } => {
                        send_text_chunk(&connection, &session_id, &thinking, true).await;
                    }
                    _ => {}
                }
            }
            QueryEvent::ToolStart {
                tool_name,
                tool_id,
                input_json,
            } => {
                let kind = classify_tool_kind(&tool_name);
                let raw_input = serde_json::from_str::<serde_json::Value>(&input_json).ok();
                let title = tool_title(&tool_name, raw_input.as_ref());
                active_tools.insert(
                    tool_id.clone(),
                    ToolMeta {
                        title: title.clone(),
                        kind,
                    },
                );
                let mut tool_call = acp::ToolCall::new(
                    acp::ToolCallId::new(tool_id.as_str()),
                    title,
                )
                .kind(kind)
                .status(acp::ToolCallStatus::InProgress);
                if let Some(input) = raw_input {
                    tool_call = tool_call.raw_input(Some(input));
                }
                send_session_update(
                    &connection,
                    &session_id,
                    acp::SessionUpdate::ToolCall(tool_call),
                )
                .await;
            }
            QueryEvent::ToolEnd {
                tool_name: _,
                tool_id,
                result,
                is_error,
            } => {
                let status = if is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                };
                let content = vec![acp::ToolCallContent::Content(acp::Content::new(
                    acp::ContentBlock::Text(acp::TextContent::new(result.clone())),
                ))];
                let raw_output =
                    serde_json::from_str::<serde_json::Value>(&result).ok().or_else(|| {
                        Some(serde_json::Value::String(result.clone()))
                    });
                let mut fields = acp::ToolCallUpdateFields::new()
                    .status(status)
                    .content(content);
                if let Some(out) = raw_output {
                    fields = fields.raw_output(Some(out));
                }
                let update = acp::ToolCallUpdate::new(
                    acp::ToolCallId::new(tool_id.as_str()),
                    fields,
                );
                send_session_update(
                    &connection,
                    &session_id,
                    acp::SessionUpdate::ToolCallUpdate(update),
                )
                .await;
                active_tools.remove(&tool_id);
            }
            QueryEvent::Error(msg) => {
                send_text_chunk(&connection, &session_id, &format!("\n[error: {}]", msg), false)
                    .await;
            }
            _ => {}
        }
    }
}

struct ToolMeta {
    #[allow(dead_code)]
    title: String,
    #[allow(dead_code)]
    kind: acp::ToolKind,
}

async fn send_text_chunk(
    connection: &Arc<Connection>,
    session_id: &acp::SessionId,
    text: &str,
    is_thought: bool,
) {
    let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text)));
    let update = if is_thought {
        acp::SessionUpdate::AgentThoughtChunk(chunk)
    } else {
        acp::SessionUpdate::AgentMessageChunk(chunk)
    };
    send_session_update(connection, session_id, update).await;
}

async fn send_session_update(
    connection: &Arc<Connection>,
    session_id: &acp::SessionId,
    update: acp::SessionUpdate,
) {
    let notif = acp::SessionNotification::new(session_id.clone(), update);
    if let Err(e) = connection.send_notification("session/update", notif).await {
        warn!(?e, "ACP: failed to send session/update");
    } else {
        debug!("ACP: sent session/update");
    }
}

fn classify_tool_kind(tool_name: &str) -> acp::ToolKind {
    match tool_name {
        "Read" | "FileRead" => acp::ToolKind::Read,
        "Edit" | "FileEdit" | "Write" | "FileWrite" | "BatchEdit" | "ApplyPatch" => {
            acp::ToolKind::Edit
        }
        "Bash" | "Shell" | "Execute" => acp::ToolKind::Execute,
        "WebFetch" | "WebSearch" => acp::ToolKind::Fetch,
        "Glob" | "Grep" | "GlobTool" => acp::ToolKind::Search,
        "Delete" | "Rm" => acp::ToolKind::Delete,
        "Move" | "Rename" => acp::ToolKind::Move,
        "Think" | "Sequential" => acp::ToolKind::Think,
        _ => acp::ToolKind::Other,
    }
}

/// Compose a short, human-readable title for a tool call. Falls back to the
/// tool's bare name if no descriptive field is present.
fn tool_title(tool_name: &str, raw_input: Option<&serde_json::Value>) -> String {
    if let Some(input) = raw_input {
        // Prefer path-like fields for file tools.
        for key in &["file_path", "path", "filename", "url", "pattern", "command"] {
            if let Some(v) = input.get(*key).and_then(|x| x.as_str()) {
                return format!("{}: {}", tool_name, v);
            }
        }
    }
    tool_name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(blocks: Vec<acp::ContentBlock>) -> String {
        user_message(blocks, false)
            .expect("prompt has content")
            .get_all_text()
    }

    #[test]
    fn user_message_joins_text_blocks_with_blank_line() {
        let blocks = vec![
            acp::ContentBlock::Text(acp::TextContent::new("first")),
            acp::ContentBlock::Text(acp::TextContent::new("second")),
        ];
        assert_eq!(text_of(blocks), "first\n\nsecond");
    }

    #[test]
    fn user_message_renders_resource_links() {
        let blocks = vec![acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
            "notes.md",
            "file:///tmp/notes.md",
        ))];
        assert_eq!(text_of(blocks), "[resource link: file:///tmp/notes.md]");
    }

    #[test]
    fn user_message_passes_images_through_when_supported() {
        let blocks = vec![
            acp::ContentBlock::Text(acp::TextContent::new("caption")),
            acp::ContentBlock::Image(acp::ImageContent::new("base64data", "image/png")),
        ];
        let message = user_message(blocks, true).expect("prompt has content");
        let content = message.content_blocks();
        assert!(matches!(
            &content[0],
            ContentBlock::Image { source }
                if source.source_type == "base64"
                    && source.media_type.as_deref() == Some("image/png")
                    && source.data.as_deref() == Some("base64data")
        ));
        assert!(matches!(&content[1], ContentBlock::Text { text } if text == "caption"));
    }

    #[test]
    fn user_message_notes_images_the_provider_cannot_take() {
        let blocks = vec![
            acp::ContentBlock::Text(acp::TextContent::new("caption")),
            acp::ContentBlock::Image(acp::ImageContent::new("base64data", "image/png")),
        ];
        let text = text_of(blocks);
        assert!(text.starts_with("caption\n\n[image omitted"), "{text}");
    }

    #[test]
    fn user_message_empty_input_is_none() {
        assert!(user_message(Vec::new(), true).is_none());
    }

    #[test]
    fn classify_tool_kind_maps_known_names() {
        assert_eq!(classify_tool_kind("Read"), acp::ToolKind::Read);
        assert_eq!(classify_tool_kind("Edit"), acp::ToolKind::Edit);
        assert_eq!(classify_tool_kind("Bash"), acp::ToolKind::Execute);
        assert_eq!(classify_tool_kind("WebFetch"), acp::ToolKind::Fetch);
        assert_eq!(classify_tool_kind("Grep"), acp::ToolKind::Search);
        assert_eq!(classify_tool_kind("Rm"), acp::ToolKind::Delete);
        assert_eq!(classify_tool_kind("Rename"), acp::ToolKind::Move);
        assert_eq!(classify_tool_kind("Sequential"), acp::ToolKind::Think);
    }

    #[test]
    fn classify_tool_kind_unknown_name_is_other() {
        assert_eq!(classify_tool_kind("SomeMcpTool"), acp::ToolKind::Other);
    }

    #[test]
    fn tool_title_falls_back_to_bare_name_without_input() {
        assert_eq!(tool_title("Bash", None), "Bash");
    }

    #[test]
    fn tool_title_prefers_path_like_fields_in_priority_order() {
        let input = serde_json::json!({ "path": "/a", "file_path": "/b" });
        // file_path is checked before path in the priority list.
        assert_eq!(tool_title("Edit", Some(&input)), "Edit: /b");
    }

    #[test]
    fn tool_title_uses_command_field_for_bash() {
        let input = serde_json::json!({ "command": "ls -la" });
        assert_eq!(tool_title("Bash", Some(&input)), "Bash: ls -la");
    }

    #[test]
    fn tool_title_falls_back_when_input_has_no_known_field() {
        let input = serde_json::json!({ "unrelated": 42 });
        assert_eq!(tool_title("CustomTool", Some(&input)), "CustomTool");
    }
}
