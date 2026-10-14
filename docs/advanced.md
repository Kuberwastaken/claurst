# Advanced Features

This document covers Claurst's advanced capabilities beyond basic coding assistance.

---

## Extended thinking

Extended thinking gives the model additional computation budget to reason through hard problems before it responds.

### Commands

```
/thinking          Toggle extended thinking on or off for the session (alias /think)
/effort            Open the effort picker
/effort <level>    Set the effort level: low, normal, or high
```

### CLI flags

```
claurst --thinking <tokens>    Set a specific token budget for thinking
claurst --effort <level>       Set the effort level for the session
```

### Effort levels

The levels are `none`, `minimal`, `low`, `medium` (alias `normal`), `high`, `xhigh`, `max` and `ultracode`; `--effort` accepts any of them. The `/effort` picker offers only the levels the current model supports: a model with reasoning variants gets its own tiers (for example `low` to `max` on recent Opus models), other models get `low`, `medium` and `high`, and `ultracode` is always offered last.

`/effort low`, `/effort normal` and `/effort high` also set the response token cap (4,096 for `low`, 32,768 for `high`, the default for `normal`).

The level lasts for the session and is not saved.

### How levels are sent

Claude 4.6 and later models use adaptive thinking, and the level is sent as `output_config.effort` (`none` and `minimal` map to the lowest effort). Older Claude models get a `budget_tokens` thinking budget instead.

---

## Auto-compaction

The context window has a finite size. Auto-compaction automatically summarises the conversation history when token usage approaches the limit, keeping the session alive without interruption.

### How it works

Before every model request, including the one after each batch of tool results, Claurst estimates the prompt size: the prompt tokens the provider last reported plus an estimate of what was appended since. When that reaches the threshold, the head of the conversation is summarised through the active provider and replaced by the summary; the most recent messages (about 16,000 tokens) are kept verbatim.

The threshold is the lower of 90% of the context window and the window minus the response reserve (`max_tokens`, capped at a quarter of the window). For a 200,000-token window and `max_tokens` 32,000 that is 168,000 tokens.

If the provider still rejects a request as too long, Claurst compacts once and retries the request. A second overflow on the same request is reported as an error.

The `PreCompact` hook fires before compaction, with `trigger` set to `auto` or `manual`; it cannot block it. There is no `PostCompact` hook. See [Hooks](hooks.md).

### Controlling auto-compaction

```bash
claurst --no-auto-compact
```

`--no-auto-compact` turns off both the threshold check and the overflow retry for the session; `/compact` still works. It is the only switch: the `auto_compact` and `compact_threshold` keys in `config` and the top-level `autoCompact` key are not read by the query loop.

### Manual compaction

```
/compact [custom instructions]
```

Runs compaction immediately. Optionally pass custom instructions to guide the summary (e.g. `/compact focus on the database schema changes`).

---

## Context window management

### /context

```
/context
```

Displays the current token usage relative to the model's context window. In the TUI it toggles the context overlay.

The TUI's token warning turns yellow at 80% of the context window (or with 20,000 tokens or fewer left) and red at 95%. Input is never blocked; auto-compaction runs before the window fills (see [Auto-compaction](#auto-compaction)).

### ctx-viz

```
/ctx-viz
```

Aliases: `/ctx`, `/context-visualizer`. Prints an estimated breakdown of the context by category (system prompt, conversation messages, tool results) against a 200,000-token window.

---

## Session management

Sessions live in `~/.claurst/sessions/` (or `$CLAURST_HOME/sessions/`). Each session id has up to three files, all owner-only (0600):

| File | Contents |
|---|---|
| `<id>.json` | Snapshot: the whole session (messages, title, model, cost, compactions). Rewritten at the end of every turn and on idle metadata changes, atomically: written to a temp file in the same directory, flushed, then renamed over the old one. A crash leaves the previous or the new snapshot, never a half-written one. |
| `<id>.jsonl` | Journal: append-only, one entry per line. The agent loop appends after every assistant message, every batch of tool results and every compaction, so a crash or a killed terminal loses at most the step that was in flight. It is never rewritten, so it also keeps the messages a later rewind or compaction replaced. |
| `<id>.lock` | Advisory lock held by the one claurst process writing the session. The OS releases it when that process exits, so a crash never leaves a stale lock. |

A second claurst that opens a session another process is writing can read it, but shows a warning and does not save it, so the two never interleave writes.

`~/.claurst/sessions.db` is a SQLite search index refreshed at the end of each turn; it is not needed to resume.

### Commands

| Command | Description |
|---|---|
| `/resume <session-id>` | Load a saved session (TUI: opens the browser). Alias: `/continue`. |
| `/session` | Session browser (TUI); text form shows the current and 5 most recent sessions, or the bridge URL. |
| `/fork [N]` | Save the conversation up to message N as a new session titled "Fork of …". |
| `/rename <title>` | Rename the current session. |
| `/export` | Export the current session transcript. |
| `/rewind` | Step back to an earlier point in the conversation, including messages an earlier compaction replaced (listed first, marked `[compacted]`). |

### Journal format and recovery

Each journal line is `{"seq":N,"entry":{"type":...}}` with `seq` increasing by one per line. Entry types:

| `type` | Meaning |
|---|---|
| `meta` | Session metadata (id, title, model, working directory, cost). The first line of every journal; repeated at each checkpoint. |
| `message` | Append one message. |
| `truncate` | Drop the messages from `len` on (a rewind). Every journal starts with `meta` then `truncate` to 0, so replaying it onto an older snapshot (from before the journal existed, or after the journal file was lost) replaces that snapshot's history rather than appending a second copy. |
| `compaction` | Replace the head of the history, `[0, messages_before + 1 - messages_after)`, with `summary`. The replaced messages are kept in the compaction record so `/rewind` can return to them. |
| `drop_compactions` | Forget the compactions from `keep` on (a rewind past a compaction). |

The snapshot records the `seq` of the last journal entry it includes (`journal_seq`). Loading a session (`--resume`, `/resume`, the session browser):

1. Parse `<id>.json` and replay the journal entries with a higher `seq`. `--resume` reports how many it recovered; those are the steps of a turn that was interrupted before its snapshot.
2. If the snapshot is missing or does not parse, rebuild the session from the journal alone.
3. A journal line that does not parse (for example a line cut short by a crash) is skipped; the next write cuts it off before appending.

A resumed session continues from the latest compaction summary forward, as before the crash.

Sessions saved by older versions (a `<id>.json` with no journal) load unchanged. Their journal is created, starting with the full history, the first time they are written.

---

## Worktree support

The model can move the session into an isolated git worktree to avoid interfering with the main working tree.

### Tools

- `EnterWorktree` — creates a git worktree (default branch `claurst-<timestamp>`, default path `.worktrees/<branch>`), optionally runs a setup command in it, and switches the session's working directory to it.
- `ExitWorktree` — returns to the original directory; `action: "keep"` leaves the worktree on disk, `"remove"` deletes it and its branch.

---

## Plan mode

Plan mode restricts the model to read-only operations, allowing it to research a codebase and propose a plan before making any changes.

### Entering plan mode

```
/plan [description]
claurst --permission-mode plan
```

When in plan mode:
- Write and execute operations are denied.
- The model can read files, search, and reason freely.
- Exiting plan mode (via the `ExitPlanMode` tool) returns to the normal permission model.

The `EnterPlanMode` and `ExitPlanMode` tools manage transitions.

---

## Goal system

The goal system lets Claurst work autonomously across multiple turns toward a single, verifiable objective. Instead of prompting repeatedly, you set the goal once and Claurst iterates until the goal is complete, paused, or the built-in runaway guard fires.

### Setting a goal

```
/goal <objective>
/goal --tokens 250K Refactor the auth module to use JWT tokens
```

Once a goal is set, Claurst begins working immediately. It continues across turns without waiting for user input until one of these conditions is met:

- The model calls the `GoalComplete` tool with an audit summary and evidence
- You run `/goal pause` or `/goal clear`
- The runaway guard fires after 200 turns (the goal is paused; `/goal resume` continues)
- A token budget is set and exhausted (the goal stops as `budget_limited`)

### Monitoring and controlling goals

```
/goal                  — show current goal status
/goal status           — show current goal status
/goal pause            — pause the active goal (you can resume it later)
/goal resume           — resume a paused goal
/goal clear            — delete the current goal entirely
/goal complete         — manually request a completion audit
```

### How completion works

When the model believes the objective has been met, it calls the `GoalComplete` tool rather than simply responding. This tool requires two arguments:

- `audit_summary` — a concise description of what was accomplished
- `evidence` — specific, verifiable evidence (files changed, tests passing, output produced)

Claurst displays both to the user before marking the goal complete. The model is expected to genuinely audit the outcome before calling; calling without real evidence is rejected.

### Goal status lifecycle

| Status | Meaning |
|--------|---------|
| `active` | Goal is set and work is ongoing |
| `paused` | Paused by the user or by the 200-turn runaway guard; goal is preserved |
| `budget_limited` | The `--tokens` budget was reached |
| `complete` | Model called `GoalComplete` with an accepted audit |

### Disabling the goal system

```bash
CLAURST_GOALS=0 claurst
```

Set `CLAURST_GOALS=0` (or `false`) in the environment: this disables the `/goal` commands and the autonomous continuation loop; the `GoalComplete` tool stays registered. Useful in environments where autonomous multi-turn execution is undesirable.

---

## Managed agents

Managed agents enable a **manager-executor** architecture where a manager model delegates subtasks to one or more executor agents running in parallel. The manager reasons about the high-level plan; executors carry out individual tasks.

### Enabling managed agents

```
/managed-agents enable
```

Or apply a built-in preset:

```
/managed-agents presets               — list available presets
/managed-agents preset <name>         — apply a preset (configures all parameters)
```

### Architecture

```
User → Manager model
         ├─ Executor 1 (e.g., implementing feature A)
         ├─ Executor 2 (e.g., writing tests for A)
         └─ Executor 3 (e.g., updating docs)
```

The manager model does not execute tools itself — it delegates to executor agents and synthesizes their outputs. Executors can run concurrently up to the `concurrent` limit.

### Configuration

```
/managed-agents configure manager-model  anthropic/claude-opus-4-6
/managed-agents configure executor-model anthropic/claude-sonnet-4-6
/managed-agents configure executor-turns 20
/managed-agents configure concurrent     3
/managed-agents configure isolation      on
```

Model format: `provider/model` (e.g., `anthropic/claude-opus-4-6`, `openai/gpt-4o`).

### Budget split policies

Control how the total token/cost budget is divided between the manager and executors:

| Policy | Command | Description |
|--------|---------|-------------|
| `shared` | `configure budget-split shared` | All agents draw from a single shared pool |
| `percentage` | `configure budget-split percentage:20` | Manager gets 20%, executors share the rest |
| `fixed` | `configure budget-split fixed:0.50:2.00` | Manager capped at $0.50, each executor at $2.00 |

```
/managed-agents budget 5.00           — set a total $5 budget (0 to clear)
```

### Viewing configuration

```
/managed-agents status
```

`/managed-agents setup` prints the presets and the manual configuration steps, `disable` turns managed agents off without removing the configuration, and `reset` removes it. `enable` requires both models to be set.

Configuration persists to `~/.claurst/settings.json` under `managed_agents` (see [Managed Agents Configuration](configuration.md#managed-agents-configuration)).

> **Preview feature.** The managed-agents API is under active development and may change in future releases.

---

## Speech modes

Speech modes are the `caveman` and `rocky` output styles. `/caveman` and `/rocky` select them and save the choice to `config.output_style`, exactly like `/output-style caveman`; they take no arguments. Typing the single word `caveman`, `rocky` or `normal` inline in a prompt applies it to that one turn only.

### Caveman mode

Strips pleasantries, hedging phrases, transitional sentences, and articles. Output is dense and telegraphic.

```
/caveman
```

**Example (normal mode):**
> "I'd be happy to help with that. Let me take a look at the file and see what changes might be appropriate here."

**Example (caveman full):**
> "Look at file. Make changes."

### Rocky mode

The model adopts the communication style of Rocky from *Project Hail Mary* — an Eridian alien engineer with a distinctive grammar, heavy emphasis, and alien perspective on human tasks.

```
/rocky
```

**Example (rocky full):**
> "Read file. *Yes!* Change function. Return value wrong! Fix now. *Eridians not make this mistake.*"

### Deactivating speech modes

```
/normal
```

Resets the output style to `default`, the same as `/output-style default`.

---

## Headless mode

Headless mode runs Claurst non-interactively for scripts, CI pipelines and programmatic orchestration. A positional prompt or `-p` / `--print` selects it.

```bash
claurst -p "summarise the changes in this PR"
cat my_prompt.txt | claurst --print
claurst -p --output-format stream-json "fix the failing test" | jq -c .
```

With no positional prompt, the prompt is read from stdin. `--input-format stream-json` reads newline-delimited `{"role":"user"|"assistant","content":"..."}` messages from stdin instead.

### Output formats

| `--output-format` | stdout |
|---|---|
| `text` (default) | The text of each assistant message, printed once the message is complete. Tool progress (`[Bash...]`), retries and the stop reason go to stderr. |
| `json` | One `result` object (below) when the run ends. |
| `stream-json` | One JSON event per line, in the order below, ending with the `result` event. |

Text is printed per completed message rather than per streamed token, so output from an attempt that fails and is retried never reaches stdout.

### stream-json events

The events follow Claude Code's `stream-json` shape. Every event carries `session_id`.

```jsonl
{"type":"system","subtype":"init","session_id":"3f0c...","model":"claude-sonnet-4-6","cwd":"/repo","tools":["Bash","Read","Edit"],"permissionMode":"default"}
{"type":"assistant","message":{"id":"msg_01...","role":"assistant","content":[{"type":"text","text":"Running the tests."},{"type":"tool_use","id":"toolu_01...","name":"Bash","input":{"command":"cargo test"}}]},"parent_tool_use_id":null,"session_id":"3f0c..."}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01...","content":"test result: ok. 12 passed"}]},"parent_tool_use_id":null,"session_id":"3f0c..."}
{"type":"retry","attempt":1,"max_attempts":5,"delay_ms":2000,"reason":"HTTP 529 overloaded","session_id":"3f0c..."}
{"type":"compacted","at":"2026-10-14T10:00:00Z","trigger":"threshold","summary_uuid":"...","messages_before":80,"messages_after":12,"tokens_before":180000,"tokens_after":24000,"summarised":true,"session_id":"3f0c..."}
{"type":"assistant","message":{"id":"msg_02...","role":"assistant","content":[{"type":"text","text":"All 12 tests pass."}]},"parent_tool_use_id":null,"session_id":"3f0c..."}
{"type":"result","subtype":"success","is_error":false,"result":"All 12 tests pass.","stop_reason":"end_turn","session_id":"3f0c...","num_turns":2,"duration_ms":18340,"total_cost_usd":0.0412,"usage":{"input_tokens":20510,"output_tokens":388,"cache_creation_input_tokens":0,"cache_read_input_tokens":11264}}
```

| Event | When |
|---|---|
| `system` (`subtype: "init"`) | First, before the model is called: session id, model, working directory, tool names, permission mode. |
| `assistant` | Each assistant message, once complete, with all content blocks (`text`, `thinking`, `tool_use` with its `id`). |
| `user` | Each message of tool results (`tool_result` blocks with `tool_use_id`; `is_error: true` marks a failed or rejected call). |
| `retry` | A model request failed transiently and is retried after `delay_ms`. Output from the failed attempt is discarded. |
| `compacted` | The history was compacted; the fields are the session's compaction record. |
| `result` | Last line of the run. |

`result` fields: `subtype`, `is_error`, `result` (text of the last assistant message), `stop_reason`, `error` (when the run did not succeed), `session_id`, `num_turns` (assistant messages in this run), `duration_ms`, `total_cost_usd`, `usage` (run totals), and `structured_output` (with `--json-schema`).

This replaces the earlier `text_delta` / `tool_start` / `result` stream.

### Exit codes

| Code | `subtype` | `stop_reason` | Meaning |
|---|---|---|---|
| 0 | `success` | `end_turn` | The model finished. |
| 1 | `error_during_execution` | `error` | An error ended the run (`error` has the message). |
| 2 | `error_max_turns` | `max_turns` | The turn cap was reached (after one tool-less summary turn). |
| 2 | `error_max_budget_usd` | `budget` | `--max-budget-usd` was reached. |
| 2 | `error_max_tokens` | `max_tokens` | The response hit the output-token limit after recovery attempts. |
| 130 | `cancelled` | `cancelled` | Interrupted with Ctrl-C. |

Every case writes a `result` (`json` / `stream-json`) or the reason on stderr (`text`). A bad flag, an unreadable `--json-schema` or a missing session exits 1 before the run starts.

The subtypes `success`, `error_max_turns`, `error_max_budget_usd` and `error_during_execution` match Claude Code; `error_max_tokens` and `cancelled` are claurst additions.

On non-Anthropic providers, `--max-budget-usd` is not enforced and a response that stops at the output-token limit ends the run as `success` (exit 0): both checks exist only on the Anthropic request path so far.

### Sessions

Headless runs are saved like interactive ones (`~/.claurst/sessions/<id>.json`), so a later run or the TUI can continue them.

| Flag | Effect |
|---|---|
| `-c`, `--continue` | Continue the most recently updated session. With no saved session, a new one is started. |
| `--resume <id>` | Continue session `<id>`; exits 1 if it does not exist. `--resume` without an id is `-c`. |
| `--session-id <id>` | Continue session `<id>` if it exists, otherwise create it with that id. Cannot be combined with `-c` / `--resume`. |

The new prompt is appended to the saved history, and a continued session keeps its saved model unless `--model` is given. Session ids may contain letters, digits, `-`, `_` and `.`.

### Permissions

Headless runs never prompt. A tool call that would ask for approval in the TUI (for example a write in `default` mode) is rejected, and the tool result tells the model why and not to retry; the run continues. Use `--permission-mode`, permission rules, or `--dangerously-skip-permissions` to allow more. `--allowed-tools` / `--disallowed-tools` add allow and deny rules for the run; see [Permission Rules and Approvals](configuration.md#permission-rules-and-approvals).

### Structured output

```bash
claurst -p --output-format json \
  --json-schema '{"type":"object","properties":{"verdict":{"type":"string"}},"required":["verdict"]}' \
  "Is this branch ready to merge?"
```

`--json-schema` takes inline JSON or a path to a JSON file. The schema must have `"type": "object"`. It adds the `StructuredOutput` tool with that schema as its input; a call whose top-level properties do not match (missing `required` keys, wrong property `type`) is returned to the model as an error. The input of the last accepted call is the result's `structured_output`. If the model never calls the tool, the field is absent.

---

## Budget control

Limit resource consumption per invocation:

```bash
claurst -p --max-budget-usd 2.00 "..."   # stop once the run has spent $2.00
claurst -p --max-turns 30 "..."          # stop after 30 model turns (default 100)
claurst -p --max-tokens 8000 "..."       # cap each response at 8,000 output tokens
```

`--max-turns` defaults to 100. When the cap is reached, the model gets one final turn without tools to summarise what it did and where it stopped; the run then ends with `error_max_turns` and exit code 2. `--max-budget-usd` ends the run with `error_max_budget_usd` and exit code 2. See [Exit codes](#exit-codes).

---

## The Buddy companion system

The `claurst-buddy` crate implements a companion derived deterministically from a user ID. It is not wired into the TUI or any command yet, so no companion is shown. This section describes the crate.

### How companions are generated

The companion's visual traits (species, eyes, hat, rarity, shiny status, stats) are generated by hashing the user ID (FNV-1a) into a seeded PRNG (Mulberry32). The bones are regenerated from the hash on every read, so they cannot be faked by editing a file.

Only the soul (name, personality, hatch time) is persisted, to `<config dir>/companion.json`, after it has been "hatched".

### Species

18 species are available: duck, goose, blob, cat, dragon, octopus, owl, penguin, turtle, snail, ghost, axolotl, capybara, cactus, robot, rabbit, mushroom, chonk.

### Rarity tiers

| Rarity | Weight | Stars |
|---|---|---|
| common | 60% | ★ |
| uncommon | 25% | ★★ |
| rare | 10% | ★★★ |
| epic | 4% | ★★★★ |
| legendary | 1% | ★★★★★ |

Rarity affects the floor value of the companion's stats. A legendary companion has a minimum stat floor of 50, while a common companion starts at 5.

### Stats

Each companion has five stats: DEBUGGING, PATIENCE, CHAOS, WISDOM, SNARK. One stat is the peak (higher rolls), one is the dump stat (lower rolls), and the rest are scattered around the rarity floor.

### Persistence

The stored format in `<config dir>/companion.json`:

```json
{
  "name": "Vortox",
  "personality": "a chaotic little axolotl who celebrates every bug as a feature",
  "hatched_at": "2026-04-05T12:34:56Z"
}
```

The bones (species, rarity, stats, eyes, hat, shiny) are never stored and are always recomputed from the user ID.

---

## Voice mode

```
/voice
```

Experimental voice input using the device microphone. When voice mode is on, press Alt+V to record; Esc cancels a recording. The audio is transcribed through a Whisper-compatible API: OpenAI's transcription endpoint with `OPENAI_API_KEY` by default, or the server at `WHISPER_ENDPOINT_URL` (for example a local whisper.cpp).

`/voice` toggles voice mode for this session; `/voice on|off` only save the startup state (effective next start); `/voice status` shows it. `CLAURST_VOICE_ENABLED=1` starts with voice mode on.

---

## Vim keybindings

```
/vim
```

Toggles vim-style modal keybindings for the input buffer (alias `/vi`; `/vim on` and `/vim off` set it). When enabled, Esc switches between insert and normal mode, and normal mode accepts vim motions.

The setting is saved to `<config dir>/ui-settings.json`. There is no CLI flag for it.

---

## Bridge and remote sessions

The remote-control bridge lets a remote UI send prompts to a running Claurst session and receive its responses.

```
/remote-control [start|stop|status]     (alias /rc)
```

No argument or `status` shows the bridge status. `start` and `stop` set `remoteControlAtStartup`; the change applies on the next start. `/mobile session` shows a QR code that links to the active bridge session. Set `remoteControlAtStartup: true` in the global settings to start it with every session. `CLAURST_BRIDGE_URL` sets the server URL and enables the bridge; `CLAURST_BRIDGE_TOKEN` sets its bearer token.

There is no `--ssh` flag.

---

## Instruction files

Claurst loads `AGENTS.md` / `CLAUDE.md` instruction files once at session start: the global `~/.claurst/` and `~/.claude/` directories first, then each directory from the git root down to the working directory (never above the git root; only the working directory when there is no `.git`). In each directory `AGENTS.override.md` replaces the other files; otherwise `AGENTS.md`, `CLAUDE.md`, `.claude/CLAUDE.md` and `.claurst/AGENTS.md` are loaded. `@include` lines are expanded within the project root, and all files share a 32 KiB budget. Each file is sent as an `Instructions from: <path>` block in the cached part of the system prompt.

See [Instruction Files](configuration.md#instruction-files-agentsmd--claudemd) for the full rules.

The `/memory` command opens the memory management UI for viewing, editing, and organising instruction files.

---

## Security and permissions

### Permission modes

| Mode | Description |
|---|---|
| `default` | Prompts the user before executing dangerous or write operations. |
| `plan` | Read-only; write and execute operations are denied. |
| `acceptEdits` | Accepts file edits without prompting; other calls behave as in `default`. |
| `bypassPermissions` | Skips the permission prompts. Intended for trusted automation only. |

The active mode is set with `--permission-mode <mode>` (`--dangerously-skip-permissions`, alias `--yolo`, selects `bypassPermissions`). See [Permission Modes](configuration.md#permission-modes).

### Tool risk classification

Every tool is classified into a risk tier that determines the default permission behaviour:

| Tier | Examples | Default behaviour |
|---|---|---|
| `forbidden` | A Bash command classified as Critical | Always blocked |
| `dangerous` | `computer` (computer use) | Prompt required |
| `execute` | Bash, PowerShell | Prompt required |
| `write` | Write, Edit | Prompt in default mode |
| `readonly` | Read, Glob, Grep | Reads inside the workspace are allowed automatically |
| `none` | TodoWrite | No permission needed |

### Bash command risk classification

The `Bash` tool classifies each command as Safe, Low, Medium, High or Critical by matching the command string against known patterns. Critical commands (`rm -rf /`, `dd if=...`, `mkfs`, fork bombs) are refused in every permission mode.

A `PreToolUse` hook can decide a tool call before the permission prompt: `allow` skips the prompt, `ask` forces it, `deny` blocks the call. See [Hooks](hooks.md).

---

## Output styles

```
/output-style [style]
```

An output style adds instructions to the system prompt that shape how the model writes its responses. Without an argument the command lists the available styles; `/output-style <name>` switches and saves the choice to `config.output_style`. Changes apply from the next request.

Built-in styles: `default`, `concise`, `explanatory`, `learning`, `caveman`, `rocky`. Custom styles are `.md` or `.json` files in `<config dir>/output-styles/`.

---

## Custom commands

Custom slash commands are defined in `settings.json` under the `commands` map, keyed by command name. A custom command is a template that expands to a prompt when invoked.

```json
{
  "commands": {
    "standup": {
      "description": "Summarise today's work",
      "template": "Summarise what I worked on today based on the git log since midnight."
    },
    "explain": {
      "description": "Explain a file",
      "template": "Explain what $ARGUMENTS does."
    }
  }
}
```

A built-in command with the same name takes precedence. See [Custom Slash Commands](configuration.md#custom-slash-commands) for the fields. Skills are also run as slash commands (`/<skill-name>`); see [Skills](configuration.md#skills).

---

## Formatters

Auto-formatters run automatically after file writes when configured in the `formatter` map (see [File Formatters](configuration.md#file-formatters)). A `PostToolUse` hook on `Write` and `Edit` works too:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit|Write",
        "hooks": [
          {
            "type": "command",
            "command": "f=$(jq -r .tool_input.file_path); prettier --write \"$f\" 2>/dev/null || true"
          }
        ]
      }
    ]
  }
}
```

Common formatters:
- **Prettier** (`prettier --write <file>`) — TypeScript, JavaScript, JSON, CSS, Markdown.
- **Ruff** (`ruff format <file>`) — Python.
- **rustfmt** (`rustfmt <file>`) — Rust.
- **gofmt** (`gofmt -w <file>`) — Go.
- **clang-format** (`clang-format -i <file>`) — C/C++.

A `PostToolUse` hook cannot undo the tool call; a non-zero exit other than 2 is only logged. Suppress failures for unknown file types (`|| true`).

---

## Environment management

### --add-dir

```bash
claurst --add-dir /path/to/additional/project "..."
```

Lets Claurst read files in an additional directory outside the working directory without a prompt; writes there still follow the permission mode. Useful when a task spans multiple repositories or when config files live outside the project root.

Multiple `--add-dir` flags can be combined. `/add-dir <path>` adds a directory permanently (to `config.workspace_paths`).

### Environment variables in config

The `config.env` map holds variables for remote sessions, managed with `/remote-env`. It is not injected into local tool executions. To give a tool a variable, export it in the shell that starts Claurst.

---

## LSP integration

The `LSP` tool provides code intelligence by communicating with a Language Server Protocol server for the current file.

### Operations

| Operation | Description |
|---|---|
| `hover` | Get type information and documentation for the symbol at a position |
| `definition` | Find where a symbol is defined |
| `references` | Find all references to a symbol |
| `symbols` | List all symbols (functions, classes, variables) in the file |
| `diagnostics` | Get errors and warnings reported by the language server |

### Input schema

```json
{
  "action": "hover",
  "file": "src/main.rs",
  "line": 42,
  "column": 15
}
```

- `action` — required, one of the operations above
- `file` — required, absolute or working-directory-relative path
- `line` — 1-based line number (required for `hover`, `definition`, `references`)
- `column` — 1-based column number (required for `hover`, `definition`, `references`)

The `symbols` and `diagnostics` actions only require `action` and `file`.

### Configuration

LSP servers must be configured in the `config` object of the global `settings.json` (`lsp_servers` is global-only):

```json
{
  "config": {
    "lsp_servers": [
      {
        "name": "rust-analyzer",
        "command": "rust-analyzer",
        "args": [],
        "file_patterns": ["*.rs"],
        "extension_to_language": { ".rs": "rust" }
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

`name`, `command`, `args` and `file_patterns` are required; `initialization_options` (passed in `initialize`), `extension_to_language` and `env` are optional.

If no LSP server is configured for the file's language, the tool returns an informative error. Path resolution is relative to the current working directory.
