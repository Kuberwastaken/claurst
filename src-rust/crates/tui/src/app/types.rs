//! Pure data model: messages, tool blocks, dialogs, state structs.

use claurst_core::types::Message;

/// Visual style for inline system messages in the conversation pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemMessageStyle {
    Info,
    Warning,
    /// Compact / auto-compact boundary marker.
    Compact,
}

/// A synthetic system annotation inserted between conversation messages.
/// `after_index` is the index in `App::messages` after which this annotation
/// should appear (0 = before all messages, 1 = after message 0, etc.).
#[derive(Debug, Clone)]
pub struct SystemAnnotation {
    pub after_index: usize,
    pub text: String,
    pub style: SystemMessageStyle,
}

/// A displayable item in the conversation pane — either a real message or
/// a synthetic system annotation (e.g. compact boundary).
/// Used only by `render.rs`; constructed on the fly from `messages` +
/// `system_annotations`.
#[derive(Debug, Clone)]
pub enum DisplayMessage {
    /// A real conversation turn.
    Conversation(Message),
    /// An injected system notice (e.g. compact boundary).
    System { text: String, style: SystemMessageStyle },
}

/// Context menu state: position and currently selected item index.
#[derive(Debug, Clone, Copy)]
pub struct ContextMenuState {
    /// X coordinate of the menu (column).
    pub x: u16,
    /// Y coordinate of the menu (row).
    pub y: u16,
    /// Currently selected menu item index (0-based).
    pub selected_index: usize,
    /// What the context menu is acting on.
    pub kind: ContextMenuKind,
}

/// What content the context menu is currently targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuKind {
    /// A specific transcript message.
    Message { message_index: usize },
    /// The current text selection anywhere in the frame.
    Selection,
}

/// Available context menu items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuItem {
    Copy,
    Fork,
}

/// State for the Go to Line dialog (Ctrl+G in message pane).
#[derive(Debug, Clone)]
pub struct GoToLineDialog {
    /// Input field for line number.
    pub input: String,
    /// Whether the dialog is currently active.
    pub active: bool,
    /// Total number of lines (for validation feedback).
    pub total_lines: usize,
}

impl Default for GoToLineDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl GoToLineDialog {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            active: false,
            total_lines: 0,
        }
    }

    pub fn open(&mut self, total_lines: usize) {
        self.input.clear();
        self.active = true;
        self.total_lines = total_lines;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.input.clear();
    }

    /// Parse the input as a line number (1-indexed).
    /// Returns None if invalid or out of range.
    pub fn parse_line_number(&self) -> Option<usize> {
        let line_num: usize = self.input.trim().parse().ok()?;
        if line_num >= 1 && line_num <= self.total_lines {
            Some(line_num)
        } else {
            None
        }
    }
}

/// Status of an active or completed tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done,
    Error,
}

/// Represents an active or completed tool invocation visible in the UI.
#[derive(Debug, Clone)]
pub struct ToolUseBlock {
    pub id: String,
    pub name: String,
    pub turn_index: Option<usize>,
    pub status: ToolStatus,
    pub output_preview: Option<String>,
    /// JSON-serialised input for the tool call (populated from the API stream).
    pub input_json: String,
}

#[derive(Debug, Clone, Default)]
pub struct TurnMetadata {
    pub submitted_at: Option<String>,
    pub model_name: Option<String>,
    pub agent_mode: Option<String>,
    pub duration: Option<String>,
    pub interrupted: bool,
}

/// State for Ctrl+R history search mode (legacy inline struct, kept for test
/// compatibility — the overlay version lives in `overlays::HistorySearchOverlay`).
#[derive(Debug, Clone)]
pub struct HistorySearch {
    pub query: String,
    /// Indices into `input_history` that match the current query.
    pub matches: Vec<usize>,
    /// Which match is currently highlighted.
    pub selected: usize,
}

impl Default for HistorySearch {
    fn default() -> Self {
        Self::new()
    }
}

impl HistorySearch {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        }
    }

    /// Re-compute matches against the given history slice.
    pub fn update_matches(&mut self, history: &[String]) {
        let q = self.query.to_lowercase();
        self.matches = history
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                if s.to_lowercase().contains(&q) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        // Clamp selected to valid range
        if !self.matches.is_empty() && self.selected >= self.matches.len() {
            self.selected = self.matches.len() - 1;
        }
    }

    /// Return the currently selected history entry, if any.
    pub fn current_entry<'a>(&self, history: &'a [String]) -> Option<&'a str> {
        self.matches
            .get(self.selected)
            .and_then(|&i| history.get(i))
            .map(String::as_str)
    }
}

/// Which area of the TUI currently has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    /// Keyboard input goes to the prompt editor.
    Input,
    /// Keyboard input goes to the transcript/message pane (scroll, etc.).
    Transcript,
}

/// A lightweight record of a recent session, shown in the welcome screen's
/// "Recent activity" list.
///
/// Loaded asynchronously from `session_storage` (see `recent_sessions_pending`
/// in the run loop) so the render path never touches disk. Holds only what the
/// welcome box needs: a display label plus the transcript's modification time,
/// from which a relative timestamp ("2h ago") is computed at render time.
#[derive(Debug, Clone)]
pub struct RecentSession {
    /// Display label: the custom title, else a truncated last prompt, else
    /// `"(untitled)"`.
    pub label: String,
    /// Transcript modification time, used to derive a relative timestamp.
    pub mtime: std::time::SystemTime,
}

/// Build the display label for a recent session: prefer the custom title, fall
/// back to the first line of the last prompt (truncated), else `"(untitled)"`.
pub fn recent_session_label(title: Option<String>, last_prompt: Option<String>) -> String {
    /// Cap stored labels so a huge prompt never bloats `App` state; the render
    /// path truncates further to the column width.
    const MAX_LABEL: usize = 80;

    let pick = |s: String| -> Option<String> {
        // First non-empty line, trimmed.
        let line = s.lines().find(|l| !l.trim().is_empty())?.trim();
        if line.is_empty() {
            return None;
        }
        let truncated: String = line.chars().take(MAX_LABEL).collect();
        Some(truncated)
    };

    title
        .and_then(pick)
        .or_else(|| last_prompt.and_then(pick))
        .unwrap_or_else(|| "(untitled)".to_string())
}

