// command_queue.rs — T1-4: Command Queue Draining
//
// A priority queue shared between the TUI input thread and the query loop.
// Commands are drained at the start of each turn, before the API call.
//
// Mirrors the TypeScript `messageQueueManager.js` behaviour.

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
    pub fn push(&self, command: QueuedCommand, priority: CommandPriority) -> u64 {
        let mut state = self.state();
        let seq = state.next_seq;
        state.next_seq += 1;
        state.heap.push(QueueEntry { command, priority, seq });
        seq
    }

    /// Drain all pending commands in priority order (highest first).
    pub fn drain(&self) -> Vec<QueuedCommand> {
        let mut state = self.state();
        let mut out = Vec::with_capacity(state.heap.len());
        while let Some(entry) = state.heap.pop() {
            out.push(entry.command);
        }
        out
    }

    /// Take back a command that has not been drained yet.
    pub fn remove(&self, id: u64) -> Option<QueuedCommand> {
        let mut state = self.state();
        let mut entries = std::mem::take(&mut state.heap).into_vec();
        let removed = entries
            .iter()
            .position(|entry| entry.seq == id)
            .map(|index| entries.swap_remove(index).command);
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
            queue.push(QueuedCommand::InjectUserMessage(text.into()), CommandPriority::Normal);
        }
        assert_eq!(texts(queue.drain()), ["first", "second", "third"]);
        assert!(queue.is_empty());
    }

    #[test]
    fn higher_priority_drains_first() {
        let queue = CommandQueue::new();
        queue.push(QueuedCommand::InjectUserMessage("normal".into()), CommandPriority::Normal);
        queue.push(QueuedCommand::InjectUserMessage("high".into()), CommandPriority::High);
        assert_eq!(texts(queue.drain()), ["high", "normal"]);
    }

    #[test]
    fn remove_takes_back_one_command_and_keeps_the_order_of_the_rest() {
        let queue = CommandQueue::new();
        let a = queue.push(QueuedCommand::InjectUserMessage("a".into()), CommandPriority::Normal);
        let b = queue.push(QueuedCommand::InjectUserMessage("b".into()), CommandPriority::Normal);
        let c = queue.push(QueuedCommand::InjectUserMessage("c".into()), CommandPriority::Normal);
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
        other.push(QueuedCommand::InjectUserMessage("x".into()), CommandPriority::Normal);
        assert!(!queue.is_empty());
        let messages = drain_command_queue(&queue);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].get_all_text(), "x");
        assert!(other.is_empty());
    }
}
