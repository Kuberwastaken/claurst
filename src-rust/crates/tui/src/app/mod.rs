//! App state struct and main event loop.

mod commands;
mod keys;
mod messages;
mod mouse;
mod prompt;
mod providers;
mod run;
#[cfg(test)]
mod tests;
mod turns;
mod types;
mod views;

pub use types::{
    ContextMenuKind, DisplayMessage, FocusTarget, HistorySearch,
    RecentSession, SystemAnnotation, SystemMessageStyle, ToolStatus,
    ToolUseBlock, TurnMetadata, recent_session_label,
};

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};

use claurst_core::config::{Config, Settings, Theme};
use claurst_core::cost::CostTracker;
use claurst_core::file_history::FileHistory;
use claurst_core::keybindings::{KeybindingResolver, UserKeybindings};
use claurst_core::types::Message;
use crate::agents_view::AgentsMenuState;
use crate::bridge_state::BridgeConnectionState;
use crate::context_viz::ContextVizState;
use crate::dialog_select::{DialogSelectState, SelectItem};
use crate::dialogs::{McpApprovalDialogState, PermissionRequest};
use crate::diff_viewer::DiffViewerState;
use crate::export_dialog::ExportDialogState;
use crate::import_config_dialog::ImportConfigDialogState;
use crate::mcp_view::McpViewState;
use crate::model_picker::{EffortLevel, ModelPickerState};
use crate::notifications::NotificationQueue;
use crate::overlays::{
    GlobalSearchState, HelpOverlay, HistorySearchOverlay,
    MessageSelectorOverlay, RewindFlowOverlay,
};
use crate::plugin_views::PluginHintBanner;
use crate::prompt_input::PromptInputState;
use crate::session_browser::SessionBrowserState;
use crate::settings_screen::SettingsScreen;
use crate::stats_dialog::StatsDialogState;
use crate::tasks_overlay::TasksOverlay;
use crate::theme_screen::ThemeScreen;
use ratatui::style::Color;
use commands::{PROMPT_SLASH_COMMANDS, help_overlay_entries};
use providers::{import_config_picker_items, provider_picker_items};
use types::{ContextMenuState, GoToLineDialog};

/// Attempt to copy text to the system clipboard using platform CLI tools.
/// Returns true if successful.
///
/// Crate-level public API — re-exported by `lib.rs` (`pub use app::try_copy_to_clipboard`).
pub fn try_copy_to_clipboard(text: &str) -> bool {
    // Windows
    #[cfg(target_os = "windows")]
    {
        use std::io::Write;
        if let Ok(mut child) = std::process::Command::new("clip")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
                drop(stdin);
            }
            return child.wait().map(|s| s.success()).unwrap_or(false);
        }
    }
    // macOS
    #[cfg(target_os = "macos")]
    {
        use std::io::Write;
        if let Ok(mut child) = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            return child.wait().map(|s| s.success()).unwrap_or(false);
        }
    }
    // Linux / Wayland / X11
    #[cfg(target_os = "linux")]
    {
        use std::io::Write;
        for cmd in &["wl-copy", "xclip -selection clipboard", "xsel --clipboard --input"] {
            let parts: Vec<&str> = cmd.split_whitespace().collect();
            if let Some((prog, args)) = parts.split_first() {
                if let Ok(mut child) = std::process::Command::new(prog)
                    .args(args)
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                {
                    if let Some(stdin) = child.stdin.as_mut() {
                        let _ = stdin.write_all(text.as_bytes());
                    }
                    if child.wait().map(|s| s.success()).unwrap_or(false) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// The top-level TUI application.
pub struct App {
    // Core state
    pub config: Config,
    pub cost_tracker: Arc<CostTracker>,
    pub messages: Vec<Message>,
    /// Combined display list kept in sync with `messages`: real conversation turns
    /// plus injected system annotations. Used by the renderer so it can iterate
    /// a single sequence instead of merging two lists on every frame.
    pub display_messages: Vec<DisplayMessage>,
    /// Synthetic system annotations interleaved between real messages at render time.
    pub system_annotations: Vec<SystemAnnotation>,
    pub input: String,
    pub prompt_input: PromptInputState,
    pub input_history: Vec<String>,
    pub history_index: Option<usize>,
    pub scroll_offset: usize,
    pub is_streaming: bool,
    pub streaming_text: String,
    pub streaming_thinking: String,
    pub status_message: Option<String>,
    /// Randomly chosen thinking verb shown next to the spinner while streaming.
    pub spinner_verb: Option<String>,
    pub should_exit: bool,
    pub show_help: bool,
    /// Whether the terminal speaks the kitty keyboard protocol (progressive
    /// keyboard enhancement is active). When `false` — e.g. Windows conhost /
    /// CMD / legacy PowerShell and most default terminals — printable keys
    /// arrive as their final, layout-correct character (Shift already applied),
    /// so we must NOT re-apply a US-QWERTY shift map to them (issue #183: typing
    /// `/` produced `?`). When `true`, the terminal reports the unshifted base
    /// key plus a SHIFT modifier, so we normalize it ourselves. Defaults to
    /// `true`; the run loop overwrites it with the detected value once the
    /// terminal has been initialized.
    pub kitty_keyboard_active: bool,

    // Extended state
    pub tool_use_blocks: Vec<ToolUseBlock>,
    pub permission_request: Option<PermissionRequest>,
    pub frame_count: u64,
    pub token_count: u32,
    /// Maximum token budget (from env var or model context window) — P2 feature flag
    pub token_budget: Option<u32>,
    pub cost_usd: f64,
    pub model_name: String,
    /// Whether the app has valid API credentials configured.
    /// False = show the in-TUI provider setup dialog on startup.
    pub has_credentials: bool,
    /// Current effort level (controls extended-thinking budget_tokens).
    pub effort_level: EffortLevel,
    /// Whether fast mode is currently active (model locked to FAST_MODE_MODEL).
    pub fast_mode: bool,
    /// Current agent mode name: "build", "plan".
    pub agent_mode: Option<String>,
    /// Accent color derived from the current agent mode.
    /// Build = pink, Plan = blue.
    pub accent_color: Color,
    /// Set by `cycle_agent_mode` so the main loop can update the query config
    /// and tool list to match the newly-selected agent.
    pub agent_mode_changed: bool,
    pub agent_status: Vec<(String, String)>,
    pub history_search: Option<HistorySearch>,
    pub keybindings: KeybindingResolver,

    // Cursor position within input (byte offset)
    pub cursor_pos: usize,

    // ---- Scrollback / auto-scroll -----------------------------------------

    /// When `true`, the message pane follows the latest messages automatically.
    pub auto_scroll: bool,
    /// Count of messages that arrived while the user was scrolled up.
    pub new_messages_while_scrolled: usize,

    // ---- Token warning tracking -------------------------------------------

    /// Which threshold (0 = none, 80, 95, 100) was last notified so we only
    /// show each banner once.
    pub token_warning_threshold_shown: u8,

    // ---- Session timing ---------------------------------------------------

    /// Instant the session started (used for elapsed-time in the status bar).
    pub session_start: std::time::Instant,
    /// Current Rustle pose for rendering (updated each frame).
    pub rustle_current_pose: crate::rustle::RustlePose,
    /// Temporary Rustle pose override (e.g. look-down on Tab). Reverts to
    /// default after this instant passes.
    pub rustle_pose_until: Option<std::time::Instant>,
    /// The temporary pose to show until `rustle_pose_until`.
    pub rustle_temp_pose: Option<crate::rustle::RustlePose>,
    /// Frame counter at which the next random eye-shift should fire.
    pub rustle_next_blink: u64,
    /// Instant the current turn's streaming began (reset each time streaming starts).
    pub turn_start: Option<std::time::Instant>,
    /// Elapsed time string for the last completed turn, e.g. "2m 5s".
    pub last_turn_elapsed: Option<String>,
    /// Past-tense verb shown after turn completes, e.g. "Worked" / "Baked".
    pub last_turn_verb: Option<&'static str>,
    /// Per-user turn snapshots used by the transcript renderer.
    pub turn_metadata: Vec<TurnMetadata>,
    /// Incremented whenever transcript-visible state changes so rendering can
    /// reuse cached layout between keystrokes.
    pub transcript_version: Cell<u64>,

    // ---- New overlay / notification fields --------------------------------

    /// Full-screen help overlay (? / F1).
    pub help_overlay: HelpOverlay,
    /// Ctrl+R history search overlay.
    pub history_search_overlay: HistorySearchOverlay,
    /// Global ripgrep search / quick-open overlay.
    pub global_search: GlobalSearchState,
    /// Message selector used by /rewind.
    pub message_selector: MessageSelectorOverlay,
    /// Multi-step rewind flow overlay.
    pub rewind_flow: RewindFlowOverlay,
    /// Bridge connection state.
    pub bridge_state: BridgeConnectionState,
    /// Active notification queue.
    pub notifications: NotificationQueue,
    /// Scroll offset for error modal text (in lines).
    pub error_modal_scroll_offset: usize,
    /// Plugin hint banners.
    pub plugin_hints: Vec<PluginHintBanner>,
    /// Optional session title shown in the status bar.
    pub session_title: Option<String>,
    /// Remote session URL (set when bridge connects; readable by commands).
    pub remote_session_url: Option<String>,
    /// Live MCP manager snapshot source when available.
    pub mcp_manager: Option<Arc<claurst_mcp::McpManager>>,
    /// Queued request for a real MCP reconnect from the interactive loop.
    pub pending_mcp_reconnect: bool,
    /// Set after an in-session provider connection (e.g. a Claude Pro/Max OAuth
    /// login) so the main loop re-resolves credentials and swaps in a fresh
    /// client + provider registry. Without it the session keeps the client built
    /// at startup, which for a fresh OAuth login still has no usable credential.
    pub pending_provider_reload: bool,
    /// Pending MCP panel-auth request for the interactive loop.
    pub pending_mcp_panel_auth: Option<String>,
    /// Shared file-history service used for turn diff reconstruction.
    pub file_history: Option<Arc<parking_lot::Mutex<FileHistory>>>,
    /// Shared query-loop turn counter for turn-local diff reconstruction.
    pub current_turn: Option<Arc<std::sync::atomic::AtomicUsize>>,

    // ---- Visual mode indicators -------------------------------------------

    /// Plan mode — input border turns blue, [PLAN] shown in status bar.
    pub plan_mode: bool,
    /// "While you were away" summary text shown on the welcome screen.
    pub away_summary: Option<String>,
    /// When streaming stalled (used to turn the spinner red after 3 s).
    pub stall_start: Option<std::time::Instant>,

    // ---- Settings / theme / privacy screens --------------------------------

    /// Full-screen tabbed settings screen (/config, /settings).
    pub settings_screen: SettingsScreen,
    /// Theme picker overlay (/theme).
    pub theme_screen: ThemeScreen,
    /// Token/cost analytics dialog.
    pub stats_dialog: StatsDialogState,
    /// MCP server browser and tool detail view.
    pub mcp_view: McpViewState,
    /// Agent definitions and active agent status overlay.
    pub agents_menu: AgentsMenuState,
    /// Diff viewer overlay.
    pub diff_viewer: DiffViewerState,
    /// Read-only viewer for [Pasted text #N ...] placeholders.
    pub paste_viewer: crate::paste_viewer::PasteViewer,
    /// Session-quality feedback survey overlay.
    pub feedback_survey: crate::feedback_survey::FeedbackSurveyState,
    /// Memory file selector overlay (AGENTS.md browser).
    pub memory_file_selector: crate::memory_file_selector::MemoryFileSelectorState,
    /// Read-only hooks configuration browser.
    pub hooks_config_menu: crate::hooks_config_menu::HooksConfigMenuState,
    /// Overage credit upsell banner.
    pub overage_upsell: crate::overage_upsell::OverageCreditUpsellState,
    /// Voice mode availability notice.
    pub voice_mode_notice: crate::voice_mode_notice::VoiceModeNoticeState,
    /// Desktop app upsell startup dialog.
    pub desktop_upsell: crate::desktop_upsell_startup::DesktopUpsellStartupState,
    /// Startup error dialog for malformed settings.json or AGENTS.md.
    pub invalid_config_dialog: crate::invalid_config_dialog::InvalidConfigDialogState,
    /// Memory update notification banner.
    pub memory_update_notification: crate::memory_update_notification::MemoryUpdateNotificationState,
    /// MCP elicitation dialog (form requested by an MCP server).
    pub elicitation: crate::elicitation_dialog::ElicitationDialogState,
    /// Model picker overlay (/model command).
    pub model_picker: ModelPickerState,
    /// Session browser overlay (/session, /resume, /rename, /export).
    pub session_browser: SessionBrowserState,
    /// Session branching overlay (Ctrl+B) — create and switch branches.
    pub session_branching: crate::session_branching::SessionBranchingState,
    /// Task progress overlay (Ctrl+T) — shows task status with toggle capability.
    pub tasks_overlay: TasksOverlay,
    /// Export format picker dialog (/export).
    pub export_dialog: ExportDialogState,
    /// Context window / rate limit visualization overlay (/context).
    pub context_viz: ContextVizState,
    /// MCP server approval dialog.
    pub mcp_approval: McpApprovalDialogState,
    /// Project-defined MCP servers awaiting the user's approval decision.
    /// Populated at startup with the gated (untrusted) project servers; the
    /// main loop shows one approval dialog at a time, draining this queue.
    pub mcp_pending_project: std::collections::VecDeque<claurst_core::config::McpServerConfig>,
    /// The project MCP server currently shown in the approval dialog, if any.
    pub mcp_prompting: Option<claurst_core::config::McpServerConfig>,
    /// Fingerprints of project MCP servers approved for THIS session only
    /// (the "Allow this session" choice). Not persisted to disk.
    pub mcp_session_trusted: std::collections::HashSet<String>,
    /// Project root used to key persistent MCP trust approvals.
    pub mcp_project_root: Option<std::path::PathBuf>,
    /// Go to Line dialog (Ctrl+G in message pane).
    pub go_to_line_dialog: GoToLineDialog,
    /// Bypass-permissions startup confirmation dialog.
    /// Shown at startup when --dangerously-skip-permissions was passed.
    /// User must explicitly accept or the session exits.
    pub bypass_permissions_dialog: crate::bypass_permissions_dialog::BypassPermissionsDialogState,
    /// Whether the bypass-permissions dialog has been shown this session.
    pub bypass_permissions_dialog_shown: bool,
    /// File injection warning dialog.
    /// Shown when oversized or binary files are detected in @refs.
    pub file_injection_dialog: crate::file_injection_dialog::FileInjectionDialogState,
    /// When true, the next file injection size check uses limit 0 (no limit),
    /// letting files that were "allowed" through the warning dialog be injected.
    pub file_injection_force: bool,
    /// First-launch onboarding welcome dialog.
    pub onboarding_dialog: crate::onboarding_dialog::OnboardingDialogState,
    /// Effort-level picker (/effort with no args).
    pub effort_picker: crate::effort_picker::EffortPickerState,
    /// API key input dialog (opened from /connect for key-based providers).
    pub key_input_dialog: crate::key_input_dialog::KeyInputDialogState,
    /// Custom provider dialog for URL + API key input.
    pub custom_provider_dialog: crate::custom_provider_dialog::CustomProviderDialogState,
    /// "Free" composite-provider setup dialog (warning + 2 API keys).
    pub free_mode_dialog: crate::free_mode_dialog::FreeModeDialogState,
    /// Device code / browser auth dialog (GitHub Copilot device flow, Anthropic OAuth).
    pub device_auth_dialog: crate::device_auth_dialog::DeviceAuthDialogState,
    /// When set, the main loop should spawn the async auth task for this provider.
    pub device_auth_pending: Option<String>,
    /// Shared provider registry for dynamic model fetching.
    pub provider_registry: Option<std::sync::Arc<claurst_api::ProviderRegistry>>,
    /// Model registry populated from models.dev — single source of truth for
    /// all provider models shown in the `/model` picker.
    pub model_registry: claurst_api::ModelRegistry,
    /// When `true`, the main event loop should spawn an async task to fetch
    /// the model list from the current provider's `list_models()` API.
    pub model_picker_fetch_pending: bool,
    /// The provider ID that the model picker was opened for (used when the
    /// fetch is triggered from /connect before the provider is activated).
    pub model_picker_provider_id: Option<String>,
    /// When `true`, the main event loop should spawn an async task to load
    /// the session list from disk and populate the session browser.
    pub session_list_pending: bool,
    /// Receiver for background session-list results.
    pub session_list_rx:
        Option<tokio::sync::mpsc::Receiver<Vec<crate::session_browser::SessionEntry>>>,
    /// The most-recent sessions shown in the welcome screen's "Recent activity"
    /// list. Populated once from disk via the background loader below; empty
    /// until it resolves (or when there are genuinely no sessions).
    pub recent_sessions: Vec<RecentSession>,
    /// When `true`, the main event loop should spawn a one-shot async task to
    /// load recent sessions from disk (mirrors `session_list_pending`). Set once
    /// at startup and cleared when the load is kicked off, so we never re-list
    /// every frame.
    pub recent_sessions_pending: bool,
    /// Receiver for the background recent-sessions load.
    pub recent_sessions_rx: Option<tokio::sync::mpsc::Receiver<Vec<RecentSession>>>,
    /// Credential store for provider API keys and OAuth tokens.
    pub auth_store: claurst_core::AuthStore,
    /// Messages typed by the user while a query was streaming. They will be
    /// auto-submitted in order once the current turn completes (issue #149).
    pub queued_messages: std::collections::VecDeque<String>,
    /// When `true`, the main loop will inject a synthetic Enter event on the
    /// next iteration to dequeue and submit the next queued message.
    pub pending_auto_submit: bool,
    /// Connect-a-provider dialog (/connect command).
    pub connect_dialog: DialogSelectState,
    /// Import-config source picker (/import-config command).
    pub import_config_picker: DialogSelectState,
    /// Import-config preview and confirmation dialog.
    pub import_config_dialog: ImportConfigDialogState,
    /// Ctrl+K command palette overlay.
    pub command_palette: DialogSelectState,
    /// Whether Claurst was launched from the user's home directory.
    /// Shown as a startup notice: "Note: You have launched Claurst in your home directory…"
    pub home_dir_warning: bool,
    /// Output style: "auto" | "stream" | "verbose".
    pub output_style: String,
    /// PR number for the current branch (None if not in a PR context).
    pub pr_number: Option<u32>,
    /// PR URL for the current branch.
    pub pr_url: Option<String>,
    /// PR review state: "approved", "changes_requested", "review_required", etc.
    pub pr_state: Option<String>,
    /// Current working directory path.
    pub current_dir: Option<String>,
    /// Current git branch name.
    pub git_branch: Option<String>,
    /// Count of in-progress background tasks (drives the footer pill).
    pub background_task_count: usize,
    /// Background task status text shown in footer pill.
    pub background_task_status: Option<String>,
    /// External status line command output (from CLAUDE_STATUS_COMMAND).
    pub status_line_override: Option<String>,
    /// Whether auto-compact is enabled (from settings).
    pub auto_compact_enabled: bool,
    /// Context threshold (0-100) at which to auto-compact.
    pub auto_compact_threshold: u8,
    /// Guard to prevent re-triggering auto-compact while one is in flight.
    pub auto_compact_running: bool,

    // ---- Voice hold-to-talk ------------------------------------------------

    /// The global voice recorder, Some when voice is enabled in config.
    pub voice_recorder: Option<Arc<Mutex<claurst_core::voice::VoiceRecorder>>>,
    /// True while recording is active (Alt+V toggled on).
    pub voice_recording: bool,
    /// Receiver for VoiceEvent messages produced by the recorder task.
    pub voice_event_rx: Option<tokio::sync::mpsc::Receiver<claurst_core::voice::VoiceEvent>>,
    /// A single key event that was drained from the queue during paste-burst
    /// detection but wasn't part of the burst (e.g. a modifier key that stopped
    /// the burst). Replayed at the top of the next loop iteration.
    pub pending_key: Option<crossterm::event::KeyEvent>,
    /// Receiver for model-list results fetched in the background when the
    /// /model picker opens.  Drained each frame so models appear as soon as
    /// the fetch completes.
    pub model_fetch_rx:
        Option<tokio::sync::mpsc::Receiver<Result<Vec<crate::model_picker::ModelEntry>, ()>>>,
    /// Receiver for `UserQuestionEvent`s produced by the AskUserQuestion tool.
    /// When a question arrives, `ask_user_dialog` is populated and shown.
    pub user_question_rx:
        Option<tokio::sync::mpsc::UnboundedReceiver<claurst_tools::UserQuestionEvent>>,
    /// State for the model-initiated ask-user question dialog.
    pub ask_user_dialog: crate::ask_user_dialog::AskUserDialogState,

    // ---- Context window & rate limit info ----------------------------------

    /// Total context window size for the current model (tokens).
    pub context_window_size: u64,
    /// How many tokens are currently used in the context window.
    pub context_used_tokens: u64,
    /// Rate limit info — 5-hour window usage percentage (0–100).
    pub rate_limit_5h_pct: Option<f32>,
    /// Rate limit info — 7-day window usage percentage (0–100).
    pub rate_limit_7day_pct: Option<f32>,
    /// Active worktree name (if in a worktree).
    pub worktree_name: Option<String>,
    /// Active worktree branch (if in a worktree).
    pub worktree_branch: Option<String>,
    /// Agent type badge: "agent" | "coordinator" | "subagent".
    pub agent_type_badge: Option<String>,
    /// Goal badge string shown in the footer, e.g. "active · 5m · 3 turns".
    /// None when no goal is active. Updated by the REPL after each turn.
    pub active_goal_badge: Option<String>,

    // ---- Thinking block expansion state ----------------------------------
    /// Set of thinking block content hashes that are expanded.
    pub thinking_expanded: std::collections::HashSet<u64>,
    /// The message pane area from the last render frame (used for mouse hit testing).
    pub last_msg_area: Cell<ratatui::layout::Rect>,
    /// The frame region that supports text selection.
    pub last_selectable_area: Cell<ratatui::layout::Rect>,
    /// The prompt input area from the last render frame (used for focus routing).
    pub last_input_area: Cell<ratatui::layout::Rect>,
    /// The footer's right column area (where tips are shown) from the last render.
    pub footer_right_column_area: Cell<ratatui::layout::Rect>,
    /// Which area of the TUI currently has keyboard focus.
    pub focus: FocusTarget,
    /// Maps virtual_row_index → thinking_block_hash for click detection.
    pub thinking_row_map: RefCell<std::collections::HashMap<u16, u64>>,
    /// Maps screen row → transcript message index for right-click hit testing.
    pub message_row_map: RefCell<std::collections::HashMap<u16, usize>>,
    /// Total message lines from the last render (used for virtual row mapping).
    pub total_message_lines: Cell<usize>,
    /// Scroll offset from the last render frame (used for selection validation).
    pub last_render_scroll_offset: Cell<u16>,
    /// Maximum `scroll_offset` (lines above the bottom) from the last render.
    /// Written by the renderer, which is the only place the full content height
    /// is known; read back on the next scroll event to clamp `scroll_offset` so
    /// scrolling up past the top can't inflate it unboundedly (#223).
    pub last_max_scroll: Cell<usize>,

    // ---- Text selection state --------------------------------------------
    /// Selection drag anchor (col, row) — set on mouse-down.
    pub selection_anchor: Option<(u16, u16)>,
    /// Selection drag focus (col, row) — updated on mouse-drag / mouse-up.
    pub selection_focus: Option<(u16, u16)>,
    /// Text extracted from the current selection (updated each render frame).
    pub selection_text: RefCell<String>,
    /// Cache of row -> rendered text within the selectable area, refreshed
    /// each frame. Used by double/triple-click word and paragraph detection
    /// (issue #149 follow-up: prior word-boundary detection was a placeholder).
    pub last_row_text: RefCell<std::collections::HashMap<u16, String>>,

    // ---- Keyboard text selection state ----------------------------------
    /// Keyboard selection mode active (entered with `v` on empty prompt).
    pub kb_select_mode: bool,
    /// Cursor row for keyboard selection, as a screen row within
    /// `last_msg_area`. Converted to absolute (col, row) on demand.
    pub kb_cursor_row: u16,

    // ---- Advanced mouse interaction state --------------------------------
    /// Timestamp of the last left mouse click (for double/triple-click detection).
    pub last_click_time: Option<std::time::Instant>,
    /// Position of the last left mouse click (for double/triple-click detection).
    pub last_click_position: Option<(u16, u16)>,
    /// Count of consecutive clicks: 1 = single, 2 = double, 3+ = triple.
    pub click_count: u32,
    /// Context menu state: position and selected index.
    pub context_menu_state: Option<ContextMenuState>,

    // ---- Scroll acceleration state (trackpad feel) -----------------------
    /// Current acceleration multiplier for scroll events.
    scroll_accel: f32,
    /// Timestamp of the last scroll event (for burst detection).
    scroll_last_time: Option<std::time::Instant>,

    // ---- Bash prefix allowlist -------------------------------------------
    /// Command prefixes that have been permanently allowed this session via
    /// the "Allow commands starting with X" option in the bash permission dialog.
    /// Before showing the dialog for a bash command, the first whitespace-delimited
    /// word is checked against this set; a match silently auto-approves the request.
    pub bash_prefix_allowlist: std::collections::HashSet<String>,

    // ---- Auto-update notification ----------------------------------------
    /// If a newer version was found during background update check, this holds
    /// the latest version string (e.g. "0.1.0"). Shown in the footer status bar.
    pub update_available: Option<String>,
    /// Cost breakdown for managed agent sessions: (manager_usd, executors_usd, total_usd).
    pub managed_agent_cost_breakdown: Option<(f64, f64, f64)>,
    /// Whether managed agent mode is currently active.
    pub managed_agents_active: bool,
    /// Timestamp of the first exit key press that showed confirmation (valid for ~2 seconds).
    pub last_exit_key_warning: Option<std::time::Instant>,
    /// Which exit key ('c' or 'd') started the current confirmation sequence.
    pub exit_key_sequence_start: Option<char>,
}

/// Accent color for build mode (default pink).
pub const ACCENT_BUILD: Color = Color::Rgb(233, 30, 99);

/// Accent color for plan mode (blue).
pub const ACCENT_PLAN: Color = Color::Rgb(66, 135, 245);

/// Return the accent color for a given agent mode name.
pub fn accent_for_mode(mode: Option<&str>) -> Color {
    match mode {
        Some("plan") => ACCENT_PLAN,
        _ => ACCENT_BUILD,
    }
}

impl App {
    pub fn new(config: Config, cost_tracker: Arc<CostTracker>) -> Self {
        let model_name = config.effective_model().to_string();
        let user_keybindings = UserKeybindings::load(&Settings::config_dir());
        // Build the model registry up front so user metadata overrides
        // (issue #309) are layered on before the struct owns `config`.
        let model_registry = {
            let mut reg = claurst_api::ModelRegistry::new();
            // Try to load cached models.dev data from disk.
            let cache_path = dirs::cache_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("claurst")
                .join("models.json");
            reg.load_cache(&cache_path);
            reg.apply_model_overrides(&config.model_overrides);
            reg
        };
        Self {
            config,
            cost_tracker,
            messages: Vec::new(),
            display_messages: Vec::new(),
            system_annotations: Vec::new(),
            input: String::new(),
            prompt_input: PromptInputState::new(),
            input_history: Vec::new(),
            history_index: None,
            scroll_offset: 0,
            is_streaming: false,
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            status_message: None,
            spinner_verb: None,
            should_exit: false,
            show_help: false,
            kitty_keyboard_active: true,
            tool_use_blocks: Vec::new(),
            permission_request: None,
            frame_count: 0,
            token_count: 0,
            token_budget: Self::load_token_budget(),
            cost_usd: 0.0,
            model_name,
            has_credentials: true, // overridden by caller when no key is configured
            effort_level: EffortLevel::Medium,
            fast_mode: false,
            agent_mode: None,
            agent_mode_changed: false,
            accent_color: ACCENT_BUILD,
            agent_status: Vec::new(),
            history_search: None,
            keybindings: KeybindingResolver::new(&user_keybindings),
            cursor_pos: 0,
            auto_scroll: true,
            new_messages_while_scrolled: 0,
            token_warning_threshold_shown: 0,
            session_start: std::time::Instant::now(),
            rustle_current_pose: crate::rustle::RustlePose::Default,
            rustle_pose_until: None,
            rustle_temp_pose: None,
            rustle_next_blink: 200 + (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos() as u64 % 300),
            turn_start: None,
            last_turn_elapsed: None,
            last_turn_verb: None,
            turn_metadata: Vec::new(),
            transcript_version: Cell::new(0),
            help_overlay: {
                let mut overlay = HelpOverlay::new();
                overlay.populate_from_commands(help_overlay_entries());
                overlay
            },
            history_search_overlay: HistorySearchOverlay::new(),
            global_search: GlobalSearchState::default(),
            message_selector: MessageSelectorOverlay::new(),
            rewind_flow: RewindFlowOverlay::new(),
            bridge_state: BridgeConnectionState::Disconnected,
            notifications: NotificationQueue::new(),
            error_modal_scroll_offset: 0,
            plugin_hints: Vec::new(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            pending_mcp_reconnect: false,
            pending_provider_reload: false,
            pending_mcp_panel_auth: None,
            file_history: None,
            current_turn: None,
            plan_mode: false,
            away_summary: None,
            stall_start: None,
            settings_screen: SettingsScreen::new(),
            theme_screen: ThemeScreen::new(),
            stats_dialog: StatsDialogState::new(),
            mcp_view: McpViewState::new(),
            agents_menu: AgentsMenuState::new(),
            diff_viewer: DiffViewerState::new(),
            paste_viewer: crate::paste_viewer::PasteViewer::default(),
            feedback_survey: crate::feedback_survey::FeedbackSurveyState::new(),
            memory_file_selector: crate::memory_file_selector::MemoryFileSelectorState::new(),
            hooks_config_menu: crate::hooks_config_menu::HooksConfigMenuState::new(),
            overage_upsell: crate::overage_upsell::OverageCreditUpsellState::new(),
            voice_mode_notice: crate::voice_mode_notice::VoiceModeNoticeState::new(),
            desktop_upsell: crate::desktop_upsell_startup::DesktopUpsellStartupState::new(),
            invalid_config_dialog: crate::invalid_config_dialog::InvalidConfigDialogState::new(),
            memory_update_notification: crate::memory_update_notification::MemoryUpdateNotificationState::new(),
            elicitation: crate::elicitation_dialog::ElicitationDialogState::new(),
            model_picker: ModelPickerState::new(),
            session_browser: SessionBrowserState::new(),
            session_branching: crate::session_branching::SessionBranchingState::new(),
            tasks_overlay: TasksOverlay::new(),
            export_dialog: ExportDialogState::new(),
            context_viz: ContextVizState::new(),
            mcp_approval: McpApprovalDialogState::new(),
            mcp_pending_project: std::collections::VecDeque::new(),
            mcp_prompting: None,
            mcp_session_trusted: std::collections::HashSet::new(),
            mcp_project_root: None,
            go_to_line_dialog: GoToLineDialog::new(),
            bypass_permissions_dialog: crate::bypass_permissions_dialog::BypassPermissionsDialogState::new(),
            bypass_permissions_dialog_shown: false,
            file_injection_dialog: crate::file_injection_dialog::FileInjectionDialogState::new(),
            file_injection_force: false,
            onboarding_dialog: crate::onboarding_dialog::OnboardingDialogState::new(),
            effort_picker: crate::effort_picker::EffortPickerState::new(),
            key_input_dialog: crate::key_input_dialog::KeyInputDialogState::new(),
            custom_provider_dialog: crate::custom_provider_dialog::CustomProviderDialogState::new(),
            free_mode_dialog: crate::free_mode_dialog::FreeModeDialogState::new(),
            device_auth_dialog: crate::device_auth_dialog::DeviceAuthDialogState::new(),
            device_auth_pending: None,
            provider_registry: None,
            model_registry,
            model_picker_fetch_pending: false,
            model_picker_provider_id: None,
            session_list_pending: false,
            session_list_rx: None,
            recent_sessions: Vec::new(),
            // Load recent activity once, lazily, on the first run-loop iteration.
            recent_sessions_pending: true,
            recent_sessions_rx: None,
            auth_store: claurst_core::AuthStore::load(),
            queued_messages: std::collections::VecDeque::new(),
            pending_auto_submit: false,
            connect_dialog: DialogSelectState::new("Connect a provider", provider_picker_items()),
            import_config_picker: DialogSelectState::new("Import config", import_config_picker_items()),
            import_config_dialog: ImportConfigDialogState::new(),
            command_palette: {
                let items: Vec<SelectItem> = PROMPT_SLASH_COMMANDS
                    .iter()
                    .map(|(name, desc)| SelectItem {
                        id: format!("/{}", name),
                        title: format!("/{}", name),
                        description: desc.to_string(),
                        category: "Commands".to_string(),
                        badge: None,
                    })
                    .collect();
                DialogSelectState::new("Command Palette", items)
            },
            home_dir_warning: false,
            output_style: "auto".to_string(),
            pr_number: None,
            pr_url: None,
            pr_state: None,
            current_dir: std::env::current_dir().ok().and_then(|p| {
                p.to_str().map(|s| s.to_string())
            }),
            git_branch: claurst_core::git_utils::get_repo_root(
                std::env::current_dir().as_deref().unwrap_or_else(|_| std::path::Path::new("."))
            ).map(|repo_root| claurst_core::git_utils::get_current_branch(&repo_root)),
            background_task_count: 0,
            background_task_status: None,
            status_line_override: None,
            auto_compact_enabled: false,
            auto_compact_threshold: 95,
            auto_compact_running: false,
            voice_recorder: {
                // Check whether voice input has been enabled via the /voice command
                // (stored in ~/.claurst/ui-settings.json).  We also accept
                // CLAURST_VOICE_ENABLED=1 as an override for easier testing.
                let voice_on = std::env::var("CLAURST_VOICE_ENABLED")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false)
                    || {
                        let path = claurst_core::config::Settings::config_dir()
                            .join("ui-settings.json");
                        std::fs::read_to_string(&path)
                            .ok()
                            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                            .and_then(|v| v["voice_enabled"].as_bool())
                            .unwrap_or(false)
                    };
                if voice_on {
                    let recorder = claurst_core::voice::global_voice_recorder();
                    if let Ok(mut r) = recorder.lock() {
                        r.set_enabled(true);
                    }
                    Some(recorder)
                } else {
                    None
                }
            },
            voice_recording: false,
            voice_event_rx: None,
            pending_key: None,
            model_fetch_rx: None,
            user_question_rx: None,
            ask_user_dialog: crate::ask_user_dialog::AskUserDialogState::new(),
            context_window_size: 0,
            context_used_tokens: 0,
            rate_limit_5h_pct: None,
            rate_limit_7day_pct: None,
            worktree_name: None,
            worktree_branch: None,
            agent_type_badge: None,
            active_goal_badge: None,
            thinking_expanded: std::collections::HashSet::new(),
            last_msg_area: Cell::new(ratatui::layout::Rect::default()),
            last_selectable_area: Cell::new(ratatui::layout::Rect::default()),
            last_input_area: Cell::new(ratatui::layout::Rect::default()),
            footer_right_column_area: Cell::new(ratatui::layout::Rect::default()),
            focus: FocusTarget::Input,
            thinking_row_map: RefCell::new(std::collections::HashMap::new()),
            message_row_map: RefCell::new(std::collections::HashMap::new()),
            total_message_lines: Cell::new(0),
            last_render_scroll_offset: Cell::new(0),
            last_max_scroll: Cell::new(0),
            selection_anchor: None,
            selection_focus: None,
            selection_text: RefCell::new(String::new()),
            last_row_text: RefCell::new(std::collections::HashMap::new()),
            kb_select_mode: false,
            kb_cursor_row: 0,
            last_click_time: None,
            last_click_position: None,
            click_count: 0,
            context_menu_state: None,
            scroll_accel: 3.0,
            scroll_last_time: None,
            bash_prefix_allowlist: std::collections::HashSet::new(),
            update_available: None,
            managed_agent_cost_breakdown: None,
            managed_agents_active: false,
            last_exit_key_warning: None,
            exit_key_sequence_start: None,
        }
    }

    /// Load token budget from environment or model defaults.
    /// Returns Some(max_tokens) if available, None otherwise.
    /// Only enabled when the `token_budget` feature flag is active.
    #[cfg(feature = "token_budget")]
    fn load_token_budget() -> Option<u32> {
        // First check CLAURST_TOKEN_BUDGET env var
        if let Ok(budget_str) = std::env::var("CLAURST_TOKEN_BUDGET") {
            if let Ok(budget) = budget_str.parse::<u32>() {
                return Some(budget);
            }
        }
        // Could extend this to check model defaults, but for now just env var
        None
    }

    #[cfg(not(feature = "token_budget"))]
    fn load_token_budget() -> Option<u32> {
        None
    }

    /// Update the Rustle pose for this frame — handles temporary poses, random blinks,
    /// and the loading spinner on stalls/errors.
    /// Call once per frame before rendering.
    pub fn tick_rustle_pose(&mut self) {
        // Loading spinner: shown when streaming has stalled (no data for 3s+).
        if self.is_streaming {
            if let Some(start) = self.stall_start {
                if start.elapsed() > std::time::Duration::from_secs(3) {
                    self.rustle_current_pose = crate::rustle::RustlePose::Loading {
                        frame: self.frame_count,
                    };
                    return;
                }
            }
        }

        // Check if a temporary pose is active.
        if let Some(until) = self.rustle_pose_until {
            if std::time::Instant::now() < until {
                self.rustle_current_pose = self.rustle_temp_pose.clone()
                    .unwrap_or(crate::rustle::RustlePose::Default);
                return;
            }
            // Expired — clear it.
            self.rustle_pose_until = None;
            self.rustle_temp_pose = None;
        }

        // Random eye-shift: every ~200-500 frames, briefly look right.
        if self.frame_count >= self.rustle_next_blink {
            self.rustle_temp_pose = Some(crate::rustle::RustlePose::LookRight);
            self.rustle_pose_until = Some(
                std::time::Instant::now() + std::time::Duration::from_millis(800)
            );
            // Schedule next blink 200-500 frames from now (random-ish).
            let jitter = (self.frame_count.wrapping_mul(7) % 300) + 200;
            self.rustle_next_blink = self.frame_count + jitter;
            self.rustle_current_pose = crate::rustle::RustlePose::LookRight;
            return;
        }

        self.rustle_current_pose = crate::rustle::RustlePose::Default;
    }

    /// Trigger Rustle looking down briefly (called on Tab / mode switch).
    pub fn rustle_look_down(&mut self) {
        self.rustle_temp_pose = Some(crate::rustle::RustlePose::LookDown);
        self.rustle_pose_until = Some(
            std::time::Instant::now() + std::time::Duration::from_secs(1)
        );
    }

    /// Cycle to the next agent mode: build → plan → build.
    /// Sets `agent_mode_changed` so the main loop can update the query config
    /// and tool list accordingly.
    pub fn cycle_agent_mode(&mut self) {
        const MODES: &[&str] = &["build", "plan"];
        let current = self.agent_mode.as_deref().unwrap_or("build");
        let idx = MODES.iter().position(|&m| m == current).unwrap_or(0);
        let next = MODES[(idx + 1) % MODES.len()];
        self.agent_mode = Some(next.to_string());
        self.agent_mode_changed = true;
        self.accent_color = accent_for_mode(Some(next));

        // Sync plan_mode flag for legacy code paths
        self.plan_mode = next == "plan";

        let label = match next {
            "build" => "Build",
            "plan" => "Plan",
            other => other,
        };
        self.status_message = Some(format!("Switched to {} mode.", label));
    }

    /// Update the context window size from the model registry for the current model.
    pub fn refresh_context_window_size(&mut self) {
        let provider = self.config.provider.as_deref().unwrap_or("anthropic");
        let model_id = self.model_name
            .strip_prefix(&format!("{}/", provider))
            .unwrap_or(&self.model_name);
        if let Some(entry) = self.model_registry.get(provider, model_id) {
            self.context_window_size = entry.info.context_window as u64;
        } else {
            // Fallback: common defaults
            self.context_window_size = match provider {
                "anthropic" => 200_000,
                "openai" => 128_000,
                "google" => 1_048_576,
                _ => 128_000,
            };
        }
    }

    /// Apply a theme by name, persisting it to config.
    pub fn apply_theme(&mut self, theme_name: &str) {
        let theme = match theme_name {
            "dark" => Theme::Dark,
            "light" => Theme::Light,
            "default" => Theme::Default,
            "deuteranopia" => Theme::Deuteranopia,
            other => Theme::Custom(other.to_string()),
        };
        self.config.theme = theme;
        // Persist to settings file
        let mut settings = Settings::load_sync().unwrap_or_default();
        settings.config.theme = self.config.theme.clone();
        let _ = settings.save_sync();
        self.status_message = Some(format!("Theme set to: {}", theme_name));
    }

}

