# Feature: Multiple OpenAI-Compatible Providers

## Problem

Claurst has exactly ONE generic OpenAI-compatible provider slot (`custom-openai`).
Users with multiple custom OpenAI-compatible endpoints (e.g. two Revolut gateways,
a local vLLM + a cloud gateway, a coding endpoint + a non-coding endpoint) cannot
register them simultaneously. The `providers` map only accepts known fixed provider
ids — `custom-openai` is the single catch-all.

## Root Cause

`src-rust/crates/api/src/providers/openai_compat_providers.rs`:
`provider_for_id()` is a hardcoded `match` on provider id strings. Each OpenAI-compat
backend is a distinct fixed id with its own factory fn. Only `"custom-openai"` routes
to the generic adapter, and `custom_openai()` reads `providers["custom-openai"]`
exclusively.

`src-rust/crates/core/src/lib.rs`:
`ProviderConfig` struct has no `models` sub-map (unlike opencode). Model catalog
comes from bundled snapshot + models.dev, not settings.

`src-rust/crates/query/src/lib.rs` (~line 798):
`effective_model.split_once('/')` parses provider prefix. `selected_provider_id()`
derives provider from model string's first `/`.

## Design

### 1. Settings schema: `customProviders` map

Add a new top-level settings key. Uses a **map** (not array) for O(1) lookup
by id and to prevent duplicate ids:

```jsonc
{
  "customProviders": {
    "llmg-coding": {
      "name": "LLMG Coding Gateway",
      "apiBase": "https://llm-gateway-coding.revolut.com/proxy/openai/opencode",
      "apiKey": "{env:LLMG_CODING_API_KEY}",
      "headers": { "x-coding-tool": "opencode" },
      "models": {
        "together_ai/revolut-ca/glm-5-2": {
          "name": "T.AI GLM5.2",
          "contextWindow": 230000,
          "maxOutputTokens": 32072,
          "reasoningEffort": "high",
          "variants": {
            "max": { "reasoningEffort": "max" },
            "high": { "reasoningEffort": "high" },
            "none": { "reasoningEffort": "none" }
          }
        }
      },
      "requestTimeoutSecs": 300
    },
    "llmg-non-coding": {
      "name": "LLMG Non-Coding Gateway",
      "apiBase": "https://llm-gateway-coding.revolut.com/proxy/openai/opencode",
      "apiKey": null,
      "models": {
        "fireworks_revolut-non-coding/accounts/revolut-non-coding/deployments/hxjsp23w": {
          "name": "FW GLM 5.2",
          "contextWindow": 1000000,
          "maxOutputTokens": 131072
        }
      }
    }
  }
}
```

#### Rust structs

`CustomProviderDef` is a **new** struct — NOT `ProviderConfig`. It lives on
`Settings` directly and carries fields that `ProviderConfig` does not have
(`headers`, `models`). The existing `ProviderConfig` struct (`lib.rs:907`) is
unchanged — it only has `api_key`, `api_base`, `enabled`, `models_whitelist`,
`models_blacklist`, `options`, `request_timeout_secs`. The `headers` field is
**not** added to `ProviderConfig` (it's only relevant for custom OpenAI-compat
providers, not for native providers like Anthropic/Google).

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CustomProviderDef {
    pub name: String,
    #[serde(rename = "apiBase", alias = "api_base")]
    pub api_base: String,
    #[serde(rename = "apiKey", alias = "api_key", default)]
    pub api_key: Option<String>,
    /// Custom HTTP headers sent on every request. Only on CustomProviderDef,
    /// NOT on ProviderConfig (native providers don't need this).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Model catalog local to this provider.
    #[serde(default)]
    pub models: HashMap<String, CustomModelDef>,
    #[serde(rename = "requestTimeoutSecs", alias = "request_timeout_secs", default)]
    pub request_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CustomModelDef {
    #[serde(rename = "contextWindow", alias = "context_window", default)]
    pub context_window: Option<u32>,
    #[serde(rename = "maxOutputTokens", alias = "max_output_tokens", default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "reasoningEffort", alias = "reasoning_effort", default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub variants: HashMap<String, ModelVariant>,
}
```

Field naming follows claurst's existing serde convention: snake_case Rust
fields with `#[serde(rename = "camelCase")]` + `#[serde(alias = "snake_case")]`
for JSON compatibility. This matches `ProviderConfig::request_timeout_secs`
(`rename = "requestTimeoutSecs"`), `ModelOverride::context_window`
(`rename = "contextWindow"`), etc.

Each entry is a self-contained provider definition with:
- **Map key**: unique id, becomes the provider id in `provider/model` routing
- `name`: display name in picker
- `apiBase`: OpenAI-compatible base URL (claurst appends `/chat/completions`)
- `apiKey`: API key (`{env:VAR}` substitution supported)
- `headers`: custom HTTP headers (sent on every request) — **only on
  `CustomProviderDef`, not `ProviderConfig`**
- `models`: model catalog local to this provider (primary catalog source)
- `requestTimeoutSecs`: per-provider timeout override

### 2. Provider routing changes

**`openai_compat_providers.rs`**: add `provider_for_custom_id(id: &str) -> Option<OpenAiCompatProvider>`
that looks up `Settings.customProviders` by id (map lookup, O(1)). Called BEFORE
the fixed `match` in `provider_for_id()` so custom ids take priority. Custom
providers use `OpenAiCompatProvider::new(id, name, api_base)` +
`.with_api_key(key)` + `.with_header(name, value)` (builder API already exists
in `openai_compat.rs:173-179`).

**`registry.rs`**: `provider_from_key()` and `runtime_provider_for()` must check
custom providers first, then fall through to the existing fixed-id dispatch.

**`query/src/lib.rs`** (~line 798): the `known_providers` array is a hardcoded
list. Instead of adding custom ids to this array (which is compiled in and
can't know custom ids at compile time), add a **runtime check**:
```rust
if known_providers.contains(&p) || settings.custom_providers.contains_key(p) {
    (p.to_string(), m.to_string())
}
```
This requires `Settings` to be accessible at that call site (it already is via
`tool_ctx.config`).

### 3. Model registry integration

Custom provider models registered into `ModelRegistry` at load time, keyed as
`<custom_provider_id>/<model_id>`. The `modelOverrides` map remains for metadata
correction; `customProviders[].models` is the primary catalog source for these
providers.

### 4. `/add` command

New slash command to add a custom provider interactively:

```
/add <name> <apiBase> [apiKey]
```

Writes to `customProviders` map in `settings.json`.

**Concurrent write safety**: claurst rewrites `settings.json` on startup and
during `/connect`. To prevent races:
1. Load current settings from disk (fresh read, not in-memory cache)
2. Insert the new entry into `customProviders` map
3. Write atomically: serialize to `settings.json.tmp`, then `fs::rename()` to
   `settings.json` (atomic on Unix). This matches `Settings::save_sync()` if it
   already uses atomic writes — check and reuse; if not, add it there.
4. A `parking_lot::Mutex` or file-lock guards concurrent saves across the
   process. A single `save_settings()` entry point serializes all writes.

The TUI custom provider dialog (`custom_provider_dialog.rs`) is extended to
support naming the provider and editing its model list.

### 5. Effort / variants per model

Each model entry in `customProviders[].models` can carry `reasoningEffort` and
`variants` (like opencode). These map to `reasoning_effort` in the request body
via the existing `merge_openai_compatible_options` path in `request_options.rs:34`.

**Blocking fix**: `is_openaiish_provider()` in `query/src/runner/provider_options.rs:62`
is a hardcoded `matches!()` list that does NOT include custom provider ids. The
effort mapping is skipped for unknown providers. Solution: change the check to
also accept custom provider ids:
```rust
pub(crate) fn is_openaiish_provider(provider_id: &str) -> bool {
    // ... existing fixed list ...
    || settings.custom_providers.contains_key(provider_id)
}
```
Alternatively, since all custom providers are OpenAI-compatible by definition,
the simplest fix is to return `true` for any id present in `customProviders`.

## Files to modify

| File | Change |
|------|--------|
| `src-rust/crates/core/src/lib.rs` | Add `CustomProviderDef`, `CustomModelDef` structs; `Settings.customProviders` field |
| `src-rust/crates/api/src/providers/openai_compat_providers.rs` | `provider_for_custom_id()`, headers support |
| `src-rust/crates/api/src/providers/openai_compat.rs` | `with_headers()` from settings (already has builder API) |
| `src-rust/crates/api/src/registry.rs` | Custom provider dispatch in `provider_from_key` + `runtime_provider_for` |
| `src-rust/crates/api/src/model_registry.rs` | Register custom provider models |
| `src-rust/crates/query/src/lib.rs` | Include custom ids in `known_providers` list (~line 798) |
| `src-rust/crates/commands/src/providers.rs` | `/add` command |
| `src-rust/crates/tui/src/custom_provider_dialog.rs` | Extended dialog with name field |
| `src-rust/docs/configuration.md` | Document `customProviders` |

## Compatibility

- Existing `custom-openai` single-slot config continues to work (legacy fallback)
- Existing fixed provider ids (together-ai, nvidia, etc.) unaffected
- `modelOverrides` still works for any provider

## Testing strategy

- **Unit tests**: `CustomProviderDef` deserialization (camelCase + snake_case
  aliases), `{env:VAR}` substitution in `apiKey`, header map serialization
- **Unit tests**: `provider_for_custom_id()` returns `Some` for registered ids,
  `None` for unknown ids
- **Unit tests**: `is_openaiish_provider()` returns `true` for custom ids
- **Integration test**: load settings with 2 custom providers → `ModelRegistry`
  contains entries for both under `<id>/<model>` keys
- **Integration test**: `model = "llmg-coding/together_ai/revolut-ca/glm-5-2"`
  → provider dispatched as `llmg-coding`, model sent as `together_ai/revolut-ca/glm-5-2`
- **Integration test**: `/add` command writes to settings.json atomically —
  verify file not corrupted if process killed mid-write
- **Error paths**: invalid `apiBase` (not a URL) → user-facing error message;
  duplicate `id` in map → serde rejects (map key uniqueness guaranteed);
  missing `apiBase` → validation error on provider construction

## Error handling

- Invalid custom provider config (missing `apiBase`, unparseable URL) → logged
  to stderr, provider skipped (not registered), user warned via status notice
- API key env var not set → provider registered but `health_check()` returns
  `ProviderStatus::Unavailable` (existing pattern from `openai_compat_providers.rs`)
- Custom headers with invalid header names → rejected at provider construction
  time with a descriptive error

## Merge order

This feature should land **first** — `model-favorite-selector` and
`cursor-cli-acp-support` both benefit from custom provider ids existing.