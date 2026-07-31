# Workflow State

## Objective
Clone claurst repo, create separate feature branches for 5 features, research
codebase + jcode reference, write design docs per branch, review design docs,
apply review fixes.

## Current status
Complete. Repo cloned, 5 branches created, design docs committed + reviewed +
review fixes applied. All blocking and important review items resolved.

## Last completed step
Applied review fixes to all 5 design docs. Each branch now has 2 commits:
1. Initial design doc
2. Review fixes applied

## Blockers
None. Design + review phase complete. Implementation not started.

## Branches

| Branch | Commits | Review fixes applied |
|--------|---------|---------------------|
| `feature/multiple-openai-compatible-providers` | 2 | headers on CustomProviderDef (not ProviderConfig), atomic writes for /add, is_openaiish_provider runtime check, HashMap not array, testing + error handling |
| `feature/model-favorite-selector` | 2 | dedicated model_picker.rs widget (not dialog_select), valid_favorites() validation, Settings not Config placement, testing |
| `feature/todo-management-status-indicator` | 2 | corrected schema (has id/content/status/priority, NOT activeForm), auto-poke ON by default, force_reopen for completed→pending, no-progress 3-turn threshold, confidence_history cap at 20, SystemMessageStyle::TodoCard, testing |
| `feature/batch-commands-no-model` | 2 | std::process::Command (not PtyBashTool), !! multi-line via Shift+Enter, plan mode blocks !, separate shell history, default off, testing |
| `feature/cursor-cli-acp-support` | 2 | tokio::sync::Mutex, Drop with start_kill, Connection client-mode lifecycle, lazy reconnection, ModelRegistry singleton (no OnceLock), testing + error handling |

## Review verdicts (post-fix)

All 5 design docs pass review. Blocking items resolved:
- multiple-providers: headers field clarified, atomic writes specified
- model-favorite: separate widget, stale validation specified
- todo-management: factual schema corrected, auto-poke default ON
- batch-commands: std::process::Command, !! multi-line model specified
- cursor-acp: tokio::sync::Mutex, Drop lifecycle, reconnection strategy

## Recommended merge order
1. `feature/multiple-openai-compatible-providers` — other features depend on custom provider ids
2. `feature/model-favorite-selector` — benefits from custom provider models
3. `feature/cursor-cli-acp-support` — independent but benefits from custom provider infrastructure
4. `feature/todo-management-status-indicator` — independent, most complex
5. `feature/batch-commands-no-model` — fully independent, simplest

## Next action
Switch to a feature branch and begin implementation following the design doc.
Recommended start: `feature/multiple-openai-compatible-providers` (foundational).