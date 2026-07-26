# Claurst Keybindings Reference

This document covers all keyboard shortcuts in Claurst, how to customize them, vim mode, and special input behaviors.

---

## Table of Contents

1. [Default Keybindings](#default-keybindings)
   - [Global Context](#global-context)
   - [Chat Context](#chat-context)
   - [Confirmation Context](#confirmation-context)
2. [Keybinding Contexts](#keybinding-contexts)
3. [Customizing Keybindings](#customizing-keybindings)
   - [Via /keybindings command](#via-keybindings-command)
   - [Via keybindings.json](#via-keybindingsjson)
   - [Chord Bindings](#chord-bindings)
4. [Non-Rebindable Keys](#non-rebindable-keys)
5. [Vim Mode](#vim-mode)
6. [Special Input Behaviors](#special-input-behaviors)
   - [Shift+Enter for Newline](#shiftenter-for-newline)
   - [ESC During Streaming](#esc-during-streaming)
   - [@file Injection with Typeahead](#file-injection-with-typeahead)
7. [Non-English Keyboard Layout Support](#non-english-keyboard-layout-support)

---

## Default Keybindings

### Global Context

These bindings are active in all contexts.

| Key | Action | Description |
|-----|--------|-------------|
| `Ctrl+C` | interrupt | Interrupt the current operation (non-rebindable) |
| `Ctrl+D` | exit | Exit Claurst (non-rebindable) |
| `Ctrl+L` | redraw | Redraw the terminal screen |
| `Ctrl+R` | historySearch | Open interactive history search |
| `Ctrl+B` | createBranch | Create a new git branch |
| `Alt+H` | openHelp | Open the help panel |

### Chat Context

These bindings are active when focus is in the chat input field.

| Key | Action | Description |
|-----|--------|-------------|
| `Enter` | submit | Submit the current message to the model |
| `Shift+Enter` | newline | Insert a literal newline without submitting |
| `Ctrl+J` | newline | Newline (fallback for terminals without CSI-u protocol) |
| `Up` / `Ctrl+O` | historyPrev | Navigate to the previous message in input history |
| `Down` / `Ctrl+I` | historyNext | Navigate to the next message in input history |
| `Tab` | indent | Insert indentation (or cycle completions if open) |
| `Shift+Tab` | reverseIndent | Remove one level of indentation |
| `Page Up` | scrollUp | Scroll the conversation view up one page |
| `Page Down` | scrollDown | Scroll the conversation view down one page |
| `Home` / `Cmd+Left` / `Ctrl+A` | goLineStart | Move cursor to beginning of line |
| `End` / `Cmd+Right` / `Ctrl+E` | goLineEnd | Move cursor to end of line |
| `Ctrl+Left` | moveWordBackward | Move one word left |
| `Ctrl+Right` | moveWordForward | Move one word right |
| `Alt+Left` | previousMessage | Jump to previous user/assistant message |
| `Alt+Right` | nextMessage | Jump to next user/assistant message |
| `Ctrl+Shift+A` | openModelPicker | Open the interactive model picker |
| `Ctrl+K` | openCommandPalette | Open the slash command palette |
| `Ctrl+U` | killToStart | Delete from cursor to beginning of line |
| `Ctrl+W` / `Alt+Backspace` | killWord | Delete the word before the cursor |
| `Alt+D` | deleteWord | Delete the word after the cursor |
| `Ctrl+H` | deleteCharBefore | Delete character before cursor |
| `Ctrl+L` | clearLine | Clear current input line |
| `Ctrl+F` | findInMessage | Open inline search within the current conversation |
| `Ctrl+Shift+F` | globalSearch | Open global codebase search |
| `F3` / `Ctrl+]` | findNext | Jump to next search match |
| `Shift+F3` / `Ctrl+[` | findPrev | Jump to previous search match |
| `Ctrl+G` | goToLine | Jump to a specific line |
| `Ctrl+.` | jumpToNextError | Jump to next error / issue |
| `Ctrl+Shift+.` | jumpToPreviousError | Jump to previous error / issue |

> `Ctrl+A` previously opened the model picker; it now moves the cursor to the line start (matching Emacs/readline). The model picker is now `Ctrl+Shift+A`. Old `keybindings.json` files are auto-migrated.

### Confirmation Context

These bindings are active when Claurst is displaying a yes/no confirmation prompt (e.g., tool permission requests).

| Key | Action | Description |
|-----|--------|-------------|
| `Y` / `y` | confirm | Approve the pending action |
| `N` / `n` | deny | Deny the pending action |
| `A` / `a` | alwaysAllow | Approve and add a permanent allow rule |
| `Enter` | defaultAction | Accept the highlighted default option |
| `Escape` | cancel | Cancel the prompt and deny the action |

---

## Keybinding Contexts

Claurst uses a context system so that the same key can have different effects depending on where focus is. A binding in a more specific context takes precedence over a binding in a broader context.

| Context | Description |
|---------|-------------|
| `global` | Always active regardless of focus |
| `chat` | Active when the chat input field has focus |
| `confirmation` | Active when a permission confirmation dialog is open |
| `modelPicker` | Active inside the model selection overlay |
| `commandPalette` | Active inside the slash command palette overlay |
| `search` | Active while the inline search bar is open |
| `vim.normal` | Active in vim normal mode (when vim mode is enabled) |
| `vim.insert` | Active in vim insert mode (when vim mode is enabled) |
| `vim.visual` | Active in vim visual mode (when vim mode is enabled) |

---

## Customizing Keybindings

### Via /keybindings command

The `/keybindings` command opens an interactive TUI keybinding editor:

```
/keybindings
```

The editor lists all bindable actions grouped by context. Use arrow keys to navigate, press `Enter` on an action to enter rebind mode, then press the desired key combination. Press `Escape` to cancel a rebind. Changes are saved immediately to `~/.claurst/keybindings.json`.

### Via keybindings.json

For batch edits or scripted configuration, edit `~/.claurst/keybindings.json` directly. The file format is:

```json
{
  "schema_version": 1,
  "bindings": [
    {
      "context": "Chat",
      "action": "submit",
      "chord": "ctrl+enter"
    },
    {
      "context": "Global",
      "action": "historySearch",
      "chord": "ctrl+p"
    }
  ]
}
```

Each binding object has:

| Field | Type | Description |
|-------|------|-------------|
| `context` | string | Keybinding context (see table above) |
| `action` | string \| null | Action identifier (or `null` to unbind) |
| `chord` | string | Key combination in normalized form |

### Schema Versioning and Smart Merge

`keybindings.json` carries a top-level `schema_version` field (currently `1`). When Claurst's defaults change in a release, the file is auto-migrated on next launch:

1. Claurst reads the file and compares `schema_version` against the bundled `KEYBINDINGS_SCHEMA_VERSION`.
2. If the file is older, Claurst runs a **smart merge**:
   - Your customizations (any binding whose `chord` you set explicitly) are preserved.
   - Stale bindings that match an *old* default that has since changed are dropped — for example, the previous `ctrl+a → openModelPicker` binding is removed because `ctrl+a` is now reserved for select-all in the input.
   - Any new bindings present in the current defaults but not in your file are added.
3. The migrated file is written back with the new `schema_version`.

A warning is logged whenever a migration occurs. If you want to opt out of the merge for a binding, leave a different action assigned to it — explicit customizations always win.

Setting `"action": null` for a chord explicitly **unbinds** the default — useful when you want a key to do nothing rather than fire its default action.

Key notation uses lowercase letters, with modifier prefixes separated by `+`:

| Prefix | Modifier key |
|--------|-------------|
| `ctrl+` | Control |
| `alt+` | Alt / Option |
| `shift+` | Shift |
| `super+` | Super / Cmd |

Special key names: `enter`, `escape`, `tab`, `backspace`, `delete`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `f1` through `f12`.

After editing the file, run `/keybindings` and then exit to trigger a reload, or restart Claurst.

### Chord Bindings

Claurst supports chord bindings — multi-key sequences where you press a leader key and then a follow-up key. Chord bindings are defined with a `chord` array instead of a single `key`:

```json
{
  "context": "chat",
  "action": "openModelPicker",
  "chord": ["ctrl+x", "ctrl+m"]
}
```

The first key in the chord acts as the leader. After pressing the leader key, Claurst enters a brief chord-wait state (500 ms by default). If the follow-up key arrives within that window, the chord fires. If the timeout expires or a different key is pressed, the leader key's default action (if any) fires instead.

Chords can be up to two keys deep. Three-key chords are not supported.

Example — map `Ctrl+X Ctrl+C` to exit:

```json
{
  "context": "global",
  "action": "exit",
  "chord": ["ctrl+x", "ctrl+c"]
}
```

---

## Non-Rebindable Keys

The following keys have fixed behavior and cannot be rebound:

| Key | Fixed behavior |
|-----|---------------|
| `Ctrl+C` | Interrupt current operation / send SIGINT to foreground process |
| `Ctrl+D` | Exit Claurst when input is empty; signal EOF when input has content |
| `Ctrl+M` | Identical to `Enter` at the terminal level (terminals emit `CR` for both) |

These keys are handled at the terminal input layer before the keybinding system processes events. If any of them appear as a `chord` in `keybindings.json`, Claurst:

1. Logs a warning at startup (`Cannot rebind protected key '<chord>' in keybindings.json`).
2. **Filters the binding out** of the loaded set before resolving any keystrokes.

So overriding a protected key is a no-op in behavior, but you also get a clear signal in the logs that the binding was rejected.

---

## Vim Mode

Vim mode replaces the default line editor with a modal input field that mimics vim's normal, insert, and visual modes.

### Enabling Vim Mode

```
/vim
/vim on
/vim off
```

Or set it persistently:

```
/config set vim true
```

### Vim Mode Keybindings

In vim mode the input field has three modes:

**Insert mode** — behaves like the normal chat input; type freely, `Escape` returns to normal mode.

**Normal mode** — movement and editing commands:

| Key | Action |
|-----|--------|
| `h` / `l` | Move cursor left / right |
| `j` / `k` | History prev / next |
| `w` / `b` | Move forward / backward by word |
| `0` / `$` | Move to line start / end |
| `i` | Enter insert mode at cursor |
| `a` | Enter insert mode after cursor |
| `A` | Enter insert mode at end of line |
| `I` | Enter insert mode at beginning of line |
| `x` | Delete character under cursor |
| `dd` | Delete entire line |
| `u` | Undo last change |
| `Ctrl+R` | Redo |
| `yy` | Yank (copy) line |
| `p` | Paste after cursor |
| `Enter` | Submit message |
| `/` | Enter inline search |
| `Escape` | Clear pending command / return to normal |

**Visual mode** — entered with `v` from normal mode; use movement keys to select text, then:

| Key | Action |
|-----|--------|
| `y` | Yank selection |
| `d` | Delete selection |
| `Escape` | Exit visual mode |

### Vim Mode Indicator

When vim mode is active, a mode indicator (`NORMAL`, `INSERT`, `VISUAL`) is displayed in the status line.

---

## Special Input Behaviors

### Shift+Enter for Newline

Pressing `Shift+Enter` in the chat input field inserts a literal newline character without submitting the message. This is the standard way to write multi-line prompts.

Pressing plain `Enter` always submits the message regardless of the number of lines already in the input buffer.

In vim insert mode, `Enter` also submits. Use `Shift+Enter` for newlines in vim mode as well.

### ESC During Streaming

Pressing `Escape` while the model is streaming a response interrupts the stream. The partial response is preserved in the conversation history and the model stops generating. The input field regains focus and you can send a follow-up message.

This is equivalent to pressing `Ctrl+C` during streaming, except that `Ctrl+C` also signals any tool calls in progress to abort (via `AbortController`), while `Escape` only stops the stream and allows running tools to finish.

---

### @file Injection with Typeahead

Type `@` followed by a path in the prompt to inject a file's contents into your message. The `@` token only triggers when it is at a word boundary (start of input or preceded by whitespace). As you type after the `@`, Claurst opens a typeahead completion overlay scanning the current working directory.

```
explain @src/main.rs and compare to @tests/integration.rs
```

When you press `Enter`, Claurst:

1. Scans the message for `@<path>` tokens at word boundaries.
2. Resolves each path relative to the working directory; `~/` expands to your home directory.
3. Reads the file contents and substitutes them inline before sending the prompt to the model.

The `@` reference works with:

- Plain absolute paths: `@/etc/hosts`
- Paths relative to cwd: `@src/main.rs`
- Home-relative paths: `@~/.bashrc`
- Trailing punctuation is stripped: `@src/main.rs.` is treated as `@src/main.rs`

An `@` that is *not* at a word boundary (e.g. inside an email `me@example.com`) is left alone — neither the typeahead nor the file injection triggers.

**Limits and warnings.** If a referenced path is too large, binary, a directory, or unreadable, Claurst opens a confirmation dialog before sending:

| Issue | Behavior |
|-------|----------|
| File exceeds size limit | Dialog offers "Allow anyway" or "Abort" |
| Binary file | Dialog warns; same choice |
| Path is a directory | Dialog warns; cannot inject (must remove or rewrite the @ref) |
| Path unreadable | Skipped; error shown in dialog |

Files that pass all checks are injected silently — no dialog is shown.

**Configuration.** Two settings in `~/.claurst/settings.json`:

| Setting | Default | Description |
|---------|---------|-------------|
| `fileInjectionEnabled` | `true` | Master switch — set to `false` to disable @-injection entirely |
| `fileInjectionMaxSize` | `100` | Per-file size limit in KB; `0` disables the check (accept all) |

These can also be edited in the in-app settings screen.

**Typeahead navigation.** While the completion overlay is open:

| Key | Action |
|-----|--------|
| `Up` / `Down` | Move selection |
| `Tab` / `Enter` | Insert the highlighted completion |
| `Escape` | Dismiss the overlay (keep typed text) |

---

## Non-English Keyboard Layout Support

### The Problem

Terminal key events for `Ctrl+<key>` combinations are reported as raw control codes (`0x01` through `0x1A` for `Ctrl+A` through `Ctrl+Z`). These codes map to the physical QWERTY key position, not the character printed on the key.

On non-English keyboard layouts (Cyrillic, Arabic, Greek, CJK, etc.), the Latin letters used in Claurst's shortcuts may not appear on the keycaps, and some input methods send layout-translated scan codes for Ctrl combinations — causing Claurst to miss the shortcut entirely.

### The Fix

Claurst resolves this by mapping `Ctrl+<scancode>` events to their QWERTY positional equivalents before keybinding lookup. Concretely:

1. When a `Ctrl+<key>` event arrives, the physical scan position is extracted.
2. That position is mapped to the corresponding QWERTY letter (e.g., physical position of the Cyrillic `Ф` key = QWERTY `A` position).
3. The resulting `Ctrl+A` event is passed to the keybinding system.

This means `Ctrl+Ф` on a Cyrillic layout fires the same action as `Ctrl+A` on a QWERTY layout, regardless of the active input method or language setting. All `Ctrl+<letter>` keybindings in the default table above work by physical position.

### Implications for Custom Bindings

If you add a binding for `ctrl+a` in `keybindings.json`, it will fire when you press `Ctrl` and the key in the `A` position on your physical keyboard, regardless of what character that key is labeled. This is intentional.

If you want a binding that fires only when the `A` character is actually produced (i.e., layout-aware), prefix the key with `char:`:

```json
{
  "context": "chat",
  "action": "myAction",
  "key": "ctrl+char:a"
}
```

Layout-aware bindings are not recommended for the standard workflow bindings because they break on non-QWERTY layouts.

### Alt Key on macOS

On macOS, `Alt` (Option) key combinations produce special Unicode characters at the OS level before they reach the terminal. Claurst intercepts these at the terminal input layer and re-emits them as `alt+<key>` events using the same positional mapping described above.

If an `alt+<key>` binding does not fire on macOS, check whether your terminal emulator is configured to send `Escape + key` sequences for Option key combinations (the iTerm2 and Alacritty option is "Use Option as Meta Key" or equivalent).
