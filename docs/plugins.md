# Plugins

Claurst's plugin system lets you extend the agent with additional slash commands, agents, skills, MCP servers, LSP servers, and lifecycle hooks — all packaged in a single directory.

---

## Plugin Discovery

Plugins are loaded from the `~/.claurst/plugins/` directory. Each subdirectory that contains a valid `plugin.toml` or `plugin.json` manifest is treated as a plugin.

```
~/.claurst/plugins/
├── my-plugin/
│   ├── plugin.toml          <- manifest
│   ├── commands/            <- *.md slash command definitions
│   ├── agents/              <- *.md agent definitions
│   ├── skills/              <- subdirectories with SKILL.md
│   ├── hooks/               <- hooks.json (optional)
│   └── output-styles/       <- *.md or *.json style definitions
└── another-plugin/
    └── plugin.json
```

Both `plugin.toml` (TOML format) and `plugin.json` (JSON format) are supported. The loader normalises camelCase and snake_case field names, so manifests written in either convention are accepted.

---

## Plugin Manifest Format

### plugin.toml

```toml
name        = "my-plugin"
version     = "1.0.0"
description = "Adds custom commands and hooks for my workflow"
license     = "MIT"
keywords    = ["formatting", "git"]

[author]
name  = "Your Name"
email = "you@example.com"
url   = "https://example.com"

homepage   = "https://example.com/my-plugin"
repository = "https://github.com/you/my-plugin"

# Extra command files beyond the commands/ directory
commands = ["./extra/review.md"]

# Extra agent markdown files beyond the agents/ directory
agents = ["./agents/reviewer.md"]

# Extra skill directories beyond the skills/ directory
skills = ["./extra-skills/"]

# Inline MCP server definitions
[[mcp_servers]]
name    = "my-tool-server"
command = "npx"
args    = ["-y", "my-mcp-server"]
type    = "stdio"

[mcp_servers.env]
API_TOKEN = "${MY_SERVICE_TOKEN}"

# Inline LSP server definitions
[[lsp_servers]]
name    = "pyright"
command = "pyright-langserver"
args    = ["--stdio"]
transport = "stdio"
restart_on_crash = true

[lsp_servers.extension_to_language]
".py" = "python"

# User-configurable options (surfaced in /plugin info)
[user_config.api_token]
type        = "string"
title       = "API Token"
description = "Token for the upstream service"
required    = true
sensitive   = true

[user_config.max_results]
type        = "number"
title       = "Max Results"
description = "Maximum items to return per query"
default     = 20

# Capability grants (omit to allow all)
capabilities = ["read_files", "network", "shell"]

# Marketplace identifier
marketplace_id = "you/my-plugin"
```

### plugin.json

```json
{
  "name": "my-plugin",
  "version": "1.0.0",
  "description": "Adds custom commands and hooks for my workflow",
  "author": {
    "name": "Your Name",
    "email": "you@example.com"
  },
  "homepage": "https://example.com/my-plugin",
  "repository": "https://github.com/you/my-plugin",
  "license": "MIT",
  "keywords": ["formatting", "git"],
  "commands": ["./extra/review.md"],
  "agents": ["./agents/reviewer.md"],
  "skills": ["./extra-skills/"],
  "mcpServers": {
    "my-tool-server": {
      "command": "npx",
      "args": ["-y", "my-mcp-server"],
      "type": "stdio",
      "env": {
        "API_TOKEN": "${MY_SERVICE_TOKEN}"
      }
    }
  },
  "userConfig": {
    "api_token": {
      "type": "string",
      "title": "API Token",
      "description": "Token for the upstream service",
      "required": true,
      "sensitive": true
    },
    "max_results": {
      "type": "number",
      "title": "Max Results",
      "description": "Maximum items to return per query",
      "default": 20
    }
  },
  "capabilities": ["read_files", "network", "shell"],
  "marketplaceId": "you/my-plugin"
}
```

---

## Manifest Fields Reference

### Required

| Field | Type | Description |
|---|---|---|
| `name` | string | Plugin name. Must be non-empty and contain no spaces (use kebab-case). |

### Metadata (optional)

| Field | Type | Description |
|---|---|---|
| `version` | string | Plugin version string |
| `description` | string | Human-readable description |
| `author` | object | `name`, `email` (optional), `url` (optional) |
| `homepage` | string | URL for the plugin's home page |
| `repository` | string | URL for the source repository |
| `license` | string | SPDX license identifier (e.g. `"MIT"`) |
| `keywords` | array of strings | Tags used in marketplace search |
| `marketplace_id` | string | Unique identifier in the plugin marketplace (e.g. `"author/plugin-name"`) |

### Content Declarations

| Field | Type | Description |
|---|---|---|
| `commands` | array of strings | Paths to extra slash command `.md` files or directories, relative to the plugin root. Supplements the `commands/` directory. |
| `agents` | array of strings | Paths to extra agent `.md` files. Supplements the `agents/` directory. |
| `skills` | array of strings | Paths to extra skill directories (each must contain a `SKILL.md`). Supplements the `skills/` directory. |
| `output_styles` | array of strings | Paths to extra output style definitions. |

### mcp_servers

An array of inline MCP server definitions. Each entry is identical to an `McpServerConfig` (see the MCP documentation). In `plugin.json` the field can also be written as `"mcpServers"` with an object mapping (the loader converts it to the array form automatically).

### lsp_servers

An array of LSP server definitions for language-aware editing support:

| Field | Type | Description |
|---|---|---|
| `name` | string | Server identifier |
| `command` | string | Executable to launch |
| `args` | array | Command-line arguments |
| `extension_to_language` | object | Map of file extension → LSP language ID |
| `transport` | string | `"stdio"` (default) |
| `env` | object | Extra environment variables |
| `workspace_folder` | string | Optional workspace path |
| `startup_timeout` | number | Milliseconds to wait for server readiness |
| `shutdown_timeout` | number | Milliseconds to wait for clean shutdown |
| `restart_on_crash` | bool | Automatically restart on unexpected exit |
| `max_restarts` | number | Maximum restart attempts |

### hooks

Either a path string pointing to a `hooks.json` file inside the plugin directory, or an inline hooks configuration object (see the Hooks section below).

### user_config

A map of option keys to `PluginUserConfigOption` objects, allowing the plugin to declare user-configurable settings that are surfaced by `/plugin info`:

| Field | Type | Description |
|---|---|---|
| `type` | enum | Value type: `"string"`, `"number"`, `"boolean"`, `"directory"`, `"file"` |
| `title` | string | Display label |
| `description` | string | Explanation of the option |
| `required` | bool | Whether the user must supply a value |
| `default` | any | Default value (optional) |
| `sensitive` | bool | When `true`, the value is masked in UI output |

### capabilities

An optional array of capability category strings. When present, the plugin is restricted to only those categories. Omit the field entirely to allow all capabilities (backwards compatibility behaviour). An empty array (`[]`) grants no capabilities.

Known categories: `"read_files"`, `"write_files"`, `"network"`, `"shell"`, `"browser"`, `"mcp"`.

---

## Hook Events

Plugins can run shell commands in response to lifecycle events. Hooks receive a JSON payload on stdin describing the event.

### Available Events

| Event | When it fires |
|---|---|
| `PreToolUse` | Before any tool is executed |
| `PostToolUse` | After a tool returns its result |
| `PostToolUseFailure` | After a tool call throws an error |
| `PermissionDenied` | When a permission request is rejected |
| `PermissionRequest` | When a permission is requested (before decision) |
| `Notification` | General notification from the agent |
| `UserPromptSubmit` | When the user submits a prompt |
| `SessionStart` | At the beginning of a session |
| `SessionEnd` | At clean session shutdown |
| `Stop` | When the model finishes its turn |
| `StopFailure` | When the stop sequence fails |
| `SubagentStart` | When a sub-agent is spawned |
| `SubagentStop` | When a sub-agent finishes |
| `PreCompact` | Before context compaction |
| `PostCompact` | After context compaction |
| `Setup` | During plugin setup phase |
| `TeammateIdle` | When a teammate agent becomes idle |
| `TaskCreated` | When a task is created |
| `TaskCompleted` | When a task finishes |
| `Elicitation` | When the model requests clarification |
| `ElicitationResult` | When elicitation receives a response |
| `ConfigChange` | When configuration is modified |
| `WorktreeCreate` | When a git worktree is created |
| `WorktreeRemove` | When a git worktree is removed |
| `InstructionsLoaded` | When CLAUDE.md / instructions are loaded |
| `CwdChanged` | When the working directory changes |
| `FileChanged` | When a watched file changes |

### HookEntry Fields

Each hook entry in a hooks configuration:

| Field | Type | Description |
|---|---|---|
| `command` | string | Shell command to run. Receives event JSON on stdin. |
| `matcher` | string | Optional tool-name filter. Supports `*` wildcard (e.g. `"File*"`, `"*Tool"`). Only relevant for `PreToolUse` / `PostToolUse`. |
| `blocking` | bool | If `true`, a non-zero exit code blocks the operation. Non-blocking hooks (default) only log a warning on failure. |

### Hooks Configuration Format

Hooks can be defined inline in the manifest or in a separate `hooks/hooks.json` file. Both the flat form and the wrapped form are accepted:

**Flat form:**

```json
{
  "PreToolUse": [
    {
      "matcher": "Bash",
      "hooks": [
        {
          "command": "echo \"About to run Bash tool\" >&2",
          "blocking": false
        }
      ]
    }
  ],
  "Stop": [
    {
      "hooks": [
        {
          "command": "notify-send 'Claurst finished'",
          "blocking": false
        }
      ]
    }
  ]
}
```

**Wrapped form (with description):**

```json
{
  "description": "Audit and notification hooks",
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit",
        "hooks": [
          {
            "command": "python3 lint_check.py",
            "blocking": true
          }
        ]
      }
    ]
  }
}
```

When a blocking hook exits non-zero, Claurst denies the operation and reports the hook's stderr as the reason.

**Environment variables available to hook processes:**

| Variable | Value |
|---|---|
| `CLAUDE_PLUGIN_ROOT` | Absolute path to the plugin directory |
| `CLAUDE_PLUGIN_NAME` | Plugin name from the manifest |
| `CLAUDE_TOOL_NAME` | Tool name (PostToolUse hooks only) |
| `CLAUDE_TOOL_INPUT` | Tool input as JSON string (PostToolUse hooks only) |
| `CLAUDE_TOOL_RESULT` | Tool result as JSON string (PostToolUse hooks only) |

---

## Managing Plugins with /plugin

The `/plugin` slash command manages plugins from within an interactive session:

```
/plugin                      — list all installed plugins
/plugin list                 — list all installed plugins with status
/plugin info <name>          — show detailed info about a plugin
/plugin enable <name>        — enable a plugin (persisted to settings)
/plugin disable <name>       — disable a plugin (persisted to settings)
/plugin install <path>       — install a plugin from a local directory
/plugin reload               — reload all plugins from disk
```

After enabling or disabling a plugin, run `/plugin reload` or use `/reload-plugins` to apply changes in the current session without restarting.

### /reload-plugins

```
/reload-plugins
```

Rescans `~/.claurst/plugins/`, re-reads all manifests, and refreshes the active hook registry, commands, agents, skills, and MCP server definitions. Use this after making changes to a plugin directory or after installing a new plugin.

---

## Plugin Marketplace Integration

Plugins published to the Claurst marketplace have a `marketplace_id` field in their manifest (e.g. `"author/plugin-name"`). The marketplace integration allows:

- Browsing available plugins
- Installing plugins by ID
- Updating installed plugins to newer versions

```
/plugin install author/plugin-name     — install from the marketplace
```

Locally installed plugins (via a file path) do not require a `marketplace_id`.

---

## Example: A Complete Plugin

```toml
# ~/.claurst/plugins/code-quality/plugin.toml

name        = "code-quality"
version     = "0.3.1"
description = "Runs linters and formatters as blocking pre-tool hooks"
license     = "MIT"
keywords    = ["lint", "format", "quality"]

[author]
name = "Dev Team"

capabilities = ["shell", "read_files"]

[user_config.fail_on_warning]
type        = "boolean"
title       = "Fail on Warnings"
description = "Treat linter warnings as errors"
default     = false
```

```json
// ~/.claurst/plugins/code-quality/hooks/hooks.json
{
  "description": "Lint and format on file edits",
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit",
        "hooks": [
          {
            "command": "eslint --fix \"$CLAUDE_TOOL_INPUT\" 2>&1 || true",
            "blocking": false
          }
        ]
      },
      {
        "matcher": "Write",
        "hooks": [
          {
            "command": "prettier --write \"$CLAUDE_TOOL_INPUT\" 2>&1 || true",
            "blocking": false
          }
        ]
      }
    ]
  }
}
```
