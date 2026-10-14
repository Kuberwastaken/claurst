# Changelog

User-visible changes per release. Pull request numbers refer to [Kuberwastaken/claurst](https://github.com/Kuberwastaken/claurst/pulls).

---

## v0.2.0

### Breaking changes

- **MCP tool names are now `mcp__<server>__<tool>`** (#463). The old `<server>_<tool>` names are not aliased. Permission rules that named MCP tools must be rewritten; `mcp__<server>` and `mcp__<server>__*` match every tool of a server. See [MCP](mcp.md).
- **Hooks use the Claude Code settings shape** (#483): `"hooks": { "<Event>": [ { "matcher": "...", "hooks": [ { "type": "command", "command": "...", "timeout": 30 } ] } ] }`. The old flat `{ "command", "tool_filter", "blocking" }` entries are skipped with a warning; `blocking` has no replacement (a hook blocks with exit code 2 or `"decision": "deny"`). See [Hooks](hooks.md).
- **`.claurst/commands/*.md` is no longer read** (#474). Move those files into a skill directory (`.claurst/skills/<name>/SKILL.md`). See [Skills](configuration.md#skills).
- **Headless `stream-json` events changed** (#481). The `text_delta` / `tool_start` / `error` / `result` stream is replaced by Claude Code's shape: `system` (init), `assistant`, `user`, `retry`, `compacted`, `result`. See [stream-json events](advanced.md#stream-json-events).
- **Headless exit codes** (#481): `0` success, `1` error, `2` turn cap / budget / output-token limit, `130` Ctrl-C. Previously limits exited 0. See [Exit codes](advanced.md#exit-codes).
- **`--max-turns` defaults to 100** (was 10) (#481). Hitting the cap now gives the model one tool-less summary turn and exits 2.
- **Bash approval rules are matched per command** (#469). A command line is split at `;`, `&&`, `||`, `|` and newlines, and every simple command must be allowed. The first-word TUI allowlist is gone; `allowedBashPrefixes` entries load as `<prefix> *` rules. Lines with substitutions, heredocs, `eval`, `sudo`, `bash -c` and similar always ask. The approval dialog keys are now `y` / `Y` / `p` / `a` / `n`. See [Permission rules](configuration.md).
- **Deny rules apply in every mode, including bypass** (#469), and a tool-wide deny hides the tool from the model. `allowed_tools` is global-only; a project file cannot set it.
- **`ApplyPatch` is checked as `Edit`** (#477). Each patched path goes through the Edit permission check, so `Edit` rules and `acceptEdits` mode govern patches; rules naming `ApplyPatch` no longer match.
- **`/sandbox-toggle exclude` and the `sandbox_mode` / `sandbox_excluded_commands` UI settings are removed** (#476); the sandbox is configured by the top-level `sandbox` settings block.

### Security

- **macOS Seatbelt sandbox for Bash** (#476). On macOS, Bash commands run under `/usr/bin/sandbox-exec` by default (`sandbox.mode: "workspace-write"`): writes are limited to the workspace, `--add-dir` roots, `$TMPDIR` and `/tmp`; `.git` and `.claurst` stay read-only; network is off unless `sandbox.network` is true. A likely sandbox denial offers a rerun outside the sandbox (once, session, or always for that command prefix). Configure with `/sandbox-toggle` or the `sandbox` block. Other platforms are not sandboxed yet.
- **Shell scanner for Bash approvals** (#469). `ls; rm -rf /` is classified per command, so the critical-command block and deny rules cannot be bypassed by chaining.
- **Project-scoped approvals are stored outside the repository** (#469), in `~/.claurst/projects/<id>/permissions.json`.
- **`acceptEdits` is limited to the workspace** (#469): it auto-allows `Edit`, `Write`, `NotebookEdit`, `BatchEdit` and `ApplyPatch` inside the workspace roots and asks outside them. Paths with unresolved `..` never count as inside.
- **Hooks from project plugins are not registered** (#483); project settings files still cannot define hooks.
- **`@include` in instruction files is confined** to the project root (global files: the global directory) (#474).
- **MCP project-server trust covers `headers`** (#463), so an approved server cannot later add an `Authorization: Bearer ${SECRET}` header without re-approval.
- **Bash spill files are private** (#455): `~/.claurst/tool-output/` is 0700 and files are 0600; the shell-state block never reaches disk.

### Features

- **Hooks engine** (#483): events `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `SubagentStop`, `SessionStart`, `SessionEnd`, `Notification`, `PreCompact`, `PostModelTurn`; JSON on stdin and stdout (`decision`, `reason`, `updatedInput`, `additionalContext`, `hookSpecificOutput`); regex/glob matchers; per-hook timeouts; plugin hooks; `/hooks` lists the effective hooks.
- **Skill registry and instruction files** (#474): one registry feeds the Skill tool, `/skills`, skill slash commands and autocomplete; skills are listed to the model in `<available_skills>`. `AGENTS.md`, `AGENTS.override.md`, `CLAUDE.md`, `.claude/CLAUDE.md` and `.claurst/AGENTS.md` are loaded from the global directory and from the git root down to the working directory, with `@include` and a 32 KiB budget. See [Instruction files](configuration.md#instruction-files-agentsmd--claudemd).
- **Headless contract** (#481): typed `stream-json` events, exit codes, `-c` / `--resume` / `--session-id` for headless runs, rejected permission asks with a reason instead of a hang, and `--json-schema` for a structured answer (`structured_output`). See [Headless mode](advanced.md#headless-mode).
- **Codex / ChatGPT subscription parity** (#453): tokens refresh 5 minutes before expiry (one refresh per process), the `ChatGPT-Account-Id` header is sent, the system prompt is sent once, `prompt_cache_key` / `session-id` are set, rate-limit percentages fill the footer, and reasoning is captured and replayed.
- **Anthropic prompt caching** (#454): cache breakpoints on the stable system block, the last tool and the last two user turns; the cached system block is byte-identical across turns; the footer shows cache read/write tokens.
- **Adaptive thinking** (#454): Claude 4.6 and later get `thinking: {type: "adaptive"}` plus `output_config.effort` from `/effort`; older models keep `budget_tokens`.
- **Unified retry policy** (#458) for every provider: retryable statuses (408/409/425/429/5xx/529), `Retry-After` / `retry-after-ms`, exponential back-off with jitter, `maxRetries` setting (default 5). The TUI shows a `Reconnecting n/max` pill; headless emits `retry` events.
- **Provider-agnostic compaction** (#459): automatic compaction before each request and on context-overflow errors for every provider; `/compact [instructions]` compacts immediately; `--no-auto-compact` turns it off; tool results are capped once at insert (`tool_result_max_bytes`, default 64 KiB).
- **MCP robustness** (#463): servers connect concurrently in the background with `connect_timeout_ms` / `catalog_timeout_ms`; remote servers retry with back-off; `notifications/tools/list_changed` refreshes tools; server instructions reach the prompt; `structuredContent` and images in tool results; `headers` with `${VAR}` expansion on HTTP/SSE servers; ToolSearch indexes MCP tools.
- **ApplyPatch accepts the Codex `*** Begin Patch` envelope** (#477) as well as unified diffs, with fuzzy context matching; Codex models get a freeform `apply_patch` tool; `apply_patch <<'EOF'` in Bash is routed to the same executor.
- **Read and Edit** (#452): Read returns at most 2000 lines / about 50 KB per call with an offset continuation hint; Edit falls back to a whitespace-tolerant match when exactly one place matches and shows the closest region on failure; per-path locks serialise concurrent edits.
- **Bash output bounds** (#455): output is drained to the end into head and tail windows; cut output is saved to a spill file named in a `Full output: <path>` trailer; child processes run with `PAGER=cat`, `GIT_PAGER=cat`, `GH_PAGER=cat`, `GIT_TERMINAL_PROMPT=0`, `CLAURST=1`, `AGENT=1`.
- **Permission model v2** (#469): `--allowed-tools` / `--disallowed-tools` and the `allowed_tools` / `disallowed_tools` settings are enforced; `/permissions allow|deny|list`; denying a call can carry a reason to the model; a denial skips the rest of the tool batch.
- **Steering and turn control** (#482): Enter during a turn steers the running turn, Tab queues a new turn; `Alt+Up` / `Alt+X` edit or drop the last queued entry; background sub-agents and `notify: true` Bash commands wake an idle session (`completionWakeups`); Esc cancels a running turn; desktop notifications when the terminal is unfocused (`notifications`); `~N tok/s` in the footer; the context meter shows the last request's prompt size.
- **Session journal** (#480): sessions are written per step to an append-only journal plus an atomic snapshot, recovered on load after a crash, and locked against a second writer; `/rewind` can return to messages that compaction replaced.
- **System prompt** (#472): provider-neutral identity, a capabilities list built from the declared tools, one platform block, a permissions-and-approvals section, and an `<environment_context>` block with the date.
- **ACP** (#460): MCP tools on ACP sessions, permissions evaluated by the same rules as the TUI ("Allow always" is saved), per-session working-directory settings and instruction files, image prompts, and JSON-RPC errors for failed turns.

### Fixes

- Codex logins stored no expiry, so tokens never refreshed (#453).
- `--append-system-prompt` was sent twice in the TUI (#454).
- Streams that ended without a stop event were accepted as complete; unparseable tool JSON ran the tool with `{}` (#458).
- Shell output truncation panicked on multi-byte characters; background commands could deadlock on a full stderr pipe; the PTY reader stopped at 2 MB (#455).
- Unified diffs: a removed `-- ` line ended the hunk, `+++ /dev/null` wrote a file named `/dev/null`, `-l,0` inserted at the wrong line (#477).
- Read claimed images and PDFs were shown to the model; they are not (#452).
- Stop hooks fired twice per turn; hooks did not run on non-Anthropic providers; plugin hooks never ran (#483).
- Switching agent mode with Tab dropped MCP and Agent tools (#463).
- ACP: one cancel token for a session's lifetime, every tool asked the client, MCP tools were missing, and failed turns were reported as refusals (#460).
- An idle key press after a tool step replaced the model history with the display list, dropping tool calls and results from the next request (#489).
- `/permissions set <mode>` did not reach the permission manager (#469).
- A sub-agent cancelled with its parent was marked completed (#482).
