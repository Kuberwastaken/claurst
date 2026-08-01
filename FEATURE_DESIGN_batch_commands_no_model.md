# Feature: Bang (`!`) Batch Commands Independent of Model

## Problem

Claurst currently has NO `!` batch command feature. Users cannot run shell
commands directly from the input prompt without going through the model (which
consumes tokens, takes time, and adds the command to the conversation context).

The user wants: typing `! <command>` in the prompt should execute the command
directly via the `Bash` tool, WITHOUT sending anything to the model. The output
should be displayed inline. Zero token consumption.

## Current State

- `src-rust/crates/tui/src/app.rs` (~line 6730): `take_input()` returns the raw
  input string. If it's a slash command (`is_slash_command()`), it's intercepted.
  Otherwise, the input goes to `claurst_query` → model turn.
- `src-rust/crates/tui/src/input.rs` (`is_slash_command`): returns true for
  `/`-prefixed input. There's no `!`-prefix handling.
- `src-rust/crates/tui/src/app.rs` (~line 5396): `bash_command_allowed_by_prefix()`
  handles the bash prefix allowlist for the `Bash` tool permission system —
  this is about tool permission, not direct execution.
- `src-rust/crates/tools/src/` has `Bash` tool, but it's invoked by the model
  during a turn, not directly by user input.

## Design

### 1. Intercept `!` prefix in input handler

In `src-rust/crates/tui/src/app.rs` (~line 6720, the Enter/submit handler):

```rust
let input = self.take_input();
if input.starts_with('!') {
    let command = input[1..].trim();
    if !command.is_empty() {
        return Ok(Some(format!("__BANG__{}", command)));
    }
    continue;
}
```

Or better: intercept before `take_input()` returns to the query path, similar
to how slash commands are intercepted:

```rust
// Check for bang command BEFORE sending to model
if self.prompt_input.text.starts_with('!') {
    let command = self.prompt_input.text[1..].trim().to_string();
    if !command.is_empty() {
        self.clear_prompt();
        self.execute_bang_command(command).await;
        continue;
    }
}
```

### 2. `execute_bang_command()` method

The bash tool in claurst is `PtyBashTool` (`tools/src/lib.rs:569`), a stateful
PTY-based tool. Using it for `!` commands would bypass the PTY state machine and
is unnecessarily complex for a one-shot command. Instead, use
`std::process::Command` (or `tokio::process::Command` for async) directly:

```rust
async fn execute_bang_command(&mut self, command: String) {
    // Display the command as a user-style message in the transcript
    // (display_messages only — NOT self.messages, so model never sees it)
    let display_msg = DisplayMessage::bang_command(format!("$ {}", command));
    self.display_messages.push(display_msg);

    // Execute via tokio::process::Command — NO model round-trip, NO PTY
    let result = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&command)
        .current_dir(&self.working_dir)
        .output()
        .await;

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let display = if stderr.is_empty() {
                stdout.to_string()
            } else {
                format!("{}\n--- stderr ---\n{}", stdout, stderr)
            };
            let exit_code = output.status.code().unwrap_or(-1);
            let full = if exit_code != 0 {
                format!("{}\n[exit: {}]", display, exit_code)
            } else {
                display.to_string()
            };
            self.display_messages.push(DisplayMessage::bang_output(full));
        }
        Err(e) => {
            self.display_messages.push(DisplayMessage::bang_error(
                format!("Error: {}", e),
            ));
        }
    }

    // NO token consumption — no query, no model call
    // NO context pollution — command + output in display_messages only
}
```

New `DisplayMessage` variants: `bang_command`, `bang_output`, `bang_error` —
rendered via the existing `render_bash_input_line()` pattern. These are
display-only and never enter the conversation sent to the model.

### 3. Permission handling

The `!` command still goes through the normal `Bash` permission system:
- In `default` mode: prompt the user before execution (existing permission dialog)
- In `acceptEdits` / `bypassPermissions` mode: auto-approve
- The `bash_prefix_allowlist` applies — previously-allowed prefixes skip the prompt
- **In `plan` mode: `!` commands are BLOCKED**. Plan mode restricts to read-only
  operations (verified: `PermissionMode::Plan` in `lib.rs`). Shell execution is
  not read-only. The `!` prefix should show: "Bang commands disabled in plan mode."
  and not execute. This prevents accidental writes during analysis sessions.

### 4. Multi-line / batch support

Claurst's input is single-line: Enter submits, Shift+Enter inserts a newline
(verified at `app.rs:7246` `shift_enter_inserts_newline_not_submit`). The `!!`
multi-line design must work within this model:

- `!` followed by a single command → execute directly (one line, Enter submits)
- `!!` (double bang) → enters **multi-line bang mode**: the prompt shows
  `!!>` prefix. Each Enter inserts a newline (does NOT submit). User presses
  `Ctrl+D` or types a blank line then Enter to execute the full multi-line
  script as `bash -c "<script>"`.
- Multi-line mode is exited after execution, back to normal prompt.
- Output of `!` / `!!` commands is NOT sent to the model (truly independent)

### 5. Settings

```jsonc
{
  "bangCommands": {
    "enabled": false,
    "addToHistory": false,
    "showInTranscript": true
  }
}
```

- `enabled`: toggle the feature (default `false` — `!` is a surprising prefix
  for users who don't expect it; opt-in is safer)
- `addToHistory`: add `!` commands to a **separate** shell-command history (not
  the prompt history), recalled via `Alt+Up` / `Alt+Down` (avoids accidentally
  re-submitting a shell command as a prompt)
- `showInTranscript`: display command + output in chat transcript (vs. just
  showing a brief confirmation)

### 6. Rendering

Use `render_bash_input_line()` from `src-rust/crates/tui/src/messages/mod.rs:1280`
(which already exists!) to render the command. Output rendered as a code block
system message.

## Files to modify

| File | Change |
|------|--------|
| `src-rust/crates/tui/src/app.rs` | `!` prefix interception in submit handler, `execute_bang_command()` |
| `src-rust/crates/tui/src/input.rs` | `is_bang_command()` helper (parallel to `is_slash_command()`) |
| `src-rust/crates/tui/src/messages/mod.rs` | New `DisplayMessage` variants for bang output |
| `src-rust/crates/core/src/lib.rs` | `Settings.bang_commands` config field |
| `src-rust/docs/commands.md` | Document `!` command syntax |

## Key insight

The critical design point: `!` commands must NOT enter the `claurst_query`
pipeline at all. They execute locally and display output, but the model never
sees them. This means:
- No `ProviderRequest` is built
- No `provider.create_message_stream()` call
- No tokens consumed
- Command + output not added to `self.messages` (the conversation sent to model)
- Only added to `display_messages` (what the user sees)

## Compatibility

- Existing slash commands (`/...`) unaffected — `!` checked separately
- `PtyBashTool` (model-invoked Bash tool) still works normally during a turn
- `bash_prefix_allowlist` applies to `!` commands too
- `plan` mode blocks `!` commands entirely

## Testing strategy

- **Unit test**: `is_bang_command()` true for `!ls`, `!! script`; false for
  `ls`, `/help`, `!` alone (empty)
- **Unit test**: `execute_bang_command()` runs `echo hello` → stdout "hello\n"
  displayed, exit code 0
- **Unit test**: `execute_bang_command()` runs `false` → stderr shown, exit
  code 1 displayed
- **Unit test**: command not found → error message displayed, no panic
- **Integration test**: `!` command output appears in `display_messages`, NOT
  in `self.messages` (verify model never sees it)
- **Integration test**: `plan` mode → `!` command blocked, message shown
- **Integration test**: `bypassPermissions` mode → `!` auto-executes, no dialog
- **Error paths**: command spawn fails (e.g. `bash` not on PATH) → error
  displayed; command takes >30s → timeout, kill process, show partial output

## Error handling

- Command spawn fails (`bash` not found) → `DisplayMessage::bang_error("Error:
  command not found: bash")` in display_messages
- Command timeout (default 30s, configurable) → kill child process, display
  partial output + "[timeout after 30s]"
- Non-zero exit code → output shown with `[exit: N]` marker, no error dialog
  (it's not a claurst error, just a command result)
- Stderr present → shown after stdout with `--- stderr ---` separator