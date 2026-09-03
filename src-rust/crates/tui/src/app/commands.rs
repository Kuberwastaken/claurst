//! Slash-command catalog and dispatch.

use claurst_core::config::Theme;
use claurst_core::types::Role;
use crate::notifications::NotificationKind;
use crate::overlays::HelpEntry;
use super::App;
use super::run::open_file_externally;
use super::try_copy_to_clipboard;

pub(super) const PROMPT_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("advisor", "Set or unset the server-side advisor model"),
    ("agent", "List available agents or show agent details"),
    ("agents", "Browse agent definitions and active agents"),
    ("changes", "Inspect changes from the current session"),
    ("clear", "Clear the conversation transcript"),
    ("compact", "Compact the conversation context"),
    ("config", "Open settings"),
    ("connect", "Connect an AI provider"),
    ("context", "Show context window and rate limit usage"),
    ("copy", "Copy the last assistant response to clipboard"),
    ("cost", "Show cost breakdown"),
    ("diff", "Inspect the current git diff"),
    ("doctor", "Run diagnostics"),
    ("effort", "Set effort level (low/medium/high/max)"),
    ("exit", "Quit Claurst"),
    ("export", "Export conversation"),
    ("fast", "Toggle fast mode"),
    ("fork", "Fork session into a new branch"),
    ("goal", "Set or view the current session goal"),
    ("heapdump", "Show process memory and diagnostic information"),
    ("help", "Show help"),
    ("hooks", "Browse configured hooks (read-only)"),
    ("import-config", "Import CLAUDE.md and settings.json from ~/.claude"),
    ("init", "Initialize AGENTS.md for this project"),
    ("insights", "Generate a session analysis report with conversation statistics"),
    ("keybindings", "Show keybinding configuration"),
    ("links", "Open URLs from this session in your browser"),
    ("login", "Log in to Claurst"),
    ("logout", "Log out of Claurst"),
    ("managed-agents", "Configure manager-executor managed agent system"),
    ("mcp", "Browse configured MCP servers"),
    ("memory", "Browse and open AGENTS.md memory files"),
    ("model", "Change the AI model"),
    ("move", "Re-home this session to another worktree of the same project"),
    ("new", "Start a fresh session (keeps model, provider & directory)"),
    ("output-style", "Show or switch the output style / persona"),
    ("plugin", "Manage plugins (list/info/enable/disable/reload)"),
    ("providers", "List available AI providers and their status"),
    ("caveman", "Caveman persona output style — save big token"),
    ("rocky", "Rocky persona output style — amaze amaze amaze"),
    ("normal", "Reset persona / output style to default"),
    ("quit", "Exit Claurst"),
    ("refresh", "Clear saved provider auth and model caches"),
    ("rename", "Rename this session"),
    ("resume", "Resume a previous session"),
    ("review", "Review changes (git diff)"),
    ("rewind", "Rewind to an earlier turn"),
    ("session", "Browse and manage sessions"),
    ("settings", "Open settings"),
    ("share", "Upload the current session as a secret gist and get a shareable URL"),
    ("stats", "Open token and cost stats"),
    ("survey", "Open session feedback survey"),
    ("theme", "Open the theme picker"),
    ("ultrareview", "Run an exhaustive multi-dimensional code review"),
    ("update", "Check for updates and upgrade to the latest version"),
    ("upgrade", "Check for updates and upgrade to the latest version"),
    ("vim", "Toggle vim keybindings"),
    ("voice", "Toggle voice input mode"),
    ("yolo", "Toggle YOLO mode (bypass all permission prompts)"),
];

pub(super) fn help_command_category(name: &str) -> &'static str {
    match name {
        "connect" | "model" | "providers" | "refresh" | "fast" | "effort" | "voice" => "Model & Provider",
        "yolo" => "Permissions",
        "changes" | "diff" | "review" | "rewind" | "export" | "copy" | "share" | "links" => "Review & History",
        "stats" | "cost" | "context" | "insights" | "heapdump" | "doctor" => "Diagnostics",
        "config" | "settings" | "theme" | "keybindings" | "hooks" | "mcp" | "import-config" => {
            "Workspace"
        }
        "agent" | "agents" | "memory" | "plugin" | "survey" => "Tools",
        "session" | "resume" | "rename" | "fork" | "clear" | "new" | "move" | "compact"
        | "quit" | "exit" => "Session",
        _ => "Commands",
    }
}

pub(super) fn help_overlay_entries() -> Vec<HelpEntry> {
    PROMPT_SLASH_COMMANDS
        .iter()
        .map(|(name, description)| HelpEntry {
            name: (*name).to_string(),
            aliases: String::new(),
            description: (*description).to_string(),
            category: help_command_category(name).to_string(),
        })
        .collect()
}

impl App {
    /// Handle slash commands that should open UI screens rather than execute
    /// as normal commands. Returns `true` if the command was intercepted.
    /// Handle `/yolo [on|off]`. Toggles YOLO mode (bypass all permission
    /// prompts). When turning on, shows the BypassPermissionsModeDialog
    /// (security gate) before activating. When turning off, restores
    /// the Default permission mode (not Plan/AcceptEdits).
    pub fn handle_yolo_command(&mut self, args: &str) {
        let turning_on = match args.trim() {
            "" => {
                self.config.permission_mode != claurst_core::PermissionMode::BypassPermissions
            }
            "on" | "true" | "1" => true,
            "off" | "false" | "0" => false,
            _ => {
                self.status_message = Some("Usage: /yolo [on|off]".to_string());
                return;
            }
        };

        if turning_on {
            // Show the security dialog before activating. Mark it as a
            // runtime toggle so key handling knows acceptance should switch
            // the permission mode (and decline cancels instead of exiting).
            // Remember the current mode so `/yolo off` restores it instead
            // of hard-setting `Default`.
            self.yolo_previous_mode = Some(self.config.permission_mode.clone());
            self.bypass_permissions_dialog.show();
            self.yolo_pending = true;
        } else if self.config.permission_mode == claurst_core::PermissionMode::BypassPermissions {
            // Restore the mode that was active before YOLO was enabled —
            // or `Default` if YOLO came from settings/CLI rather than `/yolo`.
            self.config.permission_mode = self
                .yolo_previous_mode
                .take()
                .unwrap_or(claurst_core::PermissionMode::Default);
            self.status_message =
                Some("YOLO mode off. Permission prompts restored.".to_string());
        } else {
            self.status_message = Some("YOLO mode is not active.".to_string());
        }
    }

    pub fn intercept_slash_command_with_args(&mut self, cmd: &str, args: &str) -> bool {
        if cmd == "yolo" {
            self.handle_yolo_command(args.trim());
            return true;
        }
        if cmd == "mcp" && !args.trim().is_empty() {
            return false;
        }
        self.intercept_slash_command(cmd)
    }

    pub fn intercept_slash_command(&mut self, cmd: &str) -> bool {
        self.close_secondary_views();
        self.dismiss_error_notifications();
        match cmd {
            "config" | "settings" => {
                self.settings_screen.open();
                true
            }
            "theme" => {
                let current = match &self.config.theme {
                    Theme::Dark => "dark",
                    Theme::Light => "light",
                    Theme::Default => "default",
                    Theme::Deuteranopia => "deuteranopia",
                    Theme::Custom(s) => s.as_str(),
                };
                self.theme_screen.open(current);
                true
            }
            "stats" => {
                self.stats_dialog.open();
                true
            }
            "mcp" => {
                let servers = self.load_mcp_servers();
                self.mcp_view.open(servers);
                true
            }
            "agents" => {
                self.open_agents_menu();
                true
            }
            "diff" | "review" => {
                let root = self.project_root();
                self.diff_viewer.open(&root);
                true
            }
            "changes" => {
                let root = self.project_root();
                self.refresh_turn_diff_from_history();
                self.diff_viewer.open_turn(&root);
                true
            }
            "search" | "find" => {
                self.global_search.open();
                true
            }
            "survey" => {
                self.feedback_survey.open();
                true
            }
            "memory" => {
                let root = self.project_root();
                self.memory_file_selector.open(&root);
                true
            }
            "hooks" => {
                self.hooks_config_menu.open();
                true
            }
            "import-config" => {
                self.open_import_config_picker();
                true
            }
            "connect" => {
                self.connect_dialog.open();
                true
            }
            "model" => {
                if !self.has_credentials {
                    self.connect_dialog.open();
                    self.status_message = Some("Connect a provider to choose a model.".to_string());
                    return true;
                }
                let provider = self
                    .config
                    .provider
                    .clone()
                    .unwrap_or_else(|| "anthropic".to_string());
                self.open_model_picker_for_provider(&provider, None);
                true
            }
            "session" | "resume" => {
                self.session_browser.open(vec![]);
                self.session_list_pending = true;
                true
            }
            // `/new` (opencode's lazy-home) resets the same visible transcript
            // state as `/clear`; the CLI layer then swaps in a brand-new session
            // and overrides the status line to "Started a new session.".
            "clear" | "new" => {
                self.messages.clear();
                self.system_annotations.clear();
                self.display_messages.clear();
                self.streaming_text.clear();
                self.streaming_thinking.clear();
                self.tool_use_blocks.clear();
                self.turn_metadata.clear();
                self.cost_usd = 0.0;
                self.invalidate_transcript();
                self.status_message = Some("Conversation cleared.".to_string());
                true
            }
            "exit" | "quit" => {
                self.should_exit = true;
                true
            }
            "vim" => {
                self.prompt_input.vim_enabled = !self.prompt_input.vim_enabled;
                let status = if self.prompt_input.vim_enabled { "enabled" } else { "disabled" };
                self.status_message = Some(format!("Vim mode {}.", status));
                self.refresh_prompt_input();
                true
            }
            "fast" => {
                self.fast_mode = !self.fast_mode;
                let status = if self.fast_mode { "enabled" } else { "disabled" };
                self.status_message = Some(format!("Fast mode {}.", status));
                true
            }
            "plan" => {
                use claurst_core::config::PermissionMode;
                self.plan_mode = !self.plan_mode;
                self.config.permission_mode = if self.plan_mode {
                    PermissionMode::Plan
                } else {
                    PermissionMode::Default
                };
                self.status_message = Some(if self.plan_mode {
                    "Plan mode ON — Claurst will plan before acting.".to_string()
                } else {
                    "Plan mode OFF.".to_string()
                });
                // Allow CLI path to also run (sends UserMessage to Claurst).
                false
            }
            "compact" => {
                // Handled by execute_command in the CLI loop (real LLM compaction).
                false
            }
            "copy" => {
                // Copy last assistant message to clipboard. Attempt arboard; fall back to notification.
                let last = self.messages.iter().rev()
                    .find(|m| m.role == Role::Assistant)
                    .map(|m| m.get_all_text());
                if let Some(text) = last {
                    // Try xclip/xsel/pbcopy/clip.exe for clipboard; fall back to notification.
                    let copied = try_copy_to_clipboard(&text);
                    if copied {
                        self.push_notification(
                            NotificationKind::Info,
                            "Copied to clipboard.".to_string(),
                            Some(3),
                        );
                    } else {
                        self.push_notification(
                            NotificationKind::Info,
                            format!("Last response: {} chars (clipboard unavailable)", text.len()),
                            Some(5),
                        );
                    }
                } else {
                    self.push_notification(
                        NotificationKind::Warning,
                        "No assistant message to copy.".to_string(),
                        Some(3),
                    );
                }
                true
            }
            "output-style" => {
                self.output_style = match self.output_style.as_str() {
                    "auto" => "stream".to_string(),
                    "stream" => "verbose".to_string(),
                    _ => "auto".to_string(),
                };
                self.status_message = Some(format!("Output style: {}.", self.output_style));
                true
            }
            "effort" => {
                // Open the horizontal picker so users can pick an effort level
                // visually instead of cycling/typing it (issues #149 / #268). The
                // selectable ladder is model-adaptive: it comes from
                // `supported_efforts` for the current provider + model.
                let provider = self.config.provider.as_deref().unwrap_or("anthropic");
                let model_id = self
                    .model_name
                    .strip_prefix(&format!("{}/", provider))
                    .unwrap_or(&self.model_name);
                let levels = claurst_api::supported_efforts(
                    provider,
                    model_id,
                    Some(&self.model_registry),
                );
                self.effort_picker.open(self.effort_level, levels);
                true
            }
            "voice" => {
                let was_on = self.voice_recorder.is_some();
                if was_on {
                    // Stop any active recording before disabling.
                    if self.voice_recording {
                        self.voice_recording = false;
                        self.voice_event_rx = None;
                        if let Some(ref recorder_arc) = self.voice_recorder {
                            let recorder = recorder_arc.clone();
                            tokio::task::spawn_blocking(move || {
                                if let Ok(mut r) = recorder.lock() {
                                    tokio::runtime::Handle::current()
                                        .block_on(r.stop_recording())
                                        .ok();
                                }
                            });
                        }
                    }
                    self.voice_recorder = None;
                    self.voice_mode_notice.dismiss();
                    self.status_message = Some("Voice mode disabled.".to_string());
                } else {
                    let recorder = claurst_core::voice::global_voice_recorder();
                    if let Ok(mut r) = recorder.lock() {
                        r.set_enabled(true);
                    }
                    self.voice_recorder = Some(recorder);
                    self.voice_mode_notice = crate::voice_mode_notice::VoiceModeNoticeState::new();
                    self.status_message = Some(
                        "Voice mode enabled. Press Alt+V to start recording.".to_string(),
                    );
                }
                true
            }
            "doctor" => {
                // Handled by execute_command (DoctorCommand).
                false
            }
            "cost" => {
                self.stats_dialog.open();
                true
            }
            "rewind" => {
                self.open_rewind_flow();
                true
            }
            "export" => {
                self.export_dialog.open();
                true
            }
            "context" => {
                self.context_viz.toggle();
                true
            }
            "rename" => {
                self.session_browser.open(vec![]);
                self.session_list_pending = true;
                self.session_browser.start_rename();
                true
            }
            "init" | "login" | "logout" => {
                // Handled by execute_command (CLI-level operations).
                false
            }
            "keybindings" => {
                // Open the keybindings.json file in the external editor
                let keybindings_path = claurst_core::config::Settings::config_dir().join("keybindings.json");

                if let Err(e) = open_file_externally(&keybindings_path) {
                    eprintln!("Failed to open keybindings file: {}", e);
                }
                true
            }
            "help" => {
                // Open the help overlay (same as pressing `?` or F1).
                if !self.help_overlay.visible {
                    self.show_help = true;
                    self.help_overlay.toggle();
                }
                true
            }
            _ => false,
        }
    }

}
