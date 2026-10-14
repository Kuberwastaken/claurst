<div align="center">

# Claurst

<img src="../public/Ship.png" alt="Rustle on the ship" width="350" />

Claurst is a high-performance Rust reimplementation of Claude Code — a terminal-native AI coding agent with streaming responses, 40+ built-in tools, 15+ LLM provider integrations, a full ratatui TUI, and an extensible plugin system.

**Version:** 0.1.8 (Beta) · **License:** GPL-3.0 · [GitHub](https://github.com/Kuberwastaken/claurst)

</div>

---

## What Claurst does

You give Claurst a task in natural language. It plans, reads and writes files, runs shell commands, searches the web, and iterates — all inside your terminal, with every step visible in real time.

```
$ claurst "add input validation to the signup form"
```

Claurst reads your codebase, implements the change across multiple files, runs your tests, and reports back — without you leaving the terminal.

---

## Key capabilities

### Agentic loop
Claurst runs a multi-turn loop: it streams a response from the model, executes any tool calls (file read, bash, web search, …), feeds the results back, and continues until the task is done or the turn limit is reached.

### 40+ built-in tools
- **File operations** — read, write, edit, patch, batch-edit
- **Shell** — bash with persistent working directory and environment
- **Search** — glob file patterns, grep contents, web search, web fetch
- **Worktrees** — enter and leave isolated git worktrees
- **Notebooks** — edit Jupyter notebook cells
- **Desktop automation** — screenshot, click, type (optional feature)
- **Task management** — create, track, and complete tasks

### 15+ LLM providers
Anthropic Claude (default), OpenAI, Google Gemini, AWS Bedrock, Azure OpenAI, Ollama, Groq, Mistral, DeepSeek, xAI, Cohere, OpenRouter, Together AI, Perplexity, GitHub Copilot, Cerebras, LM Studio, and LLaMA.cpp.

### AMOLED terminal UI
A ratatui-based TUI with real-time streaming, syntax-highlighted code blocks, diff viewer, permission dialogs, slash command autocomplete, session browser, and a full keybinding system.

### Multi-account credentials
Store multiple named Anthropic (Claude.ai / Console) and Codex (ChatGPT) accounts in one install and switch between them instantly with `/switch` or `claurst auth switch <id>`. Identity is detected from the OAuth JWT, so re-logging-in the same account is idempotent. See [Authentication](auth#multi-account-profiles).

### @file injection
Type `@path/to/file` anywhere in a prompt to inject the file's contents inline. Typeahead autocomplete suggests paths as you type, with size/binary safety checks before submit. See [@file Injection](keybindings#file-injection-with-typeahead).

### Permissions and sandbox
Reads inside the workspace run without asking; edits and shell commands ask first. Bash approvals are matched per command, and approvals can be kept for the session, the project or everywhere. On macOS, Bash commands run in a Seatbelt sandbox that limits writes to the workspace. See [Permission modes](configuration#permission-modes) and the `sandbox` settings in [Configuration](configuration).

### Hooks
Shell commands that run before and after tool calls, on prompt submit, on stop and at session start and end, using Claude Code's hooks contract. See [Hooks](hooks).

### Instruction files and skills
`AGENTS.md` and `CLAUDE.md` files from the git root down to the working directory are added to the system prompt. Skills in `.claurst/skills`, `.agents/skills` or `.claude/skills` are listed to the model and run with the Skill tool or as slash commands. See [Instruction files](configuration#instruction-files-agentsmd--claudemd) and [Skills](configuration#skills).

### Plugin system
Extend Claurst with plugins (`plugin.toml` or `plugin.json`) that add slash commands, MCP servers and hooks (agents feed sub-agent prompts; skills, LSP servers and output styles are accepted in the manifest but not loaded yet).

### Multi-agent orchestration
Run named agents (`build`, `plan`, `explore`) or spawn sub-agents with the `Agent` tool. Agents communicate via a shared task registry and message channels.

### Goal system
Set a durable objective with `/goal` and Claurst works autonomously across turns until the goal is verified complete — using the `GoalCompleteTool` for audited completion rather than just stopping.

### Managed agents (preview)
Configure a manager-executor architecture with `/managed-agents` where a manager model delegates subtasks to parallel executor agents with full budget split controls.

### Speech modes
Activate `/caveman` or `/rocky` to compress model responses by 40–85%, saving tokens in long sessions. Deactivate with `/normal`.

---

## Quick start

**1. Install**

```bash
# Linux / macOS
curl -fsSL https://github.com/Kuberwastaken/claurst/releases/latest/download/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://github.com/Kuberwastaken/claurst/releases/latest/download/install.ps1 | iex
```

The installer auto-detects your platform/arch, drops `claurst` into
`~/.local/bin` (`$XDG_BIN_HOME` if set; `%LOCALAPPDATA%\Programs\claurst` on Windows), and adds it to your `PATH`. See
[Installation](installation) for flags, manual download, and uninstall steps.

**2. Set your API key**

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

**3. Run interactively**

```bash
claurst
```

Or send a single prompt and exit:

```bash
claurst --print "explain the auth module"
```

---

## Configuration

Claurst reads `~/.claurst/settings.json` at startup. The most common settings:

```json
{
  "config": {
    "model": "claude-opus-4-6",
    "permission_mode": "default"
  }
}
```

See [Configuration](configuration) for the full reference.

---

## Using a different provider

```bash
# Use Ollama with a local model
claurst --provider ollama --model llama3.2

# Use OpenAI
OPENAI_API_KEY=sk-... claurst --provider openai --model gpt-4o
```

See [Providers](providers) for setup instructions for every supported provider,
or the [Local Models](local-models) guide for running against llama.cpp,
LM Studio, Ollama, or any OpenAI-compatible server on your own machine.

---

## Interactive vs headless

| Mode | Command | Use case |
|------|---------|----------|
| Interactive TUI | `claurst` | Day-to-day coding |
| Single prompt | `claurst "task"` | Quick one-shot tasks |
| Headless print | `claurst --print "task"` | Scripts, CI |
| JSON output | `claurst --output-format json "task"` | Machine consumption |
| Stream JSON | `claurst --output-format stream-json "task"` | Real-time piping |
| Structured answer | `claurst -p --output-format json --json-schema schema.json "task"` | A JSON answer that matches a schema |
| Continue a session | `claurst -p --resume <id> "task"` | Multi-step scripts |

Headless runs never prompt for permission, exit with `0` / `1` / `2` / `130`, and are saved like interactive sessions. See [Headless mode](advanced#headless-mode).

---

## Slash commands

Inside the interactive TUI, type `/` to see all available commands. Common ones:

| Command | Description |
|---------|-------------|
| `/help` | Show all commands |
| `/model` | Switch model or provider |
| `/login` | OAuth login (Anthropic; `--codex` for ChatGPT, `--label <name>` to name) |
| `/accounts` | List stored Anthropic + Codex accounts |
| `/switch <id>` | Switch active account (`--codex` for Codex) |
| `/logout` | Clear credentials for the active account (`--all` to purge) |
| `/goal <objective>` | Set an autonomous multi-turn goal |
| `/managed-agents` | Configure manager-executor agents |
| `/compact` | Compress conversation history |
| `/cost` | Token usage and cost for this session |
| `/insights` | Session statistics and tool usage report |
| `/caveman` | Activate telegraphic speech mode (save tokens) |
| `/rocky` | Activate Rocky (Project Hail Mary) speech mode |
| `/normal` | Deactivate speech modes |
| `/rewind` | Go back to a previous message |
| `/copy` | Copy last response to clipboard |
| `/export` | Save session transcript |
| `/think-back` | View thinking traces from previous responses |
| `/ultrareview` | Exhaustive multi-dimensional code review |
| `/advisor <model>` | Set a secondary advisor model |
| `/sandbox-toggle` | Toggle sandboxed shell execution |
| `/update` | Check for and download updates |
| `/exit` | Quit |

See [Slash Commands](commands) for the complete reference.

---

## Next steps

- [Installation](installation) — download, build from source, system requirements
- [Authentication](auth) — API keys and OAuth
- [Configuration](configuration) — settings.json reference
- [Slash Commands](commands) — all 70+ commands
- [Tools Reference](tools) — all 40+ tools and permission levels
- [Providers](providers) — configuring each LLM provider
- [Local Models](local-models) — llama.cpp, LM Studio, Ollama, and other OpenAI-compatible servers
- [MCP Integration](mcp) — Model Context Protocol servers
- [Plugins](plugins) — building and using plugins
- [Agents](agents) — multi-agent orchestration
- [Hooks](hooks) — event-driven automation
- [Advanced Features](advanced) — extended thinking, sessions, headless mode, and more
- [Changelog](changelog) — changes per release
