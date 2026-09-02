//! Overlay/dialog orchestration and MCP view state.

use std::sync::Arc;

use crate::agents_view::{AgentInfo, AgentStatus};
use crate::export_dialog::ExportFormat;
use crate::mcp_view::{McpServerView, McpToolView, McpViewStatus};
use super::App;

impl App {
    pub(super) fn close_secondary_views(&mut self) {
        self.stats_dialog.close();
        self.mcp_view.close();
        self.agents_menu.close();
        self.diff_viewer.close();
        self.feedback_survey.close();
        self.memory_file_selector.close();
        self.hooks_config_menu.close();
        self.model_picker.close();
        self.session_browser.close();
        self.session_branching.close();
        self.tasks_overlay.close();
        self.export_dialog.dismiss();
        self.context_viz.close();
        self.connect_dialog.close();
        self.import_config_picker.close();
        self.import_config_dialog.close();
        self.command_palette.close();
        self.key_input_dialog.close();
        self.custom_provider_dialog.close();
        self.free_mode_dialog.close();
        self.device_auth_dialog.close();
        self.settings_screen.close();
        self.theme_screen.close();
    }

    pub fn any_modal_open(&self) -> bool {
        self.permission_request.is_some()
            || self.rewind_flow.visible
            || self.tasks_overlay.visible
            || self.help_overlay.visible
            || self.show_help
            || self.history_search_overlay.visible
            || self.history_search.is_some()
            || self.settings_screen.visible
            || self.theme_screen.visible
            || self.stats_dialog.visible
            || self.mcp_view.visible
            || self.agents_menu.visible
            || self.diff_viewer.visible
            || self.paste_viewer.visible
            || self.global_search.visible
            || self.feedback_survey.visible
            || self.memory_file_selector.visible
            || self.hooks_config_menu.visible
            || self.overage_upsell.visible
            || self.voice_mode_notice.visible
            || self.memory_update_notification.visible
            || self.desktop_upsell.visible
            || self.import_config_dialog.visible
            || self.invalid_config_dialog.visible
            || self.bypass_permissions_dialog.visible
            || self.ask_user_dialog.visible
            || self.onboarding_dialog.visible
            || self.import_config_picker.visible
            || self.connect_dialog.visible
            || self.key_input_dialog.visible
            || self.custom_provider_dialog.visible
            || self.free_mode_dialog.visible
            || self.device_auth_dialog.visible
            || self.command_palette.visible
            || self.elicitation.visible
            || self.model_picker.visible
            || self.effort_picker.visible
            || self.session_browser.visible
            || self.session_branching.visible
            || self.export_dialog.visible
            || self.context_viz.visible
            || self.mcp_approval.visible
            || self.file_injection_dialog.visible
            || self.context_menu_state.is_some()
    }

    pub(super) fn dismiss_error_notifications(&mut self) {
        while self.notifications.current_is_error() {
            self.notifications.dismiss_current();
        }
        self.error_modal_scroll_offset = 0;
    }

    /// Perform the export based on the selected format. Returns the path written.
    pub fn perform_export(&mut self) -> Option<String> {
        use crate::export_dialog::{export_as_json, export_as_markdown};
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let (filename, content) = match self.export_dialog.selected {
            ExportFormat::Json => {
                let json = export_as_json(&self.messages, self.session_title.as_deref());
                let s = serde_json::to_string_pretty(&json).unwrap_or_default();
                (format!("claude-export-{}.json", ts), s)
            }
            ExportFormat::Markdown => {
                let md = export_as_markdown(&self.messages, self.session_title.as_deref());
                (format!("claude-export-{}.md", ts), md)
            }
        };
        if std::fs::write(&filename, &content).is_ok() {
            self.export_dialog.dismiss();
            Some(filename)
        } else {
            None
        }
    }

    pub(super) fn project_root(&self) -> std::path::PathBuf {
        self.config
            .project_dir
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }

    pub(super) fn refresh_global_search(&mut self) {
        let root = self.project_root();
        self.global_search.run_search(&root);
    }

    pub(super) fn load_mcp_servers(&self) -> Vec<McpServerView> {
        if let Some(manager) = self.mcp_manager.as_ref() {
            let tool_defs = manager.all_tool_definitions();
            return self
                .config
                .mcp_servers
                .iter()
                .map(|server| {
                    let transport = server
                        .url
                        .as_ref()
                        .map(|_| server.server_type.clone())
                        .or_else(|| server.command.as_ref().map(|_| "stdio".to_string()))
                        .unwrap_or_else(|| server.server_type.clone());

                    let tools: Vec<McpToolView> = tool_defs
                        .iter()
                        .filter(|(server_name, _)| server_name == &server.name)
                        .map(|(_, tool_def)| McpToolView {
                            name: tool_def
                                .name
                                .strip_prefix(&format!("{}_", server.name))
                                .unwrap_or(&tool_def.name)
                                .to_string(),
                            server: server.name.clone(),
                            description: tool_def.description.clone(),
                            input_schema: Some(tool_def.input_schema.to_string()),
                        })
                        .collect();

                    let (status, error_message) = match manager.server_status(&server.name) {
                        claurst_mcp::McpServerStatus::Connected { .. } => {
                            (McpViewStatus::Connected, None)
                        }
                        claurst_mcp::McpServerStatus::Connecting => {
                            (McpViewStatus::Connecting, None)
                        }
                        claurst_mcp::McpServerStatus::Disconnected { last_error } => {
                            if last_error.is_some() {
                                (McpViewStatus::Error, last_error)
                            } else {
                                (McpViewStatus::Disconnected, None)
                            }
                        }
                        claurst_mcp::McpServerStatus::Failed { error, .. } => {
                            (McpViewStatus::Error, Some(error))
                        }
                    };

                    let catalog = manager.server_catalog(&server.name);
                    McpServerView {
                        name: server.name.clone(),
                        transport,
                        status,
                        tool_count: catalog
                            .as_ref()
                            .map(|entry| entry.tool_count)
                            .unwrap_or_else(|| tools.len()),
                        resource_count: catalog
                            .as_ref()
                            .map(|entry| entry.resource_count)
                            .unwrap_or(0),
                        prompt_count: catalog
                            .as_ref()
                            .map(|entry| entry.prompt_count)
                            .unwrap_or(0),
                        resources: catalog
                            .as_ref()
                            .map(|entry| entry.resources.clone())
                            .unwrap_or_default(),
                        prompts: catalog
                            .as_ref()
                            .map(|entry| entry.prompts.clone())
                            .unwrap_or_default(),
                        error_message,
                        tools,
                    }
                })
                .collect();
        }

        self.config
            .mcp_servers
            .iter()
            .map(|server| {
                let transport = server
                    .url
                    .as_ref()
                    .map(|_| server.server_type.clone())
                    .or_else(|| server.command.as_ref().map(|_| "stdio".to_string()))
                    .unwrap_or_else(|| server.server_type.clone());
                let description = if let Some(url) = &server.url {
                    format!("Endpoint: {}", url)
                } else if let Some(command) = &server.command {
                    let args = if server.args.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", server.args.join(" "))
                    };
                    format!("Command: {}{}", command, args)
                } else {
                    "Configured server".to_string()
                };
                McpServerView {
                    name: server.name.clone(),
                    transport,
                    status: McpViewStatus::Disconnected,
                    tool_count: 0,
                    resource_count: 0,
                    prompt_count: 0,
                    resources: vec![],
                    prompts: vec![],
                    error_message: None,
                    tools: vec![McpToolView {
                        name: "connection".to_string(),
                        server: server.name.clone(),
                        description,
                        input_schema: None,
                    }],
                }
            })
            .collect()
    }

    pub(super) fn open_agents_menu(&mut self) {
        let root = self.project_root();
        self.agents_menu.open(&root);
        self.agents_menu.active_agents = self
            .agent_status
            .iter()
            .enumerate()
            .map(|(idx, (name, status))| AgentInfo {
                id: format!("agent-{}", idx + 1),
                name: name.clone(),
                status: match status.as_str() {
                    "running" => AgentStatus::Running,
                    "waiting" | "waiting_for_tool" => AgentStatus::WaitingForTool,
                    "complete" | "completed" | "done" => AgentStatus::Complete,
                    "failed" | "error" => AgentStatus::Failed,
                    _ => AgentStatus::Idle,
                },
                current_tool: None,
                turns_completed: 0,
                is_coordinator: false,
                last_output: Some(status.clone()),
                agent_role: crate::agents_view::AgentRole::Normal,
                model_name: None,
                cost_usd: 0.0,
            })
            .collect();
    }

    pub fn attach_mcp_manager(&mut self, mcp_manager: Arc<claurst_mcp::McpManager>) {
        self.mcp_manager = Some(mcp_manager);
    }

    pub fn refresh_mcp_view(&mut self) {
        let servers = self.load_mcp_servers();
        self.mcp_view.open(servers);
    }

    pub fn take_pending_mcp_panel_auth(&mut self) -> Option<String> {
        self.pending_mcp_panel_auth.take()
    }

    pub fn take_pending_mcp_reconnect(&mut self) -> bool {
        let pending = self.pending_mcp_reconnect;
        self.pending_mcp_reconnect = false;
        pending
    }

    pub fn take_pending_provider_reload(&mut self) -> bool {
        let pending = self.pending_provider_reload;
        self.pending_provider_reload = false;
        pending
    }

    /// If a project MCP server is waiting for approval and no approval dialog
    /// is currently open, pop the next one and show the approval dialog for it.
    ///
    /// Called from the main loop. Returns `true` when a dialog was shown.
    pub fn maybe_prompt_next_mcp_server(&mut self) -> bool {
        if self.mcp_approval.visible || self.mcp_prompting.is_some() {
            return false;
        }
        if let Some(server) = self.mcp_pending_project.pop_front() {
            self.mcp_approval.show(
                &server.name,
                server.url.as_deref(),
                server.command.as_deref(),
                // Tools are unknown until the server is launched; the dialog
                // shows the command/url so the user can judge before running it.
                Vec::new(),
            );
            self.mcp_prompting = Some(server);
            true
        } else {
            false
        }
    }

    /// Apply the user's decision for the project MCP server currently shown in
    /// the approval dialog. Persists "always allow" choices to the on-disk
    /// trust store and requests an MCP reconnect when a server is approved.
    pub fn handle_mcp_approval_decision(&mut self, choice: crate::dialogs::McpApprovalChoice) {
        use crate::dialogs::McpApprovalChoice;
        let server = match self.mcp_prompting.take() {
            Some(s) => s,
            None => return,
        };
        match choice {
            McpApprovalChoice::AllowSession => {
                self.mcp_session_trusted
                    .insert(claurst_core::mcp_trust::server_fingerprint(&server));
                self.pending_mcp_reconnect = true;
                self.status_message = Some(format!(
                    "Approved MCP server '{}' for this session.",
                    server.name
                ));
            }
            McpApprovalChoice::AllowAlways => {
                self.mcp_session_trusted
                    .insert(claurst_core::mcp_trust::server_fingerprint(&server));
                if let Some(root) = self.mcp_project_root.clone() {
                    let mut store = claurst_core::mcp_trust::McpTrustStore::load();
                    store.approve(&root, &server);
                    if let Err(e) = store.save() {
                        self.status_message = Some(format!(
                            "Approved '{}', but failed to persist trust: {}",
                            server.name, e
                        ));
                    } else {
                        self.status_message = Some(format!(
                            "Always allowing MCP server '{}' for this project.",
                            server.name
                        ));
                    }
                } else {
                    self.status_message = Some(format!(
                        "Approved MCP server '{}' (no project root to persist to).",
                        server.name
                    ));
                }
                self.pending_mcp_reconnect = true;
            }
            McpApprovalChoice::Deny => {
                self.status_message = Some(format!(
                    "Skipped project MCP server '{}'.",
                    server.name
                ));
            }
        }
    }

    /// Persist `has_completed_onboarding = true` to the settings file.
    /// Best-effort: failures are silently ignored to not disrupt the session.
    pub(super) fn persist_onboarding_complete() -> anyhow::Result<()> {
        let mut settings = claurst_core::config::Settings::load_sync()?;
        settings.has_completed_onboarding = true;
        settings.save_sync()
    }

    /// Public wrapper so the main loop can mark onboarding complete without
    /// going through the dialog flow.
    pub fn persist_onboarding_complete_pub() -> anyhow::Result<()> {
        Self::persist_onboarding_complete()
    }

    /// Persist `skip_dangerous_mode_permission_prompt = true` to the settings
    /// file after the user accepts the Bypass Permissions warning, so the
    /// dialog is a one-time gate rather than shown on every launch.
    /// Best-effort: failures are silently ignored to not disrupt the session.
    pub(super) fn persist_bypass_permissions_accepted() -> anyhow::Result<()> {
        let mut settings = claurst_core::config::Settings::load_sync()?;
        settings.skip_dangerous_mode_permission_prompt = true;
        settings.save_sync()
    }

}
