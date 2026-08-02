// notifications.rs — Notification / banner system for the TUI.

use std::collections::VecDeque;
use std::time::Instant;

use crate::overlays::{CLAURST_ACCENT, CLAURST_MUTED, CLAURST_PANEL_BORDER, CLAURST_TEXT};
use unicode_width::UnicodeWidthStr;

/// Severity / visual style of a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationKind {
    Info,
    Warning,
    Error,
    Success,
}

/// A single notification entry.
#[derive(Debug, Clone)]
pub struct Notification {
    /// Unique identifier (used for dismissal).
    pub id: String,
    pub kind: NotificationKind,
    pub message: String,
    /// When this notification was created — used to calculate progress bar fill.
    pub pushed_at: Instant,
    /// When `Some`, the notification auto-expires at this instant.
    pub expires_at: Option<Instant>,
    /// Whether the user can manually dismiss this notification.
    pub dismissible: bool,
}

/// A FIFO queue of active notifications.
#[derive(Debug, Default)]
pub struct NotificationQueue {
    pub notifications: VecDeque<Notification>,
    next_id: u64,
}

impl NotificationQueue {
    pub fn new() -> Self {
        Self {
            notifications: VecDeque::new(),
            next_id: 0,
        }
    }

    /// Push a new notification.
    ///
    /// * `duration_secs` — `None` for persistent, `Some(n)` for auto-expire after *n* seconds.
    pub fn push(&mut self, kind: NotificationKind, msg: String, duration_secs: Option<u64>) {
        let pushed_at = Instant::now();
        let expires_at = duration_secs.map(|secs| pushed_at + std::time::Duration::from_secs(secs));
        self.notifications
            .retain(|n| !(n.kind == kind && n.message == msg));
        let id = format!("notif-{}", self.next_id);
        self.next_id += 1;
        self.notifications.push_back(Notification {
            id,
            kind,
            message: msg,
            pushed_at,
            expires_at,
            dismissible: true,
        });
    }

    /// Dismiss the notification with the given `id`.
    pub fn dismiss(&mut self, id: &str) {
        self.notifications.retain(|n| n.id != id);
    }

    /// Remove all expired notifications.  Call this once per render frame.
    pub fn tick(&mut self) {
        let now = Instant::now();
        self.notifications
            .retain(|n| n.expires_at.is_none_or(|exp| exp > now));
    }

    /// Return the currently visible (most recent) notification, if any.
    pub fn current(&self) -> Option<&Notification> {
        self.notifications.back()
    }

    /// Dismiss the currently visible notification.
    pub fn dismiss_current(&mut self) {
        if let Some(n) = self.notifications.back().cloned() {
            if n.dismissible {
                self.notifications.pop_back();
            }
        }
    }

    pub fn current_is_error(&self) -> bool {
        self.current()
            .is_some_and(|n| n.kind == NotificationKind::Error)
    }

    /// Return `true` if there are no active notifications.
    pub fn is_empty(&self) -> bool {
        self.notifications.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

impl NotificationKind {
    pub fn color(&self) -> Color {
        match self {
            NotificationKind::Info => CLAURST_ACCENT,
            NotificationKind::Warning => Color::Yellow,
            NotificationKind::Error => Color::Red,
            NotificationKind::Success => Color::Rgb(80, 200, 120),
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            NotificationKind::Info => "ℹ",
            NotificationKind::Warning => "⚠",
            NotificationKind::Error => "✗",
            NotificationKind::Success => "✓",
        }
    }
}

/// Render the topmost notification as a floating toast at the top-right of `area`.
///
/// Layout (3 rows):
///   row 0: ▐ [icon] [message truncated]          [Esc] ▌
///   row 1: ▐ [progress bar for timed notifs]            ▌
///   row 2: (bottom border row, blank)
pub fn render_notification_banner(frame: &mut Frame, queue: &NotificationQueue, area: Rect) {
    let notif = match queue.current() {
        Some(n) => n,
        None => return,
    };

    // Toast width: 48 cols max, right-aligned with a 2-col right margin.
    let toast_width = 52u16.min(area.width.saturating_sub(4));
    if toast_width < 20 {
        return;
    }
    // Adapt toast height based on available space, minimum 1 row for the message
    let toast_height = 3u16.min(area.height.saturating_sub(1).max(1));
    let toast_area = Rect {
        x: area.x + area.width.saturating_sub(toast_width + 2),
        // Position at bottom if not enough space at top
        y: if area.height >= 4 {
            area.y + 1
        } else {
            area.y + area.height.saturating_sub(toast_height)
        },
        width: toast_width,
        height: toast_height,
    };

    let color = notif.kind.color();
    let bg = Color::Rgb(18, 18, 22); // slightly elevated from terminal bg

    // Clear the area so the toast has a distinct background.
    frame.render_widget(Clear, toast_area);

    // ── Row 0: icon + message + optional "Esc" hint ──
    let inner_w = toast_width.saturating_sub(4) as usize; // 2 side bars + 1 pad each side
    let esc_hint = "  esc";
    let icon_with_spaces = format!(" {} ", notif.kind.icon());
    let icon_width = icon_with_spaces.width();
    let esc_width = if notif.dismissible {
        esc_hint.width()
    } else {
        0
    };

    // Available width for message: use inner_w as the base
    let msg_width_budget = inner_w.saturating_sub(icon_width + esc_width);

    // Truncate message based on display width, not character count
    let message = {
        let msg_width = notif.message.width();
        if msg_width > msg_width_budget {
            // Truncate character by character, checking width until we fit
            let mut truncated = String::new();
            for ch in notif.message.chars() {
                let test = format!("{}{}", truncated, ch);
                if test.width() + 1 > msg_width_budget {
                    // +1 for ellipsis
                    break;
                }
                truncated.push(ch);
            }
            format!("{}…", truncated)
        } else {
            notif.message.clone()
        }
    };

    let mut row0_spans = vec![
        Span::styled(
            icon_with_spaces.clone(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(message, Style::default().fg(CLAURST_TEXT)),
    ];
    if notif.dismissible {
        row0_spans.push(Span::styled(
            esc_hint.to_string(),
            Style::default().fg(CLAURST_MUTED),
        ));
    }

    // ── Row 1: thin progress bar for timed notifications ──
    let progress_line = if let Some(exp) = notif.expires_at {
        let now = Instant::now();
        let remaining = if exp > now {
            (exp - now).as_millis()
        } else {
            0
        };
        let total_ms = (exp - notif.pushed_at).as_millis().max(1);
        let frac = (remaining as f64 / total_ms as f64).min(1.0);
        let bar_w = (inner_w as f64 * frac) as usize;
        let bar_w = bar_w.min(inner_w);
        let filled: String = "─".repeat(bar_w);
        let empty: String = " ".repeat(inner_w.saturating_sub(bar_w));
        Line::from(vec![
            Span::styled(format!(" {}", filled), Style::default().fg(color)),
            Span::styled(empty, Style::default().fg(CLAURST_MUTED)),
            Span::raw(" "),
        ])
    } else {
        Line::from(Span::styled(
            format!(" {}", "─".repeat(inner_w)),
            Style::default().fg(CLAURST_PANEL_BORDER),
        ))
    };

    // Render background and borders to the buffer
    {
        let buf = frame.buffer_mut();

        // Helper: paint a full row with bg color, with bounds checking
        let paint_row = |buf: &mut ratatui::buffer::Buffer, row: u16| {
            if toast_area.y + row >= buf.area().bottom() {
                return;
            }
            for col in 0..toast_width {
                let x = toast_area.x + col;
                if x >= buf.area().right() {
                    break;
                }
                if let Some(cell) = buf.cell_mut((x, toast_area.y + row)) {
                    cell.set_bg(bg);
                }
            }
        };
        for row in 0..toast_height {
            paint_row(buf, row);
        }

        // Left accent bar (all rows)
        if toast_area.x < buf.area().right() {
            for row in 0..toast_height {
                if toast_area.y + row < buf.area().bottom() {
                    if let Some(cell) = buf.cell_mut((toast_area.x, toast_area.y + row)) {
                        cell.set_bg(bg);
                        cell.set_fg(color);
                        cell.set_char('▌');
                    }
                }
            }
        }
        // Right border bar (all rows)
        let right_x = toast_area.x + toast_width.saturating_sub(1);
        if right_x < buf.area().right() && toast_area.x < buf.area().right() {
            for row in 0..toast_height {
                if toast_area.y + row < buf.area().bottom() {
                    if let Some(cell) = buf.cell_mut((right_x, toast_area.y + row)) {
                        cell.set_bg(bg);
                        cell.set_fg(CLAURST_PANEL_BORDER);
                        cell.set_char('▐');
                    }
                }
            }
        }
    }

    // Render widgets (message, progress, padding)
    // Row 0: message (always show if space allows)
    if toast_area.y < frame.area().height {
        let msg_rect = Rect {
            x: toast_area.x + 1,
            y: toast_area.y,
            width: toast_width.saturating_sub(2),
            height: 1,
        };
        let para0 = Paragraph::new(Line::from(row0_spans)).style(Style::default().bg(bg));
        frame.render_widget(para0, msg_rect);
    }

    // Row 1: progress / divider (if space allows)
    if toast_height > 1 && toast_area.y + 1 < frame.area().height {
        let prog_rect = Rect {
            x: toast_area.x + 1,
            y: toast_area.y + 1,
            width: toast_width.saturating_sub(2),
            height: 1,
        };
        let para1 = Paragraph::new(progress_line).style(Style::default().bg(bg));
        frame.render_widget(para1, prog_rect);
    }

    // Row 2: blank bottom padding (if space allows)
    if toast_height > 2 && toast_area.y + 2 < frame.area().height {
        let pad_rect = Rect {
            x: toast_area.x + 1,
            y: toast_area.y + 2,
            width: toast_width.saturating_sub(2),
            height: 1,
        };
        frame.render_widget(Paragraph::new("").style(Style::default().bg(bg)), pad_rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_current() {
        let mut q = NotificationQueue::new();
        assert!(q.current().is_none());
        q.push(NotificationKind::Info, "hello".to_string(), None);
        assert_eq!(q.current().unwrap().message, "hello");
    }

    #[test]
    fn dismiss_by_id() {
        let mut q = NotificationQueue::new();
        q.push(NotificationKind::Warning, "warn".to_string(), None);
        let id = q.current().unwrap().id.clone();
        q.dismiss(&id);
        assert!(q.is_empty());
    }

    #[test]
    fn current_prefers_latest_notification() {
        let mut q = NotificationQueue::new();
        q.push(NotificationKind::Warning, "older".to_string(), None);
        q.push(NotificationKind::Info, "newer".to_string(), Some(3));
        assert_eq!(q.current().unwrap().message, "newer");
        q.dismiss_current();
        assert_eq!(q.current().unwrap().message, "older");
    }

    #[test]
    fn duplicate_notification_is_refreshed_not_duplicated() {
        let mut q = NotificationQueue::new();
        q.push(NotificationKind::Info, "same".to_string(), Some(3));
        q.push(NotificationKind::Info, "same".to_string(), Some(5));
        assert_eq!(q.notifications.len(), 1);
    }

    #[test]
    fn tick_removes_expired() {
        let mut q = NotificationQueue::new();
        // Push a notification that expired in the past
        q.notifications.push_back(super::Notification {
            id: "x".to_string(),
            kind: NotificationKind::Info,
            message: "gone".to_string(),
            pushed_at: Instant::now(),
            expires_at: Some(Instant::now() - std::time::Duration::from_secs(1)),
            dismissible: true,
        });
        assert!(!q.is_empty());
        q.tick();
        assert!(q.is_empty());
    }

    #[test]
    fn persistent_notification_survives_tick() {
        let mut q = NotificationQueue::new();
        q.push(NotificationKind::Success, "persistent".to_string(), None);
        q.tick();
        assert!(!q.is_empty());
    }
}
