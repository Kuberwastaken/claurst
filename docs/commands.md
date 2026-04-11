# Claurst Slash Commands Reference

This document is the complete reference for every slash command available in Claurst, the Rust reimplementation of Claude Code CLI. Commands are invoked by typing `/command-name` at the REPL prompt.

---

## Table of Contents

1. [Command System Overview](#command-system-overview)
2. [Session & Navigation](#session--navigation)
3. [Model & Provider](#model--provider)
4. [Configuration & Settings](#configuration--settings)
5. [Code & Git](#code--git)
6. [Search & Files](#search--files)
7. [Memory & Context](#memory--context)
8. [Agents & Tasks](#agents--tasks)
9. [Planning & Review](#planning--review)
10. [MCP & Integrations](#mcp--integrations)
11. [Authentication](#authentication)
12. [Display & Terminal](#display--terminal)
13. [Diagnostics & Info](#diagnostics--info)
14. [Export & Sharing](#export--sharing)
15. [Advanced & Internal](#advanced--internal)
16. [Command Availability](#command-availability)

---

## Command System Overview

Commands are registered in a priority-ordered registry. When you type a command name, Claurst resolves it through this chain:

```
bundledSkills -> builtinPluginSkills -> skillDirCommands ->
workflowCommands -> pluginCommands -> pluginSkills -> COMMANDS()
```

### Command Types

| Type | Behavior |
|------|----------|
| `local` | Runs synchronously; returns text output directly |
| `local-jsx` | Renders an interactive TUI component (model picker, theme selector, etc.) |
| `prompt` | Expands to a prompt sent to the model via the main inference loop |

Commands support aliases — for example `/h`, `/?`, and `/help` all invoke the same handler.

### Usage Syntax

```
/command-name [arguments]
```

Arguments are passed as a single string after the command name. Most commands that accept arguments are documented with an `argumentHint` shown in the command palette.

---

## Session & Navigation

### /help
**Aliases:** `h`, `?`

Display all available commands with their descriptions. Respects `isHidden` flags — internal or rarely-needed commands are suppressed unless you are an Anthropic employee.

```
/help
/h
/?
```

---

### /clear
**Aliases:** `reset`, `new`

Clear the current conversation history and start a fresh session. The session file is retained on disk; only the in-memory message list is cleared.

```
/clear
```

---

### /exit
**Aliases:** `quit`

Exit the Claurst REPL. Equivalent to pressing `Ctrl+D`. Unsaved session state is flushed before exit.

```
/exit
/quit
```

---

### /resume
**Aliases:** `continue`

Resume a previous session from the session store. Displays a list of recent sessions with timestamps and summaries. Select one to restore its message history and file state.

```
/resume
/resume <session-id>
```

---

### /session
**Aliases:** `remote`

Manage active and stored sessions. Subcommands allow listing, switching, deleting, and attaching to remote sessions.

```
/session
/session list
/session delete <session-id>
/session attach <session-id>
```

---

### /fork

Fork the current session into a new independent session that begins from the current conversation state. Useful for exploring two different approaches without losing either.

```
/fork
/fork <new-session-name>
```

---

### /rename

Rename the current session. The new name is used in session listings and exports.

```
/rename <new-name>
```

---

### /rewind
**Aliases:** `checkpoint`

Rewind the conversation to a previous message. Displays a numbered list of messages; enter a number to truncate history to that point and resume from there.

```
/rewind
/rewind <message-index>
```

---

### /compact

Summarize and compress the conversation history to reduce context window usage. The model is asked to produce a dense summary of the prior exchange; that summary replaces the raw messages.

```
/compact
```

---

## Model & Provider

### /model

Open the interactive model picker. Displays a searchable list of available models from all configured providers. The selected model is used for all subsequent inference in the current session.

```
/model
/model claude-opus-4-5
/model claude-sonnet-4-6
```

---

### /providers

List all configured AI providers and their connection status. Shows provider name, base URL, and whether credentials are present.

```
/providers
```

---

### /connect

Connect to a remote AI provider or configure a custom provider endpoint. Supports OpenAI-compatible APIs, Anthropic direct, and others.

```
/connect
/connect <provider-name>
/connect openai https://api.openai.com/v1
```

---

### /thinking

Configure extended thinking for the current session. Extended thinking allows the model to reason through problems before responding, at the cost of additional tokens.

```
/thinking
/thinking on
/thinking off
```

See also `/effort` for a higher-level interface to thinking depth.

---

### /effort

Set the thinking effort level. This is a convenience wrapper over `/thinking` that maps human-readable levels to token budgets.

| Level | Description |
|-------|-------------|
| `low` | Minimal thinking; fastest responses |
| `medium` | Balanced thinking and speed |
| `high` | Deep reasoning; slower responses |
| `max` | Maximum token budget for thinking |

```
/effort low
/effort medium
/effort high
/effort max
```

---

## Configuration & Settings

### /config
**Aliases:** `settings`

View or modify Claurst configuration values. Without arguments, renders an interactive settings panel. With arguments, acts as a key-value accessor.

```
/config
/config get <key>
/config set <key> <value>
/config reset <key>
```

Common keys:

| Key | Description |
|-----|-------------|
| `model` | Default model name |
| `theme` | Color theme name |
| `vim` | Vim mode enabled (`true`/`false`) |
| `outputStyle` | Output rendering style |
| `autoApprove` | Auto-approve tool calls |

---

### /keybindings

Open the interactive keybinding configurator. Displays all bound actions with their current shortcuts. Select an action to rebind it. Changes are written to `~/.claude/keybindings.json`.

```
/keybindings
```

See [keybindings.md](./keybindings.md) for the full keybindings reference.

---

### /permissions
**Aliases:** `allowed-tools`

View and manage tool permission rules. Permissions control which tools can run without prompting, which are blocked, and which always require confirmation.

```
/permissions
/permissions list
/permissions allow <tool-name>
/permissions deny <tool-name>
/permissions reset
```

---

### /hooks

Manage event hooks. Hooks are shell commands or scripts that execute when lifecycle events fire (e.g., before/after tool calls, on session start/end).

```
/hooks
/hooks list
/hooks add <event> <command>
/hooks remove <hook-id>
```

Available events: `pre-tool`, `post-tool`, `session-start`, `session-end`, `message-send`, `message-receive`.

---

### /mcp

Configure and manage Model Context Protocol (MCP) servers. MCP servers expose additional tools and resources to the agent.

```
/mcp
/mcp list
/mcp add <name> <command>
/mcp remove <name>
/mcp restart <name>
```

---

### /output-style

Select how the model's output is rendered in the terminal. Choices include `auto`, `plain`, `markdown`, `streaming`, and others depending on terminal capabilities.

```
/output-style
/output-style plain
/output-style markdown
```

---

### /theme

Open the interactive theme picker. Preview and select a color theme for the Claurst TUI.

```
/theme
/theme dark
/theme light
/theme solarized
```

---

### /statusline

Configure the status line displayed at the bottom of the TUI. Toggle individual elements such as model name, token count, session name, and git branch.

```
/statusline
/statusline toggle model
/statusline toggle tokens
```

---

### /vim

Toggle vim keybinding mode on or off. In vim mode the input field behaves like a vim editor (normal/insert/visual modes). Persisted to config.

```
/vim
/vim on
/vim off
```

---

### /voice

Configure voice input/output. Requires a supported audio backend. Subcommands control microphone selection, TTS voice, and push-to-talk behavior.

```
/voice
/voice on
/voice off
/voice mic <device>
/voice tts <voice-name>
```

---

### /terminal-setup

Run the terminal capability detection and setup wizard. Checks for true-color support, font ligatures, Unicode rendering, and configures Claurst accordingly.

```
/terminal-setup
```

---

## Code & Git

### /commit

Stage and commit changes to the current git repository. The model drafts a commit message based on the diff. You can review and edit the message before confirming.

```
/commit
/commit "optional message override"
```

---

### /diff

Show file diffs for changes made during the current session. Displays a unified diff of all files Claurst has written or edited since the session started.

```
/diff
/diff <file-path>
```

---

### /undo

Undo file changes made during the current session. Restores files to their state before Claurst's last write operation. Can be called multiple times to step further back.

```
/undo
/undo <file-path>
```

---

### /review

Initiate a code review pass over recent changes. The model examines all modified files and produces inline comments and a summary of issues found.

```
/review
/review <file-path>
/review --since HEAD~3
```

---

### /security-review

Run a security-focused review pass. The model looks specifically for vulnerabilities, credential exposure, injection risks, and other security concerns in modified files.

```
/security-review
/security-review <file-path>
```

---

### /init

Initialize Claurst project configuration in the current directory. Creates a `CLAUDE.md` file that acts as persistent project-level context injected at the start of every session.

```
/init
```

---

### /search

Search the codebase using natural language or regex patterns. Wraps the GrepTool and GlobTool with a higher-level interface.

```
/search <query>
/search "TODO" --type ts
/search "function.*export" --regex
```

---

## Search & Files

### /files

List all files currently tracked (read or written) in the active session. Useful for reviewing what context the model has access to.

```
/files
/files --written
/files --read
```

---

### /context

Analyze context window usage. Shows a breakdown of tokens consumed by system prompt, conversation history, file contents, and tool results. Helps identify what to compact or drop.

```
/context
```

---

## Memory & Context

### /memory

Manage session memory. Memory entries are short notes persisted across sessions. The model can read these at session start to maintain continuity.

```
/memory
/memory list
/memory add <note>
/memory delete <id>
/memory clear
```

---

### /usage

Display a detailed token usage breakdown for the current session. Shows input tokens, output tokens, cache reads, cache writes, and estimated cost per API call.

```
/usage
```

---

### /cost

Show the total token usage and estimated cost for the current session. Provides a quick summary without the per-call breakdown of `/usage`.

```
/cost
```

---

### /stats

Display session statistics: number of messages, tool calls, files modified, tokens used, session duration, and model used.

```
/stats
```

---

### /status

Show the current session status. Includes active model, permission mode, thinking config, connected MCP servers, and loaded plugins.

```
/status
```

---

## Agents & Tasks

### /agents

Manage sub-agents. Sub-agents are parallel model instances that can be spawned to work on independent tasks simultaneously.

```
/agents
/agents list
/agents stop <agent-id>
/agents output <agent-id>
```

---

### /tasks
**Aliases:** `bashes`

Manage tracked background tasks. Tasks are shell commands or model invocations running asynchronously. Monitor progress, fetch output, or stop tasks from this interface.

```
/tasks
/tasks list
/tasks output <task-id>
/tasks stop <task-id>
```

---

## Planning & Review

### /plan

Enter plan mode (read-only). In plan mode the model can read files and reason about changes but cannot write, edit, or execute anything. Use this to draft an approach before allowing writes.

```
/plan
```

To exit plan mode, use `/plan off` or the `/exit-plan` internal action.

---

### /ultraplan

Extended planning mode with deeper reasoning. Like `/plan` but with an elevated thinking budget to allow more thorough analysis before acting.

```
/ultraplan
```

---

## MCP & Integrations

### /mcp

Documented above under [Configuration & Settings](#configuration--settings).

---

### /skills

List and manage skills. Skills are bundled prompt-commands that extend Claurst's capabilities without writing code. They appear alongside built-in commands in the registry.

```
/skills
/skills list
/skills enable <skill-name>
/skills disable <skill-name>
/skills reload
```

---

### /plugin
**Aliases:** `plugins`, `marketplace`

Manage plugins. Plugins are loadable modules that can register new commands, tools, and hooks. Browse the marketplace or install from a local path.

```
/plugin
/plugin list
/plugin install <name>
/plugin install <path>
/plugin remove <name>
/plugin reload
```

---

## Authentication

### /login

Authenticate with Anthropic via OAuth. Opens a browser window for the OAuth flow and stores the resulting credentials in the system keychain or `~/.claude/credentials`.

```
/login
```

---

### /logout

Clear stored authentication tokens. After logout, Claurst will prompt for credentials on next use.

```
/logout
```

---

### /refresh

Refresh the provider authentication state. Forces a token refresh without full re-authentication. Useful when a session token has expired mid-session.

```
/refresh
```

---

## Display & Terminal

### /theme

Documented above under [Configuration & Settings](#configuration--settings).

---

### /output-style

Documented above under [Configuration & Settings](#configuration--settings).

---

### /statusline

Documented above under [Configuration & Settings](#configuration--settings).

---

### /vim

Documented above under [Configuration & Settings](#configuration--settings).

---

### /terminal-setup

Documented above under [Code & Git](#code--git).

---

## Diagnostics & Info

### /doctor

Run the Claurst diagnostics suite. Checks configuration integrity, provider connectivity, tool availability, MCP server health, and reports any issues.

```
/doctor
```

---

### /version
**Aliases:** `v`

Display the current Claurst version string and build metadata.

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

## Export & Sharing

### /export

Export the current session transcript. Supported formats include Markdown, JSON, and plain text. The output is written to a file or printed to stdout.

```
/export
/export --format markdown
/export --format json --output session.json
/export --stdout
```

---

## Advanced & Internal

### /thinking

Documented above under [Model & Provider](#model--provider).

---

### /connect

Documented above under [Model & Provider](#model--provider).

---

### /fork

Documented above under [Session & Navigation](#session--navigation).

---

### /effort

Documented above under [Model & Provider](#model--provider).

---

### /summary

Generate a summary of the current session. The model produces a condensed description of what was accomplished. Primarily used internally for session metadata.

```
/summary
```

---

### /brief

Output a brief status message for use in non-interactive contexts. Renders minimal session info without the full TUI.

```
/brief
```

---

### /context

Documented above under [Search & Files](#search--files).

---

## Command Availability

Not all commands are available in all contexts.

### Remote Mode

When running with `--remote`, only a restricted set of commands is available:

`session`, `exit`, `clear`, `help`, `theme`, `vim`, `cost`, `usage`, `plan`, `keybindings`, `statusline`

### Bridge Mode

Over the Remote Control bridge (used by IDE integrations), only `local`-type commands are forwarded:

`compact`, `clear`, `cost`, `files`

### Internal-Only Commands

The following commands are only available when the `USER_TYPE` environment variable is set to `ant` (Anthropic internal builds):

`commit-push-pr`, `ctx_viz`, `good-claude`, `issue`, `init-verifiers`, `mock-limits`, `bridge-kick`, `ultraplan`, `summary`, `teleport`, `ant-trace`, `perf-issue`, `env`, `oauth-refresh`, `debug-tool-call`, `autofix-pr`, `bughunter`, `backfill-sessions`, `break-cache`

### Feature-Flagged Commands

Some commands check `isEnabled()` at runtime. For example, voice-related commands check for audio device availability; the desktop command checks for a display server.
