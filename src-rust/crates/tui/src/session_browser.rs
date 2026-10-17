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
    /// User is typing a filter query that narrows the visible list.
    Search,
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
}

/// State for the session browser overlay.
pub struct SessionBrowserState {
    pub visible: bool,
    pub selected_idx: usize,
    pub sessions: Vec<SessionEntry>,
    pub mode: SessionBrowserMode,
    /// Input buffer used while in `Rename` mode.
    pub rename_input: String,
    /// Filter query narrowing the visible list (empty = show all). Kept when
    /// leaving `Search` mode so the narrowed view persists until cleared.
    pub filter_input: String,
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
            filter_input: String::new(),
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

    /// Open the browser with a pre-applied filter query. The list is populated
    /// later by the async loader (which assigns `sessions` directly), so the
    /// query only needs to live in `filter_input`; the visible rows recompute
    /// from it on every render.
    pub fn open_with_query(&mut self, sessions: Vec<SessionEntry>, query: &str) {
        self.open(sessions);
        self.filter_input = query.trim().to_string();
    }

    /// Sessions matching the current filter: case-insensitive substring match
    /// against the title or the session id. An empty filter shows everything.
    pub fn visible_sessions(&self) -> Vec<&SessionEntry> {
        let query = self.filter_input.trim().to_lowercase();
        if query.is_empty() {
            return self.sessions.iter().collect();
        }
        self.sessions
            .iter()
            .filter(|s| {
                s.title.to_lowercase().contains(&query) || s.id.to_lowercase().contains(&query)
            })
            .collect()
    }

    /// Enter search mode, keeping any existing filter so it can be edited.
    pub fn start_search(&mut self) {
        self.mode = SessionBrowserMode::Search;
    }

    /// Append a character to the filter input and reset the selection to the
    /// first visible row so the highlight never points past the narrowed list.
    pub fn push_filter_char(&mut self, c: char) {
        if self.mode == SessionBrowserMode::Search {
            self.filter_input.push(c);
            self.selected_idx = 0;
        }
    }

    /// Remove the last character from the filter input.
    pub fn pop_filter_char(&mut self) {
        if self.mode == SessionBrowserMode::Search {
            self.filter_input.pop();
            self.selected_idx = 0;
        }
    }

    /// Leave search mode, keeping the filter applied. Returns to `Browse` so
    /// navigation and resume/rename work against the narrowed list.
    pub fn confirm_search(&mut self) {
        self.mode = SessionBrowserMode::Browse;
    }

    /// Clear the filter entirely.
    pub fn clear_filter(&mut self) {
        self.filter_input.clear();
        self.selected_idx = 0;
    }

    /// Close the browser entirely. The filter is kept so reopening the
    /// browser remembers the previous query.
    pub fn close(&mut self) {
        self.visible = false;
        self.mode = SessionBrowserMode::Browse;
        self.rename_input.clear();
    }

    /// Move selection up one visible row, wrapping to the end.
    pub fn select_prev(&mut self) {
        let count = self.visible_sessions().len();
        if count == 0 {
            return;
        }
        if self.selected_idx == 0 {
            self.selected_idx = count - 1;
        } else {
            self.selected_idx -= 1;
        }
    }

    /// Move selection down one visible row, wrapping to the start.
    pub fn select_next(&mut self) {
        let count = self.visible_sessions().len();
        if count == 0 {
            return;
        }
        self.selected_idx = (self.selected_idx + 1) % count;
    }

    /// Return a reference to the currently selected visible session, if any.
    pub fn selected_session(&self) -> Option<&SessionEntry> {
        self.visible_sessions().get(self.selected_idx).copied()
    }

    /// Switch to rename mode, pre-populating the input with the current title.
    pub fn start_rename(&mut self) {
        if let Some(session) = self.selected_session() {
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
        // Look up the selected session through the filtered view, then apply
        // the rename to the underlying entry so it sticks across filter edits.
        let selected_id = self.selected_session()?.id.clone();
        let session = self.sessions.iter_mut().find(|s| s.id == selected_id)?;
        session.title = new_name.clone();
        self.mode = SessionBrowserMode::Browse;
        self.rename_input.clear();
        Some((selected_id, new_name))
    }

    /// Cancel the current mode:
    /// - In `Search`, `Rename` or `Confirm` mode: return to `Browse`.
    ///   (In `Search` mode the filter text stays applied.)
    /// - In `Browse` mode: close the overlay.
    pub fn cancel(&mut self) {
        match self.mode {
            SessionBrowserMode::Browse => self.close(),
            SessionBrowserMode::Search
            | SessionBrowserMode::Rename
            | SessionBrowserMode::Confirm => {
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
/// Draws a centred modal (≈70 wide × ≈20 tall) with:
/// - A scrollable list of sessions (id, title, date, messages, cost)
/// - Selection highlight on the focused row
/// - Mode-sensitive hint bar at the bottom
/// - A rename input field shown when in `Rename` mode
pub fn render_session_browser(state: &SessionBrowserState, area: Rect, buf: &mut Buffer) {
    if !state.visible {
        return;
    }

    const MODAL_W: u16 = 70;
    const MODAL_H: u16 = 20;

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

    let inner_w = dialog_area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();

    // --- Filter bar -------------------------------------------------------
    if !state.filter_input.is_empty() || state.mode == SessionBrowserMode::Search {
        let label = "  Filter: ";
        let cursor = "\u{2588}"; // block cursor while typing
        let shown_cursor = if state.mode == SessionBrowserMode::Search {
            cursor
        } else {
            ""
        };
        let input_display = format!("{}{}", state.filter_input, shown_cursor);
        let count = state.visible_sessions().len();
        let hint = if state.mode == SessionBrowserMode::Search {
            format!("  ({}/{})", count, state.sessions.len())
        } else {
            format!(
                "  ({}/{})  / to edit, Ctrl+U to clear",
                count,
                state.sessions.len()
            )
        };
        lines.push(Line::from(vec![
            Span::styled(
                label,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                input_display,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(hint, Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::from(""));
    }

    // --- Session list -----------------------------------------------------
    let visible = state.visible_sessions();
    if visible.is_empty() {
        if state.sessions.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                "  No sessions found.",
                Style::default().fg(Color::DarkGray),
            )]));
        } else {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                format!("  No sessions match '{}'.", state.filter_input.trim()),
                Style::default().fg(Color::DarkGray),
            )]));
        }
    } else {
        // Column widths (approximate):
        //   title: ~40 chars  |  date: ~14 chars  |  msgs: 5  |  cost: 9
        let date_w: usize = 14;
        let msgs_w: usize = 5;
        let cost_w: usize = 9;
        let fixed = date_w + msgs_w + cost_w + 6; // separators & padding
        let title_w = inner_w.saturating_sub(fixed).max(10);

        // Header row
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<title_w$}  {:<date_w$}  {:>msgs_w$}  {:>cost_w$}",
                    "Title", "Last Updated", "Msgs", "Cost",
                    title_w = title_w, date_w = date_w,
                    msgs_w = msgs_w, cost_w = cost_w),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::UNDERLINED),
            ),
        ]));
        lines.push(Line::from(""));

        for (i, session) in visible.iter().enumerate() {
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

            lines.push(Line::from(vec![
                Span::styled("  ", prefix_style),
                Span::styled(format!("{:<title_w$}", title_cell, title_w = title_w), title_style),
                Span::styled("  ", meta_style),
                Span::styled(format!("{:<date_w$}", date_cell, date_w = date_w), meta_style),
                Span::styled("  ", meta_style),
                Span::styled(msgs_cell, meta_style),
                Span::styled("  ", meta_style),
                Span::styled(cost_cell, meta_style),
            ]));
        }
    }

    lines.push(Line::from(""));

    // --- Mode-sensitive bottom section -----------------------------------
    match &state.mode {
        SessionBrowserMode::Browse => {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "\u{2191}\u{2193}",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" navigate  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "/",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=filter  ", Style::default().fg(Color::DarkGray)),
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
                    "Esc",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=close", Style::default().fg(Color::DarkGray)),
            ]));
        }
        SessionBrowserMode::Search => {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "Enter",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=apply  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Esc",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=apply & browse  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Ctrl+U",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=clear", Style::default().fg(Color::DarkGray)),
            ]));
        }
        SessionBrowserMode::Rename => {
            // Show rename input field.
            let label = "  Rename: ";
            let cursor = "\u{2588}"; // block cursor
            let input_display = format!("{}{}", state.rename_input, cursor);
            lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(
                    input_display,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(vec![
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
            ]));
        }
        SessionBrowserMode::Confirm => {
            lines.push(Line::from(vec![
                Span::styled(
                    "  Confirm? ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "Enter",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=yes  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Esc",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=no", Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Sessions ")
        .title_alignment(Alignment::Center)
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false });

    use ratatui::widgets::Widget;
    para.render(dialog_area, buf);
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
            },
            SessionEntry {
                id: "sess-002".to_string(),
                title: "Write unit tests".to_string(),
                last_updated: "yesterday".to_string(),
                message_count: 12,
                cost_usd: 0.0045,
            },
            SessionEntry {
                id: "sess-003".to_string(),
                title: "Debug memory leak".to_string(),
                last_updated: "3 days ago".to_string(),
                message_count: 57,
                cost_usd: 0.0289,
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
    #[test]
    fn truncate_display_trims() {
        let long = "abcdefghij"; // 10 chars
        let result = truncate_display(long, 5);
        assert!(result.width() <= 6, "truncated string should fit within budget");
        assert!(result.ends_with('…'));
    }

    // 16. Empty filter shows all sessions.
    #[test]
    fn empty_filter_shows_all() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        assert_eq!(s.visible_sessions().len(), 3);
    }

    // 17. Filter narrows by title, case-insensitively.
    #[test]
    fn filter_narrows_by_title_case_insensitive() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.filter_input = "AUTH".to_string();
        let visible = s.visible_sessions();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "sess-001");
    }

    // 18. Filter also matches the session id.
    #[test]
    fn filter_matches_session_id() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.filter_input = "sess-003".to_string();
        let visible = s.visible_sessions();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "sess-003");
    }

    // 19. Non-matching filter yields an empty visible list but keeps sessions.
    #[test]
    fn filter_no_match_keeps_underlying_list() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.filter_input = "nope".to_string();
        assert!(s.visible_sessions().is_empty());
        assert_eq!(s.sessions.len(), 3);
    }

    // 20. open_with_query pre-applies the filter.
    #[test]
    fn open_with_query_prefilters() {
        let mut s = SessionBrowserState::new();
        s.open_with_query(sample_sessions(), "  tests  ");
        assert!(s.visible);
        assert_eq!(s.filter_input, "tests");
        let visible = s.visible_sessions();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "sess-002");
    }

    // 21. Typing in Search mode edits the filter and resets selection.
    #[test]
    fn search_mode_typing_edits_filter() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.start_search();
        s.selected_idx = 2;
        s.push_filter_char('t');
        s.push_filter_char('e');
        s.push_filter_char('s');
        s.push_filter_char('t');
        assert_eq!(s.filter_input, "test");
        assert_eq!(s.selected_idx, 0);
        s.pop_filter_char();
        assert_eq!(s.filter_input, "tes");
        s.pop_filter_char();
        assert_eq!(s.filter_input, "te");
    }

    // 22. Filter chars are ignored outside Search mode.
    #[test]
    fn filter_chars_ignored_outside_search_mode() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.push_filter_char('x');
        assert!(s.filter_input.is_empty());
    }

    // 23. confirm_search returns to Browse and keeps the filter applied.
    #[test]
    fn confirm_search_keeps_filter() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.start_search();
        s.push_filter_char('a');
        s.push_filter_char('u');
        s.confirm_search();
        assert_eq!(s.mode, SessionBrowserMode::Browse);
        assert_eq!(s.visible_sessions().len(), 1);
    }

    // 24. clear_filter empties the filter and resets selection.
    #[test]
    fn clear_filter_resets() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.filter_input = "auth".to_string();
        s.selected_idx = 0;
        s.clear_filter();
        assert!(s.filter_input.is_empty());
        assert_eq!(s.visible_sessions().len(), 3);
    }

    // 25. Selection navigates the filtered view, not the raw list.
    #[test]
    fn selection_indexes_filtered_view() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.filter_input = "e".to_string(); // matches all three titles
        s.select_next();
        assert_eq!(s.selected_session().map(|x| x.id.as_str()), Some("sess-002"));
        // Narrow further without going through push_filter_char: the stale
        // index can point past the visible rows, in which case there is no
        // selection until the next navigation keypress clamps it back in.
        s.filter_input = "leak".to_string();
        assert_eq!(s.visible_sessions().len(), 1);
        assert!(s.selected_session().is_none());
        s.select_next();
        assert_eq!(s.selected_session().map(|x| x.id.as_str()), Some("sess-003"));
    }

    // 26. Rename applies to the underlying entry selected through the filter.
    #[test]
    fn rename_through_filter_updates_underlying_entry() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.filter_input = "leak".to_string(); // only sess-003 visible
        s.start_rename();
        assert_eq!(s.mode, SessionBrowserMode::Rename);
        s.rename_input = "Fixed the leak".to_string();
        let result = s.confirm_rename();
        assert_eq!(
            result,
            Some(("sess-003".to_string(), "Fixed the leak".to_string()))
        );
        // The underlying entry was renamed even though it is not at index 0 of
        // the raw list.
        assert_eq!(s.sessions[2].title, "Fixed the leak");
    }

    // 27. close() keeps the filter for the next open.
    #[test]
    fn close_keeps_filter_for_reopen() {
        let mut s = SessionBrowserState::new();
        s.open_with_query(sample_sessions(), "auth");
        s.close();
        assert!(!s.visible);
        assert_eq!(s.filter_input, "auth");
        // Reopening without a query still shows the previous filter applied.
        s.open(sample_sessions());
        assert_eq!(s.visible_sessions().len(), 1);
    }

    // 28. Rendering with an active filter and no matches does not panic.
    #[test]
    fn render_no_match_does_not_panic() {
        let mut s = SessionBrowserState::new();
        s.open_with_query(sample_sessions(), "zzz");
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
    }

    // 29. Rendering while typing in Search mode does not panic.
    #[test]
    fn render_search_mode_does_not_panic() {
        let mut s = SessionBrowserState::new();
        s.open(sample_sessions());
        s.start_search();
        s.push_filter_char('a');
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
    }
}
