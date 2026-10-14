# Plugins

Claurst's plugin system lets you extend the agent with additional slash commands, MCP servers, and lifecycle hooks, packaged in a single directory. The manifest also accepts agents, skills, output styles and LSP servers, but those are not loaded yet (see the field reference).

---

## Plugin Discovery

Plugins are loaded from `~/.claurst/plugins/` (user plugins) and from `<project>/.claurst/plugins/` (project plugins). Each subdirectory that contains a valid `plugin.json` or `plugin.toml` manifest is treated as a plugin. `--bare` skips plugin loading at startup.

Every discovered plugin is enabled. The `enabledPlugins` / `disabledPlugins` settings written by `/plugin enable` and `/plugin disable` are not consulted when plugins load.

```
~/.claurst/plugins/
├── my-plugin/
│   ├── plugin.toml          <- manifest
│   ├── commands/            <- *.md slash command definitions
│   ├── agents/              <- scanned, not used
│   ├── skills/              <- subdirectories with SKILL.md
│   ├── hooks/               <- hooks.json (optional)
│   └── output-styles/       <- scanned, not used
└── another-plugin/
    └── plugin.json
```

Both `plugin.toml` (TOML format) and `plugin.json` (JSON format) are supported; when a directory has both, `plugin.json` is used. The loader normalises camelCase and snake_case field names, so manifests written in either convention are accepted.

### Plugin commands

Every `.md` file under `commands/` (searched recursively) and every path listed in the manifest's `commands` field becomes a slash command named `<plugin>:<subdir>:<file stem>`, e.g. `commands/build/deploy.md` in `my-plugin` is `/my-plugin:build:deploy`. A `SKILL.md` among them becomes `/<plugin>:<its directory name>`. Running the command sends the file's content as a user message, with `Arguments: <args>` appended when arguments are given. Plugin commands are read from disk each time they run.

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

# Extra agent markdown files (parsed, not loaded yet)
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

# Inline LSP server definitions (parsed, not started yet)
[[lsp_servers]]
name    = "pyright"
command = "pyright-langserver"
args    = ["--stdio"]
transport = "stdio"
restart_on_crash = true

[lsp_servers.extension_to_language]
".py" = "python"

# User-configurable options (parsed, not used yet)
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

# Capability grants for plugin slash commands (omit to allow all)
capabilities = ["read_files", "network", "shell"]

# Marketplace identifier (parsed, not used)
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
| `keywords` | array of strings | Tags (parsed, not used) |
| `marketplace_id` | string | Parsed, not used; there is no plugin marketplace |

### Content Declarations

| Field | Type | Description |
|---|---|---|
| `commands` | array of strings | Paths to extra slash command `.md` files or directories, relative to the plugin root. Supplements the `commands/` directory. |
| `agents` | array of strings | Parsed, not used: plugin agents (this field and the `agents/` directory) are not loaded. |
| `skills` | array of strings | Paths to extra skill directories (each must contain a `SKILL.md`). Supplements the `skills/` directory. Plugin skill directories are not yet part of the session skill registry; to use a plugin's skills today, add its `skills/` directory to `skills.paths` (see [Skills](configuration.md#skills)). |
| `output_styles` | array of strings | Parsed, not used: plugin output styles (this field and the `output-styles/` directory) are not loaded. |

### mcp_servers

An array of inline MCP server definitions with `name`, `command`, `args`, `env`, `url` and `type` (see [MCP](mcp.md)); `headers` and the timeout fields are not read from plugins. In `plugin.json` the field can also be written as `"mcpServers"` with an object mapping (the loader converts it to the array form automatically).

Plugin servers are added to the session's server list at startup unless a configured server already has that name. They are treated as user servers, so they launch without the project-server approval prompt, including servers from plugins in a project's `.claurst/plugins/`. `/plugin reload` does not add or remove servers.

### lsp_servers

Parsed and counted in the `/reload-plugins` summary, but not started: the `LSP` tool only uses `config.lsp_servers` from settings (see [LspTool](tools.md#lsptool)). Accepted fields:

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

An inline hooks configuration object (see [Hooks](#hooks) below). A `hooks/hooks.json` file in the plugin directory takes precedence over it.

### user_config

A map of option keys to `PluginUserConfigOption` objects declaring user-configurable settings. The field is parsed but not used: `/plugin info` does not show it and no values are passed to the plugin.

| Field | Type | Description |
|---|---|---|
| `type` | enum | Value type: `"string"`, `"number"`, `"boolean"`, `"directory"`, `"file"` |
| `title` | string | Display label |
| `description` | string | Explanation of the option |
| `required` | bool | Whether the user must supply a value |
| `default` | any | Default value (optional) |
| `sensitive` | bool | When `true`, the value is masked in UI output |

### capabilities

An optional array of capability strings. It is checked only when one of the plugin's slash commands runs: a markdown command needs `"read_files"`. Omit the field to allow everything; an empty array (`[]`) blocks the plugin's markdown commands. Hooks and MCP servers are not restricted by it.

---

## Hooks

Plugins can ship hooks: shell commands that run on session events. Plugin hooks use exactly the same format, events, input and output as settings hooks, and run through the same engine, after the user's settings hooks. See [Hooks](hooks.md) for the events (`PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `SubagentStop`, `SessionStart`, `SessionEnd`, `Notification`, `PreCompact`, `PostModelTurn`), the JSON on stdin, exit code 2 and the stdout JSON fields.

Hooks are read from `hooks/hooks.json` in the plugin directory or, if there is none, from the manifest's inline `hooks` object. Both the plain events map and a wrapped form with a description are accepted:

```json
{
  "description": "Audit and notification hooks",
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit|Write",
        "hooks": [
          { "type": "command", "command": "\"$CLAUDE_PLUGIN_ROOT\"/scripts/lint.sh", "timeout": 30 }
        ]
      }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "notify-send 'Claurst finished'" }] }
    ]
  }
}
```

Entries for events Claurst does not implement are skipped with a warning.

**Trust.** Hooks from plugins in `~/.claurst/plugins/` run like your own hooks. Hooks from plugins found in a project's `.claurst/plugins/` are ignored (a warning is logged and `/plugin info` marks them as not run), for the same reason project settings cannot define hooks: they come from the repository, not from you.

**Environment variables available to plugin hook processes:**

| Variable | Value |
|---|---|
| `CLAUDE_PLUGIN_ROOT` | Absolute path to the plugin directory |
| `CLAUDE_PLUGIN_NAME` | Plugin name from the manifest |
| `CLAUDE_PROJECT_DIR` | The session's working directory |

Plugin hooks are registered when Claurst starts and re-registered by `/plugin reload` and `/reload-plugins` (never under `--bare`). `/hooks` lists them with the plugin's name.

---

## Managing Plugins with /plugin

The `/plugin` slash command manages plugins from within an interactive session:

```
/plugin                      — list all installed plugins (alias: /plugins)
/plugin list                 — list all installed plugins with status
/plugin info <name>          — show version, source, path, commands and hooks of a plugin
/plugin enable <name>        — add the name to enabledPlugins in the global settings
/plugin disable <name>       — add the name to disabledPlugins in the global settings
/plugin install <path>       — copy a local plugin directory to ~/.claurst/plugins/<name>
/plugin reload               — reload plugins from disk (alias: /plugin refresh)
```

`enabledPlugins` and `disabledPlugins` are not read when plugins load, so `/plugin enable` and `/plugin disable` currently have no effect on which plugins run.

### /reload-plugins

```
/reload-plugins
```

Rescans `~/.claurst/plugins/` and `<project>/.claurst/plugins/`, re-reads all manifests, re-registers plugin hooks, and reports what changed. Use this after changing a plugin's hooks or installing a new plugin. Plugin MCP servers are only picked up at startup.

---

## No Plugin Marketplace

There is no plugin marketplace. `/plugin install` accepts only a local directory path, and the manifest's `marketplace_id` is not used.

---

## Example: A Complete Plugin

```toml
# ~/.claurst/plugins/code-quality/plugin.toml

name        = "code-quality"
version     = "0.3.1"
description = "Runs linters and formatters after file edits"
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
        "matcher": "Edit|Write",
        "hooks": [
          {
            "type": "command",
            "command": "f=$(jq -r .tool_input.file_path); eslint --fix \"$f\" >/dev/null 2>&1; prettier --write \"$f\" >/dev/null 2>&1; exit 0"
          }
        ]
      }
    ]
  }
}
```
