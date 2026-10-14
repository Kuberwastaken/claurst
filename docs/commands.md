# Claurst Slash Commands Reference

This document is the complete reference for every slash command available in Claurst, the Rust reimplementation of Claude Code CLI. Commands are invoked by typing `/command-name` at the REPL prompt.

---

## Table of Contents

1. [Command System Overview](#command-system-overview)
2. [Session & Navigation](#session--navigation) — `/help`, `/clear`, `/new`, `/move`, `/exit`, `/resume`, `/session`, `/fork`, `/branch`, `/tag`, `/rename`, `/rewind`, `/compact`
3. [Model & Provider](#model--provider) — `/model`, `/providers`, `/connect`, `/thinking`, `/effort`, `/advisor`, `/fast`, `/turns`, `/yolo`
4. [Configuration & Settings](#configuration--settings) — `/config`, `/keybindings`, `/permissions`, `/hooks`, `/import-config`, `/privacy-settings`, `/mcp`, `/output-style`, `/theme`, `/statusline`, `/cc-statusline`, `/vim`, `/voice`, `/terminal-setup`, `/add-dir`
5. [Code & Git](#code--git) — `/commit`, `/diff`, `/changes`, `/undo`, `/revert`, `/checkpoints`, `/snapshot`, `/review`, `/security-review`, `/init`, `/search`
6. [Search & Files](#search--files) — `/files`, `/context`, `/ctx-viz`
7. [Memory & Context](#memory--context) — `/memory`, `/usage`, `/extra-usage`, `/cost`, `/stats`, `/status`, `/insights`, `/rate-limit-options`
8. [Agents & Tasks](#agents--tasks) — `/agents`, `/tasks`, `/goal`, `/managed-agents`, `/agent`
9. [Planning & Review](#planning--review) — `/plan`, `/ultrareview`
10. [MCP & Integrations](#mcp--integrations) — `/mcp`, `/skills`, `ultracode`, `/plugin`, `/reload-plugins`, `/chrome`, `/ide`, `/pr-comments`, `/install-github-app`
11. [Authentication](#authentication) — `/login`, `/logout`, `/accounts`, `/switch`, `/refresh`
12. [Display & Terminal](#display--terminal) — `/caveman`, `/rocky`, `/normal`, `/mobile`, `/desktop`, `/color`, `/stickers`, `/passes`
13. [Diagnostics & Info](#diagnostics--info) — `/doctor`, `/heapdump`, `/version`, `/update`, `/release-notes`
14. [Export & Sharing](#export--sharing) — `/export`, `/copy`, `/share`, `/links`, `/teleport`
15. [Advanced & Internal](#advanced--internal) — `/summary`, `/btw`, `/sandbox-toggle`, `/think-back`, `/thinkback-play`, `/remote-control`, `/remote-env`, `/web-setup`, `/survey`
16. [Command Availability](#command-availability)

---

## Command System Overview

When you submit `/name args`, Claurst resolves `name` in this order:

1. Built-in commands (this page), by name or alias.
2. Custom command templates from the `commands` map in settings ([Custom Slash Commands](configuration.md#custom-slash-commands)).
3. Skills discovered on disk ([Skills](configuration.md#skills)).
4. Slash commands defined by plugins ([plugins.md](plugins.md)).

A built-in name always wins, so a template, skill or plugin command with the same name as a built-in is never reached.

In the TUI, some commands open a view instead of printing text (for example `/model`, `/theme`, `/config`, `/diff`, `/stats`, `/mcp`, `/memory`, `/export`). When a view opens, the command's text output is not shown. `/turns`, `/yolo`, `/cc-statusline`, `/changes`, `/survey` and `/find` exist only in the TUI. An unknown name shows `Unknown command: /<name>`.

Commands support aliases — for example `/h`, `/?`, and `/help` all invoke the same handler.

### Usage Syntax

```
/command-name [arguments]
```

Arguments are passed as a single string after the command name. Most commands that accept arguments are documented with an `argumentHint` shown in the command palette.

### Typing while a turn runs

The prompt stays editable while the model works.

| Key | Effect |
|-----|--------|
| `Enter` | Steer: the message is added to the running turn and the model sees it before its next request (after the current tool calls finish). If the turn ends first, it starts the next turn. |
| `Tab` | Queue: the message is sent as a new turn after this one ends. |
| `Alt+Up` | Pull the last pending message back into the (empty) prompt to edit it. |
| `Alt+X` | Drop the last pending message. |
| `Esc` / `Ctrl+C` | Cancel the turn: the model request and running tools stop, the output so far stays on screen, and steers not yet delivered and Tab-queued messages return to the prompt (nothing is sent automatically). Completion notices from background tasks are still delivered afterwards. |

Pending messages are listed above the prompt until delivered. Slash commands typed during a turn always wait for the turn to end. All keys can be rebound (`editQueued`, `removeQueued`, `cancelTurn`; see [keybindings.md](./keybindings.md)).

When a background sub-agent or a background Bash command started with `notify: true` finishes, its result is delivered the same way as a steer: at the next step of a running turn, or as a new turn when the session is idle. See `completionWakeups` in [configuration.md](./configuration.md).

---

## Session & Navigation

### /help
**Aliases:** `h`, `?`

In the TUI, opens the help overlay (also `F1`, or `?` on an empty prompt), with or without an argument. The text form lists every non-hidden command by category, and `/help <command>` prints that command's aliases and usage.

```
/help
/help compact
/h
/?
```

---

### /clear
**Aliases:** `reset`, `c`

Clear the current conversation history. The session id and its on-disk file are retained — only the in-memory message list is wiped, so you stay in the same session. Use `/new` to start a genuinely fresh session instead.

```
/clear
```

---

### /new

Start a fresh session, mirroring opencode's `/new`. The transcript resets to a blank home and a brand-new session id begins, while your current model, provider, effort level and working directory carry over. The new session is *lazy* — it is not written to disk until your first message, so opening `/new` without typing anything leaves no trace.

Unlike `/clear` (which keeps the same session id and history file), `/new` opens a clean, separate session.

```
/new
```

---

### /move

Re-home the current session to another worktree or directory of the **same project**, mirroring opencode's `/move`. Any uncommitted changes in the current directory are carried over to the destination and reset in the original location; the model is informed of the new working directory on its next turn.

The destination must belong to the same git repository (typically a linked `git worktree`); moving to an unrelated project is refused. Pass `--no-changes` to re-home the session without relocating working-tree changes.

```
/move <directory>
/move ../myapp-feature
/move --no-changes /path/to/other/worktree
```

**Adaptation note:** opencode presents an interactive worktree picker and can create a new worktree on the fly. Claurst takes the destination directory as an argument and re-homes the live session's working directory (claurst has no separate session-per-worktree registry). Uncommitted changes are relocated with `git diff`/`git apply` and the source is reset with `git checkout` (index preserved) plus `git clean` (untracked removed), matching opencode's `move-session` change handling.

---

### /exit
**Aliases:** `quit`, `q`

Exit Claurst. Pressing `Ctrl+C` or `Ctrl+D` (on an empty prompt) twice within two seconds also exits.

```
/exit
/quit
```

---

### /resume
**Aliases:** `r`, `continue`

Load a saved session: its messages, model and working directory replace the current ones. In the TUI, `/resume` without an argument opens the session browser; outside the TUI it loads the most recent session. `/resume <session-id>` loads that session directly.

```
/resume
/resume <session-id>
```

---

### /session
**Aliases:** `remote`

In the TUI, `/session` opens the session browser. The text form shows the current session and the five most recent ones, or the remote session URL when the remote-control bridge is connected. `/session list` prints the ten most recent sessions. There are no other subcommands; switch with `/resume <id>`.

```
/session
/session list
```

---

### /fork

Save the conversation up to message index N (default: all of it) as a new session titled "Fork of ...", with the current session as its parent. The current session stays active; switch with `/resume <id>`.

```
/fork
/fork 5
```

---

### /branch

Create a branch of the current session at this point, list the current session's branches, or switch to one.

```
/branch
/branch create
/branch list
/branch switch <session-id>
```

---

### /tag

Add, remove or toggle searchable tags on the current session.

```
/tag list
/tag add <tag>
/tag remove <tag>
/tag toggle <tag>
```

---

### /rename

Set the session title. Without a name, a short kebab-case title is generated from the conversation with the provider's fast model. In the TUI, `/rename` also opens the session browser in rename mode.

```
/rename <new-name>
/rename
```

---

### /rewind

Open a picker over the conversation's messages (`Up`/`Down`, `Enter` to select, `y` to confirm). The selected message and everything after it are removed from the conversation and the saved session. The cut never separates a tool call from its result. Files are not restored; use `/undo` or `/revert` for that. Takes no arguments.

```
/rewind
```

---

### /compact

Summarise the older part of the conversation now, through the active provider, keeping the most recent messages and your last few requests verbatim. Optional instructions steer what the summary keeps. `PreCompact` hooks run first. Reports the approximate token count before and after. See [Manual compaction](advanced.md#manual-compaction).

```
/compact
/compact keep the list of failing tests
```

---

## Model & Provider

### /model

Without arguments, opens the model picker for the current provider (or the connect dialog when no provider has credentials). With a model ID, switches to it; a `provider/model` ID also switches the provider.

```
/model
/model claude-sonnet-4-6
/model openai/gpt-4o
```

---

### /providers

List the providers in the model registry with their model counts, context windows and pricing.

```
/providers
```

---

### /connect

Open the provider picker to connect and authenticate a provider. Takes no arguments.

```
/connect
```

---

### /thinking
**Aliases:** `think`

Print whether the current model supports extended thinking. It changes no setting and takes no arguments; use `/effort` to control reasoning depth.

```
/thinking
```

---

### /effort

Set the reasoning effort level. Without arguments, opens the effort picker; the levels offered depend on the provider and model (from `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, and `ultracode`). With an argument the level is applied (the TUI picker still opens; `normal` is accepted as `medium`). Only `low`, `normal` and `high` adjust the max output tokens (4096 / default / 32768).

```
/effort
/effort low
/effort high
/effort max
```

---

### /advisor

Store or clear an advisor model name. The value is written to `advisorModel` in `~/.claurst/settings.json`; nothing in the current build reads it, so it does not affect requests.

```
/advisor                          — show current advisor setting
/advisor claude-opus-4-6          — set advisor model by name
/advisor provider/model           — set advisor using provider/model format
/advisor off                      — disable the advisor
/advisor unset                    — disable the advisor
```

The advisor model persists to `~/.claurst/settings.json` under `advisorModel`. Model IDs must start with `claude-` or contain a `/` (provider/model format).

---

### /fast
**Aliases:** `speed`

Toggle fast mode. On switches the model to the active provider's fast model; off clears the model override, returning to the default model.

```
/fast          — toggle fast mode on/off
/fast on       — enable fast mode
/fast off      — disable fast mode
```

The on/off state is saved to `~/.claurst/ui-settings.json`.

---

### /turns

Set or disable the max turn limit for the current session. The override takes precedence over both the agent definition (e.g. the `plan` agent's built-in cap) and the `max_turns` config value. `/turns off` is not unlimited: it sets a ceiling of 1000 turns so a runaway loop still stops. `/turns 0` is rejected. Session-only: it is not persisted and resets on relaunch.

```
/turns         — show the current cap (override or config/agent default)
/turns 25      — cap the loop at 25 turns
/turns off     — lift the cap to a ceiling of 1000 turns
```

### /yolo

Toggle YOLO mode. In YOLO mode the effective permission mode is
`bypassPermissions`: all tool-permission prompts are bypassed. Every
`/yolo on` shows the bypass-permissions security dialog before activating;
acceptance activates it for the session only (nothing is written to
settings), declining cancels the toggle.

```
/yolo          — toggle (shows dialog when turning on)
/yolo on       — show the security dialog, then activate
/yolo off      — restore the permission mode active before /yolo on
```

YOLO mode can also be enabled persistently via `"yoloMode": true` in the
`config` block of the global settings file. That field is global-only: a
project settings file cannot enable it. Same root/sudo guard as
`--dangerously-skip-permissions` applies.

---

## Configuration & Settings

### /config
**Aliases:** `settings`

In the TUI, opens the settings screen. The text form prints the effective configuration as JSON and supports these keys, saved to `~/.claurst/settings.json`:

```
/config
/config get <theme|output-style|model|permission-mode>
/config set theme <default|dark|light|...>
/config set output-style <name>
/config set model <model>
/config set permission-mode <default|accept-edits|bypass-permissions|plan>
/config unset <model|output-style>
```

---

### /keybindings

Open `~/.claurst/keybindings.json` with the system's default application for the file (not `$EDITOR`), creating it from the default bindings if it does not exist. Edits take effect the next time Claurst starts.

```
/keybindings
```

See [keybindings.md](./keybindings.md) for the full keybindings reference.

---

### /permissions

View and manage tool permission rules. Permissions control which tools can run without prompting, which are blocked, and which always require confirmation. `/permissions` (or `list`) shows the mode and every rule in effect: configured, saved for this project, saved everywhere, and this session's.

```
/permissions
/permissions list
/permissions set <default|accept-edits|bypass-permissions|plan>
/permissions allow <rule>
/permissions deny <rule>
/permissions reset
```

A rule is `Tool`, `Tool(pattern)` or `*`, e.g. `Bash(git status *)` or `Edit(/repo/src/**)`. Bash patterns match each command of a compound line. Deny rules apply in every mode and a tool-wide deny hides the tool from the model. See [Permission Rules and Approvals](configuration.md#permission-rules-and-approvals).

---

### /hooks

Browse the hooks that will run, read-only: event, matcher, command, timeout and source (settings or a plugin). Edit `~/.claurst/settings.json` to change them; see [Hooks](hooks.md).

```
/hooks
```

Events: `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `SubagentStop`, `SessionStart`, `SessionEnd`, `Notification`, `PreCompact`, `PostModelTurn`.

---

### /import-config

Open a dialog that previews and imports `~/.claude/CLAUDE.md` and `~/.claude/settings.json` into Claurst's configuration.

```
/import-config
```

---

### /privacy-settings

Open `https://claude.ai/settings/data-privacy-controls` in the browser, or print the URL if no browser can be launched.

```
/privacy-settings
```

---

### /mcp

Inspect MCP servers. In the TUI, `/mcp` without arguments opens the MCP panel. Servers are added and removed by editing `mcpServers` in `~/.claurst/settings.json`; there are no add, remove or restart subcommands. See [The /mcp Command](mcp.md#the-mcp-command).

```
/mcp                                     — configured servers with live status
/mcp list                                — same
/mcp status                              — type, endpoint and state of each server
/mcp tools [server]                      — tools exposed by connected servers
/mcp auth <server>                       — run the OAuth flow for an HTTP/SSE server, then reconnect it
/mcp connect <server>                    — report a server's state (a failed server needs /mcp auth or a restart)
/mcp logs <server>                       — recent errors for a server
/mcp resources [server]                  — resources from connected servers
/mcp prompts [server]                    — prompt templates from connected servers
/mcp get-prompt <server> <prompt> [key=value ...]
```

---

### /output-style

Show or switch the output style (the response persona added to the system prompt). Without arguments, lists the available styles and marks the current one; with a name, switches and saves it to settings. Built-in styles: `default`, `concise`, `explanatory`, `learning`, `caveman`, `rocky`; plugin and user styles are listed too. In the TUI the command also cycles a status-bar label (`auto`, `stream`, `verbose`) and the text listing is not shown.

```
/output-style
/output-style concise
/output-style default
```

---

### /theme

Without arguments, opens the theme picker (Default, Dark, Light, Solarized, Nord, Dracula, Monokai, Deuteranopia). `/theme <name>` sets the theme and saves it to settings.

```
/theme
/theme dark
/theme light
/theme solarized
```

---

### /statusline

Save which items (`cost`, `tokens`, `model`, `time`, or `all`) the status line should show, in `~/.claurst/ui-settings.json`. The built-in status bar does not read these settings in the current build, so they have no visible effect. For a custom status line command, see `/cc-statusline`.

```
/statusline
/statusline show cost
/statusline hide tokens
/statusline show all
```

---

### /cc-statusline

**Aliases:** `cc-status-line`

Show the external `statusLine` command configured in `~/.claurst/settings.json` (command, poll interval, padding). See [External status line](configuration.md#external-status-line).

```
/cc-statusline
```

---

### /vim
**Aliases:** `vi`

Toggle vim mode in the prompt for this session; `/vim on` and `/vim off` set it explicitly. The choice is also written to `~/.claurst/ui-settings.json`, which is not read at startup. See [Vim Mode](keybindings.md#vim-mode) for current limitations.

```
/vim
/vim on
/vim off
```

---

### /voice

Toggle voice input. When on, `Alt+V` starts recording and `Alt+V` again stops and transcribes into the prompt. Transcription uses a Whisper-compatible API: `OPENAI_API_KEY` (or `ANTHROPIC_API_KEY` as a fallback key), and `WHISPER_ENDPOINT_URL` to use a local server. `/voice` toggles voice mode for this session; `/voice on|off` only save the startup state (effective next start); `/voice status` shows it. `CLAURST_VOICE_ENABLED=1` also enables it at startup. There is no text-to-speech.

```
/voice
/voice on
/voice off
/voice status
```

---

### /terminal-setup

Print a report on the terminal (name, color depth, UTF-8 locale and related checks) with recommendations. It changes no settings.

```
/terminal-setup
```

---

### /add-dir

Add a directory to the allowed workspace paths and save it to settings.

```
/add-dir <path>
```

---

## Code & Git

### /commit

Ask the model to commit the currently staged changes: it reads `git diff --cached`, writes a message following the repository's conventions and runs `git commit`. Text after the command is passed as the message to use. Nothing is staged for you and there is no separate confirmation step.

```
/commit
/commit fix login redirect
```

---

### /diff

In the TUI, opens the diff viewer on the project's git working tree. The text form runs `git diff` and accepts `--stat`, `--staged` (or `--cached`), or a ref to diff against. For changes made by individual assistant turns, use `/changes`.

```
/diff
/diff --stat
/diff --staged
/diff main
```

---

### /changes

TUI only. Open the diff viewer on the file changes recorded for each assistant turn in this session.

```
/changes
```

---

### /undo

Revert all file changes made during the most recent assistant turn; same as `/revert` with no argument. Takes no arguments.

```
/undo
```

---

### /revert

Restore the files changed during an assistant turn from the shadow-git snapshot and move the transcript's active leaf to just before that turn; the reverted turn and later turns are kept on a sibling branch. Requires git.

```
/revert           — the most recent turn
/revert 2         — the second most recent turn
/revert abc123    — the turn whose message id starts with abc123
```

---

### /checkpoints

List the assistant turns in this session that changed files.

```
/checkpoints
```

---

### /snapshot

Show the shadow-git diff for an assistant turn.

```
/snapshot             — the most recent turn
/snapshot 2           — the second most recent turn
/snapshot <hash>      — a snapshot tree hash (8+ hex characters)
```

---

### /review

Send a diff to the model for a structured review: `git diff <base>...HEAD` when a base ref is given, otherwise the staged diff (falling back to unstaged changes). When `GITHUB_TOKEN` is set and a PR number is known (`CLAUDE_PR_NUMBER`, or detected from git), the review is also posted as a PR comment. In the TUI, `/review` also opens the diff viewer and the review text is not shown.

```
/review
/review main
/review origin/main
```

---

### /security-review

Ask the model for a security review of a path (default: the working directory), covering injection, auth flaws, secrets, deserialization, path traversal, cryptography, dependencies, race conditions and information disclosure, with a severity per finding.

```
/security-review
/security-review <path>
```

---

### /init

Create `AGENTS.md` in the working directory from a short template, unless it already exists. It does not analyse the project. See [Instruction Files](configuration.md#instruction-files-agentsmd--claudemd).

```
/init
```

---

### /search

In the TUI, opens the project search dialog, which runs ripgrep over the project (`/find` and `Ctrl+P` open the same dialog). The text form, `/search <query>`, searches saved session titles and messages in `~/.claurst/sessions.db`; its output is not shown in the TUI.

```
/search
/search <query>
```

---

## Search & Files

### /files

List the file paths mentioned in the conversation text that exist on disk. Takes no arguments.

```
/files
```

---

### /context

In the TUI, toggles the context-usage overlay. The text form estimates the tokens used by the conversation against a fixed 200k-token window.

```
/context
```

---

### /ctx-viz
**Aliases:** `ctx`, `context-visualizer`

Print an estimated breakdown of context usage: system prompt, conversation messages and tool results against a 200k-token window.

```
/ctx-viz
```

---

## Memory & Context

### /memory

Work with the `AGENTS.md` memory files: `<project>/.claurst/AGENTS.md`, `<project>/AGENTS.md` and the global `~/.claurst/AGENTS.md`. In the TUI, `/memory` opens a picker of these files. The text form:

```
/memory                — show all AGENTS.md files
/memory edit           — open the project AGENTS.md in your editor
/memory edit global    — open ~/.claurst/AGENTS.md in your editor
/memory clear          — clear the project AGENTS.md
/memory clear global   — clear ~/.claurst/AGENTS.md
```

---

### /usage

Show this session's token usage and cost, plus account quota and rate-limit information when available.

```
/usage
```

---

### /extra-usage

Show extended usage statistics: API call count, cache hit ratio, token breakdown by type and cost per call.

```
/extra-usage
```

---

### /cost

Show the session's token usage and estimated cost. In the TUI, opens the stats dialog (same as `/stats`).

```
/cost
```

---

### /stats

In the TUI, opens the stats dialog. The text form prints the model, user and assistant turn counts, tool calls, token usage (with cache reads and writes) and estimated cost. For totals across saved sessions, run `claurst stats` from the shell.

```
/stats
```

---

### /status

Show authentication, model, permission mode, fast mode, editor mode, session id and title, message count, working directory, git branch, the number of configured MCP servers and hooks, and usage so far.

```
/status
```

---

### /insights

Generate an analytical report of the current session. Prints a structured breakdown of conversation statistics including turn count, token usage (input/output/total), average tokens per exchange, estimated cost, total tool calls, and the most frequently invoked tool.

```
/insights
```

Sample output:
```
Session Insights
──────────────────────────────────────
Conversation
├─ User turns          : 12
├─ Assistant turns     : 12
└─ Completed exchanges : 12

Tokens
├─ Input               : 48320
├─ Output              : 9140
├─ Total               : 57460
└─ Avg per exchange    : 4788

Cost
└─ Estimated USD       : $0.1823

Tools
├─ Total calls         : 34
└─ Most used           : Bash (18 calls)
```

---

### /rate-limit-options

Show the account's plan type and rate-limit tier information.

```
/rate-limit-options
```

---

## Agents & Tasks

### /agents

Open the agents menu: agent definitions and running agents. `/agents list` prints the definitions (visible outside the TUI menu). See [The /agent and /agents Commands](agents.md#the-agent-and-agents-commands).

```
/agents
```

---

### /tasks
**Aliases:** `bashes`

Ask the model to list the current tasks with the `TaskList` tool. Takes no arguments. `Ctrl+T` opens the tasks overlay.

```
/tasks
```

---

### /goal

Set a durable multi-turn autonomous goal. When a goal is active, Claurst continues working across turns until the goal is marked complete, paused, or a 200-turn runaway guard fires. Designed for complex, sustained tasks that would otherwise require repeated manual re-prompting.

```
/goal <objective>                    — set a new goal and begin working autonomously
/goal --tokens 250K <objective>      — set a goal with a soft token budget cap
/goal                                — show current goal status
/goal status                         — show current goal status
/goal pause                          — pause the active goal
/goal resume                         — resume a paused goal
/goal clear                          — delete the current goal
/goal complete                       — request a completion audit
```

When the model believes the goal has been achieved, it calls the `GoalComplete` tool with an audit summary and evidence. Goals can be disabled globally by setting `CLAURST_GOALS=0` in your environment.

See [Goal System](./advanced.md#goal-system) in the advanced guide.

---

### /managed-agents

Configure the manager-executor agent architecture, where a manager model delegates subtasks to one or more executor agents working in parallel. Includes budget controls and isolation options.

```
/managed-agents                                       — show current configuration
/managed-agents status                                — show current configuration
/managed-agents presets                               — list built-in presets
/managed-agents preset <name>                         — apply a named preset
/managed-agents setup                                 — show setup instructions
/managed-agents enable                                — enable managed agents
/managed-agents disable                               — disable managed agents
/managed-agents reset                                 — remove all managed-agent configuration
/managed-agents configure manager-model <model>       — set the manager model
/managed-agents configure executor-model <model>      — set the executor model
/managed-agents configure executor-turns <n>          — set executor max turns
/managed-agents configure concurrent <n>              — set max concurrent executors
/managed-agents configure isolation on|off            — toggle executor isolation
/managed-agents configure budget-split shared         — shared token pool
/managed-agents configure budget-split percentage:<n> — percentage split (manager gets n%)
/managed-agents configure budget-split fixed:<m>:<e>  — fixed USD caps (manager / executor)
/managed-agents budget <amount>                       — set total budget in USD (0 to clear)
```

Model format: `provider/model` (e.g., `anthropic/claude-opus-4-6`, `openai/gpt-4o`). Configuration persists to `~/.claurst/settings.json` under `managed_agents`.

> **Preview feature.** Behaviour may change across releases.

See [Managed Agents](./advanced.md#managed-agents) in the advanced guide.

---

### /agent

List all available named agents, or show details for a specific agent. Named agents are predefined configurations with their own system prompts, model bindings, and access levels. Useful for discovering what agents are available before starting a session.

```
/agent             — list all visible named agents with access levels
/agent <name>      — show full details for a specific named agent
```

To activate an agent, start Claurst with `--agent <name>`. See [agents.md](./agents.md) for defining custom agents.

---

## Planning & Review

### /plan

Toggle plan mode (permission mode `plan`) and send a message asking the model for a step-by-step plan for the description (default: "the current task") without running commands or writing files. Every `/plan` sends that message, including the one that turns plan mode off; `Shift+Tab` leaves plan mode without sending anything. `/plan exit` toggles the same way but sends an exit note instead. See [Plan mode](advanced.md#plan-mode).

```
/plan
/plan add rate limiting to the API client
/plan exit
```

`/ultraplan` is not a slash command. `claurst ultraplan [--effort=medium|high|maximum]` prints a placeholder message and does not run a planner.

---

### /ultrareview

Run an exhaustive multi-dimensional code review over the current working directory or a specified path. Goes significantly beyond `/review` and `/security-review`, covering:

- **Security** — OWASP Top 10, injection vulnerabilities, cryptographic weaknesses, path traversal, race conditions, dependency risks
- **Performance** — algorithmic complexity, allocations, N+1 queries, blocking I/O, memory leaks
- **Maintainability** — function length, nesting depth, DRY violations, naming, dead code
- **Error handling** — swallowed errors, panic paths, missing input validation
- **Test coverage** — missing tests, brittle tests, missing edge cases
- **API design, documentation, accessibility, and architecture**

The command sends this request to the model as a prompt for a path (default: the working directory). Each finding is tagged by category and severity.

```
/ultrareview
/ultrareview <path>
```

---

## MCP & Integrations

### /mcp

Documented above under [Configuration & Settings](#configuration--settings).

---

### /skills
**Aliases:** `skill`

List the skills available to this session (discovered and bundled) with each skill's description and source file. Skills loaded from disk can be run as `/<name> args`; the model loads any listed skill with the `Skill` tool. See [Skills](configuration.md#skills).

```
/skills
```

---

### ultracode (top effort + keyword)

Run a disciplined **ultracode** workflow for serious coding tasks. Ultracode is claurst's take on Claude Code's `ultrathink`: a supervised procedure that classifies the task, picks a mode, and — when it genuinely helps — delegates bounded work across claurst's native agent primitives, then integrates and verifies in the parent session.

Ultracode is the **highest effort level** — it sits past `max` on the "Smarter" end of the effort ladder and runs the model's top reasoning **plus** the workflow procedure. (It is no longer a `/skill`.) There are two ways to trigger it:

- **In the effort selector.** Run `/effort` and pick **ultracode** — the rightmost level, past the `│` divider, drawn with an animated purple spectrum. Applies for subsequent turns until you change the effort.
- **As a keyword.** Type the single word `ultracode` anywhere in a normal prompt. The keyword renders with a purple gradient in the input, and for that turn the effort is set to ultracode (its operating procedure is injected as a system-prompt addendum). No keyword means no change to normal prompts.

```
please ultracode <task>    — activate ultracode for this one turn via the inline keyword
/effort  →  ultracode      — set ultracode as the current effort level
```

**What it does**

1. **Classify** the task by type, risk, blast radius, verification needs, and whether independent packets exist.
2. **Pick a mode** — *Direct* (small, tightly-coupled work), *Workflow* (multi-phase work executed as isolated passes), or *Delegated* (the default for non-trivial work with independent packets).
3. **Delegate** in delegated mode using native primitives: `Agent` for bounded subagents (with `isolation: "worktree"` / `run_in_background: true`), `TeamCreate` for parallel swarms, and `TaskCreate` for background tasks. It fans out **2–4** subagents (cap ~5) on non-overlapping packets while the parent keeps the blocking critical path.
4. **Integrate** every result in the parent, checking claimed edits against the files and rejecting evidence-free outputs.
5. **Verify** with checks scaled to risk (targeted tests → lint/typecheck → build → smoke → independent review), reporting any skipped checks honestly.

**Composes with `/goal`.** Ultracode governs *how* a turn plans, delegates, integrates, and verifies; `/goal <objective>` keeps the work going *across* turns. Combine them for long, autonomous objectives — the goal loop spans turns while ultracode structures each one.

---

### /plugin
**Aliases:** `plugins`

Manage installed plugins. There is no marketplace and no remove subcommand. See [plugins.md](plugins.md).

```
/plugin                  — list installed plugins
/plugin list
/plugin info <name>
/plugin enable <name>    — saved to settings
/plugin disable <name>   — saved to settings
/plugin install <path>   — install from a local directory
/plugin reload
```

---

### /reload-plugins

Reload all plugins from disk, re-register their hooks and show what changed.

```
/reload-plugins
```

---

### /chrome

Browser automation via Chrome DevTools Protocol (CDP). Connects to a running Chrome or Chromium instance and lets Claurst control it — navigate pages, click elements, fill forms, evaluate JavaScript, and take screenshots.

First, launch Chrome with remote debugging enabled:

```bash
chrome --remote-debugging-port=9222 --no-first-run
```

Then:

```
/chrome connect [--port 9222]      — connect to Chrome on the given port (default: 9222)
/chrome navigate <url>             — navigate to a URL
/chrome screenshot                 — take a screenshot, saved to a temp file
/chrome click <selector>           — click a CSS selector
/chrome fill <selector> <text>     — fill an input field
/chrome eval <js>                  — evaluate JavaScript and return the result
/chrome disconnect                 — disconnect from Chrome
```

Useful for testing web applications, scraping, or automating browser-based workflows without a separate browser-automation tool.

---

### /ide

Show the detected IDE and the IDE connections found in `~/.claurst/ide/*.lock`.

```
/ide
```

---

### /pr-comments

Fetch the review comments of the current branch's GitHub pull request with the `gh` CLI.

```
/pr-comments
```

---

### /install-github-app

Print the steps to set up the GitHub app and Actions workflow for a repository.

```
/install-github-app
```

---

## Authentication

Claurst supports **multiple named accounts per provider** — Anthropic (Claude.ai or Console) and Codex (OpenAI ChatGPT subscription). Each login creates a profile under `~/.claurst/accounts/<provider>/<id>/` and the registry at `~/.claurst/accounts.json` tracks which one is active.

See [Authentication Guide](./auth.md#multi-account-profiles) for the full story and on-disk layout.

### /login

Authenticate with Anthropic or Codex via OAuth PKCE. Opens a browser for the flow and saves tokens under the active profile (or creates a new profile if none exists).

```
/login                            — Claude.ai OAuth (Bearer token, default)
/login --console                  — Console OAuth (creates an API key)
/login --codex                    — Codex / ChatGPT OAuth
/login --label work               — name the new profile "work"
/login --codex --label personal   — Codex login, name the profile "personal"
```

If a profile matching the JWT's email or account_id already exists, that profile is refreshed in place — re-logging-in is idempotent. Use `--label` to either name a fresh profile or to disambiguate.

---

### /logout

Remove credentials. By default removes only the **active** profile for the provider; other stored profiles remain switchable.

```
/logout                — clear active Anthropic profile (drops it from registry)
/logout --codex        — clear active Codex profile
/logout --all          — purge every Anthropic profile + clear any API key in settings
/logout --codex --all  — purge every Codex profile
```

---

### /accounts

List every stored account across providers. The active profile in each provider is marked with `*`.

```
/accounts
```

Sample output:

```
Anthropic:
  * personal [pro]    kuber@personal.example
    work     [max]    kuber@company.example
Codex:
    work              kuber@company.example
```

---

### /switch

Switch the active account for a provider. Anthropic by default; pass `--codex` for Codex. Run `/accounts` first to see available profile ids.

```
/switch work                     — set active Anthropic profile to "work"
/switch --codex personal         — set active Codex profile to "personal"
```

---

### /refresh

Delete saved provider credentials (the auth store and OAuth token file), the model caches, and the saved provider, model and API key in settings, then rebuild the provider state. Run `/connect` afterwards to authenticate again. Refused while a response is streaming. Takes no arguments.

```
/refresh
```

---

## Display & Terminal

`/theme`, `/output-style`, `/statusline`, `/vim` and `/terminal-setup` are documented under [Configuration & Settings](#configuration--settings).

---

### /caveman

Switch the output style to the `caveman` persona (terse, few words) and save it to settings; same as `/output-style caveman`. Takes no arguments. Typing the word `caveman` in a prompt applies it for that turn only. Deactivate with `/normal`.

```
/caveman
```

---

### /rocky

Switch the output style to the `rocky` persona (the Eridian engineer from *Project Hail Mary*) and save it to settings; same as `/output-style rocky`. Takes no arguments. Typing the word `rocky` in a prompt applies it for that turn only. Deactivate with `/normal`.

```
/rocky
```

---

### /normal

Reset the output style to `default` and save it; same as `/output-style default`. Typing `normal` in a prompt resets for that turn only.

```
/normal
```

---

### /mobile

Display a QR code and download links for the Claude mobile app. Supports a `session` subcommand that generates a QR code linking directly to an active remote Claurst session.

```
/mobile             — show QR code for claude.ai/mobile (works for both platforms)
/mobile ios         — show QR code for the iOS App Store
/mobile android     — show QR code for Google Play
/mobile session     — show QR code linking to the active remote session (requires a connected remote-control bridge)
```

---

### /desktop

Show download and setup information for the Claude desktop app for the current platform.

```
/desktop
```

---

### /color

Save a prompt bar color to `~/.claurst/ui-settings.json`. Accepts `red`, `green`, `blue`, `yellow`, `cyan`, `magenta`, `white`, `orange`, `purple`, `pink`, `gray`/`grey`, `#RGB` or `#RRGGBB`. The TUI does not read this setting in the current build, so the color does not change. `/color-set` is a hidden internal variant.

```
/color               — show the saved color
/color <name>
/color #ff6b6b
/color default       — clear the saved color
```

---

### /stickers

Opens the Claurst sticker page (`stickermule.com/claudecode`) in your default browser. Falls back to printing the URL if no browser can be launched.

```
/stickers
```

---

### /passes

Print where to get a referral link (`claude.ai/passes`).

```
/passes
```

---

## Diagnostics & Info

### /doctor

Run diagnostics: API key validation (a live models request), git availability, MCP server status, disk space, config file integrity, a tool permission summary, the Claurst version and the log file path.

```
/doctor
```

---

### /heapdump

Show process diagnostics: process ID, platform, architecture and memory figures (RSS and peak from `/proc/self/status` on Linux).

```
/heapdump
```

---

### /version
**Aliases:** `v`

Print the Claurst version.

```
/version
/v
```

---

### /update
**Aliases:** `upgrade`

Check for available updates. Queries the GitHub releases API and displays the latest version. If a newer version exists, prints the download URL or upgrade instructions. Does not auto-update.

```
/update
/upgrade
```

---

### /release-notes

Fetch release notes from GitHub for the current version or a given one.

```
/release-notes
/release-notes 0.2.0
```

---

## Export & Sharing

### /export

Export the conversation as JSON (default) or Markdown. In the TUI, `/export` opens the export dialog. The text form writes to `--output <file>` (a bare file name also works; a `.md` name implies Markdown) or prints the export.

```
/export
/export --format markdown
/export --format json --output session.json
/export conversation.md
```

---

### /copy

Copy the most recent assistant response to the system clipboard. Pass a number to copy the Nth most-recent response. On Linux a `wl-clipboard` or `xclip` backend is used; on macOS and Windows the native clipboard API is used.

```
/copy         — copy the most recent response
/copy 2       — copy the second most recent response
/copy N       — copy the Nth most recent response
```

---

### /share

Render the session as a single HTML file, upload it as a secret GitHub gist with the `gh` CLI, and print a viewer URL (`https://claurst.kuber.studio/session/#<gist-id>`). Requires `gh auth login`. `CLAURST_SHARE_VIEWER_URL` overrides the viewer base URL. Anyone with the link can read a secret gist.

```
/share
```

---

### /links
**Aliases:** `link`

Open URLs found in the session's messages in your browser.

```
/links          — open the most recent URL
/links list     — numbered list, most recent first
/links <N>      — open the Nth URL from the list
/links last     — same as /links
```

---

### /teleport

Move session context between machines as a JSON bundle.

```
/teleport export [--output <file>]   — default ~/.claurst/teleport_<session_id>.json
/teleport import <file>              — restore messages, working directory and tool permissions
/teleport link                       — print a teleport:// deep link (base64 bundle)
```

---

## Advanced & Internal

### /summary

Ask the model for a 3-5 sentence summary of the conversation so far. The request and the answer become part of the conversation.

```
/summary
```

---

### /btw

Send a side question to the model. The question is sent as a normal user message with a `[/btw side-question ...]` prefix, so it and the answer are part of the conversation history.

```
/btw <question>
```

---

### /sandbox-toggle
**Aliases:** `sandbox`

Show or change the OS sandbox for Bash commands. On macOS, Bash commands run under Seatbelt (`/usr/bin/sandbox-exec`) unless the sandbox is off. Changes are saved to the `sandbox` block of the global settings file and apply to the next command. Other platforms have no OS sandbox yet: the setting is kept but has no effect. See [Shell Sandbox](configuration.md#shell-sandbox).

```
/sandbox-toggle                          — toggle between off and workspace-write
/sandbox-toggle status                   — show the current mode and network setting
/sandbox-toggle on | workspace-write     — writes only inside the workspace, --add-dir and temp dirs (default)
/sandbox-toggle read-only                — no writes
/sandbox-toggle off                      — run commands unsandboxed
/sandbox-toggle network on|off           — network access for sandboxed commands (default off)
```

---

### /think-back
**Aliases:** `thinkback`

Display the extended-thinking traces from previous model responses in the current session. Only available when extended thinking was used for those responses. Pass a number to view the Nth most-recent trace.

```
/think-back         — show the most recent thinking trace
/think-back 2       — show the second most recent thinking trace
/thinkback          — alias
```

Thinking traces appear when the model uses extended thinking mode (see `/thinking`). If no traces are found, Claurst suggests enabling extended thinking.

---

### /thinkback-play

Replay a previous extended-thinking trace as a formatted, step-numbered walkthrough. Useful for reviewing the model’s reasoning path in detail.

```
/thinkback-play         — replay the most recent thinking trace
/thinkback-play 2       — replay the second most recent thinking trace
```

---

### /remote-control
**Aliases:** `rc`

Show the remote-control bridge status. `start` and `stop` set `remoteControlAtStartup` in settings; the change applies on the next start. The bridge uses `CLAURST_BRIDGE_URL` and `CLAURST_BRIDGE_TOKEN`.

```
/remote-control
/remote-control status
/remote-control start
/remote-control stop
```

---

### /remote-env

List, set or unset the variables in the settings `env` map that are forwarded to remote sessions. Values of keys containing `KEY`, `TOKEN`, `SECRET` or `PASSWORD` are masked in the listing.

```
/remote-env
/remote-env set <KEY> <VALUE>
/remote-env unset <KEY>
```

---

### /web-setup
**Aliases:** `remote-setup`

Check what a remote Claurst environment needs (provider credentials, network reachability) and print setup steps.

```
/web-setup
```

---

### /survey

TUI only. Open the session feedback survey.

```
/survey
```

---

## Command Availability

All commands listed here are available in every interactive session; there are no remote-only, bridge-only or internal-only command sets.

| Command | Condition |
|---------|-----------|
| `/goal` | Disabled when `CLAURST_GOALS=0` |
| `/color-set` | Hidden from `/help`; use `/color` |
