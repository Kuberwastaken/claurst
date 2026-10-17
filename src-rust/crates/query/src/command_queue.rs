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

#[derive(Debug)]
struct QueueEntry {
    command: QueuedCommand,
    priority: CommandPriority,
    /// Push order, unique per queue. Tie-breaker among equal priorities
    /// (lower = older = first) and the handle returned by [`CommandQueue::push`].
    seq: u64,
    /// Store snapshot persisted for this entry. `None` when the queue is not
    /// durable, or for command variants that carry no model-facing text.
    persisted: Option<PersistedSoftInterrupt>,
}

impl PartialEq for QueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
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
            .then(other.seq.cmp(&self.seq))
    }
}

#[derive(Debug, Default)]
struct QueueState {
    heap: BinaryHeap<QueueEntry>,
    next_seq: u64,
    /// Session id when the queue mirrors its text entries into the
    /// soft-interrupt store so they survive a restart. `None`: memory only.
    persist_session: Option<String>,
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

    /// Push a command with the given priority. Returns an id that
    /// [`CommandQueue::remove`] and [`CommandQueue::contains`] accept.
    ///
    /// When persistence is enabled ([`CommandQueue::enable_persistence`]),
    /// text-carrying commands are also written to the soft-interrupt store so
    /// they survive a crash. A write failure is logged, not propagated: a full
    /// disk must not take the steering path down with it.
    pub fn push(&self, command: QueuedCommand, priority: CommandPriority) -> u64 {
        let mut state = self.state();
        let seq = state.next_seq;
        state.next_seq += 1;
        let persisted = self.persist_snapshot(&state, &command, seq, priority);
        state.heap.push(QueueEntry {
            command,
            priority,
            seq,
            persisted,
        });
        seq
    }

    /// Persist `command` when the queue is durable and it carries model-facing
    /// text. Returns the snapshot stored in the heap entry, if any.
    fn persist_snapshot(
        &self,
        state: &QueueState,
        command: &QueuedCommand,
        seq: u64,
        priority: CommandPriority,
    ) -> Option<PersistedSoftInterrupt> {
        let session_id = state.persist_session.as_deref()?;
        let interrupt = PersistedSoftInterrupt {
            content: match command {
                QueuedCommand::InjectUserMessage(text) => text.clone(),
                QueuedCommand::InjectSystemMessage(text) => format!("[System]: {text}"),
                // TUI-layer commands with no model-facing text are never
                // replayed after a restart, so they are not persisted.
                QueuedCommand::SetModel(_)
                | QueuedCommand::Compact
                | QueuedCommand::Clear
                | QueuedCommand::TriggerSkill(_) => return None,
            },
            seq,
            priority: priority as u8,
        };
        if let Err(err) = claurst_core::soft_interrupt_store::append(session_id, interrupt.clone())
        {
            tracing::warn!(session = session_id, error = %err, "Failed to persist a soft interrupt");
        }
        Some(interrupt)
    }

    /// Make the queue durable for `session_id` and restore the interrupts a
    /// previous run persisted for that session, in their original order.
    /// Restored entries replay as plain user messages at Normal priority: the
    /// store does not keep the original priority kind, only its rank, and the
    /// queue drains before the next request either way. A restore failure is
    /// logged and skipped: a corrupt or unwritable store must not block the
    /// session from starting.
    pub fn enable_persistence(&self, session_id: &str) {
        let mut state = self.state();
        match claurst_core::soft_interrupt_store::take(session_id) {
            Ok(restored) => {
                for interrupt in restored {
                    let seq = state.next_seq;
                    state.next_seq += 1;
                    state.heap.push(QueueEntry {
                        command: QueuedCommand::InjectUserMessage(interrupt.content),
                        priority: CommandPriority::Normal,
                        seq,
                        persisted: None,
                    });
                }
            }
            Err(err) => {
                tracing::warn!(
                    session = session_id,
                    error = %err,
                    "Failed to restore persisted soft interrupts"
                );
            }
        }
        state.persist_session = Some(session_id.to_string());
    }

    /// Drain all pending commands in priority order (highest first). A
    /// durable queue clears its store entry: every drained command has been
    /// delivered to the conversation, so nothing must replay after a restart.
    pub fn drain(&self) -> Vec<QueuedCommand> {
        let mut state = self.state();
        if let Some(session) = state.persist_session.clone() {
            if let Err(err) = claurst_core::soft_interrupt_store::overwrite(&session, &[]) {
                tracing::warn!(
                    session = session.as_str(),
                    error = %err,
                    "Failed to clear persisted soft interrupts on drain"
                );
            }
        }
        let mut out = Vec::with_capacity(state.heap.len());
        while let Some(entry) = state.heap.pop() {
            out.push(entry.command);
        }
        out
    }

    /// Take back a command that has not been drained yet. A durable queue
    /// also removes it from the soft-interrupt store.
    pub fn remove(&self, id: u64) -> Option<QueuedCommand> {
        let mut state = self.state();
        let mut entries = std::mem::take(&mut state.heap).into_vec();
        let removed = entries
            .iter()
            .position(|entry| entry.seq == id)
            .map(|index| {
                let entry = &entries[index];
                if let (Some(session), Some(persisted)) =
                    (state.persist_session.as_deref(), &entry.persisted)
                {
                    let _ = claurst_core::soft_interrupt_store::remove_by_seqs(
                        session,
                        &[persisted.seq],
                    );
                }
                entries.swap_remove(index).command
            });
        state.heap = BinaryHeap::from(entries);
        removed
    }

    /// Whether the command `id` is still waiting to be drained.
    pub fn contains(&self, id: u64) -> bool {
        self.state().heap.iter().any(|entry| entry.seq == id)
    }

    pub fn is_empty(&self) -> bool {
        self.state().heap.is_empty()
    }
}

// ---------------------------------------------------------------------------
// drain_command_queue
// ---------------------------------------------------------------------------

/// Consume all pending commands and convert them to `Message`s that can be
/// prepended to the conversation before the next API call.
///
/// Commands that are handled purely by the TUI/app layer (Compact, Clear,
/// SetModel, TriggerSkill) are silently dropped here — the query loop does
/// not need to act on them directly.
pub fn drain_command_queue(queue: &CommandQueue) -> Vec<claurst_core::types::Message> {
    use claurst_core::types::Message;

    let commands = queue.drain();
    let mut messages = Vec::new();

    for cmd in commands {
        match cmd {
            QueuedCommand::InjectUserMessage(text) => {
                messages.push(Message::user(text));
            }
            QueuedCommand::InjectSystemMessage(text) => {
                // System messages are injected as user turns with a [System]
                // prefix so they remain compatible with the Anthropic Messages API.
                messages.push(Message::user(format!("[System]: {text}")));
            }
            QueuedCommand::SetModel(_)
            | QueuedCommand::Compact
            | QueuedCommand::Clear
            | QueuedCommand::TriggerSkill(_) => {
                // Handled at the TUI/app layer; the query loop ignores them.
            }
        }
    }

    messages
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

    // ---- Soft-interrupt persistence ----------------------------------------
    //
    // These tests exercise the durable queue through the default config-dir
    // wrappers, which honour `CLAURST_HOME`. The env var is process-wide, so
    // the tests serialize on a mutex and each pins its own tempdir.

    static PERSIST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Pin the store to a fresh tempdir for `session`, enable persistence on a
    /// new queue, run the body, then clean the env. The tempdir is dropped
    /// last so the store outlives every queue the body created.
    fn with_pinned_store(session: &str, f: impl FnOnce(&CommandQueue, &str)) {
        let _guard = PERSIST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CLAURST_HOME", dir.path());
        let queue = CommandQueue::new();
        queue.enable_persistence(session);
        f(&queue, session);
        std::env::remove_var("CLAURST_HOME");
        drop(dir);
    }

    #[test]
    fn durable_push_survives_a_new_queue_for_the_same_session() {
        with_pinned_store("ses-durable", |queue, session| {
            queue.push(
                QueuedCommand::InjectUserMessage("steer one".into()),
                CommandPriority::Normal,
            );
            queue.push(
                QueuedCommand::InjectSystemMessage("notice".into()),
                CommandPriority::High,
            );

            // A fresh queue for the same session restores both, in push order.
            let revived = CommandQueue::new();
            revived.enable_persistence(session);
            let drained = drain_command_queue(&revived);
            let texts: Vec<String> = drained.iter().map(Message::get_all_text).collect();
            assert_eq!(texts, ["steer one", "[System]: notice"]);

            // The restore consumed the store: nothing replays twice.
            let again = CommandQueue::new();
            again.enable_persistence(session);
            assert!(again.is_empty());
        });
    }

    #[test]
    fn drain_clears_the_store() {
        with_pinned_store("ses-drain-clears", |queue, session| {
            queue.push(
                QueuedCommand::InjectUserMessage("lost on crash".into()),
                CommandPriority::Normal,
            );
            drain_command_queue(queue);
            let revived = CommandQueue::new();
            revived.enable_persistence(session);
            assert!(revived.is_empty(), "delivered input must not replay");
        });
    }

    #[test]
    fn remove_takes_the_entry_back_out_of_the_store() {
        with_pinned_store("ses-remove", |queue, session| {
            let keep = queue.push(
                QueuedCommand::InjectUserMessage("keep".into()),
                CommandPriority::Normal,
            );
            queue.push(
                QueuedCommand::InjectUserMessage("drop".into()),
                CommandPriority::Normal,
            );
            assert!(
                matches!(queue.remove(keep), Some(QueuedCommand::InjectUserMessage(t)) if t == "keep")
            );
            let revived = CommandQueue::new();
            revived.enable_persistence(session);
            let drained = drain_command_queue(&revived);
            assert_eq!(drained.len(), 1, "only the still-queued steer replays");
            assert_eq!(drained[0].get_all_text(), "drop");
        });
    }

    #[test]
    fn non_text_commands_are_not_persisted() {
        with_pinned_store("ses-no-text", |queue, session| {
            queue.push(QueuedCommand::Compact, CommandPriority::Normal);
            queue.push(
                QueuedCommand::SetModel("foo".into()),
                CommandPriority::Normal,
            );
            let revived = CommandQueue::new();
            revived.enable_persistence(session);
            assert!(revived.is_empty());
        });
    }
}
