# Feature: Model Favorite Selector

## Problem

Claurst's `/model` picker shows all models from all providers in a flat list. Users
with many providers (especially after the multiple-custom-providers feature) end up
scrolling through a long list to find their preferred models. There's no way to
pin/favorite models for quick access.

## Current State

- `src-rust/crates/api/src/model_registry.rs`: `ModelRegistry` stores models keyed
  by `"provider/model"`. `list_visible_by_provider()` returns models for one
  provider. `best_model_for_provider()` picks a default by priority patterns.
- `src-rust/crates/commands/src/providers.rs` (or `display.rs`): the `/model`
  command opens a picker dialog that lists available models.
- `src-rust/crates/tui/src/dialog_select.rs`: generic selection dialog used by
  model picker.
- `src-rust/crates/core/src/lib.rs`: `Config::model` field persists the selected
  model. No "favorites" or "recent models" concept exists.
- `src-rust/crates/core/src/output_styles.rs`: `/output-style` picker is a
  reference pattern for a similar UI picker.

## Design

### 1. Settings: `favoriteModels` array

```jsonc
{
  "favoriteModels": [
    "custom-openai/together_ai/revolut-ca/glm-5-2",
    "nvidia/z-ai/glm-5.2",
    "openai/gpt-5.5"
  ]
}
```

Persisted in `~/.claurst/settings.json`. Simple array of `"provider/model"` keys.

### 2. TUI: favorites toggle in a dedicated model picker widget

The generic `dialog_select.rs` is used by many pickers (slash commands, output
styles, etc.). Adding favorites-specific key handling and grouped sections to it
would violate SRP — the generic dialog becomes model-picker-specific.

**Solution**: create a new `src-rust/crates/tui/src/model_picker.rs` widget that
owns the model picker rendering and key handling. It wraps the existing
selection logic but adds:
- A "★ Favorites" section at the top of the list
- `f` or `*` key to toggle favorite state on the highlighted model
- Favorite indicator: `★` prefix for favorited, `☆` for non-favorited
- Two grouped sections:
  1. **★ Favorites** — favorited models that exist in the current registry
  2. **All Models** — full list (existing behavior, minus favorited duplicates)

This keeps `dialog_select.rs` unchanged and isolates model-specific logic.

### 3. Toggle mechanism

- Key `f` or `*` in the model picker → calls `toggle_favorite(model_key)`
- Updates `Settings.favoriteModels` array and saves to disk
- Picker re-renders immediately to reflect the change

### 4. Persistence

`favoriteModels: Vec<String>` lives on **`Settings`** (top-level, not nested in
`Config`). This matches `providers` and `modelOverrides` which are also
top-level on `Settings` — they are user preferences, not runtime behavior.
Verified: `theme` and `output_style` are on `Config` (`lib.rs:1021,1023`) because
they are runtime behavior; `favoriteModels` is a preference → `Settings`.

In `src-rust/crates/core/src/lib.rs` `Settings` struct (around line 1208):
```rust
#[serde(default)]
pub favorite_models: Vec<String>,
```

`effective_config()` merge: `Settings.favorite_models` is available directly on
the `Settings` struct — no merge needed since it's not on `Config` (unlike
`model_overrides` which gets merged into `config.model_overrides`). The TUI reads
it via `Settings::load_sync().favorite_models`.

### 5. Model picker rendering

The new `model_picker.rs` widget renders grouped sections. Validation function:

```rust
/// Filter favorites to only those present in the current model registry.
/// Stale favorites (model removed from catalog, provider uninstalled) are
/// silently excluded from the picker display but NOT removed from settings
/// (user may re-install the provider later).
fn valid_favorites(
    favorites: &[String],
    registry: &ModelRegistry,
) -> Vec<String> {
    favorites
        .iter()
        .filter(|key| {
            if let Some((provider, model)) = key.split_once('/') {
                registry.get(provider, model).is_some()
            } else {
                false
            }
        })
        .cloned()
        .collect()
}
```

Reference: `output_styles.rs` `all_styles()` grouping pattern, and jcode's
model picker preview (`state_model_poke_03.rs` shows `inline_interactive_state`
with `selected` index and `preview` mode).

## Files to modify

| File | Change |
|------|--------|
| `src-rust/crates/core/src/lib.rs` | `Settings.favorite_models: Vec<String>` field |
| `src-rust/crates/tui/src/model_picker.rs` | **NEW** — dedicated model picker widget with grouped rendering + `f`/`*` key handler |
| `src-rust/crates/commands/src/providers.rs` | `/model` command wires favorites into picker |
| `src-rust/crates/tui/src/app.rs` | Route model picker key events to new widget |
| `src-rust/docs/configuration.md` | Document `favoriteModels` |

## Reference: jcode pattern

Jcode's model picker (`state_model_poke_03.rs`) uses an `inline_interactive_state`
with `selected` index and `preview` mode. Arrow keys navigate. This is a good
reference for the interactive navigation pattern.

## Compatibility

- Empty `favoriteModels` → picker works exactly as today (no favorites section)
- Favorites referencing non-existent models → silently filtered out at render
  time via `valid_favorites()` (NOT removed from settings — user may re-install
  the provider later)
- Favorites work across all providers, including custom providers

## Testing strategy

- **Unit test**: `valid_favorites()` filters stale entries, keeps valid ones
- **Unit test**: `favorite_models` deserialization (empty array default)
- **Unit test**: `toggle_favorite()` adds/removes from array, saves to disk
- **Integration test**: open model picker with 3 favorites → "★ Favorites"
  section shows 3 items, "All Models" section shows remaining (no duplicates)
- **Integration test**: favorite a model, remove provider, reopen picker →
  stale favorite hidden, no crash
- **Error paths**: settings write fails (read-only filesystem) → error logged,
  in-memory state still toggles correctly