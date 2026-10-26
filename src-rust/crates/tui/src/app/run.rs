//! Main event loop, query events, jump-to-error, file open.

use claurst_core::types::Message;
use claurst_core::{sample_completion_verb, sample_spinner_verb};
use claurst_query::QueryEvent;
use crate::notifications::NotificationKind;
use tracing::debug;
use super::App;
use super::turns::format_elapsed_ms;
use super::types::{RecentSession, SystemMessageStyle, ToolStatus, ToolUseBlock, recent_session_label};
use crate::session_browser::SessionEntry;

/// Load the `/session` / `/resume` browser entries from the store that
/// `history::save_session` writes (`<config_dir>/sessions/`), newest first.
pub(super) async fn load_session_entries() -> Vec<SessionEntry> {
    let now = chrono::Utc::now();
    claurst_core::history::list_sessions()
        .await
        .into_iter()
        .map(|s| {
            let age = now.signed_duration_since(s.updated_at);
            let last_updated = if age.num_minutes() < 1 {
                "just now".to_string()
            } else if age.num_hours() < 1 {
                format!("{}m ago", age.num_minutes())
            } else if age.num_hours() < 24 {
                format!("{}h ago", age.num_hours())
            } else {
                format!("{}d ago", age.num_days())
            };
            SessionEntry {
                message_count: s.messages.len(),
                id: s.id,
                title: s.title.unwrap_or_else(|| "(untitled)".to_string()),
                last_updated,
                cost_usd: s.total_cost,
                working_dir: s.working_dir,
            }
        })
        .collect()
}

/// Load the welcome screen's "Recent activity" for `project_root` from the
/// same store as [`load_session_entries`]: sessions whose working directory is
/// the project root or below it, newest first.
pub(super) async fn load_recent_sessions(project_root: &std::path::Path) -> Vec<RecentSession> {
    // Show at most a handful; list_sessions is already newest-first.
    const MAX_RECENT: usize = 5;
    claurst_core::history::list_sessions()
        .await
        .into_iter()
        .filter(|s| {
            s.working_dir
                .as_deref()
                .is_some_and(|wd| std::path::Path::new(wd).starts_with(project_root))
        })
        .take(MAX_RECENT)
        .map(|s| {
            let last_prompt = s
                .messages
                .iter()
                .rev()
                .filter(|m| m.role == claurst_core::types::Role::User)
                .map(Message::get_all_text)
                .find(|t| !t.trim().is_empty());
            RecentSession {
                label: recent_session_label(s.title, last_prompt),
                mtime: s.updated_at.into(),
            }
        })
        .collect()
}

impl App {
    /// Detect the current PR from environment variables or git.
    pub fn detect_pr(&mut self) {
        // Check CLAUDE_PR_NUMBER and CLAUDE_PR_URL env vars
        if let Ok(num) = std::env::var("CLAUDE_PR_NUMBER") {
            if let Ok(n) = num.parse::<u32>() {
                self.pr_number = Some(n);
            }
        }
        if let Ok(url) = std::env::var("CLAUDE_PR_URL") {
            self.pr_url = Some(url);
        }
        if let Ok(state) = std::env::var("CLAUDE_PR_STATE") {
            if !state.trim().is_empty() {
                self.pr_state = Some(state.trim().to_string());
            }
        }
        // Fall back to gh CLI if no env vars
        if self.pr_number.is_none() {
            if let Ok(output) = std::process::Command::new("gh")
                .args(["pr", "view", "--json", "number,url", "--jq", ".number,.url"])
                .output()
            {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    let parts: Vec<&str> = text.trim().split('\n').collect();
                    if parts.len() >= 2 {
                        if let Ok(n) = parts[0].trim().parse::<u32>() {
                            self.pr_number = Some(n);
                            self.pr_url = Some(parts[1].trim().to_string());
                        }
                    }
                }
            }
        }
    }

    /// Push a completed assistant message and trigger auto-scroll bookkeeping.
    pub(super) fn push_assistant_message(&mut self, text: String) {
        let msg = Message::assistant(text);
        self.messages.push(msg);
        self.invalidate_transcript();
        self.on_new_message();
    }

    /// Process a query event from the agentic loop.
    pub fn handle_query_event(&mut self, event: QueryEvent) {
        // Auto-dismiss error modal when assistant responds
        match &event {
            QueryEvent::Stream(_) | QueryEvent::TurnComplete { .. } => {
                self.dismiss_error_notifications();
            }
            _ => {}
        }
        // Any progress or terminal event ends the "Reconnecting" state.
        if matches!(
            event,
            QueryEvent::Stream(_) | QueryEvent::TurnComplete { .. } | QueryEvent::Error(_)
        ) {
            self.retry_status = None;
        }
        // A finished, failed or retried request closes its tok/s window.
        if matches!(
            event,
            QueryEvent::TurnComplete { .. } | QueryEvent::Error(_) | QueryEvent::Retry { .. }
        ) {
            self.request_output_started = None;
            self.request_output_bytes = 0;
        }

        match event {
            QueryEvent::Stream(stream_evt) => {
                if !self.is_streaming {
                    let seed = self.frame_count as usize ^ (self.messages.len() * 17);
                    self.spinner_verb = Some(sample_spinner_verb(seed).to_string());
                    // turn_start is set in begin_user_turn_snapshot (prompt
                    // submission time).  Only fall back here if somehow no
                    // user message was pushed before streaming began (e.g.
                    // headless / programmatic callers).
                    if self.turn_start.is_none() {
                        self.turn_start = Some(std::time::Instant::now());
                    }
                    self.streaming_thinking.clear();
                }
                self.is_streaming = true;
                match stream_evt {
                    claurst_api::AnthropicStreamEvent::ContentBlockDelta { delta, .. } => {
                        // Reset stall timer on any incoming delta — we're making progress.
                        self.stall_start = None;
                        match delta {
                            claurst_api::streaming::ContentDelta::TextDelta { text } => {
                                self.note_request_output(text.len());
                                self.streaming_text.push_str(&text);
                                self.invalidate_transcript();
                            }
                            claurst_api::streaming::ContentDelta::ThinkingDelta { thinking } => {
                                debug!(len = thinking.len(), "Thinking delta received");
                                self.note_request_output(thinking.len());
                                self.streaming_thinking.push_str(&thinking);
                                self.invalidate_transcript();
                            }
                            _ => {}
                        }
                    }
                    claurst_api::AnthropicStreamEvent::MessageStop => {
                        self.is_streaming = false;
                        self.spinner_verb = None;
                        self.stall_start = None;
                        self.flush_streamed_assistant_message();
                    }
                    _ => {
                        // Any other stream event: if we have no stall_start yet,
                        // record now so the red-spinner timer can begin.
                        if self.stall_start.is_none() {
                            self.stall_start = Some(std::time::Instant::now());
                        }
                    }
                }
            }

            QueryEvent::ToolStart { tool_name, tool_id, input_json } => {
                if !self.is_streaming && self.spinner_verb.is_none() {
                    let seed = self.frame_count as usize ^ (self.messages.len() * 17);
                    self.spinner_verb = Some(sample_spinner_verb(seed).to_string());
                }
                self.is_streaming = true;
                self.status_message = Some(format!("Running {}…", tool_name));
                let turn_index = self.current_user_turn_index();
                if let Some(existing) =
                    self.tool_use_blocks.iter_mut().find(|b| b.id == tool_id)
                {
                    existing.turn_index = turn_index;
                    existing.status = ToolStatus::Running;
                    existing.output_preview = None;
                    existing.input_json = input_json;
                } else {
                    self.tool_use_blocks.push(ToolUseBlock {
                        id: tool_id,
                        name: tool_name,
                        turn_index,
                        status: ToolStatus::Running,
                        output_preview: None,
                        input_json,
                    });
                }
                self.invalidate_transcript();
            }

            QueryEvent::ToolEnd {
                tool_name: _,
                tool_id,
                result,
                is_error,
            } => {
                // Build a multi-line preview: show up to 3 lines, truncate if more.
                let all_lines: Vec<&str> = result.lines().collect();
                let preview_lines = all_lines.len().min(3);
                let mut preview = all_lines[..preview_lines].join("\n");
                let remaining = all_lines.len().saturating_sub(preview_lines);
                if remaining > 0 {
                    preview.push_str(&format!("\n\u{2026} {} more lines", remaining));
                }
                if let Some(block) =
                    self.tool_use_blocks.iter_mut().find(|b| b.id == tool_id)
                {
                    block.status = if is_error {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Done
                    };
                    block.output_preview = Some(preview);
                }
                self.invalidate_transcript();
                if is_error {
                    self.status_message = Some(format!("Tool error: {}", result));
                } else {
                    self.status_message = None;
                }
                self.refresh_turn_diff_from_history();
            }

            QueryEvent::TurnComplete { turn, stop_reason, usage, .. } => {
                debug!(turn, stop_reason, "Turn complete");
                self.is_streaming = false;
                self.spinner_verb = None;

                // Context meter (#444): the prompt size of the request that
                // just finished (uncached + cache-write + cache-read input),
                // the figure the loop's own compaction check anchors on. Each
                // request resends the whole history, so summing them
                // over-counts. A provider that reports no input keeps the
                // previous reading.
                if let Some(prompt_tokens) = usage
                    .as_ref()
                    .map(claurst_core::types::UsageInfo::total_input)
                    .filter(|tokens| *tokens > 0)
                {
                    self.context_used_tokens = prompt_tokens;
                }
                // Record elapsed time and pick a completion verb
                let seed = self.frame_count as usize ^ (self.messages.len() * 7);
                let elapsed = self.turn_start.take()
                    .map(|start| format_elapsed_ms(start.elapsed().as_millis()));
                self.last_turn_elapsed = Some(
                    elapsed.unwrap_or_else(|| "0s".to_string())
                );
                self.last_turn_verb = Some(sample_completion_verb(seed));
                self.flush_streamed_assistant_message();
                self.tool_use_blocks.retain(|b| b.status != ToolStatus::Running);
                self.complete_current_turn_snapshot(stop_reason.contains("abort") || stop_reason.contains("cancel"));
                self.invalidate_transcript();
                self.refresh_turn_diff_from_history();
            }

            QueryEvent::Status(msg) => {
                self.status_message = Some(msg);
            }

            QueryEvent::Retry { attempt, max_attempts, delay_ms, reason } => {
                // The failed attempt's partial output is discarded by the
                // loop; drop it from the live view too.
                self.streaming_text.clear();
                self.streaming_thinking.clear();
                self.stall_start = None;
                self.retry_status = Some((attempt, max_attempts));
                self.status_message = Some(format!(
                    "Reconnecting {}/{} in {:.1}s: {}",
                    attempt,
                    max_attempts,
                    delay_ms as f64 / 1000.0,
                    reason
                ));
                self.invalidate_transcript();
            }

            QueryEvent::Error(msg) => {
                self.is_streaming = false;
                self.spinner_verb = None;
                self.streaming_text.clear();
                self.streaming_thinking.clear();
                self.invalidate_transcript();
                let err_msg = format!("Error: {}", msg);
                self.push_assistant_message(err_msg.clone());
                self.push_notification(NotificationKind::Error, err_msg, None);
            }
            QueryEvent::TokenWarning { state, pct_used } => {
                // Push a notification for context window warnings (notification + threshold tracking).
                use claurst_query::compact::TokenWarningState;

                // Only escalate — never repeat a threshold already shown.
                match state {
                    TokenWarningState::Ok => {
                        // Reset threshold tracking when back to normal
                        self.token_warning_threshold_shown = 0;
                    }
                    TokenWarningState::Warning if self.token_warning_threshold_shown < 80 => {
                        self.token_warning_threshold_shown = 80;
                        self.push_notification(
                            NotificationKind::Warning,
                            format!("Context window {:.0}% full. Consider /compact.", pct_used * 100.0),
                            Some(30),
                        );
                    }
                    TokenWarningState::Critical if self.token_warning_threshold_shown < 95 => {
                        self.token_warning_threshold_shown = 95;
                        self.push_notification(
                            NotificationKind::Error,
                            format!("Context window {:.0}% full! Run /compact now.", pct_used * 100.0),
                            None,
                        );
                    }
                    _ => {}
                }
            }
            QueryEvent::Compacted(record) => {
                // The model now sees the compacted history: its estimate is
                // the meter's new baseline until the next request reports
                // real prompt tokens, and warnings may fire again.
                self.context_used_tokens = record.tokens_after;
                self.token_warning_threshold_shown = 0;
            }
            QueryEvent::RateLimits { primary_used_percent, secondary_used_percent } => {
                if primary_used_percent.is_some() {
                    self.rate_limit_5h_pct = primary_used_percent;
                }
                if secondary_used_percent.is_some() {
                    self.rate_limit_7day_pct = secondary_used_percent;
                }
            }
            QueryEvent::InputInjected(texts) => self.show_injected_input(texts),
            // The TUI renders from the stream and syncs the history when the
            // query task finishes; committed messages are for headless output.
            // The loop also reports the cap through a Status message.
            QueryEvent::MessageCommitted(_) | QueryEvent::MaxTurnsReached { .. } => {}
        }

        // Update token count from tracker.
        self.token_count = self.cost_tracker.total_tokens() as u32;
    }

    /// Spawn and drain the background session-list loads requested by
    /// `session_list_pending` (the `/session` / `/resume` browser) and
    /// `recent_sessions_pending` (the welcome screen), plus the
    /// bang-command result and voice transcription events. Whichever loop
    /// drives the TUI must call this every frame; the CLI's interactive loop
    /// does (issue #382).
    pub fn poll_session_loads(&mut self) {
        self.poll_bang_command();
        self.poll_voice_events();

        if let Some(ref mut rx) = self.session_list_rx {
            match rx.try_recv() {
                Ok(entries) => {
                    self.session_browser.sessions = entries;
                    self.session_browser.selected_idx = 0;
                    self.session_list_rx = None;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.session_list_rx = None;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            }
        }

        if self.session_list_pending {
            self.session_list_pending = false;
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            self.session_list_rx = Some(rx);
            tokio::spawn(async move {
                let _ = tx.send(load_session_entries().await).await;
            });
        }

        if let Some(ref mut rx) = self.recent_sessions_rx {
            match rx.try_recv() {
                Ok(sessions) => {
                    self.recent_sessions = sessions;
                    self.recent_sessions_rx = None;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.recent_sessions_rx = None;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            }
        }

        if self.recent_sessions_pending {
            self.recent_sessions_pending = false;
            let root = self.project_root();
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            self.recent_sessions_rx = Some(rx);
            tokio::spawn(async move {
                let _ = tx.send(load_recent_sessions(&root).await).await;
            });
        }
    }

    /// Drain the pending bang-command result from its spawned task and
    /// show it in the transcript. Called every frame by the live loop.
    fn poll_bang_command(&mut self) {
        let Some(ref mut rx) = self.bang_command_pending else {
            return;
        };
        match rx.try_recv() {
            Ok((command, output)) => {
                self.bang_command_pending = None;
                if self.config.bang_commands.show_in_transcript {
                    let formatted = if output.is_empty() {
                        format!("$ ! {} (no output)", command)
                    } else {
                        format!("$ ! {}\n{}", command, output)
                    };
                    self.push_system_message(formatted, SystemMessageStyle::Info);
                } else {
                    self.status_message = Some(format!("! {} finished.", command));
                }
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                self.bang_command_pending = None;
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
        }
    }

    /// Drain voice transcription events (non-blocking). When the background
    /// recording/transcription task emits a TranscriptReady event we insert
    /// the text directly into the prompt so the user can review and submit it.
    fn poll_voice_events(&mut self) {
        use claurst_core::voice::VoiceEvent;
        let mut events = Vec::new();
        if let Some(ref mut rx) = self.voice_event_rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            match ev {
                VoiceEvent::RecordingStarted => {
                    self.voice_recording = true;
                    self.status_message =
                        Some("Recording\u{2026} (Alt+V or Esc to stop)".to_string());
                }
                VoiceEvent::RecordingStopped => {
                    self.voice_recording = false;
                    self.status_message = Some("Transcribing\u{2026}".to_string());
                }
                VoiceEvent::TranscriptReady(text) => {
                    if !text.is_empty() {
                        // Append to existing prompt text with a space separator
                        // so the user can combine voice + typed input.
                        if !self.prompt_input.text.is_empty()
                            && !self.prompt_input.text.ends_with(' ')
                        {
                            self.prompt_input.paste(" ");
                        }
                        self.prompt_input.paste(&text);
                        self.refresh_prompt_input();
                        self.status_message =
                            Some(format!("Transcribed: {}", &text[..text.len().min(60)]));
                    }
                    // Clear the channel once we have the result.
                    self.voice_event_rx = None;
                }
                VoiceEvent::Error(msg) => {
                    self.voice_recording = false;
                    self.voice_event_rx = None;
                    self.push_notification(
                        NotificationKind::Warning,
                        format!("Voice: {}", msg),
                        Some(8),
                    );
                }
            }
        }
    }

    /// Jump to the next error/issue in messages.
    /// Searches for common error indicators: "Error:", "ERROR:", "error", "failed", "FAIL".
    pub(super) fn jump_to_next_error(&mut self) {
        const ERROR_KEYWORDS: &[&str] = &["error:", "failed:", "fail"];

        // Search forward from current position
        for i in 0..self.messages.len() {
            let msg = &self.messages[i];
            let content = msg.get_all_text().to_lowercase();

            // Check if message contains error keywords
            let has_error = ERROR_KEYWORDS.iter().any(|keyword| {
                content.contains(keyword)
            });

            if has_error && i > (self.messages.len().saturating_sub(self.scroll_offset / 2)) {
                // Found an error message, scroll to it
                let new_offset = self.messages.len().saturating_sub(i);
                self.scroll_offset = new_offset.saturating_mul(2);
                self.auto_scroll = false;
                self.status_message = Some(format!("Error found in message {}", i + 1));
                return;
            }
        }

        self.status_message = Some("No more errors found.".to_string());
    }

    /// Jump to the previous error/issue in messages.
    /// Searches backwards for common error indicators.
    pub(super) fn jump_to_previous_error(&mut self) {
        const ERROR_KEYWORDS: &[&str] = &["error:", "failed:", "fail"];

        // Search backward from current position
        for i in (0..self.messages.len()).rev() {
            let msg = &self.messages[i];
            let content = msg.get_all_text().to_lowercase();

            // Check if message contains error keywords
            let has_error = ERROR_KEYWORDS.iter().any(|keyword| {
                content.contains(keyword)
            });

            if has_error && i < (self.messages.len().saturating_sub(self.scroll_offset / 2)) {
                // Found an error message, scroll to it
                let new_offset = self.messages.len().saturating_sub(i);
                self.scroll_offset = new_offset.saturating_mul(2);
                self.auto_scroll = false;
                self.status_message = Some(format!("Error found in message {}", i + 1));
                return;
            }
        }

        self.status_message = Some("No previous errors found.".to_string());
    }

}
