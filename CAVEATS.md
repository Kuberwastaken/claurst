# Caveats & Known Issues

This document catalogs known limitations, technical debt, and areas requiring hardening in the claurst clean-room Rust reimplementation. Organized by severity and category, with a concrete implementation plan at the end.

---

## Critical — Must Fix Before Production Use

### 1. `unwrap()` Density (143 calls)

**Risk**: Panics on malformed input, missing config fields, or unexpected API responses.

**Scope**: 143 `unwrap()` calls across 25 files. While many are defensible (post-validation, test code, compiled regexes), a significant portion guards runtime-fallible operations.

**Top offenders**:
- `cc-commands/src/lib.rs` — 8 occurrences
- `cc-core/src/memdir.rs` — config/path operations
- `cc-core/src/keybindings.rs` — deserialization
- `cc-core/src/team_memory_sync.rs` — file I/O

**Remediation**: Audit each call site. Replace with `?` propagation, `.unwrap_or_default()`, or `expect()` with descriptive messages. Target: reduce to <30 (test code + compile-time constants only). A production CLI must never panic on user-controlled or network-derived input.

### 2. Missing Feature Parity (~25%)

The following features from the original are absent or stubbed:

| Feature | Status | Complexity |
|---------|--------|------------|
| Agent Teams / Swarm (`amber_flint`) | Not implemented | High — tmux/iTerm2 multi-pane, team memory sync |
| KAIROS / Proactive Mode | Not implemented | High — tick loop, daily logs, exclusive tools |
| UltraPlan | Not implemented | High — CCR session, browser UI, teleport |
| LSP Integration | Not implemented | Medium — LSP client protocol |
| Voice Input | Stub only (`start_recording()` returns error) | Medium — audio capture, streaming |
| Keybinding Config UI | Not implemented | Low |

### 3. Missing Tools (7 of 40+)

| Tool | Notes |
|------|-------|
| `REPLTool` | Interactive VM shell for bare mode |
| `LSPTool` | Language Server Protocol communication |
| `WebBrowserTool` | Full browser automation |
| `SnipTool` | History snippet extraction |
| `TeamCreateTool` | Swarm agent team creation |
| `TeamDeleteTool` | Swarm agent team deletion |
| `ListPeersTool` | UDS inbox peer listing |

---

## High — Should Fix Before Maintenance at Scale

### 4. Monolithic `cc-commands/src/lib.rs` (4,120 lines)

**Problem**: The original TypeScript has 87 command files in a directory. The Rust port collapses all commands into a single 4,120-line module with 65 `async_trait` uses.

**Impact**: Merge conflicts, cognitive load, compile-time coupling. Adding or modifying a single command requires reading/recompiling the entire module.

**Remediation**: Split into `cc-commands/src/commands/*.rs` with one file per command (or per logical group). Re-export through `mod.rs`. The `async_trait` contract and dispatch table stay in `lib.rs`.

### 5. Dense TUI Modules

| File | Lines | Responsibility |
|------|-------|---------------|
| `cc-tui/src/app.rs` | 1,099 | Event loop, state structs, history search |
| `cc-tui/src/overlays.rs` | 901 | Modal overlays |
| `cc-tui/src/render.rs` | 837 | All rendering logic |

**Impact**: Fragile under extension. Adding a new overlay or widget requires modifying large, tightly-coupled files.

**Remediation**: Extract state structs to `state.rs`, history search to `history.rs`, and split overlays into individual files per overlay type.

---

## Medium — Technical Debt

### 6. `async_trait` Overhead (131 uses)

**Context**: `async_trait` was necessary before Rust 1.75, but return-position `impl Trait` in traits (RPITIT) is now stable.

**Impact**: Each `async_trait` use introduces a `Box<dyn Future>` heap allocation and prevents monomorphization. In hot paths (tool dispatch, query loop), this adds measurable overhead.

**Remediation**: Migrate to native `async fn` in traits where the trait is not object-safe or doesn't require dynamic dispatch. Estimated reduction: ~80% of uses. Keep `async_trait` only where `dyn Trait` is required (e.g., `Tool` trait used as `Box<dyn Tool>`).

### 7. Buddy PRNG Divergence

**Issue**: The Rust port seeds the Mulberry32 PRNG via SHA-256, while the original TypeScript uses FNV-1a. This means the same `userId` produces a **different buddy** in the Rust vs. TypeScript implementation.

**Impact**: Cosmetic only, but breaks user expectation of determinism across implementations. Documented in code but worth noting for compatibility.

**Remediation**: Implement FNV-1a seeding if cross-implementation buddy consistency is desired.

### 8. Test Coverage — Thin

33 `#[cfg(test)]` modules exist, covering key algorithmic pieces:
- Shell state parsing
- JWT decode
- PRNG determinism
- Compaction thresholds

**Gaps**: No integration tests for the full query loop, no TUI snapshot tests, no end-to-end API client tests with mock servers, no permission system tests.

---

## Low — Awareness Items

### 9. Voice Feature Gate Compiles but Doesn't Function

The `voice` feature in `cc-core` compiles successfully but `start_recording()` returns an error stub. The feature gate exists to reserve the interface; actual audio capture is not implemented.

### 10. No Swarm Feature Gate Implementation

The `amber_flint` feature gate for Agent Teams is referenced but the underlying multi-pane orchestration (tmux/iTerm2 integration, team memory sync, color-coded agent panes) is not implemented.

### 11. Bridge JWT — No Signature Verification

`cc-bridge`'s `JwtClaims::decode()` decodes JWT claims without signature verification. This is correct for client-side use (the client doesn't have the signing key), but should be clearly documented to prevent misuse in server-side contexts.

---

## Architecture Notes (Not Bugs)

These are intentional design decisions worth understanding:

- **`SHELL_STATE_SENTINEL` in `BashTool`**: Parses a terminal state block after command execution to persist `cwd` and environment. Faithful reimplementation of the stateful shell pattern. Correct but unusual — maintainers should understand this before modifying shell handling.

- **`ClaudeError::is_retryable()` / `is_context_limit()`**: Error classification is modeled as methods on the error enum, not as separate error types. This keeps the error hierarchy flat but means retry logic depends on pattern matching within these methods.

- **Workspace-level dependency pinning**: All version pins are in the root `Cargo.toml`. Crate-level `Cargo.toml` files reference `workspace = true`. This is correct and should be maintained.

- **`cc-buddy` as a separate crate**: Feature-gated and isolated from the query loop. Correct boundary — the buddy system has no business coupling to core agent logic.

---
---

# Implementation Plan

A phased plan to address every caveat above. Each phase is independently shippable — later phases do not block earlier ones. Estimated scope is in parentheses.

## Phase 0 — Hardening (Week 1)

The only phase that **must** happen before any production use.

### 0.1 `unwrap()` Elimination Pass

**Goal**: Reduce 143 `unwrap()` calls to <30 (test code + compile-time constants only).

**Approach**: Crate-by-crate, file-by-file audit. Three categories:

| Category | Action | Example |
|----------|--------|---------|
| **Keep** | `unwrap()` in `#[cfg(test)]`, `Regex::new()` on literals, `once_cell` init | `Regex::new(r"...").unwrap()` — compile-time constant, infallible |
| **Replace with `?`** | Any call in an `async fn` or `fn` that returns `Result` | File I/O, JSON parsing, config reads, HTTP responses |
| **Replace with `expect()`** | Invariants that truly cannot fail but aren't in test code | Post-`.insert()` lookups, validated enum conversions |

**Execution order** (by risk, highest first):
1. `cc-core/src/memdir.rs` — config paths, file reads (user-controlled)
2. `cc-core/src/keybindings.rs` — deserialization (user-controlled JSON)
3. `cc-core/src/team_memory_sync.rs` — file I/O (filesystem-dependent)
4. `cc-commands/src/lib.rs` — 8 calls across command implementations
5. `cc-api/src/*.rs` — network response parsing (adversarial input)
6. `cc-bridge/src/*.rs` — JWT decode, long-poll responses
7. `cc-query/src/*.rs` — query loop, compaction
8. `cc-tools/src/*.rs` — tool result parsing
9. Remaining crates

**Validation**: `grep -c 'unwrap()' --include='*.rs' -r src-rust/` must return <30. Each remaining `unwrap()` gets a comment explaining why it's safe.

### 0.2 Add `#[deny(clippy::unwrap_used)]` Lint

After the cleanup pass, add to the workspace `Cargo.toml`:

```toml
[workspace.lints.clippy]
unwrap_used = "deny"
```

And in each crate's `Cargo.toml`:

```toml
[lints]
workspace = true
```

This prevents regressions. Test code gets `#[allow(clippy::unwrap_used)]` at the module level.

---

## Phase 1 — Structural Refactoring (Week 2)

No behavior changes. Pure code reorganization to unlock maintainability.

### 1.1 Split `cc-commands/src/lib.rs` (4,120 → ~40 files)

**Current state**: 40+ command structs + all `impl SlashCommand` blocks + the dispatch table + `CommandContext` + `CommandResult` — all in one file.

**Target structure**:
```
cc-commands/src/
├── lib.rs              (~150 lines: trait, types, registry, re-exports)
├── context.rs          (CommandContext, CommandResult)
└── commands/
    ├── mod.rs          (pub mod declarations + all_commands() registry fn)
    ├── help.rs         (HelpCommand)
    ├── clear.rs        (ClearCommand)
    ├── compact.rs      (CompactCommand)
    ├── cost.rs         (CostCommand)
    ├── model.rs        (ModelCommand)
    ├── config.rs       (ConfigCommand)
    ├── session.rs      (SessionCommand, ResumeCommand, RenameCommand)
    ├── memory.rs       (MemoryCommand)
    ├── doctor.rs       (DoctorCommand)
    ├── auth.rs         (LoginCommand, LogoutCommand)
    ├── mcp.rs          (McpCommand)
    ├── hooks.rs        (HooksCommand)
    ├── plan.rs         (PlanCommand)
    ├── tasks.rs        (TasksCommand)
    ├── diff.rs         (DiffCommand)
    ├── export.rs       (ExportCommand)
    ├── review.rs       (ReviewCommand)
    ├── skills.rs       (SkillsCommand)
    ├── stats.rs        (StatsCommand)
    ├── rewind.rs       (RewindCommand)
    ├── effort.rs       (EffortCommand)
    ├── thinking.rs     (ThinkingCommand)
    ├── plugin.rs       (PluginCommand, ReloadPluginsCommand)
    ├── theme.rs        (ThemeCommand, ColorCommand, OutputStyleCommand)
    ├── keybindings.rs  (KeybindingsCommand)
    ├── privacy.rs      (PrivacySettingsCommand)
    ├── remote.rs       (RemoteControlCommand, RemoteEnvCommand)
    ├── context.rs      (ContextCommand)
    ├── misc.rs         (ExitCommand, VersionCommand, BugCommand, UsageCommand,
    │                     CopyCommand, ChromeCommand, VimCommand, VoiceCommand,
    │                     UpgradeCommand, SummaryCommand, CommitCommand, FilesCommand,
    │                     InitCommand, PermissionsCommand, StatusCommand)
    └── ...
```

**Rules**:
- `lib.rs` keeps only: `SlashCommand` trait, `CommandContext`, `CommandResult`, `all_commands()` registry.
- Each file owns its struct + `impl SlashCommand`.
- Group closely related commands (e.g., auth, theme, session) into one file.
- `mod.rs` has the `pub fn all_commands() -> Vec<Box<dyn SlashCommand>>` dispatch table.

**Validation**: `cargo test -p cc-commands` passes. `wc -l lib.rs` < 200.

### 1.2 Split TUI Modules

**Target**:
```
cc-tui/src/
├── app.rs           (~400 lines: event loop only)
├── state.rs         (~200 lines: AppState, InputState, ScrollState)
├── history.rs       (~200 lines: history search, navigation)
├── render/
│   ├── mod.rs       (re-exports)
│   ├── messages.rs  (message list rendering)
│   ├── input.rs     (input area rendering)
│   ├── status.rs    (status bar rendering)
│   └── tools.rs     (tool result rendering)
├── overlays/
│   ├── mod.rs       (overlay dispatch)
│   ├── help.rs      (help overlay)
│   ├── model.rs     (model picker overlay)
│   ├── confirm.rs   (confirmation dialogs)
│   ├── cost.rs      (cost display overlay)
│   └── search.rs    (search overlay)
└── ...
```

**Validation**: `cargo test -p cc-tui` passes. No file exceeds 500 lines.

---

## Phase 2 — `async_trait` Migration (Week 3)

### 2.1 Identify Traits That Must Stay `async_trait`

Two traits require dynamic dispatch and **must keep `async_trait`**:

| Trait | Location | Reason |
|-------|----------|--------|
| `Tool` | `cc-tools/src/lib.rs` | Used as `Box<dyn Tool>` in the tool registry |
| `PermissionHandler` | `cc-core/src/permissions.rs` | Used as `dyn PermissionHandler` across crate boundaries |

### 2.2 Migrate Everything Else to Native `async fn`

Traits that do NOT need dynamic dispatch — replace `#[async_trait]` with native `async fn in trait`:

| Trait | Location | Uses |
|-------|----------|------|
| `SlashCommand` | `cc-commands/src/lib.rs` | 40+ impls, but dispatched via `Vec<Box<dyn SlashCommand>>` — **check if this needs `dyn`** |
| Internal helper traits | Various in `cc-query`, `cc-mcp` | Used with concrete types only |

**Note**: `SlashCommand` is dispatched via `Box<dyn SlashCommand>` in `all_commands()`. If we want to drop `async_trait` here, we'd need to change the dispatch to an enum. This is a design decision — evaluate whether the 40+ variant enum is worth the ergonomic cost. If not, keep `async_trait` for `SlashCommand`.

**Estimated reduction**: ~90 of 131 uses removed (keep ~40 for `Tool` impls + `SlashCommand` impls + `PermissionHandler`).

**Validation**: `cargo build --workspace` succeeds. `grep -c 'async_trait' -r src-rust/` drops from 131 to ~40.

---

## Phase 3 — Test Coverage (Weeks 3–4, ongoing)

### 3.1 Priority Test Targets

| Priority | Area | Type | What to test |
|----------|------|------|-------------|
| **P0** | Permission system | Unit | `PermissionMode` transitions, path traversal prevention, protected file guards |
| **P0** | API client retry logic | Unit + mock | Retry on 429/500/503, no retry on 400/401, context limit detection |
| **P0** | `unwrap()` elimination validation | Unit | Malformed config, missing fields, empty responses — must not panic |
| **P1** | Query loop | Integration | Single turn: user msg → API call → tool use → response, with mock HTTP |
| **P1** | Auto-compaction | Unit | Threshold triggers, circuit breaker behavior, token counting |
| **P1** | Bridge protocol | Unit | JWT decode (valid, expired, malformed), long-poll timeout, reconnect |
| **P2** | TUI | Snapshot | Render known state → compare terminal output (via `ratatui::TestBackend`) |
| **P2** | Commands | Unit | Each command with valid/invalid args, edge cases |
| **P3** | End-to-end | Integration | Full session: start → multi-turn → compaction → exit |

### 3.2 Test Infrastructure

Add to the workspace:
- `mockito` or `wiremock` for HTTP mocking (API client tests)
- `insta` for snapshot testing (TUI rendering)
- A `tests/` directory in each crate for integration tests
- A shared `test-fixtures/` directory for config files, malformed JSON, etc.

### 3.3 Coverage Gate

Add to CI:
```yaml
- run: cargo tarpaulin --workspace --out xml
- run: # fail if coverage < 40% (initial target, raise over time)
```

---

## Phase 4 — Missing Tools (Weeks 4–6)

Implement in order of standalone complexity (least dependencies first):

| Order | Tool | Depends on | Estimated size |
|-------|------|------------|----------------|
| 1 | `SnipTool` | History access only | ~150 lines |
| 2 | `ListPeersTool` | UDS socket, `SendMessageTool` inbox | ~100 lines |
| 3 | `REPLTool` | `tokio::process`, VM integration | ~300 lines |
| 4 | `LSPTool` | `tower-lsp` or raw JSON-RPC | ~500 lines |
| 5 | `WebBrowserTool` | Headless browser crate (`chromiumoxide`) | ~600 lines |
| 6–7 | `TeamCreateTool`, `TeamDeleteTool` | Swarm system (Phase 5) | Blocked |

Tools 1–3 can ship independently. Tools 4–5 need dependency additions. Tools 6–7 are blocked on Phase 5.

---

## Phase 5 — Missing Features (Weeks 6–12)

These are large features. Each is a standalone project.

### 5.1 LSP Integration (Medium — ~2 weeks)

- Add `tower-lsp` dependency
- Implement LSP client in `cc-tools/src/lsp.rs`
- Wire into `LSPTool` from Phase 4
- Support: `textDocument/definition`, `textDocument/references`, `textDocument/hover`
- Test with `rust-analyzer` and `typescript-language-server`

### 5.2 Agent Teams / Swarm (High — ~3 weeks)

- Implement `amber_flint` feature gate
- `cc-core/src/team.rs`: team state, member registry, color assignment
- `cc-core/src/team_memory_sync.rs`: shared scratchpad via filesystem
- `cc-tools/src/team_create.rs`, `team_delete.rs`: tool implementations
- tmux integration: `tmux split-window`, pane management
- iTerm2 integration: AppleScript-based pane creation (macOS only)
- Test with 2–3 agent team on a real codebase task

### 5.3 KAIROS / Proactive Mode (High — ~3 weeks)

- Tick loop architecture: `tokio::time::interval` with configurable period
- Append-only daily log files in `~/.config/claude/kairos/`
- 15-second blocking budget enforcement
- Exclusive tools: `SendUserFile`, `PushNotification`, `SubscribePR`
- Brief output mode integration with TUI
- Feature gate: `KAIROS` compile-time + `PROACTIVE` runtime

### 5.4 UltraPlan (High — ~2 weeks)

- CCR session spin-up via API
- 3-second polling loop for result
- `__ULTRAPLAN_TELEPORT_LOCAL__` sentinel handling
- Browser UI is out of scope (separate frontend project)

### 5.5 Voice Input (Medium — ~1 week)

- Replace `start_recording()` stub with `cpal` or `rodio` for audio capture
- Streaming audio to Anthropic API
- Feature gate already exists (`voice`)

### 5.6 Keybinding Config UI (Low — ~3 days)

- TUI overlay for viewing/editing keybindings
- Read/write `~/.config/claude/keybindings.json`
- Integrate into the existing overlays system

---

## Phase 6 — Buddy PRNG Compatibility (Optional, ~1 day)

Replace SHA-256 seeding with FNV-1a to match the original TypeScript implementation. Only needed if cross-implementation buddy consistency is a requirement.

```rust
fn fnv1a_hash(input: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for &byte in input {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}
```

---

## Dependency Summary

New crate dependencies required across all phases:

| Dependency | Phase | Purpose |
|------------|-------|---------|
| `wiremock` | 3 | HTTP mock server for API tests |
| `insta` | 3 | Snapshot testing for TUI |
| `cargo-tarpaulin` | 3 | Code coverage |
| `tower-lsp` | 4–5 | LSP client protocol |
| `chromiumoxide` | 4 | Headless browser for `WebBrowserTool` |
| `cpal` | 5 | Audio capture for voice input |
| `fnv` | 6 | FNV-1a hash (or hand-roll, 6 lines) |

---

## Summary Timeline

| Phase | Scope | Duration | Shippable independently? |
|-------|-------|----------|--------------------------|
| **0** | Hardening (`unwrap()` + lint) | 1 week | Yes — **do this first** |
| **1** | Structural refactoring (commands + TUI split) | 1 week | Yes |
| **2** | `async_trait` migration | 1 week | Yes |
| **3** | Test coverage | 2 weeks (ongoing) | Yes |
| **4** | Missing tools (5 of 7) | 2 weeks | Yes (tools 1–5 only) |
| **5** | Missing features (6 items) | 6 weeks | Each sub-item ships independently |
| **6** | PRNG compatibility | 1 day | Yes |

**Total estimated effort**: ~14 weeks for full parity. Phases 0–2 (hardening + structure + async cleanup) in ~3 weeks gets the codebase to a maintainable, production-safe state without adding any new features.
