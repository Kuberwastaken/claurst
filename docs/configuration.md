# Claurst Configuration Reference

Claurst is configured through a layered system of JSON files, environment
variables, and command-line flags. This document describes every option.

---

## Configuration File Location

The global settings file is `settings.json` in the Claurst config directory,
the first of:

1. `$CLAURST_HOME`, when set and non-empty.
2. `~/.claurst/`, when that directory already exists.
3. `$XDG_CONFIG_HOME/claurst/` (when `XDG_CONFIG_HOME` is an absolute path),
   otherwise `~/.config/claurst/`.

The rest of the docs write this directory as `~/.claurst/`. It is created when
Claurst first saves settings.

The global file must be plain JSON: comments are not stripped from it. A file
that does not parse is reported at startup and left unchanged, and Claurst runs
with default settings.

When the `config` object is present it must contain every key that has no
default: `permission_mode`, `theme`, `auto_compact`, `compact_threshold`,
`verbose`, `output_format`, `mcp_servers`, `allowed_tools`,
`disallowed_tools`, `env`, `enable_all_mcp_servers` and
`disable_claude_mds`. A `config` object missing any of them fails to parse.
The file Claurst writes itself always has them, so edit that file rather than
starting from an empty one.

### Per-project settings

Claurst walks up from the current working directory looking for a project-level
settings file. The first file found wins (project settings take precedence over
global settings):

```
<project-root>/.claurst/settings.json
<project-root>/.claurst/settings.jsonc
```

Settings that appear in the project file override the corresponding global
values. Keys absent from the project file fall back to the global value.
Project files may contain `//` and `/* */` comments. A project file that does
not parse is ignored without a message, and the search stops at it.

A project settings file comes from the repository and is treated as untrusted.
Settings that run commands, fetch URLs, redirect provider traffic, or widen what
the agent may do without asking are **global-only**: Claurst reads them only from
the global settings file and ignores them (with a warning in the log) when a
project file sets them:

| Global-only setting | Why |
|---------------------|-----|
| `hooks` (every event) | Runs shell commands |
| `formatter` (top-level and `config.formatter`) | Runs a command after every file write |
| `lsp_servers` | Spawns language-server processes |
| `skills.urls` (top-level and `config.skills.urls`) | Clones the URL with git |
| `providers` / `config.provider_configs` | Can point the API base URL, and your API key, at another host |
| `workspace_paths`, `additional_dirs` | Widen the auto-allowed read set |
| `permission_mode`, `permissionRules`, `allowedBashPrefixes`, `allowed_tools`, `skipDangerousModePermissionPrompt` | Grant or pre-approve permissions |
| `trustProjectMcpServers` | Bypasses the project MCP server approval prompt |
| `enabledPlugins` | Enables plugins, which can ship hooks and servers |
| `remoteControlAtStartup` | Exposes the session over the remote-control bridge |
| `sandbox` (every field) | Turning the shell sandbox off, opening its network or exempting commands removes the containment for everything a project can make the agent run |

Project-defined MCP servers are still read from the project file, but they do
not launch until you approve them for that project (or set
`trustProjectMcpServers` globally). `skills.paths` from a project file is merged
with the global list.

---

## Top-level Settings Structure

```json
{
  "version": 1,
  "provider": "anthropic",
  "config": { ... },
  "providers": { ... },
  "modelOverrides": { ... },
  "projects": { ... },
  "commands": { ... },
  "formatter": { ... },
  "agents": { ... },
  "skills": { ... },
  "permissionRules": [],
  "enabledPlugins": [],
  "disabledPlugins": [],
  "hasCompletedOnboarding": false
}
```

Most day-to-day options live inside the `config` object. Provider credentials
live in the `providers` map. Corrected model metadata for self-hosted or
unknown models lives in the `modelOverrides` map — see
[Model metadata overrides](providers.md#overriding-model-metadata).

### Terminal status reporting

The interactive TUI reports its state to the terminal through two escape
sequences. Both are top-level settings, default to `true`, are read once at
startup, and are never emitted by headless (`--print`), ACP, or bridge modes.

| Key | Sequence | Description |
|-----|----------|-------------|
| `terminalProgressBar` | OSC 9;4 | Busy indicator while a turn is streaming. Only sent to terminals known to render it (iTerm2, WezTerm, Ghostty, Windows Terminal, ConEmu), and never inside tmux/screen. |
| `terminalProgramStatus` | OSC 7501 | [Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status) reports, so terminals and session managers that support it (Rex, cmux) can show which tab is working, waiting on you, or finished. Terminals without support ignore it. Skipped when stdout is not a tty, `TERM=dumb`, or inside tmux/screen. |

OSC 7501 reports use the root record with `app=claurst`:

| State | When |
|-------|------|
| `idle` | At startup, after a cancelled turn, and once you start typing after a finished turn. |
| `working` | A turn is in flight: the model is streaming or tools are running. |
| `blocked` | Waiting on you. `kind=permission` for a tool permission or MCP server approval, `kind=question` for an AskUserQuestion prompt, `kind=auth` for a device/browser sign-in. Carries a short base64 `msg`. |
| `done` | The last turn finished and the prompt is waiting. |
| `error` | The last turn ended in an error. |

On a normal exit Claurst sends `state=clear` to remove its record. If it crashes it reports `state=error` instead, so the tab still shows that something went wrong.

### Attention notifications

`notifications` (top-level, default `true`, also in `/config`) rings the terminal bell and sends a desktop notification when a turn finishes, fails, or waits on you (tool permission, question, sign-in) while the terminal window is in the background. Focus comes from the terminal's focus reporting; terminals without it count as always focused and never alert.

| Terminal | Sequence |
|----------|----------|
| iTerm2, WezTerm, Ghostty, kitty | OSC 9 |
| Others | OSC 777 (`notify`), ignored where unsupported |
| Inside tmux | The OSC wrapped in a DCS passthrough; needs `set -g allow-passthrough on`, and `set -g focus-events on` so tmux forwards focus changes |

The bell is sent in every case.

```json
{
  "notifications": false
}
```

```json
{
  "terminalProgressBar": true,
  "terminalProgramStatus": false
}
```

---

## The `config` Object

The `config` object holds runtime behaviour options.

### Model and token settings

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `api_key` | string \| null | null | Anthropic API key. Overrides `ANTHROPIC_API_KEY` env var. Prefer the env var in shared environments. |
| `model` | string \| null | provider default | Model ID to use. When absent, the provider's default is used (e.g. `claude-opus-4-6` for Anthropic, `gpt-4o` for OpenAI). |
| `max_tokens` | integer \| null | 32000 | Maximum tokens per model response. `--max-tokens` overrides it. |
| `provider` | string \| null | `"anthropic"` | Active provider. See the [Providers](#providers) section. |
| `effort` | string \| null | unset | Reasoning effort for new turns: `"none"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`, or `"ultracode"`. When unset, no thinking-budget or temperature override is applied at all — this is not the same as `"medium"`, which has its own explicit values. |
| `maxRetries` | integer \| null | null (5) | Retries after a model request fails transiently: HTTP 408/409/425/429/5xx, dropped connections, and streams that stall or end before completion. Waits follow the server's `retry-after-ms` / `Retry-After` header, otherwise exponential back-off with jitter, each capped at 60 s. Auth, quota/billing and invalid-request errors are never retried. `0` disables retrying. Note the camelCase key. |
| `completionWakeups` | object \| null | null | Which background completions are delivered to the session as `<subagent …>` / `<background-shell …>` messages: at the next step of a running turn, or as a new turn when idle. `{"subagents": bool, "shell": bool}`; unset keys keep the defaults: sub-agents `true`, shell `false`. With `shell` unset or `true`, a Bash call's own `notify: true` / `false` decides for that command; `"shell": false` is a hard off that `notify: true` cannot override. Cancelled tasks never report. Global settings only: a project settings file cannot set it. Note the camelCase key. |
| `degradationSummaryEnabled` | boolean \| null | null (on) | When set to `false`, exceeding `max_turns` returns the last assistant message immediately instead of running one final tool-less degradation summary turn. Applies to sub-agents as well. Unset means enabled. Note the camelCase key. |

### Permission mode

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `permission_mode` | string | `"default"` | Controls how tool permissions are enforced. One of `"default"`, `"acceptEdits"`, `"bypassPermissions"`, `"plan"`. Currently replaced at startup by `--permission-mode` (default `default`), so set the mode with the flag or `yoloMode`. |
| `yoloMode` | boolean | `false` | Persistent YOLO mode: the effective permission mode is `bypassPermissions`. Global-only in the settings merge: a project settings file cannot enable it. |

See [Permission Modes](#permission-modes) for a full description of each value.

### Interface and output

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `theme` | string | `"default"` | Color theme for the TUI. One of `"default"`, `"dark"`, `"light"`, `"deuteranopia"`. |
| `output_style` | string \| null | null | Named output style. Built-in values: `"default"`, `"concise"`, `"explanatory"`, `"learning"`, `"caveman"`, `"rocky"`. Custom styles can be added as `.md` or `.json` files under `~/.claurst/output-styles/`. Set by `/output-style`, `/caveman`, `/rocky` and `/normal`. |
| `output_format` | string | `"text"` | Not used by headless runs: `--output-format` (default `text`) decides. See [Headless mode](advanced.md#headless-mode) for the `json` and `stream-json` shapes. |
| `verbose` | boolean | false | Not used: replaced at startup by `--verbose`, which sets the log level to `debug` (default `warn`). |

### Context compaction

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `auto_compact` | boolean | — | Not read by the query loop: auto-compaction is on unless `--no-auto-compact` is passed. The top-level `autoCompact` key is not read either. |
| `compact_threshold` | float | — | Not used: the trigger point is fixed. |

See [Auto-compaction](advanced.md#auto-compaction) for when compaction runs.

### System prompt

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `custom_system_prompt` | string \| null | null | Replace the default Claurst system prompt entirely with this text. |
| `append_system_prompt` | string \| null | null | Append this text to the base system prompt (before the instruction files and the skill listing). |

### Tool access

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `allowed_tools` | array of rule specs | [] | Allow these without prompting. Global-only: ignored in a project settings file. |
| `disallowed_tools` | array of rule specs | [] | Deny these in every permission mode, `bypassPermissions` included. A tool-wide entry (`Write`) also hides the tool from the model. A project settings file may add entries. |

Each entry is a rule spec: `Tool` (every use), `Tool(pattern)`, or `*` (every
tool). For `Bash` the pattern is a shell rule matched per command
(`Bash(git status *)`, `Bash(npm run test:*)`); for other tools it is a path
glob (`Edit(/repo/src/**)`). Tool names match the internal names: `Bash`,
`Read`, `Write`, `Edit`, `NotebookEdit`, `BatchEdit`, `ApplyPatch`, `Glob`,
`Grep`, `WebSearch`, `WebFetch`, and MCP tool names in the form
`mcp__<server>__<tool>` (see [MCP tool names](mcp.md#mcp-tool-names));
`mcp__<server>` or `mcp__<server>__*` covers every tool of that server, so
`disallowed_tools: ["mcp__github"]` hides all of them. The
`--allowed-tools` / `--disallowed-tools` flags
and `/permissions allow|deny` use the same syntax. See
[Permission Rules and Approvals](#permission-rules-and-approvals).

### Directory access

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `additional_dirs` | array of strings | [] | Directories outside the working directory whose files can be read without a prompt; writes there still follow the permission mode. Currently replaced at startup by the `--add-dir` values, so pass the flag instead. |
| `workspace_paths` | array of strings | [] | Same read allowance as `additional_dirs`, kept across sessions. `/add-dir <path>` (or `claurst add-dir <path>`) appends to it. |

### MCP servers

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `mcp_servers` | array of `McpServerConfig` | [] | Model Context Protocol servers to connect at startup. |

Each `McpServerConfig` object:

```json
{
  "name": "my-server",
  "command": "/path/to/server",
  "args": ["--flag"],
  "env": { "MY_VAR": "value" },
  "type": "stdio"
}
```

`type` can be `"stdio"` (default), `"http"` (streamable HTTP) or `"sse"`
(legacy SSE). Remote servers set `url` instead of `command`. See
[McpServerConfig Fields](mcp.md#mcpserverconfig-fields) for every field.

### Environment variables for remote sessions

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `env` | object (string → string) | {} | Variables managed by `/remote-env` (`set`, `unset`, `list`) for remote sessions. They are not injected into local tool executions (Bash, MCP servers or LSP servers). |

### Hooks

Hooks run shell commands on session events (before and after tool calls,
prompt submission, stop, session start and end, and more), using Claude Code's
hook format. They are global-only. See [Hooks](hooks.md) for the events, the
JSON input and output, and examples.

```json
"hooks": {
  "PreToolUse": [
    {
      "matcher": "Bash",
      "hooks": [{ "type": "command", "command": "~/.claurst/hooks/check-bash.sh", "timeout": 10 }]
    }
  ],
  "Stop": [
    { "hooks": [{ "type": "command", "command": "notify-send 'Claurst done'" }] }
  ]
}
```

### External status line

`config.statusLine` runs a shell command periodically and shows its stdout as a row
above the footer, compatible with Claude Code `statusLine` scripts (for example
the `cc-statusline` ecosystem). The command receives a JSON session payload on
stdin (`session_id`, `cwd`, `model`, `workspace.current_dir`, `context_window`,
`cost.total_cost_usd`, `elapsed_seconds`). In `context_window`,
`total_input_tokens` is the number of tokens in the current context window (the
figure `used_percentage` is derived from, not a session total), while `total_output_tokens` and the `current_usage` breakdown are cumulative
session totals.

```json
"statusLine": {
  "command": "~/.claurst/statusline.sh",
  "pollIntervalMs": 5000,
  "padding": 1,
  "maxLines": 2
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `command` | string | required | Shell command (`sh -c`, `cmd /C` on Windows). |
| `pollIntervalMs` | integer | 5000 | Poll interval, clamped to 1000–300000. A run that exceeds it is killed. |
| `padding` | integer | 0 | Horizontal padding in spaces, clamped to half the terminal width. |
| `maxLines` | integer | 10 | Maximum number of output lines shown. |

Output is capped at 4 KiB, ANSI escapes and control characters are stripped,
long lines are truncated with `...`, and stderr is discarded. Empty output hides
the row. `statusLine` is only read from the global `~/.claurst/settings.json`;
a project `.claurst/settings.json` cannot set or override it. The legacy
`CLAUDE_STATUS_COMMAND` environment variable is used when `statusLine` is unset.
Run `/cc-statusline` to show the active configuration.

### Other keys

Inside `config`:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `lsp_servers` | array | [] | Language servers for the `LSP` tool. See [LSP integration](advanced.md#lsp-integration). |
| `requestTimeoutSecs` | integer \| null | 600 | Total HTTP request timeout for model requests, in seconds. A provider entry's `requestTimeoutSecs` overrides it. |
| `autoCommits` | boolean \| null | null | Shadow-git snapshots after changes (`/revert`, `/checkpoints`, `/snapshot`). Same as `--auto-commits`. |
| `mouseCapture` | boolean \| null | null (on) | `false` releases the mouse to the terminal for native selection. |
| `cursorBlinkEnabled` | boolean | false | Blink the prompt cursor. |
| `fileAutocompleteLimit` | integer | 15 | Maximum `@` file suggestions. |
| `fileAutocompleteShowHiddenFiles` | boolean | false | Include hidden files in `@` suggestions. |
| `fileInjectionEnabled` | boolean | true | Inject the contents of `@file` references into the message. |
| `fileInjectionMaxSize` | integer | 100 | Size limit in KB for an injected file; `0` means no limit. |

Top-level:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `remoteControlAtStartup` | boolean | false | Start the remote-control bridge with the session. Global-only. |
| `trustProjectMcpServers` | boolean | false | Launch project MCP servers without approval. Global-only. Same as `--trust-project-mcp`. |
| `showCwd` | boolean | true | Show the working directory in the footer. |
| `showGitBranch` | boolean | true | Show the git branch in the footer. |

---

## Permission Modes

The `permission_mode` field (and `--permission-mode` CLI flag) controls how
tool calls are approved.

### `default`

Read-only operations (file reads, searches, glob) are permitted automatically.
Write and execute operations (file writes, shell commands) prompt the user for
confirmation in the TUI. In headless mode (`--print`) nobody can answer the
prompt: the call is rejected, the tool result tells the model why, and the run
continues (see [Headless mode](advanced.md#headless-mode)).

### `acceptEdits`

File edits and writes (`Edit`, `Write`, `NotebookEdit`, `BatchEdit`,
`ApplyPatch`) whose targets lie inside the workspace (the working directory,
`workspace_paths` and `--add-dir` directories) are accepted without
prompting. A target outside the workspace, including one reached through a
symlink, still asks. Shell commands and network access keep the `default`
behaviour.

### `bypassPermissions`

Permission prompts are skipped and every tool call is allowed, except calls
covered by a deny rule (`disallowed_tools`, `--disallowed-tools`,
`/permissions deny`), which are refused in every mode. This mode cannot be
used when running as root or via `sudo` on Unix systems (Claurst blocks it).

Use with caution: the model can read and modify any file reachable from the
current working directory without any user confirmation.

### `plan`

Read-only mode. File reads and searches are allowed; file writes and command
execution are blocked. This matches the built-in `plan` agent's behaviour and
is useful for code analysis sessions where you want to prevent accidental
modifications.

The permission mode can also be overridden per-session on the command line:

```bash
claurst --permission-mode acceptEdits "refactor the auth module"
claurst --dangerously-skip-permissions "..."  # equivalent to bypassPermissions
```

---

## Permission Rules and Approvals

Rules refine the permission mode per tool. Each rule allows or denies a tool,
optionally restricted to a pattern.

### Evaluation order

1. A matching **deny** rule refuses the call, in every mode.
2. `bypassPermissions` allows everything else.
3. A matching **allow** rule allows the call without prompting.
4. Otherwise the mode decides (see [Permission Modes](#permission-modes)):
   reads inside the workspace run, `acceptEdits` covers workspace edits,
   `plan` refuses writes, and everything else asks (headless runs refuse
   instead of asking).

Rules come from four places, all evaluated together:

| Source | Where it lives | Scope |
|--------|----------------|-------|
| `allowed_tools` / `disallowed_tools`, `--allowed-tools` / `--disallowed-tools`, `/permissions allow\|deny` | settings `config` block (flags: this session only) | configured |
| "Always allow in this project" | `<claurst home>/projects/<encoded project root>/permissions.json` | the current project (repository root, or the working directory outside a repository) |
| "Always allow everywhere" | `permissionRules` in the global settings file | every project |
| "Allow this session" | memory | this session |

Saved approvals never live inside the repository, so a repository cannot grant
itself permissions. `allowed_tools`, `permissionRules` and `allowedBashPrefixes`
are read only from the global settings file; a project settings file can add
`disallowed_tools` entries. Each `allowedBashPrefixes` entry `git` acts as the
rule `Bash(git *)`. Bash rules saved by earlier versions as `<command>*` (a
prefix of the raw line) are read as the per-command rule `<command> *`.

### Shell commands

Bash rules are matched per command, not against the raw command line.
Claurst splits the line at `;`, `&&`, `||`, `|`, `|&`, `&` and newlines,
removes quotes, and drops redirections. A line is auto-allowed only when
**every** command in it is allowed, and denied when **any** command matches a
deny rule (also after stripping leading `NAME=value` assignments and the
command's directory, so `FOO=1 /bin/rm x` still matches `Bash(rm *)`). For
deny rules an argument the shell would expand (it contains `$`, `{`, `}`, `*`,
`?` or `[`) matches any word of the pattern, so `git $X -f` and
`git {push,-f}` are denied by `Bash(git push *)`.

A Bash pattern is a command with an optional trailing ` *`, which matches
whole words: `Bash(npm run test:*)` is read as `npm run test *` and covers
`npm run test` and `npm run test -- --watch`, but not `npm run test:unit`.

| Pattern | Matches | Does not match |
|---------|---------|----------------|
| `git status *` | `git status`, `git status --short` | `git push`, `git status && rm -rf ~` |
| `npm run *` | `npm run build`, `npm run test -- --watch` | `npm install` |
| `git diff` | `git diff` | `git diff HEAD` |

Some lines cannot be judged from their text and always need an explicit
approval; patterns never cover them (only a rule equal to the whole line, as
recorded by "Allow this session", does):

- command or process substitution: `$(...)`, backticks, `<(...)`, `>(...)`
- heredocs (`<<`)
- subshells, groups and compound commands: `( ... )`, `{ ...; }`, `if`, `for`, `while`, `case`, functions
- commands that run other commands: `eval`, `exec`, `source`, `.`, `bash -c`,
  `sh -c` (and other shells with `-c`), `xargs`, `sudo`, `doas`, `su`, `env`,
  `command`, `builtin`, `nohup`, `nice`, `time`, `timeout`, `watch`,
  `parallel`, `export`, `find -exec`, interpreters running inline code
  (`python -c`, `node -e`, `perl -e`, `ruby -e`, ...), and a command name the
  shell would expand (`$CMD`, `r{m,}`)

A tool-wide `Bash` allow covers these lines too, unless a Bash deny pattern
exists. Deny patterns are best effort for these lines: they are only matched
against the whole line, so in `bypassPermissions` mode a denied command
hidden inside one (`eval "rm -rf x"`) is not caught. Use a tool-wide deny, or
another mode, when that matters. A command that redirects output to a file
(`>`, `>>`, `&>`) is never covered by a pattern; redirection to `/dev/null`
and `2>&1` are fine. "Allow this session" on such a line records the whole
line, and `p`/`a` are not offered for it. Commands
classified as Critical (`rm -rf /`, fork bombs, `dd if=...`, ...) are blocked
in every mode, including when they appear later in a compound line.

### The permission dialog

| Key | Choice |
|-----|--------|
| `y` | Allow once |
| `Y` | Allow this session (for Bash: exactly these commands) |
| `p` | Always allow in this project |
| `a` | Always allow everywhere |
| `n` | Deny, with an optional reason |

For Bash, `p` and `a` save one rule per command using its arity prefix
(`git status *`, `npm run build *`, `cargo test *`); the option label shows
the rules. A command shorter than its prefix (a bare `git`) or starting with
an assignment is saved exactly. Opaque lines offer only `y`, `Y` and `n`. For
file tools the saved rule covers the path (`<path>*`).

After `n`, type a one-line reason and press Enter (or Esc to deny without
one). The reason is returned to the model with the denial. A denial also
rejects the rest of the model's current batch of tool calls: calls that have
not started yet return an error instead of running, so the model stops and
reads your reason.

---

## Shell Sandbox

On macOS, every Bash tool command (foreground and `run_in_background`) runs
under Seatbelt: `/usr/bin/sandbox-exec -p <profile> -DWRITABLE_ROOT_0=... --
bash -c <command>`. The sandbox applies in every permission mode, including
`bypassPermissions`; the permission prompts decide *whether* a command runs,
the sandbox limits *what it can touch*. Other platforms have no OS sandbox yet
(Linux Landlock is planned), so commands there run unsandboxed whatever the
setting says. Only the Bash tool is sandboxed; PowerShell, REPL and MCP
servers are not.

The settings are a top-level `sandbox` block in the **global** settings file.
They are global-only: a project `.claurst/settings.json` cannot change them.

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "network": false,
    "unsandboxedPrefixes": []
  }
}
```

| Key | Values | Default | Description |
|-----|--------|---------|-------------|
| `mode` | `off`, `workspace-write`, `read-only` | `workspace-write` | `off` runs commands with your full privileges. |
| `network` | boolean | `false` | Allow network access (outbound and inbound) from sandboxed commands. |
| `unsandboxedPrefixes` | array of strings | `[]` | First words of commands that always run outside the sandbox. Filled by the "always" choice of the denial prompt; only matches simple commands (no `;`, `&`, `\|`, redirections or substitutions). |

What the profile allows (adapted from OpenAI Codex's Apache-2.0 Seatbelt
policies):

- Reading any file.
- `workspace-write`: writing beneath the working directory, `workspace_paths`,
  `additional_dirs` / `--add-dir` directories, `$TMPDIR` and `/tmp`. Inside
  every writable root, `.git`, `.claurst`, `.ssh`, `.aws` and `.gnupg` stay
  read-only, so a sandboxed command can run `git status` or `git diff` but not
  `git commit`.
- `read-only`: no writes (only `/dev/null` and the terminal).
- Network only with `network: true`.
- Everything else is denied (`(deny default)`), including XPC services.

What the sandbox does not protect against:

- Every file stays readable, including `~/.ssh`, `~/.aws`, keychains, other
  repositories and `~/.claurst/settings.json` (API keys). With `network: true`
  nothing stops a command from sending them elsewhere.
- Any binary can be executed; the profile only limits what it can write and
  reach.
- Only Bash commands are sandboxed. The file tools (Write, Edit, ApplyPatch),
  PowerShell, REPL, MCP servers, hooks and the status line are not.
- `.git` is read-only, so `git add`, `git commit` and `git stash` go through
  the rerun prompt.
- If Claurst itself already runs under Seatbelt (a sandboxed terminal or
  agent), every command fails with `sandbox-exec: sandbox_apply: Operation not
  permitted` (exit 71); set `mode` to `off` there.
- Apple marks `sandbox-exec` as deprecated, but it ships and works on macOS 15
  and 26.
- Linux and Windows run commands unsandboxed.

When a sandboxed command fails with a non-zero exit (other than 2, 126 or 127)
and its output contains a denial message such as `Operation not permitted`,
`Permission denied` or `Read-only file system` (or, with `network: false`,
`Could not resolve host`, `Couldn't connect to server` or `Network is
unreachable`), it is treated as a likely sandbox denial:

- In the interactive TUI a **Sandbox Denied** dialog offers to rerun it
  outside the sandbox **once**, **for this session**, or **always for commands
  starting with the same word** (saved to `sandbox.unsandboxedPrefixes`; not
  offered for launchers such as `bash`, `sh`, `env`, `xargs` or `python3`). The
  dialog is never answered by permission rules or `allowedBashPrefixes`.
- In headless (`--print`) and ACP sessions nobody is asked: the failed result
  is returned with a `[sandbox]` note so the caller can decide.

`/sandbox-toggle` (alias `/sandbox`) shows and changes the setting:
`/sandbox status`, `/sandbox off`, `/sandbox workspace-write` (or `on`),
`/sandbox read-only`, `/sandbox network on|off`; with no argument it toggles
between `off` and `workspace-write`. Changes are saved to the global settings
and apply to the next command. While the sandbox is active the footer shows
`sandbox: <mode>` (`+net` with network) and the system prompt says which
sandbox the model's commands run in.

---

## Instruction Files (AGENTS.md / CLAUDE.md)

Instruction files are plain Markdown documents that Claurst loads once at
session start and sends in the cached part of the system prompt. They give the
model standing context about your project, coding standards, or personal
preferences.

### File locations and order

Files are loaded in this order; later files are more specific and win when
two conflict:

1. **Global**: `~/.claurst/` (the Claurst config directory), then `~/.claude/`
   (Claude Code's global `CLAUDE.md`).
2. **Project**: each directory from the git root down to the working
   directory, outermost first.

The project walk stops at the directory that contains `.git` and never goes
above it. When the working directory is not inside a git repository, only the
working directory itself is read, so a stray `~/AGENTS.md` or `/AGENTS.md` is
never picked up.

In each directory:

| File | Behaviour |
|------|-----------|
| `AGENTS.override.md` | When present, it is the only file loaded from that directory. Use it for local overrides you do not commit. |
| `AGENTS.md` | Loaded first. |
| `CLAUDE.md` | Loaded second, for compatibility with Claude Code. |
| `.claude/CLAUDE.md` | Loaded third (Claude Code's alternative project location). |
| `.claurst/AGENTS.md` | Loaded last. Session memories are saved here. |

A file that is the same file as, or has the same content as, a file already
loaded is skipped (for example a `CLAUDE.md` symlinked to `AGENTS.md`).

Each file is sent as its own block, labelled with its path:

```
Instructions from: /home/me/project/AGENTS.md
<file content>
```

### Size budget

All instruction files together are limited to 32 KiB. The file that crosses
the limit is truncated with a note telling the model to read the rest from
disk; files after it are replaced by a note naming them. Global files are
loaded first, so a large global file leaves less room for project files.

### YAML frontmatter

A leading YAML frontmatter block (`---` ... `---`) is stripped before the file
is sent. Its fields are ignored.

### @include directives

Instruction files can pull in other files with `@include` lines:

```markdown
# Project Guide

@include ./docs/architecture.md
@include docs/coding-standards.md
```

Paths may be relative to the including file, absolute, or `~/`-prefixed, but
the target must stay inside the project root (the git root, or the working
directory without git) for project files, or inside the global directory the
file came from (`~/.claurst/` or `~/.claude/`) for global files. Anything outside is skipped with a comment, so a repository cannot pull
files such as `~/.ssh/config` into the prompt. Circular includes are detected
and skipped, nesting stops after 10 levels, and included files larger than
40 KB are skipped with a comment. Included content counts toward the 32 KiB
budget.

Claude Code's bare `@path` import syntax is not recognised; only lines
starting with `@include ` are expanded.

### Disabling instruction files

To skip all instruction files for a session:

```bash
claurst --no-claude-md "your prompt"
```

The `--bare` flag also disables them (together with hooks, plugins and
project skill directories).

---

## Skills

A skill is a Markdown file of instructions for a specific task. Claurst
lists every available skill (name and one-line description) to the model in
the cached part of the system prompt, inside an `<available_skills>` block, and
the model loads one with the `Skill` tool when a task matches. Skills loaded
from disk can also be run directly as slash commands (`/<name> args`).

### Skill files

A skill is either a directory with a `SKILL.md` file (`deploy/SKILL.md`, named
after the directory) or a flat `<name>.md` file. Optional frontmatter sets the
name and description:

```markdown
---
name: deploy
description: Deploy the service to staging or production.
---

Run `scripts/deploy.sh $ARGUMENTS` and report the result.
```

`$ARGUMENTS` is replaced with the arguments passed to the skill. Without a
`description`, the skill is listed as "Custom skill".

When the model loads a skill, the `Skill` tool returns the body wrapped in
`<skill_content name="..." base_dir="...">`, where `base_dir` is the skill's
directory, with a note that relative paths in the skill (for example
`scripts/`) resolve against it.

### Search locations

Skills are discovered once at session start, in the background: the TUI
opens at once, and the first request waits for discovery to finish (only a
first-time `skills.urls` clone takes noticeable time). For a given name the
first match wins, in this order:

1. Project: `.claurst/skills/`, `.agents/skills/` and `.claude/skills/` in
   each directory from the working directory up to the git root (nearer
   directories win; without git, only the working directory).
2. Global: `~/.claurst/skills/`, `~/.agents/skills/`, `~/.claude/skills/`.
3. `skills.paths`: extra directories (relative paths resolve against the
   working directory; `~/` is expanded).
4. `skills.urls`: git repositories, cloned once (`--depth=1`) into the system
   cache directory; skills are read from the repository root and its
   `skills/` subdirectory.
5. Bundled skills shipped with Claurst (`simplify`, `debug`, `batch`, ...). A
   skill found on disk with the same name replaces the bundled one.

```json
"skills": {
  "paths": ["~/my-skills", "tools/skills"],
  "urls": ["https://github.com/example/skills.git"]
}
```

`skills.urls` is global-only (see above); `skills.paths` from a project file
is merged with the global list. `--bare` skips the project skill directories
(step 1); global, configured and bundled skills stay.

`.claurst/commands/*.md` files are no longer read as skills; move them to
`.claurst/skills/`.

### Listing budget

The `<available_skills>` block uses the first line of each description,
capped at 250 characters, and stops adding descriptions at about 3k tokens;
the remaining skills are listed by name only. The block is omitted when the
`Skill` tool is not enabled for the session. `/skills` lists every skill with
its source file.

---

## Providers

Claurst can send requests to multiple LLM providers. Set the active provider
via the `provider` key in settings or the `--provider` CLI flag.

### Provider IDs

| Provider ID | Default model |
|-------------|--------------|
| `anthropic` | `claude-opus-4-6` |
| `openai` | `gpt-4o` |
| `google` | `gemini-2.5-flash` |
| `groq` | `llama-3.3-70b-versatile` |
| `cerebras` | `llama-3.3-70b` |
| `deepseek` | `deepseek-v4-pro` |
| `mistral` | `mistral-large-latest` |
| `xai` | `grok-2` |
| `openrouter` | `anthropic/claude-sonnet-4` |
| `together-ai` | `meta-llama/Llama-3.3-70B-Instruct-Turbo` |
| `perplexity` | `sonar-pro` |
| `cohere` | `command-r-plus` |
| `qwen` | `qwen3-max` |
| `deepinfra` | `meta-llama/Llama-3.3-70B-Instruct` |
| `github-copilot` | `gpt-4o` |
| `ollama` | `llama3.2` |
| `lmstudio` | `default` |
| `llamacpp` | `default` |
| `custom-openai` | `default` |
| `azure` | `gpt-4o` |
| `amazon-bedrock` | `anthropic.claude-sonnet-4-6-v1` |
| `venice` | `llama-3.3-70b` |

### Per-provider configuration

Each provider can have its own entry in the `providers` map (top-level in
`settings.json`) or in `config.provider_configs`. Provider-level `api_key`
and `api_base` override the corresponding environment variables.

```json
"providers": {
  "anthropic": {
    "api_key": "sk-ant-...",
    "api_base": "https://api.anthropic.com",
    "enabled": true,
    "models_whitelist": [],
    "models_blacklist": []
  },
  "openai": {
    "api_key": "sk-...",
    "enabled": true
  },
  "ollama": {
    "api_base": "http://localhost:11434",
    "enabled": true
  }
}
```

`ProviderConfig` fields:

| Field | Type | Description |
|-------|------|-------------|
| `api_key` | string \| null | API key for this provider. `{env:VARNAME}` is replaced with that variable's value. |
| `api_base` | string \| null | Override the default API base URL. `{env:VARNAME}` is replaced as for `api_key`. |
| `enabled` | boolean | Whether this provider is active. Default: true. |
| `models_whitelist` | array | If non-empty, only these model IDs are offered. |
| `models_blacklist` | array | These model IDs are never offered. |
| `options` | object | Provider-specific passthrough options. |
| `requestTimeoutSecs` | integer \| null | Request timeout for this provider, overriding `config.requestTimeoutSecs`. |

---

## Environment Variables

| Variable | Description |
|----------|-------------|
| `CLAURST_HOME` | Config directory (settings, sessions, skills). See [Configuration File Location](#configuration-file-location). |
| `XDG_CONFIG_HOME` | Used for the config directory (`$XDG_CONFIG_HOME/claurst`) when `CLAURST_HOME` is unset and `~/.claurst` does not exist. |
| `ANTHROPIC_API_KEY` | Anthropic API key. Checked after the `config.api_key` and `providers.anthropic.api_key` settings. |
| `ANTHROPIC_BASE_URL` | Override the Anthropic API base URL. |
| `OPENAI_BASE_URL`, `OLLAMA_HOST`, `LM_STUDIO_HOST`, `LLAMA_CPP_HOST`, `MINIMAX_BASE_URL` | Override the base URL of the `openai`, `ollama`, `lmstudio`, `llamacpp` and `minimax` providers. |
| `CLAURST_PROVIDER` | Active provider. Equivalent to `--provider`. |
| `CLAURST_API_BASE` | Override the API base URL for the active provider. Equivalent to `--api-base`. |
| `CLAURST_GOALS` | Set to `0` to disable the goal system (`/goal` command and `GoalCompleteTool`). |
| `OPENAI_API_KEY` | API key for the `openai` provider. |
| `GOOGLE_API_KEY` | API key for the `google` provider. |
| `GROQ_API_KEY` | API key for the `groq` provider. |
| `XAI_API_KEY` | API key for the `xai` provider. |
| `MISTRAL_API_KEY` | API key for the `mistral` provider. |
| `OPENROUTER_API_KEY` | API key for the `openrouter` provider. |
| `DEEPSEEK_API_KEY` | API key for the `deepseek` provider. |
| `COHERE_API_KEY` | API key for the `cohere` provider. |
| `DEEPINFRA_API_KEY` | API key for the `deepinfra` provider. |
| `VENICE_API_KEY` | API key for the `venice` provider. |
| `GITHUB_TOKEN` | Token for the `github-copilot` provider. |
| `AZURE_API_KEY` | API key for the `azure` provider. |
| `HF_TOKEN` | Token for the `huggingface` provider. |
| `NVIDIA_API_KEY` | API key for the `nvidia` provider. |
| `CLAURST_DISABLE_MODELS_FETCH` | When set, never refresh the models.dev catalog. |
| `CLAURST_MODELS_URL` / `MODELS_DEV_URL` | Catalog URL (default `https://models.dev/api.json`). |
| `CLAURST_MODELS_PATH` | Load the model catalog from this file instead of the cache. |
| `CLAURST_ENABLE_EXPERIMENTAL_MODELS` | `1` lists alpha and deprecated models in the picker. |
| `CLAURST_DISABLE_AUTO_MEMORY` | A truthy value turns off automatic memory. |
| `CLAURST_SKIP_PROMPT_HISTORY` | `1`, `true` or `yes`: do not record prompts in the prompt history. |
| `CLAURST_NO_HYPERLINKS` | `1`, `true`, `yes` or `on`: no OSC 8 hyperlinks in the TUI. |
| `CLAURST_VOICE_ENABLED` | `1` or `true`: start with voice input enabled. |
| `WHISPER_ENDPOINT_URL` | Speech-to-text endpoint for voice input (default OpenAI's transcription API). |
| `CLAURST_MCP_TOKENS_DIR` | Directory for stored MCP OAuth tokens (default `<config dir>/mcp-tokens`). |
| `CLAURST_SHARE_NO_OPEN` | Do not open the browser after `/share`. |
| `CLAUDE_STATUS_COMMAND` | Legacy status line command, used when `statusLine` is unset. |
| `CLAURST_BRIDGE_URL` | Enable the remote-control bridge by setting the server URL. |
| `CLAURST_BRIDGE_TOKEN` | Bearer token for the remote-control bridge (`CLAUDE_BRIDGE_OAUTH_TOKEN` is also read). |
| `RUST_LOG` | Tracing filter (e.g. `debug`, `claurst_core=trace`). In the interactive TUI, logs go to `<config dir>/logs/claurst.log` (rotated to `claurst.log.old` at 5 MB) so they never draw over the screen; in headless (`--print`) mode they go to stderr. `/doctor` shows the log file path. |

---

## Custom Slash Commands

User-defined slash commands can be added to the `commands` map:

```json
"commands": {
  "review": {
    "template": "Please review the following code for bugs and style: $ARGUMENTS",
    "description": "Review code",
    "agent": "plan",
    "model": null
  }
}
```

`CommandTemplate` fields:

| Field | Description |
|-------|-------------|
| `template` | Template string. `$ARGUMENTS` is replaced with whatever the user types after the command name, `$1` and `$2` with its first and second words. |
| `description` | Short description shown in `/help`. |
| `agent` | Parsed but not applied yet. |
| `model` | Parsed but not applied yet. |

Use the command with `/review path/to/file.rs`. A built-in command with the
same name takes precedence.

---

## Named Agents

Agents are named configurations that combine a system prompt prefix, model,
permission level, and turn limit. Three are built in:

| Agent | Access | Description |
|-------|--------|-------------|
| `build` | full | Read, write, and execute. For feature implementation. |
| `plan` | read-only | Read files; no writes or commands. For analysis and planning. |
| `explore` | search-only | Search and read. For rapid codebase exploration. |

You can define custom agents in `settings.json`:

```json
"agents": {
  "review": {
    "description": "Code review agent",
    "model": "anthropic/claude-haiku-4-5",
    "temperature": 0.3,
    "prompt": "You are a senior engineer doing code review. Be thorough and direct.",
    "access": "read-only",
    "visible": true,
    "max_turns": 30,
    "color": "magenta"
  }
}
```

`AgentDefinition` fields:

| Field | Type | Description |
|-------|------|-------------|
| `description` | string \| null | Description shown in `@agent` autocomplete. |
| `model` | string \| null | Model override for this agent. |
| `temperature` | float \| null | Sampling temperature override. |
| `prompt` | string \| null | System prompt prefix (prepended before the main system prompt). |
| `access` | string | Permission level: `"full"`, `"read-only"`, or `"search-only"`. |
| `visible` | boolean | Whether to show in autocomplete. Default: true. |
| `max_turns` | integer \| null | Maximum agentic turns. |
| `color` | string \| null | ANSI display color: `"cyan"`, `"magenta"`, `"green"`, `"yellow"`, etc. |

Invoke an agent with `@agentname` in the TUI or `--agent agentname` on the CLI.

---

## Managed Agents Configuration

The `managed_agents` key stores the managed-agents architecture configuration set via `/managed-agents configure`. The command writes it both at the top level and inside `config`; the copy inside `config` is the one read. It rarely needs to be edited manually.

```json
"managed_agents": {
  "enabled": true,
  "manager_model": "anthropic/claude-opus-4-6",
  "executor_model": "anthropic/claude-sonnet-4-6",
  "executor_max_turns": 20,
  "max_concurrent_executors": 3,
  "executor_isolation": true,
  "budget_split": {
    "type": "percentage",
    "manager_pct": 20
  },
  "total_budget_usd": 5.00,
  "preset_name": null
}
```

`executor_max_turns` defaults to 10 and `max_concurrent_executors` to 4.

`budget_split` types:

| Type | JSON | Description |
|------|------|-------------|
| `shared_pool` | `{ "type": "shared_pool" }` | All agents draw from a single pool (default) |
| `percentage` | `{ "type": "percentage", "manager_pct": 20 }` | Manager gets N% of total budget |
| `fixed_caps` | `{ "type": "fixed_caps", "manager_usd": 0.50, "executor_usd": 2.00 }` | Hard USD caps per role |

Configure via `/managed-agents configure` or `/managed-agents preset <name>`. Set `enabled: false` to disable without removing the configuration.

---

## File Formatters

Formatters run automatically after Claurst writes a file whose extension
matches. They are defined in the `formatter` map:

```json
"formatter": {
  "prettier": {
    "command": ["prettier", "--write"],
    "extensions": [".ts", ".tsx", ".js", ".json"],
    "disabled": false
  },
  "rustfmt": {
    "command": ["rustfmt"],
    "extensions": [".rs"],
    "disabled": false
  }
}
```

| Field | Description |
|-------|-------------|
| `command` | Command array. A `$FILE` or `{file}` argument is replaced with the file path; without one, the path is appended as the final argument. |
| `extensions` | File extensions this formatter handles (include the leading dot). |
| `disabled` | Set to true to temporarily disable without removing the entry. |

Only the first matching formatter runs. It has 30 seconds, and its failures
are ignored.

---

## Annotated Example `settings.json`

The comments below are explanations only. Remove them before using the
example as the global file, which must be plain JSON.

```json
{
  // Settings schema version
  "version": 1,

  // Active provider (can be overridden per-session with --provider)
  "provider": "anthropic",

  "config": {
    // Omit api_key here; use ANTHROPIC_API_KEY env var instead
    "api_key": null,

    // Model — leave null to use the provider's default
    "model": null,

    // Cap responses at 8 192 tokens
    "max_tokens": 8192,

    // Required key; --permission-mode decides the mode at startup
    "permission_mode": "default",

    // Dark theme for the TUI
    "theme": "dark",

    // Required keys; auto-compaction is controlled by --no-auto-compact
    "auto_compact": true,
    "compact_threshold": 0.9,

    // Required keys; --verbose and --output-format decide at startup
    "verbose": false,
    "output_format": "text",

    // Required keys
    "allowed_tools": [],
    "enable_all_mcp_servers": false,
    "disable_claude_mds": false,

    // Add a custom instruction to every session
    "append_system_prompt": "Always explain your reasoning before making changes.",

    // Block the Bash tool globally
    "disallowed_tools": ["Bash"],

    // Variables for remote sessions (/remote-env); not injected into local tools
    "env": {},

    // Run a script after every tool use (see docs/hooks.md)
    "hooks": {
      "PostToolUse": [
        { "hooks": [{ "type": "command", "command": "/home/user/scripts/audit-log.sh" }] }
      ]
    },

    // Connect an MCP server at startup
    "mcp_servers": [
      {
        "name": "filesystem",
        "command": "mcp-server-filesystem",
        "args": ["/home/user/projects"],
        "env": {},
        "type": "stdio"
      }
    ]
  },

  // Per-provider credentials and options
  "providers": {
    "anthropic": {
      "api_key": null,
      "enabled": true
    },
    "openai": {
      "api_key": "sk-...",
      "enabled": true
    },
    "ollama": {
      "api_base": "http://localhost:11434",
      "enabled": true
    }
  },

  // Correct metadata for self-hosted / unknown models (keyed by provider/model).
  // Overrides win over the models.dev catalog.
  "modelOverrides": {
    "custom-openai/my-local-llm": {
      "contextWindow": 32768,
      "maxOutputTokens": 4096,
      "name": "My Local LLM"
    }
  },

  // Custom slash commands
  "commands": {
    "test": {
      "template": "Run the tests for $ARGUMENTS and report any failures.",
      "description": "Run and report tests"
    }
  },

  // Auto-run prettier on JS/TS file writes
  "formatter": {
    "prettier": {
      "command": ["prettier", "--write"],
      "extensions": [".ts", ".tsx", ".js", ".jsx"],
      "disabled": false
    }
  }
}
```
