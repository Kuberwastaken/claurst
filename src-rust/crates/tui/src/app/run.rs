//! Main event loop, query events, jump-to-error, file open.

use std::io::Stdout;

use claurst_core::types::Message;
use claurst_core::{sample_completion_verb, sample_spinner_verb};
use claurst_query::QueryEvent;
use crate::notifications::NotificationKind;
use crate::render;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tracing::debug;
use super::App;
use super::turns::format_elapsed_ms;
use super::types::{RecentSession, ToolStatus, ToolUseBlock, recent_session_label};

pub(super) fn open_file_externally(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    // Try to open with the system's default application
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(path)
            .spawn()?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()?;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(&["/C", "start", ""])
            .arg(path)
            .spawn()?;
        Ok(())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        // Fallback for other systems: try common editors in order
        for editor in &["nano", "vi", "vim", "emacs"] {
            match std::process::Command::new(editor)
                .arg(path)
                .spawn()
            {
                Ok(_) => return Ok(()),
                Err(_) => continue,
            }
        }
        Err("No suitable editor found".into())
    }
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
                                self.streaming_text.push_str(&text);
                                self.invalidate_transcript();
                            }
                            claurst_api::streaming::ContentDelta::ThinkingDelta { thinking } => {
                                debug!(len = thinking.len(), "Thinking delta received");
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

                // Update context window usage from the usage info.
                if let Some(ref u) = usage {
                    let turn_tokens = u.input_tokens + u.output_tokens
                        + u.cache_creation_input_tokens + u.cache_read_input_tokens;
                    self.context_used_tokens = self.context_used_tokens.saturating_add(turn_tokens);
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
        }

        // Update token count from tracker.
        self.token_count = self.cost_tracker.total_tokens() as u32;
    }

    /// Run the TUI event loop. Returns `Some(input)` when the user submits
    /// a message, or `None` when the user quits.
    pub fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> anyhow::Result<Option<String>> {
        loop {
            self.frame_count = self.frame_count.wrapping_add(1);

            // Drain background session-list results.
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

            // Spawn async session-list load when requested.
            if self.session_list_pending {
                self.session_list_pending = false;
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                self.session_list_rx = Some(rx);
                tokio::spawn(async move {
                    let sessions = claurst_core::history::list_sessions().await;
                    let entries: Vec<crate::session_browser::SessionEntry> = sessions
                        .into_iter()
                        .map(|s| {
                            let age = chrono::Utc::now()
                                .signed_duration_since(s.updated_at);
                            let last_updated = if age.num_minutes() < 1 {
                                "just now".to_string()
                            } else if age.num_hours() < 1 {
                                format!("{}m ago", age.num_minutes())
                            } else if age.num_hours() < 24 {
                                format!("{}h ago", age.num_hours())
                            } else {
                                format!("{}d ago", age.num_days())
                            };
                            crate::session_browser::SessionEntry {
                                id: s.id,
                                title: s.title.unwrap_or_else(|| "(untitled)".to_string()),
                                last_updated,
                                message_count: s.messages.len(),
                                cost_usd: s.total_cost,
                            }
                        })
                        .collect();
                    let _ = tx.send(entries).await;
                });
            }

            // Drain background recent-sessions results into the welcome screen.
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

            // Spawn the one-shot recent-sessions load when requested (startup).
            if self.recent_sessions_pending {
                self.recent_sessions_pending = false;
                let root = self.project_root();
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                self.recent_sessions_rx = Some(rx);
                tokio::spawn(async move {
                    // Show at most a handful; list_sessions is already newest-first.
                    const MAX_RECENT: usize = 5;
                    let summaries = claurst_core::session_storage::list_sessions(&root)
                        .await
                        .unwrap_or_default();
                    let recent: Vec<RecentSession> = summaries
                        .into_iter()
                        .take(MAX_RECENT)
                        .map(|s| RecentSession {
                            label: recent_session_label(s.title, s.last_prompt),
                            mtime: s.mtime,
                        })
                        .collect();
                    let _ = tx.send(recent).await;
                });
            }

            // Drain the one-shot skill-count discovery into the status-bar
            // indicator. Discovery runs on a background task because it can
            // shell out to `git clone` for `skills.urls` entries.
            if let Some(ref mut rx) = self.skill_count_rx {
                match rx.try_recv() {
                    Ok(count) => {
                        self.skill_count = count;
                        self.skill_count_rx = None;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        self.skill_count_rx = None;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                }
            }

            // Spawn the one-shot skill discovery when requested (startup).
            if self.skill_count_pending {
                self.skill_count_pending = false;
                let root = self.project_root();
                let skills_config = self.config.skills.clone();
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                self.skill_count_rx = Some(rx);
                tokio::spawn(async move {
                    let count = tokio::task::spawn_blocking(move || {
                        claurst_core::discover_skills(&root, &skills_config).len()
                    })
                    .await
                    .unwrap_or(0);
                    let _ = tx.send(count).await;
                });
            }

            // Drain voice transcription events (non-blocking).
            // When the background recording/transcription task emits a
            // TranscriptReady event we insert the text directly into the
            // prompt so the user can review and submit it.
            {
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
                            self.status_message =
                                Some("Transcribing\u{2026}".to_string());
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
                                self.status_message = Some(
                                    format!("Transcribed: {}", &text[..text.len().min(60)])
                                );
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

            // Draw the frame, and immediately scan the *just-rendered*
            // buffer for URL runs. ratatui swaps its two buffers at the
            // end of draw(), so by the time draw() returns,
            // `terminal.current_buffer_mut()` points at the empty next-frame
            // slot. `CompletedFrame.buffer` is the one we actually want.
            let osc8_hits = {
                let completed = terminal.draw(|f| render::render_app(f, self))?;
                crate::osc8::scan_buffer_for_urls(completed.buffer)
            };

            // Post-paint OSC 8 overlay: re-emit URL cells wrapped in
            // hyperlink escapes so terminals that support OSC 8 (Windows
            // Terminal, iTerm2, WezTerm, Kitty, Konsole, VS Code, …) make
            // them Ctrl/Cmd-clickable. Failure is non-fatal — we never want
            // an overlay glitch to kill the TUI.
            if let Err(err) = crate::osc8::emit_hits(&osc8_hits) {
                tracing::debug!(target: "osc8", "hyperlink overlay write failed: {err}");
            }

            // Replay a key that was saved by try_detect_paste_burst in a
            // previous iteration (e.g. a modifier key that terminated a burst).
            let pending = self.pending_key.take();

            // Poll for events with a short timeout so we can redraw for animation
            let got_event = pending.is_some()
                || event::poll(std::time::Duration::from_millis(50))?;

            if got_event {
                let event = if let Some(k) = pending {
                    Event::Key(k)
                } else {
                    event::read()?
                };
                match event {
                    Event::Key(key) => {
                        // On Windows crossterm fires both Press and Release events.
                        // We normally skip non-press events, but when voice PTT mode
                        // is active we need the Release event for the `V` key so we
                        // can stop recording as soon as the user lifts the key.
                        if key.kind != crossterm::event::KeyEventKind::Press {
                            // Handle V-key release to stop PTT recording.
                            if key.kind == crossterm::event::KeyEventKind::Release
                                && key.code == KeyCode::Char('v')
                                && key.modifiers == KeyModifiers::NONE
                                && self.voice_recording
                                && self.voice_recorder.is_some()
                            {
                                self.handle_voice_ptt_stop();
                            }
                            continue;
                        }

                        // ---- Paste-burst detection -----------------------------------------
                        // On Windows Terminal, Ctrl+V causes the terminal to write clipboard
                        // content as raw character events (not as Event::Paste).  Every `\n`
                        // fires as Enter (submitting the prompt) and stray `v` chars trigger
                        // voice PTT.  We detect this by draining the event queue with a
                        // zero-timeout immediately after the first character arrives — a paste
                        // dumps every character at once while normal typing rarely queues more
                        // than one char in the same 50 ms window.
                        if key.modifiers == KeyModifiers::NONE
                            || key.modifiers == KeyModifiers::SHIFT
                        {
                            if let KeyCode::Char(c) = key.code {
                                if self.prompt_is_accepting_text() {
                                    if let Some(burst) = self.try_detect_paste_burst(c) {
                                        self.handle_paste_data(burst);
                                        self.refresh_prompt_input();
                                        continue;
                                    }
                                }
                            }
                        }
                        // -------------------------------------------------------------------

                        let should_submit = self.handle_key_event(key);
                        // Honour `:q`/`:wq` from vim command-line mode
                        if self.prompt_input.vim_quit_requested {
                            self.prompt_input.vim_quit_requested = false;
                            self.should_exit = true;
                        }
                        if self.should_exit {
                            return Ok(None);
                        }
                        if should_submit {
                            // Dismiss any active error modal when the user sends a message
                            self.dismiss_error_notifications();
                            // Check if this is a slash command that should open a UI screen
                            if crate::input::is_slash_command(&self.prompt_input.text) {
                                let slash_input = self.prompt_input.text.clone();
                                let (cmd, args) =
                                        crate::input::parse_slash_command(&slash_input);
                                if self.intercept_slash_command_with_args(cmd, args) {
                                    self.clear_prompt();
                                    continue;
                                }
                            }
                            let input = self.take_input();
                            if !input.is_empty() {
                                return Ok(Some(input));
                            }
                        }
                    }
                    Event::Paste(data)
                        if !self.is_streaming
                            && self.permission_request.is_none()
                            && !self.history_search_overlay.visible
                            && self.history_search.is_none() =>
                    {
                        self.handle_paste_data(data);
                        self.refresh_prompt_input();
                    }
                    Event::Mouse(mouse_event) => {
                        self.handle_mouse_event(mouse_event);
                    }
                    _ => {}
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
