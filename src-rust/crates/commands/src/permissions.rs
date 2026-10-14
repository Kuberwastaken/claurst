// `/permissions` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;
use claurst_core::permissions::{parse_rule_spec, rule_spec, PermissionAction, PermissionRule};

pub struct PermissionsCommand;

/// Append one titled section of rules to `out` (nothing when empty).
fn push_rules(out: &mut String, title: &str, rules: &[PermissionRule]) {
    if rules.is_empty() {
        return;
    }
    out.push_str(&format!("\n{}:\n", title));
    for rule in rules {
        let action = match rule.action {
            PermissionAction::Allow => "allow",
            PermissionAction::Deny => "deny ",
        };
        out.push_str(&format!("  {}  {}\n", action, rule_spec(rule)));
    }
}

/// Validate a `/permissions allow|deny` argument as a rule spec; the error
/// is the message to show.
fn checked_spec(sub: &str, arg: &str) -> Result<String, String> {
    if arg.is_empty() {
        return Err(format!("Usage: /permissions {} <Tool | Tool(pattern)>", sub));
    }
    if parse_rule_spec(arg, PermissionAction::Allow).is_none() {
        return Err(format!(
            "Invalid rule '{}': expected Tool, Tool(pattern) or * (e.g. Bash(git status *), Edit(/repo/src/**))",
            arg
        ));
    }
    Ok(arg.to_string())
}

// ---- /permissions --------------------------------------------------------

#[async_trait]
impl SlashCommand for PermissionsCommand {
    fn name(&self) -> &str { "permissions" }
    fn description(&self) -> &str { "View or change tool permission settings" }
    fn help(&self) -> &str {
        "Usage: /permissions [list|set <mode>|allow <rule>|deny <rule>|reset]\n\n\
         Modes: default, accept-edits, bypass-permissions, plan\n\
         Rules: Tool, Tool(pattern) or *. Bash patterns match each command of a\n\
         line (Bash(git status *)); other tools take a path glob (Edit(/repo/src/**)).\n\n\
         Examples:\n\
           /permissions                       — list the mode and every rule in effect\n\
           /permissions set accept-edits      — auto-accept file edits in the workspace\n\
           /permissions allow Bash(cargo test *) — allow without prompting\n\
           /permissions deny Write            — refuse a tool and hide it from the model\n\
           /permissions reset                 — clear allow/deny lists and the mode"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();

        if args.is_empty() || args == "list" {
            let mut out = format!(
                "Permission Settings\n\
                 ───────────────────\n\
                 Mode: {:?}\n",
                ctx.config.permission_mode,
            );
            let manager = ctx.permission_manager.as_ref().and_then(|m| m.lock().ok());
            match manager {
                Some(m) => {
                    push_rules(&mut out, "Configured (allowed_tools / disallowed_tools, flags, /permissions)", &m.config_rules);
                    push_rules(&mut out, "Saved for this project", &m.project_rules);
                    push_rules(&mut out, "Saved everywhere (global settings)", &m.persistent_rules);
                    push_rules(&mut out, "This session", &m.session_rules);
                    let empty = m.config_rules.is_empty()
                        && m.project_rules.is_empty()
                        && m.persistent_rules.is_empty()
                        && m.session_rules.is_empty();
                    if empty {
                        out.push_str("\nNo rules: every tool follows the mode.\n");
                    }
                }
                None => {
                    let configured: Vec<PermissionRule> = ctx
                        .config
                        .allowed_tools
                        .iter()
                        .filter_map(|s| parse_rule_spec(s, PermissionAction::Allow))
                        .chain(
                            ctx.config
                                .disallowed_tools
                                .iter()
                                .filter_map(|s| parse_rule_spec(s, PermissionAction::Deny)),
                        )
                        .collect();
                    push_rules(&mut out, "Configured (allowed_tools / disallowed_tools)", &configured);
                }
            }
            out.push_str(
                "\nDeny rules win over allow rules and apply in every mode.\n\
                 Use /permissions allow|deny <rule> to add a rule, /permissions reset to clear the lists.",
            );
            return CommandResult::Message(out);
        }

        let mut parts = args.splitn(2, ' ');
        let sub = parts.next().unwrap_or("").trim();
        let arg = parts.next().unwrap_or("").trim();

        match sub {
            "set" => {
                let mode = match arg.to_lowercase().as_str() {
                    "default" => claurst_core::config::PermissionMode::Default,
                    "accept-edits" | "accept_edits" => claurst_core::config::PermissionMode::AcceptEdits,
                    "bypass-permissions" | "bypass_permissions" => claurst_core::config::PermissionMode::BypassPermissions,
                    "plan" => claurst_core::config::PermissionMode::Plan,
                    _ => return CommandResult::Error(
                        "Mode must be: default, accept-edits, bypass-permissions, or plan".to_string()
                    ),
                };
                let mut new_config = ctx.config.clone();
                new_config.permission_mode = mode.clone();
                if let Err(e) = save_settings_mutation(|s| s.config.permission_mode = mode.clone()) {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!("Permission mode set to {:?}.", mode),
                )
            }
            "allow" => {
                let tool = match checked_spec(sub, arg) {
                    Ok(spec) => spec,
                    Err(message) => return CommandResult::Error(message),
                };
                let mut new_config = ctx.config.clone();
                if !new_config.allowed_tools.contains(&tool) {
                    new_config.allowed_tools.push(tool.clone());
                }
                new_config.disallowed_tools.retain(|t| t != &tool);
                if let Err(e) = save_settings_mutation(|s| {
                    if !s.config.allowed_tools.contains(&tool) {
                        s.config.allowed_tools.push(tool.clone());
                    }
                    s.config.disallowed_tools.retain(|t| t != &tool);
                }) {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                CommandResult::ConfigChangeMessage(new_config, format!("Allowed: {}", tool))
            }
            "deny" => {
                let tool = match checked_spec(sub, arg) {
                    Ok(spec) => spec,
                    Err(message) => return CommandResult::Error(message),
                };
                let mut new_config = ctx.config.clone();
                if !new_config.disallowed_tools.contains(&tool) {
                    new_config.disallowed_tools.push(tool.clone());
                }
                new_config.allowed_tools.retain(|t| t != &tool);
                if let Err(e) = save_settings_mutation(|s| {
                    if !s.config.disallowed_tools.contains(&tool) {
                        s.config.disallowed_tools.push(tool.clone());
                    }
                    s.config.allowed_tools.retain(|t| t != &tool);
                }) {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                CommandResult::ConfigChangeMessage(new_config, format!("Denied: {}", tool))
            }
            "reset" => {
                let mut new_config = ctx.config.clone();
                new_config.allowed_tools.clear();
                new_config.disallowed_tools.clear();
                new_config.permission_mode = claurst_core::config::PermissionMode::Default;
                if let Err(e) = save_settings_mutation(|s| {
                    s.config.allowed_tools.clear();
                    s.config.disallowed_tools.clear();
                    s.config.permission_mode = claurst_core::config::PermissionMode::Default;
                }) {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    "Permissions reset to defaults.".to_string(),
                )
            }
            other => CommandResult::Error(format!(
                "Unknown subcommand '{}'. Use: /permissions [list|set|allow|deny|reset]",
                other
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::config::PermissionMode;
    use claurst_core::permissions::PermissionManager;

    fn ctx_with_manager() -> CommandContext {
        let mut manager = PermissionManager::new(PermissionMode::Default, &Settings::default());
        let config = Config {
            allowed_tools: vec!["Bash(git status *)".to_string()],
            disallowed_tools: vec!["Write".to_string()],
            ..Default::default()
        };
        manager.sync_config(&config);
        manager.add_session_allow_command("Bash", "cargo test");
        CommandContext {
            config,
            cost_tracker: claurst_core::cost::CostTracker::new(),
            messages: vec![],
            working_dir: std::path::PathBuf::from("."),
            session_id: "perm-test".to_string(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            permission_manager: Some(Arc::new(std::sync::Mutex::new(manager))),
        }
    }

    #[tokio::test]
    async fn list_shows_rules_from_every_source() {
        let mut ctx = ctx_with_manager();
        let CommandResult::Message(out) = PermissionsCommand.execute("list", &mut ctx).await else {
            panic!("expected a message");
        };
        assert!(out.contains("allow  Bash(git status *)"), "{out}");
        assert!(out.contains("deny   Write"), "{out}");
        assert!(out.contains("This session:\n  allow  Bash(cargo test)"), "{out}");
    }

    #[tokio::test]
    async fn allow_and_deny_reject_malformed_rules() {
        let mut ctx = ctx_with_manager();
        for args in ["allow Bash(", "deny Ba sh", "allow"] {
            assert!(
                matches!(PermissionsCommand.execute(args, &mut ctx).await, CommandResult::Error(_)),
                "{args}"
            );
        }
    }
}
