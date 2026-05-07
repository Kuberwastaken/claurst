# Hooks

Hooks let you run arbitrary shell commands (or HTTP requests, LLM prompts, or agentic verifiers) in response to events that happen inside a Claurst session. They are the primary mechanism for extending and automating Claurst's behavior without modifying the agent itself.

---

## What hooks are

A hook is a piece of executable logic that Claurst calls at a specific lifecycle event. The simplest kind is a shell command. When the event fires, Claurst:

1. Serialises a JSON payload describing the event.
2. Passes that JSON to the hook's stdin.
3. Waits for the hook to exit (unless the hook is marked async).
4. Interprets the exit code according to that event's blocking rules.

Because every hook receives structured JSON and returns a plain exit code, hooks can be written in any language that can read stdin and write to stderr/stdout.

---

## Hook types

Claurst supports four hook implementations:

### `command` — shell command

```json
{
  "type": "command",
  "command": "bash /path/to/my-hook.sh"
}
```

Runs the given string through the configured shell (`bash` by default, or `powershell`).

| Field | Description |
|---|---|
| `command` | Shell command string to execute. |
| `shell` | `"bash"` (default, uses `$SHELL`) or `"powershell"` (uses `pwsh`). |
| `timeout` | Per-hook timeout in seconds. |
| `statusMessage` | Custom spinner text shown while the hook runs. |
| `async` | If `true`, the hook runs in the background without blocking the event. |
| `asyncRewake` | Background hook that wakes the model on exit code 2. Implies `async`. |
| `once` | If `true`, the hook is removed from the session after it fires once. |
| `if` | Condition filter; see [Filtering with `if`](#filtering-with-if). |

### `prompt` — LLM evaluation

```json
{
  "type": "prompt",
  "prompt": "Does this tool call look safe? $ARGUMENTS"
}
```

Sends the event payload to a lightweight model for evaluation. The model must respond with `{"ok": true}` to pass, or `{"ok": false, "reason": "..."}` to fail.

| Field | Description |
|---|---|
| `prompt` | Prompt string. Use `$ARGUMENTS` as a placeholder for the hook's JSON input. |
| `model` | Model ID to use (e.g. `"claude-haiku-4-5"`). Defaults to the fastest available small model. |
| `timeout` | Timeout in seconds. |
| `statusMessage` | Spinner text. |
| `once` | Run once and remove. |
| `if` | Condition filter. |

### `agent` — agentic verifier

```json
{
  "type": "agent",
  "prompt": "Verify that the unit tests passed. Use $ARGUMENTS for context."
}
```

Spawns a short-lived agent session to verify a condition. Like `prompt`, it expects a structured `{"ok": bool, "reason": "..."}` response from the `SyntheticOutput` tool.

| Field | Description |
|---|---|
| `prompt` | Verification description. `$ARGUMENTS` expands to the hook input JSON. |
| `model` | Model ID to use. Defaults to Haiku. |
| `timeout` | Timeout in seconds (default 60). |
| `statusMessage` | Spinner text. |
| `once` | Run once and remove. |
| `if` | Condition filter. |

### `http` — HTTP POST

```json
{
  "type": "http",
  "url": "https://hooks.example.com/claurst",
  "headers": {
    "Authorization": "Bearer $SLACK_TOKEN"
  },
  "allowedEnvVars": ["SLACK_TOKEN"]
}
```

POSTs the event payload JSON to a URL.

| Field | Description |
|---|---|
| `url` | Destination URL. |
| `headers` | Extra request headers. Values may reference env vars using `$VAR` or `${VAR}`. |
| `allowedEnvVars` | Explicit list of env var names that may be interpolated in headers. |
| `timeout` | Timeout in seconds. |
| `statusMessage` | Spinner text. |
| `once` | Run once and remove. |
| `if` | Condition filter. |

---

## Filtering with `if`

The `if` field accepts permission rule syntax to skip a hook when the event does not match, avoiding unnecessary process spawning.

```json
{
  "type": "command",
  "command": "echo 'git command ran'",
  "if": "Bash(git *)"
}
```

The pattern is evaluated against the hook input's `tool_name` and `tool_input` fields. Standard glob wildcards apply. A hook with no `if` fires for every event matching its event type and matcher.

---

## Hook events

Each event has specific semantics for how it uses the hook's exit code and output.

### `PreToolUse`

Fires **before** any tool executes. The matcher field is compared against `tool_name`.

**Payload fields:** `tool_name`, `tool_input`, `tool_use_id`.

**Exit codes:**
- `0` — allow the tool call; stdout/stderr are not shown.
- `2` — **block the tool call**; stderr is shown to the model.
- Other — show stderr to the user only; the tool call proceeds.

### `PostToolUse`

Fires **after** a tool completes successfully. The matcher field is compared against `tool_name`.

**Payload fields:** `tool_name`, `inputs` (tool call arguments), `response` (tool output).

**Exit codes:**
- `0` — success; stdout is shown in transcript mode (`Ctrl+O`).
- `2` — show stderr to the model immediately.
- Other — show stderr to user only.

### `PostToolUseFailure`

Fires **after** a tool errors. The matcher field is compared against `tool_name`.

**Payload fields:** `tool_name`, `tool_input`, `tool_use_id`, `error`, `error_type`, `is_interrupt`, `is_timeout`.

**Exit codes:** Same as `PostToolUse`.

### `Stop`

Fires **right before** the model concludes its response for a turn.

**Exit codes:**
- `0` — allow the stop; no output shown.
- `2` — **continue the conversation**; stderr is shown to the model.
- Other — show stderr to user only.

### `StopFailure`

Fires when the turn ends due to an API error (rate limit, auth failure, etc.) instead of a normal stop. Fire-and-forget — exit codes and output are ignored.

**Payload fields:** `error` (one of `rate_limit`, `authentication_failed`, `billing_error`, `invalid_request`, `server_error`, `max_output_tokens`, `unknown`).

### `UserPromptSubmit`

Fires when the user submits input.

**Payload fields:** the original prompt text.

**Exit codes:**
- `0` — stdout is shown to Claude as additional context.
- `2` — **block processing**, erase the original prompt, show stderr to the user.
- Other — show stderr to user only.

### `Notification`

Fires when Claurst sends a notification. The matcher field is compared against `notification_type`.

**Notification types:** `permission_prompt`, `idle_prompt`, `auth_success`, `elicitation_dialog`, `elicitation_complete`, `elicitation_response`.

**Exit codes:**
- `0` — no output shown.
- Other — show stderr to user only.

### `SessionStart`

Fires when a new session begins. The matcher field is compared against `source`.

**Source values:** `startup`, `resume`, `clear`, `compact`.

**Exit codes:**
- `0` — stdout is shown to Claude.
- Other — show stderr to user only. Blocking errors are ignored.

### `SessionEnd`

Fires when a session is ending. The matcher field is compared against `reason`.

**Reason values:** `clear`, `logout`, `prompt_input_exit`, `other`.

**Exit codes:**
- `0` — success.
- Other — show stderr to user only.

### `SubagentStart`

Fires when an agent tool call starts a subagent. The matcher field is compared against `agent_type`.

**Payload fields:** `agent_id`, `agent_type`.

**Exit codes:**
- `0` — stdout is shown to the subagent.
- Other — show stderr to user only. Blocking errors are ignored.

### `SubagentStop`

Fires right before a subagent concludes its response. The matcher field is compared against `agent_type`.

**Payload fields:** `agent_id`, `agent_type`, `agent_transcript_path`.

**Exit codes:**
- `0` — no output shown.
- `2` — show stderr to the subagent; it continues running.
- Other — show stderr to user only.

### `PreCompact`

Fires before a compaction. The matcher field is compared against `trigger`.

**Trigger values:** `manual`, `auto`.

**Exit codes:**
- `0` — stdout is appended as custom compaction instructions.
- `2` — **block the compaction**.
- Other — show stderr to user only; compaction proceeds.

### `PostCompact`

Fires after compaction completes. The matcher field is compared against `trigger`.

**Payload fields:** compaction details and the generated summary.

**Exit codes:**
- `0` — stdout shown to user.
- Other — show stderr to user only.

### `PermissionRequest`

Fires when a permission dialog is displayed to the user. The matcher field is compared against `tool_name`.

**Payload fields:** `tool_name`, `tool_input`, `tool_use_id`.

**Output:** JSON with `hookSpecificOutput` containing a `decision` field to allow or deny automatically.

**Exit codes:**
- `0` — use the hook's decision if one was provided.
- Other — show stderr to user only.

### `PermissionDenied`

Fires when the auto-mode permission classifier denies a tool call. The matcher field is compared against `tool_name`.

**Payload fields:** `tool_name`, `tool_input`, `tool_use_id`, `reason`.

**Output:** Return `{"hookSpecificOutput": {"hookEventName": "PermissionDenied", "retry": true}}` to tell the model it may retry.

**Exit codes:**
- `0` — stdout shown in transcript mode.
- Other — show stderr to user only.

### `TaskCreated`

Fires when a task is being created. Can block creation.

**Payload fields:** `task_id`, `task_subject`, `task_description`, `teammate_name`, `team_name`.

**Exit codes:**
- `0` — allow creation.
- `2` — show stderr to model; **prevent task creation**.
- Other — show stderr to user only.

### `TaskCompleted`

Fires when a task is being marked complete. Can block completion.

**Payload fields:** `task_id`, `task_subject`, `task_description`, `teammate_name`, `team_name`.

**Exit codes:** Same as `TaskCreated`.

### `Elicitation`

Fires when an MCP server requests user input. The matcher field is compared against `mcp_server_name`.

**Payload fields:** `mcp_server_name`, `message`, `requested_schema`.

**Output:** JSON with `hookSpecificOutput` containing `action` (`accept`/`decline`/`cancel`) and optional `content`.

**Exit codes:**
- `0` — use the hook response if one was provided.
- `2` — deny the elicitation.
- Other — show stderr to user only.

### `ElicitationResult`

Fires after the user responds to an MCP elicitation. Can override the response.

**Payload fields:** `mcp_server_name`, `action`, `content`, `mode`, `elicitation_id`.

**Output:** JSON with `hookSpecificOutput` containing optional `action` and `content` overrides.

**Exit codes:**
- `0` — use the hook response if provided.
- `2` — block the response (action becomes `decline`).
- Other — show stderr to user only.

### `ConfigChange`

Fires when a configuration file changes during a session. The matcher field is compared against `source`.

**Source values:** `user_settings`, `project_settings`, `local_settings`, `policy_settings`, `skills`.

**Payload fields:** `source`, `file_path`.

**Exit codes:**
- `0` — allow the change.
- `2` — **block the change** from being applied to the session.
- Other — show stderr to user only.

### `WorktreeCreate`

Fires when a git worktree is being created.

**Payload fields:** `name` (suggested worktree slug).

**Output:** stdout should contain the absolute path to the created worktree directory.

**Exit codes:**
- `0` — success.
- Other — worktree creation fails.

### `WorktreeRemove`

Fires when a previously created worktree is being removed.

**Payload fields:** `worktree_path` (absolute path).

**Exit codes:**
- `0` — success.
- Other — show stderr to user only.

### `InstructionsLoaded`

Fires when a CLAUDE.md or rules file is loaded (observability-only, cannot block).

**Payload fields:** `file_path`, `memory_type` (`User`/`Project`/`Local`/`Managed`), `load_reason` (`session_start`, `nested_traversal`, `path_glob_match`, `include`, `compact`), and optional `globs`, `trigger_file_path`, `parent_file_path`.

**Exit codes:** `0` success; other — show stderr to user only.

### `CwdChanged`

Fires after the working directory changes.

**Payload fields:** `old_cwd`, `new_cwd`.

The `CLAUDE_ENV_FILE` environment variable is set — write `export KEY=VALUE` lines to that file to propagate environment variables into subsequent Bash tool commands.

**Output:** `hookSpecificOutput.watchPaths` (array of absolute paths) registers paths with the `FileChanged` watcher.

### `FileChanged`

Fires when a watched file changes. The matcher field specifies a filename pattern to watch in the current directory (e.g. `".envrc|.env"`).

**Payload fields:** `file_path`, `event` (`change`, `add`, `unlink`).

`CLAUDE_ENV_FILE` is set for env propagation. `hookSpecificOutput.watchPaths` can dynamically update the watch list.

---

## JSON payload structure

Every hook receives a JSON object on stdin. The exact fields depend on the event, but the envelope is always an object with at minimum a `hook_event_name` field and the event-specific fields described above.

For `PreToolUse`:

```json
{
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": {
    "command": "rm -rf /tmp/foo"
  },
  "tool_use_id": "toolu_01abc..."
}
```

For `PostToolUse`:

```json
{
  "hook_event_name": "PostToolUse",
  "tool_name": "FileWrite",
  "inputs": {
    "file_path": "/src/main.rs",
    "content": "fn main() {}"
  },
  "response": {
    "output": "Written successfully"
  }
}
```

For `SessionEnd`:

```json
{
  "hook_event_name": "SessionEnd",
  "reason": "prompt_input_exit"
}
```

---

## Configuring hooks in settings.json

Hooks are declared under the `"hooks"` key in `.claude/settings.json` (project-shared) or `.claude/settings.local.json` (gitignored, user-local).

The value is a map from event name to an array of matcher objects. Each matcher object contains:

- `matcher` (optional) — a string pattern compared against the event's matchable field (e.g. tool name, trigger type). When absent or empty, the hooks apply to all values.
- `hooks` — an array of hook definitions.

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "python3 ~/.claurst/hooks/check_bash.py"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "FileWrite",
        "hooks": [
          {
            "type": "command",
            "command": "prettier --write \"$TOOL_INPUT_FILE_PATH\" 2>/dev/null || true"
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash ~/.claurst/hooks/notify-slack.sh"
          }
        ]
      }
    ]
  }
}
```

Multiple hook definitions inside a single `hooks` array run in order. Multiple matcher objects for the same event are all evaluated independently.

---

## Configuring hooks in plugin manifests

Plugins can ship their own hooks by declaring them in their manifest. Plugin hooks are registered at plugin load time and are scoped to the plugin's root. They appear in `/hooks` as `pluginHook` source entries.

A plugin `plugin.json` excerpt:

```json
{
  "name": "my-formatter",
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "FileWrite",
        "hooks": [
          {
            "type": "command",
            "command": "node $PLUGIN_ROOT/format.js"
          }
        ]
      }
    ]
  }
}
```

Plugin hooks are subject to the same policy controls as user-defined hooks. If `allowManagedHooksOnly` is set in policy settings, plugin hooks still run; user and project hooks are suppressed.

---

## /hooks command

Run `/hooks` inside an active session to open the interactive hooks configuration menu.

The menu displays all registered hooks grouped by event and matcher, showing the source (user settings, project settings, local settings, plugin, or built-in) for each.

From this menu you can:
- View which hooks are active for each event.
- Add, edit, or remove hooks from editable settings sources.
- Inspect the event metadata for any event (summary, description, matchable fields).

Changes made through `/hooks` are written immediately to the appropriate settings file.

---

## Example hooks

### Log all tool calls to a file

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "jq -c '{ts: now | todate, event: .hook_event_name, tool: .tool_name, input: .tool_input}' >> ~/.claurst/tool.log"
          }
        ]
      }
    ]
  }
}
```

### Block dangerous shell patterns

Create `~/.claurst/hooks/guard.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.tool_input.command // ""')

DANGEROUS_PATTERNS=(
  'rm -rf /'
  'dd if=.*of=/dev/'
  'mkfs\.'
  ':(){:|:&};:'
)

for pattern in "${DANGEROUS_PATTERNS[@]}"; do
  if echo "$CMD" | grep -qP "$pattern"; then
    echo "Blocked: command matches dangerous pattern '$pattern'" >&2
    exit 2
  fi
done
```

Register it:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "bash ~/.claurst/hooks/guard.sh"
          }
        ]
      }
    ]
  }
}
```

An exit code of `2` sends the stderr message directly to the model, which will typically reconsider.

### Send a Slack notification when a session ends

```bash
#!/usr/bin/env bash
# ~/.claurst/hooks/slack-session-end.sh
INPUT=$(cat)
REASON=$(echo "$INPUT" | jq -r '.reason')

curl -s -X POST "$SLACK_WEBHOOK_URL" \
  -H 'Content-Type: application/json' \
  -d "{\"text\": \"Claurst session ended (reason: ${REASON})\"}"
```

```json
{
  "hooks": {
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "http",
            "url": "https://hooks.slack.com/services/YOUR/WEBHOOK/URL"
          }
        ]
      }
    ]
  }
}
```

Or using a shell command with environment variable interpolation:

```json
{
  "hooks": {
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash ~/.claurst/hooks/slack-session-end.sh"
          }
        ]
      }
    ]
  }
}
```

### Auto-format on file write

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "FileWrite",
        "hooks": [
          {
            "type": "command",
            "command": "bash -c 'FILE=$(jq -r .inputs.file_path); case \"$FILE\" in *.ts|*.tsx|*.js|*.jsx|*.json|*.css|*.md) prettier --write \"$FILE\" 2>/dev/null ;; *.py) ruff format \"$FILE\" 2>/dev/null ;; *.rs) rustfmt \"$FILE\" 2>/dev/null ;; esac'"
          }
        ]
      }
    ]
  }
}
```

---

## Testing hooks

The simplest way to test a hook is to print the incoming JSON payload and inspect it:

```bash
#!/usr/bin/env bash
cat > /tmp/last-hook-input.json
```

Register this as a `PreToolUse` hook for the tool you want to observe. After the next tool call, inspect `/tmp/last-hook-input.json` to confirm the payload shape.

To test a blocking hook without a live session, pipe a sample payload directly:

```bash
echo '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /"},"tool_use_id":"test"}' \
  | bash ~/.claurst/hooks/guard.sh
echo "Exit: $?"
```

To test an `http` hook, use a service like `https://webhook.site` as the target URL and inspect the POSTed body in the browser.

For `prompt` and `agent` hooks, enable verbose logging or use `/hooks` to observe whether the hook ran and what it returned during a real session.
