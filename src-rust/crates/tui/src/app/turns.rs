//! Turn lifecycle: snapshots, streaming flush, elapsed-time labels.

use std::sync::Arc;

use crate::diff_viewer::build_turn_diff;
use claurst_core::file_history::FileHistory;
use claurst_core::types::{ContentBlock, Message, Role};
use super::App;
use super::types::TurnMetadata;

pub(super) fn format_elapsed_ms(ms: u128) -> String {
    let total_secs = ((ms + 500) / 1000) as u64; // round to nearest second
    if total_secs < 60 {
        format!("{}s", total_secs)
    } else {
        format!("{}m {}s", total_secs / 60, total_secs % 60)
    }
}

pub(super) fn format_turn_time_label() -> String {
    chrono::Local::now()
        .format("%I:%M %p")
        .to_string()
        .trim_start_matches('0')
        .to_lowercase()
}

impl App {
    pub(super) fn current_user_turn_index(&self) -> Option<usize> {
        self.messages
            .iter()
            .filter(|msg| msg.role == Role::User)
            .count()
            .checked_sub(1)
    }

    pub(super) fn current_agent_mode_snapshot(&self) -> String {
        self.agent_mode
            .clone()
            .unwrap_or_else(|| if self.plan_mode { "plan" } else { "build" }.to_string())
    }

    pub(super) fn begin_user_turn_snapshot(&mut self) {
        self.turn_metadata.push(TurnMetadata {
            submitted_at: Some(format_turn_time_label()),
            model_name: Some(self.model_name.clone()),
            agent_mode: Some(self.current_agent_mode_snapshot()),
            duration: None,
            interrupted: false,
        });
        // Start the latency timer now — at prompt-submission time — so it
        // measures actual round-trip time even when the provider buffers its
        // full response before yielding any stream events (e.g. Gemini flash).
        self.turn_start = Some(std::time::Instant::now());
        self.last_turn_elapsed = None;
        self.last_turn_verb = None;
    }

    pub(super) fn sync_turn_metadata_to_messages(&mut self) {
        let user_count = self
            .messages
            .iter()
            .filter(|msg| msg.role == Role::User)
            .count();

        if self.turn_metadata.len() > user_count {
            self.turn_metadata.truncate(user_count);
            return;
        }

        while self.turn_metadata.len() < user_count {
            self.turn_metadata.push(TurnMetadata::default());
        }
    }

    pub(super) fn complete_current_turn_snapshot(&mut self, interrupted: bool) {
        if let Some(index) = self.current_user_turn_index() {
            if self.turn_metadata.len() <= index {
                self.sync_turn_metadata_to_messages();
            }

            let model_name = self.model_name.clone();
            let agent_mode = self.current_agent_mode_snapshot();
            if let Some(meta) = self.turn_metadata.get_mut(index) {
                meta.duration = self.last_turn_elapsed.clone();
                meta.interrupted = interrupted;
                if meta.model_name.is_none() {
                    meta.model_name = Some(model_name);
                }
                if meta.agent_mode.is_none() {
                    meta.agent_mode = Some(agent_mode);
                }
            }
        }
    }

    /// Count streamed output toward the current request's tok/s.
    pub(super) fn note_request_output(&mut self, bytes: usize) {
        self.request_output_started.get_or_insert_with(std::time::Instant::now);
        self.request_output_bytes += bytes;
    }

    /// Output rate of the request streaming now, in approximate tokens per
    /// second (about four bytes per token). `None` while idle or during the
    /// first half second, when the figure is mostly noise.
    pub fn stream_tokens_per_sec(&self) -> Option<f64> {
        if !self.is_streaming {
            return None;
        }
        let elapsed = self.request_output_started?.elapsed().as_secs_f64();
        (elapsed >= 0.5 && self.request_output_bytes > 0)
            .then(|| self.request_output_bytes as f64 / 4.0 / elapsed)
    }

    /// Whether a turn is running: the model is streaming, or the query task
    /// is between requests (tools running, retry back-off).
    pub(super) fn turn_running(&self) -> bool {
        self.is_streaming || self.query_in_flight
    }

    /// Esc / Ctrl+C during a turn (#444): stop it the same way from either
    /// key. What streamed so far stays in the transcript, the retry pill is
    /// cleared, undelivered steers go back to the prompt, and the interactive
    /// loop cancels the turn's token (stopping the model and running tools).
    pub(super) fn interrupt_turn(&mut self) {
        self.is_streaming = false;
        self.spinner_verb = None;
        self.stall_start = None;
        self.retry_status = None;
        self.request_output_started = None;
        self.request_output_bytes = 0;
        self.flush_streamed_assistant_message();
        self.tool_use_blocks.retain(|b| b.status != super::types::ToolStatus::Running);
        self.status_message = Some("Cancelled.".to_string());
        self.complete_current_turn_snapshot(true);
        self.restore_pending_steers();
        self.cancel_requested = true;
        self.invalidate_transcript();
    }

    pub(super) fn flush_streamed_assistant_message(&mut self) {
        if self.streaming_text.trim().is_empty() && self.streaming_thinking.trim().is_empty() {
            self.streaming_text.clear();
            self.streaming_thinking.clear();
            return;
        }

        let thinking = std::mem::take(&mut self.streaming_thinking);
        let text = std::mem::take(&mut self.streaming_text);

        let mut blocks = Vec::new();
        if !thinking.trim().is_empty() {
            blocks.push(ContentBlock::Thinking {
                thinking,
                signature: String::new(),
            });
        }
        if !text.is_empty() {
            blocks.push(ContentBlock::Text { text });
        }

        let msg = match blocks.len() {
            0 => return,
            1 => match blocks.pop().unwrap() {
                ContentBlock::Text { text } => Message::assistant(text),
                block => Message::assistant_blocks(vec![block]),
            },
            _ => Message::assistant_blocks(blocks),
        };

        self.messages.push(msg);
        self.invalidate_transcript();
        self.on_new_message();
    }

    /// Return the elapsed session time as a human-readable string, e.g. "2m 5s".
    pub fn elapsed_str(&self) -> String {
        let secs = self.session_start.elapsed().as_secs();
        if secs < 60 {
            format!("{}s", secs)
        } else {
            format!("{}m {}s", secs / 60, secs % 60)
        }
    }

    pub fn attach_turn_diff_state(
        &mut self,
        file_history: Arc<parking_lot::Mutex<FileHistory>>,
        current_turn: Arc<std::sync::atomic::AtomicUsize>,
    ) {
        self.file_history = Some(file_history);
        self.current_turn = Some(current_turn);
        self.refresh_turn_diff_from_history();
    }

    pub(super) fn refresh_turn_diff_from_history(&mut self) {
        let Some(file_history) = self.file_history.as_ref() else {
            self.diff_viewer.set_turn_diff(Vec::new());
            return;
        };
        let Some(current_turn) = self.current_turn.as_ref() else {
            self.diff_viewer.set_turn_diff(Vec::new());
            return;
        };

        let turn_index = current_turn.load(std::sync::atomic::Ordering::Relaxed);
        if turn_index == 0 {
            self.diff_viewer.set_turn_diff(Vec::new());
            return;
        }

        let root = self.project_root();
        let files = {
            let history = file_history.lock();
            build_turn_diff(&history, turn_index, &root)
        };
        self.diff_viewer.set_turn_diff(files);
    }

}
