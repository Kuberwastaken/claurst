# Feature: Cursor CLI ACP Support

## Problem

Claurst has an ACP **server** (`src-rust/crates/acp/`) — editors like Zed connect
to claurst as a subprocess. But claurst has no ACP **client** mode — it cannot
connect to another ACP agent (like Cursor's `agent acp` CLI) as a provider.

The user's opencode config had a `cursor-acp` provider pointing at
`http://127.0.0.1:32124/v1`. Claurst cannot use Cursor's models because:
1. Cursor CLI uses ACP (JSON-RPC 2.0 over stdio), not a REST `/v1/chat/completions`
   endpoint
2. Claurst only has OpenAI-compatible REST providers + native providers (Anthropic,
   Google, etc.) — no ACP-client provider

## Jcode Reference (the working implementation)

Jcode has a full Cursor ACP runtime: `crates/jcode-provider-cursor-acp-runtime/src/lib.rs`
(1940 lines). Key patterns:

### 1. Subprocess management
- Spawns `agent --force --trust acp` as a subprocess (Cursor CLI)
- `DEFAULT_COMMAND = "agent"`, `DEFAULT_ACP_ARG = "acp"`
- `DEFAULT_PERMISSION_ARGS = ["--force", "--trust"]` (before `acp` subcommand)
- Env overrides: `JCODE_CURSOR_ACP_PATH`, `JCODE_CURSOR_ACP_ARGS`,
  `JCODE_CURSOR_ACP_EXTRA_ARGS`
- `ACP_HANDSHAKE_TIMEOUT = 30s`, `ACP_READ_TIMEOUT = 120s` per line

### 2. Protocol implementation
- Newline-delimited JSON-RPC 2.0 over stdin/stdout
- `IncomingMessage` struct: parse `{id, method, params, result, error}`
- `AcpProcess` struct: owns `Child`, `ChildStdin`, `BufReader<ChildStdout>`
- Stderr captured to bounded tail (`STDERR_TAIL_BYTES = 8192`) for debugging

### 3. Model catalog discovery
- `ModelCatalog` struct: `models: Vec<String>`, `current: Option<String>`
- Parses `availableModels`, `currentModelId`, `configOptions` from initialize response
- `resolve_model()`: exact ID match, or bare ID when exactly one bracketed variant
  exists
- Process-wide `SHARED_DISCOVERED_MODELS: OnceLock<Arc<RwLock<Vec<String>>>>` —
  one prefetch populates all instances

### 4. Streaming
- `session/prompt` → streams `session/update` notifications
- Maps ACP events to jcode's `StreamEvent` enum
- `ACP_READ_TIMEOUT = 120s` per line (long-running tools OK, hung process detected)

## Design

### 1. New module in `claurst_api`

New file: `src-rust/crates/api/src/providers/cursor_acp.rs`

```rust
pub struct CursorAcpProvider {
    // tokio::sync::Mutex — NOT std::sync::Mutex — because create_message_stream()
    // is async and holds the lock across .await points. A std::sync::Mutex would
    // block the async runtime. LlmProvider requires Send + Sync; tokio::sync::Mutex
    // satisfies both for async contexts.
    process: tokio::sync::Mutex<Option<AcpProcess>>,
    model: tokio::sync::RwLock<String>,
    command: CursorAcpCommand,
}

struct AcpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_tail: Arc<RwLock<String>>,
    next_id: u64,
    session_id: String,
    catalog: ModelCatalog,
    supports_images: bool,
}

impl Drop for AcpProcess {
    fn drop(&mut self) {
        // Kill the Cursor subprocess to prevent orphaned processes.
        // Jcode's AcpProcess does the same (verified in jcode source).
        let _ = self.child.start_kill();
    }
}
```

Implements `LlmProvider` trait (verified at `provider.rs:75`) — `create_message_stream()`
is the streaming entry point. The `process` field uses `Option<AcpProcess>` so the
mutex can be held with `None` during reconnection (see reconnection strategy below).

### 2. Provider registration

In `src-rust/crates/api/src/providers/openai_compat_providers.rs` or a new
`providers/mod.rs` entry:

```rust
// In provider_from_key() in registry.rs:
"cursor-acp" => Some(Arc::new(CursorAcpProvider::new(key))),
```

In `src-rust/crates/api/src/registry.rs` `runtime_provider_for()`:
```rust
"cursor-acp" => return Some(Arc::new(CursorAcpProvider::from_env())),
```

### 3. Settings

```jsonc
{
  "provider": "cursor-acp",
  "providers": {
    "cursor-acp": {
      "api_base": null,
      "api_key": null,
      "enabled": true,
      "options": {
        "command": "agent",
        "args": ["--force", "--trust", "acp"],
        "models": []
      }
    }
  }
}
```

Env vars (parallel to jcode):
- `CLAURST_CURSOR_ACP_PATH` — executable (default: `agent`)
- `CLAURST_CURSOR_ACP_ARGS` — full arg list (default: `--force --trust acp`)
- `CLAURST_CURSOR_ACP_EXTRA_ARGS` — permission args before `acp` (default:
  `--force --trust`)

### 4. ACP client protocol flow

```
1. Spawn: agent --force --trust acp (subprocess)
2. initialize → get capabilities + model catalog
3. session/new (cwd = current working dir)
4. session/prompt → stream session/update events
   - Map text deltas → claurst StreamEvent::Text
   - Map tool calls → claurst StreamEvent::ToolCall
   - Map tool results → claurst StreamEvent::ToolResult
   - Map thinking → claurst StreamEvent::Thinking
5. session/cancel on user interrupt
```

**Connection reuse**: claurst's `Connection` struct (`acp/src/connection.rs:47`) is
already bidirectional — it has `send_request()` (outbound request + pending response
routing via `DashMap<String, oneshot::Sender<...>>`) and `run_reader()` (inbound
parsing → `Inbound::Request` / `Inbound::Notification`). As a **client**, claurst
sends `initialize`/`session/new`/`session/prompt` requests (outbound), and receives
responses (routed internally to pending futures) + `session/update` notifications
(inbound). The `Connection` handles both paths correctly.

**Lifecycle caveat**: `Connection::new(writer)` creates the writer side. `run_reader()`
must be spawned as a separate tokio task to pump inbound messages. The reader task
writes parsed inbound to an `mpsc` channel. The client must consume this channel
concurrently with `send_request()` calls. The `AcpProcess` owns both the writer
(via `Connection`) and the reader task handle. On drop, both are cleaned up:
subprocess killed, reader task aborted.

### 5. Model catalog

On `initialize`, parse the Cursor response to extract available models. Register
them in `ModelRegistry` as `cursor-acp/<model_id>`. Use jcode's `ModelCatalog`
parsing logic as reference:
- `availableModels` array
- `currentModelId` string
- `configOptions` with `category: "model"`

Claurst's `ModelRegistry` is already a singleton — custom provider models are
registered directly into it (no need for jcode's process-wide `OnceLock` cache).

### 6. Permission flow

Cursor ACP sends `session/request_permission` for tool calls. Claurst's existing
permission system (`src-rust/crates/tui/src/dialogs.rs` `PermissionRequest`) handles
this:
- `default` mode: show permission dialog
- `acceptEdits`/`bypassPermissions`: auto-approve
- Map ACP permission request → claurst `PermissionRequest`

### 7. Reconnection strategy

When the Cursor subprocess crashes mid-session (exit code non-zero, stdin EOF,
or `ACP_READ_TIMEOUT` exceeded):

1. **Detect**: `run_reader()` returns EOF or error. The current `create_message_stream()`
   call returns `ProviderError` to the caller.
2. **Propagate error**: user sees "Cursor ACP subprocess exited (code N)" with
   the stderr tail (last 8KB) for debugging.
3. **Lazy reconnection**: the next `create_message_stream()` call checks if
   `process` is `None` (dropped after crash). If so, it re-spawns the subprocess,
   runs `initialize` + `session/new`, and retries. This is lazy — no background
   reconnection loop, just re-on-demand.
4. **Session loss**: a new `session/new` means the conversation context is lost
   (Cursor's session is per-subprocess). The provider logs a warning: "Cursor ACP
   session lost — starting fresh session." The claurst conversation history is
   re-sent on the next `session/prompt` (claurst already sends full context).

This mirrors jcode's behavior: error → propagate → re-on-demand, not auto-restart.

## Files to modify/create

| File | Change |
|------|--------|
| `src-rust/crates/api/src/providers/cursor_acp.rs` | **NEW** — `CursorAcpProvider`, `AcpProcess`, `ModelCatalog` |
| `src-rust/crates/api/src/providers/mod.rs` | Add `cursor_acp` module |
| `src-rust/crates/api/src/registry.rs` | Register `cursor-acp` provider id |
| `src-rust/crates/core/src/lib.rs` | Cursor ACP env var resolution |
| `src-rust/crates/acp/src/connection.rs` | Reuse as client (already bidirectional) |
| `src-rust/crates/api/src/model_registry.rs` | Register cursor ACP models |
| `src-rust/docs/providers.md` | Document `cursor-acp` provider |

## Jcode files referenced

| File | Purpose |
|------|---------|
| `crates/jcode-provider-cursor-acp-runtime/src/lib.rs` | Full ACP client implementation (1940 lines) |
| `crates/jcode-provider-cursor-runtime/src/agent_transport.rs` | Alternative Cursor transport (572 lines) |

## Key insights from jcode

1. **`--force --trust` are top-level flags**, not `acp` subcommand flags — they
   must come before `acp` on the command line
2. **Model IDs are opaque** — don't try to parse them, just pass through
3. **Process-wide model cache** — `OnceLock<Arc<RwLock<Vec<String>>>>` so one
   prefetch populates all provider instances
4. **Per-line read timeout** (120s) not per-response — long-running tools don't
   trip it, only hung processes do
5. **stderr tail** — bounded 8KB ring buffer for debugging spawn failures

## Compatibility

- Existing ACP server (`claurst acp`) unaffected — this is a client, not server
- Cursor CLI must be installed (`agent` on PATH) — `CursorAcpCommand::configured()`
  gates availability
- Works alongside other providers — `cursor-acp` is just another provider id

## Testing strategy

- **Unit test**: `CursorAcpCommand::from_env()` parses env vars correctly, falls
  back to defaults
- **Unit test**: `CursorAcpCommand::configured()` returns false when `agent` not
  on PATH, true when it is
- **Unit test**: `resolve_model()` — exact ID match wins, bare ID resolves when
  one bracketed variant exists, ambiguous → error
- **Unit test**: `ModelCatalog::merge()` parses `availableModels` +
  `currentModelId` + `configOptions`
- **Integration test** (requires mock ACP server): spawn mock → initialize →
  session/new → session/prompt → verify `StreamEvent::Text` deltas received
- **Integration test**: mock server sends `session/request_permission` →
  permission dialog shown in `default` mode
- **Integration test**: mock server crashes mid-stream → error propagated,
  next call re-spawns and reconnects
- **Integration test**: Drop of `CursorAcpProvider` → subprocess killed
  (verify no orphaned `agent` process)
- **Error paths**: `agent` not found → `ProviderStatus::Unavailable`;
  `initialize` timeout (30s) → error with stderr tail; `session/prompt` read
  timeout (120s) → error, subprocess not killed (tool may be running)

## Error handling

- Subprocess spawn fails → `ProviderError::Connection("failed to spawn agent:
  {error}")` with stderr tail if available
- `initialize` handshake timeout (30s) → `ProviderError::Connection("handshake
  timeout — Cursor CLI may be unresponsive")`
- `session/prompt` read timeout (120s per line) → `ProviderError::Connection
  ("read timeout — Cursor CLI stopped responding")`, subprocess kept alive
  (a tool may still be running; killing mid-tool is worse than waiting)
- Subprocess exits non-zero → `ProviderError::Connection("Cursor CLI exited
  (code N): {stderr_tail}")`, process set to `None` for lazy reconnection
- Invalid JSON-RPC line from subprocess → logged, skipped (robustness — one
  bad line shouldn't kill the session)