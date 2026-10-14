# Agents and Multi-Agent Features

Claurst has a named-agent system that lets you select a pre-configured persona with its own tool permissions, model, system prompt, and turn budget. For larger tasks the model can spawn sub-agents with the `Agent` tool, and `/managed-agents` configures a manager-executor split.

---

## Built-in Named Agents

Three agents ship by default. Their definitions can be overridden per-user in `~/.claurst/settings.json`.

### build

Full tool access. Intended for implementing features and fixing bugs.

| Property | Value |
|---|---|
| Access | `full` — all tools available |
| Max turns | Unlimited (uses the global default) |
| Display color | Cyan |

Default system prompt prefix:

> You are the build agent. You have full access to read, write, and execute. Focus on implementing the requested changes completely and correctly.

### plan

Read-only analysis. Cannot write files or execute commands. Intended for understanding codebases and planning changes before committing to implementation.

| Property | Value |
|---|---|
| Access | `read-only` — file reads, no writes or shell execution |
| Max turns | 20 |
| Display color | Yellow |

Default system prompt prefix:

> You are the plan agent. You can read files and analyze code but cannot write files or execute commands. Focus on understanding the codebase and describing what changes should be made.

### explore

Fast search-only exploration. Intended for quickly locating relevant code and answering questions about structure.

| Property | Value |
|---|---|
| Access | `search-only` — search tools only |
| Max turns | 15 |
| Display color | Green |

Default system prompt prefix:

> You are the explore agent. You can search and read files. Focus on quickly finding relevant code and answering questions about the codebase.

---

## Selecting an Agent with --agent

Pass `--agent <name>` to activate a named agent for a session:

```
claurst --agent build "implement the OAuth2 login flow"
claurst --agent plan "analyze the database schema and suggest improvements"
claurst --agent explore "find all usages of the deprecated config API"
```

The `--agent` flag can be combined with `--provider` and `--model`:

```
claurst --agent plan --provider openai --model o3 "review this architecture"
```

An unknown name prints a warning and runs without an agent. In the interactive TUI, pressing `Tab` on an empty prompt (while no response is streaming) toggles between the `build` and `plan` agents.

Access levels filter the tool list: `read-only` keeps tools whose permission level is read-only or none, plus `AskUserQuestion`; `search-only` keeps only `Grep`, `Glob`, `Read`, `WebSearch`, and `WebFetch`.

---

## The /agent and /agents Commands

Within an interactive session, `/agent` lists all available named agents (built-in and custom) and `/agent <name>` shows one agent's details:

```
/agent
/agent plan
```

The list shows the agent name, description, access level, and max turn limit. Agents with `visible: false` in their definition are hidden from this list. `/agent` does not switch agents; start Claurst with `--agent <name>` (or use `Tab`, above).

`/agents` opens a separate TUI menu that browses and edits Markdown agent files in `~/.claurst/agents/` and `<project>/.claurst/agents/` (front-matter keys `name`, `description`, `model`, `tools`, `memory_scope`). These files are not the `settings.json` definitions below and are not selectable with `--agent`.

---

## Custom Agent Definitions

Define custom agents in `~/.claurst/settings.json` under the `agents` key. Custom definitions override built-in agents of the same name.

```json
{
  "agents": {
    "review": {
      "description": "Senior code reviewer focused on correctness and security",
      "model": "anthropic/claude-opus-4-6",
      "temperature": 0.3,
      "prompt": "You are a senior software engineer performing code review. Focus on correctness, security vulnerabilities, performance issues, and maintainability. Be specific about problems and suggest concrete fixes.",
      "access": "read-only",
      "visible": true,
      "max_turns": 30,
      "color": "magenta"
    },
    "test-writer": {
      "description": "Writes comprehensive unit and integration tests",
      "model": "anthropic/claude-sonnet-4-6",
      "prompt": "You are a test engineer. Write thorough tests covering happy paths, edge cases, and error conditions. Use the project's existing test framework and conventions.",
      "access": "full",
      "visible": true,
      "max_turns": null,
      "color": "blue"
    },
    "docs": {
      "description": "Technical documentation writer",
      "model": "anthropic/claude-sonnet-4-6",
      "temperature": 0.5,
      "prompt": "You are a technical writer. Write clear, accurate documentation for the code you are given. Use the project's existing documentation style.",
      "access": "read-only",
      "visible": true,
      "max_turns": 25,
      "color": "cyan"
    }
  }
}
```

### AgentDefinition Fields

| Field | Type | Description |
|---|---|---|
| `description` | string | Short description shown in `/agent` |
| `model` | string | Model override in `provider/model` or bare `model` form. Omit to use the session default. |
| `temperature` | number | Sampling temperature override (0.0–1.0). Omit to use the model default. |
| `prompt` | string | System prompt prefix prepended before the main system prompt. |
| `access` | string | Permission restriction: `"full"` (all tools), `"read-only"` (no writes/shell), `"search-only"` (search tools only). Default: `"full"`. |
| `visible` | bool | Whether to show in `/agent` output. Default: `true`. |
| `max_turns` | number or null | Maximum agentic turns. Null means unlimited. Overrides the global turn budget. |
| `color` | string | ANSI terminal color for display: `"cyan"`, `"magenta"`, `"green"`, `"yellow"`, `"blue"`, etc. |

Use the agent with the `--agent` flag:

```
claurst --agent review "check the authentication module for security issues"
claurst --agent test-writer "write tests for the payment processor"
```

---

## Sub-agents and Task Tools

The `Agent` tool lets the model spawn a sub-agent with its own prompt, optional tool list, model, turn cap (default 10), `isolation: "worktree"`, and `run_in_background`. `SendMessage`, `TeamCreate`, and `TeamDelete` are also in the standard tool set. See [Tools](tools.md) for each tool's parameters.

The task tools (`TaskCreate`, `TaskGet`, `TaskUpdate`, `TaskList`, `TaskStop`, `TaskOutput`) are part of the standard tool set in every session. `/tasks` (alias `/bashes`) asks the model to list current tasks with `TaskList`.

### Coordinator mode (not wired)

`CLAURST_COORDINATOR_MODE` is defined in `crates/query/src/coordinator.rs`, but nothing in the running agent loop reads it: no coordinator prompt is injected, no tools are withheld from workers, and resumed sessions are not mode-matched. Setting it has no effect. See [Local Models](local-models.md#is-claurst_coordinator_mode1-a-real-thing).

---

## Agent Definitions in settings.json: Complete Example

```json
{
  "provider": "anthropic",
  "agents": {
    "build": {
      "description": "Full-access implementation agent",
      "model": "anthropic/claude-sonnet-4-6",
      "prompt": "You are the build agent. Implement requested changes completely. Prefer targeted, minimal edits over rewrites.",
      "access": "full",
      "visible": true,
      "max_turns": null,
      "color": "cyan"
    },
    "plan": {
      "description": "Read-only analysis and planning agent",
      "model": "anthropic/claude-opus-4-6",
      "temperature": 0.2,
      "prompt": "You are the plan agent. Analyse the codebase carefully before producing a detailed, step-by-step implementation plan. Do not write or execute anything.",
      "access": "read-only",
      "visible": true,
      "max_turns": 20,
      "color": "yellow"
    },
    "explore": {
      "description": "Fast search-only exploration agent",
      "model": "anthropic/claude-haiku-4-5-20251001",
      "prompt": "You are the explore agent. Search and read files to answer questions quickly.",
      "access": "search-only",
      "visible": true,
      "max_turns": 15,
      "color": "green"
    },
    "security": {
      "description": "Security-focused read-only audit agent",
      "model": "anthropic/claude-opus-4-6",
      "temperature": 0.1,
      "prompt": "You are a security auditor. Look for authentication flaws, injection vulnerabilities, insecure dependencies, and data-exposure risks. Report findings with severity levels (critical, high, medium, low) and concrete remediation steps.",
      "access": "read-only",
      "visible": true,
      "max_turns": 40,
      "color": "magenta"
    },
    "architect": {
      "description": "System design and architecture advisor",
      "model": "anthropic/claude-opus-4-6",
      "temperature": 0.4,
      "prompt": "You are a software architect. Reason carefully about system design trade-offs, scalability, maintainability, and technical debt. Produce clear architectural recommendations with rationale.",
      "access": "read-only",
      "visible": true,
      "max_turns": 30,
      "color": "blue"
    }
  }
}
```

---

## Managed Agents (Preview)

Managed agents provide a **manager-executor** architecture: you configure separate models, turn budgets, concurrency limits, and budget splits for the manager and for executors. When enabled, the session runs on the manager model and gets orchestration instructions in its system prompt.

### Enabling and configuring

```
/managed-agents presets                               — list presets
/managed-agents preset <name>                         — apply a preset
/managed-agents configure manager-model  anthropic/claude-opus-4-6
/managed-agents configure executor-model anthropic/claude-sonnet-4-6
/managed-agents configure concurrent     3
/managed-agents budget 5.00
/managed-agents enable
```

See [Managed Agents](./advanced.md#managed-agents) in the advanced guide for the full configuration reference.
