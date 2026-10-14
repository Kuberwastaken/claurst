# Claurst Keybindings Reference

This document covers all keyboard shortcuts in Claurst, how to customize them, vim mode, and special input behaviors.

---

## Table of Contents

1. [Default Keybindings](#default-keybindings)
   - [Global Context](#global-context)
   - [Chat Context](#chat-context)
   - [Fixed Keys](#fixed-keys)
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
| `Ctrl+C` | (fixed) | Copy the mouse selection if there is one; otherwise cancel the running turn; otherwise clear the prompt, and a second press within 2 seconds exits. Not rebindable |
| `Ctrl+D` | (fixed) | On an empty prompt, a second press within 2 seconds exits. Not rebindable |
| `Ctrl+L` | redraw | Does nothing; in the prompt, `clearLine` (below) takes precedence |
| `Ctrl+R` | historySearch | Open prompt-history search |
| `Ctrl+B` | createBranch | Bound by default but not implemented: the key does nothing |
| `Alt+H` | openHelp | Toggle the help overlay |

### Chat Context

These bindings are active when focus is in the chat input field.

| Key | Action | Description |
|-----|--------|-------------|
| `Enter` | submit | Submit the current message to the model. While a turn runs it steers the turn instead (see [Typing while a turn runs](./commands.md#typing-while-a-turn-runs)) |
| `Escape` | cancelTurn | Cancel the running turn, like `Ctrl+C`. Ignored while idle |
| `Shift+Enter` | newline | Insert a literal newline without submitting |
| `Ctrl+J` | newline | Newline (fallback for terminals without CSI-u protocol) |
| `Up` / `Ctrl+O` | historyPrev | Navigate to the previous message in input history |
| `Down` / `Ctrl+I` | historyNext | Navigate to the next message in input history |
| `Tab` | indent | Insert indentation (or cycle completions if open). While a turn runs it queues the message for after the turn |
| `Alt+Up` | editQueued | Pull the last pending (steered or queued) message back into the empty prompt. Scrolls up when nothing is pending |
| `Alt+X` | removeQueued | Drop the last pending message |
| `Shift+Tab` | reverseIndent | Remove one level of indentation |
| `Page Up` | scrollUp | Scroll the transcript up 10 lines |
| `Page Down` | scrollDown | Scroll the transcript down 10 lines |
| `Home` / `Cmd+Left` / `Ctrl+A` | goLineStart | Move the cursor to the start of the prompt |
| `End` / `Cmd+Right` / `Ctrl+E` | goLineEnd | Move the cursor to the end of the prompt |
| `Alt+Left` | previousMessage | Scroll the transcript up 5 lines |
| `Alt+Right` | nextMessage | Scroll the transcript down 5 lines |
| `Ctrl+Shift+A` | openModelPicker | Open the interactive model picker |
| `Ctrl+K` | openCommandPalette | Open the slash command palette |
| `Ctrl+U` | killToStart | Delete from cursor to beginning of line |
| `Ctrl+W` / `Alt+Backspace` | killWord | Delete the word before the cursor |
| `Alt+D` | deleteWord | Delete the word after the cursor |
| `Ctrl+H` | deleteCharBefore | Delete character before cursor |
| `Ctrl+L` | clearLine | Clear the prompt |
| `Alt+Enter` | newline | Newline (another fallback for terminals without CSI-u) |
| `Alt+E` | expandPaste | Expand the `[Pasted text #N ...]` placeholder at the cursor (or the first one) into the full pasted text |
| `Ctrl+.` | jumpToNextError | Jump to next error / issue |
| `Ctrl+Shift+.` | jumpToPreviousError | Jump to previous error / issue |

`Shift+Tab` (`reverseIndent`) cycles the permission mode (Default, Accept edits, Bypass permissions, back to Default; Plan goes to Default); it does not remove indentation. Most terminals send `Ctrl+I` as `Tab`, so `historyNext` on `Ctrl+I` only fires when the terminal reports it separately (kitty keyboard protocol).

The defaults also bind `Ctrl+Left` / `Ctrl+Right` (`moveWordBackward` / `moveWordForward`), `Ctrl+F` (`findInMessage`), `Ctrl+Shift+F` (`globalSearch`), `Ctrl+]` (`findNext`), `Ctrl+[` (`findPrev`) and `Ctrl+G` (`goToLine`), but these actions are not implemented, so the keys do nothing. `F3` / `Shift+F3` are listed too, but function keys never reach the binding table. Use `Alt+B` / `Alt+F` to move by word and `Ctrl+P` for project search. The `openSearch` action (no default key) also opens project search and can be bound; the default `globalSearch` name does nothing.

> `Ctrl+A` previously opened the model picker; it now moves the cursor to the line start (matching Emacs/readline). The model picker is now `Ctrl+Shift+A`. Old `keybindings.json` files are auto-migrated.

### Fixed Keys

These keys are hard-coded in the prompt and have no action names, so they cannot be remapped. A `keybindings.json` binding on the same key (other than `F1` and `Shift+Insert`) takes precedence over them.

| Key | Effect |
|-----|--------|
| `F1`, or `?` on an empty prompt | Toggle the help overlay |
| `Ctrl+T` | Toggle the tasks overlay |
| `Ctrl+P` | Open the project search dialog (ripgrep) |
| `Ctrl+Y` / `Alt+Y` | Yank the last killed text / cycle to an earlier kill |
| `Alt+B` / `Alt+F` | Move one word left / right |
| `Ctrl+Backspace` | Delete the word before the cursor |
| `Alt+Delete` / `Ctrl+Delete` | Delete the word after the cursor |
| `Alt+Up` / `Alt+Down` | Scroll the transcript 20 lines (`Alt+Up` edits the last queued message instead when one is pending) |
| `Ctrl+V` / `Cmd+V` | Paste: an image from the clipboard if there is one, otherwise text |
| `Shift+Insert` | Paste text |
| `Alt+V` | Start / stop voice recording (when `/voice` is on) |

### Confirmation Context

Tool permission prompts handle their keys directly; these keys cannot be rebound.

| Key | Effect |
|-----|--------|
| `y` | Allow once |
| `Y` | Allow for this session |
| `p` | Always allow in this project |
| `a` | Always allow everywhere |
| `n` | Deny; then type an optional reason and press Enter (Esc denies without one) |
| `1`-`9` | Select the option with that number |
| `Up` / `Down` | Move the selection |
| `Enter` | Choose the selected option |
| `Escape` | Deny without a reason (Sandbox Denied: keep the denial) |

Opaque Bash lines (substitutions, heredocs, `eval`, `sudo`, `bash -c` and similar) offer only `y`, `Y` and `n`. A **Sandbox Denied** prompt uses `y` (rerun outside the sandbox once), `Y` (this session), `P` (always for commands starting with this word) and `n`. See [The permission dialog](configuration.md#the-permission-dialog) and [Shell Sandbox](configuration.md#shell-sandbox).

---

## Keybinding Contexts

Each binding belongs to a context. When a key is pressed, Claurst looks at bindings in the active context and in `Global`; when several match, the one defined last wins, so your `keybindings.json` entries override the defaults. Context names are PascalCase; an unknown or missing `context` falls back to `Global`.

| Context | Takes effect |
|---------|--------------|
| `Global` | Merged into every lookup (same situations as `Chat`) |
| `Chat` | When the prompt has focus and no dialog or overlay is open |

In practice only `Global` and `Chat` bindings take effect. The other accepted names (`Autocomplete`, `Confirmation`, `Help`, `Transcript`, `HistorySearch`, `Task`, `ThemePicker`, `Settings`, `Tabs`, `Attachments`, `Footer`, `MessageSelector`, `DiffDialog`, `ModelPicker`, `Select`, `Plugin`) carry default bindings, but every dialog, picker and overlay handles its own keys before the binding table is consulted, so bindings in those contexts are never used.

---

## Customizing Keybindings

### Via /keybindings command

`/keybindings` opens `~/.claurst/keybindings.json` with the system's default application for the file (not `$EDITOR`). If the file does not exist it is first created from the default bindings, grouped by context:

```
/keybindings
```

Bindings are read once at startup; restart Claurst after editing the file.

### Via keybindings.json

Edit `~/.claurst/keybindings.json` directly. The file format is:

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
| `context` | string | Keybinding context, PascalCase (see table above). Optional; defaults to `Global` |
| `action` | string \| null | Action identifier (or `null` to unbind) |
| `chord` | string | Key combination, e.g. `ctrl+shift+a`; a multi-key chord separates keystrokes with spaces |

The file written by `/keybindings` uses a grouped form, which is also accepted:

```json
{
  "bindings": [
    { "context": "Chat", "bindings": { "ctrl+k": "openCommandPalette", "ctrl+g": null } }
  ]
}
```

A grouped file has no `schema_version`, so it is migrated to the flat form above on the next start.

### Schema Versioning and Smart Merge

`keybindings.json` carries a top-level `schema_version` field (currently `1`). When Claurst's defaults change in a release, the file is auto-migrated on next launch:

1. Claurst reads the file and compares `schema_version` against the bundled `KEYBINDINGS_SCHEMA_VERSION`.
2. If the file is older, Claurst runs a **smart merge**:
   - Your customizations (any binding whose `chord` you set explicitly) are preserved.
   - Stale bindings that match an *old* default that has since changed are dropped: `ctrl+a → openModelPicker` (`Ctrl+A` now moves to the start of the prompt) and `tab → togglePreview` in `Chat`.
   - Any new bindings present in the current defaults but not in your file are added.
3. The migrated file is written back with the new `schema_version`.

A warning is logged whenever a migration occurs. If you want to opt out of the merge for a binding, leave a different action assigned to it — explicit customizations always win.

Setting `"action": null` for a chord explicitly **unbinds** the default — useful when you want a key to do nothing rather than fire its default action.

Key notation uses lowercase letters, with modifier prefixes separated by `+`:

| Prefix | Modifier key |
|--------|-------------|
| `ctrl+` (or `control+`) | Control |
| `alt+` (or `opt+`, `option+`) | Alt / Option |
| `shift+` | Shift |
| `super+` (or `cmd+`, `meta+`, `win+`) | Super / Cmd |

Special key names: `enter`, `escape` (`esc`), `tab`, `backspace`, `delete`, `space`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`. Function keys can be written but never match, because they are not passed to the binding table.

### Chord Bindings

A chord is a sequence of keystrokes written in one `chord` string, separated by spaces:

```json
{
  "context": "Chat",
  "action": "openModelPicker",
  "chord": "ctrl+x ctrl+o"
}
```

After the first keystroke of a chord, Claurst waits for the next one with no timeout. If the next key completes a binding, its action runs; if it matches nothing, it is discarded and the chord is reset. Use an unbound first key: if the first keystroke is itself bound in the same context or `Global`, that binding fires immediately and the chord never completes.

Example — open help with `Ctrl+X H`:

```json
{
  "context": "Global",
  "action": "openHelp",
  "chord": "ctrl+x h"
}
```

---

## Non-Rebindable Keys

The following keys have fixed behavior and cannot be rebound:

| Key | Fixed behavior |
|-----|---------------|
| `Ctrl+C` | Copy the selection, cancel the running turn, or clear the prompt; twice within 2 seconds exits |
| `Ctrl+D` | On an empty prompt, twice within 2 seconds exits; otherwise does nothing |
| `Ctrl+M` | Identical to `Enter` at the terminal level (terminals emit `CR` for both) |

These keys are handled by fixed code in the TUI. If any of them appear as a `chord` in `keybindings.json`, Claurst:

1. Logs a warning at startup (`Cannot rebind protected key '<chord>' in keybindings.json`).
2. **Filters the binding out** of the loaded set before resolving any keystrokes.

So overriding a protected key is a no-op in behavior, but you also get a clear signal in the logs that the binding was rejected.

The check compares the lowercased chord string literally against `ctrl+c`, `ctrl+d` and `ctrl+m`, so another spelling such as `control+c` is not filtered and does rebind the key (a known bug).

The action names behind the fixed keys still exist and can be bound to other chords: `interrupt` (cancel the running turn, or clear the prompt and exit on a second press within 2 seconds) and `exit` (exit when the prompt is empty).

---

## Vim Mode

The prompt editor contains a vim emulation with insert, normal, visual (`v`, `V`), command-line (`:`) and search (`/`) modes.

### Enabling Vim Mode

```
/vim
/vim on
/vim off
```

The setting lasts for the current session. There is no persistent setting: `/vim on|off` also writes `editor_mode` to `~/.claurst/ui-settings.json`, but nothing reads it at startup, and `/config` has no vim key.

### Current Limitation

Vim mode starts in insert mode, and the TUI does not pass `Escape` to the vim editor, so normal, visual and command-line modes cannot be reached from the keyboard. With vim mode on, the prompt behaves like the default editor.

### Implemented Commands

For reference, the editor implements: motions `h` `l` `w` `b` `e` `W` `B` `E` `0` `^` `$` `G` `gg` `f` `F` `t` `T` `;` `,`; operators `d` `c` `y` with motions plus `dd` `yy` `D` `C` `Y`; `x` `X` `s` `S` `r` `~` `J` `>` `<` `o` `O` `p` `P`; insert entries `i` `a` `I` `A`; `u` undo, `.` repeat, `n` / `N` search repeat; registers (`"`), marks (`m`, `'`) and macros (`q`, `@`); `:q`, `:set vim`, `:set novim`, `:noh`. There are no `j` / `k` history motions and no redo.

### Vim Mode Indicator

When vim mode is active, the prompt shows the mode (`-- INSERT --`, `-- NORMAL --`, ...).

---

## Special Input Behaviors

### Shift+Enter for Newline

Pressing `Shift+Enter` in the chat input field inserts a literal newline character without submitting the message. This is the standard way to write multi-line prompts.

Pressing plain `Enter` always submits the message regardless of the number of lines already in the input buffer.

In vim insert mode, `Enter` also submits. Use `Shift+Enter` for newlines in vim mode as well.

### ESC During Streaming

Pressing `Escape` while a turn runs cancels it exactly like `Ctrl+C`: the turn's cancellation token stops the model request and any running tools (and background sub-agents started by the turn). The output streamed so far stays in the transcript, the Reconnecting pill clears, and steers that were not yet delivered and Tab-queued messages return to the prompt; background completion notices are still delivered afterwards. A turn counts as running until its task ends, so `Escape` also works while tools run between model requests.

On an idle prompt `Escape` keeps its other meanings (vim normal mode, leaving transcript focus); pressing it twice does nothing.

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

When a `Ctrl` combination reports a non-Latin character, Claurst maps it to the Latin letter at the same physical position on a QWERTY keyboard before handling the key. For binding lookups the same mapping also applies to `Alt` combinations. The mapping covers the Russian and Ukrainian JCUKEN layouts (for example `Ctrl+Ф` becomes `Ctrl+A`); characters from other layouts are left unchanged.

There is no layout-aware binding syntax: a `ctrl+a` binding matches the `A` key, or the key mapped to it.

### Alt Key on macOS

On macOS, `Option` combinations produce special characters unless the terminal sends them as `Escape` + key. If an `alt+<key>` binding does not fire, enable "Use Option as Meta Key" (Terminal.app), "Esc+" for Option (iTerm2) or the equivalent setting in your terminal. Claurst does not translate those characters itself.
