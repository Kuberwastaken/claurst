// render.rs â€” All ratatui rendering logic.

use std::cell::RefCell;

use crate::agents_view::render_agents_menu;
use crate::context_viz::render_context_viz;
use crate::export_dialog::render_export_dialog;
use crate::app::{App, SystemAnnotation, SystemMessageStyle, ToolStatus};
use crate::clawd::{clawd_lines, ClawdPose};
use crate::diff_viewer::render_diff_dialog;
use crate::model_picker::render_model_picker;
use crate::session_browser::render_session_browser;
use crate::dialogs::{render_mcp_approval_dialog, render_permission_dialog};
use crate::feedback_survey::render_feedback_survey;
use crate::overage_upsell::render_overage_upsell;
use crate::voice_mode_notice::render_voice_mode_notice;
use crate::desktop_upsell_startup::render_desktop_upsell_startup;
use crate::memory_update_notification::render_memory_update_notification;
use crate::invalid_config_dialog::render_invalid_config_dialog;
use crate::bypass_permissions_dialog::render_bypass_permissions_dialog;
use crate::onboarding_dialog::render_onboarding_dialog;
use crate::elicitation_dialog::render_elicitation_dialog;
use crate::figures;
use crate::hooks_config_menu::render_hooks_config_menu;
use crate::mcp_view::render_mcp_view;
use crate::memory_file_selector::render_memory_file_selector;
use crate::messages::{RenderContext, render_markdown, render_message};
use crate::notifications::render_notification_banner;
use crate::overlays::{
    render_global_search, render_help_overlay, render_history_search_overlay, render_rewind_flow,
};
use crate::plugin_views::render_plugin_hints;
use crate::privacy_screen::render_privacy_screen;
use crate::prompt_input::{InputMode, TypeaheadSource, VimMode, input_height, render_prompt_input};
use crate::settings_screen::render_settings_screen;
use crate::stats_dialog::render_stats_dialog;
use crate::theme_screen::render_theme_screen;
use crate::virtual_list::{VirtualItem, VirtualList};
use cc_core::constants::APP_VERSION;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;
use std::time::Instant;

// Spinner frames matching the TypeScript SpinnerGlyph: platform-specific base
// characters mirrored (forward + reverse) for a smooth pulse effect.
// Windows uses '*' instead of '✳'/'✽' for better font coverage.
#[cfg(target_os = "windows")]
const SPINNER: &[char] = &['\u{00b7}', '\u{2722}', '*', '\u{2736}', '\u{273b}', '\u{273d}',
                            '\u{273d}', '\u{273b}', '\u{2736}', '*', '\u{2722}', '\u{00b7}'];
#[cfg(not(target_os = "windows"))]
const SPINNER: &[char] = &['\u{00b7}', '\u{2722}', '\u{2733}', '\u{2736}', '\u{273b}', '\u{273d}',
                            '\u{273d}', '\u{273b}', '\u{2736}', '\u{2733}', '\u{2722}', '\u{00b7}'];
const CLAUDE_ORANGE: Color = Color::Rgb(233, 30, 99);
const WELCOME_BOX_HEIGHT: u16 = 11;

fn spinner_char(frame_count: u64) -> char {
    SPINNER[(frame_count as usize) % SPINNER.len()]
}

/// Returns the colour to use for the streaming spinner.
/// Turns red when no stream data has arrived for more than 3 seconds.
fn spinner_color(app: &App) -> Color {
    if let Some(start) = app.stall_start {
        if start.elapsed() > std::time::Duration::from_secs(3) {
            return Color::Red;
        }
    }
    Color::Yellow
}

/// Global monotonic animation tick, capped to a fixed cadence.
/// 200ms step = 5 animation updates/second, independent of render FPS.
fn animation_tick_200ms() -> u64 {
    let start = Instant::now();
    (start.elapsed().as_millis() / 200) as u64
}

// -----------------------------------------------------------------------
// Text truncation helpers
// -----------------------------------------------------------------------

fn truncate_end(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "\u{2026}".to_string();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4]));
        if width + ch_width >= max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push('\u{2026}');
    out
}

fn truncate_middle(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return truncate_end(text, max_width);
    }
    let keep_each_side = (max_width.saturating_sub(1)) / 2;
    let left: String = text.chars().take(keep_each_side).collect();
    let right: String = text
        .chars()
        .rev()
        .take(keep_each_side)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{left}\u{2026}{right}")
}

fn truncate_text(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let next = format!("{out}{ch}");
        if next.width() > max_width {
            if max_width > 1 && out.width() < max_width {
                out.push('\u{2026}');
            }
            break;
        }
        out.push(ch);
    }
    out
}

// -----------------------------------------------------------------------
// Startup notice helpers
// -----------------------------------------------------------------------

fn startup_notice_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let max_width = width.saturating_sub(10) as usize;

    if let Some(summary) = app.away_summary.as_deref() {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", crate::figures::REFERENCE_MARK),
                Style::default().fg(CLAUDE_ORANGE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_end(summary, max_width),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    match &app.bridge_state {
        crate::bridge_state::BridgeConnectionState::Connected { peer_count, .. } => {
            let label = if *peer_count > 0 {
                format!("Remote session active \u{00b7} {} peer{}", peer_count, if *peer_count == 1 { "" } else { "s" })
            } else {
                "Remote session active".to_string()
            };
            lines.push(Line::from(vec![
                Span::styled(" remote ", Style::default().fg(CLAUDE_ORANGE)),
                Span::styled(label, Style::default().fg(Color::DarkGray)),
            ]));
        }
        crate::bridge_state::BridgeConnectionState::Reconnecting { attempt } => {
            lines.push(Line::from(vec![
                Span::styled(" remote ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("Reconnecting remote session (attempt #{attempt})"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        crate::bridge_state::BridgeConnectionState::Failed { reason } => {
            lines.push(Line::from(vec![
                Span::styled(" remote ", Style::default().fg(Color::Red)),
                Span::styled(
                    truncate_end(reason, max_width),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        _ => {}
    }

    if let Some(url) = app.remote_session_url.as_deref() {
        lines.push(Line::from(vec![
            Span::styled(" link ", Style::default().fg(CLAUDE_ORANGE)),
            Span::styled(
                truncate_end(url, max_width),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    // Home-directory warning: mirrors TS feedConfigs.tsx warningText.
    // "Note: You have launched claude in your home directory. For the best
    //  experience, launch it in a project directory instead."
    if app.home_dir_warning {
        lines.push(Line::from(vec![
            Span::styled(" note ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(
                truncate_end(
                    "You have launched claude in your home directory. \
                     For the best experience, launch it in a project directory instead.",
                    max_width,
                ),
                Style::default().fg(Color::Yellow),
            ),
        ]));
    }

    // Additional directories (from --add-dir)
    for dir in &app.config.additional_dirs {
        lines.push(Line::from(vec![
            Span::styled(" +dir ", Style::default().fg(Color::Cyan)),
            Span::styled(
                truncate_end(&dir.display().to_string(), max_width),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    lines
}

fn render_startup_notices(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let lines = startup_notice_lines(app, area.width);
    if lines.is_empty() {
        return;
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

#[derive(Clone)]
struct RenderedLineItem {
    line: Line<'static>,
    search_text: String,
    is_header: bool,
}

impl VirtualItem for RenderedLineItem {
    fn measure_height(&self, _width: u16) -> u16 {
        1
    }

    fn render(&self, area: Rect, buf: &mut Buffer, _selected: bool) {
        Paragraph::new(vec![self.line.clone()]).render(area, buf);
    }

    fn search_text(&self) -> String {
        self.search_text.clone()
    }

    fn is_section_header(&self) -> bool {
        self.is_header
    }
}

fn flatten_line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.to_string())
        .collect::<Vec<_>>()
        .join("")
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MessageLinesCacheKey {
    width: u16,
    messages_ptr: usize,
    messages_len: usize,
    annotations_ptr: usize,
    annotations_len: usize,
    thinking_expanded_len: usize,
}

#[derive(Clone)]
struct MessageLinesCache {
    key: MessageLinesCacheKey,
    lines: Vec<RenderedLineItem>,
}

/// Cache key for completed messages only (no ptr — len change = new message).
#[derive(Clone, Copy, PartialEq, Eq)]
struct CompletedMsgCacheKey {
    width: u16,
    messages_len: usize,
    annotations_len: usize,
    thinking_expanded_len: usize,
}

#[derive(Clone)]
struct CompletedMsgCache {
    key: CompletedMsgCacheKey,
    lines: Vec<RenderedLineItem>,
}

thread_local! {
    static MESSAGE_LINES_CACHE: RefCell<Option<MessageLinesCache>> = const { RefCell::new(None) };
    /// Stores rendered lines for committed messages only; valid even during streaming.
    static COMPLETED_MSG_CACHE: RefCell<Option<CompletedMsgCache>> = const { RefCell::new(None) };
}

// -----------------------------------------------------------------------
// Top-level layout
// -----------------------------------------------------------------------

/// Render the entire application into the current frame.
pub fn render_app(frame: &mut Frame, app: &App) {
    let size = frame.area();

    // Fill the entire frame with a black background so the terminal's default
    // color (blue on Windows) doesn't bleed through cells not covered by widgets.
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Black).fg(Color::White)),
        size,
    );

    let prompt_focused =
        !app.is_streaming && app.permission_request.is_none() && !app.history_search_overlay.visible;
    let status_visible = should_render_status_row(app);
    // One blank separator row above the status/input area when status is active,
    // matching the visual breathing room in the TS layout.
    let separator_height: u16 = if status_visible { 1 } else { 0 };
    let status_height: u16 = if status_visible { 1 } else { 0 };
    let suggestions_height = if prompt_focused && !app.prompt_input.suggestions.is_empty() {
        app.prompt_input.suggestions.len().min(5) as u16
    } else {
        0
    };
    let prompt_height = input_height(&app.prompt_input);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(separator_height),
            Constraint::Length(status_height),
            Constraint::Length(prompt_height),
            Constraint::Length(suggestions_height),
            Constraint::Length(1),
        ])
        .split(size);

    render_messages(frame, app, chunks[0]);
    // chunks[1] is the blank separator — intentionally left empty
    if status_height > 0 {
        render_status_row(frame, app, chunks[2]);
    }
    render_input(frame, app, chunks[3], prompt_focused);
    if suggestions_height > 0 {
        render_prompt_suggestions(frame, app, chunks[4]);
    }
    render_footer(frame, app, chunks[5]);

    // Overlays (rendered on top in Z-order)

    // Permission dialog (highest priority)
    if let Some(ref pr) = app.permission_request {
        render_permission_dialog(frame, pr, size);
    }

    // Rewind flow (takes over screen)
    if app.rewind_flow.visible {
        render_rewind_flow(frame, &app.rewind_flow, size);
    }

    // New help overlay
    if app.help_overlay.visible {
        render_help_overlay(frame, &app.help_overlay, size);
    } else if app.show_help {
        // Legacy fallback â€” render the simple help overlay
        render_simple_help_overlay(frame, size);
    }

    // History search overlay
    if app.history_search_overlay.visible {
        render_history_search_overlay(
            frame,
            &app.history_search_overlay,
            &app.prompt_input.history,
            size,
        );
    } else if let Some(ref hs) = app.history_search {
        // Legacy history search rendering
        render_legacy_history_search(frame, hs, app, size);
    }

    // Settings screen (highest-priority full-screen overlay)
    if app.settings_screen.visible {
        render_settings_screen(frame, &app.settings_screen, size);
    }

    // Theme picker overlay
    if app.theme_screen.visible {
        render_theme_screen(frame, &app.theme_screen, size);
    }

    // Privacy settings dialog
    if app.privacy_screen.visible {
        render_privacy_screen(frame, &app.privacy_screen, size);
    }

    if app.stats_dialog.open {
        render_stats_dialog(&app.stats_dialog, size, frame.buffer_mut());
    }

    if app.mcp_view.open {
        render_mcp_view(&app.mcp_view, size, frame.buffer_mut());
    }

    if app.agents_menu.open {
        render_agents_menu(&app.agents_menu, size, frame.buffer_mut());
    }

    if app.diff_viewer.open {
        let mut state = app.diff_viewer.clone();
        render_diff_dialog(&mut state, size, frame.buffer_mut());
    }

    if app.global_search.open {
        render_global_search(&app.global_search, size, frame.buffer_mut());
    }

    if app.feedback_survey.visible {
        render_feedback_survey(&app.feedback_survey, size, frame.buffer_mut());
    }

    if app.memory_file_selector.visible {
        render_memory_file_selector(&app.memory_file_selector, size, frame.buffer_mut());
    }

    if app.hooks_config_menu.visible {
        render_hooks_config_menu(&app.hooks_config_menu, size, frame.buffer_mut());
    }

    // Overage credit upsell banner
    if app.overage_upsell.visible {
        let banner_h = app.overage_upsell.height();
        if size.height > banner_h + 4 {
            let banner_area = Rect { x: size.x, y: size.y, width: size.width, height: banner_h };
            render_overage_upsell(&app.overage_upsell, banner_area, frame.buffer_mut());
        }
    }

    // Voice mode availability notice
    if app.voice_mode_notice.visible {
        let notice_h = app.voice_mode_notice.height();
        if size.height > notice_h + 4 {
            let notice_area = Rect { x: size.x, y: size.y, width: size.width, height: notice_h };
            render_voice_mode_notice(&app.voice_mode_notice, notice_area, frame.buffer_mut());
        }
    }

    // Memory update notification banner (bottom of message area)
    if app.memory_update_notification.visible {
        let notif_h = app.memory_update_notification.height();
        if size.height > notif_h + 4 {
            // Place at the bottom of the screen, just above the prompt bar area
            let notif_y = size.y + size.height.saturating_sub(notif_h + 4);
            let notif_area = Rect { x: size.x, y: notif_y, width: size.width, height: notif_h };
            render_memory_update_notification(
                &app.memory_update_notification,
                notif_area,
                frame.buffer_mut(),
            );
        }
    }

    // Desktop upsell startup modal
    if app.desktop_upsell.visible {
        render_desktop_upsell_startup(&app.desktop_upsell, size, frame.buffer_mut());
    }

    // Invalid config/settings dialog (shown when settings.json or CLAUDE.md is malformed)
    if app.invalid_config_dialog.visible {
        render_invalid_config_dialog(frame, &app.invalid_config_dialog, size);
    }

    // Bypass-permissions confirmation dialog (topmost — rendered last so it sits above all)
    if app.bypass_permissions_dialog.visible {
        render_bypass_permissions_dialog(frame, &app.bypass_permissions_dialog, size);
    }

    // First-launch onboarding dialog (shown after bypass dialog, below elicitation)
    if app.onboarding_dialog.visible {
        render_onboarding_dialog(frame, &app.onboarding_dialog, size);
    }

    // MCP elicitation dialog (highest priority modal — rendered last to sit on top)
    if app.elicitation.visible {
        render_elicitation_dialog(&app.elicitation, size, frame.buffer_mut());
    }

    // Model picker overlay
    if app.model_picker.visible {
        render_model_picker(&app.model_picker, size, frame.buffer_mut());
    }

    // Session browser overlay
    if app.session_browser.visible {
        render_session_browser(&app.session_browser, size, frame.buffer_mut());
    }

    // Export format picker dialog
    if app.export_dialog.visible {
        render_export_dialog(frame, &app.export_dialog, size);
    }

    // Context visualization overlay
    if app.context_viz.visible {
        render_context_viz(
            frame,
            &app.context_viz,
            size,
            app.context_used_tokens,
            app.context_window_size,
            app.rate_limit_5h_pct,
            app.rate_limit_7day_pct,
            app.cost_usd,
        );
    }

    // MCP approval dialog
    if app.mcp_approval.visible {
        render_mcp_approval_dialog(&app.mcp_approval, size, frame.buffer_mut());
    }

    // Notification banner (bottom of overlays stack so it's always visible)
    if !app.notifications.is_empty() {
        render_notification_banner(frame, &app.notifications, size);
    }

    // ---- Text selection highlight (topmost post-pass) ---------------------
    // Only show selection when no full-screen modal overlay is active.
    let modal_active = app.settings_screen.visible
        || app.theme_screen.visible
        || app.privacy_screen.visible
        || app.bypass_permissions_dialog.visible
        || app.rewind_flow.visible
        || app.help_overlay.visible
        || app.global_search.open;
    if !modal_active {
        apply_selection_highlight(frame, app, size);
    }
}

/// Post-render pass: invert colours on selected cells and extract the
/// selection text into `app.selection_text`.
fn apply_selection_highlight(frame: &mut Frame, app: &App, size: Rect) {
    let (anchor, focus) = match (app.selection_anchor, app.selection_focus) {
        (Some(a), Some(f)) => (a, f),
        _ => return,
    };
    if anchor == focus {
        return;
    }

    // Normalise so start ≤ end (row-major order).
    let (start, end) = if (anchor.1, anchor.0) <= (focus.1, focus.0) {
        (anchor, focus)
    } else {
        (focus, anchor)
    };

    let buf = frame.buffer_mut();
    let mut text = String::new();
    let last_row = end.1.min(size.y + size.height - 1);
    for row in start.1..=last_row {
        let col_from = if row == start.1 { start.0 } else { 0 };
        let col_to = if row == end.1 { end.0 } else { size.x + size.width - 1 };
        let col_to = col_to.min(size.x + size.width - 1);
        for col in col_from..=col_to {
            if let Some(cell) = buf.cell_mut((col, row)) {
                let sym = cell.symbol().to_owned();
                text.push_str(if sym.is_empty() || sym == "\0" { " " } else { &sym });
                // Highlight: white background, black foreground
                let new_style = Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(200, 200, 200));
                cell.set_style(new_style);
            }
        }
        if row < last_row {
            // Trim trailing spaces from line before newline
            while text.ends_with(' ') { text.pop(); }
            text.push('\n');
        }
    }
    while text.ends_with(|c: char| c.is_whitespace()) { text.pop(); }
    *app.selection_text.borrow_mut() = text;
}

// -----------------------------------------------------------------------
// Messages pane
// -----------------------------------------------------------------------

fn render_messages(frame: &mut Frame, app: &App, area: Rect) {
    // Reserve space at the top for plugin hint banners
    let hint_height = if app.plugin_hints.iter().any(|h| h.is_visible()) {
        3u16
    } else {
        0
    };

    let (hint_area, content_area) = if hint_height > 0 && area.height > hint_height + 2 {
        let splits = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(hint_height), Constraint::Min(1)])
            .split(area);
        (Some(splits[0]), splits[1])
    } else {
        (None, area)
    };

    // Render plugin hint banner if there is one
    if let Some(ha) = hint_area {
        render_plugin_hints(frame, &app.plugin_hints, ha);
    }

    let notice_lines = startup_notice_lines(app, content_area.width);
    let header_height = WELCOME_BOX_HEIGHT + notice_lines.len() as u16;
    let show_logo_header = content_area.height >= header_height + 3 && content_area.width >= 60;
    let (logo_area, notices_area, msg_area) = if show_logo_header {
        let splits = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(header_height), Constraint::Min(1)])
            .split(content_area);
        if notice_lines.is_empty() {
            (Some(splits[0]), None, splits[1])
        } else {
            let header_splits = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(WELCOME_BOX_HEIGHT),
                    Constraint::Length(notice_lines.len() as u16),
                ])
                .split(splits[0]);
            (Some(header_splits[0]), Some(header_splits[1]), splits[1])
        }
    } else {
        (None, None, content_area)
    };

    if let Some(la) = logo_area {
        render_welcome_box(frame, app, la);
        if let Some(na) = notices_area {
            render_startup_notices(frame, app, na);
        }
    } else if app.messages.is_empty() && app.streaming_text.is_empty() {
        render_welcome_box(frame, app, content_area);
        return;
    }

    let lines = render_message_items(app, msg_area.width);

    // Highlight search matches in transcript when global search is active
    let lines = if app.global_search.open && !app.global_search.query.is_empty() {
        let query_lc = app.global_search.query.to_lowercase();
        lines.into_iter().map(|mut item| {
            if item.search_text.to_lowercase().contains(query_lc.as_str()) {
                // Re-render the line with yellow highlight on matching spans
                let highlighted_spans: Vec<Span<'static>> = item.line.spans.into_iter().map(|span| {
                    if span.content.to_lowercase().contains(query_lc.as_str()) {
                        Span::styled(
                            span.content,
                            span.style.bg(Color::Rgb(60, 50, 0)).fg(Color::Yellow),
                        )
                    } else {
                        span
                    }
                }).collect();
                item.line = ratatui::text::Line::from(highlighted_spans);
            }
            item
        }).collect()
    } else {
        lines
    };

    // Compute total virtual height and apply scroll clamping.
    // When auto_scroll is on we always show the tail; otherwise we respect
    // the user's scroll_offset.
    let content_height = lines.len() as u16;
    let visible_height = msg_area.height;  // no borders, full height available
    let max_scroll = content_height.saturating_sub(visible_height) as usize;
    // scroll_offset counts lines above the bottom (0 = at bottom).
    // ratatui scroll() takes an absolute top-row index, so convert:
    //   top_row = max_scroll - scroll_offset  (clamped to [0, max_scroll])
    let scroll = if app.auto_scroll {
        max_scroll
    } else {
        max_scroll.saturating_sub(app.scroll_offset)
    };

    // No border — messages render directly into the area.
    let mut list = VirtualList::new();
    list.viewport_height = msg_area.height;
    list.sticky_bottom = app.auto_scroll;
    list.set_items(lines);
    list.scroll_offset = scroll as u16;
    list.render(msg_area, frame.buffer_mut());

    // "â†“ N new messages" indicator when scrolled up and new messages arrived.
    if app.new_messages_while_scrolled > 0 && msg_area.height > 4 && msg_area.width > 20 {
        let indicator = format!(
            " \u{2193} {} new message{} ",
            app.new_messages_while_scrolled,
            if app.new_messages_while_scrolled == 1 { "" } else { "s" }
        );
        let ind_len = indicator.len() as u16;
        let ind_x = msg_area
            .x
            .saturating_add(msg_area.width.saturating_sub(ind_len + 2));
        let ind_y = msg_area.y + msg_area.height.saturating_sub(1);
        let ind_area = Rect {
            x: ind_x,
            y: ind_y,
            width: ind_len.min(msg_area.width.saturating_sub(2)),
            height: 1,
        };
        let ind_line = Line::from(vec![Span::styled(
            indicator,
            Style::default()
                .fg(Color::Black)
                .bg(CLAUDE_ORANGE)
                .add_modifier(Modifier::BOLD),
        )]);
        frame.render_widget(Paragraph::new(vec![ind_line]), ind_area);
    }
}

fn render_message_items(app: &App, width: u16) -> Vec<RenderedLineItem> {
    let streaming = !app.streaming_text.is_empty();
    let has_tool_blocks = !app.tool_use_blocks.is_empty();
    let cacheable = !streaming && !has_tool_blocks;

    // Fast path: nothing live — use the full-result cache (ptr-stable check).
    let full_key = MessageLinesCacheKey {
        width,
        messages_ptr: app.messages.as_ptr() as usize,
        messages_len: app.messages.len(),
        annotations_ptr: app.system_annotations.as_ptr() as usize,
        annotations_len: app.system_annotations.len(),
        thinking_expanded_len: app.thinking_expanded.len(),
    };
    if cacheable {
        if let Some(lines) = MESSAGE_LINES_CACHE.with(|cache| {
            cache
                .borrow()
                .as_ref()
                .filter(|c| c.key == full_key)
                .map(|c| c.lines.clone())
        }) {
            return lines;
        }
    }

    // Build or retrieve the completed-messages portion.
    // This cache is always valid while messages_len and annotations_len are unchanged,
    // even during active streaming — so we only re-render committed messages when a
    // new message is actually added, not on every streaming frame.
    let completed_key = CompletedMsgCacheKey {
        width,
        messages_len: app.messages.len(),
        annotations_len: app.system_annotations.len(),
        thinking_expanded_len: app.thinking_expanded.len(),
    };
    let completed_lines: Vec<RenderedLineItem> =
        if let Some(lines) = COMPLETED_MSG_CACHE.with(|cache| {
            cache
                .borrow()
                .as_ref()
                .filter(|c| c.key == completed_key)
                .map(|c| c.lines.clone())
        }) {
            lines
        } else {
            let tool_names = build_tool_names(&app.messages);
            let mut raw: Vec<Line> = Vec::new();
            let mut header_indices: std::collections::HashSet<usize> = std::collections::HashSet::new();
            let total = app.messages.len();
            for i in 0..=total {
                for ann in app.system_annotations.iter().filter(|a| a.after_index == i) {
                    render_system_annotation_lines(&mut raw, ann, width as usize);
                }
                if i < total {
                    // Mark the first line of each message as a section header
                    let msg_start = raw.len();
                    render_message_lines(&mut raw, &app.messages[i], width as usize, &tool_names, &app.thinking_expanded);
                    if raw.len() > msg_start {
                        header_indices.insert(msg_start);
                    }
                }
            }
            let items: Vec<RenderedLineItem> = raw
                .into_iter()
                .enumerate()
                .map(|(idx, line)| RenderedLineItem {
                    search_text: flatten_line_text(&line),
                    is_header: header_indices.contains(&idx),
                    line,
                })
                .collect();
            COMPLETED_MSG_CACHE.with(|cache| {
                *cache.borrow_mut() = Some(CompletedMsgCache {
                    key: completed_key,
                    lines: items.clone(),
                });
            });
            items
        };

    // If there is no live content, store in the full cache and return.
    if cacheable {
        MESSAGE_LINES_CACHE.with(|cache| {
            *cache.borrow_mut() = Some(MessageLinesCache {
                key: full_key,
                lines: completed_lines.clone(),
            });
        });
        return completed_lines;
    }

    // Append live tool blocks and streaming text (cheap — small and transient).
    let mut items = completed_lines;
    let mut live: Vec<Line> = Vec::new();

    for block in &app.tool_use_blocks {
        render_tool_block_lines(&mut live, block, app.frame_count);
    }

    if streaming {
        let rendered = render_markdown(&app.streaming_text, width);
        let mut first = true;
        for line in rendered {
            let mut spans = line.spans;
            if first {
                let mut prefixed = Vec::with_capacity(spans.len() + 1);
                prefixed.push(Span::styled(
                    "\u{2022} ",
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ));
                prefixed.extend(spans);
                spans = prefixed;
                first = false;
            }
            live.push(Line::from(spans));
        }
    }

    items.extend(live.into_iter().map(|line| RenderedLineItem {
        search_text: flatten_line_text(&line),
        is_header: false,
        line,
    }));
    items
}

// â”€â”€ Welcome / startup screen â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Render the two-column orange round-bordered welcome box (matches TS LogoV2).
fn render_welcome_box(frame: &mut Frame, app: &App, area: Rect) {
    // Shorten cwd: replace $USERPROFILE/$HOME prefix with ~
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .ok();
            if let Some(h) = home {
                let hs = p.display().to_string();
                if hs.starts_with(&h) {
                    return Some(format!("~{}", &hs[h.len()..]));
                }
            }
            Some(p.display().to_string())
        })
        .unwrap_or_else(|| ".".to_string());

    // --- Box dimensions ---
    // The box should be at most the full area width, and a fixed height.
    let box_width = area.width.min(area.width);
    let box_height: u16 = WELCOME_BOX_HEIGHT;
    if area.height < box_height || box_width < 30 {
        // Too small: fall back to a single line
        let line = Line::from(vec![
            Span::styled("Claude Code ", Style::default().fg(CLAUDE_ORANGE).add_modifier(Modifier::BOLD)),
            Span::styled(format!("v{}", APP_VERSION), Style::default().fg(Color::DarkGray)),
        ]);
        frame.render_widget(Paragraph::new(vec![line]), area);
        return;
    }
    let box_area = Rect { x: area.x, y: area.y, width: box_width, height: box_height };

    // Outer border with title "Claude Code vX.Y"
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(CLAUDE_ORANGE))
        .title(Line::from(vec![
            Span::styled(" Claude Code ", Style::default().fg(CLAUDE_ORANGE).add_modifier(Modifier::BOLD)),
            Span::styled(format!("v{} ", APP_VERSION), Style::default().fg(Color::DarkGray)),
        ]));
    frame.render_widget(outer_block, box_area);

    // Inner area (inside the border)
    let inner = Rect {
        x: box_area.x + 1,
        y: box_area.y + 1,
        width: box_area.width.saturating_sub(2),
        height: box_area.height.saturating_sub(2),
    };

    // Split inner into left | divider(1) | right
    // Left width: ~28 chars or half the inner width, whichever is smaller
    let left_w = (inner.width / 2).max(22).min(32).min(inner.width.saturating_sub(3));
    let right_w = inner.width.saturating_sub(left_w + 1);
    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(left_w),
            Constraint::Length(1),
            Constraint::Length(right_w),
        ])
        .split(inner);

    // Draw orange vertical divider
    let divider_lines: Vec<Line> = (0..inner.height)
        .map(|_| Line::from(Span::styled("\u{2502}", Style::default().fg(CLAUDE_ORANGE))))
        .collect();
    frame.render_widget(Paragraph::new(divider_lines), h_chunks[1]);

    // --- Left column ---
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|u| !u.is_empty());
    let welcome_msg = if let Some(ref name) = username {
        format!("Welcome back {}!", name)
    } else {
        "Welcome back!".to_string()
    };
    let clawd = clawd_lines(&ClawdPose::Default);
    let model_line = format!("{} \u{00b7} API Usage Billing", app.model_name);

    let mut left_lines: Vec<Line> = Vec::new();
    left_lines.push(Line::from(Span::styled(
        welcome_msg,
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    )));
    left_lines.push(Line::from(""));
    // Center mascot in left column
    let mascot_indent = left_w.saturating_sub(11) / 2;
    let pad = " ".repeat(mascot_indent as usize);
    for cl in &clawd {
        let mut spans = vec![Span::raw(pad.clone())];
        spans.extend(cl.spans.iter().cloned());
        left_lines.push(Line::from(spans));
    }
    left_lines.push(Line::from(""));
    left_lines.push(Line::from(Span::styled(
        model_line,
        Style::default().fg(Color::DarkGray),
    )));
    left_lines.push(Line::from(Span::styled(
        cwd,
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(left_lines).wrap(Wrap { trim: false }), h_chunks[0]);

    // --- Right column ---
    let tip_text = cc_core::tips::select_tip(0)
        .map(|t| t.content.to_string())
        .unwrap_or_else(|| "Run /init to create a CLAUDE.md file with instructions for Claude".to_string());

    let mut right_lines: Vec<Line> = Vec::new();
    right_lines.push(Line::from(Span::styled(
        "Tips for getting started",
        Style::default().fg(CLAUDE_ORANGE).add_modifier(Modifier::BOLD),
    )));
    // Word-wrap the tip text into the right column width
    let right_w_usize = right_w.saturating_sub(1) as usize;
    for chunk in tip_text.chars().collect::<Vec<_>>().chunks(right_w_usize.max(1)) {
        right_lines.push(Line::from(chunk.iter().collect::<String>()));
    }
    right_lines.push(Line::from(""));
    right_lines.push(Line::from(Span::styled(
        "Recent activity",
        Style::default().fg(CLAUDE_ORANGE).add_modifier(Modifier::BOLD),
    )));
    right_lines.push(Line::from(Span::styled(
        "No recent activity",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(right_lines).wrap(Wrap { trim: false }), h_chunks[2]);
}

// â”€â”€ Per-message rendering â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Build a tool_use_id → tool_name lookup from all messages in the transcript.
/// This allows ToolResult blocks to dispatch to tool-specific renderers.
fn build_tool_names(messages: &[cc_core::types::Message]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for msg in messages {
        for block in msg.content_blocks() {
            if let cc_core::types::ContentBlock::ToolUse { id, name, .. } = block {
                map.insert(id.clone(), name.clone());
            }
        }
    }
    map
}

fn render_message_lines(
    lines: &mut Vec<Line<'static>>,
    msg: &cc_core::types::Message,
    width: usize,
    tool_names: &std::collections::HashMap<String, String>,
    expanded_thinking: &std::collections::HashSet<u64>,
) {
    let rendered = render_message(
        msg,
        &RenderContext {
            width: width as u16,
            highlight: true,
            show_thinking: false,
            tool_names: tool_names.clone(),
            expanded_thinking: expanded_thinking.clone(),
        },
    );

    // Truncate very long outputs with a "â€¦ N more lines" notice
    const MAX_LINES_PER_MSG: usize = 200;
    if rendered.len() > MAX_LINES_PER_MSG {
        lines.extend(rendered[..MAX_LINES_PER_MSG].iter().cloned());
        lines.push(Line::from(vec![Span::styled(
            format!(
                "  \u{2026} {} more lines (scroll up to read all)",
                rendered.len() - MAX_LINES_PER_MSG
            ),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )]));
    } else {
        lines.extend(rendered);
    }
}

// â”€â”€ System annotation (compact boundary, info notices) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

fn render_system_annotation_lines(
    lines: &mut Vec<Line<'static>>,
    ann: &SystemAnnotation,
    width: usize,
) {
    // Compact boundary: show âœ» prefix with dimmed text
    if ann.style == SystemMessageStyle::Compact {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", figures::TEARDROP_ASTERISK),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                ann.text.clone(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        ]));
        lines.push(Line::from(""));
        return;
    }

    let (text_color, border_color) = match ann.style {
        SystemMessageStyle::Info => (Color::DarkGray, Color::DarkGray),
        SystemMessageStyle::Warning => (Color::Yellow, Color::Yellow),
        SystemMessageStyle::Compact => unreachable!(),
    };

    // Centred, padded rule: "â”€â”€â”€ text â”€â”€â”€"
    let text = ann.text.as_str();
    let inner_width = width.saturating_sub(4);
    let text_len = text.len();
    let dashes = inner_width.saturating_sub(text_len + 2);
    let left = dashes / 2;
    let right = dashes - left;

    lines.push(Line::from(vec![
        Span::styled(
            format!("  {}", "\u{2500}".repeat(left)),
            Style::default().fg(border_color),
        ),
        Span::styled(
            format!("\u{2500} {} \u{2500}", text),
            Style::default().fg(text_color).add_modifier(Modifier::DIM),
        ),
        Span::styled(
            "\u{2500}".repeat(right),
            Style::default().fg(border_color),
        ),
    ]));
    lines.push(Line::from(""));
}

// â”€â”€ Tool use block â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

fn render_tool_block_lines(lines: &mut Vec<Line<'static>>, block: &crate::app::ToolUseBlock, frame_count: u64) {
    // ● icon: blinks Yellow↔DarkGray when running, solid Green/Red when done/error
    let (icon_color, name_color) = match block.status {
        ToolStatus::Running => {
            let blink_on = (frame_count / 4) % 2 == 0;
            let c = if blink_on { Color::Yellow } else { Color::DarkGray };
            (c, Color::White)
        }
        ToolStatus::Done => (Color::Green, Color::White),
        ToolStatus::Error => (Color::Red, Color::Red),
    };

    // Extract summary from input_json for header
    let input_val: serde_json::Value =
        serde_json::from_str(&block.input_json).unwrap_or(serde_json::Value::Null);
    let summary = crate::messages::extract_tool_summary(&block.name, &input_val);

    // Header: ● ToolName (summary)
    let mut header_spans = vec![
        Span::styled("  \u{25cf} ".to_string(), Style::default().fg(icon_color)),
        Span::styled(
            block.name.clone(),
            Style::default().fg(name_color).add_modifier(Modifier::BOLD),
        ),
    ];
    if !summary.is_empty() {
        header_spans.push(Span::styled(
            format!(" ({})", summary),
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::from(header_spans));

    // Bash/PowerShell: show `$ command` body
    if block.name == "Bash" || block.name == "PowerShell" {
        let command = input_val
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        for (i, cmd_line) in command.lines().enumerate() {
            if i >= 2 {
                break;
            }
            let display: String = cmd_line.chars().take(160).collect();
            let display = if cmd_line.chars().count() > 160 {
                format!("{}\u{2026}", display)
            } else {
                display
            };
            lines.push(Line::from(vec![
                Span::styled(
                    "    $ ".to_string(),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    display,
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]));
        }
    }

    // Output preview (done/error state)
    if let Some(ref preview) = block.output_preview {
        let preview_style = match block.status {
            ToolStatus::Error => Style::default().fg(Color::Red),
            _ => Style::default().fg(Color::DarkGray),
        };
        for line_text in preview.lines() {
            if line_text.starts_with('\u{2026}') {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        line_text.to_string(),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    ),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(line_text.to_string(), preview_style),
                ]));
            }
        }
    }

    // (ctrl+o to expand) hint
    lines.push(Line::from(vec![Span::styled(
        "  (ctrl+o to expand)".to_string(),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )]));
}

// -----------------------------------------------------------------------
// Input pane
// -----------------------------------------------------------------------

fn render_input(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    render_prompt_input(
        &app.prompt_input,
        area,
        frame.buffer_mut(),
        focused,
        if app.is_streaming {
            InputMode::Readonly
        } else if app.plan_mode {
            InputMode::Plan
        } else {
            InputMode::Default
        },
    );
}

fn should_render_status_row(app: &App) -> bool {
    app.voice_recording
        || app.is_streaming
        || app.last_turn_elapsed.is_some()
        || (!app.is_streaming && app.status_message.is_some())
}

fn render_status_row(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }

    let spans = if app.voice_recording {
        vec![Span::styled(
            format!("{} Recording... press Alt+V to transcribe", figures::black_circle()),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]
    } else if app.is_streaming {
        // Spinner glyph (turns red on stall)
        let frame_count = animation_tick_200ms();
        let mut s = vec![Span::styled(
            spinner_char(frame_count).to_string(),
            Style::default().fg(spinner_color(app)).add_modifier(Modifier::BOLD),
        )];

        // Label: explicit tool status takes priority over the thinking verb
        let raw_label = app.status_message.as_deref()
            .or(app.spinner_verb.as_deref())
            .unwrap_or("Thinking");
        let label = format!("{}…", raw_label.trim_end_matches('…'));

        s.push(Span::raw(" "));
        s.extend(shimmer_spans(&label, app.frame_count));
        s
    } else if let (Some(verb), Some(elapsed)) = (app.last_turn_verb, app.last_turn_elapsed.as_deref()) {
        // "✽ Worked for 2m 5s" — mirrors TS TeammateSpinnerLine idle state
        vec![Span::styled(
            format!("{} {} for {}", figures::TEARDROP_ASTERISK, verb, elapsed),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        )]
    } else if let Some(status) = app.status_message.as_deref() {
        vec![Span::styled(status.to_string(), Style::default().fg(Color::DarkGray))]
    } else {
        Vec::new()
    };

    if spans.is_empty() {
        return;
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Build spans for a text string with a right-to-left glimmer sweep, matching
/// the TS `GlimmerMessage` behaviour (glimmerSpeed=200ms, 3-char shimmer window).
///
/// At ~50ms per frame a 4-frame step ≈ 200ms, giving the same cadence as TS.
fn shimmer_spans(text: &str, frame_count: u64) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    if len == 0 {
        return Vec::new();
    }

    // Cycle length = text_len + 20 (10 off-screen on each side)
    let cycle_len = len + 20;
    // One step every 4 frames (~200ms at 50ms/frame)
    let cycle_pos = (frame_count as usize / 4) % cycle_len;
    // Glimmer sweeps right→left: starts at len+10 (off right), ends at -10 (off left)
    let glimmer_center = (len + 10).saturating_sub(cycle_pos) as isize;

    let base = Style::default().fg(Color::DarkGray);
    let bright = Style::default().fg(Color::White);

    // Accumulate runs of same style to minimise span count
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_bright = false;

    for (i, &ch) in chars.iter().enumerate() {
        let is_bright = (i as isize - glimmer_center).abs() <= 1
            && glimmer_center >= 0
            && glimmer_center < len as isize;

        if is_bright != run_bright && !run.is_empty() {
            spans.push(Span::styled(run.clone(), if run_bright { bright } else { base }));
            run.clear();
        }
        run_bright = is_bright;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_bright { bright } else { base }));
    }
    spans
}
// Keybinding hints footer
// -----------------------------------------------------------------------

/// Single footer line matching the TS contract more closely:
/// - `? for shortcuts` is suppressed once the prompt becomes non-empty
/// - the right side shows comprehensive status info and notifications
fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }

    // Left side: ordered pills — voice > PR badge > background task > vim > hint
    let left_spans: Vec<Span> = if app.voice_recording {
        vec![Span::styled(
            format!(" {} REC — speak now", figures::black_circle()),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]
    } else {
        let mut spans: Vec<Span> = Vec::new();

        // Agent type badge (shown when running as subagent / coordinator)
        if let Some(ref badge) = app.agent_type_badge {
            spans.push(Span::styled(
                format!("\u{2699} {}", badge),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
        }

        // PR badge — shows "PR #<n>" in cyan, with optional state in brackets.
        // State color: approved=green, changes_requested=red,
        //              review_required=yellow, else=gray.
        if let Some(pr_num) = app.pr_number {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            let pr_label = match &app.pr_state {
                Some(state) => format!("PR #{} [{}]", pr_num, state),
                None => format!("PR #{}", pr_num),
            };
            let pr_color = match app.pr_state.as_deref() {
                Some("approved") => Color::Green,
                Some("changes_requested") => Color::Red,
                Some("review_required") => Color::Yellow,
                Some(_) => Color::Gray,
                None => Color::Cyan,
            };
            spans.push(Span::styled(
                pr_label,
                Style::default().fg(pr_color).add_modifier(Modifier::BOLD),
            ));
        }

        // Background task status pill — shows "⟳ N tasks" when count > 0.
        // Falls back to background_task_status pre-formatted string if set.
        if app.background_task_count > 0 {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            let label = if app.background_task_count == 1 {
                "\u{27f3} 1 task".to_string()
            } else {
                format!("\u{27f3} {} tasks", app.background_task_count)
            };
            spans.push(Span::styled(
                label,
                Style::default().fg(Color::Yellow),
            ));
        } else if let Some(ref task_status) = app.background_task_status {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                format!("\u{27f3} {}", task_status),
                Style::default().fg(Color::Yellow),
            ));
        }

        // Vim mode indicator (only when non-insert in vim mode, to save space)
        if app.prompt_input.vim_enabled && app.prompt_input.vim_mode == VimMode::Insert {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                "-- INSERT --",
                Style::default().fg(Color::DarkGray),
            ));
        }

        // Bash prefix indicator — shown when prompt starts with '!'
        if app.prompt_input.text.starts_with('!') {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                "[BASH]",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        }

        // Permission mode badge (left side, mirrors TS bottom-left indicator).
        // Default mode is silent; non-default modes show a badge.
        {
            use cc_core::config::PermissionMode;
            match &app.config.permission_mode {
                PermissionMode::BypassPermissions => {
                    if !spans.is_empty() { spans.push(Span::raw("  ")); }
                    spans.push(Span::styled(
                        "\u{23f5}\u{23f5} bypass",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                }
                PermissionMode::AcceptEdits => {
                    if !spans.is_empty() { spans.push(Span::raw("  ")); }
                    spans.push(Span::styled(
                        "accept-edits",
                        Style::default().fg(Color::Yellow),
                    ));
                }
                PermissionMode::Plan => {
                    if !spans.is_empty() { spans.push(Span::raw("  ")); }
                    spans.push(Span::styled(
                        "plan",
                        Style::default().fg(Color::Blue),
                    ));
                }
                PermissionMode::Default => {}
            }
        }

        // During streaming show "esc to interrupt"; otherwise "? for shortcuts" when idle
        if spans.is_empty() {
            if app.is_streaming {
                spans.push(Span::styled(
                    "esc to interrupt",
                    Style::default().fg(Color::DarkGray),
                ));
            } else if app.prompt_input.text.is_empty() {
                spans.push(Span::styled(
                    "? for shortcuts",
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }

        spans
    };

    // Right side: notifications take priority, otherwise build status parts
    let right_spans: Vec<Span> = if let Some(notification) = app.notifications.current() {
        vec![Span::styled(
            notification.message.clone(),
            Style::default()
                .fg(notification.kind.color())
                .add_modifier(Modifier::BOLD),
        )]
    } else {
        let mut parts: Vec<Span> = Vec::new();

        // 1. Context window usage — show "N% until auto-compact" mirroring TS TokenWarning
        if app.context_window_size > 0 {
            let used_pct = (app.context_used_tokens as f64 / app.context_window_size as f64 * 100.0) as u64;
            let left_pct = 100u64.saturating_sub(used_pct);

            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }

            if used_pct >= 95 {
                // Critical: red, urge /compact now
                parts.push(Span::styled(
                    format!("{}% context used — /compact now", used_pct),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
            } else if used_pct >= 70 {
                // Warning: show "N% until auto-compact" in yellow
                parts.push(Span::styled(
                    format!("{}% until auto-compact", left_pct),
                    Style::default().fg(Color::Yellow),
                ));
            } else {
                // Normal: dim green percentage
                let used_k = app.context_used_tokens / 1000;
                let total_k = app.context_window_size / 1000;
                parts.push(Span::styled(
                    format!("{}k/{}k", used_k, total_k),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }

        // 3. Cost
        if app.cost_usd > 0.0 {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                format!("${:.2}", app.cost_usd),
                Style::default().fg(Color::DarkGray),
            ));
        }

        // 4. Rate limits
        if let Some(pct) = app.rate_limit_5h_pct {
            if pct > 0.0 {
                if !parts.is_empty() {
                    parts.push(Span::raw("  "));
                }
                let color = if pct >= 90.0 { Color::Red } else { Color::Yellow };
                parts.push(Span::styled(
                    format!("5h:{:.0}%", pct),
                    Style::default().fg(color),
                ));
            }
        }
        if let Some(pct) = app.rate_limit_7day_pct {
            if pct > 0.0 {
                if !parts.is_empty() {
                    parts.push(Span::raw("  "));
                }
                let color = if pct >= 90.0 { Color::Red } else { Color::Yellow };
                parts.push(Span::styled(
                    format!("7d:{:.0}%", pct),
                    Style::default().fg(color),
                ));
            }
        }

        // 5. Vim mode indicator
        if app.prompt_input.vim_enabled {
            let (mode_str, mode_color) = match app.prompt_input.vim_mode {
                VimMode::Insert => ("[INSERT]", Color::Blue),
                VimMode::Normal => ("[NORMAL]", Color::Green),
                VimMode::Visual => ("[VISUAL]", Color::Magenta),
                VimMode::VisualLine => ("[VISUAL LINE]", Color::Magenta),
                VimMode::VisualBlock => ("[VISUAL BLOCK]", Color::Magenta),
                VimMode::Command => ("[COMMAND]", Color::Cyan),
                VimMode::Search => ("[SEARCH]", Color::Yellow),
            };
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(mode_str, Style::default().fg(mode_color)));
        }

        // 6. Agent type badge
        if let Some(ref badge) = app.agent_type_badge {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                format!("[{}]", badge),
                Style::default().fg(Color::Cyan),
            ));
        }

        // 7. Worktree branch
        if let Some(ref branch) = app.worktree_branch {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                format!("[{}]", branch),
                Style::default().fg(Color::Green),
            ));
        }


        // Output style indicator (only when non-default)
        if app.output_style != "auto" {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                format!("[{}]", app.output_style),
                Style::default().fg(Color::DarkGray),
            ));
        }

        // External status line override
        if let Some(ref override_text) = app.status_line_override {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            // Strip any ANSI escapes for terminal rendering (plain text)
            let clean: String = override_text
                .chars()
                .filter(|c| c.is_ascii_graphic() || *c == ' ')
                .collect();
            parts.push(Span::styled(clean, Style::default().fg(Color::DarkGray)));
        }

        // 8. Bridge badge
        if let Some(badge) = app.bridge_state.status_badge(app.frame_count) {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(badge);
        } else if app.pending_mcp_reconnect {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                "MCP reconnecting",
                Style::default().fg(Color::Yellow),
            ));
        }

        parts
    };

    // Gap fill
    let left_len: usize = left_spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let right_len: usize = right_spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let gap = (area.width as usize).saturating_sub(left_len + right_len);

    let mut spans = left_spans;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right_spans);

    frame.render_widget(Paragraph::new(vec![Line::from(spans)]), area);
}

fn render_prompt_suggestions(frame: &mut Frame, app: &App, area: Rect) {
    let suggestions = &app.prompt_input.suggestions;
    if suggestions.is_empty() || area.height == 0 {
        return;
    }

    let selected = app.prompt_input.suggestion_index.unwrap_or(0);
    let max_visible = area.height as usize;
    let start = selected
        .saturating_sub(max_visible / 2)
        .min(suggestions.len().saturating_sub(max_visible));
    let end = (start + max_visible).min(suggestions.len());
    let label_width = area.width.saturating_div(3).max(12) as usize;

    for (row, suggestion) in suggestions[start..end].iter().enumerate() {
        let is_selected = start + row == selected;
        let accent_style = if is_selected {
            Style::default().fg(CLAUDE_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let label_style = if is_selected {
            Style::default().fg(CLAUDE_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let detail_style = if is_selected {
            Style::default().fg(CLAUDE_ORANGE)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut spans = vec![Span::styled(if is_selected { "\u{203a} " } else { "  " }, accent_style)];
        match suggestion.source {
            TypeaheadSource::SlashCommand => {
                let display_name = truncate_text(&suggestion.text, label_width);
                spans.push(Span::styled(
                    format!("{display_name:<width$}", width = label_width),
                    label_style,
                ));
                spans.push(Span::styled(" [cmd] ", Style::default().fg(Color::DarkGray)));
                if !suggestion.description.is_empty() {
                    spans.push(Span::styled(
                        truncate_text(
                            &suggestion.description,
                            area.width.saturating_sub(label_width as u16 + 10) as usize,
                        ),
                        detail_style,
                    ));
                }
            }
            TypeaheadSource::FileRef => {
                spans.push(Span::styled("+ ", accent_style));
                spans.push(Span::styled(
                    truncate_middle(&suggestion.text, label_width),
                    label_style,
                ));
                if !suggestion.description.is_empty() {
                    spans.push(Span::styled(" \u{2014} ", Style::default().fg(Color::DarkGray)));
                    spans.push(Span::styled(
                        truncate_text(&suggestion.description, area.width as usize / 2),
                        detail_style,
                    ));
                }
            }
            TypeaheadSource::History => {
                let display_name = truncate_text(&suggestion.text, label_width);
                spans.push(Span::styled(
                    format!("{display_name:<width$}", width = label_width),
                    label_style,
                ));
                spans.push(Span::styled(" [history] ", Style::default().fg(Color::DarkGray)));
                if !suggestion.description.is_empty() {
                    spans.push(Span::styled(
                        truncate_text(&suggestion.description, area.width as usize / 2),
                        detail_style,
                    ));
                }
            }
        }

        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                x: area.x,
                y: area.y + row as u16,
                width: area.width,
                height: 1,
            },
        );
    }
}

// -----------------------------------------------------------------------
// Legacy simple help overlay (fallback when help_overlay is not open)
// -----------------------------------------------------------------------

fn render_simple_help_overlay(frame: &mut Frame, area: Rect) {
    let help_width = 50u16.min(area.width.saturating_sub(4));
    let help_height = 20u16.min(area.height.saturating_sub(4));
    let help_area = crate::overlays::centered_rect(help_width, help_height, area);

    frame.render_widget(Clear, help_area);

    let lines = vec![
        Line::from(vec![Span::styled(
            " Key Bindings",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )]),
        Line::from(""),
        kb_line("Enter", "Submit message"),
        kb_line("Ctrl+C", "Cancel streaming / Quit"),
        kb_line("Ctrl+D", "Quit (empty input)"),
        kb_line("Up / Down", "Navigate input history"),
        kb_line("Ctrl+R", "Search input history"),
        kb_line("PageUp / PageDown", "Scroll messages"),
        kb_line("F1 / ?", "Toggle this help"),
        Line::from(""),
        Line::from(vec![Span::styled(
            " Permission Dialog",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )]),
        Line::from(""),
        kb_line("1 / 2 / 3", "Select option"),
        kb_line("y / a / n", "Allow / Always / Deny"),
        kb_line("Enter", "Confirm selection"),
        kb_line("Esc", "Deny (close dialog)"),
        Line::from(""),
        Line::from(vec![Span::styled(
            " press F1 or ? to close ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Left);
    frame.render_widget(para, help_area);
}

fn kb_line<'a>(key: &str, desc: &str) -> Line<'a> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:<20}", key),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(desc.to_string()),
    ])
}

// -----------------------------------------------------------------------
// Legacy history search overlay (used when history_search_overlay is not open)
// -----------------------------------------------------------------------

fn render_legacy_history_search(
    frame: &mut Frame,
    hs: &crate::app::HistorySearch,
    app: &App,
    area: Rect,
) {
    let dialog_width = 60u16.min(area.width.saturating_sub(4));
    let visible_matches = 8usize;
    let dialog_height =
        (4 + visible_matches.min(hs.matches.len().max(1)) as u16).min(area.height.saturating_sub(4));
    let dialog_area = crate::overlays::centered_rect(dialog_width, dialog_height, area);

    frame.render_widget(Clear, dialog_area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::raw("  Search: "),
        Span::styled(
            hs.query.clone(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled("\u{2588}", Style::default().fg(Color::White)),
    ]));
    lines.push(Line::from(""));

    if hs.matches.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  (no matches)",
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        let start = hs.selected.saturating_sub(visible_matches / 2);
        let end = (start + visible_matches).min(hs.matches.len());
        let start = end.saturating_sub(visible_matches).min(start);

        for (display_idx, &hist_idx) in hs.matches[start..end].iter().enumerate() {
            let real_idx = start + display_idx;
            let is_selected = real_idx == hs.selected;
            let entry = app
                .prompt_input
                .history
                .get(hist_idx)
                .map(String::as_str)
                .unwrap_or("");

            let truncated = if UnicodeWidthStr::width(entry) > (dialog_width as usize - 6) {
                let mut s = entry.to_string();
                s.truncate(dialog_width as usize - 9);
                format!("{}\u{2026}", s)
            } else {
                entry.to_string()
            };

            let (prefix, style) = if is_selected {
                (
                    "  \u{25BA} ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("    ", Style::default().fg(Color::White))
            };
            lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(truncated, style),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" History Search (Esc to cancel) ")
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, dialog_area);
}

// -----------------------------------------------------------------------
// Complete status line (T2-8)
// -----------------------------------------------------------------------

/// Complete status line data for rendering.
#[derive(Debug, Clone, Default)]
pub struct StatusLineData {
    pub model: String,
    pub tokens_used: u64,
    pub tokens_total: u64,
    pub cost_cents: f64,
    pub compact_warning_pct: Option<f64>,  // None = no warning; Some(pct) = show warning
    pub vim_mode: Option<String>,           // None = no vim mode; Some("NORMAL") etc.
    pub bridge_connected: bool,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
    pub agent_badge: Option<String>,
    pub rate_limit_pct_5h: Option<f64>,
    pub rate_limit_pct_7d: Option<f64>,
}

pub fn render_full_status_line(data: &StatusLineData, area: Rect, buf: &mut ratatui::buffer::Buffer) {
    use ratatui::{
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Paragraph, Widget},
    };

    let mut spans = Vec::new();

    // Model name
    if !data.model.is_empty() {
        spans.push(Span::styled(
            format!(" {} ", data.model),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::styled(" â”‚ ", Style::default().fg(Color::DarkGray)));
    }

    // Context window
    if data.tokens_total > 0 {
        let pct = data.tokens_used as f64 / data.tokens_total as f64;
        let ctx_color = if pct >= 0.95 { Color::Red } else if pct >= 0.80 { Color::Yellow } else { Color::Green };
        let used_k = data.tokens_used / 1000;
        let total_k = data.tokens_total / 1000;
        spans.push(Span::styled(
            format!("{}k/{}k ({:.0}%)", used_k, total_k, pct * 100.0),
            Style::default().fg(ctx_color),
        ));
        spans.push(Span::styled(" â”‚ ", Style::default().fg(Color::DarkGray)));
    }

    // Cost
    if data.cost_cents > 0.0 {
        spans.push(Span::styled(
            format!("${:.2}", data.cost_cents / 100.0),
            Style::default().fg(Color::White),
        ));
        spans.push(Span::styled(" â”‚ ", Style::default().fg(Color::DarkGray)));
    }

    // Compact warning
    if let Some(pct) = data.compact_warning_pct {
        if pct >= 0.80 {
            let color = if pct >= 0.95 { Color::Red } else { Color::Yellow };
            spans.push(Span::styled(
                format!("âš  ctx {:.0}% ", pct * 100.0),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        }
    }

    // Vim mode
    if let Some(mode) = &data.vim_mode {
        let color = match mode.as_str() {
            "NORMAL" => Color::Green,
            "INSERT" => Color::Blue,
            "VISUAL" => Color::Magenta,
            _ => Color::White,
        };
        spans.push(Span::styled(
            format!("[{}]", mode),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" ", Style::default()));
    }

    // Agent badge
    if let Some(badge) = &data.agent_badge {
        spans.push(Span::styled(
            format!("[{}]", badge),
            Style::default().fg(Color::Magenta),
        ));
        spans.push(Span::styled(" ", Style::default()));
    }

    // Bridge connected
    if data.bridge_connected {
        spans.push(Span::styled(
            "ðŸ”— ",
            Style::default().fg(Color::Green),
        ));
    }

    // Session ID
    if let Some(sid) = &data.session_id {
        let short = &sid[..sid.len().min(8)];
        spans.push(Span::styled(
            format!("[session:{}]", short),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Worktree
    if let Some(wt) = &data.worktree {
        spans.push(Span::styled(
            format!("[worktree:{}]", wt),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let line = Line::from(spans);
    Paragraph::new(line)
        .style(Style::default().bg(Color::Black))
        .render(area, buf);
}


// ---------------------------------------------------------------------------
// Multi-agent UI components
// ---------------------------------------------------------------------------

/// Render a single progress-indicator row for a sub-agent.
///
/// Format: `[agent-<id>]` in cyan dim · space · status in colour · ` · ` · tool in dim gray
///
/// # Arguments
/// * `agent_id`    — short agent identifier (e.g. `"abc123"`)
/// * `status`      — current status string: `"working"`, `"done"`, `"error"`, or other
/// * `current_tool` — tool the agent is currently executing, if any
pub fn render_agent_progress_line(
    agent_id: &str,
    status: &str,
    current_tool: Option<&str>,
) -> Line<'static> {
    let status_color = match status {
        "working" | "running" => Color::Yellow,
        "done" | "complete" | "completed" => Color::Green,
        "error" | "failed" => Color::Red,
        _ => Color::DarkGray,
    };

    let mut spans = vec![
        Span::styled(
            format!("[agent-{}]", agent_id),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::DIM),
        ),
        Span::raw(" "),
        Span::styled(
            status.to_string(),
            Style::default().fg(status_color),
        ),
    ];

    if let Some(tool) = current_tool {
        spans.push(Span::styled(
            " · ".to_string(),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            tool.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }

    Line::from(spans)
}

/// Render a multi-line coordinator status block for a multi-agent session.
///
/// Returns a `Vec<Line>` containing:
/// 1. A header: `Coordinator · N agents (M active)` in cyan bold
/// 2. One compact row per entry in `active_agents` using [`render_agent_progress_line`]
///
/// # Arguments
/// * `agent_count`   — total number of sub-agents spawned
/// * `completed`     — number of agents that have finished
/// * `active_agents` — slice of agent ID strings currently running
pub fn render_coordinator_status_lines(
    agent_count: usize,
    completed: usize,
    active_agents: &[&str],
) -> Vec<Line<'static>> {
    let active_count = active_agents.len();

    let header = Line::from(vec![
        Span::styled(
            "Coordinator".to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " · ".to_string(),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("{} agent{}", agent_count, if agent_count == 1 { "" } else { "s" }),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!(" ({} active)", active_count),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            if completed > 0 {
                format!("  ✔ {} done", completed)
            } else {
                String::new()
            },
            Style::default().fg(Color::Green),
        ),
    ]);

    let mut lines = vec![header];

    for agent_id in active_agents {
        let row = render_agent_progress_line(agent_id, "working", None);
        // Indent agent rows by two spaces
        let mut indented_spans = vec![Span::raw("  ")];
        indented_spans.extend(row.spans);
        lines.push(Line::from(indented_spans));
    }

    lines
}

/// Render a single header line for a teammate's message block.
///
/// Format: `┤ teammate: <id> ├` in magenta, optional `· <session_info>` in dim
///
/// # Arguments
/// * `teammate_id`  — teammate identifier string
/// * `session_info` — optional session info snippet to append
pub fn render_teammate_header(
    teammate_id: &str,
    session_info: Option<&str>,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            "┤ teammate: ".to_string(),
            Style::default().fg(Color::Magenta),
        ),
        Span::styled(
            teammate_id.to_string(),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " ├".to_string(),
            Style::default().fg(Color::Magenta),
        ),
    ];

    if let Some(info) = session_info {
        spans.push(Span::styled(
            "  · ".to_string(),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            info.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }

    Line::from(spans)
}
