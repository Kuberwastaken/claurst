//! Steering, the queued-prompt dock and delivered input (#444).
//!
//! While a turn runs, Enter *steers*: the text is pushed to the shared
//! [`claurst_query::CommandQueue`], which the query loop drains before its next
//! model request. Tab *queues*: the text waits in `queued_messages` and is
//! submitted as a new turn once the current one ends. Both kinds show in the
//! dock above the prompt until delivered.

use claurst_core::types::Message;
use claurst_query::{CommandPriority, QueuedCommand};

use super::App;
use super::types::SystemMessageStyle;

/// How a pending prompt in the dock will be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    /// Enter during a turn: delivered at the next step boundary.
    Steer,
    /// Tab during a turn: submitted as a new turn after this one ends.
    Queued,
}

/// The opening tag line of a background-task completion notice
/// (`<subagent …>` / `<background-shell …>`), or `None` for ordinary input.
fn completion_notice_header(text: &str) -> Option<&str> {
    if text.starts_with("<subagent ") || text.starts_with("<background-shell ") {
        text.lines().next()
    } else {
        None
    }
}

impl App {
    /// Enter during a turn: deliver `text` to the model at the next step
    /// boundary. Without a command queue it is queued for after the turn.
    pub fn steer(&mut self, text: String) {
        match self.command_queue {
            Some(ref queue) => {
                let id = queue.push(
                    QueuedCommand::InjectUserMessage(text.clone()),
                    CommandPriority::Normal,
                );
                self.pending_steers.push((id, text));
            }
            None => self.queued_messages.push_back(text),
        }
    }

    /// Pending prompts in delivery order: undelivered steers, then
    /// Tab-queued messages.
    pub fn pending_prompts(&self) -> impl Iterator<Item = (PendingKind, &str)> {
        self.pending_steers
            .iter()
            .map(|(_, text)| (PendingKind::Steer, text.as_str()))
            .chain(
                self.queued_messages
                    .iter()
                    .map(|text| (PendingKind::Queued, text.as_str())),
            )
    }

    /// Take the last prompt shown in the dock back out of it: the last
    /// Tab-queued message, else the last steer the loop has not drained yet.
    fn take_last_pending(&mut self) -> Option<String> {
        if let Some(text) = self.queued_messages.pop_back() {
            return Some(text);
        }
        let queue = self.command_queue.as_ref()?;
        while let Some((id, text)) = self.pending_steers.pop() {
            if queue.remove(id).is_some() {
                return Some(text);
            }
        }
        None
    }

    /// `editQueued`: move the last pending prompt back into the (empty)
    /// prompt so it can be edited and sent again.
    pub(super) fn edit_last_pending(&mut self) {
        if !self.prompt_input.is_empty() {
            self.status_message = Some("Clear the prompt to edit a queued message.".to_string());
            return;
        }
        if let Some(text) = self.take_last_pending() {
            self.set_prompt_text(text);
        }
    }

    /// `removeQueued`: drop the last pending prompt.
    pub(super) fn remove_last_pending(&mut self) {
        if self.take_last_pending().is_some() {
            self.status_message = Some("Removed a queued message.".to_string());
        }
    }

    /// On cancel: take undelivered steers out of the queue and Tab-queued
    /// prompts out of the dock and put them back in the prompt, so nothing
    /// typed is lost and nothing is sent after the user stopped the turn.
    /// Completion notices stay queued: the model was told not to poll for
    /// them, so the idle wake-up delivers them after the cancel.
    pub fn restore_pending_steers(&mut self) {
        let mut restored: Vec<String> = Vec::new();
        if let Some(ref queue) = self.command_queue {
            for (id, text) in self.pending_steers.drain(..) {
                if queue.remove(id).is_some() {
                    restored.push(text);
                }
            }
        }
        self.pending_steers.clear();
        restored.extend(self.queued_messages.drain(..));
        if restored.is_empty() {
            return;
        }
        if !self.prompt_input.is_empty() {
            restored.push(std::mem::take(&mut self.prompt_input.text));
        }
        self.set_prompt_text(restored.join("\n"));
    }

    /// The query loop delivered queued input at a step boundary: drop the
    /// delivered steers from the dock and show the input in the transcript.
    pub(super) fn show_injected_input(&mut self, texts: Vec<String>) {
        if let Some(ref queue) = self.command_queue {
            self.pending_steers.retain(|(id, _)| queue.contains(*id));
        }
        for text in texts {
            match completion_notice_header(&text) {
                Some(header) => {
                    let line = format!("Background task finished: {header}");
                    self.push_system_message(line, SystemMessageStyle::Info);
                }
                None => self.push_message(Message::user(text)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::config::Config;
    use claurst_core::cost::CostTracker;
    use claurst_core::types::Role;
    use claurst_query::{CommandQueue, QueryEvent};

    fn app_with_queue() -> (App, CommandQueue) {
        let mut app = App::new(Config::default(), CostTracker::new());
        let queue = CommandQueue::new();
        app.command_queue = Some(queue.clone());
        (app, queue)
    }

    fn dock(app: &App) -> Vec<(PendingKind, String)> {
        app.pending_prompts().map(|(kind, text)| (kind, text.to_string())).collect()
    }

    #[test]
    fn steers_reach_the_queue_and_queued_prompts_do_not() {
        let (mut app, queue) = app_with_queue();
        app.steer("steer one".to_string());
        app.queued_messages.push_back("after the turn".to_string());
        app.steer("steer two".to_string());
        assert_eq!(
            dock(&app),
            [
                (PendingKind::Steer, "steer one".to_string()),
                (PendingKind::Steer, "steer two".to_string()),
                (PendingKind::Queued, "after the turn".to_string()),
            ]
        );
        let delivered: Vec<String> = queue
            .drain()
            .into_iter()
            .map(|command| match command {
                QueuedCommand::InjectUserMessage(text) => text,
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(delivered, ["steer one", "steer two"]);
    }

    #[test]
    fn delivered_steers_leave_the_dock_and_enter_the_transcript() {
        let (mut app, queue) = app_with_queue();
        app.steer("look at main.rs".to_string());
        let texts: Vec<String> = claurst_query::drain_command_queue(&queue)
            .iter()
            .map(Message::get_all_text)
            .collect();
        app.handle_query_event(QueryEvent::InputInjected(texts));
        assert!(app.pending_steers.is_empty());
        let last = app.messages.last().expect("steer shown");
        assert_eq!(last.role, Role::User);
        assert_eq!(last.get_all_text(), "look at main.rs");
    }

    #[test]
    fn completion_notices_show_as_annotations_not_user_messages() {
        let (mut app, _queue) = app_with_queue();
        let before = app.messages.len();
        app.handle_query_event(QueryEvent::InputInjected(vec![
            "<background-shell id=\"t1\" state=\"completed\" command=\"make\">\nok\n</background-shell>"
                .to_string(),
        ]));
        assert_eq!(app.messages.len(), before);
        let note = app.system_annotations.last().expect("annotation");
        assert!(note.text.contains("<background-shell id=\"t1\""), "{}", note.text);
    }

    #[test]
    fn edit_and_remove_take_the_last_dock_entry() {
        let (mut app, queue) = app_with_queue();
        app.steer("first steer".to_string());
        app.steer("second steer".to_string());
        app.queued_messages.push_back("queued".to_string());

        app.edit_last_pending();
        assert_eq!(app.prompt_input.text, "queued");
        app.edit_last_pending();
        assert_eq!(app.prompt_input.text, "queued", "a non-empty prompt is never overwritten");

        app.set_prompt_text(String::new());
        app.edit_last_pending();
        assert_eq!(app.prompt_input.text, "second steer");
        assert_eq!(app.pending_steers.len(), 1);

        app.remove_last_pending();
        assert!(app.pending_steers.is_empty());
        assert!(queue.is_empty(), "removed steers never reach the model");
    }

    #[test]
    fn tab_during_a_turn_queues_and_alt_up_pulls_it_back() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (mut app, queue) = app_with_queue();
        app.is_streaming = true;
        app.set_prompt_text("next task".to_string());
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.queued_messages, ["next task"]);
        assert!(app.prompt_input.is_empty());
        assert!(queue.is_empty(), "a queued prompt is not a steer");

        app.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert_eq!(app.prompt_input.text, "next task");
        assert!(app.queued_messages.is_empty());
    }

    #[test]
    fn esc_and_ctrl_c_cancel_the_turn_the_same_way() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        for key in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let (mut app, queue) = app_with_queue();
            app.query_in_flight = true;
            app.is_streaming = true;
            app.streaming_text = "partial answer".to_string();
            app.retry_status = Some((1, 5));
            app.steer("not sent yet".to_string());
            app.handle_key_event(key);
            assert!(app.cancel_requested, "{key:?} must cancel the turn's token");
            assert!(!app.is_streaming);
            assert!(app.retry_status.is_none());
            assert_eq!(
                app.messages.last().map(Message::get_all_text).as_deref(),
                Some("partial answer"),
                "partial output stays visible"
            );
            assert!(queue.is_empty(), "no steer is delivered after a cancel");
            assert_eq!(app.prompt_input.text, "not sent yet");
        }
    }

    #[test]
    fn esc_between_model_requests_still_cancels() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (mut app, _queue) = app_with_queue();
        // Tools running: the query task is alive but nothing is streaming.
        app.query_in_flight = true;
        app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.cancel_requested);
    }

    #[test]
    fn tab_between_model_requests_still_queues() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (mut app, _queue) = app_with_queue();
        // Tools running: the query task is alive but nothing is streaming.
        app.query_in_flight = true;
        let mode = app.agent_mode.clone();
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.agent_mode, mode, "an empty-prompt Tab must not switch agent mode mid-turn");
        assert!(!app.agent_mode_changed);
        app.set_prompt_text("next".to_string());
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.queued_messages, ["next"]);
        assert!(app.prompt_input.is_empty());
    }

    #[test]
    fn esc_esc_on_an_idle_prompt_does_nothing() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (mut app, _queue) = app_with_queue();
        app.push_message(Message::user("earlier prompt"));
        app.set_prompt_text("draft".to_string());
        for _ in 0..2 {
            app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
        assert!(!app.cancel_requested);
        assert!(!app.rewind_flow.visible);
        assert_eq!(app.prompt_input.text, "draft");
    }

    #[test]
    fn cancel_returns_steers_and_queued_prompts_and_keeps_notices() {
        let (mut app, queue) = app_with_queue();
        app.steer("one".to_string());
        app.steer("two".to_string());
        app.queued_messages.push_back("later".to_string());
        queue.push(
            QueuedCommand::InjectUserMessage(
                "<background-shell id=\"t\">x</background-shell>".to_string(),
            ),
            CommandPriority::Normal,
        );
        app.set_prompt_text("draft".to_string());
        app.restore_pending_steers();
        assert_eq!(app.prompt_input.text, "one\ntwo\nlater\ndraft");
        assert!(app.pending_steers.is_empty());
        assert!(app.queued_messages.is_empty(), "nothing auto-submits after a cancel");
        let left: Vec<String> = queue
            .drain()
            .into_iter()
            .map(|command| match command {
                QueuedCommand::InjectUserMessage(text) => text,
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            left,
            ["<background-shell id=\"t\">x</background-shell>"],
            "the completion notice waits for the idle wake-up"
        );
    }
}
