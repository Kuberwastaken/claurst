//! Conservative TUI idle hibernation: retain source data, release derived data.
//!
//! Ported from jcode's `tui/app/idle_hibernation.rs`. Claurst has no remote
//! history-fetch path that could rebuild the screen from the server, so the
//! frozen copy of the last frame is what keeps the display intact while the
//! derived caches behind it are dropped. Authoritative state — transcript,
//! display list, input draft, attachments, scroll position, session identity
//! — is never touched.

use super::App;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::time::{Duration, Instant};

// Terminal cells can contain wide glyphs; bound both the cell count and the
// total symbol bytes before cloning a frame.
const MAX_FROZEN_CELLS: usize = 128 * 1024;
const MAX_FROZEN_SYMBOL_BYTES: usize = 8 * 1024 * 1024;

/// Hibernation state machine: awake -> pending -> hibernated -> awake.
pub struct IdleHibernation {
    last_busy: Instant,
    /// Timer expired and the next completed frame should be captured.
    pending: bool,
    /// Bounded copy of the last rendered frame, shown verbatim while
    /// hibernated so the terminal needs no rebuild.
    pub frame: Option<Buffer>,
    /// Status-row text explaining the state and the measured RSS change.
    pub notice: String,
    rss_before: Option<u64>,
    rss_after: Option<u64>,
    notice_dirty: bool,
    wake_count: u64,
    hibernation_count: u64,
    last_wake_reason: Option<String>,
}

impl Default for IdleHibernation {
    fn default() -> Self {
        Self {
            last_busy: Instant::now(),
            pending: false,
            frame: None,
            notice: String::new(),
            rss_before: None,
            rss_after: None,
            notice_dirty: false,
            wake_count: 0,
            hibernation_count: 0,
            last_wake_reason: None,
        }
    }
}

impl App {
    /// Rechecked at the point of eviction, not just when scheduling: work
    /// arriving between the timer firing and the next paint must be protected.
    fn hibernation_has_work(&self) -> bool {
        self.is_streaming
            || self.query_in_flight
            || !self.streaming_text.is_empty()
            || !self.streaming_thinking.is_empty()
            || !self.queued_messages.is_empty()
            || !self.pending_steers.is_empty()
            || self.pending_auto_submit
            || self.pending_rewind.is_some()
            || self.permission_request.is_some()
            || self.device_auth_pending.is_some()
            || self.pending_mcp_reconnect
            || self.pending_provider_reload
            || self.pending_mcp_panel_auth.is_some()
            || self.model_picker_fetch_pending
            || self.session_list_pending
            || self.recent_sessions_pending
            || self.cancel_requested
            || self.voice_recording
            || self.background_task_count > 0
            || self.retry_status.is_some()
            || self.any_modal_open()
            || self
                .command_queue
                .as_ref()
                .is_some_and(|queue| !queue.is_empty())
    }

    /// Advance the idle state machine once per loop iteration. Returns true
    /// when the just-completed frame should be captured (caller then calls
    /// [`App::enter_idle_hibernation`] with it), or when the hibernation
    /// notice changed and a repaint is wanted.
    pub fn poll_idle_hibernation(&mut self) -> bool {
        let idle_secs = self
            .config
            .hibernation_idle_secs
            .unwrap_or(claurst_core::config::DEFAULT_HIBERNATION_IDLE_SECS);
        self.poll_idle_hibernation_at(Instant::now(), idle_secs)
    }

    fn poll_idle_hibernation_at(&mut self, now: Instant, idle_secs: u64) -> bool {
        if idle_secs == 0 || self.hibernation_has_work() {
            let was_hibernated = self.idle_hibernation.frame.is_some();
            self.wake_idle_hibernation_for("disabled_or_work_pending");
            self.idle_hibernation.last_busy = now;
            return was_hibernated;
        }
        if self.idle_hibernation.frame.is_some() {
            return std::mem::take(&mut self.idle_hibernation.notice_dirty);
        }
        if self.idle_hibernation.pending {
            return true;
        }
        let last_activity = [
            Some(self.idle_hibernation.last_busy),
            Some(self.session_start),
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(now);
        self.idle_hibernation.pending =
            now.saturating_duration_since(last_activity) >= Duration::from_secs(idle_secs);
        self.idle_hibernation.pending
    }

    /// Capture `previous_frame` and release derived caches. Called by the
    /// main loop with its last *completed* terminal frame; nothing is evicted
    /// when there is no safe, bounded frame to keep on screen.
    pub fn enter_idle_hibernation(&mut self, previous_frame: Option<&Buffer>) {
        if !self.idle_hibernation.pending || self.hibernation_has_work() {
            return;
        }
        // Already hibernating: keep the first frozen frame, no double freeze.
        if self.idle_hibernation.frame.is_some() {
            self.idle_hibernation.pending = false;
            return;
        }
        self.idle_hibernation.pending = false;
        let Some(frame) = previous_frame.filter(|frame| frame_can_be_frozen(frame)) else {
            // Do not retry an oversized frame on every deep-idle tick, but
            // leave a trace: on a huge terminal the freeze guard silently
            // keeps the session awake, and a log line is the only signal.
            tracing::info!(
                target: "hibernation",
                "idle freeze skipped: terminal frame too large to bound"
            );
            self.idle_hibernation.last_busy = Instant::now();
            return;
        };
        let before = claurst_core::process_memory::rss_bytes();
        self.idle_hibernation.frame = Some(frame.clone());
        self.idle_hibernation.hibernation_count += 1;
        // Derived, rebuildable caches: rendered transcript lines (both the
        // full-build and committed-prefix entries), prompt typeahead, and
        // per-frame hit-test maps. `transcript_version` is NOT bumped: the
        // renderer short-circuits to the frozen frame while hibernated, and
        // on wake the version alone already guarantees a rebuild because the
        // caches are gone.
        crate::render::release_idle_render_caches();
        self.prompt_input.suggestions.clear();
        self.thinking_row_map.borrow_mut().clear();
        self.message_row_map.borrow_mut().clear();
        self.last_row_text.borrow_mut().clear();
        self.selection_text.borrow_mut().clear();
        claurst_core::process_memory::release_retained_heap("tui_hibernation");
        let after = claurst_core::process_memory::rss_bytes();
        self.idle_hibernation.rss_before = before;
        self.idle_hibernation.rss_after = after;
        self.idle_hibernation.notice = hibernation_notice(before, after);
        self.idle_hibernation.notice_dirty = true;
    }

    /// Wake on any user interaction. Runs before the event is handled, and
    /// never consumes or rewrites it, so the waking keystroke is processed
    /// normally including its first character.
    #[track_caller]
    pub fn wake_idle_hibernation(&mut self) {
        if self.idle_hibernation.pending || self.idle_hibernation.frame.is_some() {
            let caller = std::panic::Location::caller();
            self.wake_idle_hibernation_for(&format!("{}:{}", caller.file(), caller.line()));
        } else {
            self.idle_hibernation.last_busy = Instant::now();
        }
    }

    pub fn wake_idle_hibernation_for(&mut self, reason: &str) {
        self.idle_hibernation.last_busy = Instant::now();
        if self.idle_hibernation.pending || self.idle_hibernation.frame.is_some() {
            self.idle_hibernation.last_wake_reason = Some(reason.to_string());
        }
        if self.idle_hibernation.frame.take().is_some() {
            self.idle_hibernation.wake_count += 1;
            tracing::debug!(target: "hibernation", "TUI hibernation wake: {reason}");
            self.idle_hibernation.notice.clear();
            self.idle_hibernation.notice_dirty = true;
        }
        self.idle_hibernation.pending = false;
    }

    /// The frozen frame, when the session is hibernated.
    pub fn hibernation_frame(&self) -> Option<&Buffer> {
        self.idle_hibernation.frame.as_ref()
    }

    /// Copy the frozen frame into the live buffer, remapping rows when the
    /// terminal was resized while hibernated. No allocation on the steady-
    /// state path: when the areas match the cells are copied one-to-one, and
    /// on a size mismatch overlapping rows are copied column-wise (the rest
    /// of the live buffer stays as cleared by the caller).
    pub fn paint_hibernation_frame(&self, target: &mut Buffer, frozen: &Buffer, size: Rect) {
        if frozen.area == size {
            for (dst, src) in target.content.iter_mut().zip(frozen.content.iter()) {
                *dst = src.clone();
            }
            return;
        }
        let rows = frozen.area.height.min(size.height);
        let cols = frozen.area.width.min(size.width);
        if rows == 0 || cols == 0 {
            return;
        }
        let frozen_w = frozen.area.width as usize;
        let target_w = size.width as usize;
        for row in 0..rows {
            for col in 0..cols {
                let src = frozen.area.y as usize + row as usize;
                let dst = size.y as usize + row as usize;
                let s = src * frozen_w + frozen.area.x as usize + col as usize;
                let d = dst * target_w + size.x as usize + col as usize;
                target.content[d] = frozen.content[s].clone();
            }
        }
    }

    /// The hibernation status-row text, when the session is hibernated.
    pub fn hibernation_notice(&self) -> Option<&str> {
        (!self.idle_hibernation.notice.is_empty()).then_some(self.idle_hibernation.notice.as_str())
    }

    /// Debug/test view of the hibernation state.
    pub fn debug_hibernation_state(&self) -> String {
        serde_json::json!({
            "state": if self.idle_hibernation.frame.is_some() { "hibernated" }
                else if self.idle_hibernation.pending { "pending" } else { "awake" },
            "scope": "tui_derived_caches",
            "busy": self.hibernation_has_work(),
            "wake_count": self.idle_hibernation.wake_count,
            "hibernation_count": self.idle_hibernation.hibernation_count,
            "last_wake_reason": self.idle_hibernation.last_wake_reason,
            "rss_before_bytes": self.idle_hibernation.rss_before,
            "rss_after_bytes": self.idle_hibernation.rss_after,
            "rss_released_bytes": self.idle_hibernation.rss_before.zip(self.idle_hibernation.rss_after)
                .map(|(before, after)| before.saturating_sub(after)),
            "rss_change_bytes": self.idle_hibernation.rss_before.zip(self.idle_hibernation.rss_after)
                .map(|(before, after)| i128::from(after) - i128::from(before)),
            "frozen_cells": self.idle_hibernation.frame.as_ref().map(|frame| frame.content.len()),
            "notice": self.idle_hibernation.notice,
        })
        .to_string()
    }
}

fn frame_can_be_frozen(frame: &Buffer) -> bool {
    !frame.content.is_empty()
        && frame.content.len() <= MAX_FROZEN_CELLS
        && frame
            .content
            .iter()
            .try_fold(0usize, |bytes, cell| bytes.checked_add(cell.symbol().len()))
            .is_some_and(|bytes| bytes <= MAX_FROZEN_SYMBOL_BYTES)
}

fn hibernation_notice(before: Option<u64>, after: Option<u64>) -> String {
    match (before, after) {
        (Some(before), Some(after)) => {
            let change = if before > after {
                format!("-{:.1} MiB", (before - after) as f64 / 1_048_576.0)
            } else if after > before {
                format!("+{:.1} MiB", (after - before) as f64 / 1_048_576.0)
            } else {
                "unchanged".to_string()
            };
            format!(
                "Hibernated · TUI RSS {change} ({:.0} MiB) · type to resume",
                after as f64 / 1_048_576.0,
            )
        }
        _ => "Hibernated · type to resume".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::config::Config;
    use ratatui::{backend::TestBackend, layout::Rect, Terminal};

    fn app() -> App {
        let mut app = App::new(Config::default(), claurst_core::cost::CostTracker::new());
        // A freshly built App is waiting on its initial recent-sessions disk
        // load, which counts as pending work and correctly blocks
        // hibernation. Tests simulate a settled session.
        app.recent_sessions_pending = false;
        app
    }

    fn expire_idle_timer(app: &mut App, idle_secs: u64) -> Instant {
        let now = Instant::now();
        let old = now - Duration::from_secs(idle_secs + 60);
        app.session_start = old;
        app.idle_hibernation.last_busy = old;
        now
    }

    fn freeze(app: &mut App) {
        let now = expire_idle_timer(app, 1800);
        assert!(app.poll_idle_hibernation_at(now, 1800));
        app.enter_idle_hibernation(Some(&Buffer::empty(Rect::new(0, 0, 100, 30))));
        assert!(app.hibernation_frame().is_some());
    }

    #[test]
    fn idle_hibernation_respects_timeout_disabled_config_and_recent_activity() {
        let mut app = app();
        let now = expire_idle_timer(&mut app, 1800);
        // Disabled.
        assert!(!app.poll_idle_hibernation_at(now, 0));
        // Not yet idle for the configured window.
        assert!(!app.poll_idle_hibernation_at(now, 1802));
        // Recent activity resets the idle window.
        app.idle_hibernation.last_busy = now;
        assert!(!app.poll_idle_hibernation_at(now, 1800));
        app.idle_hibernation.last_busy = now - Duration::from_secs(1801);
        assert!(app.poll_idle_hibernation_at(now, 1800));
    }

    #[test]
    fn idle_hibernation_rechecks_every_active_work_guard() {
        let cases: &[fn(&mut App)] = &[
            |app| app.is_streaming = true,
            |app| app.query_in_flight = true,
            |app| app.streaming_text = "stream".into(),
            |app| app.streaming_thinking = "think".into(),
            |app| app.queued_messages.push_back("pending".into()),
            |app| app.pending_steers.push((1, "pending".into())),
            |app| app.pending_auto_submit = true,
            |app| app.background_task_count = 1,
            |app| app.retry_status = Some((1, 3)),
            |app| app.voice_recording = true,
            |app| app.cancel_requested = true,
            |app| app.show_help = true,
            |app| app.permission_request = Some(test_permission_request()),
        ];
        for (index, set_busy) in cases.iter().enumerate() {
            let mut app = app();
            let now = expire_idle_timer(&mut app, 1800);
            assert!(app.poll_idle_hibernation_at(now, 1800));
            set_busy(&mut app);
            // Work arriving after the timer but before the paint is protected.
            app.enter_idle_hibernation(Some(&Buffer::empty(Rect::new(0, 0, 80, 24))));
            assert!(app.hibernation_frame().is_none(), "guard {index}");
            assert!(!app.poll_idle_hibernation_at(now, 1800), "guard {index}");
        }
    }

    fn test_permission_request() -> crate::dialogs::PermissionRequest {
        crate::dialogs::PermissionRequest::standard("test".into(), "Read".into(), "read x".into())
    }

    #[test]
    fn idle_hibernation_keeps_sources_draft_and_scroll() {
        let mut app = app();
        app.prompt_input.text = "draft".into();
        app.prompt_input.cursor = 2;
        app.scroll_offset = 17;
        app.auto_scroll = false;
        app.messages
            .push(claurst_core::types::Message::user("source"));
        let message_count = app.messages.len();
        freeze(&mut app);
        assert_eq!(app.prompt_input.text, "draft");
        assert_eq!(app.prompt_input.cursor, 2);
        assert_eq!(app.scroll_offset, 17);
        assert!(!app.auto_scroll);
        assert_eq!(app.messages.len(), message_count);
    }

    #[test]
    fn idle_hibernation_wakes_on_input_without_losing_waking_event() {
        let mut app = app();
        freeze(&mut app);
        app.wake_idle_hibernation_for("input");
        assert!(app.hibernation_frame().is_none());
        assert!(app.hibernation_notice().is_none());
        let state: serde_json::Value =
            serde_json::from_str(&app.debug_hibernation_state()).unwrap();
        assert_eq!(state["state"], "awake");
        assert_eq!(state["wake_count"], 1);
        assert_eq!(state["hibernation_count"], 1);
    }

    #[test]
    fn idle_hibernation_rearms_after_wake() {
        let mut app = app();
        freeze(&mut app);
        app.wake_idle_hibernation_for("input");
        // Immediately idle again: must not re-fire until the window elapses.
        let now = Instant::now();
        assert!(!app.poll_idle_hibernation_at(now, 1800));
        let later = now + Duration::from_secs(1801);
        assert!(app.poll_idle_hibernation_at(later, 1800));
        app.enter_idle_hibernation(Some(&Buffer::empty(Rect::new(0, 0, 80, 24))));
        assert!(app.hibernation_frame().is_some());
        let state: serde_json::Value =
            serde_json::from_str(&app.debug_hibernation_state()).unwrap();
        assert_eq!(state["hibernation_count"], 2);
    }

    #[test]
    fn idle_hibernation_paint_copies_cells_and_remaps_after_resize() {
        let mut app = app();
        freeze(&mut app);
        let mut frozen = Buffer::empty(Rect::new(0, 0, 4, 2));
        for (i, cell) in frozen.content.iter_mut().enumerate() {
            cell.set_symbol(if i % 2 == 0 { "a" } else { "b" });
        }

        // Same-size target: verbatim cell copy.
        let mut target = Buffer::empty(Rect::new(0, 0, 4, 2));
        app.paint_hibernation_frame(&mut target, &frozen, Rect::new(0, 0, 4, 2));
        assert_eq!(target.content[0].symbol(), "a");
        assert_eq!(target.content[1].symbol(), "b");
        assert_eq!(target.content[7].symbol(), "b");

        // Wider target: the frozen rows land in the top-left overlap; new
        // columns keep their cleared symbol, not a shifted row.
        let mut wide = Buffer::empty(Rect::new(0, 0, 6, 2));
        for cell in wide.content.iter_mut() {
            cell.set_symbol("z");
        }
        app.paint_hibernation_frame(&mut wide, &frozen, Rect::new(0, 0, 6, 2));
        assert_eq!(wide.content[0].symbol(), "a");
        assert_eq!(wide.content[3].symbol(), "b");
        assert_eq!(
            wide.content[4].symbol(),
            "z",
            "column past frozen width stays cleared"
        );
        assert_eq!(wide.content[5].symbol(), "z");

        // Non-zero-origin target: cells map by absolute (x, y), not by a
        // naive row-interleave that would drift on offset areas.
        let mut offset = Buffer::empty(Rect::new(0, 0, 6, 3));
        let size = Rect::new(1, 1, 4, 2);
        app.paint_hibernation_frame(&mut offset, &frozen, size);
        let row = 1; // size.y
        let col = 1; // size.x
        let first = offset.content[row * 6 + col].symbol();
        assert_eq!(first, "a", "top-left of the offset overlap");

        // Degenerate sizes paint nothing and panic no index.
        let mut zero = Buffer::empty(Rect::new(0, 0, 0, 0));
        app.paint_hibernation_frame(&mut zero, &frozen, Rect::new(0, 0, 0, 0));
    }

    #[test]
    fn idle_hibernation_rejects_unbounded_and_missing_screen_snapshots() {
        let mut app = app();
        let now = expire_idle_timer(&mut app, 1800);
        assert!(app.poll_idle_hibernation_at(now, 1800));
        app.enter_idle_hibernation(None);
        assert!(app.hibernation_frame().is_none());
        let mut frame = Buffer::empty(Rect::new(0, 0, 1, 1));
        frame.content[0].set_symbol(&"x".repeat(MAX_FROZEN_SYMBOL_BYTES + 1));
        assert!(!frame_can_be_frozen(&frame));
        assert!(!frame_can_be_frozen(&Buffer::empty(Rect::new(0, 0, 0, 0))));
    }

    #[test]
    fn idle_hibernation_notice_reports_measurement_without_inventing_savings() {
        let mib = 1_048_576;
        assert!(hibernation_notice(Some(100 * mib), Some(70 * mib)).contains("-30.0 MiB"));
        assert!(hibernation_notice(Some(70 * mib), Some(100 * mib)).contains("+30.0 MiB"));
        assert!(hibernation_notice(Some(70 * mib), Some(70 * mib)).contains("unchanged"));
        assert!(!hibernation_notice(None, Some(100)).contains('\n'));
    }

    #[test]
    fn idle_hibernation_frozen_frame_is_served_while_caches_stay_empty() {
        let mut app = app();
        freeze(&mut app);
        let snapshot = app.hibernation_frame().unwrap().clone();
        // A second entry attempt keeps the existing frame (no double freeze).
        app.idle_hibernation.pending = true;
        app.enter_idle_hibernation(Some(&Buffer::empty(Rect::new(0, 0, 5, 5))));
        assert_eq!(app.hibernation_frame().unwrap(), &snapshot);
    }

    #[test]
    fn idle_hibernation_notice_renders_on_status_row() {
        let mut app = app();
        freeze(&mut app);
        app.idle_hibernation.notice = hibernation_notice(Some(100 << 20), Some(70 << 20));
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| crate::render::render_app(frame, &app))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Hibernated"));
        assert!(text.contains("type to resume"));
    }
}
