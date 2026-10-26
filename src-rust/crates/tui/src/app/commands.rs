//! Slash-command catalog and dispatch.

use claurst_core::types::Role;
use crate::notifications::NotificationKind;
use crate::overlays::HelpEntry;
use super::App;
use super::try_copy_to_clipboard;

pub(super) const PROMPT_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("advisor", "Set or unset the server-side advisor model"),
    ("agent", "List available agents or show agent details"),
    ("agents", "Browse agent definitions and active agents"),
    ("cc-statusline", "Show the external status line configuration"),
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
    ("turns", "Set the max turn limit for this session, e.g. /turns 25, or /turns off for a 1000-turn ceiling"),
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
        "config" | "settings" | "theme" | "keybindings" | "hooks" | "mcp" | "import-config"
        | "cc-statusline" => {
            "Workspace"
        }
        "agent" | "agents" | "memory" | "plugin" | "survey" | "turns" => "Tools",
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

/// Turn cap applied by `/turns off`. Deliberately finite so a runaway loop
/// still terminates (and still gets its degradation summary turn).
const TURNS_OFF_CEILING: u32 = 1000;

impl App {
    /// Handle `/yolo [on|off]`. Toggles YOLO mode (bypass all permission
    /// prompts). When turning on, shows the BypassPermissionsModeDialog
    /// (security gate) before activating. When turning off, restores the
    /// permission mode that was active before `/yolo on` (or `Default` if
    /// YOLO came from settings or the CLI).
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
            if self.config.permission_mode == claurst_core::PermissionMode::BypassPermissions {
                self.status_message = Some("YOLO mode is already on.".to_string());
                return;
            }
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

    /// Handle slash commands that should open UI screens rather than execute
    /// as normal commands. Returns `true` if the command was intercepted.
    pub fn intercept_slash_command_with_args(&mut self, cmd: &str, args: &str) -> bool {
        if cmd == "turns" {
            let args = args.trim();
            if args.is_empty() {
                let msg = match self.max_turns_override {
                    Some(TURNS_OFF_CEILING) => format!(
                        "Max turns: off (ceiling of {} turns) for this session.",
                        TURNS_OFF_CEILING
                    ),
                    Some(n) => format!("Max turns: {} (session override).", n),
                    None => "Max turns: using config/agent default.".to_string(),
                };
                self.status_message = Some(msg);
            } else if args == "off" || args == "disable" {
                self.max_turns_override = Some(TURNS_OFF_CEILING);
                self.status_message = Some(format!(
                    "Max turns off (ceiling of {} turns) for this session.",
                    TURNS_OFF_CEILING
                ));
            } else if let Some(n) = args.parse::<u32>().ok().filter(|&n| n > 0) {
                self.max_turns_override = Some(n);
                self.status_message = Some(format!("Max turns set to {} for this session.", n));
            } else {
                self.status_message = Some(
                    "Usage: /turns <positive number> | off | (blank to show current).".to_string(),
                );
            }
            return true;
        }
        if cmd == "yolo" {
            self.handle_yolo_command(args.trim());
            return true;
        }
        if cmd == "mcp" && !args.trim().is_empty() {
            return false;
        }
        self.intercept_slash_command(cmd)
    }

    /// Execute a `!` bang command in a spawned subprocess with a timeout
    /// and output cap. The command uses the Bash tool's shell runtime (bash on
    /// Unix, cmd on Windows) and the result is pushed into the transcript
    /// as a system message when the spawned task completes. Only one bang
    /// command may run at a time.
    pub fn execute_bang_command(&mut self, command: String) {
        if !self.config.bang_commands.enabled {
            self.status_message = Some("Bang commands are disabled.".to_string());
            return;
        }
        // Block in plan mode.
        if self.plan_mode || self.config.permission_mode == claurst_core::PermissionMode::Plan {
            self.push_notification(
                crate::notifications::NotificationKind::Warning,
                "Bang commands disabled in plan mode.".to_string(),
                None,
            );
            return;
        }

        if self.bang_command_pending.is_some() {
            self.status_message = Some("A bang command is already running.".to_string());
            return;
        }

        let working_dir = self.project_root();

        let (tx, rx) = tokio::sync::mpsc::channel(1);
        self.bang_command_pending = Some(rx);

        tokio::spawn(async move {
            let output = run_shell_command(&command, &working_dir).await;
            let _ = tx.send((command, output)).await;
        });
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
                self.theme_screen.open(self.config.theme.name());
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
                self.hooks_config_menu.open(&self.config.hooks);
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
            // The CLI publishes /new only after outgoing checkpoint and
            // queue parking succeed; retain the live transcript until then.
            "new" => true,
            "clear" => {
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
            "cc-statusline" | "cc-status-line" => {
                match &self.config.status_line {
                    Some(sl) => {
                        let interval = sl.poll_interval_ms.unwrap_or(5000);
                        let padding = sl.padding.unwrap_or(0);
                        self.status_message = Some(format!(
                            "External statusLine active · command: {} · interval: {}ms · padding: {}",
                            sl.command, interval, padding,
                        ));
                    }
                    None => {
                        self.status_message = Some(
                            "No external statusLine configured. Set \"statusLine\": { \"command\": \"...\" } in ~/.claurst/settings.json".to_string(),
                        );
                    }
                }
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
                // The CLI layer owns `/keybindings`:
                // `claurst_commands::KeybindingsCommand` writes the template
                // on first use and opens the file in the system editor.
                // This TUI intercept must NOT open it a second time for the
                // same command (issue #495) — fall through to the CLI layer
                // like /doctor, /init, /login, /logout do.
                false
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


/// Use the common shell runtime so pipe lifetime, timeouts and descendant
/// cleanup stay consistent with Bash. Output keeps bounded head/tail windows
/// and a complete spill path rather than closing a chatty child's pipes.
async fn run_shell_command(command: &str, working_dir: &std::path::Path) -> String {
    claurst_tools::pty_bash::run_user_shell(
        command,
        working_dir,
        std::time::Duration::from_secs(60),
        &claurst_core::paths::tool_output_dir(),
    ).await.content
}

#[cfg(test)]
mod bang_tests {
    use super::*;
    use crate::App;

    fn test_app() -> App {
        // Same construction path as make_app in tests.rs, kept local so this
        // module stays independent of the shared fixture.
        let config = claurst_core::Config::default();
        let cost_tracker = claurst_core::cost::CostTracker::new();
        App::new(config, cost_tracker)
    }

    #[tokio::test]
    async fn bang_output_combines_stdout_and_stderr() {
        let dir = std::env::temp_dir();
        let out = run_shell_command("echo out; echo err 1>&2", &dir).await;
        assert!(out.contains("out"));
        assert!(out.contains("err"));
    }

    #[tokio::test]
    async fn bang_output_reports_nonzero_exit_code() {
        let dir = std::env::temp_dir();
        let out = run_shell_command("exit 3", &dir).await;
        assert!(out.contains("code 3"), "got: {out}");
    }

    #[tokio::test]
    async fn bang_missing_working_dir_is_reported() {
        // A directory as the working dir fails spawn on all platforms.
        let dir = std::env::temp_dir().join("claurst-bang-no-such-dir");
        let out = run_shell_command("echo hi", &dir).await;
        assert!(
            out.contains("Failed to spawn") || out.contains("No such file or directory"),
            "missing working directory must be reported, got: {out}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bang_descendant_pipe_does_not_hold_the_prompt() {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_shell_command("sleep 3 & echo prompt-returned", &std::env::temp_dir()),
        ).await.expect("an inherited descendant pipe must not hang bang completion");
        assert!(result.contains("prompt-returned"), "{result}");
    }

    #[test]
    fn bang_command_requires_global_opt_in_even_for_direct_dispatch() {
        let mut app = test_app();
        app.execute_bang_command("echo disabled".into());
        assert!(app.bang_command_pending.is_none());
    }

    #[tokio::test]
    async fn second_bang_command_is_rejected_while_one_runs() {
        let mut app = test_app();
        app.config.bang_commands.enabled = true;
        app.execute_bang_command("sleep 1".to_string());
        assert!(app.bang_command_pending.is_some());

        app.execute_bang_command("echo second".to_string());
        assert_eq!(
            app.status_message.as_deref(),
            Some("A bang command is already running.")
        );

        // Drain both so the spawned task and channel shut down cleanly.
        for _ in 0..200 {
            app.poll_session_loads();
            if app.bang_command_pending.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(app.bang_command_pending.is_none());
    }

    #[tokio::test]
    async fn bang_command_result_lands_in_transcript() {
        let mut app = test_app();
        app.config.bang_commands.enabled = true;
        app.execute_bang_command("echo bang-ok".to_string());

        for _ in 0..200 {
            app.poll_session_loads();
            if app.system_annotations.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            } else {
                break;
            }
        }
        assert_eq!(app.system_annotations.len(), 1);
        assert!(app.system_annotations[0].text.contains("bang-ok"));
    }

    #[tokio::test]
    async fn bang_command_blocked_in_plan_mode() {
        let mut app = test_app();
        app.config.bang_commands.enabled = true;
        app.config.permission_mode = claurst_core::PermissionMode::Plan;
        app.execute_bang_command("echo no".to_string());
        assert!(app.bang_command_pending.is_none());
        assert!(!app.notifications.is_empty());
    }
}

#[cfg(test)]
mod bang_first_output_render_test {
    #[tokio::test]
    async fn bang_first_output_replaces_empty_welcome_screen() {
        let mut config = claurst_core::Config::default();
        config.bang_commands.enabled = true;
        let mut app = crate::App::new(config, claurst_core::cost::CostTracker::new());
        app.execute_bang_command("echo reviewer-smoke".into());
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while app.bang_command_pending.is_some() {
                app.poll_session_loads();
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(app.messages.is_empty());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::render::render_app(frame, &app))
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            screen.contains("reviewer-smoke"),
            "first local output must replace the welcome screen"
        );
    }
}
