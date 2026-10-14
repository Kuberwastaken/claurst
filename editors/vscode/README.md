# Claurst for VS Code

Chat with [Claurst](https://github.com/Kuberwastaken/claurst) without leaving VS Code.

This extension does not reimplement the agent — it spawns `claurst acp` (the
Agent Client Protocol server already built into the `claurst` CLI, see
`src-rust/crates/acp`) as a child process and speaks newline-delimited
JSON-RPC 2.0 to it over stdio. All prompting, tool execution, and permission
logic live in `claurst` itself; this extension is a thin ACP client plus a
webview for rendering the conversation.

## Requirements

- The `claurst` CLI installed and on `PATH` (or configure
  `claurst.executablePath` to an absolute path).
- A workspace folder open (the session's `cwd` is the first workspace folder).

## Commands

- **Claurst: Open Chat** — opens the chat panel and starts a session.
- **Claurst: New Session** — discards the current session and starts fresh.
- **Claurst: Stop Current Turn** — sends `session/cancel` for the in-flight turn.

## Development

```bash
cd editors/vscode
npm install
npm run compile   # or `npm run watch`
```

Then open this directory in VS Code and press F5 to launch an Extension
Development Host with the extension loaded.

## Permission requests

`claurst` evaluates permissions with the same rules as the TUI (workspace
reads, allow rules and the permission mode resolve without asking). When a
tool call still needs approval (e.g. `Bash`, `Edit`), the extension shows a
quick pick with the options the agent offered (allow once, allow always,
reject). "Allow always" is saved by `claurst` as a rule in the global
settings. Dismissing the quick pick without choosing sends the ACP
`cancelled` outcome, so nothing is granted and the tool call fails.

## Scope

This is an MVP: a single chat panel, streamed text/thinking chunks, tool-call
status lines, and permission prompts. It does not yet support multiple
concurrent sessions, inline diffs, or `@file` mentions — contributions welcome.
