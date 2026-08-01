# Feature: Todo Management + Status Indicator with Poke Verification

## Problem

Claurst has a `TodoWrite` tool (`src-rust/crates/tools/src/todo_write.rs`) that
persists todos to `~/.claurst/todos/<session_id>.json`, and a `GoalCompleteTool`
for goal completion audits. However:

1. No **live status indicator** in the TUI showing todo progress (pending /
   in-progress / completed counts)
2. No **auto-poke mechanism** to evaluate whether work is done — the model just
   stops and the user has to manually continue
3. No **inline todo card** in the chat transcript

Jcode solves this comprehensively. This feature ports jcode's approach to claurst.

## Jcode Reference (researched from `~/Documents/projects/jcode`)

### Todo data structure (`crates/jcode-task-types/src/lib.rs:198`)
```rust
pub struct TodoItem {
    pub content: String,
    pub status: String,       // "pending" | "in_progress" | "completed"
    pub priority: String,     // "high" | "medium" | "low"
    pub id: String,
    pub group: Option<String>,
    pub confidence: Option<u8>,           // 0-100 forward-looking confidence
    pub completion_confidence: Option<u8>, // confidence when marked done
    pub confidence_history: Vec<u8>,     // every distinct confidence value
    pub blocked_by: Vec<String>,
    pub assigned_to: Option<String>,
}
```

### Auto-poke mechanism (`crates/jcode-base/src/todo.rs:297`)
```rust
pub fn build_auto_poke_message(incomplete_count: usize) -> String {
    format!(
        "You have {} incomplete todo{}. Continue working, or update the todo tool.",
        incomplete_count,
        if incomplete_count == 1 { "" } else { "s" },
    )
}
```

`is_auto_poke_message()` identifies synthetic continuation prompts. When the model
stops with incomplete todos, a synthetic user message is queued. The model treats
it as a normal continuation turn. The live UI hides it (shows "Auto-poking..."
notice instead).

### Overnight auto-poke (`crates/jcode-tui/src/tui/app/commands_overnight.rs`)
- `OVERNIGHT_MAX_POKES: u16 = 48` — safety budget
- `schedule_overnight_poke_followup_if_needed()` — checks poke budget, schedules
  next continuation
- Stops after: cancellation, budget exhaustion, no-progress turns, or non-retryable
  errors
- `stop_overnight_auto_poke_for_non_retryable_error()` — halts on fatal errors

### Todo card UI (`crates/jcode-tui/src/tui/app/todos_view.rs`)
- `toggle_todo_card()` — shows/hides inline card in chat transcript
- `show_todo_card()` — pushes a display message with `role: "todos"`
- `refresh_todo_card_if_needed()` — live-refreshes when todo list changes
  (hash-based diff to avoid unnecessary re-renders)
- `todo_card_rendered_hash` — tracks last rendered state

### Status indicator (`crates/jcode-tui/src/tui/info_widget_todos.rs`)
- Progress pips: `○` pending, `▶` in-progress (amber), `✓` completed, `✗` cancelled
- `EXACT_PIP_FLOOR: usize = 12` — below this count, render 1:1 pip per todo
- Confidence weighting: `todo_confidence_weight(priority)` — high=3, medium=2, low=1
- `todo_display_confidence()` — shows confidence score

## Design

### 1. Enhanced TodoItem struct

Extend claurst's existing `TodoWrite` tool input schema. **Current claurst
`TodoItem`** (verified at `todo_write.rs:134-139`):
```rust
struct TodoItem {
    id: String,
    content: String,
    status: TodoStatus,
    #[serde(default)]
    priority: Option<String>,  // already exists!
}
```

Current claurst already has `id`, `content`, `status`, `priority`. It does NOT
have `activeForm`. New fields to add: `group`, `confidence`,
`completion_confidence`, `confidence_history`, `blocked_by`, `assigned_to`.

```jsonc
{
  "todos": [
    {
      "id": "unique-id",
      "content": "Implement feature X",
      "status": "in_progress",
      "priority": "high",
      "group": "Phase 1",
      "confidence": 75,
      "blocked_by": ["other-todo-id"]
    }
  ]
}
```

```rust
#[derive(Debug, Clone, Deserialize)]
struct TodoItem {
    id: String,                              // existing
    content: String,                         // existing
    status: TodoStatus,                       // existing
    #[serde(default)]
    priority: Option<String>,                // existing
    #[serde(default)]
    group: Option<String>,                  // NEW
    #[serde(default)]
    confidence: Option<u8>,                  // NEW (0-100)
    #[serde(default)]
    completion_confidence: Option<u8>,       // NEW
    #[serde(default)]
    confidence_history: Vec<u8>,            // NEW, capped at 20 entries
    #[serde(default)]
    blocked_by: Vec<String>,                // NEW
    #[serde(default)]
    assigned_to: Option<String>,             // NEW
}
```

`confidence_history` is capped at **20 entries** (jcode doesn't cap, but long
sessions could grow it unboundedly). When the cap is reached, the oldest entry
is evicted (ring buffer semantics).

### 2. Auto-poke mechanism (port from jcode)

**`src-rust/crates/tools/src/todo_write.rs`** or new `src-rust/crates/core/src/todo.rs`:
```rust
pub fn build_auto_poke_message(incomplete_count: usize) -> String {
    format!(
        "You have {} incomplete todo{}. Continue working, or update the todo tool.",
        incomplete_count,
        if incomplete_count == 1 { "" } else { "s" },
    )
}

pub fn is_auto_poke_message(message: &str) -> bool { /* ... */ }
```

**`src-rust/crates/tui/src/app.rs`**: After model stops, check if todos have
incomplete items. If so, queue `build_auto_poke_message(count)` as a synthetic
user message and re-submit. Show "Auto-poking..." status notice instead of the
raw message.

**Auto-poke is ON by default.** This is the core value proposition of the
feature — users who don't want it can disable via `/todos auto-poke off` or
settings `"autoPokeEnabled": false`. When `/goal` is active, auto-poke is
always ON (cannot be disabled — it's how the goal system continues work).

Safety: max pokes per session (configurable, default 48 like jcode). Stop on:
- Cancellation
- Budget exhaustion (48 pokes)
- **3 consecutive no-progress turns** (todo hash unchanged across 3 turns —
  model is stuck repeating itself without advancing any todo)
- Non-retryable errors (auth failures, context overflow, rate limit)

No-progress detection: compute a hash of `[(id, status)]` sorted by id after
each model turn. If the hash is unchanged for 3 consecutive turns, stop
auto-poking and notify the user: "Auto-poke stopped: no progress for 3 turns."

### 3. Inline todo card (port from jcode)

**`src-rust/crates/tui/src/app.rs`**:
- `toggle_todo_card()` — `/todos` command toggles card visibility
- Card renders as a `DisplayMessage` with `SystemMessageStyle::TodoCard` (new
  variant — claurst's `Message` enum at `app.rs:793` has no `"todos"` role, so
  a new `SystemMessageStyle` variant is the least-invasive approach)
- Shows: status pips (○▶✓✗), confidence, group headers, blocked_by markers
- `refresh_todo_card_if_needed()` — hash-based live update

### 4. Status indicator in status bar

**`src-rust/crates/tui/src/render.rs`**: Add todo progress indicator to the
status bar:
```
Todos: ▶ 2 ○ 3 ✓ 5  (67%)
```
- `▶` in-progress count (amber)
- `○` pending count
- `✓` completed count
- Percentage = completed / total

### 5. Poke verification (jcode pattern)

When model calls `GoalCompleteTool` or stops with all todos completed:
- **Verify**: check that all todos are actually `completed` (not just claimed)
- **Poke**: if any todo is incomplete, inject auto-poke message and continue
- **Audit**: `GoalCompleteTool` already requires `audit_summary` + `evidence`
  — extend it to also verify todo state

**Transition relaxation for verification**: claurst's `validate_transition()`
(`todo_write.rs:191`) currently forbids `completed → pending` and
`in_progress → pending`. This is correct for normal model-driven updates.
However, poke verification may need to **reopen** a falsely-completed todo if
the model claims done but evidence doesn't match. Solution: add a new
`TodoWrite` input field `force_reopen: bool` (default `false`). When `true`,
the transition check is skipped. The model is instructed to use this only when
poke verification finds a false completion. The `force_reopen` field is NOT
exposed in the normal tool description — it's mentioned only in the
auto-poke prompt to prevent casual use.

## Files to modify

| File | Change |
|------|--------|
| `src-rust/crates/tools/src/todo_write.rs` | Enhanced `TodoItem` schema, persistence |
| `src-rust/crates/core/src/lib.rs` or new `todo.rs` | `build_auto_poke_message`, `is_auto_poke_message` |
| `src-rust/crates/tui/src/app.rs` | Auto-poke scheduling, todo card, status bar hook |
| `src-rust/crates/tui/src/render.rs` | Todo progress indicator in status bar |
| `src-rust/crates/tui/src/messages/mod.rs` | Todo card rendering (`render_bash_input_line` pattern) |
| `src-rust/crates/commands/src/lib.rs` | `/todos` command (toggle card) |
| `src-rust/crates/tools/src/goal_complete.rs` | Todo verification before accepting completion |
| `src-rust/docs/advanced.md` | Document auto-poke + todo status |

## Jcode files referenced

| File | Purpose |
|------|---------|
| `crates/jcode-base/src/todo.rs` | `build_auto_poke_message`, `is_auto_poke_message`, todo logic |
| `crates/jcode-task-types/src/lib.rs` | `TodoItem` struct, `TodoPlan` |
| `crates/jcode-tui/src/tui/app/todos_view.rs` | Todo card rendering, `toggle_todo_card` |
| `crates/jcode-tui/src/tui/info_widget_todos.rs` | Progress pips, confidence weighting |
| `crates/jcode-tui/src/tui/app/commands_overnight.rs` | Overnight auto-poke state machine |
| `crates/jcode-tui/src/tui/app/commands.rs` | `is_poke_message`, `queued_messages_are_only_pokes` |
| `crates/jcode-tui/src/tui/app/remote.rs` | `schedule_auto_poke_followup_if_needed` |

## Compatibility

- Existing `TodoWrite` calls with old schema (id/content/status/priority only) →
  still work, new fields default to `None`/empty
- Auto-poke **ON by default**; disable via `/todos auto-poke off` or settings
  `"autoPokeEnabled": false`. During `/goal` sessions, auto-poke is always ON.
- No breaking changes to session transcript format

## Testing strategy

- **Unit test**: `build_auto_poke_message()` produces correct text for 0, 1, N
  todos
- **Unit test**: `is_auto_poke_message()` true for poke messages, false for
  real user input
- **Unit test**: `TodoItem` deserialization with old schema (no new fields) →
  new fields are `None`/empty
- **Unit test**: `TodoItem` with `force_reopen: true` allows `completed → pending`
- **Unit test**: `confidence_history` cap at 20 entries — 21st insert evicts oldest
- **Unit test**: no-progress hash — 3 identical hashes → stop signal
- **Integration test**: model stops with 2 incomplete todos → auto-poke message
  queued, "Auto-poking..." status shown, not raw message
- **Integration test**: model completes all todos → `GoalCompleteTool` verifies
  todo state, accepts completion
- **Integration test**: model marks todo completed but evidence empty → poke
  reopens todo via `force_reopen`, continues
- **Integration test**: 3 no-progress turns → auto-poke stops, user notified
- **Error paths**: todos file read fails (corrupt JSON) → empty list, no crash;
  auto-poke budget exhausted → "Auto-poke stopped: budget of 48 reached"

## Error handling

- Todo file read error (corrupt JSON, permissions) → empty list returned, error
  logged to `~/.claurst/logs/`, auto-poke continues with 0 todos (no-op)
- Auto-poke scheduling fails (query engine error) → user notified, auto-poke
  disabled for rest of session
- `force_reopen` on a non-completed todo → no-op (transition check skipped but
  status already not `completed`, so no state change)