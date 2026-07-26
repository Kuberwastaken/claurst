// `/mcp` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct McpCommand;

// ---- /mcp ----------------------------------------------------------------

#[async_trait]
impl SlashCommand for McpCommand {
    fn name(&self) -> &str { "mcp" }
    fn description(&self) -> &str { "Show MCP server status and manage connections" }
    fn help(&self) -> &str {
        "Usage: /mcp [list|status|auth <server>|connect <server>|logs <server>|resources|prompts|get-prompt ...]\n\n\
         Manages Model Context Protocol (MCP) servers.\n\
         MCP servers extend Claurst with external tools, resources, and prompt templates.\n\n\
         Subcommands:\n\
           /mcp                        — list configured servers with live status\n\
           /mcp list                   — same as above\n\
           /mcp status                 — detailed connection status for all servers\n\
           /mcp auth <server>          — show OAuth auth instructions for a server\n\
           /mcp connect <server>       — reconnect a disconnected server\n\
           /mcp logs <server>          — show recent errors/logs for a server\n\
           /mcp resources [server]     — list resources from connected servers\n\
           /mcp prompts [server]       — list prompt templates from connected servers\n\
           /mcp get-prompt <server> <prompt> [key=value ...]  — expand a prompt template\n\n\
         To add/remove MCP servers, edit ~/.claurst/settings.json\n\
         under the 'mcpServers' key.\n\
         Docs: https://docs.anthropic.com/claude-code/mcp"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let sub = args.trim();
        let first_word = sub.split_whitespace().next().unwrap_or("");

        // Delegate live-server subcommands (resources/prompts/get-prompt) to the async helper.
        if matches!(first_word, "resources" | "prompts" | "get-prompt") {
            if let Some(result) = McpCommand::handle_live_subcommand(sub, ctx).await {
                return result;
            }
            // Manager not available — fall through to show configured servers
        }

        // /mcp auth <server-name>
        if first_word == "auth" {
            let server_name = sub["auth".len()..].trim();
            if server_name.is_empty() {
                return CommandResult::Error(
                    "Usage: /mcp auth <server-name>\n\
                     Example: /mcp auth my-server"
                        .to_string(),
                );
            }
            return McpCommand::handle_auth(server_name, ctx).await;
        }

        // /mcp tools [server-name]
        if first_word == "tools" {
            let rest = sub["tools".len()..].trim();
            let server_filter = if rest.is_empty() { None } else { Some(rest) };
            return McpCommand::handle_tools(server_filter, ctx);
        }

        // /mcp connect <server-name>
        if first_word == "connect" {
            let server_name = sub["connect".len()..].trim();
            if server_name.is_empty() {
                return CommandResult::Error(
                    "Usage: /mcp connect <server-name>\n\
                     Example: /mcp connect my-server"
                        .to_string(),
                );
            }
            return McpCommand::handle_connect(server_name, ctx).await;
        }

        // /mcp logs <server-name>
        if first_word == "logs" {
            let server_name = sub["logs".len()..].trim();
            if server_name.is_empty() {
                return CommandResult::Error(
                    "Usage: /mcp logs <server-name>\n\
                     Example: /mcp logs my-server"
                        .to_string(),
                );
            }
            return McpCommand::handle_logs(server_name, ctx);
        }

        if ctx.config.mcp_servers.is_empty() {
            return CommandResult::Message(
                "No MCP servers configured.\n\n\
                 To add a MCP server, edit ~/.claurst/settings.json:\n\
                 {\n\
                   \"mcpServers\": [\n\
                     {\n\
                       \"name\": \"my-server\",\n\
                       \"command\": \"npx\",\n\
                       \"args\": [\"-y\", \"@modelcontextprotocol/server-filesystem\", \"/tmp\"]\n\
                     }\n\
                   ]\n\
                 }\n\n\
                 Docs: https://docs.anthropic.com/claude-code/mcp"
                    .to_string(),
            );
        }

        // /mcp status — detailed status table
        if sub == "status" {
            let mut output = String::from("MCP Server Status\n─────────────────\n");
            for srv in &ctx.config.mcp_servers {
                let kind = srv.server_type.as_str();
                let endpoint = srv
                    .url
                    .as_deref()
                    .or(srv.command.as_deref())
                    .unwrap_or("(unknown)");

                // Fetch live status from the manager if available.
                let live_status = ctx
                    .mcp_manager
                    .as_ref()
                    .map(|m| m.server_status(&srv.name).display())
                    .unwrap_or_else(|| "unknown (manager not active)".to_string());

                output.push_str(&format!(
                    "  {name:20} [{kind:10}] {status}\n    endpoint: {endpoint}\n",
                    name = srv.name,
                    kind = kind,
                    status = live_status,
                    endpoint = endpoint,
                ));
            }
            if ctx.mcp_manager.is_none() {
                output.push_str(
                    "\nNote: MCP manager is not active in this session.\n\
                     Restart Claurst to connect to MCP servers.\n\
                     Use /mcp connect <server> to retry a single server."
                );
            }
            return CommandResult::Message(output);
        }

        // Default: /mcp or /mcp list — show configured servers with live status inline
        let manager = ctx.mcp_manager.as_ref();
        let mut output = format!(
            "Configured MCP Servers ({})\n──────────────────────────\n",
            ctx.config.mcp_servers.len()
        );
        for srv in &ctx.config.mcp_servers {
            let cmd_display = if let Some(ref url) = srv.url {
                format!("url={}", url)
            } else if let Some(ref cmd) = srv.command {
                let args_str = srv.args.join(" ");
                if args_str.is_empty() {
                    cmd.clone()
                } else {
                    format!("{} {}", cmd, args_str)
                }
            } else {
                "(no command)".to_string()
            };

            let status_str = manager
                .map(|m| m.server_status(&srv.name).display())
                .unwrap_or_else(|| "not running".to_string());

            output.push_str(&format!(
                "  {name}  [{status}]\n    type: {type_}  |  {cmd}\n",
                name = srv.name,
                status = status_str,
                type_ = srv.server_type,
                cmd = cmd_display,
            ));
        }
        output.push_str(
            "\nSubcommands: status | auth <server> | connect <server> | logs <server>\n\
             Also: resources | prompts | get-prompt <server> <prompt> [key=val ...]"
        );
        CommandResult::Message(output)
    }
}

impl McpCommand {
    /// Handle `/mcp auth <server>` — initiate OAuth or show auth instructions.
    ///
    /// For HTTP/SSE servers: runs the browser-based OAuth flow, stores the
    /// resulting token, and requests the runtime to reconnect.
    ///
    /// For stdio servers: shows env-var auth instructions.
    async fn handle_auth(server_name: &str, ctx: &CommandContext) -> CommandResult {
        let srv = match ctx.config.mcp_servers.iter().find(|s| s.name == server_name) {
            Some(s) => s,
            None => {
                let configured: Vec<&str> = ctx.config.mcp_servers.iter().map(|s| s.name.as_str()).collect();
                return CommandResult::Error(format!(
                    "No MCP server named '{}' is configured.\n\
                     Configured servers: {}",
                    server_name,
                    if configured.is_empty() { "(none)".to_string() } else { configured.join(", ") }
                ));
            }
        };

        let is_http = matches!(srv.server_type.as_str(), "http" | "sse");

        if !is_http {
            // stdio — env-var / API-key auth
            let env_keys: Vec<&str> = srv.env.keys().map(|k| k.as_str()).collect();
            let env_note = if env_keys.is_empty() {
                "No environment variables configured.".to_string()
            } else {
                format!("Configured env vars: {}", env_keys.join(", "))
            };
            let token_note = match claurst_mcp::oauth::get_mcp_token(server_name) {
                Some(tok) if !tok.is_expired(60) => " (valid token stored)".to_string(),
                Some(_) => " (stored token is expired)".to_string(),
                None => " (no token stored)".to_string(),
            };
            return CommandResult::Message(format!(
                "MCP Server '{}' (stdio){}\n\
                 {}\n\n\
                 stdio servers authenticate via environment variables (API keys etc.).\n\
                 Add required variables to the 'env' block in ~/.claurst/settings.json,\n\
                 then restart Claurst or run /mcp connect {} to reconnect.",
                server_name, token_note, env_note, server_name
            ));
        }

        if let Some(manager) = &ctx.mcp_manager {
            use claurst_mcp::McpServerStatus;
            if matches!(manager.server_status(server_name), McpServerStatus::Connecting) {
                return CommandResult::Message(format!(
                    "MCP server '{}' is currently connecting — try again shortly.",
                    server_name
                ));
            }

            if let Some(run_auth) = &ctx.mcp_auth_runner {
                // In the interactive CLI/TUI we start browser auth in the background
                // so the event loop stays responsive while waiting for the callback.
                match manager.begin_auth(server_name).await {
                    Ok(session) => {
                        let auth_url = session.auth_url.clone();
                        let redirect_uri = session.redirect_uri.clone();
                        run_auth(session);
                        return CommandResult::McpAuthFlow {
                            server_name: server_name.to_string(),
                            auth_url,
                            redirect_uri,
                        };
                    }
                    Err(e) => {
                        let server_url = srv.url.as_deref().unwrap_or("(URL not configured)");
                        return CommandResult::Message(format!(
                            "MCP OAuth — '{}'\n\
                             Could not initiate OAuth flow: {}\n\n\
                             Manual authentication fallback:\n  Open {} in your browser and complete the OAuth flow.\n\
                             Then run /mcp connect {} to reconnect.",
                            server_name, e, server_url, server_name
                        ));
                    }
                }
            }

            match manager.authenticate(server_name).await {
                Ok(result) => {
                    return CommandResult::Message(format!(
                        "MCP OAuth — '{}'\n\
                         Browser authentication completed; token saved to:\n  {}\n\n\
                         The runtime will attempt to reload the MCP connection; if it still does not reconnect, run /mcp connect {} manually.",
                        server_name,
                        result.token_path.display(),
                        server_name
                    ));
                }
                Err(e) => {
                    let server_url = srv.url.as_deref().unwrap_or("(URL not configured)");
                    return CommandResult::Message(format!(
                        "MCP OAuth — '{}'\n\
                         Could not complete OAuth flow: {}\n\n\
                         Manual authentication fallback:\n  Open {} in your browser and complete the OAuth flow.\n\
                         Then run /mcp connect {} to reconnect.",
                        server_name, e, server_url, server_name
                    ));
                }
            }
        }

        // No live manager — static instructions.
        let server_url = srv.url.as_deref().unwrap_or("(URL not configured)");
        let token_note = match claurst_mcp::oauth::get_mcp_token(server_name) {
            Some(tok) if !tok.is_expired(60) => " (valid token stored)".to_string(),
            Some(_) => " (stored token is expired)".to_string(),
            None => " (no token stored)".to_string(),
        };
        CommandResult::Message(format!(
            "MCP OAuth Authentication — '{}'{}\n\
             Server URL: {}\n\n\
             To authenticate:\n\
             1. Open the server URL in your browser and complete OAuth\n\
             2. The token is saved to ~/.claurst/mcp-tokens/{}.json\n\
             3. Restart Claurst — the token will be used automatically\n\n\
             Token storage: ~/.claurst/mcp-tokens/{}.json",
            server_name, token_note, server_url, server_name, server_name
        ))
    }

    /// Handle `/mcp tools [server]` — list available tools.
    fn handle_tools(server_filter: Option<&str>, ctx: &CommandContext) -> CommandResult {
        let manager = match ctx.mcp_manager.as_ref() {
            Some(m) => m,
            None => return CommandResult::Message(
                "MCP manager is not active. No tool information available.\n\
                 Restart Claurst to connect to MCP servers.".to_string()
            ),
        };

        let all_tools = manager.all_tool_definitions();
        let tools: Vec<_> = if let Some(filter) = server_filter {
            all_tools.iter().filter(|(srv, _)| srv.as_str() == filter).collect()
        } else {
            all_tools.iter().collect()
        };

        if tools.is_empty() {
            return CommandResult::Message(if let Some(filter) = server_filter {
                format!("No tools available from server '{}' (not connected or has no tools).", filter)
            } else {
                "No tools available from any connected MCP server.".to_string()
            });
        }

        let title = if let Some(filter) = server_filter {
            format!("MCP Tools — '{}' ({})", filter, tools.len())
        } else {
            format!("MCP Tools — all servers ({})", tools.len())
        };
        let mut out = format!("{}\n{}\n", title, "─".repeat(title.len()));
        let mut last_server = "";
        for (server, tool) in &tools {
            if server.as_str() != last_server && server_filter.is_none() {
                out.push_str(&format!("[{}]\n", server));
                last_server = server.as_str();
            }
            // Strip the "servername_" prefix for display
            let bare = tool.name.strip_prefix(&format!("{}_", server)).unwrap_or(&tool.name);
            let preview: String = tool.description.chars().take(80).collect();
            let ellipsis = if tool.description.len() > 80 { "…" } else { "" };
            out.push_str(&format!("  {}\n    {}{}\n", bare, preview, ellipsis));
        }
        CommandResult::Message(out)
    }

    /// Handle `/mcp connect <server>` — attempt to reconnect a server.
    async fn handle_connect(server_name: &str, ctx: &CommandContext) -> CommandResult {
        // Validate that the server is configured.
        if !ctx.config.mcp_servers.iter().any(|s| s.name == server_name) {
            let names: Vec<&str> = ctx.config.mcp_servers.iter().map(|s| s.name.as_str()).collect();
            return CommandResult::Error(format!(
                "No MCP server named '{}' is configured.\n\
                 Configured servers: {}",
                server_name,
                if names.is_empty() { "(none)".to_string() } else { names.join(", ") }
            ));
        }

        match &ctx.mcp_manager {
            None => {
                // No live manager — give useful instructions.
                CommandResult::Message(format!(
                    "The MCP manager is not running in this session.\n\
                     To connect '{}', restart Claurst — servers connect automatically\n\
                     on startup using the configuration in ~/.claurst/settings.json.\n\
                     \n\
                     If the server requires authentication, run /mcp auth {} first.",
                    server_name, server_name
                ))
            }
            Some(manager) => {
                let current = manager.server_status(server_name);
                use claurst_mcp::McpServerStatus;
                match current {
                    McpServerStatus::Connected { tool_count } => {
                        CommandResult::Message(format!(
                            "MCP server '{}' is already connected ({} tool{} available).",
                            server_name,
                            tool_count,
                            if tool_count == 1 { "" } else { "s" }
                        ))
                    }
                    McpServerStatus::Connecting => {
                        CommandResult::Message(format!(
                            "MCP server '{}' is already in the process of connecting.\n\
                             Check back in a moment.",
                            server_name
                        ))
                    }
                    McpServerStatus::Disconnected { .. } | McpServerStatus::Failed { .. } => {
                        // The McpManager doesn't expose a reconnect method — it's built at
                        // startup.  Inform the user and suggest a restart.
                        CommandResult::Message(format!(
                            "MCP server '{}' is currently disconnected.\n\
                             Status: {}\n\
                             \n\
                             The runtime MCP manager reconnects servers automatically.\n\
                             If the server stays disconnected:\n\
                             1. Check authentication: /mcp auth {}\n\
                             2. Verify the command/URL in ~/.claurst/settings.json\n\
                             3. Restart Claurst to force a full reconnect",
                            server_name,
                            manager.server_status(server_name).display(),
                            server_name
                        ))
                    }
                }
            }
        }
    }

    /// Handle `/mcp logs <server>` — show recent error/log information.
    fn handle_logs(server_name: &str, ctx: &CommandContext) -> CommandResult {
        // Validate server name.
        if !ctx.config.mcp_servers.iter().any(|s| s.name == server_name) {
            let names: Vec<&str> = ctx.config.mcp_servers.iter().map(|s| s.name.as_str()).collect();
            return CommandResult::Error(format!(
                "No MCP server named '{}' is configured.\n\
                 Configured servers: {}",
                server_name,
                if names.is_empty() { "(none)".to_string() } else { names.join(", ") }
            ));
        }

        let mut lines = vec![format!("MCP Server Logs — '{}'\n──────────────────────", server_name)];

        if let Some(manager) = &ctx.mcp_manager {
            use claurst_mcp::McpServerStatus;
            let status = manager.server_status(server_name);
            lines.push(format!("Current status:  {}", status.display()));

            match &status {
                McpServerStatus::Disconnected { last_error: Some(e) } => {
                    lines.push(format!("\nLast connection error:\n  {}", e));
                    lines.push(String::new());
                    lines.push("Troubleshooting:".to_string());
                    lines.push(format!("  /mcp auth {}    — check authentication", server_name));
                    lines.push(format!("  /mcp connect {} — attempt reconnect", server_name));
                }
                McpServerStatus::Failed { error, retry_at } => {
                    lines.push(format!("\nConnection failure:\n  {}", error));
                    let retry_secs = retry_at.saturating_duration_since(std::time::Instant::now()).as_secs();
                    if retry_secs > 0 {
                        lines.push(format!("  Automatic retry in {}s", retry_secs));
                    }
                    let _ = retry_at; // used above
                }
                McpServerStatus::Connected { tool_count } => {
                    lines.push(format!("\nServer is healthy — {} tool{} available.", tool_count, if *tool_count == 1 { "" } else { "s" }));
                    // Show catalog info if available.
                    if let Some(catalog) = manager.server_catalog(server_name) {
                        if !catalog.resources.is_empty() {
                            lines.push(format!("Resources ({}): {}", catalog.resource_count, catalog.resources.join(", ")));
                        }
                        if !catalog.prompts.is_empty() {
                            lines.push(format!("Prompts ({}): {}", catalog.prompt_count, catalog.prompts.join(", ")));
                        }
                    }
                }
                McpServerStatus::Disconnected { last_error: None } => {
                    lines.push("\nServer disconnected cleanly (no error recorded).".to_string());
                    lines.push(format!("Run /mcp connect {} to reconnect.", server_name));
                }
                McpServerStatus::Connecting => {
                    lines.push("\nConnection in progress…".to_string());
                }
            }

            // Show failed server errors from the initial connect_all pass.
            for (name, err) in manager.failed_servers() {
                if name == server_name {
                    lines.push(format!("\nStartup connection error:\n  {}", err));
                    break;
                }
            }
        } else {
            lines.push("MCP manager is not active in this session.".to_string());
            lines.push("Restart Claurst to start the MCP runtime.".to_string());
        }

        // Hint about log files.
        lines.push(String::new());
        lines.push("Note: Detailed stdio output from MCP server processes is not\n\
                    captured by the manager. Run the server command directly in a\n\
                    terminal to see its full output.".to_string());

        CommandResult::Message(lines.join("\n"))
    }
}

// Helper: handle async /mcp resources|prompts|get-prompt subcommands via a separate trait impl.
// These need the mcp_manager from CommandContext.
impl McpCommand {
    async fn handle_live_subcommand(sub: &str, ctx: &CommandContext) -> Option<CommandResult> {
        let manager = ctx.mcp_manager.as_ref()?;
        let parts: Vec<&str> = sub.splitn(4, ' ').collect();
        match parts[0] {
            "resources" => {
                let filter = parts.get(1).copied();
                let resources = manager.list_all_resources(filter).await;
                if resources.is_empty() {
                    return Some(CommandResult::Message(
                        "No resources available (servers may not support resources/list).".to_string()
                    ));
                }
                let mut out = format!("MCP Resources ({})\n──────────────────\n", resources.len());
                for r in &resources {
                    let server = r.get("server").and_then(|v| v.as_str()).unwrap_or("?");
                    let uri = r.get("uri").and_then(|v| v.as_str()).unwrap_or("?");
                    let name = r.get("name").and_then(|v| v.as_str()).unwrap_or(uri);
                    let desc = r.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    if desc.is_empty() {
                        out.push_str(&format!("  [{server}] {name}\n    {uri}\n"));
                    } else {
                        out.push_str(&format!("  [{server}] {name} — {desc}\n    {uri}\n"));
                    }
                }
                Some(CommandResult::Message(out))
            }
            "prompts" => {
                let filter = parts.get(1).copied();
                let prompts = manager.list_all_prompts(filter).await;
                if prompts.is_empty() {
                    return Some(CommandResult::Message(
                        "No prompt templates available (servers may not support prompts/list).".to_string()
                    ));
                }
                let mut out = format!("MCP Prompt Templates ({})\n─────────────────────────\n", prompts.len());
                for p in &prompts {
                    let server = p.get("server").and_then(|v| v.as_str()).unwrap_or("?");
                    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let desc = p.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    let args: Vec<String> = p.get("arguments")
                        .and_then(|a| a.as_array())
                        .map(|arr| arr.iter()
                            .filter_map(|a| a.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
                            .collect())
                        .unwrap_or_default();
                    let args_display = if args.is_empty() { String::new() } else { format!(" ({})", args.join(", ")) };
                    if desc.is_empty() {
                        out.push_str(&format!("  [{server}] {name}{args_display}\n"));
                    } else {
                        out.push_str(&format!("  [{server}] {name}{args_display} — {desc}\n"));
                    }
                }
                out.push_str("\nUse: /mcp get-prompt <server> <prompt> [key=value ...]\n");
                Some(CommandResult::Message(out))
            }
            "get-prompt" => {
                // /mcp get-prompt <server> <prompt-name> [key=val key2=val2 ...]
                let server = match parts.get(1) {
                    Some(s) => *s,
                    None => return Some(CommandResult::Error("Usage: /mcp get-prompt <server> <prompt> [key=value ...]".to_string())),
                };
                let prompt_name = match parts.get(2) {
                    Some(p) => *p,
                    None => return Some(CommandResult::Error("Usage: /mcp get-prompt <server> <prompt> [key=value ...]".to_string())),
                };
                let mut args: std::collections::HashMap<String, String> = std::collections::HashMap::new();
                if let Some(kv_str) = parts.get(3) {
                    for kv in kv_str.split_whitespace() {
                        if let Some((k, v)) = kv.split_once('=') {
                            args.insert(k.to_string(), v.to_string());
                        }
                    }
                }
                let arguments = if args.is_empty() { None } else { Some(args) };
                match manager.get_prompt(server, prompt_name, arguments).await {
                    Ok(result) => {
                        let mut injected = String::new();
                        for msg in &result.messages {
                            let text = match &msg.content {
                                claurst_mcp::PromptMessageContent::Text { text } => text.clone(),
                                claurst_mcp::PromptMessageContent::Image { .. } => "[image]".to_string(),
                                claurst_mcp::PromptMessageContent::Resource { resource } => {
                                    resource.to_string()
                                }
                            };
                            injected.push_str(&format!("[{}]: {}\n", msg.role, text));
                        }
                        Some(CommandResult::UserMessage(injected.trim().to_string()))
                    }
                    Err(e) => Some(CommandResult::Error(format!("Failed to get prompt '{}' from '{}': {}", prompt_name, server, e))),
                }
            }
            _ => None,
        }
    }
}
