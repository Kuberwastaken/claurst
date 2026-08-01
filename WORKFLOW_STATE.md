# Workflow State

## Objective
Implement 5 features in claurst repo. Currently implementing `feature/multiple-openai-compatible-providers`.

## Current Status
- Branch: `feature/multiple-openai-compatible-providers`
- Design doc written + reviewed + fixes applied (commits 1-2)
- Implementation: STARTING

## Last Completed Step
- Read all key source files, understood insertion points
- Design doc finalized

## Implementation Plan (multiple-openai-compatible-providers)

### Step 1: Core structs + Settings (foundation)
- `core/src/lib.rs`: Add `CustomProviderDef`, `CustomModelDef`, `ModelVariant` structs
- `core/src/lib.rs`: Add `custom_providers` field to `Settings`
- `core/src/lib.rs`: Make `save_to_path_sync` use atomic writes (tmp+rename)
- Tests: deserialization, env var substitution, atomic write

### Step 2: Provider routing + registry (integration)
- `api/src/providers/openai_compat_providers.rs`: `provider_for_custom_id()`
- `api/src/registry.rs`: Custom dispatch in `provider_from_key()` + `runtime_provider_for()`
- `api/src/model_registry.rs`: Register custom provider models

### Step 3: Query routing + is_openaiish_provider
- `query/src/lib.rs`: Runtime check for custom ids in `known_providers`
- `query/src/runner/provider_options.rs`: `is_openaiish_provider()` runtime check

### Step 4: /add command + documentation
- `commands/src/providers.rs`: `/add` slash command
- `docs/configuration.md`: Document `customProviders`

## Blockers
- (none)

## Next Action
Delegate Step 1 to implementer subagent