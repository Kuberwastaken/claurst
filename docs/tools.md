# Claurst Tools Reference

This document is the complete reference for every tool available to the Claurst agent. Tools are the mechanism by which the model interacts with the outside world — reading files, running commands, searching the web, and coordinating sub-agents.

---

## Table of Contents

1. [Tool System Overview](#tool-system-overview)
2. [Permission System](#permission-system)
3. [File Tools](#file-tools)
4. [Shell Execution Tools](#shell-execution-tools)
5. [Search Tools](#search-tools)
6. [Web Tools](#web-tools)
7. [Task Management Tools](#task-management-tools)
8. [MCP Integration Tools](#mcp-integration-tools)
9. [Agent Tools](#agent-tools)
10. [Notebook Tools](#notebook-tools)
11. [Planning Tools](#planning-tools)
12. [Worktree Tools](#worktree-tools)
13. [Utility Tools](#utility-tools)
14. [Cron Tools](#cron-tools)
15. [Code Intelligence Tools](#code-intelligence-tools)
16. [Advanced Tools](#advanced-tools)
17. [Tool Framework Internals](#tool-framework-internals)

---

## Tool System Overview

Every tool implements the Rust `Tool` trait (`src-rust/crates/tools/src/lib.rs`):

- `name()` — the name the model calls (see [Tool Names](#tool-names))
- `description()` and `input_schema()` — a description and a JSON Schema object sent to the model
- `permission_level()` — see [Permission Levels](#permission-levels)
- `self_gates()` — `true` when the tool asks for permission itself inside `execute()`
- `execute()` — runs the call and returns a `ToolResult` (text, an error flag, optional metadata and images)

The tool set is built once per session: every tool returned by `all_tools()`, plus `Agent`, plus one tool per tool of each connected MCP server, plus `StructuredOutput` when `--json-schema` is given. A named agent with `read-only` or `search-only` access narrows the set (see [Agents](agents.md)). Every tool is sent with every request, except tools hidden by a tool-wide deny rule and the final max-turns summary turn (which gets no tools); there is no deferred or on-demand tool loading.

### Tool Names

Section headings below use the implementation type names; the model sees these names:

| Tool names | Section |
|------------|---------|
| `Read`, `Write`, `Edit`, `BatchEdit`, `ApplyPatch` | [File Tools](#file-tools) |
| `Bash`, `monitor`, `PowerShell`, `REPL` | [Shell Execution Tools](#shell-execution-tools) |
| `Glob`, `Grep`, `ToolSearch` | [Search Tools](#search-tools) |
| `WebFetch`, `WebSearch` | [Web Tools](#web-tools) |
| `TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate`, `TaskStop`, `TaskOutput`, `TodoWrite` | [Task Management Tools](#task-management-tools) |
| `ListMcpResources`, `ReadMcpResource`, `mcp__auth`, `mcp__<server>__<tool>` | [MCP Integration Tools](#mcp-integration-tools) |
| `Agent`, `SendMessage`, `TeamCreate`, `TeamDelete`, `RemoteTrigger`, `Skill`, `GoalComplete` | [Agent Tools](#agent-tools) |
| `NotebookEdit` | [Notebook Tools](#notebook-tools) |
| `EnterPlanMode`, `ExitPlanMode` | [Planning Tools](#planning-tools) |
| `EnterWorktree`, `ExitWorktree` | [Worktree Tools](#worktree-tools) |
| `AskUserQuestion`, `Brief`, `Sleep`, `Config` | [Utility Tools](#utility-tools) |
| `CronCreate`, `CronDelete`, `CronList` | [Cron Tools](#cron-tools) |
| `LSP` | [Code Intelligence Tools](#code-intelligence-tools) |
| `computer` (only in builds with the `computer-use` feature), `StructuredOutput` (only with `--json-schema`) | [Advanced Tools](#advanced-tools) |

### Tool Concurrency

When one response contains several tool calls, their PreToolUse hooks run one at a time, then every call that was not blocked runs concurrently and the results are returned in the original order. `Write`, `Edit`, `BatchEdit`, `ApplyPatch` and `NotebookEdit` lock each file they change for the whole call, so two calls on the same file do not interleave.

---

## Permission System

### Permission Levels

Each tool declares a permission level:

| Level | Meaning | Examples |
|-------|---------|----------|
| **None** | No permission check | `Sleep`, `TodoWrite`, `Task*` (except `TaskStop`), `AskUserQuestion`, `Agent` |
| **ReadOnly** | Reads files or the network | `Read`, `Glob`, `Grep`, `WebFetch`, `WebSearch`, `LSP` |
| **Write** | Changes files or settings | `Write`, `Edit`, `BatchEdit`, `ApplyPatch`, `NotebookEdit`, `Config`, `TeamCreate` |
| **Execute** | Runs code or processes | `Bash`, `PowerShell`, `REPL`, `CronCreate`, `TaskStop`, MCP server tools |
| **Dangerous** | Desktop control | `computer` |
| **Forbidden** | Never runs | `Bash` commands the classifier rates Critical (e.g. `rm -rf /`)* |

\* No tool declares this level; the refusal happens inside `Bash`.

A tool that does not check permissions itself is checked before it runs when its level is anything other than `None` or `ReadOnly`. File tools, `Bash`, `PowerShell`, `REPL`, `computer` and MCP tools check inside the call, with the path or command they are about to touch.

### Permission Modes

Set with `--permission-mode` (`default`, `accept-edits`, `bypass-permissions`, `plan`) or `permission_mode` in settings:

| Mode | Behavior |
|------|----------|
| `default` | Reads (`Read`, `Glob`, `Grep`, `LSP`, `Skill`) inside the workspace run without a prompt. Write/Execute/Dangerous tools ask, as do reads outside the workspace and `ListMcpResources`/`ReadMcpResource`; `None`-level tools, `WebSearch` and `monitor` never prompt |
| `plan` | `Read`, `Glob`, `Grep`, `LSP`, `Skill`, `ListMcpResources` and `ReadMcpResource` run anywhere without a prompt; every other tool is denied |
| `acceptEdits` | `Edit`, `Write`, `NotebookEdit`, `BatchEdit` and `ApplyPatch` run without a prompt for targets inside the workspace; everything else behaves as in `default` |
| `bypassPermissions` | Every tool runs without a prompt unless a deny rule covers it. `--dangerously-skip-permissions` and `yoloMode` select it; both refuse to start as root |

Bash commands rated Critical by the classifier are refused in every mode. In headless (`--print`) runs nobody can answer a prompt, so a call that would ask is rejected with the reason returned to the model.

### Permission Rules

Rules come from `allowed_tools` / `disallowed_tools` (and `--allowed-tools` / `--disallowed-tools`, `/permissions allow|deny`), from the dialog's "always allow in this project" choice (stored outside the repository) and from its "always allow everywhere" choice (stored in the global settings file under `permissionRules`). Bash rules are matched against each command of a compound line. The full model is in [Permission Rules and Approvals](configuration.md#permission-rules-and-approvals). A stored rule has:

- `tool_name` — a tool name; omit it to match every tool. `mcp__<server>` or `mcp__<server>__*` matches every tool of that MCP server
- `path_pattern` — optional glob matched against the call's path (file tools) or command (`Bash`)
- `action` — `Allow` or `Deny`

Order does not matter: if any deny rule matches, the call is denied (in every mode, `bypassPermissions` included); otherwise, if any allow rule matches, it is allowed; otherwise the mode decides.

### No Read-Before-Write Check

`Write`, `Edit`, `BatchEdit` and `ApplyPatch` do not require the file to have been read first. Every change they make is recorded per turn, so `/undo` and `/revert` can restore the previous content.

---

## File Tools

### FileReadTool

**Permission level:** ReadOnly

Read the contents of a file from the local filesystem. Returns file contents as a string. Supports optional line range to read a subset of a large file.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | yes | Absolute path to the file |
| `offset` | integer | no | First line to read (1-indexed) |
| `limit` | integer | no | Maximum number of lines to read (default 2000, must be at least 1) |

Returns text only, prefixed with line numbers. Each call returns at most 2000 lines and about 50 KB of text; lines longer than 2000 bytes are cut. When more of the file remains, the result ends with `[truncated: N more lines; call Read with offset=M to continue]`. Images (PNG, JPG, GIF, BMP, WEBP, ICO) and PDFs are not decoded: the result only notes that the file exists and that its content was not seen. SVG is read as text. Binary files are rejected.

---

### FileWriteTool

**Permission level:** Write

Write content to a file. Creates the file and any missing parent directories. Overwrites existing files entirely.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | yes | Absolute path to write |
| `content` | string | yes | Full file content |

The file does not need to have been read first. The previous content is recorded for `/undo` and `/revert`.

---

### FileEditTool

**Permission level:** Write

Perform an exact string replacement within an existing file. Fails if `old_string` is not found or is not unique. Prefer this tool over `FileWriteTool` when making targeted edits, as it only transmits the diff rather than the entire file.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `file_path` | string | yes | Absolute path to the file |
| `old_string` | string | yes | Exact text to replace |
| `new_string` | string | yes | Replacement text |
| `replace_all` | boolean | no | Replace all occurrences (default: false) |

Whitespace and indentation must match exactly.

---

### BatchEditTool

**Permission level:** Write

Apply multiple `FileEditTool`-style edits in a single tool call. More efficient than calling `FileEditTool` repeatedly when making many changes to the same file or across multiple files.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `edits` | array | yes | Array of `{file_path, old_string, new_string}` objects; each `old_string` must occur exactly once |
| `description` | string | no | Human-readable description of the batch |

Edits within the same file are applied in order. Every edit is validated before anything is written; if one fails (string not found, not unique), the batch is rejected and no file changes. If a write fails partway, files already written are rolled back.

---

### ApplyPatchTool

**Permission level:** Write

Apply a patch to one or more files. Two formats are accepted, detected from the first line:

- **Envelope** (the format OpenAI's Codex models are trained on), starting with `*** Begin Patch`.
- **Unified diff**, as produced by `git diff` or `diff -u`.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `patch` | string | yes | Envelope patch or unified diff |
| `dry_run` | boolean | no | Validate the patch and report what it would change without writing (default: false) |

**Envelope format.**

```
*** Begin Patch
*** Add File: docs/new.md
+first line of the new file
*** Update File: src/app.py
*** Move to: src/main.py
@@ def main():
-    print("hi")
+    print("hello")
*** Delete File: old.txt
*** End Patch
```

- `*** Add File: <path>` creates (or overwrites) a file; every following line starts with `+`.
- `*** Delete File: <path>` deletes a file.
- `*** Update File: <path>` edits a file, optionally renaming it with `*** Move to: <path>` on the next line. Its changes are `@@` sections of ` ` context, `-` removed and `+` added lines. `@@ <line>` first finds that anchor line (a class or function header, for example) and looks for the section after it. Sections are searched in order from where the previous one ended. `*** End of File` after a section means its lines end the file. An update always leaves a trailing newline.
- A section's lines are matched exactly first, then ignoring trailing whitespace, then ignoring leading and trailing whitespace, then with typographic dashes, quotes and spaces folded to ASCII. A non-exact match that fits more than one place is an error; add context lines or an `@@` anchor so it matches once.
- Each line keeps its own line ending (CRLF, LF or CR); inserted lines use the file's first line ending. A UTF-8 BOM is kept.
- On success the result lists the files as `A` (added), `M` (modified or moved) and `D` (deleted).

**Unified diffs.** Each hunk's body is delimited by the line counts in its `@@ -l,s +l,s @@` header, as in `git apply`, so removed lines that start with `--`, added lines that start with `++`, and empty context lines whose leading space was stripped all stay in the hunk. A header whose counts do not match its lines is an error. `--- /dev/null` creates the file (it must not exist) and `+++ /dev/null` deletes it (its hunks must remove every line). Later hunks are located from their header line shifted by the earlier hunks' net change.

**Writes and permissions.** Every change is computed before anything is written; if one fails, no file is touched. Each path the patch writes, including a move destination, gets the same permission check as `FileEditTool` on that path. All paths are locked for the whole call, in the same sorted order as the other file-editing tools, so concurrent edits cannot interleave or deadlock. Updates and deletes go through the file handle that was checked, so a symlink swapped in afterwards cannot redirect them.

**Codex models.** With the `codex` provider, gpt-5 and later models get this tool as `apply_patch`, a freeform tool constrained by the official Lark grammar, as the official Codex client sends it. Their raw patch text arrives as the `patch` input. Other providers and models see the JSON tool above.

**Shell heredocs.** A `BashTool` command that is only `apply_patch <<'EOF'` followed by an envelope patch and `EOF` (or `applypatch`, with `<<EOF` or `<<"EOF"`, optionally after `cd <dir> &&`) is applied by this tool from the shell's current directory instead of being run, with the same per-path permission checks. A command that does anything else runs normally.

---

## Shell Execution Tools

### BashTool

**Permission level:** Execute

**Tool name:** `Bash`

Execute a shell command in bash. The working directory (`cd`) and exported variables persist between calls within a session. There is one shell tool: it runs in a pseudo-terminal, described under [PtyBashTool](#ptybashtool) (output bounding, spill files, child environment). On Windows it runs `cmd /C` without a PTY.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes | Shell command to execute |
| `description` | string | no | Shown to the user as the approval question |
| `timeout` | integer | no | Timeout in milliseconds (default 120000, max 600000) |
| `run_in_background` | boolean | no | Run asynchronously and return a task ID immediately |
| `notify` | boolean | no | With `run_in_background`: deliver the finished command's output to the session (`completionWakeups`) |

Commands the bash classifier rates Critical (e.g. `rm -rf /`, fork bombs) are refused in every permission mode.

When `run_in_background` is `true`, the command runs with `bash -c` in the shell's current directory and the task ID is returned immediately. Use the `monitor` tool to check status, retrieve output, or cancel the task.

---

### MonitorTool

**Tool name:** `monitor`

**Permission level:** ReadOnly

Monitor background tasks started with `Bash`'s `run_in_background=true` or `Agent`'s `run_in_background=true`. Supports listing all tasks, checking the status or output of a specific task, and cancelling a running task.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `action` | string | no | `list` (default), `status`, `output`, or `cancel` |
| `task_id` | string | no | Task ID to inspect or cancel. Required for `status`, `output`, `cancel` |

**Actions:**

| Action | Effect |
|--------|--------|
| `list` | Lists all background tasks with their IDs, status, and names |
| `status` | Shows the status and metadata for a specific task |
| `output` | Retrieves the stdout/stderr output collected so far |
| `cancel` | Sends a termination signal to a running task |

Task statuses: `running`, `completed`, `failed: <reason>`, `cancelled`.

---

### PtyBashTool

**Permission level:** Execute

Execute a command in a pseudo-terminal (PTY) so programs that check `isatty()` behave as in a terminal. There is no stdin writer, so interactive programs cannot be driven. Pagers are disabled (`PAGER`, `GIT_PAGER`, `GH_PAGER` = `cat`, `GIT_TERMINAL_PROMPT=0`) and `CLAURST=1` / `AGENT=1` are set. Output larger than 2000 lines / 50 KiB at either end is cut to its head and tail; the full output is saved to a private file under `<claurst home>/tool-output/`, named in a trailing `Full output: <path>` line, and readable without a permission prompt. Files older than 7 days are deleted.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes | Command to run in PTY |
| `timeout` | integer | no | Timeout in milliseconds |

#### Sandbox (macOS)

On macOS the shell runs under Seatbelt unless `sandbox.mode` is `off`:
`/usr/bin/sandbox-exec -p <profile> -DWRITABLE_ROOT_n=<root> ... -- bash -c
<command>`. The default `workspace-write` profile reads anything, writes only
beneath the workspace roots, `--add-dir` directories, `$TMPDIR` and `/tmp`
(with `.git` and `.claurst` kept read-only), and has no network unless
`sandbox.network` is `true`; `read-only` allows no writes. Background commands
use the same profile.

A failed command whose output looks like a sandbox denial (`Operation not
permitted`, `Permission denied`, `Read-only file system`, ...) opens a
**Sandbox Denied** dialog in the TUI: rerun outside the sandbox once, for the
session, or always for that command's first word. A rerun starts from the
shell state the denied run started from. Headless and ACP runs return the
failure with a `[sandbox]` note instead. See
[Shell Sandbox](configuration.md#shell-sandbox) for the settings.

---

### PowerShellTool

**Permission level:** Execute

**Tool name:** `PowerShell`

Execute a PowerShell command with `-NoProfile -NonInteractive -Command`. Uses `powershell` (Windows PowerShell) on Windows and `pwsh` elsewhere. Commands are classified by risk first: Critical commands are refused in every mode; High-risk commands get the permission check with a risk explanation; Medium-risk commands are checked in `default` and `plan` mode or when `require_confirmation` is set.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes | PowerShell command to execute |
| `description` | string | no | Description of what the command does |
| `timeout` | integer | no | Timeout in milliseconds (default 120000, max 600000) |
| `require_confirmation` | boolean | no | Also ask for Medium-risk commands |

The tool is registered on every platform; without `pwsh` installed it fails to start the shell.

---

### ReplTool

**Permission level:** Execute

**Tool name:** `REPL`

Keep an interpreter process alive per session and language: `bash --norc --noprofile` (default), `python3 -u -i`, or `node`. Variables, imports, and definitions persist between calls. The code cannot call other tools.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `code` | string | yes | Code to evaluate |
| `language` | string | no | `bash` (default), `python`, or `javascript` |

Only stdout is returned; stderr (including Python and Node exception text) is not. A call fails if no output line arrives for 30 seconds. It asks for permission as an Execute tool under its own name (Bash rules do not cover it); code rated Critical is refused.

---

## Search Tools

### GlobTool

**Permission level:** ReadOnly

Find files matching a glob pattern. Searches from a specified directory (defaults to the current working directory). Returns matching file paths sorted by modification time, most recent first.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `pattern` | string | yes | Glob pattern (e.g., `**/*.rs`, `src/**/*.ts`) |
| `path` | string | no | Directory to search from |

---

### GrepTool

**Permission level:** ReadOnly

Search file contents with a Rust `regex` pattern (not ripgrep). Directories are walked directly: hidden directories, `node_modules`, `target` and `__pycache__` are skipped, and `.gitignore` is not read. Output modes: matching lines with context, file paths only (the default, `files_with_matches`), or match counts.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `pattern` | string | yes | Regular expression pattern |
| `path` | string | no | Directory or file to search |
| `glob` | string | no | File glob filter (e.g., `*.rs`) |
| `type` | string | no | File type filter (e.g., `rust`, `py`, `js`) |
| `output_mode` | string | no | `content`, `files_with_matches`, or `count` |
| `-i` | boolean | no | Case-insensitive search |
| `-n` | boolean | no | Show line numbers |
| `context` | integer | no | Lines of context around each match |
| `multiline` | boolean | no | Enable multiline matching |
| `head_limit` | integer | no | Limit output lines (default 250) |

---

### ToolSearchTool

**Permission level:** None

Search available tools by name or keyword. Returns matching tool names with their descriptions, not parameter schemas; every tool's schema is already sent with each request, so nothing is loaded by this tool. `select:Name[,Other]` looks up exact names. The catalog covers the built-in tools and every tool of every connected MCP server (listed as `mcp__<server>__<tool>`, matched on its name, server and description).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | string | yes | Keywords, a task description, or `select:Name` |
| `max_results` | integer | no | Maximum results (default 5, max 20) |

---

## Web Tools

### WebFetchTool

**Permission level:** ReadOnly

Fetch a URL with a GET request (30 s timeout, up to 10 redirects). HTML is converted to plain text (for pages where stripping leaves little text, the model is asked to extract the main content); any other response body is returned as text, so PDFs and other binary formats are not decoded. Results longer than 100,000 characters are truncated. A non-2xx status is an error.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `url` | string | yes | URL to fetch |
| `prompt` | string | no | Accepted but not used |

Network requests are subject to the host's firewall and proxy settings.

---

### WebSearchTool

**Permission level:** ReadOnly

Perform a web search and return a list of results with titles, URLs, and snippets.

The backend is selected by environment, in priority order:

1. **SearXNG** — set `SEARXNG_URL` to a self-hosted instance's base URL (its `settings.yml` must have the JSON `format` enabled).
2. **Brave Search** — set `BRAVE_SEARCH_API_KEY`.
3. **Serply** — set `SERPLY_API_KEY` (keys at https://serply.io, API reference at https://serply.io/docs). Returns at most 10 results per request.
4. **DuckDuckGo** — no-config fallback used when none of the above is set.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | string | yes | Search query |
| `num_results` | integer | no | Number of results to return (default 5, max 10) |

---

## Task Management Tools

The `Task*` tools keep an in-memory list of work items for the current process (shared by sub-agents, lost on exit). They do not start or run anything; background commands and agents are tracked separately and inspected with the [`monitor`](#monitortool) tool.

### TaskCreateTool

**Tool name:** `TaskCreate` · **Permission level:** None

Create a work item with status `pending`. Returns `task_id`.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `subject` | string | yes | Brief title |
| `description` | string | yes | What needs to be done |
| `metadata` | object | no | Arbitrary metadata |

---

### TaskGetTool

**Tool name:** `TaskGet` · **Permission level:** None

Return the full record of a task (subject, description, status, owner, blocks, blocked_by, metadata, output, timestamps), or `null` if it does not exist.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `task_id` | string | yes | Task identifier |

---

### TaskListTool

**Tool name:** `TaskList` · **Permission level:** None

List tasks (id, subject, status, owner, blocked_by). Deleted tasks are never listed; completed tasks only when requested.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `include_completed` | boolean | no | Include completed tasks (default false) |

---

### TaskUpdateTool

**Tool name:** `TaskUpdate` · **Permission level:** None

Update fields of a task. Setting `status` to `deleted` removes it.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `task_id` | string | yes | Task identifier |
| `subject` | string | no | New subject |
| `description` | string | no | New description |
| `status` | string | no | `pending`, `in_progress`, `completed`, `deleted`, or `failed` |
| `owner` | string | no | Owner name |
| `addBlocks` | array | no | Task IDs this task blocks |
| `addBlockedBy` | array | no | Task IDs blocking this task |
| `metadata` | object | no | Replaces the metadata |
| `output` | string | no | Output text to record |

---

### TaskStopTool

**Tool name:** `TaskStop` · **Permission level:** Execute

Mark a `running` or `in_progress` task as `completed`. No process is signalled; to stop a background command or agent use `monitor` with `action: "cancel"`.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `task_id` | string | yes | Task identifier (`shell_id` is accepted as an alias) |

---

### TaskOutputTool

**Tool name:** `TaskOutput` · **Permission level:** None

Return the task record with a `retrieval_status` of `success`, or `not_ready` for a task that is `Running` or `InProgress` when `block` is false. It does not wait.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `task_id` | string | yes | Task identifier |
| `block` | boolean | no | Default true; only changes the reported `retrieval_status` |

---

### TodoWriteTool

**Tool name:** `TodoWrite` · **Permission level:** None

Replace the session TODO list with the given list. It is saved to `~/.claurst/todos/<session_id>.json` and shown in the TUI. From the third turn on, while items are not `completed`, a reminder is appended to the system prompt.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `todos` | array | yes | The complete list of `{id, content, status, priority}` objects; `id`, `content` and `status` are required |

Status values: `pending`, `in_progress`, `completed`. `priority` is a free-form string.

---

## MCP Integration Tools

Model Context Protocol (MCP) tools bridge Claurst to external MCP servers. See [MCP](mcp.md) for server configuration.

### MCP server tools

**Tool name:** `mcp__<server>__<tool>` · **Permission level:** Execute

Each tool of each connected server is registered as its own tool, with the server's description and input schema. Every call goes through the permission check (a `mcp__<server>` rule covers all of a server's tools). See [MCP tool names](mcp.md#mcp-tool-names).

---

### ListMcpResourcesTool

**Tool name:** `ListMcpResources` · **Permission level:** ReadOnly

List resources exposed by connected MCP servers. Asks for permission in `default` mode.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `server` | string | no | Only list this server's resources |

Returns a list of resource URIs with descriptions.

---

### ReadMcpResourceTool

**Tool name:** `ReadMcpResource` · **Permission level:** ReadOnly

Read the content of a specific resource from an MCP server. Asks for permission in `default` mode.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `server` | string | yes | MCP server name |
| `uri` | string | yes | Resource URI to read |

---

### McpAuthTool

**Tool name:** `mcp__auth` · **Permission level:** None (the call itself asks for permission)

Run the OAuth 2.0 + PKCE flow for an MCP server that requires authentication and store the token locally. For a server with no `url` (stdio) the call fails with an error that points to `/mcp auth <server>`.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `server_name` | string | yes | MCP server name |

---

## Agent Tools

Agent tools enable multi-agent coordination: spawning sub-agents, forming teams, and passing messages between them.

### AgentTool

**Tool name:** `Agent` · **Permission level:** None (the sub-agent's own tool calls are checked as usual)

Run a sub-agent with its own agentic loop and return its final result. Registered in the interactive, headless and ACP front ends; see [Agents](agents.md).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `description` | string | yes | Short (3-5 word) description of the task |
| `prompt` | string | yes | The complete task for the sub-agent |
| `tools` | array | no | Tool names to make available (default: all) |
| `system_prompt` | string | no | System prompt override |
| `max_turns` | integer | no | Turn limit (default 10, or the managed-agents executor setting) |
| `model` | string | no | Model override |
| `isolation` | string | no | `worktree` runs the sub-agent in its own git worktree |
| `run_in_background` | boolean | no | Return an `agent_id` immediately; inspect it with `monitor` |

---

### SendMessageTool

**Tool name:** `SendMessage` · **Permission level:** None

Queue a message for another agent by name or session ID, or for every existing inbox with `to: "*"`. Messages are held in an in-process inbox; nothing in the current query loop reads that inbox, so recipients do not see them.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `to` | string | yes | Recipient agent name or session ID, or `*` |
| `message` | string | yes | Message content |
| `summary` | string | no | Short preview for the UI |

---

### TeamCreateTool

**Tool name:** `TeamCreate` · **Permission level:** Write

Create a named team, write its config under `~/.claurst/teams/<team_name>/`, and run one sub-agent per entry in `agents` on the shared task. Agents run in parallel unless `parallel` is false; their outputs are aggregated into the result.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `team_name` | string | yes | Team name (a suffix is added if it already exists) |
| `task` | string | yes | Shared task for all agents |
| `agents` | array | no | `{name, role?, tools?, task?}` objects; `tools` restricts the agent's tools, `task` overrides the shared task |
| `parallel` | boolean | no | Run agents in parallel (default true) |
| `description` | string | no | Team description stored in the config |

---

### TeamDeleteTool

**Tool name:** `TeamDelete` · **Permission level:** Write

Signal a team's running agents to stop and remove `~/.claurst/teams/<team_name>/`.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `team_name` | string | yes | Team to delete |

---

### RemoteTriggerTool

**Tool name:** `RemoteTrigger` · **Permission level:** None

POST a named event to `https://api.claude.ai/api/sessions/<session_id>/trigger`. The request carries no authentication token. Not related to the [Cron Tools](#cron-tools).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `session_id` | string | yes | Target session ID |
| `event_name` | string | yes | Event name (e.g. `task_complete`) |
| `payload` | object | no | JSON payload delivered with the event |

---

### SkillTool

**Permission level:** Read-only

Load a skill's instructions by name. The available skills are listed to the model in the system prompt (`<available_skills>`); `skill="list"` enumerates them. The body is returned wrapped in `<skill_content name="..." base_dir="...">` (no `base_dir` for bundled skills), with `$ARGUMENTS` replaced by `args`. See [Skills](configuration.md#skills) for where skills are discovered.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `skill` | string | yes | Skill name, or `list` |
| `args` | string | no | Arguments; replaces `$ARGUMENTS` in the skill |

---

### GoalCompleteTool

**Permission level:** None

**Tool name:** `GoalComplete`

Mark the session's active goal (set with `/goal`) as complete. The tool is always registered; with no active goal the call still reports success (nothing is updated). Its description tells the model to call it only after a completion audit.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `audit_summary` | string | yes | Concise summary of the goal-completion audit |
| `evidence` | string | yes | Specific evidence demonstrating the goal was achieved (files changed, tests passed, output produced, etc.) |

A successful call sets the goal's status to complete in the goal store and returns the audit summary and evidence. An empty `audit_summary` or `evidence` is rejected.

See also: `/goal complete` command.

---

## Notebook Tools

### NotebookEditTool

**Permission level:** Write

**Tool name:** `NotebookEdit`

Edit a Jupyter notebook (`.ipynb`) by replacing, inserting, or deleting cells. Operates on the notebook's JSON structure directly.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `notebook_path` | string | yes | Absolute path to the notebook |
| `cell_id` | string | no | Cell ID or `cell-N` index; required for `replace` and `delete`. `insert` adds after it, or at the start when omitted |
| `new_source` | string | no | New cell source; required for `replace` and `insert` |
| `cell_type` | string | no | `code` (default) or `markdown`, for `insert` |
| `edit_mode` | string | no | `replace` (default), `insert`, or `delete` |

---

## Planning Tools

### EnterPlanModeTool

**Tool name:** `EnterPlanMode` · **Permission level:** None

Return a message saying plan mode was entered. The call does not change the permission mode; the session stays in its current mode. Plan mode itself is set with `/plan` or `--permission-mode plan` (see [Permission Modes](#permission-modes)).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `reason` | string | no | Why plan mode is wanted |

---

### ExitPlanModeTool

**Tool name:** `ExitPlanMode` · **Permission level:** None

Return a message saying plan mode was exited, with the optional plan summary. Like `EnterPlanMode`, it does not change the permission mode.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `summary` | string | no | Summary of the plan |

---

## Worktree Tools

Worktree tools manage git worktrees, enabling the agent to work on multiple branches simultaneously in isolated directories.

### EnterWorktreeTool

**Tool name:** `EnterWorktree` · **Permission level:** Write

Create a new git worktree on a new branch and switch the session's working directory to it. Only one worktree session can be active at a time.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `branch` | string | no | Branch to create (default: a timestamped `claurst-YYYYMMDD-HHMMSS` name) |
| `path` | string | no | Worktree directory (default `.worktrees/<branch>`) |
| `post_create_command` | string | no | Command to run inside the new worktree (e.g. `npm install`); asks for permission as an Execute tool under its own name (Bash rules do not cover it); commands rated Critical are refused |

---

### ExitWorktreeTool

**Tool name:** `ExitWorktree` · **Permission level:** Write

Leave the worktree created by `EnterWorktree` in this session and restore the original working directory.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `action` | string | yes | `keep` leaves the worktree on disk; `remove` deletes it and its branch |
| `discard_changes` | boolean | no | Required as `true` to remove a worktree with uncommitted or unmerged work |

---

## Utility Tools

### AskUserQuestionTool

**Tool name:** `AskUserQuestion` · **Permission level:** None

Pause execution and ask the user a question via an interactive prompt in the TUI. Returns the user's typed response. Use sparingly — prefer acting with best judgment and asking only when the choice is genuinely ambiguous.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `question` | string | yes | Question text to display |
| `options` | array | no | Multiple-choice options (renders as a menu) |

In headless (`--print`) runs the call returns an error.

---

### BriefTool

**Tool name:** `Brief` · **Permission level:** None

Send a Markdown message to the user, optionally with file attachments. The message is the tool result; attachments must exist or the call fails.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `message` | string | yes | Message to send |
| `status` | string | yes | `normal` for a reply, `proactive` for an unsolicited update |
| `attachments` | array | no | File paths to attach |

---

### SleepTool

**Tool name:** `Sleep` · **Permission level:** None

Pause for a specified duration without holding a shell process.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `ms` | integer | yes | Milliseconds to sleep (`duration_ms` is accepted as an alias) |

Values above 300000 ms (5 minutes) are capped to 300000.

---

### ConfigTool

**Tool name:** `Config` · **Permission level:** Write

Read or change a setting; changes are saved to `~/.claurst/settings.json`. Supported settings: `model`, `provider`, `effort`, `max_tokens`, `verbose`, `permission_mode`, `auto_compact`. `setting: "list"` lists them. `verbose`, `permission_mode` and `auto_compact` written by this tool are overwritten by CLI defaults at startup.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `setting` | string | yes | Setting key, or `list` |
| `value` | any | no | New value; omit to read the current value |

---

## Cron Tools

Cron tools schedule prompts that run as agent turns.

### CronCreateTool

**Tool name:** `CronCreate` · **Permission level:** Execute

Schedule a prompt with a 5-field cron expression (`M H DoM Mon DoW`, local time). Returns the task ID.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `cron` | string | yes | Cron expression (e.g. `0 9 * * 1-5`) |
| `prompt` | string | yes | Prompt to run at each scheduled time |
| `recurring` | boolean | no | `true` (default) repeats; `false` fires once and deletes the task |
| `durable` | boolean | no | `true` saves the task to `~/.claurst/scheduled_tasks.json`; `false` (default) keeps it for this session only |

---

### CronDeleteTool

**Tool name:** `CronDelete` · **Permission level:** None

Delete a scheduled task by ID (from `CronList`).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `id` | string | yes | Task ID to delete |

---

### CronListTool

**Tool name:** `CronList` · **Permission level:** None

List scheduled tasks: ID, cron expression, a readable schedule, `recurring`, `durable`, and the start of the prompt.

No parameters.

---

## Code Intelligence Tools

Code intelligence tools query language servers for semantic information about source code.

### LspTool

**Tool name:** `LSP`

**Permission level:** ReadOnly

Query a language server for code intelligence actions. Supports hover documentation, go-to-definition, find-references, document symbols, and diagnostics. Language servers must be configured in the global `settings.json` under `config.lsp_servers` (a project settings file cannot set it).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `action` | string | yes | `hover`, `definition`, `references`, `symbols`, or `diagnostics` |
| `file` | string | yes | Absolute or working-directory-relative path to the source file |
| `line` | integer | no | 1-based line number (required for `hover`, `definition`, `references`) |
| `column` | integer | no | 1-based column number (required for `hover`, `definition`, `references`) |

**Actions:**

| Action | Description |
|--------|-------------|
| `hover` | Returns documentation/type info for the symbol at `line`:`column` |
| `definition` | Returns the file and position where the symbol is defined |
| `references` | Lists all references to the symbol at `line`:`column` |
| `symbols` | Returns all symbols (functions, classes, variables) in the file |
| `diagnostics` | Returns LSP diagnostics (errors, warnings) for the file |

**Configuration** (`settings.json`):

```json
{
  "config": {
    "lsp_servers": [
      {
        "name": "rust-analyzer",
        "command": "rust-analyzer",
        "args": [],
        "file_patterns": ["*.rs"]
      },
      {
        "name": "typescript",
        "command": "typescript-language-server",
        "args": ["--stdio"],
        "file_patterns": ["*.ts", "*.tsx"],
        "extension_to_language": { ".ts": "typescript", ".tsx": "typescriptreact" }
      }
    ]
  }
}
```

`name`, `command`, `args` and `file_patterns` are required. Optional: `initialization_options` (sent in `initialize`), `extension_to_language` (file extension to LSP language ID), and `env` (extra environment variables).

If no LSP server is configured for a file's language, the tool returns an informative error. The tool resolves relative paths against the current working directory.

---

## Advanced Tools

### ComputerUseTool

**Tool name:** `computer` · **Permission level:** Dangerous

Control the desktop GUI: take screenshots, move the mouse, click, type, press keys, scroll, and drag. The tool exists only in builds compiled with the `computer-use` cargo feature of `claurst-tools`, which no default build enables.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `action` | string | yes | `screenshot`, `mouse_move`, `left_click`, `right_click`, `double_click`, `left_click_drag`, `type_text`, `key`, `scroll`, or `get_cursor_position` |
| `coordinate` | array | no | `[x, y]` pixel coordinate for mouse actions |
| `start_coordinate`, `end_coordinate` | array | no | `[x, y]` start and end for `left_click_drag` |
| `text` | string | no | Text for `type_text`, or a key sequence for `key` (e.g. `Return`, `ctrl+c`) |
| `direction` | string | no | `up`, `down`, `left`, or `right` for `scroll` |
| `amount` | integer | no | Number of scroll notches |

Every call goes through the permission check. Requires a display.

---

### StructuredOutput

**Tool name:** `StructuredOutput` (SyntheticOutputTool)

**Permission level:** None

Return the final answer of a headless run as JSON. The tool exists only when `--json-schema <file|inline JSON>` is passed with `--print`; it is not part of the default tool set.

The schema given to `--json-schema` (it must have `"type": "object"`) is the tool's input schema. A call whose top-level properties do not match it (missing `required` keys, wrong property `type`) returns an error so the model can correct it. The input of the last accepted call is reported as `structured_output` in the run's `result` event. See [Headless mode](advanced.md#structured-output).

**Example** with `--json-schema '{"type":"object","properties":{"summary":{"type":"string"},"issues":{"type":"array"}},"required":["summary"]}'`:

```json
{
  "summary": "Three security issues found.",
  "issues": [
    { "severity": "high", "description": "SQL injection in login handler" }
  ]
}
```

---

## Tool Framework Internals

### ToolContext

Every tool receives a `ToolContext` (`src-rust/crates/tools/src/lib.rs`) at call time. Key fields:

| Field | Description |
|-------|-------------|
| `working_dir` | Directory relative paths resolve against |
| `permission_mode` | Active permission mode |
| `permission_handler` | Decides `Allow`, `Deny` or `Ask` for a permission request |
| `session_id` | Current session ID (keys shell state, REPL processes, todos) |
| `file_history` / `current_turn` | Per-turn record of file changes used by `/undo` and `/revert` |
| `non_interactive` | `true` in headless runs; prompts become rejections |
| `mcp_manager` | Connected MCP servers, if any |
| `config` | The session's `Config` |
| `user_question_tx` | Channel `AskUserQuestion` uses to reach the TUI |
| `cancel_token` | Cancelled when the user interrupts the turn |

### PermissionDecision

A permission handler returns one of `Allow`, `AllowPermanently`, `Deny`, `DenyPermanently`, `DenyWithFeedback(String)`, or `Ask { reason }` (`src-rust/crates/core/src/lib.rs`, `permissions` module). `Ask` shows the permission dialog in the TUI.
