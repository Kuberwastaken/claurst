// command_queue.rs — T1-4: Command Queue Draining
//
// A priority queue shared between the TUI input thread and the query loop.
// Commands are drained at the start of each turn, before the API call.
//
// Mirrors the TypeScript `messageQueueManager.js` behaviour.

use claurst_core::PersistedSoftInterrupt;

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

// ---------------------------------------------------------------------------
// Priority
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandPriority {
    Interrupt = 3,
    High = 2,
    Normal = 1,
    Low = 0,
}

impl PartialOrd for CommandPriority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CommandPriority {
    fn cmp(&self, other: &Self) -> Ordering {
        (*self as u8).cmp(&(*other as u8))
    }
}

// ---------------------------------------------------------------------------
// Command variants
// ---------------------------------------------------------------------------

/// Commands that can be enqueued by the TUI or other components.
#[derive(Debug, Clone)]
pub enum QueuedCommand {
    /// /compact — compress conversation context
    Compact,
    /// /clear — reset conversation history
    Clear,
    /// Change the active model
    SetModel(String),
    /// Inject a plain user message into the conversation
    InjectUserMessage(String),
    /// Inject a system-level message (sent as a user message with [System] prefix)
    InjectSystemMessage(String),
    /// Trigger a named skill
    TriggerSkill(String),
}

// ---------------------------------------------------------------------------
// Internal heap entry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct QueueEntry {
    command: QueuedCommand,
    priority: CommandPriority,
    /// Push order, unique per queue. Tie-breaker among equal priorities
    /// (lower = older = first) and the handle returned by [`CommandQueue::push`].
    seq: u64,
    /// Ordering is separate from the public take-back handle so older restored
    /// input can precede commands pushed before initialization.
    order: u64,
    /// Store snapshot persisted for this entry. `None` when the queue is not
    /// durable, or for command variants that carry no model-facing text.
    persisted: Option<PersistedSoftInterrupt>,
}

impl PartialEq for QueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.order == other.order && self.seq == other.seq
    }
}

impl Eq for QueueEntry {}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority wins; among equal priorities, the older entry wins.
        self.priority
            .cmp(&other.priority)
            .then(other.order.cmp(&self.order))
            .then(other.seq.cmp(&self.seq))
    }
}

#[derive(Debug, Default)]
struct QueueState {
    heap: BinaryHeap<QueueEntry>,
    next_seq: u64,
    next_order: u64,
    persistence: Option<Persistence>,
}

#[derive(Debug, Clone)]
struct Persistence {
    directory: std::path::PathBuf,
    session: String,
}

// ---------------------------------------------------------------------------
// CommandQueue
// ---------------------------------------------------------------------------

/// Thread-safe priority queue of commands. Equal-priority commands drain in
/// push order.
///
/// Cloning the handle yields a second handle to the *same* queue (Arc semantics).
#[derive(Debug, Clone, Default)]
pub struct CommandQueue(Arc<Mutex<QueueState>>);

impl CommandQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// The queue state. A panic while the lock was held cannot leave the
    /// heap half-updated, so a poisoned lock is recovered rather than
    /// propagated.
    fn state(&self) -> MutexGuard<'_, QueueState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keep delayed background completions attached to the session that
    /// launched them, even after the interactive queue switches sessions.
    pub fn push_for_session(
        &self,
        session: &str,
        command: QueuedCommand,
        priority: CommandPriority,
    ) -> claurst_core::Result<()> {
        let mut state = self.state();
        let location = state
            .persistence
            .as_ref()
            .ok_or_else(|| claurst_core::ClaudeError::Other("session queue is not bound".into()))?
            .clone();
        if location.session != session {
            let entries =
                claurst_core::soft_interrupt_store::load_in(&location.directory, session)?;
            let order = entries
                .iter()
                .map(|entry| entry.seq)
                .max()
                .map_or(0, |seq| seq.saturating_add(1));
            if let Some(entry) = snapshot(&command, order, priority) {
                claurst_core::soft_interrupt_store::append_in(&location.directory, session, entry)?;
            }
            return Ok(());
        }
        let seq = state.next_seq;
        state.next_seq += 1;
        let order = state.next_order;
        state.next_order += 1;
        let persisted = snapshot(&command, order, priority);
        let persisted_result = persisted.as_ref().map_or(Ok(()), |entry| {
            claurst_core::soft_interrupt_store::append_in(
                &location.directory,
                session,
                entry.clone(),
            )
        });
        state.heap.push(QueueEntry {
            command,
            priority,
            seq,
            order,
            persisted,
        });
        persisted_result
    }

    /// Push a command and mirror its stable identity to the pending store.
    /// Store failures leave it queued; delivery retries persistence and never
    /// acknowledges input until the session journal accepts its message.
    pub fn push(&self, command: QueuedCommand, priority: CommandPriority) -> u64 {
        let mut state = self.state();
        let seq = state.next_seq;
        state.next_seq += 1;
        let order = state.next_order;
        state.next_order += 1;
        let persisted = state.persistence.as_ref().and_then(|location| {
            let snapshot = snapshot(&command, order, priority)?;
            if let Err(error) = claurst_core::soft_interrupt_store::append_in(&location.directory, &location.session, snapshot.clone()) {
                tracing::warn!(%error, "Steering input is queued but its pending-store write failed");
            }
            Some(snapshot)
        });
        state.heap.push(QueueEntry {
            command,
            priority,
            seq,
            order,
            persisted,
        });
        seq
    }

    pub fn enable_persistence(&self, session_id: &str) -> claurst_core::Result<()> {
        if self
            .state()
            .persistence
            .as_ref()
            .is_some_and(|location| location.session == session_id)
        {
            return Ok(());
        }
        self.enable_persistence_in(&claurst_core::config::Settings::config_dir(), session_id)
    }

    /// Initialize once without consuming pending input. The explicit directory
    /// also lets embedders and tests isolate stores without process-global env.
    pub fn enable_persistence_in(
        &self,
        directory: &std::path::Path,
        session_id: &str,
    ) -> claurst_core::Result<()> {
        self.bind_persistence(directory, session_id, false)
    }

    /// Called at the session transition, before accepting input for the new
    /// session. Parking retains the old outbox on disk without acknowledging it.
    pub fn switch_session(&self, session_id: &str) -> claurst_core::Result<()> {
        let directory = self
            .state()
            .persistence
            .as_ref()
            .map(|location| location.directory.clone())
            .unwrap_or_else(claurst_core::config::Settings::config_dir);
        self.bind_persistence(&directory, session_id, true)
    }

    fn bind_persistence(
        &self,
        directory: &std::path::Path,
        session_id: &str,
        allow_switch: bool,
    ) -> claurst_core::Result<()> {
        let mut state = self.state();
        let switching = state.persistence.as_ref().is_some_and(|location| {
            location.session != session_id || location.directory != directory
        });
        if let Some(location) = &state.persistence {
            if !switching {
                return Ok(());
            }
            if !state.heap.is_empty() {
                if !allow_switch {
                    return Err(claurst_core::ClaudeError::Other(
                        "switch the steering queue before accepting input for another session"
                            .into(),
                    ));
                }
                let parked: Vec<_> = state
                    .heap
                    .iter()
                    .filter_map(|entry| entry.persisted.clone())
                    .collect();
                claurst_core::soft_interrupt_store::merge_in(
                    &location.directory,
                    &location.session,
                    &parked,
                )?;
            }
        }
        let restored = claurst_core::soft_interrupt_store::load_in(directory, session_id)?;
        let mut next_seq = state.next_seq;
        let mut next_order = restored
            .iter()
            .map(|entry| entry.seq)
            .max()
            .map_or(0, |seq| seq.saturating_add(1));
        let mut entries = if switching {
            Vec::new()
        } else {
            state.heap.clone().into_vec()
        };
        entries.sort_by_key(|entry| entry.order);
        for entry in &mut entries {
            entry.order = next_order;
            next_order += 1;
            entry.persisted = snapshot(&entry.command, entry.order, entry.priority);
        }
        let pending: Vec<_> = entries
            .iter()
            .filter_map(|entry| entry.persisted.clone())
            .collect();
        let mut identities = std::collections::HashSet::new();
        for interrupt in restored {
            if !identities.insert(interrupt.id.clone()) {
                continue;
            }
            let priority = match interrupt.priority {
                0 => CommandPriority::Low,
                1 => CommandPriority::Normal,
                2 => CommandPriority::High,
                3 => CommandPriority::Interrupt,
                rank => {
                    return Err(claurst_core::ClaudeError::Other(format!(
                        "invalid persisted steering priority {rank}"
                    )));
                }
            };
            entries.push(QueueEntry {
                command: QueuedCommand::InjectUserMessage(interrupt.content.clone()),
                priority,
                seq: next_seq,
                order: interrupt.seq,
                persisted: Some(interrupt),
            });
            next_seq += 1;
        }
        claurst_core::soft_interrupt_store::merge_in(directory, session_id, &pending)?;
        state.heap = BinaryHeap::from(entries);
        state.next_seq = next_seq;
        state.next_order = next_order;
        state.persistence = Some(Persistence {
            directory: directory.to_path_buf(),
            session: session_id.to_string(),
        });
        Ok(())
    }

    /// Memory-only consumers may drain immediately. Durable queues must use
    /// deliver_command_queue so the journal commit precedes acknowledgement.
    pub fn drain(&self) -> Vec<QueuedCommand> {
        let mut state = self.state();
        if state.persistence.is_some() {
            tracing::warn!("Durable steering requires journal-backed delivery");
            return Vec::new();
        }
        let mut commands = Vec::with_capacity(state.heap.len());
        while let Some(entry) = state.heap.pop() {
            commands.push(entry.command);
        }
        commands
    }
    pub fn remove(&self, id: u64) -> Option<QueuedCommand> {
        match self.try_remove(id) {
            Ok(command) => command,
            Err(error) => {
                tracing::warn!(%error, "Could not take back persisted steering input");
                None
            }
        }
    }

    pub fn try_remove(&self, id: u64) -> claurst_core::Result<Option<QueuedCommand>> {
        let mut state = self.state();
        let Some(entry) = state.heap.iter().find(|entry| entry.seq == id) else {
            return Ok(None);
        };
        if let (Some(location), Some(persisted)) = (&state.persistence, &entry.persisted) {
            claurst_core::soft_interrupt_store::remove_by_ids_in(
                &location.directory,
                &location.session,
                std::slice::from_ref(&persisted.id),
            )?;
        }
        let mut entries = std::mem::take(&mut state.heap).into_vec();
        let removed = entries
            .iter()
            .position(|entry| entry.seq == id)
            .map(|index| entries.swap_remove(index).command);
        state.heap = BinaryHeap::from(entries);
        Ok(removed)
    }

    /// Snapshot for rebuilding a session's pending-input dock after a bind.
    pub fn pending_user_messages(&self) -> Vec<(u64, String)> {
        self.state()
            .heap
            .clone()
            .into_sorted_vec()
            .into_iter()
            .rev()
            .filter_map(|entry| match entry.command {
                QueuedCommand::InjectUserMessage(text) => Some((entry.seq, text)),
                _ => None,
            })
            .collect()
    }
    pub fn contains(&self, id: u64) -> bool {
        self.state().heap.iter().any(|entry| entry.seq == id)
    }

    pub fn is_empty(&self) -> bool {
        self.state().heap.is_empty()
    }
}
fn snapshot(
    command: &QueuedCommand,
    seq: u64,
    priority: CommandPriority,
) -> Option<PersistedSoftInterrupt> {
    let message = command_message(command)?;
    Some(PersistedSoftInterrupt {
        id: format!("steer-{}", uuid::Uuid::new_v4()),
        content: message.get_all_text(),
        seq,
        priority: priority as u8,
    })
}

fn command_message(command: &QueuedCommand) -> Option<claurst_core::types::Message> {
    use claurst_core::types::Message;
    match command {
        QueuedCommand::InjectUserMessage(text) => Some(Message::user(text)),
        QueuedCommand::InjectSystemMessage(text) => {
            Some(Message::user(format!("[System]: {text}")))
        }
        QueuedCommand::SetModel(_)
        | QueuedCommand::Compact
        | QueuedCommand::Clear
        | QueuedCommand::TriggerSkill(_) => None,
    }
}

pub fn drain_command_queue(queue: &CommandQueue) -> Vec<claurst_core::types::Message> {
    queue.drain().iter().filter_map(command_message).collect()
}

/// Commit pending identities to the session journal before removing them from
/// disk or the heap. Replays already present in recovered history are only
/// acknowledged, never appended again. An error leaves every entry pending.
pub fn deliver_command_queue(
    queue: &CommandQueue,

    messages: &mut Vec<claurst_core::types::Message>,
    journal: Option<&claurst_core::session_journal::SessionJournal>,
) -> claurst_core::Result<Vec<claurst_core::types::Message>> {
    let mut state = queue.state();
    let Some(location) = &state.persistence else {
        drop(state);
        let injected = drain_command_queue(queue);
        messages.extend(injected.iter().cloned());
        return Ok(injected);
    };
    let journal = journal.ok_or_else(|| {
        claurst_core::ClaudeError::Other("durable steering requires a session journal".into())
    })?;
    let pending: Vec<_> = state
        .heap
        .iter()
        .filter_map(|entry| entry.persisted.clone())
        .collect();
    claurst_core::soft_interrupt_store::merge_in(&location.directory, &location.session, &pending)?;
    let mut injected = Vec::new();
    for entry in state.heap.clone().into_sorted_vec().into_iter().rev() {
        if let Some(mut message) = command_message(&entry.command) {
            message.uuid = entry.persisted.as_ref().map(|item| item.id.clone());
            if message.uuid.as_ref().is_some_and(|id| {
                messages
                    .iter()
                    .any(|existing| existing.uuid.as_ref() == Some(id))
            }) {
                continue;
            }
            injected.push(message);
        }
    }
    let before = messages.len();
    messages.extend(injected.iter().cloned());
    if let Err(error) = journal.sync_messages(messages) {
        messages.truncate(before);
        return Err(claurst_core::ClaudeError::Other(format!(
            "steering was not journaled: {error}"
        )));
    }
    let ids: Vec<_> = pending.into_iter().map(|entry| entry.id).collect();
    claurst_core::soft_interrupt_store::remove_by_ids_in(
        &location.directory,
        &location.session,
        &ids,
    )?;
    state.heap.clear();
    Ok(injected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::types::Message;

    fn texts(commands: Vec<QueuedCommand>) -> Vec<String> {
        commands
            .into_iter()
            .map(|command| match command {
                QueuedCommand::InjectUserMessage(text) => text,
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn equal_priority_drains_in_push_order() {
        let queue = CommandQueue::new();
        for text in ["first", "second", "third"] {
            queue.push(
                QueuedCommand::InjectUserMessage(text.into()),
                CommandPriority::Normal,
            );
        }
        assert_eq!(texts(queue.drain()), ["first", "second", "third"]);
        assert!(queue.is_empty());
    }

    #[test]
    fn higher_priority_drains_first() {
        let queue = CommandQueue::new();
        queue.push(
            QueuedCommand::InjectUserMessage("normal".into()),
            CommandPriority::Normal,
        );
        queue.push(
            QueuedCommand::InjectUserMessage("high".into()),
            CommandPriority::High,
        );
        assert_eq!(texts(queue.drain()), ["high", "normal"]);
    }

    #[test]
    fn remove_takes_back_one_command_and_keeps_the_order_of_the_rest() {
        let queue = CommandQueue::new();
        let a = queue.push(
            QueuedCommand::InjectUserMessage("a".into()),
            CommandPriority::Normal,
        );
        let b = queue.push(
            QueuedCommand::InjectUserMessage("b".into()),
            CommandPriority::Normal,
        );
        let c = queue.push(
            QueuedCommand::InjectUserMessage("c".into()),
            CommandPriority::Normal,
        );
        assert!(queue.contains(b));
        assert!(matches!(queue.remove(b), Some(QueuedCommand::InjectUserMessage(t)) if t == "b"));
        assert!(!queue.contains(b));
        assert!(queue.remove(b).is_none(), "a removed id is gone");
        assert!(queue.contains(a) && queue.contains(c));
        assert_eq!(texts(queue.drain()), ["a", "c"]);
        assert!(!queue.contains(a), "drained ids are gone");
    }

    #[test]
    fn handles_share_one_queue() {
        let queue = CommandQueue::new();
        let other = queue.clone();
        other.push(
            QueuedCommand::InjectUserMessage("x".into()),
            CommandPriority::Normal,
        );
        assert!(!queue.is_empty());
        let messages = drain_command_queue(&queue);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].get_all_text(), "x");
        assert!(other.is_empty());
    }

    fn journal(
        dir: &std::path::Path,
        session: &str,
    ) -> Arc<claurst_core::session_journal::SessionJournal> {
        let mut conversation = claurst_core::history::ConversationSession::new("test-model".into());
        conversation.id = session.to_string();
        claurst_core::session_journal::SessionJournal::new(&dir.join("sessions"), &conversation)
    }

    #[test]
    fn repeated_initialization_is_idempotent_and_restore_survives_two_crashes() {
        let dir = tempfile::tempdir().unwrap();
        let queue = CommandQueue::new();
        queue.enable_persistence_in(dir.path(), "session").unwrap();
        queue.push(
            QueuedCommand::InjectUserMessage("keep me".into()),
            CommandPriority::High,
        );
        queue.enable_persistence_in(dir.path(), "session").unwrap();
        drop(queue);
        for _ in 0..2 {
            let revived = CommandQueue::new();
            revived
                .enable_persistence_in(dir.path(), "session")
                .unwrap();
            assert_eq!(revived.state().heap.len(), 1);
            // No journal acceptance: dropping another process cannot consume it.
        }
        assert_eq!(
            claurst_core::soft_interrupt_store::load_in(dir.path(), "session")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn durable_delivery_preserves_priority_and_acknowledges_only_committed_messages() {
        let dir = tempfile::tempdir().unwrap();
        let queue = CommandQueue::new();
        queue.enable_persistence_in(dir.path(), "session").unwrap();
        for (text, priority) in [
            ("normal", CommandPriority::Normal),
            ("high", CommandPriority::High),
            ("low", CommandPriority::Low),
        ] {
            queue.push(QueuedCommand::InjectUserMessage(text.into()), priority);
        }
        drop(queue);
        let revived = CommandQueue::new();
        revived
            .enable_persistence_in(dir.path(), "session")
            .unwrap();
        let journal = journal(dir.path(), "session");
        let mut history = Vec::new();
        let injected = deliver_command_queue(&revived, &mut history, Some(&journal)).unwrap();
        assert_eq!(
            injected
                .iter()
                .map(Message::get_all_text)
                .collect::<Vec<_>>(),
            ["high", "normal", "low"]
        );
        assert!(revived.is_empty());
        assert!(
            claurst_core::soft_interrupt_store::load_in(dir.path(), "session")
                .unwrap()
                .is_empty()
        );
        let saved = claurst_core::session_journal::load(&dir.path().join("sessions"), "session")
            .unwrap()
            .session
            .messages;
        assert_eq!(
            serde_json::to_value(saved).unwrap(),
            serde_json::to_value(history).unwrap()
        );
    }

    #[test]
    fn crash_after_journal_before_store_ack_does_not_duplicate_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let queue = CommandQueue::new();
        queue.enable_persistence_in(dir.path(), "session").unwrap();
        queue.push(
            QueuedCommand::InjectUserMessage("exactly once".into()),
            CommandPriority::Normal,
        );
        let pending = claurst_core::soft_interrupt_store::load_in(dir.path(), "session").unwrap();
        let mut message = Message::user(&pending[0].content);
        message.uuid = Some(pending[0].id.clone());
        let journal = journal(dir.path(), "session");
        journal.sync_messages(&[message]).unwrap();
        drop(journal);
        drop(queue);
        let loaded = claurst_core::session_journal::load(&dir.path().join("sessions"), "session")
            .unwrap()
            .session;
        let resumed_journal = claurst_core::session_journal::SessionJournal::new(
            &dir.path().join("sessions"),
            &loaded,
        );
        let mut history = loaded.messages;
        let revived = CommandQueue::new();
        revived
            .enable_persistence_in(dir.path(), "session")
            .unwrap();
        assert!(
            deliver_command_queue(&revived, &mut history, Some(&resumed_journal))
                .unwrap()
                .is_empty()
        );
        assert_eq!(history.len(), 1);
        assert!(revived.is_empty());
        assert!(
            claurst_core::soft_interrupt_store::load_in(dir.path(), "session")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn failed_journal_acceptance_keeps_pending_input_and_retry_delivers_once() {
        let dir = tempfile::tempdir().unwrap();
        let queue = CommandQueue::new();
        queue.enable_persistence_in(dir.path(), "session").unwrap();
        let handle = queue.push(
            QueuedCommand::InjectUserMessage("retry".into()),
            CommandPriority::Normal,
        );
        std::fs::write(dir.path().join("sessions"), "not a directory").unwrap();
        let journal = journal(dir.path(), "session");
        let mut history = Vec::new();
        assert!(deliver_command_queue(&queue, &mut history, Some(&journal)).is_err());
        assert!(history.is_empty());
        assert!(queue.contains(handle));
        assert_eq!(
            claurst_core::soft_interrupt_store::load_in(dir.path(), "session")
                .unwrap()
                .len(),
            1
        );
        std::fs::remove_file(dir.path().join("sessions")).unwrap();
        assert_eq!(
            deliver_command_queue(&queue, &mut history, Some(&journal))
                .unwrap()
                .len(),
            1
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn preinitialization_input_follows_older_restored_input_and_takeback_stays_exact() {
        let dir = tempfile::tempdir().unwrap();
        let old = CommandQueue::new();
        old.enable_persistence_in(dir.path(), "session").unwrap();
        old.push(
            QueuedCommand::InjectUserMessage("older".into()),
            CommandPriority::Normal,
        );
        drop(old);
        let queue = CommandQueue::new();
        let handle = queue.push(
            QueuedCommand::InjectUserMessage("newer".into()),
            CommandPriority::Normal,
        );
        queue.enable_persistence_in(dir.path(), "session").unwrap();
        assert!(queue.contains(handle));
        let mut history = Vec::new();
        let journal = journal(dir.path(), "session");
        assert_eq!(
            deliver_command_queue(&queue, &mut history, Some(&journal))
                .unwrap()
                .iter()
                .map(Message::get_all_text)
                .collect::<Vec<_>>(),
            ["older", "newer"]
        );
    }

    #[test]
    fn explicit_session_switch_parks_input_and_restores_only_its_own_session() {
        let dir = tempfile::tempdir().unwrap();
        let queue = CommandQueue::new();
        queue.enable_persistence_in(dir.path(), "A").unwrap();
        queue.push(
            QueuedCommand::InjectUserMessage("belongs to A".into()),
            CommandPriority::High,
        );
        let original = claurst_core::soft_interrupt_store::load_in(dir.path(), "A").unwrap();
        assert!(queue.enable_persistence_in(dir.path(), "B").is_err());
        assert_eq!(queue.pending_user_messages().len(), 1);
        queue.bind_persistence(dir.path(), "B", true).unwrap();
        assert!(queue.is_empty());
        queue.push(
            QueuedCommand::InjectUserMessage("belongs to B".into()),
            CommandPriority::Normal,
        );
        queue.bind_persistence(dir.path(), "A", true).unwrap();
        assert_eq!(
            queue
                .pending_user_messages()
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>(),
            ["belongs to A"]
        );
        assert_eq!(
            claurst_core::soft_interrupt_store::load_in(dir.path(), "A").unwrap(),
            original
        );
        assert_eq!(
            claurst_core::soft_interrupt_store::load_in(dir.path(), "B").unwrap()[0].content,
            "belongs to B"
        );
        assert_eq!(
            queue.state().heap.peek().unwrap().priority,
            CommandPriority::High
        );
    }

    #[test]
    fn delayed_completion_is_parked_with_its_launching_session() {
        let dir = tempfile::tempdir().unwrap();
        let queue = CommandQueue::new();
        queue.enable_persistence_in(dir.path(), "A").unwrap();
        queue.switch_session("B").unwrap();
        queue
            .push_for_session(
                "A",
                QueuedCommand::InjectUserMessage("A finished".into()),
                CommandPriority::Normal,
            )
            .unwrap();
        assert!(queue.is_empty());
        queue
            .push_for_session(
                "B",
                QueuedCommand::InjectUserMessage("B finished".into()),
                CommandPriority::Normal,
            )
            .unwrap();
        assert_eq!(queue.pending_user_messages()[0].1, "B finished");
        queue.switch_session("A").unwrap();
        assert_eq!(queue.pending_user_messages()[0].1, "A finished");
    }

    #[test]
    fn same_session_switch_is_idempotent_and_failed_target_load_keeps_old_queue() {
        let dir = tempfile::tempdir().unwrap();
        let queue = CommandQueue::new();
        queue.enable_persistence_in(dir.path(), "A").unwrap();
        let id = queue.push(
            QueuedCommand::InjectUserMessage("A input".into()),
            CommandPriority::High,
        );
        queue.switch_session("A").unwrap();
        assert_eq!(queue.pending_user_messages(), vec![(id, "A input".into())]);
        std::fs::write(dir.path().join("pending-soft-interrupts/B.json"), "broken").unwrap();
        assert!(queue.switch_session("B").is_err());
        assert_eq!(queue.pending_user_messages(), vec![(id, "A input".into())]);
        assert_eq!(queue.state().persistence.as_ref().unwrap().session, "A");
        assert_eq!(
            claurst_core::soft_interrupt_store::load_in(dir.path(), "A")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn taking_back_input_removes_only_its_persisted_identity() {
        let dir = tempfile::tempdir().unwrap();
        let queue = CommandQueue::new();
        queue.enable_persistence_in(dir.path(), "session").unwrap();
        let first = queue.push(
            QueuedCommand::InjectUserMessage("first".into()),
            CommandPriority::Normal,
        );
        queue.push(
            QueuedCommand::InjectUserMessage("second".into()),
            CommandPriority::High,
        );
        assert!(queue.remove(first).is_some());
        let pending = claurst_core::soft_interrupt_store::load_in(dir.path(), "session").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].content, "second");
    }
}
