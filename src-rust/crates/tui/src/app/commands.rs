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
];

pub(super) fn help_command_category(name: &str) -> &'static str {
    match name {
        "connect" | "model" | "providers" | "refresh" | "fast" | "effort" | "voice" => "Model & Provider",
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
    pub fn intercept_slash_command_with_args(&mut self, cmd: &str, args: &str) -> bool {
        if cmd == "mcp" && !args.trim().is_empty() {
            return false;
        }
        self.intercept_slash_command(cmd)
    }

    /// Execute a `!` bang command in a spawned subprocess with a timeout
    /// and output cap. The command runs through the system shell (sh on
    /// Unix, cmd on Windows) — not hardcoded to bash — and the result
    /// is displayed in the transcript. The spawned task sends the
    /// (command, output) pair back over a channel so the TUI never blocks.
    pub fn execute_bang_command(&mut self, command: String) {
        // Block in plan mode.
        if self.config.permission_mode == claurst_core::PermissionMode::Plan {
            self.push_notification(
                crate::notifications::NotificationKind::Warning,
                "Bang commands disabled in plan mode.".to_string(),
                None,
            );
            return;
        }

        let working_dir = self.project_root();

        let (tx, rx) = tokio::sync::mpsc::channel(1);
        self.bang_command_pending = Some(rx);

        tokio::spawn(async move {
            // Run the command in a blocking task with a timeout.
            let cmd_str = command.clone();
            let working_dir = working_dir.clone();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                tokio::task::spawn_blocking(move || {
                    run_shell_command(&cmd_str, &working_dir)
                }),
            )
            .await;

            let output = match result {
                Ok(Ok(Ok(output))) => output,
                Ok(Ok(Err(e))) => format!("Error: {e}"),
                Ok(Err(e)) => format!("Task panicked: {e}"),
                Err(_) => "Command timed out after 60 seconds.".to_string(),
            };

            // Cap output to 100KB to prevent OOM in the TUI.
            const MAX_OUTPUT: usize = 100_000;
            let output = if output.len() > MAX_OUTPUT {
                format!("{}\n... (output truncated at {} bytes)", &output[..MAX_OUTPUT], MAX_OUTPUT)
            } else {
                output
            };

            let _ = tx.send((command, output)).await;
        });

        // Result will be displayed when the spawned task completes.
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


/// Run a shell command with a 60-second timeout. Uses `sh` on Unix and
/// `cmd` on Windows — never hardcoded to `bash`. Returns the combined
/// stdout+stderr output.
fn run_shell_command(command: &str, working_dir: &std::path::Path) -> Result<String, String> {
    #[cfg(unix)]
    {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(working_dir)
            .output()
            .map_err(|e| format!("Failed to spawn: {e}"))?;
        let mut combined = String::new();
        if !output.stdout.is_empty() {
            combined.push_str(&String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        if !output.status.success() {
            combined = format!("Exit code: {}\n{}", output.status.code().unwrap_or(-1), combined);
        }
        Ok(combined)
    }

    #[cfg(not(unix))]
    {
        let output = std::process::Command::new("cmd")
            .arg("/C")
            .arg(command)
            .current_dir(working_dir)
            .output()
            .map_err(|e| format!("Failed to spawn: {e}"))?;
        let mut combined = String::new();
        if !output.stdout.is_empty() {
            combined.push_str(&String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        if !output.status.success() {
            combined = format!("Exit code: {}\n{}", output.status.code().unwrap_or(-1), combined);
        }
        Ok(combined)
    }
}
