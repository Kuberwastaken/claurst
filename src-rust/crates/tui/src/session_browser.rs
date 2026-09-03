//! Session browser overlay (/session, /resume, /rename, /export).
//! Mirrors TS session management in REPL.tsx

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::overlays::centered_rect;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The interaction mode of the session browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionBrowserMode {
    /// Default: list sessions, navigate with arrow keys.
    Browse,
    /// User is typing a new name for the selected session.
    Rename,
    /// Waiting for the user to confirm a destructive action (delete / export).
    Confirm,
}

/// A single session entry shown in the browser list.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub id: String,
    pub title: String,
    /// Human-readable relative time, e.g. "2 hours ago".
    pub last_updated: String,
    pub message_count: usize,
    /// Estimated USD cost for the session.
    pub cost_usd: f64,
    /// Working directory recorded for the session, shown when path display
    /// is toggled on (`a`). `None` for sessions saved before the field
    /// existed.
    pub working_dir: Option<String>,
}

/// State for the session browser overlay.
pub struct SessionBrowserState {
    pub visible: bool,
    pub selected_idx: usize,
    pub sessions: Vec<SessionEntry>,
    pub mode: SessionBrowserMode,
    /// Input buffer used while in `Rename` mode.
    pub rename_input: String,
    /// Whether each session's working-directory path is shown in the list
    /// (toggled with `a`). Off by default to keep rows compact.
    pub show_paths: bool,
    /// Whether a preview of the selected session is shown below the list
    /// (toggled with `p`). On by default.
    pub show_preview: bool,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl SessionBrowserState {
    /// Create a new, hidden browser with an empty session list.
    pub fn new() -> Self {
        Self {
            visible: false,
            selected_idx: 0,
            sessions: Vec::new(),
            mode: SessionBrowserMode::Browse,
            rename_input: String::new(),
            show_paths: false,
            show_preview: true,
        }
    }

    /// Open the browser with the provided session list.
    pub fn open(&mut self, sessions: Vec<SessionEntry>) {
        self.sessions = sessions;
        self.selected_idx = 0;
        self.mode = SessionBrowserMode::Browse;
        self.rename_input.clear();
        self.visible = true;
    }

    /// Close the browser entirely.
    pub fn close(&mut self) {
        self.visible = false;
        self.mode = SessionBrowserMode::Browse;
        self.rename_input.clear();
    }

    /// Move selection up one row, wrapping to the end.
    pub fn select_prev(&mut self) {
        let count = self.sessions.len();
        if count == 0 {
            return;
        }
        if self.selected_idx == 0 {
            self.selected_idx = count - 1;
        } else {
            self.selected_idx -= 1;
        }
    }

    /// Move selection down one row, wrapping to the start.
    pub fn select_next(&mut self) {
        let count = self.sessions.len();
        if count == 0 {
            return;
        }
        self.selected_idx = (self.selected_idx + 1) % count;
    }

    /// Toggle whether each session's working-directory path is shown.
    pub fn toggle_show_paths(&mut self) {
        self.show_paths = !self.show_paths;
    }

    /// Toggle whether the selected session's preview is shown.
    pub fn toggle_show_preview(&mut self) {
        self.show_preview = !self.show_preview;
    }

    /// Return a reference to the currently selected session, if any.
    pub fn selected_session(&self) -> Option<&SessionEntry> {
        self.sessions.get(self.selected_idx)
    }

    /// Switch to rename mode, pre-populating the input with the current title.
    pub fn start_rename(&mut self) {
        if let Some(session) = self.sessions.get(self.selected_idx) {
            self.rename_input = session.title.clone();
            self.mode = SessionBrowserMode::Rename;
        }
    }

    /// Append a character to the rename input buffer.
    pub fn push_rename_char(&mut self, c: char) {
        if self.mode == SessionBrowserMode::Rename {
            self.rename_input.push(c);
        }
    }

    /// Remove the last character from the rename input buffer.
    pub fn pop_rename_char(&mut self) {
        if self.mode == SessionBrowserMode::Rename {
            self.rename_input.pop();
        }
    }

    /// Confirm the rename. Returns `(session_id, new_name)` when in rename mode
    /// with a non-empty name and a valid selection. Resets to browse mode.
    pub fn confirm_rename(&mut self) -> Option<(String, String)> {
        if self.mode != SessionBrowserMode::Rename {
            return None;
        }
        let new_name = self.rename_input.trim().to_string();
        if new_name.is_empty() {
            return None;
        }
        let session_id = self.sessions.get(self.selected_idx)?.id.clone();
        // Apply the rename in the local list immediately for UI consistency.
        if let Some(session) = self.sessions.get_mut(self.selected_idx) {
            session.title = new_name.clone();
        }
        self.mode = SessionBrowserMode::Browse;
        self.rename_input.clear();
        Some((session_id, new_name))
    }

    /// Cancel the current mode:
    /// - In `Rename` or `Confirm` mode: return to `Browse`.
    /// - In `Browse` mode: close the overlay.
    pub fn cancel(&mut self) {
        match self.mode {
            SessionBrowserMode::Browse => self.close(),
            SessionBrowserMode::Rename | SessionBrowserMode::Confirm => {
                self.mode = SessionBrowserMode::Browse;
                self.rename_input.clear();
            }
        }
    }
}

impl Default for SessionBrowserState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Format a cost as a dollar string with 4 decimal places.
fn fmt_cost(usd: f64) -> String {
    if usd < 0.0001 {
        "$0.0000".to_string()
    } else {
        format!("${:.4}", usd)
    }
}

/// Truncate `s` to fit within `max_width` display columns, appending `…` if cut.
fn truncate_display(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        if out.width() + ch.len_utf8() + 1 > max_width {
            break;
        }
        out.push(ch);
    }
    format!("{}…", out)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the session browser overlay directly into `buf`.
///
/// Layout is fixed so every region stays reachable regardless of session
/// count: list (windowed around the selection), preview panel (when enabled),
/// and a mode-sensitive hint bar at the bottom. Windowing keeps the selection
/// visible past ~14 sessions instead of letting the list push the preview and
/// hints off the modal.
pub fn render_session_browser(state: &SessionBrowserState, area: Rect, buf: &mut Buffer) {
    if !state.visible {
        return;
    }

    const MODAL_W: u16 = 70;
    const MODAL_H: u16 = 24;

    let dialog_area = centered_rect(
        MODAL_W.min(area.width.saturating_sub(2)),
        MODAL_H.min(area.height.saturating_sub(2)),
        area,
    );

    // --- Clear background -------------------------------------------------
    for y in dialog_area.y..dialog_area.y + dialog_area.height {
        for x in dialog_area.x..dialog_area.x + dialog_area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }

    // --- Layout: list area, preview area, hint area ------------------------
    // Borders consume 2 rows; the hint bar gets 2 lines (hint + spacer).
    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };
    let hint_h: u16 = 2;
    let preview_h: u16 = if state.show_preview { 6 } else { 0 };
    let list_h = inner.height.saturating_sub(hint_h + preview_h);
    let list_area = Rect { height: list_h, ..inner };
    let preview_area = Rect {
        y: inner.y + list_h,
        height: preview_h,
        ..inner
    };
    let hint_area = Rect {
        y: inner.y + list_h + preview_h,
        height: hint_h,
        ..inner
    };

    let inner_w = inner.width as usize;

    // --- Session list (windowed around the selection) -----------------------
    let mut list_lines: Vec<Line> = Vec::new();
    if state.sessions.is_empty() {
        list_lines.push(Line::from(""));
        list_lines.push(Line::from(vec![Span::styled(
            "  No sessions found.",
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        // Column widths (approximate):
        //   title: flexible  |  date: ~14 chars  |  msgs: 5  |  cost: 9
        let date_w: usize = 14;
        let msgs_w: usize = 5;
        let cost_w: usize = 9;
        let fixed = date_w + msgs_w + cost_w + 6; // separators & padding
        let title_w = inner_w.saturating_sub(fixed).max(10);

        // Header row
        list_lines.push(Line::from(vec![Span::styled(
            format!(
                "  {:<title_w$}  {:<date_w$}  {:>msgs_w$}  {:>cost_w$}",
                "Title",
                "Last Updated",
                "Msgs",
                "Cost",
                title_w = title_w,
                date_w = date_w,
                msgs_w = msgs_w,
                cost_w = cost_w
            ),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::UNDERLINED),
        )]));
        list_lines.push(Line::from(""));

        // Visible-row window: show at most list_h-2 rows (header + blank),
        // keeping the selection inside the window.
        let list_rows = list_h.saturating_sub(2) as usize;
        let (window_start, window_end) = list_window(state.selected_idx, state.sessions.len(), list_rows);

        for i in window_start..window_end {
            let session = &state.sessions[i];
            let is_selected = i == state.selected_idx;

            let title_cell = truncate_display(&session.title, title_w);
            let date_cell = truncate_display(&session.last_updated, date_w);
            let msgs_cell = format!("{:>msgs_w$}", session.message_count, msgs_w = msgs_w);
            let cost_cell = format!("{:>cost_w$}", fmt_cost(session.cost_usd), cost_w = cost_w);

            let row_bg = if is_selected {
                Color::Rgb(40, 60, 80)
            } else {
                // transparent — ratatui uses reset/default for "no background"
                Color::Reset
            };

            let title_style = if is_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let meta_style = if is_selected {
                Style::default().fg(Color::Rgb(180, 200, 220)).bg(row_bg)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let prefix_style = Style::default().bg(row_bg);

            list_lines.push(Line::from(vec![
                Span::styled("  ", prefix_style),
                Span::styled(
                    format!("{:<title_w$}", title_cell, title_w = title_w),
                    title_style,
                ),
                Span::styled("  ", meta_style),
                Span::styled(
                    format!("{:<date_w$}", date_cell, date_w = date_w),
                    meta_style,
                ),
                Span::styled("  ", meta_style),
                Span::styled(msgs_cell, meta_style),
                Span::styled("  ", meta_style),
                Span::styled(cost_cell, meta_style),
            ]));

            // Optional working-directory row under each entry (toggle: `a`).
            if state.show_paths {
                let path_display = session
                    .working_dir
                    .as_deref()
                    .unwrap_or("(no working dir)");
                let path_cell = truncate_display(path_display, inner_w.saturating_sub(4));
                let path_style = if is_selected {
                    Style::default()
                        .fg(Color::Rgb(140, 160, 180))
                        .bg(row_bg)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                list_lines.push(Line::from(vec![
                    Span::styled("      ", prefix_style),
                    Span::styled(path_cell, path_style),
                ]));
            }
        }
    }

    // --- Preview panel (toggle: `p`) ---------------------------------------
    let mut preview_lines: Vec<Line> = Vec::new();
    if state.show_preview {
        preview_lines.push(Line::from(vec![Span::styled(
            " Preview",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]));
        match state.selected_session() {
            Some(session) => {
                preview_lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(&session.title, Style::default().fg(Color::White)),
                ]));
                preview_lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        format!(
                            "{} messages · {} · {}",
                            session.message_count,
                            session.last_updated,
                            fmt_cost(session.cost_usd)
                        ),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                if let Some(dir) = session.working_dir.as_deref() {
                    preview_lines.push(Line::from(vec![
                        Span::styled("  ", Style::default()),
                        Span::styled(dir, Style::default().fg(Color::DarkGray)),
                    ]));
                }
            }
            None => preview_lines.push(Line::from(vec![Span::styled(
                "  (no session selected)",
                Style::default().fg(Color::DarkGray),
            )])),
        }
    }

    // --- Mode-sensitive hint bar -------------------------------------------
    let hint_lines: Vec<Line> = match &state.mode {
        SessionBrowserMode::Browse => vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "\u{2191}\u{2193}",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" navigate  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Enter",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=resume  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "r",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=rename  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "a",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=paths  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "p",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=preview  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Esc",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=close", Style::default().fg(Color::DarkGray)),
            ]),
        ],
        SessionBrowserMode::Rename => vec![
            Line::from(vec![
                Span::styled(
                    "  Rename: ",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{}\u{2588}", state.rename_input),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "Enter",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=confirm  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Esc",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=cancel", Style::default().fg(Color::DarkGray)),
            ]),
        ],
        SessionBrowserMode::Confirm => vec![
            Line::from(vec![
                Span::styled(
                    "  Confirm? ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "Enter",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("=yes  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Esc",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=no", Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(""),
        ],
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Sessions ")
        .title_alignment(Alignment::Center)
        .border_style(Style::default().fg(Color::Cyan));

    use ratatui::widgets::Widget;
    let inner_block = Block::default().borders(Borders::ALL);
    let _ = inner_block; // layout already accounts for the outer borders

    Paragraph::new(list_lines)
        .block(block.clone())
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false })
        .render(list_area, buf);

    if state.show_preview {
        Paragraph::new(preview_lines)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false })
            .render(preview_area, buf);
    }

    Paragraph::new(hint_lines)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false })
        .render(hint_area, buf);
}

/// Compute the `[start, end)` window of rows to render so the selection stays
/// visible. Scrolls the window only when the selection would leave it.
fn list_window(selected: usize, total: usize, rows: usize) -> (usize, usize) {
    if total == 0 || rows == 0 {
        return (0, 0);
    }
    let rows = rows.min(total);
    if selected < rows {
        return (0, rows);
    }
    let start = (selected + 1).saturating_sub(rows);
    (start, start + rows)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_sessions() -> Vec<SessionEntry> {
        vec![
            SessionEntry {
                id: "sess-001".to_string(),
                title: "Refactor auth module".to_string(),
                last_updated: "2 hours ago".to_string(),
                message_count: 34,
                cost_usd: 0.0124,
                working_dir: Some("/home/user/project-a".to_string()),
            },
            SessionEntry {
                id: "sess-002".to_string(),
                title: "Write unit tests".to_string(),
                last_updated: "yesterday".to_string(),
                message_count: 12,
                cost_usd: 0.0045,
                working_dir: Some("/home/user/project-b".to_string()),
            },
            SessionEntry {
                id: "sess-003".to_string(),
                title: "Debug memory leak".to_string(),
                last_updated: "3 days ago".to_string(),
                message_count: 57,
                cost_usd: 0.0289,
                working_dir: None,
            },
        ]
    }

    // 1. new() starts hidden with no sessions.
    #[test]
    fn new_starts_hidden() {
        let s = SessionBrowserState::new();
        assert!(!s.visible);
        assert!(s.sessions.is_empty());
        assert_eq!(s.mode, SessionBrowserMode::Browse);
    }

    // 2. open() populates sessions and becomes visible.
    #[test]
    fn open_populates_and_shows() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        assert!(s.visible);
        assert_eq!(s.sessions.len(), 3);
        assert_eq!(s.selected_idx, 0);
        assert_eq!(s.mode, SessionBrowserMode::Browse);
    }

    // 3. select_next() advances selection and wraps to the start.
    #[test]
    fn select_next_wraps_to_start() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.select_next();
        assert_eq!(s.selected_idx, 1);
        s.select_next();
        assert_eq!(s.selected_idx, 2);
        s.select_next();
        assert_eq!(s.selected_idx, 0);
    }

    // 4. select_prev() decrements and wraps to the end.
    #[test]
    fn select_prev_wraps_to_end() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.select_prev();
        assert_eq!(s.selected_idx, 2);
    }

    // 5. selected_session() returns correct entry.
    #[test]
    fn selected_session_correct() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.selected_idx = 1;
        let sess = s.selected_session().unwrap();
        assert_eq!(sess.id, "sess-002");
    }

    // 6. start_rename() switches mode and pre-fills input.
    #[test]
    fn start_rename_prefills_title() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.selected_idx = 0;
        s.start_rename();
        assert_eq!(s.mode, SessionBrowserMode::Rename);
        assert_eq!(s.rename_input, "Refactor auth module");
    }

    // 7. push_rename_char / pop_rename_char edit the input buffer.
    #[test]
    fn rename_char_editing() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.start_rename();
        s.rename_input.clear(); // clear prefill for clean test
        s.push_rename_char('H');
        s.push_rename_char('i');
        assert_eq!(s.rename_input, "Hi");
        s.pop_rename_char();
        assert_eq!(s.rename_input, "H");
    }

    // 8. confirm_rename() returns (id, new_name) and resets mode.
    #[test]
    fn confirm_rename_returns_pair() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.selected_idx = 0;
        s.start_rename();
        s.rename_input = "  New Title  ".to_string(); // intentional whitespace
        let result = s.confirm_rename();
        assert_eq!(result, Some(("sess-001".to_string(), "New Title".to_string())));
        assert_eq!(s.mode, SessionBrowserMode::Browse);
        assert!(s.rename_input.is_empty());
        // Also check local title was updated
        assert_eq!(s.sessions[0].title, "New Title");
    }

    // 9. confirm_rename() with empty input returns None.
    #[test]
    fn confirm_rename_empty_returns_none() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.start_rename();
        s.rename_input = "   ".to_string(); // whitespace only
        let result = s.confirm_rename();
        assert!(result.is_none());
    }

    // 10. cancel() in Rename mode returns to Browse without closing.
    #[test]
    fn cancel_rename_goes_to_browse() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.start_rename();
        s.cancel();
        assert_eq!(s.mode, SessionBrowserMode::Browse);
        assert!(s.visible, "overlay should remain visible after cancel-from-rename");
    }

    // 11. cancel() in Browse mode closes the overlay.
    #[test]
    fn cancel_browse_closes() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        assert_eq!(s.mode, SessionBrowserMode::Browse);
        s.cancel();
        assert!(!s.visible);
    }

    // 12. render_session_browser does not panic.
    #[test]
    fn render_does_not_panic() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
    }

    // 13. render is a no-op when hidden.
    #[test]
    fn render_noop_when_hidden() {
        let s = SessionBrowserState::new(); // visible = false
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
        for cell in buf.content() {
            assert_eq!(cell.symbol(), " ", "buffer should be empty when browser is hidden");
        }
    }

    // 14. fmt_cost formats correctly.
    #[test]
    fn fmt_cost_formats() {
        assert_eq!(fmt_cost(0.0), "$0.0000");
        assert_eq!(fmt_cost(0.0124), "$0.0124");
        assert_eq!(fmt_cost(1.5), "$1.5000");
    }

    // 15. truncate_display trims long strings.
    // Toggle defaults: paths off, preview on.
    #[test]
    fn toggle_defaults() {
        let s = SessionBrowserState::new();
        assert!(!s.show_paths);
        assert!(s.show_preview);
    }

    // Toggling flips the flags.
    #[test]
    fn toggles_flip_flags() {
        let mut s = SessionBrowserState::new();
        s.toggle_show_paths();
        assert!(s.show_paths);
        s.toggle_show_preview();
        assert!(!s.show_preview);
    }

    // Windowing: selection visible even far down a long list.
    #[test]
    fn list_window_keeps_selection_visible() {
        // Early selection: window starts at 0.
        assert_eq!(list_window(2, 50, 10), (0, 10));
        // Selection past the first page: window scrolls to keep it.
        assert_eq!(list_window(15, 50, 10), (6, 16));
        // Selection at the end.
        assert_eq!(list_window(49, 50, 10), (40, 50));
        // Degenerate inputs.
        assert_eq!(list_window(0, 0, 10), (0, 0));
        assert_eq!(list_window(0, 5, 0), (0, 0));
        // Rows larger than total: everything fits.
        assert_eq!(list_window(3, 5, 10), (0, 5));
    }

    // Rendering with paths and preview shown must not panic.
    #[test]
    fn render_with_paths_and_preview_does_not_panic() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.toggle_show_paths();
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
    }

    // Rendering a long list keeps the selection row on screen.
    #[test]
    fn render_long_list_does_not_panic() {
        let mut s = SessionBrowserState::new();
        let sessions: Vec<SessionEntry> = (0..40)
            .map(|i| SessionEntry {
                id: format!("sess-{:03}", i),
                title: format!("Session number {}", i),
                last_updated: "1h ago".to_string(),
                message_count: i,
                cost_usd: 0.001 * (i as f64),
                working_dir: Some(format!("/home/user/proj-{}", i % 3)),
            })
            .collect();
        s.open(sessions);
        // Walk the selection through the whole list.
        for _ in 0..40 {
            s.select_next();
            let area = Rect::new(0, 0, 80, 24);
            let mut buf = Buffer::empty(area);
            render_session_browser(&s, area, &mut buf);
        }
    }

    #[test]
    fn truncate_display_trims() {
        let long = "abcdefghij"; // 10 chars
        let result = truncate_display(long, 5);
        assert!(result.width() <= 6, "truncated string should fit within budget");
        assert!(result.ends_with('…'));
    }
}
