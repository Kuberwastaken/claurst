// claurst-tui: Terminal UI using ratatui + crossterm for Claurst.
//
// This crate provides the interactive terminal interface including:
// - Message display with syntax highlighting
// - Input prompt with history
// - Streaming response rendering
// - Tool execution progress display
// - Permission dialogs
// - Cost/token tracking display
// - Notification banners
// - Help, history-search, message-selector, and rewind overlays
// - Bridge connection status badge
// - Plugin hint banners

// too_many_arguments: a handful of render/context helpers take many parameters
// (layout rects, styles, state slices); grouping them is a larger refactor out
// of scope for this cleanup.
#![allow(clippy::too_many_arguments)]

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
// EnableBracketedPaste is enabled on macOS and Linux only. On Windows, it causes
// Windows Terminal to wrap Ctrl+V content in VT escape sequences that crossterm's
// Windows Console API backend doesn't decode as `Event::Paste` — the bytes land as
// raw key events, turning every `\n` into a prompt submit. Unix terminals (macOS/Linux)
// handle bracketed paste correctly, allowing multi-line pastes to preserve newlines.
#[cfg(not(target_os = "windows"))]
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether the active terminal speaks the kitty keyboard protocol (progressive
/// keyboard enhancement). Detected once during [`setup_terminal`]. Defaults to
/// `false` so that, until proven otherwise, we treat printable key presses as
/// already-final characters (the behaviour of conhost / CMD / legacy PowerShell
/// and most default terminals) instead of re-applying a US-QWERTY shift map —
/// see `App::shift_normalize` and issue #183.
static KEYBOARD_ENHANCEMENT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Returns whether the terminal's kitty keyboard protocol is active, as detected
/// during [`setup_terminal`]. The run loop copies this onto the `App` so input
/// normalization can be gated on it.
pub fn keyboard_enhancement_active() -> bool {
    KEYBOARD_ENHANCEMENT_ACTIVE.load(Ordering::Relaxed)
}

/// Whether app-level mouse capture was enabled during [`setup_terminal`].
/// Mirrors [`KEYBOARD_ENHANCEMENT_ACTIVE`]: set once at setup from the user's
/// `mouseCapture` config so the panic hook and restore path know whether to
/// emit `DisableMouseCapture` (issue #104). Defaults to `true` so a restore
/// that runs before setup keeps the historical always-disable behaviour.
static MOUSE_CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(true);

// ---------------------------------------------------------------------------
// Sub-modules
// ---------------------------------------------------------------------------

/// Figure/icon constants matching src/constants/figures.ts
pub mod figures;
/// Rustle mascot rendering.
pub mod rustle;
/// Context window and rate-limit visualization overlay (/context).
pub mod context_viz;
/// Export format picker dialog (/export).
pub mod export_dialog;
/// Clipboard image paste and Ctrl+V text paste.
pub mod image_paste;
/// Inline image rendering via the Kitty graphics protocol (with text fallback).
pub mod kitty_image;
/// Application state and main event loop.
pub mod app;
/// Input helpers: slash command parsing.
pub mod input;
/// All ratatui rendering logic.
pub mod render;
/// Post-paint OSC 8 hyperlink emission — makes URLs Ctrl/Cmd-clickable.
pub mod osc8;
/// Permission dialogs and confirmation dialogs.
pub mod dialogs;
/// Notification / banner system.
pub mod notifications;
/// Help overlay, history search, message selector, rewind flow.
pub mod overlays;
/// Bridge connection state and status badge.
pub mod bridge_state;
/// Plugin hint/recommendation UI.
pub mod plugin_views;
/// Full-screen tabbed settings interface.
pub mod settings_screen;
/// Theme picker overlay.
pub mod theme_screen;
/// Color palette management for different themes and accessibility support.
pub mod theme_colors;
/// Diff viewer dialog (two-pane: file list + unified diff detail).
pub mod diff_viewer;
/// Read-only viewer for [Pasted text #N ...] placeholders.
pub mod paste_viewer;
/// Virtual scrollable list for efficient message rendering.
pub mod virtual_list;
/// Message type renderers (assistant, user, tool use, etc.).
pub mod messages;
/// Turn-aware transcript grouping and metadata helpers.
pub mod transcript_turn;
/// Agent definitions list and coordinator progress view.
pub mod agents_view;
/// Stats dialog with token usage and cost charts.
pub mod stats_dialog;
/// MCP server management UI.
pub mod mcp_view;
/// Complete prompt input with vim mode, history, typeahead, and paste handling.
pub mod prompt_input;
/// Session quality feedback survey overlay.
pub mod feedback_survey;
/// Memory file selector overlay (AGENTS.md browser).
pub mod memory_file_selector;
/// Read-only hooks configuration browser.
pub mod hooks_config_menu;
/// Overage credit upsell banner (shown when user exceeds free-tier limit).
pub mod overage_upsell;
/// Voice mode availability notice (shown when voice is available but not enabled).
pub mod voice_mode_notice;
/// Message copy utilities for different formatting options (markdown, plaintext, code, JSON).
pub mod message_copy;
/// Desktop app upsell startup dialog (shown at startup on macOS/Windows x64).
pub mod desktop_upsell_startup;
/// Memory update notification banner (shown after Claurst updates a AGENTS.md file).
pub mod memory_update_notification;
/// MCP elicitation dialog (form-based user input requested by MCP servers).
pub mod elicitation_dialog;
/// Model picker overlay (/model command).
pub mod model_picker;
/// Session browser overlay (/session, /resume, /rename, /export).
pub mod session_browser;
/// Startup dialog for malformed settings.json or AGENTS.md.
pub mod invalid_config_dialog;
/// Startup confirmation dialog for --dangerously-skip-permissions mode.
pub mod bypass_permissions_dialog;
/// First-launch onboarding / welcome dialog.
pub mod onboarding_dialog;
/// Effort-level picker dialog (/effort).
pub mod effort_picker;
/// Reusable fuzzy-search selection dialog widget.
pub mod dialog_select;
/// Masked text input overlay for entering API keys.
pub mod key_input_dialog;
/// Modal dialog for entering custom provider URL + API key.
pub mod custom_provider_dialog;
/// Setup dialog for the composite "Free" provider (Zen → OpenRouter).
pub mod free_mode_dialog;
/// Device code / browser-based auth overlay (GitHub Copilot, Anthropic OAuth).
pub mod device_auth_dialog;
/// Push-to-talk voice capture and Whisper transcription.
pub mod voice_capture;
/// Task progress overlay (Ctrl+T) — shows task status with inline toggle.
pub mod tasks_overlay;
/// Import-config preview and confirmation dialog.
pub mod import_config_dialog;
/// Session branching overlay (Ctrl+B) — create and switch between conversation branches.
pub mod session_branching;
/// Model-initiated question dialog (AskUserQuestion tool).
pub mod ask_user_dialog;
/// File injection utilities for parsing @file references.
pub mod file_injection;
/// File injection warning dialog (shown when oversized files detected).
pub mod file_injection_dialog;

// ---------------------------------------------------------------------------
// Public re-exports
// ---------------------------------------------------------------------------

pub use app::{App, try_copy_to_clipboard};
pub use notifications::NotificationKind;
pub use input::{is_slash_command, parse_slash_command};
pub use feedback_survey::{FeedbackSurveyState, FeedbackSurveyStage, FeedbackResponse};
pub use memory_file_selector::{MemoryFileSelectorState, MemoryFile, MemoryFileType};
pub use hooks_config_menu::{HooksConfigMenuState, HookEntry};
pub use overage_upsell::{OverageCreditUpsellState, render_overage_upsell};
pub use voice_mode_notice::{VoiceModeNoticeState, render_voice_mode_notice};
pub use desktop_upsell_startup::{DesktopUpsellStartupState, DesktopUpsellSelection, render_desktop_upsell_startup};
pub use memory_update_notification::{MemoryUpdateNotificationState, render_memory_update_notification, get_relative_memory_path};
pub use elicitation_dialog::{ElicitationDialogState, ElicitationField, ElicitationFieldKind, ElicitationResult, render_elicitation_dialog};
pub use diff_viewer::{DiffViewerState, DiffPane, DiffType, load_git_diff, parse_unified_diff, render_diff_dialog};
pub use agents_view::{AgentInfo, AgentStatus, AgentsMenuState, AgentDefinition, render_agents_menu, render_coordinator_status, load_agent_definitions};
pub use stats_dialog::{StatsDialogState, StatsTab, load_stats, render_stats_dialog};
pub use mcp_view::{McpViewState, McpServerView, McpToolView, McpViewStatus, render_mcp_view};
pub use prompt_input::{PromptInputState, VimMode, VimPendingState, VimOperator, VimFindKind, InputMode, render_prompt_input, handle_paste, compute_typeahead};
pub use model_picker::{ModelPickerState, ModelEntry, EffortLevel, render_model_picker, model_supports_effort};
pub use session_browser::{SessionBrowserState, SessionBrowserMode, SessionEntry, render_session_browser};
pub use import_config_dialog::{ImportConfigDialogState, render_import_config_dialog};
pub use session_branching::{SessionBranchingState, BranchBrowserMode, BranchInfo, render_session_branching};
pub use invalid_config_dialog::{InvalidConfigDialogState, InvalidConfigKind, render_invalid_config_dialog};
pub use bypass_permissions_dialog::{BypassPermissionsDialogState, render_bypass_permissions_dialog};
pub use onboarding_dialog::{OnboardingDialogState, render_onboarding_dialog};
pub use dialog_select::{DialogSelectState, SelectItem, render_dialog_select};
pub use key_input_dialog::{KeyInputDialogState, render_key_input_dialog};
pub use custom_provider_dialog::{CustomProviderDialogState, CustomProviderField, render_custom_provider_dialog};
pub use free_mode_dialog::{FreeModeDialogState, FreeModeField, render_free_mode_dialog};
// (FreeModeField type is now per-provider; legacy callers may still import both names.)
pub use device_auth_dialog::{DeviceAuthDialogState, DeviceAuthStatus, DeviceAuthEvent, render_device_auth_dialog};
pub use file_injection::{parse_at_refs, build_file_blocks, AtFileRef, AtFileIssue};
pub use file_injection_dialog::{FileInjectionDialogState, FileInjectionOutcome, render_file_injection_dialog};

// ---------------------------------------------------------------------------
// Terminal initialization / teardown helpers (public API)
// ---------------------------------------------------------------------------

/// Restore terminal capabilities (alternate screen, mouse capture, bracketed paste, keyboard enhancement).
/// Used by both restore_terminal and the panic hook.
fn restore_terminal_cleanup() -> io::Result<()> {
    #[cfg(not(target_os = "windows"))]
    if MOUSE_CAPTURE_ACTIVE.load(Ordering::Relaxed) {
        execute!(io::stdout(), DisableMouseCapture)?;
    }
    #[cfg(not(target_os = "windows"))]
    execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags,
    )?;

    // On Windows, KeyboardEnhancementFlags may not have been pushed
    // (conhost / older terminals reject the escape).  Pop is harmless
    // whether it succeeded or not — ignore errors on restore.
    #[cfg(target_os = "windows")]
    {
        // Pop may fail on legacy Windows conhost where the push was a no-op;
        // do it best-effort so cleanup never errors out the process.
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        if MOUSE_CAPTURE_ACTIVE.load(Ordering::Relaxed) {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
        execute!(io::stdout(), LeaveAlternateScreen)?;
    }

    Ok(())
}

/// Set up the terminal for TUI mode (raw mode + alternate screen + mouse capture).
///
/// Also installs a panic hook that restores the terminal before printing the
/// panic message.  Without this, any panic in rendering code leaves the
/// terminal in raw mode with mouse capture enabled — the user sees garbage
/// input until they run `reset`.
pub fn setup_terminal(mouse_capture: bool) -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    // Chain on top of any existing hook (e.g. from a previous call or test harness).
    // Only restore the terminal when the panic originates on the main thread.
    // Tokio worker threads also trigger this process-wide hook (Tokio catches
    // the panic internally but the hook still fires), so without this guard any
    // panicking background task would destroy the live TUI display while the
    // main render loop is still running.
    // Record whether mouse capture is on so the panic hook / restore path
    // know whether to emit DisableMouseCapture (issue #104).
    MOUSE_CAPTURE_ACTIVE.store(mouse_capture, Ordering::Relaxed);
    let main_thread_id = std::thread::current().id();
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        if std::thread::current().id() == main_thread_id {
            // Best-effort restore — ignore errors, we're already unwinding.
            let _ = disable_raw_mode();
            let _ = restore_terminal_cleanup();
            let _ = execute!(io::stdout(), crossterm::cursor::Show);
        }
        original_hook(panic_info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();

    #[cfg(not(target_os = "windows"))]
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
        ),
    )?;
    // Mouse capture is opt-out (issue #104): when on, the app handles scroll /
    // right-click menu / drag-select but the terminal can no longer do native
    // text selection. Skip it when the user set mouseCapture: false.
    #[cfg(not(target_os = "windows"))]
    if mouse_capture {
        execute!(stdout, EnableMouseCapture)?;
    }

    // On Windows, keyboard enhancement is best-effort: conhost and older
    // terminal builds do not support the kitty keyboard protocol.
    // crossterm 0.29 improved detection but may still reject in some
    // configurations.  Warn the user when it fails, then continue.
    #[cfg(target_os = "windows")]
    {
        execute!(stdout, EnterAlternateScreen)?;
        if mouse_capture {
            execute!(stdout, EnableMouseCapture)?;
        }
        // Kitty keyboard protocol is unsupported on legacy Windows conhost;
        // best-effort — modern Windows Terminal accepts it, conhost returns
        // "Keyboard progressive enhancement not implemented for the legacy
        // Windows API" which we ignore so the TUI can still start.
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
            ),
        );
    }

    // Detect whether the terminal actually speaks the kitty keyboard protocol.
    // This decides how we interpret printable key presses: kitty terminals report
    // the unshifted base key + a SHIFT modifier (so we apply the shift map), while
    // everything else (conhost / CMD / legacy PowerShell, default macOS Terminal,
    // older xterms) reports the final, layout-correct character (so we must not
    // re-shift it — issue #183). Failure to query is treated as "no kitty".
    let kitty_active = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    KEYBOARD_ENHANCEMENT_ACTIVE.store(kitty_active, Ordering::Relaxed);

    set_terminal_title("\u{1f980} Claurst");
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its original state.
pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    // Clear any terminal "busy" progress indicator (OSC 9;4) we may have set.
    set_terminal_progress(false);
    // Restore the original title by clearing it (terminals fall back to default).
    let _ = execute!(
        terminal.backend_mut(),
        crossterm::terminal::SetTitle(""),
    );
    restore_terminal_cleanup()?;
    terminal.show_cursor()?;
    Ok(())
}

/// Set the terminal window title via OSC escape sequence.
pub fn set_terminal_title(title: &str) {
    let _ = execute!(io::stdout(), crossterm::terminal::SetTitle(title));
}

/// Whether the current terminal is known to render the OSC 9;4 "progress"
/// sequence. Most terminals silently ignore an unknown OSC, but a bare
/// tmux/screen passthrough can leak it as visible text, so we require a real
/// tty and an allow-listed terminal (iTerm2, WezTerm, Ghostty, Windows
/// Terminal, ConEmu).
pub fn supports_progress_osc() -> bool {
    use std::io::IsTerminal;
    if !io::stdout().is_terminal() {
        return false;
    }
    // tmux/screen don't forward OSC 9;4 to the outer terminal by default.
    if std::env::var_os("TMUX").is_some() {
        return false;
    }
    if let Ok(term) = std::env::var("TERM") {
        if term.starts_with("screen") || term.starts_with("tmux") {
            return false;
        }
    }
    if std::env::var_os("WT_SESSION").is_some() || std::env::var_os("ConEmuPID").is_some() {
        return true;
    }
    matches!(
        std::env::var("TERM_PROGRAM").unwrap_or_default().as_str(),
        "iTerm.app" | "WezTerm" | "ghostty"
    )
}

/// Emit the OSC 9;4 progress sequence. `active = true` shows an indeterminate
/// "busy" indicator (e.g. iTerm2's progress bar, the Windows Terminal taskbar);
/// `false` clears it. No-op on terminals that don't support it.
///
/// Safe alongside the ratatui alternate screen: OSC 9;4 addresses terminal
/// chrome (title bar / taskbar), not the cell grid, so it doesn't disturb the
/// rendered frame.
pub fn set_terminal_progress(active: bool) {
    if !supports_progress_osc() {
        return;
    }
    use std::io::Write;
    // state 3 = indeterminate/busy, state 0 = clear; the progress value is
    // unused for the indeterminate state.
    let seq: &[u8] = if active { b"\x1b]9;4;3;0\x07" } else { b"\x1b]9;4;0;0\x07" };
    let mut out = io::stdout();
    let _ = out.write_all(seq);
    let _ = out.flush();
}

/// Update the terminal title to reflect the current session context.
/// Format: "🦀 | <topic>" or just "🦀 Claurst" when no topic is set.
pub fn update_terminal_title(topic: Option<&str>) {
    match topic {
        Some(t) if !t.is_empty() => set_terminal_title(&format!("\u{1f980} | {}", t)),
        _ => set_terminal_title("\u{1f980} Claurst"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use app::{App, HistorySearch, ToolStatus, ToolUseBlock};
    use claurst_core::config::Config;
    use claurst_core::cost::CostTracker;
    use claurst_core::file_history::FileHistory;
    use claurst_core::types::{ContentBlock, Role, ToolResultContent};
    use dialogs::PermissionRequest;
    use notifications::NotificationKind;
    use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect, Terminal};
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn make_app() -> App {
        App::new(Config::default(), CostTracker::new())
    }

    // ---- input helpers ---------------------------------------------------

    #[test]
    fn test_is_slash_command() {
        assert!(input::is_slash_command("/help"));
        assert!(input::is_slash_command("/compact args"));
        assert!(!input::is_slash_command("//comment"));
        assert!(!input::is_slash_command("hello"));
        assert!(!input::is_slash_command(""));
    }

    #[test]
    fn test_parse_slash_command_no_args() {
        let (cmd, args) = input::parse_slash_command("/help");
        assert_eq!(cmd, "help");
        assert_eq!(args, "");
    }

    #[test]
    fn test_parse_slash_command_with_args() {
        let (cmd, args) = input::parse_slash_command("/compact  --force ");
        assert_eq!(cmd, "compact");
        assert_eq!(args, "--force");
    }

    #[test]
    fn test_parse_slash_command_non_slash() {
        let (cmd, args) = input::parse_slash_command("hello world");
        assert_eq!(cmd, "");
        assert_eq!(args, "");
    }

    // ---- App::take_input ------------------------------------------------

    #[test]
    fn test_take_input_pushes_history() {
        let mut app = make_app();
        app.set_prompt_text("hello".to_string());
        let result = app.take_input();
        assert_eq!(result, "hello");
        assert_eq!(app.input, "");
        assert_eq!(app.prompt_input.text, "");
        assert_eq!(app.input_history, vec!["hello"]);
        assert_eq!(app.prompt_input.history, vec!["hello"]);
        assert_eq!(app.cursor_pos, 0);
    }

    #[test]
    fn test_take_input_empty_does_not_push_history() {
        let mut app = make_app();
        let result = app.take_input();
        assert_eq!(result, "");
        assert!(app.input_history.is_empty());
    }

    // ---- add_message / set_model ----------------------------------------

    #[test]
    fn test_add_message() {
        let mut app = make_app();
        app.add_message(Role::User, "hi".to_string());
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, Role::User);
    }

    #[test]
    fn test_set_model() {
        let mut app = make_app();
        app.set_model("claude-opus-4-5".to_string());
        assert_eq!(app.model_name, "claude-opus-4-5");
    }

    #[test]
    fn test_stats_slash_command_opens_dialog_and_closes_other_views() {
        let mut app = make_app();
        app.mcp_view.open(vec![]);
        app.agents_menu.visible = true;

        assert!(app.intercept_slash_command("stats"));
        assert!(app.stats_dialog.visible);
        assert!(!app.mcp_view.visible);
        assert!(!app.agents_menu.visible);
        assert!(!app.diff_viewer.visible);
    }

    #[test]
    fn test_agents_slash_command_populates_active_agents() {
        let mut app = make_app();
        app.agent_status = vec![
            ("Mendel".to_string(), "running".to_string()),
            ("Aristotle".to_string(), "waiting".to_string()),
            ("Plato".to_string(), "done".to_string()),
        ];

        assert!(app.intercept_slash_command("agents"));
        assert!(app.agents_menu.visible);
        assert_eq!(app.agents_menu.active_agents.len(), 3);
        assert_eq!(app.agents_menu.active_agents[0].status, AgentStatus::Running);
        assert_eq!(
            app.agents_menu.active_agents[1].status,
            AgentStatus::WaitingForTool
        );
        assert_eq!(app.agents_menu.active_agents[2].status, AgentStatus::Complete);
    }

    #[test]
    fn test_agents_editor_ctrl_s_saves_new_agent() {
        let temp = tempdir().unwrap();
        let mut app = make_app();
        app.agents_menu.open(temp.path());
        app.agents_menu.open_editor(None);
        app.agents_menu.editor.name = "Planner".to_string();
        app.agents_menu.editor.description = "Plans complex work".to_string();
        app.agents_menu.editor.prompt = "Help break work into steps.".to_string();

        app.handle_key_event(ctrl(KeyCode::Char('s')));

        let saved = temp.path().join(".claurst").join("agents").join("planner.md");
        assert!(saved.exists());
        let content = std::fs::read_to_string(saved).unwrap();
        assert!(content.contains("name: Planner"));
        assert!(content.contains("Help break work into steps."));
        assert!(matches!(app.agents_menu.route, agents_view::AgentsRoute::Detail(_)));
    }

    #[test]
    fn test_agents_editor_render_uses_live_editor_state() {
        let temp = tempdir().unwrap();
        let mut state = AgentsMenuState::new();
        state.open(temp.path());
        state.open_editor(None);
        state.editor.name = "Builder".to_string();
        state.editor.description = "Builds code".to_string();
        state.editor.prompt = "Ship the feature.".to_string();

        let area = Rect { x: 0, y: 0, width: 90, height: 24 };
        let mut buf = Buffer::empty(area);
        agents_view::render_agents_menu(&state, area, &mut buf);

        let rendered = buf
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(rendered.contains("Builder"));
        assert!(rendered.contains("Ship the feature."));
        assert!(!rendered.contains("Edit the agent file directly"));
    }

    #[test]
    fn test_changes_slash_command_opens_turn_diff_mode() {
        let mut app = make_app();

        assert!(app.intercept_slash_command("changes"));
        assert!(app.diff_viewer.visible);
        assert_eq!(app.diff_viewer.diff_type, DiffType::TurnDiff);
    }

    // ---- key handling ----------------------------------------------------

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn test_ctrl_c_requires_two_presses_to_exit() {
        let mut app = make_app();
        // First Ctrl+C: shows warning, doesn't exit
        app.handle_key_event(ctrl(KeyCode::Char('c')));
        assert!(!app.should_exit);
        assert!(app.last_exit_key_warning.is_some());

        // Second Ctrl+C within 1 second: exits
        app.handle_key_event(ctrl(KeyCode::Char('c')));
        assert!(app.should_exit);
    }

    #[test]
    fn test_ctrl_c_clears_input_first_press() {
        let mut app = make_app();
        app.set_prompt_text("some text".to_string());
        // First Ctrl+C: clears input
        app.handle_key_event(ctrl(KeyCode::Char('c')));
        assert!(app.prompt_input.is_empty());
        assert!(!app.should_exit);
        assert!(app.last_exit_key_warning.is_some());
    }

    #[test]
    fn test_other_key_resets_exit_warning() {
        let mut app = make_app();
        // First Ctrl+C: shows warning
        app.handle_key_event(ctrl(KeyCode::Char('c')));
        assert!(app.last_exit_key_warning.is_some());

        // Press a different key: resets warning
        app.handle_key_event(key(KeyCode::Char('a')));
        assert!(app.last_exit_key_warning.is_none());

        // Now Ctrl+C starts over (doesn't exit)
        app.handle_key_event(ctrl(KeyCode::Char('c')));
        assert!(!app.should_exit);
        assert!(app.last_exit_key_warning.is_some());
    }

    #[test]
    fn test_ctrl_c_timeout_resets_after_two_seconds() {
        let mut app = make_app();
        // First Ctrl+C: shows warning
        app.handle_key_event(ctrl(KeyCode::Char('c')));
        assert!(app.last_exit_key_warning.is_some());

        // Manually expire the timer by setting it to >2 seconds ago
        app.last_exit_key_warning = Some(std::time::Instant::now()
            - std::time::Duration::from_millis(2100));

        // Second Ctrl+C after timeout: should show warning again, not exit
        app.handle_key_event(ctrl(KeyCode::Char('c')));
        assert!(!app.should_exit);
        assert!(app.last_exit_key_warning.is_some());
    }

    #[test]
    fn test_ctrl_c_cancels_streaming() {
        let mut app = make_app();
        app.is_streaming = true;
        app.streaming_text = "partial".to_string();
        app.handle_key_event(ctrl(KeyCode::Char('c')));
        assert!(!app.is_streaming);
        assert!(!app.should_exit);
        assert!(app.streaming_text.is_empty());
    }

    #[test]
    fn test_ctrl_d_requires_two_presses_to_exit() {
        let mut app = make_app();
        // First Ctrl+D: shows warning, doesn't exit
        app.handle_key_event(ctrl(KeyCode::Char('d')));
        assert!(!app.should_exit);
        assert!(app.last_exit_key_warning.is_some());

        // Second Ctrl+D within 2 seconds: exits
        app.handle_key_event(ctrl(KeyCode::Char('d')));
        assert!(app.should_exit);
    }

    #[test]
    fn test_ctrl_d_does_not_quit_with_input() {
        let mut app = make_app();
        app.set_prompt_text("abc".to_string());
        app.handle_key_event(ctrl(KeyCode::Char('d')));
        assert!(!app.should_exit);
    }

    #[test]
    fn test_ctrl_c_then_ctrl_d_then_ctrl_c_exits() {
        let mut app = make_app();
        // First Ctrl+C: shows warning, sets start_key = 'c'
        app.handle_key_event(ctrl(KeyCode::Char('c')));
        assert!(!app.should_exit);
        assert_eq!(app.exit_key_sequence_start, Some('c'));

        // Ctrl+D (wrong key): resets timer but keeps waiting for Ctrl+C
        app.handle_key_event(ctrl(KeyCode::Char('d')));
        assert!(!app.should_exit);
        assert_eq!(app.exit_key_sequence_start, Some('c')); // Still waiting for Ctrl+C

        // Ctrl+C again: exits
        app.handle_key_event(ctrl(KeyCode::Char('c')));
        assert!(app.should_exit);
    }

    #[test]
    fn test_ctrl_d_then_ctrl_c_then_ctrl_d_exits() {
        let mut app = make_app();
        // First Ctrl+D: shows warning, sets start_key = 'd'
        app.handle_key_event(ctrl(KeyCode::Char('d')));
        assert!(!app.should_exit);
        assert_eq!(app.exit_key_sequence_start, Some('d'));

        // Ctrl+C (wrong key): resets timer but keeps waiting for Ctrl+D
        app.handle_key_event(ctrl(KeyCode::Char('c')));
        assert!(!app.should_exit);
        assert_eq!(app.exit_key_sequence_start, Some('d')); // Still waiting for Ctrl+D

        // Ctrl+D again: exits
        app.handle_key_event(ctrl(KeyCode::Char('d')));
        assert!(app.should_exit);
    }

    #[test]
    fn test_enter_returns_true() {
        let mut app = make_app();
        let submit = app.handle_key_event(key(KeyCode::Enter));
        assert!(submit);
    }

    #[test]
    fn test_enter_blocked_while_streaming() {
        let mut app = make_app();
        app.is_streaming = true;
        let submit = app.handle_key_event(key(KeyCode::Enter));
        assert!(!submit);
    }

    #[test]
    fn test_char_input_appends() {
        let mut app = make_app();
        app.handle_key_event(key(KeyCode::Char('h')));
        app.handle_key_event(key(KeyCode::Char('i')));
        assert_eq!(app.input, "hi");
        assert_eq!(app.prompt_input.text, "hi");
    }

    #[test]
    fn test_backspace_removes_char() {
        let mut app = make_app();
        app.set_prompt_text("hello".to_string());
        app.handle_key_event(key(KeyCode::Backspace));
        assert_eq!(app.input, "hell");
        assert_eq!(app.prompt_input.text, "hell");
    }

    #[test]
    fn test_history_navigation() {
        let mut app = make_app();
        app.prompt_input.history = vec!["first".to_string(), "second".to_string()];
        app.input_history = app.prompt_input.history.clone();
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.input, "second");
        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.input, "first");
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.input, "second");
        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.input, "");
        assert!(app.history_index.is_none());
    }

    #[test]
    fn test_history_navigation_restores_draft() {
        let mut app = make_app();
        app.prompt_input.history = vec!["first".to_string(), "second".to_string()];
        app.input_history = app.prompt_input.history.clone();
        app.set_prompt_text("draft".to_string());

        app.handle_key_event(key(KeyCode::Up));
        assert_eq!(app.input, "second");

        app.handle_key_event(key(KeyCode::Down));
        assert_eq!(app.input, "draft");
        assert_eq!(app.prompt_input.text, "draft");
        assert!(app.history_index.is_none());
    }

    #[test]
    fn test_tab_accepts_slash_suggestion() {
        let mut app = make_app();
        app.handle_key_event(key(KeyCode::Char('/')));
        app.handle_key_event(key(KeyCode::Char('a')));
        app.handle_key_event(key(KeyCode::Tab));

        assert_eq!(app.input, "/advisor");
        assert_eq!(app.prompt_input.text, "/advisor");
        assert_eq!(app.cursor_pos, "/advisor".len());
    }

    #[test]
    fn test_ctrl_p_opens_global_search() {
        let mut app = make_app();
        app.handle_key_event(ctrl(KeyCode::Char('p')));
        assert!(app.global_search.visible);
    }

    #[test]
    fn test_global_search_enter_inserts_selected_ref() {
        let mut app = make_app();
        app.global_search.open();
        app.global_search.results = vec![overlays::SearchResult {
            file: "src/main.rs".to_string(),
            line: 42,
            col: 1,
            text: "fn main() {}".to_string(),
            context_before: Vec::new(),
            context_after: Vec::new(),
        }];
        app.handle_key_event(key(KeyCode::Enter));

        assert!(!app.global_search.visible);
        assert_eq!(app.input, "src/main.rs:42");
        assert_eq!(app.prompt_input.text, "src/main.rs:42");
    }

    #[test]
    fn test_render_app_shows_scrollable_banner_after_first_message() {
        // Once a conversation starts, the welcome box is no longer pinned as a
        // fixed header (issue #310): a compact banner leads the transcript (and
        // scrolls away with content), while the rich two-column welcome box — its
        // mascot and "Recent activity" panel — is only the empty-screen welcome.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();
        app.push_message(claurst_core::types::Message::user("hello".to_string()));

        terminal
            .draw(|frame| crate::render::render_app(frame, &app))
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        // The compact banner (title + hint) and the message both render.
        assert!(rendered.contains("Claurst"));
        assert!(rendered.contains("? for shortcuts"));
        assert!(rendered.contains("hello"));
        // The full welcome box's recent-activity panel must NOT be drawn during
        // a conversation — that would mean the old fixed header is still there.
        assert!(!rendered.contains("Recent activity"));
    }

    #[test]
    fn test_render_app_keeps_footer_visible_with_slash_suggestions() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();
        app.handle_key_event(key(KeyCode::Char('/')));
        app.handle_key_event(key(KeyCode::Char('a')));

        terminal
            .draw(|frame| crate::render::render_app(frame, &app))
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(rendered.contains("/agents"));
        assert!(rendered.contains("[cmd]"));
        assert!(rendered.contains("Browse agent definitions"));
    }

    #[test]
    fn test_render_app_hides_generic_thinking_status_row() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();
        app.is_streaming = true;

        terminal
            .draw(|frame| crate::render::render_app(frame, &app))
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(!rendered.contains("Thinking..."));
    }

    #[test]
    fn test_render_app_shows_footer_notification_instead_of_effort() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();
        app.notifications.push(
            NotificationKind::Warning,
            "Update available! Run upgrade".to_string(),
            Some(30),
        );

        terminal
            .draw(|frame| crate::render::render_app(frame, &app))
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(rendered.contains("Update available! Run upgrade"));
        assert!(!rendered.contains("/effort"));
    }

    #[test]
    fn test_render_app_hides_shortcuts_hint_when_prompt_has_text() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();
        app.set_prompt_text("hello".to_string());

        terminal
            .draw(|frame| crate::render::render_app(frame, &app))
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(!rendered.contains("? shortcuts"));
    }

    #[test]
    fn test_render_app_shows_startup_notices_below_logo_header() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();
        app.away_summary = Some("2 agent updates while you were away".to_string());
        app.remote_session_url = Some("https://example.com/session/123".to_string());
        app.bridge_state = crate::bridge_state::BridgeConnectionState::Connected {
            session_url: "https://example.com/session/123".to_string(),
            peer_count: 2,
        };

        terminal
            .draw(|frame| crate::render::render_app(frame, &app))
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(rendered.contains("2 agent updates while you were away"));
        assert!(rendered.contains("Remote session active"));
        assert!(rendered.contains("https://example.com/session/123"));
    }

    #[test]
    fn test_turn_diff_toggle_uses_cached_turn_files() {
        let mut state = DiffViewerState::new();
        state.set_turn_diff(vec![diff_viewer::FileDiffStats {
            path: "src/lib.rs".to_string(),
            added: 2,
            removed: 1,
            binary: false,
            is_new_file: false,
            hunks: vec![diff_viewer::DiffHunk {
                old_range: (1, 1),
                new_range: (1, 2),
                lines: vec![diff_viewer::DiffLine {
                    kind: diff_viewer::DiffLineKind::Header,
                    content: "@@ -1,1 +1,2 @@".to_string(),
                    old_line_no: None,
                    new_line_no: None,
                }],
            }],
        }]);

        state.toggle_diff_type(std::path::Path::new("."));

        assert_eq!(state.diff_type, DiffType::TurnDiff);
        assert_eq!(state.files.len(), 1);
        assert_eq!(state.files[0].path, "src/lib.rs");
    }

    #[test]
    fn test_build_turn_diff_from_history_snapshots() {
        let mut history = FileHistory::new();
        history.record_modification(
            PathBuf::from("/workspace/src/lib.rs"),
            b"fn old() {}\n",
            b"fn new() {}\n",
            3,
            "FileEdit",
        );

        let files = diff_viewer::build_turn_diff(&history, 3, std::path::Path::new("/workspace"));

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!(files[0].added, 1);
        assert_eq!(files[0].removed, 1);
        assert!(!files[0].hunks.is_empty());
    }

    #[test]
    fn test_changes_slash_command_refreshes_turn_history() {
        let mut app = make_app();
        let file_history = Arc::new(parking_lot::Mutex::new(FileHistory::new()));
        let current_turn = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        file_history.lock().record_modification(
            PathBuf::from("src/lib.rs"),
            b"fn before() {}\n",
            b"fn after() {}\n",
            1,
            "FileEdit",
        );
        app.attach_turn_diff_state(file_history, current_turn);

        assert!(app.intercept_slash_command("changes"));
        assert_eq!(app.diff_viewer.diff_type, DiffType::TurnDiff);
        assert_eq!(app.diff_viewer.files.len(), 1);
        assert_eq!(app.diff_viewer.files[0].path, "src/lib.rs");
    }

    #[test]
    fn test_render_diff_dialog_shows_turn_empty_state() {
        let mut state = DiffViewerState::new();
        state.visible = true;
        state.diff_type = DiffType::TurnDiff;
        let area = Rect { x: 0, y: 0, width: 80, height: 20 };
        let mut buf = Buffer::empty(area);

        diff_viewer::render_diff_dialog(&mut state, area, &mut buf);

        let rendered = buf
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(rendered.contains("No changes were captured for this turn."));
    }

    #[test]
    fn test_page_scroll() {
        let mut app = make_app();
        // A render must have established a scrollable range; otherwise a
        // scroll-up is correctly a no-op (offset is clamped to max_scroll, #223).
        app.last_max_scroll.set(100);
        app.handle_key_event(key(KeyCode::PageUp));
        assert_eq!(app.scroll_offset, 10);
        app.handle_key_event(key(KeyCode::PageDown));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn test_f1_toggles_help() {
        let mut app = make_app();
        assert!(!app.show_help);
        app.handle_key_event(key(KeyCode::F(1)));
        assert!(app.show_help);
        app.handle_key_event(key(KeyCode::F(1)));
        assert!(!app.show_help);
    }

    #[test]
    fn test_stats_dialog_keys_switch_tab_and_close() {
        let mut app = make_app();
        app.stats_dialog.visible = true;

        app.handle_key_event(key(KeyCode::Right));
        assert_eq!(app.stats_dialog.tab, StatsTab::DailyTokens);

        app.handle_key_event(key(KeyCode::Esc));
        assert!(!app.stats_dialog.visible);
    }

    #[test]
    fn test_mcp_view_keys_search_and_close() {
        let mut app = make_app();
        app.mcp_view.open(vec![McpServerView {
            name: "filesystem".to_string(),
            transport: "stdio".to_string(),
            status: McpViewStatus::Connected,
            tool_count: 1,
            resource_count: 0,
            prompt_count: 0,
            resources: vec![],
            prompts: vec![],
            error_message: None,
            tools: vec![McpToolView {
                name: "read_file".to_string(),
                server: "filesystem".to_string(),
                description: "Read a file".to_string(),
                input_schema: None,
            }],
        }]);
        app.mcp_view.switch_pane();

        app.handle_key_event(key(KeyCode::Char('r')));
        assert_eq!(app.mcp_view.tool_search, "");
        assert_eq!(
            app.status_message.as_deref(),
            Some("Reconnecting MCP runtime...")
        );
        app.handle_key_event(key(KeyCode::Char('f')));
        assert_eq!(app.mcp_view.tool_search, "f");

        app.handle_key_event(key(KeyCode::Backspace));
        assert_eq!(app.mcp_view.tool_search, "");

        app.handle_key_event(key(KeyCode::Esc));
        assert!(!app.mcp_view.visible);
    }

    #[test]
    fn test_mcp_view_render_shows_resources_and_prompts() {
        let mut state = McpViewState::new();
        state.open(vec![McpServerView {
            name: "filesystem".to_string(),
            transport: "stdio".to_string(),
            status: McpViewStatus::Connected,
            tool_count: 1,
            resource_count: 2,
            prompt_count: 1,
            resources: vec!["workspace-root".to_string(), "project-config".to_string()],
            prompts: vec!["summarize-workspace".to_string()],
            error_message: None,
            tools: vec![McpToolView {
                name: "read_file".to_string(),
                server: "filesystem".to_string(),
                description: "Read a file".to_string(),
                input_schema: None,
            }],
        }]);
        state.switch_pane();
        state.switch_pane();

        let area = Rect { x: 0, y: 0, width: 120, height: 30 };
        let mut buf = Buffer::empty(area);
        mcp_view::render_mcp_view(&state, area, &mut buf);
        let rendered = buf
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(rendered.contains("2 res"));
        assert!(rendered.contains("1 prompts"));
    }

    #[test]
    fn test_mcp_view_auth_key_queues_selected_server_panel_auth() {
        let mut app = make_app();
        app.mcp_view.open(vec![McpServerView {
            name: "mcphub".to_string(),
            transport: "http".to_string(),
            status: McpViewStatus::Connected,
            tool_count: 1,
            resource_count: 0,
            prompt_count: 0,
            resources: vec![],
            prompts: vec![],
            error_message: None,
            tools: vec![McpToolView {
                name: "read_file".to_string(),
                server: "mcphub".to_string(),
                description: "Read a file".to_string(),
                input_schema: None,
            }],
        }]);

        let submit = app.handle_key_event(key(KeyCode::Char('a')));
        assert!(!submit);
        assert_eq!(app.prompt_input.text, "");
        assert_eq!(app.take_pending_mcp_panel_auth().as_deref(), Some("mcphub"));
        assert!(!app.mcp_view.visible);
        assert_eq!(app.mcp_view.tool_search, "");
    }

    #[test]
    fn test_mcp_view_auth_key_with_no_servers_is_safe_noop() {
        let mut app = make_app();
        app.mcp_view.open(vec![]);

        let submit = app.handle_key_event(key(KeyCode::Char('a')));
        assert!(!submit);
        assert!(app.mcp_view.visible);
        assert_eq!(app.prompt_input.text, "");
        assert!(app.take_pending_mcp_panel_auth().is_none());
    }

    #[test]
    fn test_mcp_view_auth_key_only_works_in_servers_pane() {
        let mut app = make_app();
        app.mcp_view.open(vec![McpServerView {
            name: "mcphub".to_string(),
            transport: "http".to_string(),
            status: McpViewStatus::Connected,
            tool_count: 1,
            resource_count: 0,
            prompt_count: 0,
            resources: vec![],
            prompts: vec![],
            error_message: None,
            tools: vec![McpToolView {
                name: "read_file".to_string(),
                server: "mcphub".to_string(),
                description: "Read a file".to_string(),
                input_schema: None,
            }],
        }]);
        app.mcp_view.switch_pane();

        let submit = app.handle_key_event(key(KeyCode::Char('a')));
        assert!(!submit);
        assert!(app.mcp_view.visible);
        assert_eq!(app.mcp_view.tool_search, "a");
        assert!(app.take_pending_mcp_panel_auth().is_none());
    }

    #[test]
    fn test_take_pending_mcp_panel_auth_clears_after_read() {
        let mut app = make_app();
        app.pending_mcp_panel_auth = Some("mcphub".to_string());

        assert_eq!(app.take_pending_mcp_panel_auth().as_deref(), Some("mcphub"));
        assert!(app.take_pending_mcp_panel_auth().is_none());
    }

    #[test]
    fn test_message_renderer_includes_tool_use_and_thinking_blocks() {
        let msg = claurst_core::types::Message::assistant_blocks(vec![
            ContentBlock::Thinking {
                thinking: "reasoning".to_string(),
                signature: "sig".to_string(),
            },
            ContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({ "path": "README.md" }),
                thought_signature: None,
            },
            ContentBlock::Text {
                text: "Done".to_string(),
            },
        ]);

        let rendered = messages::render_message(&msg, &messages::RenderContext::default());
        let text = rendered
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.clone()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Thinking"));
        assert!(text.contains("read_file"));
        assert!(text.contains("Done"));
    }

    #[test]
    fn test_message_renderer_includes_tool_result_errors() {
        let msg = claurst_core::types::Message::user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_1".to_string(),
            content: ToolResultContent::Text("boom".to_string()),
            is_error: Some(true),
        }]);

        let rendered = messages::render_message(&msg, &messages::RenderContext::default());
        let text = rendered
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.clone()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Error"));
        assert!(text.contains("boom"));
    }

    // ---- QueryEvent handling --------------------------------------------

    #[test]
    fn test_handle_status_event() {
        let mut app = make_app();
        app.handle_query_event(claurst_query::QueryEvent::Status("working".to_string()));
        assert_eq!(app.status_message.as_deref(), Some("working"));
    }

    #[test]
    fn test_handle_error_event() {
        let mut app = make_app();
        app.is_streaming = true;
        app.handle_query_event(claurst_query::QueryEvent::Error("oops".to_string()));
        assert!(!app.is_streaming);
        assert_eq!(app.messages.len(), 1);
        assert!(app.messages[0].get_all_text().contains("oops"));
    }

    #[test]
    fn test_handle_tool_start_and_end() {
        let mut app = make_app();
        app.handle_query_event(claurst_query::QueryEvent::ToolStart {
            tool_name: "Bash".to_string(),
            tool_id: "t1".to_string(),
            input_json: r#"{"command":"ls -la"}"#.to_string(),
        });
        assert_eq!(app.tool_use_blocks.len(), 1);
        assert_eq!(app.tool_use_blocks[0].turn_index, None);
        assert_eq!(app.tool_use_blocks[0].status, ToolStatus::Running);

        app.handle_query_event(claurst_query::QueryEvent::ToolEnd {
            tool_name: "Bash".to_string(),
            tool_id: "t1".to_string(),
            result: "output".to_string(),
            is_error: false,
        });
        assert_eq!(app.tool_use_blocks[0].status, ToolStatus::Done);
    }

    #[test]
    fn test_handle_tool_end_error() {
        let mut app = make_app();
        app.tool_use_blocks.push(ToolUseBlock {
            id: "t2".to_string(),
            name: "Read".to_string(),
            turn_index: None,
            status: ToolStatus::Running,
            output_preview: None,
            input_json: r#"{"file_path":"foo.rs"}"#.to_string(),
        });
        app.handle_query_event(claurst_query::QueryEvent::ToolEnd {
            tool_name: "Read".to_string(),
            tool_id: "t2".to_string(),
            result: "file not found".to_string(),
            is_error: true,
        });
        assert_eq!(app.tool_use_blocks[0].status, ToolStatus::Error);
        assert!(app.status_message.is_some());
    }

    #[test]
    fn test_turn_complete_flushes_streaming_text() {
        let mut app = make_app();
        app.is_streaming = true;
        app.streaming_text = "partial response".to_string();
        app.handle_query_event(claurst_query::QueryEvent::TurnComplete {
            turn: 1,
            stop_reason: "end_turn".to_string(),
            usage: None,
        });
        assert!(!app.is_streaming);
        assert!(app.streaming_text.is_empty());
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].get_all_text(), "partial response");
    }

    #[test]
    fn test_turn_complete_flushes_streaming_thinking_into_blocks() {
        let mut app = make_app();
        app.is_streaming = true;
        app.streaming_thinking = "outline the fix".to_string();
        app.handle_query_event(claurst_query::QueryEvent::TurnComplete {
            turn: 1,
            stop_reason: "end_turn".to_string(),
            usage: None,
        });

        let blocks = app.messages[0].content_blocks();
        assert!(matches!(
            blocks.first(),
            Some(ContentBlock::Thinking { thinking, .. }) if thinking == "outline the fix"
        ));
    }

    #[test]
    fn test_render_app_transcript_uses_turn_metadata_without_legacy_glyph() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();
        app.push_message(claurst_core::types::Message::user("hello".to_string()));
        app.push_message(claurst_core::types::Message::assistant("hi there".to_string()));
        // Mode/model/duration moved to the status line, so the turn-metadata
        // line (the ▣ glyph) now renders only for interrupted turns. Mark this
        // turn interrupted to exercise that metadata path.
        if let Some(meta) = app.turn_metadata.first_mut() {
            meta.interrupted = true;
        }

        terminal
            .draw(|frame| crate::render::render_app(frame, &app))
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        // Turn metadata uses the ▣ glyph, never the legacy ◆.
        assert!(!rendered.contains("◆"));
        assert!(rendered.contains("\u{25a3}"));
    }

    // ---- HistorySearch --------------------------------------------------

    #[test]
    fn test_history_search_matches() {
        let history = vec![
            "git commit".to_string(),
            "git push".to_string(),
            "cargo build".to_string(),
        ];
        let mut hs = HistorySearch::new();
        hs.query = "git".to_string();
        hs.update_matches(&history);
        assert_eq!(hs.matches.len(), 2);
        assert_eq!(hs.matches[0], 0);
        assert_eq!(hs.matches[1], 1);
    }

    #[test]
    fn test_history_search_no_matches() {
        let history = vec!["hello".to_string()];
        let mut hs = HistorySearch::new();
        hs.query = "xyz".to_string();
        hs.update_matches(&history);
        assert!(hs.matches.is_empty());
    }

    // ---- PermissionRequest --------------------------------------------

    #[test]
    fn test_permission_request_standard() {
        let pr = PermissionRequest::standard(
            "tu1".to_string(),
            "Bash".to_string(),
            "Run a shell command".to_string(),
        );
        assert_eq!(pr.options.len(), 4);
        assert_eq!(pr.options[0].key, 'y');
        assert_eq!(pr.options[1].key, 'Y');
        assert_eq!(pr.options[2].key, 'p');
        assert_eq!(pr.options[3].key, 'n');
    }
}

