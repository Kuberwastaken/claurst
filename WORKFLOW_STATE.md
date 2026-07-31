# Workflow State

## Objective
Clone claurst repo, create separate feature branches for 5 features, research
codebase + jcode reference, write design docs per branch.

## Current status
Complete. Repo cloned, 5 branches created, design docs committed per branch.

## Last completed step
All 5 feature branches created with committed design docs. Research of claurst
codebase + jcode reference complete.

## Blockers
None. Design phase complete. Implementation not started (user only requested
branches + design).

## Branches created

| Branch | Design doc | Status |
|--------|-----------|--------|
| `feature/multiple-openai-compatible-providers` | `FEATURE_DESIGN_multiple_openai_compatible_providers.md` | Design committed |
| `feature/model-favorite-selector` | `FEATURE_DESIGN_model_favorite_selector.md` | Design committed |
| `feature/todo-management-status-indicator` | `FEATURE_DESIGN_todo_management_status_indicator.md` | Design committed |
| `feature/batch-commands-no-model` | `FEATURE_DESIGN_batch_commands_no_model.md` | Design committed |
| `feature/cursor-cli-acp-support` | `FEATURE_DESIGN_cursor_cli_acp_support.md` | Design committed |

## Key research findings

### 1. Multiple OpenAI-compatible providers
- Root cause: `provider_for_id()` is hardcoded `match` in
  `openai_compat_providers.rs`; only `"custom-openai"` is generic
- Fix: `customProviders` array in settings + dynamic dispatch before fixed match
- No upstream PR implements this (confirmed via GitHub search)

### 2. Model favorite selector
- No existing favorites concept in claurst
- `ModelRegistry` keyed by `"provider/model"` — favorites = simple array of keys
- Reference: jcode's model picker has `inline_interactive_state` with
  `selected` + `preview` mode

### 3. Todo management + poke verification
- Claurst has `TodoWrite` tool (basic: content/status/activeForm) + `GoalCompleteTool`
- Missing: live status indicator, auto-poke, inline todo card
- Jcode pattern: `build_auto_poke_message()` in `jcode-base/src/todo.rs`,
  `OvernightAutoPokeState` in `commands_overnight.rs`, todo card in
  `todos_view.rs`, progress pips in `info_widget_todos.rs`
- Jcode `TodoItem` has: confidence, completion_confidence, confidence_history,
  blocked_by, group, assigned_to

### 4. Bang (`!`) batch commands
- Claurst has NO `!` feature currently — this is new, not a fix
- Design: intercept `!` prefix before model query, execute via Bash directly,
  zero token consumption, output in display_messages only (not conversation)
- Existing `render_bash_input_line()` in messages/mod.rs can be reused

### 5. Cursor CLI ACP support
- Claurst has ACP **server** only (`crates/acp/`) — no client
- Jcode has full implementation: `jcode-provider-cursor-acp-runtime` (1940 lines)
- Key: spawn `agent --force --trust acp` subprocess, JSON-RPC 2.0 over stdio,
  reuse claurst's `Connection` struct as client
- Cursor models are opaque IDs, catalog discovered from `initialize` response

## Next action
Switch to `main` branch. Implementation of any feature can begin by checking out
the corresponding branch and following the design doc.