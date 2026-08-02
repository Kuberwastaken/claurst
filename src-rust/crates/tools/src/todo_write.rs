// TodoWrite tool: task / todo list management.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tracing::debug;

// ---------------------------------------------------------------------------
// Session-aware persistence helpers
// ---------------------------------------------------------------------------

/// Validate that `session_id` is a plain filename — no path separators or
/// `..` components that could be used for directory traversal (issue #204).
fn validate_session_id(session_id: &str) -> Result<(), String> {
    if session_id.contains('/') || session_id.contains('\\') || session_id.contains("..") {
        return Err("session_id contains illegal characters".into());
    }
    Ok(())
}

/// Returns the path to the persisted todo list for `session_id`.
pub fn todos_path(session_id: &str) -> anyhow::Result<PathBuf> {
    validate_session_id(session_id).map_err(|e| anyhow::anyhow!(e))?;
    Ok(todos_dir().join(format!("{}.json", session_id)))
}

/// Directory holding persisted todo lists (`<claurst home>/todos`).
fn todos_dir() -> PathBuf {
    claurst_core::config::Settings::config_dir().join("todos")
}

/// Load the persisted todo list for `session_id`. Returns an empty vec if the
/// file does not exist, cannot be parsed, or if `session_id` contains illegal
/// path characters (issue #204).
pub fn load_todos(session_id: &str) -> Vec<Value> {
    load_todos_in(&todos_dir(), session_id)
}

/// Like [`load_todos`] but reads from an explicit todos directory. Lets tests
/// run hermetically without depending on a writable HOME.
///
/// Returns an empty vec when `session_id` contains illegal path characters
/// (issue #204).
pub fn load_todos_in(dir: &Path, session_id: &str) -> Vec<Value> {
    if validate_session_id(session_id).is_err() {
        return vec![];
    }
    let path = dir.join(format!("{}.json", session_id));
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default()
}

/// Persist `todos` to `~/.claurst/todos/<session_id>.json`.
pub fn save_todos(session_id: &str, todos: &[Value]) {
    save_todos_in(&todos_dir(), session_id, todos);
}

/// Like [`save_todos`] but writes into an explicit todos directory. Lets tests
/// run hermetically without depending on a writable HOME.
///
/// Silently returns when `session_id` contains illegal path characters
/// (issue #204).
pub fn save_todos_in(dir: &Path, session_id: &str, todos: &[Value]) {
    if validate_session_id(session_id).is_err() {
        return;
    }
    let path = dir.join(format!("{}.json", session_id));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(serialized) = serde_json::to_string_pretty(todos) {
        let _ = std::fs::write(&path, serialized);
    }
}

// ---------------------------------------------------------------------------
// Status enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// Parse case-insensitively from a string.
    fn from_str_ci(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "pending" => Ok(TodoStatus::Pending),
            "in_progress" => Ok(TodoStatus::InProgress),
            "completed" => Ok(TodoStatus::Completed),
            other => Err(format!(
                "Invalid status {:?}: must be one of \"pending\", \"in_progress\", or \"completed\".",
                other
            )),
        }
    }
}

impl<'de> serde::Deserialize<'de> for TodoStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        TodoStatus::from_str_ci(&s).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for TodoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TodoStatus::Pending => write!(f, "pending"),
            TodoStatus::InProgress => write!(f, "in_progress"),
            TodoStatus::Completed => write!(f, "completed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

pub struct TodoWriteTool;

#[derive(Debug, Deserialize)]
struct TodoWriteInput {
    todos: Vec<TodoItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct TodoItem {
    id: String,
    content: String,
    status: TodoStatus,
    #[serde(default)]
    #[allow(dead_code)]
    priority: Option<String>,
    // --- NEW FIELDS ---
    #[serde(default)]
    #[allow(dead_code)]
    group: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    confidence: Option<u8>,
    #[serde(default)]
    #[allow(dead_code)]
    completion_confidence: Option<u8>,
    #[serde(default)]
    #[allow(dead_code)]
    confidence_history: Vec<u8>,
    #[serde(default)]
    #[allow(dead_code)]
    blocked_by: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    assigned_to: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    force_reopen: bool,
}

// ---------------------------------------------------------------------------
// Transition validation
// ---------------------------------------------------------------------------

/// Check that a status transition from `old` to `new` is permitted.
///
/// Allowed:
///   pending     → in_progress   ✓
///   pending     → completed     ✓  (direct completion)
///   in_progress → completed     ✓
///
/// Forbidden:
///   completed   → anything      ✗  (completed tasks cannot be reopened)
///   in_progress → pending       ✗  (cannot move backwards)
fn validate_transition(
    id: &str,
    old: &TodoStatus,
    new: &TodoStatus,
    force_reopen: bool,
) -> Result<(), String> {
    if old == new {
        return Ok(());
    }
    match (old, new) {
        (TodoStatus::Completed, _) if !force_reopen => Err(format!(
            "Task {:?}: cannot change status of a completed task (currently \"completed\" → \"{}\"). Use force_reopen: true if poke verification found a false completion.",
            id, new
        )),
        (TodoStatus::Completed, _) if force_reopen => Ok(()), // Force reopen allowed
        (TodoStatus::InProgress, TodoStatus::Pending) => Err(format!(
            "Task {:?}: cannot move status backwards (\"in_progress\" → \"pending\").",
            id
        )),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Tool implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        claurst_core::constants::TOOL_NAME_TODO_WRITE
    }

    fn description(&self) -> &str {
        "Write and manage a todo/task list. Provide the complete list of todos \
         each time (this replaces the entire list). Use this to track progress \
         on multi-step tasks."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            },
                            "priority": { "type": "string" },
                            "group": { "type": "string" },
                            "confidence": { "type": "integer", "minimum": 0, "maximum": 100 },
                            "completion_confidence": { "type": "integer", "minimum": 0, "maximum": 100 },
                            "confidence_history": { "type": "array", "items": { "type": "integer" } },
                            "blocked_by": { "type": "array", "items": { "type": "string" } },
                            "assigned_to": { "type": "string" },
                            "force_reopen": { "type": "boolean" }
                        },
                        "required": ["id", "content", "status"]
                    },
                    "description": "The complete list of todo items"
                }
            },
            "required": ["todos"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        // --- 1. Deserialize & validate statuses (case-insensitive) ----------
        let params: TodoWriteInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        debug!(count = params.todos.len(), "Writing todo list");

        // --- 2. Task ID uniqueness check ------------------------------------
        // IDs must be unique within the incoming list itself.
        {
            let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for item in &params.todos {
                if !seen_ids.insert(item.id.as_str()) {
                    return ToolResult::error(format!(
                        "Duplicate task ID {:?} in the provided list. IDs must be unique.",
                        item.id
                    ));
                }
            }
        }

        // --- 3. Load persisted state & enforce status-transition rules -------
        let persisted = load_todos(&ctx.session_id);

        // Build a map of existing id → status from the persisted list.
        let existing: std::collections::HashMap<&str, TodoStatus> = persisted
            .iter()
            .filter_map(|v| {
                let id = v.get("id")?.as_str()?;
                let raw = v.get("status")?.as_str()?;
                TodoStatus::from_str_ci(raw).ok().map(|s| (id, s))
            })
            .collect();

        // Collect the set of IDs that were newly completed in *this* call,
        // so we can craft accurate nudge messaging.
        let mut newly_completed_ids: std::collections::HashSet<&str> =
            std::collections::HashSet::new();

        for item in &params.todos {
            match existing.get(item.id.as_str()) {
                Some(old_status) => {
                    // Existing task — validate the transition.
                    if let Err(e) =
                        validate_transition(&item.id, old_status, &item.status, item.force_reopen)
                    {
                        return ToolResult::error(e);
                    }
                    if old_status != &TodoStatus::Completed && item.status == TodoStatus::Completed
                    {
                        newly_completed_ids.insert(&item.id);
                    }
                }
                None => {
                    // New task — IDs must not collide with persisted ones.
                    // (They aren't in the map, so no collision; nothing extra to check.)
                }
            }
        }

        // --- 4. Counts -------------------------------------------------------
        let total = params.todos.len();
        let completed = params
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        let in_progress = params
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::InProgress)
            .count();
        let pending = total - completed - in_progress;

        // --- 5. Build human-readable output ----------------------------------
        let mut output = format!(
            "Todo list updated ({} total: {} pending, {} in progress, {} completed)\n\n",
            total, pending, in_progress, completed
        );

        for item in &params.todos {
            let icon = match item.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Completed => "[x]",
            };
            output.push_str(&format!("{} {} ({})\n", icon, item.content, item.id));
        }

        // --- 6. Persist to disk ----------------------------------------------
        let todos_json: Vec<Value> = params
            .todos
            .iter()
            .map(|t| {
                let mut obj = json!({
                    "id": t.id,
                    "content": t.content,
                    "status": t.status.to_string(),
                });
                if let Some(ref p) = t.priority {
                    obj["priority"] = json!(p);
                }
                if let Some(ref g) = t.group {
                    obj["group"] = json!(g);
                }
                if let Some(c) = t.confidence {
                    obj["confidence"] = json!(c);
                }
                if let Some(c) = t.completion_confidence {
                    obj["completion_confidence"] = json!(c);
                }
                if !t.confidence_history.is_empty() {
                    // Cap at 20 entries (ring buffer).
                    let history: Vec<u8> = t
                        .confidence_history
                        .iter()
                        .rev()
                        .take(20)
                        .copied()
                        .collect::<Vec<u8>>()
                        .into_iter()
                        .rev()
                        .collect();
                    obj["confidence_history"] = json!(history);
                }
                if !t.blocked_by.is_empty() {
                    obj["blocked_by"] = json!(t.blocked_by);
                }
                if let Some(ref a) = t.assigned_to {
                    obj["assigned_to"] = json!(a);
                }
                obj
            })
            .collect();
        save_todos(&ctx.session_id, &todos_json);

        // --- 7. Session-end verification message / completion nudge ----------
        if total == 0 || (pending == 0 && in_progress == 0) {
            // All tasks completed (or the list was cleared).
            if total > 0 {
                output.push_str(
                    "\n\nAll tasks completed! Great work — the session todo list is fully done.",
                );
            }
        } else {
            // Some tasks remain.

            // In-progress nudge: fire only when there are in_progress tasks
            // that were NOT just completed in this very call.
            if in_progress > 0 {
                output.push_str(&format!(
                    "\n\nReminder: {} task{} are in_progress — complete them before marking the session done.",
                    in_progress,
                    if in_progress == 1 { "" } else { "s" }
                ));
            }

            // General incomplete warning.
            let incomplete = pending + in_progress;
            output.push_str(&format!(
                "\n\nWARNING: {} task{} still incomplete. Continue working on them.",
                incomplete,
                if incomplete == 1 { " is" } else { "s are" }
            ));
        }

        ToolResult::success(output).with_metadata(json!({
            "total": total,
            "completed": completed,
            "in_progress": in_progress,
            "pending": pending,
        }))
    }
}

// ---------------------------------------------------------------------------
// Auto-poke mechanism (ported from jcode)
// ---------------------------------------------------------------------------

/// Build the synthetic continuation message sent when the model stops with
/// incomplete todos. The message instructs the model to continue working.
pub fn build_auto_poke_message(incomplete_count: usize) -> String {
    format!(
        "You have {} incomplete todo{}. Continue working, or update the todo tool.",
        incomplete_count,
        if incomplete_count == 1 { "" } else { "s" },
    )
}

/// Check whether a message is an auto-poke synthetic continuation prompt.
/// Used by the TUI to hide the raw message and show "Auto-poking..." instead.
pub fn is_auto_poke_message(message: &str) -> bool {
    message.starts_with("You have ")
        && message.contains(" incomplete todo")
        && message.contains("Continue working, or update the todo tool.")
}

/// Count incomplete (pending + in_progress) todos for a session.
pub fn count_incomplete_todos(session_id: &str) -> usize {
    let todos = load_todos(session_id);
    todos
        .iter()
        .filter_map(|t| t.get("status").and_then(|s| s.as_str()))
        .filter(|s| *s == "pending" || *s == "in_progress")
        .count()
}

/// Compute a hash of the current todo state (sorted [(id, status)] pairs).
/// Used for no-progress detection: if the hash is unchanged for 3 consecutive
/// turns, auto-poking stops.
pub fn todo_state_hash(session_id: &str) -> String {
    let todos = load_todos(session_id);
    let mut pairs: Vec<(String, String)> = todos
        .iter()
        .filter_map(|t| {
            let id = t.get("id").and_then(|v| v.as_str())?.to_string();
            let status = t.get("status").and_then(|v| v.as_str())?.to_string();
            Some((id, status))
        })
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    // Simple hash: concatenate id:status pairs.
    pairs
        .iter()
        .map(|(id, s)| format!("{}:{}", id, s))
        .collect::<Vec<_>>()
        .join("|")
}

/// Maximum auto-pokes per session (safety budget).
pub const MAX_AUTO_POKES: u32 = 48;

/// No-progress threshold: stop after this many consecutive no-progress turns.
pub const NO_PROGRESS_THRESHOLD: u32 = 3;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_todos_path_contains_session_id() {
        let path = todos_path("my-session-123").unwrap();
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("my-session-123"),
            "todos_path should embed the session id"
        );
        // Route the assertion through the same canonical resolver instead of
        // hardcoding `.claurst`: the todos file must live under the resolved
        // claurst home (which may be ~/.claurst, $CLAURST_HOME, or the XDG dir).
        let home = claurst_core::config::Settings::config_dir();
        assert!(
            path.starts_with(home.join("todos")),
            "todos_path should be under the claurst home"
        );
        assert!(
            path_str.ends_with(".json"),
            "todos_path should end with .json"
        );
    }

    #[test]
    fn test_load_todos_missing_file_returns_empty() {
        let todos = load_todos("nonexistent-session-zzzzzz-99999");
        assert!(todos.is_empty(), "Missing file should yield empty vec");
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let session_id = format!(
            "test-session-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let todos = vec![
            json!({"id": "1", "content": "Task one", "status": "pending"}),
            json!({"id": "2", "content": "Task two", "status": "completed"}),
        ];
        let dir = tempfile::tempdir().expect("tempdir");
        save_todos_in(dir.path(), &session_id, &todos);
        let loaded = load_todos_in(dir.path(), &session_id);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0]["id"].as_str(), Some("1"));
        assert_eq!(loaded[1]["status"].as_str(), Some("completed"));
        // tempdir cleans up automatically.
    }

    // --- Status parsing ------------------------------------------------------

    #[test]
    fn test_status_parsing_case_insensitive() {
        assert_eq!(
            TodoStatus::from_str_ci("PENDING").unwrap(),
            TodoStatus::Pending
        );
        assert_eq!(
            TodoStatus::from_str_ci("In_Progress").unwrap(),
            TodoStatus::InProgress
        );
        assert_eq!(
            TodoStatus::from_str_ci("COMPLETED").unwrap(),
            TodoStatus::Completed
        );
        assert!(TodoStatus::from_str_ci("done").is_err());
        assert!(TodoStatus::from_str_ci("").is_err());
    }

    #[test]
    fn test_status_display() {
        assert_eq!(TodoStatus::Pending.to_string(), "pending");
        assert_eq!(TodoStatus::InProgress.to_string(), "in_progress");
        assert_eq!(TodoStatus::Completed.to_string(), "completed");
    }

    // --- Transition rules ----------------------------------------------------

    #[test]
    fn test_valid_transitions() {
        // pending → in_progress
        assert!(
            validate_transition("t1", &TodoStatus::Pending, &TodoStatus::InProgress, false).is_ok()
        );
        // pending → completed
        assert!(
            validate_transition("t2", &TodoStatus::Pending, &TodoStatus::Completed, false).is_ok()
        );
        // in_progress → completed
        assert!(
            validate_transition("t3", &TodoStatus::InProgress, &TodoStatus::Completed, false)
                .is_ok()
        );
        // no-op transitions are always fine
        assert!(
            validate_transition("t4", &TodoStatus::Pending, &TodoStatus::Pending, false).is_ok()
        );
        assert!(validate_transition(
            "t5",
            &TodoStatus::InProgress,
            &TodoStatus::InProgress,
            false
        )
        .is_ok());
        assert!(
            validate_transition("t6", &TodoStatus::Completed, &TodoStatus::Completed, false)
                .is_ok()
        );
    }

    #[test]
    fn test_invalid_transition_completed_to_anything() {
        assert!(
            validate_transition("t1", &TodoStatus::Completed, &TodoStatus::Pending, false).is_err()
        );
        assert!(
            validate_transition("t2", &TodoStatus::Completed, &TodoStatus::InProgress, false)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_transition_in_progress_to_pending() {
        assert!(
            validate_transition("t1", &TodoStatus::InProgress, &TodoStatus::Pending, false)
                .is_err()
        );
    }

    // --- ID uniqueness -------------------------------------------------------

    #[test]
    fn test_status_from_str_invalid() {
        let err = TodoStatus::from_str_ci("banana").unwrap_err();
        assert!(
            err.contains("Invalid status"),
            "error should mention invalid status"
        );
        assert!(err.contains("banana"), "error should echo the bad value");
    }

    // --- Auto-poke ----------------------------------------------------------

    #[test]
    fn build_auto_poke_message_singular() {
        let msg = build_auto_poke_message(1);
        assert!(msg.contains("1 incomplete todo"));
        assert!(!msg.contains("todos"));
    }

    #[test]
    fn build_auto_poke_message_plural() {
        let msg = build_auto_poke_message(5);
        assert!(msg.contains("5 incomplete todos"));
    }

    #[test]
    fn is_auto_poke_message_recognizes_poke() {
        let msg = build_auto_poke_message(3);
        assert!(is_auto_poke_message(&msg));
    }

    #[test]
    fn is_auto_poke_message_rejects_normal() {
        assert!(!is_auto_poke_message("Hello, can you help me?"));
        assert!(!is_auto_poke_message("Continue working on the task."));
    }

    // --- TodoItem deserialization with new fields ----------------------------

    #[test]
    fn todo_item_deserialize_with_old_schema() {
        let json = r#"{"id":"1","content":"Test","status":"pending"}"#;
        let item: TodoItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.id, "1");
        assert!(item.group.is_none());
        assert!(item.confidence.is_none());
        assert!(item.confidence_history.is_empty());
        assert!(item.blocked_by.is_empty());
        assert!(!item.force_reopen);
    }

    #[test]
    fn todo_item_deserialize_with_new_schema() {
        let json = r#"{
            "id": "2",
            "content": "New fields test",
            "status": "in_progress",
            "priority": "high",
            "group": "Phase 1",
            "confidence": 75,
            "blocked_by": ["1"],
            "force_reopen": false
        }"#;
        let item: TodoItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.group.as_deref(), Some("Phase 1"));
        assert_eq!(item.confidence, Some(75));
        assert_eq!(item.blocked_by, vec!["1"]);
    }

    // --- Force reopen transition --------------------------------------------

    #[test]
    fn validate_transition_force_reopen_allows_completed_to_pending() {
        let result =
            validate_transition("test", &TodoStatus::Completed, &TodoStatus::Pending, true);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_transition_without_force_reopen_blocks_completed_to_pending() {
        let result =
            validate_transition("test", &TodoStatus::Completed, &TodoStatus::Pending, false);
        assert!(result.is_err());
    }
}
