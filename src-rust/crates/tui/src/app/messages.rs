//! Transcript operations: add/replace/push, notifications, scroll.

use claurst_core::types::{Message, Role};
use crate::notifications::NotificationKind;
use super::App;
use super::types::{SystemAnnotation, SystemMessageStyle};

impl App {
    /// Add a message directly (e.g. from a non-streaming source).
    pub fn add_message(&mut self, role: Role, text: String) {
        let msg = match role {
            Role::User => Message::user(text),
            Role::Assistant => Message::assistant(text),
        };
        if role == Role::User {
            self.begin_user_turn_snapshot();
        }
        self.messages.push(msg);
        self.invalidate_transcript();
        self.on_new_message();
    }

    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
        self.sync_turn_metadata_to_messages();
        self.invalidate_transcript();
    }

    pub fn push_message(&mut self, message: Message) {
        if message.role == Role::User {
            self.begin_user_turn_snapshot();
        }
        self.messages.push(message);
        self.sync_turn_metadata_to_messages();
        self.invalidate_transcript();
        self.on_new_message();
    }

    /// Push a synthetic system annotation into the conversation pane.
    /// It will appear after the current last message.
    /// Push a notification and, for Error-kind notifications, reset the error
    /// modal scroll offset so a newly arrived error is always shown from the top.
    pub fn push_notification(&mut self, kind: NotificationKind, msg: String, duration_secs: Option<u64>) {
        if kind == NotificationKind::Error {
            self.error_modal_scroll_offset = 0;
        }
        self.notifications.push(kind, msg, duration_secs);
    }

    pub fn push_system_message(&mut self, text: String, style: SystemMessageStyle) {
        self.system_annotations.push(SystemAnnotation {
            after_index: self.messages.len(),
            text,
            style,
        });
        self.invalidate_transcript();
    }

    /// Called whenever a new message is appended to `messages`.
    /// Manages the auto-scroll / new-message-counter state.
    pub(super) fn on_new_message(&mut self) {
        if self.auto_scroll {
            // Auto-scroll: keep offset at 0 so render shows the bottom.
            self.scroll_offset = 0;
        } else {
            self.new_messages_while_scrolled =
                self.new_messages_while_scrolled.saturating_add(1);
        }
    }

    pub fn invalidate_transcript(&self) {
        self.transcript_version
            .set(self.transcript_version.get().wrapping_add(1));
    }

    /// Check current token usage and push token warning notifications as
    /// appropriate.  Call this after updating `token_count`.
    pub fn check_token_warnings(&mut self) {
        let window =
            claurst_query::context_window_for_model(&self.model_name) as u32;
        if window == 0 {
            return;
        }
        let pct = (self.token_count as f64 / window as f64 * 100.0) as u8;

        // Only escalate — never repeat a threshold already shown.
        if pct >= 100 && self.token_warning_threshold_shown < 100 {
            self.token_warning_threshold_shown = 100;
            self.push_notification(
                NotificationKind::Error,
                "Context window full. Running auto-compact\u{2026}".to_string(),
                None,
            );
        } else if pct >= 95 && self.token_warning_threshold_shown < 95 {
            self.token_warning_threshold_shown = 95;
            self.push_notification(
                NotificationKind::Error,
                "Context window 95% full! Run /compact now.".to_string(),
                None, // persistent until dismissed
            );
        } else if pct >= 80 && self.token_warning_threshold_shown < 80 {
            self.token_warning_threshold_shown = 80;
            self.push_notification(
                NotificationKind::Warning,
                "Context window 80% full. Consider /compact.".to_string(),
                Some(30),
            );
        }
    }

    /// Take the current input buffer, push it to history, and return it.
    pub fn take_input(&mut self) -> String {
        let input = self.prompt_input.take();
        if !input.is_empty() {
            self.prompt_input.history.push(input.clone());
            self.prompt_input.history_pos = None;
            self.prompt_input.history_draft.clear();
            self.input_history = self.prompt_input.history.clone();
            self.history_index = self.prompt_input.history_pos;
        }
        self.refresh_prompt_input();
        input
    }

    /// Scroll the transcript up by `amount` lines and disable auto-follow.
    ///
    /// `scroll_offset` counts lines above the bottom (0 = pinned to the newest
    /// content). It is clamped to `last_max_scroll` — the maximum meaningful
    /// offset from the last render — so scrolling up past the top of the
    /// transcript can't inflate it unboundedly. Without the clamp, an over-scroll
    /// would leave `scroll_offset` far above `max_scroll`, and the user would
    /// have to press Down that many times before the view moved (#223).
    pub(super) fn scroll_up_by(&mut self, amount: usize) {
        self.scroll_offset = self
            .scroll_offset
            .saturating_add(amount)
            .min(self.last_max_scroll.get());
        self.auto_scroll = false;
    }

    /// Compute the number of lines to scroll per wheel/trackpad event.
    /// Implements a simple acceleration model: rapid events (< 40 ms apart) are
    /// treated as trackpad bursts and accelerate up to 2×; slower events (mouse
    /// wheel) stay at the base 3-line step.
    pub(super) fn scroll_step(&mut self) -> usize {
        let now = std::time::Instant::now();
        let elapsed_ms = self.scroll_last_time
            .map(|t| now.duration_since(t).as_millis())
            .unwrap_or(u128::MAX);
        self.scroll_last_time = Some(now);
        if elapsed_ms < 40 {
            // Trackpad burst — gradually accelerate
            self.scroll_accel = (self.scroll_accel + 0.4).min(6.0);
        } else {
            // Mouse click or first event — reset to base
            self.scroll_accel = 3.0;
        }
        self.scroll_accel.round() as usize
    }

}
