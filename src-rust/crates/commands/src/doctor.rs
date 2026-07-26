// `/doctor` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct DoctorCommand;

// ---- /doctor -------------------------------------------------------------

#[async_trait]
impl SlashCommand for DoctorCommand {
    fn name(&self) -> &str { "doctor" }
    fn description(&self) -> &str { "Check system health and diagnose issues" }
    fn help(&self) -> &str {
        "Usage: /doctor\n\
         Runs a comprehensive system diagnostics check:\n\
         - API key validation (live GET /v1/models call)\n\
         - Git availability\n\
         - MCP server connection status\n\
         - Disk space\n\
         - Config file integrity\n\
         - Tool permission summary\n\
         - Claurst version"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let mut lines: Vec<String> = Vec::new();

        // ── Header ─────────────────────────────────────────────────────────
        lines.push(format!(
            "Claurst v{}  |  {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
        ));
        lines.push(String::new());

        // ── API / Auth ──────────────────────────────────────────────────────
        lines.push("Authentication".to_string());
        let anthropic_auth = ctx.config.resolve_anthropic_auth_async().await.unwrap_or((String::new(), false));
        let client_config = claurst_api::client::ClientConfig {
            api_key: anthropic_auth.0,
            api_base: ctx.config.resolve_anthropic_api_base(),
            use_bearer_auth: anthropic_auth.1,
            ..Default::default()
        };
        let provider_registry = claurst_api::ProviderRegistry::from_config(&ctx.config, client_config);
        let provider_id = claurst_core::ProviderId::new(ctx.config.selected_provider_id());
        match provider_registry.get(&provider_id) {
            Some(provider) => match provider.health_check().await {
                Ok(claurst_api::provider_types::ProviderStatus::Healthy) => {
                    lines.push(format!("  ✓ {} is healthy", provider.name()));
                }
                Ok(claurst_api::provider_types::ProviderStatus::Degraded { reason }) => {
                    lines.push(format!("  ⚠ {} is degraded: {}", provider.name(), reason));
                }
                Ok(claurst_api::provider_types::ProviderStatus::Unavailable { reason }) => {
                    lines.push(format!("  ✗ {} is unavailable: {}", provider.name(), reason));
                }
                Err(err) => {
                    lines.push(format!("  ✗ {} health check failed: {}", provider.name(), err));
                }
            },
            None => {
                let hint = claurst_core::config::primary_api_key_env_var_for_provider(
                    ctx.config.selected_provider_id(),
                )
                .map(|env| format!("set {env}"))
                .unwrap_or_else(|| "configure credentials".to_string());
                lines.push(format!(
                    "  ✗ No active provider runtime found — {} or use /connect",
                    hint
                ));
            }
        }
        // Show which model is active
        lines.push(format!("  • Active model: {}", ctx.config.effective_model()));
        lines.push(String::new());

        // ── Git ─────────────────────────────────────────────────────────────
        lines.push("Tools".to_string());
        let git_out = tokio::process::Command::new("git")
            .arg("--version")
            .output()
            .await;
        match git_out {
            Ok(o) if o.status.success() => {
                let ver = String::from_utf8_lossy(&o.stdout).trim().to_string();
                lines.push(format!("  ✓ {ver}"));
            }
            _ => lines.push("  ✗ git not found — many features require git".to_string()),
        }

        // Ripgrep
        let rg_out = tokio::process::Command::new("rg")
            .arg("--version")
            .output()
            .await;
        match rg_out {
            Ok(o) if o.status.success() => {
                let first = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                lines.push(format!("  ✓ ripgrep: {first}"));
            }
            _ => lines.push("  ⚠ ripgrep (rg) not found — Grep tool will fall back to built-in".to_string()),
        }
        lines.push(String::new());

        // ── Disk space ──────────────────────────────────────────────────────
        lines.push("Disk Space".to_string());
        #[cfg(windows)]
        {
            // On Windows use PowerShell to get free space for the current drive
            let ps_out = tokio::process::Command::new("powershell")
                .args(["-NoProfile", "-Command",
                    "Get-PSDrive -Name (Split-Path -Qualifier (Get-Location)) | \
                     Select-Object Name,@{N='Used(GB)';E={[math]::Round($_.Used/1GB,1)}},\
                     @{N='Free(GB)';E={[math]::Round($_.Free/1GB,1)}} | Format-Table -HideTableHeaders"])
                .output()
                .await;
            match ps_out {
                Ok(o) if o.status.success() => {
                    let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if out.is_empty() {
                        lines.push("  • Disk info unavailable".to_string());
                    } else {
                        for l in out.lines().take(3) {
                            lines.push(format!("  • {}", l.trim()));
                        }
                    }
                }
                _ => lines.push("  ⚠ Could not query disk space".to_string()),
            }
        }
        #[cfg(not(windows))]
        {
            let df_out = tokio::process::Command::new("df")
                .args(["-h", "."])
                .output()
                .await;
            match df_out {
                Ok(o) if o.status.success() => {
                    let out = String::from_utf8_lossy(&o.stdout);
                    // Print the header + the first data line (current filesystem)
                    for (i, l) in out.lines().enumerate().take(2) {
                        if i == 0 {
                            lines.push(format!("  • {}", l));
                        } else {
                            lines.push(format!("  ✓ {}", l));
                        }
                    }
                }
                _ => lines.push("  ⚠ Could not query disk space (`df -h .` failed)".to_string()),
            }
        }
        lines.push(String::new());

        // ── Config directory ────────────────────────────────────────────────
        lines.push("Configuration".to_string());
        let config_dir = claurst_core::config::Settings::config_dir();
        if config_dir.exists() {
            lines.push(format!("  ✓ Config dir: {}", config_dir.display()));
        } else {
            lines.push(format!("  ✗ Config dir missing: {}", config_dir.display()));
        }

        // Settings validation — try loading ~/.claurst/settings.json
        let settings_path = config_dir.join("settings.json");
        if settings_path.exists() {
            match std::fs::read_to_string(&settings_path)
                .ok()
                .and_then(|s| serde_json::from_str::<claurst_core::config::Settings>(&s).ok())
            {
                Some(_) => lines.push("  ✓ settings.json valid".to_string()),
                None => {
                    // Try as raw JSON to distinguish missing vs invalid
                    match std::fs::read_to_string(&settings_path)
                        .ok()
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    {
                        Some(_) => lines.push(
                            "  ⚠ settings.json is JSON but has unexpected structure".to_string()
                        ),
                        None => lines.push(
                            "  ✗ settings.json is invalid JSON — run /config to repair".to_string()
                        ),
                    }
                }
            }
        } else {
            lines.push("  • settings.json not found (defaults will be used)".to_string());
        }

        // AGENTS.md
        let claude_md = ctx.working_dir.join("AGENTS.md");
        if claude_md.exists() {
            lines.push("  ✓ AGENTS.md present in working directory".to_string());
        } else {
            lines.push("  • No AGENTS.md in working directory (run /init to create one)".to_string());
        }
        lines.push(String::new());

        // ── MCP servers ─────────────────────────────────────────────────────
        lines.push("MCP Servers".to_string());
        let mcp_count = ctx.config.mcp_servers.len();
        if mcp_count == 0 {
            lines.push("  • No MCP servers configured".to_string());
        } else if let Some(mgr) = ctx.mcp_manager.as_ref() {
            // Report live connection status from the manager
            let statuses = mgr.all_statuses();
            for srv in ctx.config.mcp_servers.iter().take(12) {
                let status_str = match statuses.get(&srv.name) {
                    Some(claurst_mcp::McpServerStatus::Connected { tool_count }) => {
                        format!("  ✓ {} — connected ({} tool{})",
                            srv.name, tool_count, if *tool_count == 1 { "" } else { "s" })
                    }
                    Some(claurst_mcp::McpServerStatus::Connecting) => {
                        format!("  ⚠ {} — connecting…", srv.name)
                    }
                    Some(claurst_mcp::McpServerStatus::Disconnected { last_error: Some(e) }) => {
                        format!("  ✗ {} — failed: {}", srv.name, e)
                    }
                    Some(claurst_mcp::McpServerStatus::Disconnected { last_error: None }) => {
                        format!("  ✗ {} — disconnected", srv.name)
                    }
                    Some(claurst_mcp::McpServerStatus::Failed { error, .. }) => {
                        format!("  ✗ {} — failed: {}", srv.name, error)
                    }
                    None => format!("  ⚠ {} — not started", srv.name),
                };
                lines.push(status_str);
            }
            if mcp_count > 12 {
                lines.push(format!("    … and {} more", mcp_count - 12));
            }
        } else {
            // No live manager — just show configured names
            lines.push(format!("  ✓ {mcp_count} MCP server(s) configured (not yet connected):"));
            for srv in ctx.config.mcp_servers.iter().take(8) {
                lines.push(format!("    - {}", srv.name));
            }
            if mcp_count > 8 {
                lines.push(format!("    … and {} more", mcp_count - 8));
            }
        }
        lines.push(String::new());

        // ── Hooks ───────────────────────────────────────────────────────────
        lines.push("Hooks".to_string());
        let hook_count: usize = ctx.config.hooks.values().map(|v| v.len()).sum();
        if hook_count == 0 {
            lines.push("  • No hooks configured".to_string());
        } else {
            lines.push(format!("  ✓ {hook_count} hook(s) configured across {} event(s)",
                ctx.config.hooks.len()));
        }
        lines.push(String::new());

        // ── Tool permissions ─────────────────────────────────────────────────
        lines.push("Tool Permissions".to_string());
        let all_tool_names: Vec<String> = claurst_tools::all_tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        let total_tools = all_tool_names.len();
        let allowed_count = ctx.config.allowed_tools.len();
        let denied_count = ctx.config.disallowed_tools.len();
        // Tools not in allowed or denied lists require user confirmation
        let explicit_tools: std::collections::HashSet<&str> = ctx.config.allowed_tools.iter()
            .chain(ctx.config.disallowed_tools.iter())
            .map(|s| s.as_str())
            .collect();
        let confirm_count = all_tool_names.iter()
            .filter(|n| !explicit_tools.contains(n.as_str()))
            .count();
        let mode_label = match ctx.config.permission_mode {
            claurst_core::PermissionMode::BypassPermissions => "bypass-permissions (no confirmation required)",
            claurst_core::PermissionMode::AcceptEdits => "accept-edits (file edits auto-approved)",
            claurst_core::PermissionMode::Plan => "plan (read-only, no writes)",
            claurst_core::PermissionMode::Default => "default (confirm destructive actions)",
        };
        lines.push(format!("  • Mode: {mode_label}"));
        lines.push(format!("  • Total built-in tools: {total_tools}"));
        if allowed_count > 0 {
            lines.push(format!("  ✓ Always allowed: {} tool(s) — {}",
                allowed_count,
                ctx.config.allowed_tools.join(", ")));
        }
        if denied_count > 0 {
            lines.push(format!("  ✗ Always denied: {} tool(s) — {}",
                denied_count,
                ctx.config.disallowed_tools.join(", ")));
        }
        if ctx.config.permission_mode == claurst_core::PermissionMode::Default {
            lines.push(format!("  ⚠ Require confirmation: {} tool(s)", confirm_count));
        }
        lines.push(String::new());

        // ── Session / lock ──────────────────────────────────────────────────
        lines.push("Session".to_string());
        let lock_path = config_dir.join("claude.lock");
        if lock_path.exists() {
            lines.push("  ⚠ Lock file exists — another instance may be running".to_string());
        } else {
            lines.push("  ✓ No stale lock file".to_string());
        }
        lines.push(format!("  • Session ID: {}", ctx.session_id));
        lines.push(format!("  • Working dir: {}", ctx.working_dir.display()));

        CommandResult::Message(lines.join("\n"))
    }
}
