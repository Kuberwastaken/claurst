# Caveats & Known Issues

This document catalogs known limitations, technical debt, and areas requiring hardening in the claurst clean-room Rust reimplementation. Organized by severity and category.

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
