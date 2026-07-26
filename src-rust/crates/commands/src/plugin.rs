// Plugin commands: `/plugin`, `/reload-plugins`, and the plugin slash adapter.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct PluginCommand;
pub struct ReloadPluginsCommand;

// ---- /plugin -------------------------------------------------------------

#[async_trait]
impl SlashCommand for PluginCommand {
    fn name(&self) -> &str { "plugin" }
    fn aliases(&self) -> Vec<&str> { vec!["plugins"] }
    fn description(&self) -> &str { "Manage plugins" }
    fn help(&self) -> &str {
        "Usage: /plugin [list|info <name>|enable <name>|disable <name>|install <path>|reload]\n\
         Manage Claurst plugins.\n\n\
         Subcommands:\n\
           /plugin              — list all installed plugins\n\
           /plugin list         — list all installed plugins\n\
           /plugin info <name>  — show detailed info about a plugin\n\
           /plugin enable <name>   — enable a plugin (persisted to settings)\n\
           /plugin disable <name>  — disable a plugin (persisted to settings)\n\
           /plugin install <path>  — install a plugin from a local directory\n\
           /plugin reload       — reload plugins from disk"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let project_dir = ctx.working_dir.clone();

        // Helper: prefer the already-loaded global registry, falling back to a
        // fresh disk scan so the command still works without the global being set.
        async fn get_registry(
            project_dir: &std::path::Path,
        ) -> claurst_plugins::PluginRegistry {
            if let Some(global) = claurst_plugins::global_plugin_registry() {
                let mut reg = claurst_plugins::PluginRegistry::new();
                for p in global.all() {
                    reg.insert(p.clone());
                }
                reg
            } else {
                claurst_plugins::load_plugins(project_dir, &[]).await
            }
        }

        let parsed = claurst_plugins::parse_plugin_args(args);
        match parsed {
            claurst_plugins::PluginSubCommand::List => {
                let registry = get_registry(&project_dir).await;
                CommandResult::Message(claurst_plugins::format_plugin_list(&registry))
            }
            claurst_plugins::PluginSubCommand::Enable(ref name) if name.is_empty() => {
                CommandResult::Error(
                    "Usage: /plugin enable <name>\nRun /plugin list to see installed plugins."
                        .to_string(),
                )
            }
            claurst_plugins::PluginSubCommand::Enable(name) => {
                let registry = get_registry(&project_dir).await;
                if registry.get(&name).is_none() {
                    return CommandResult::Error(format!(
                        "Plugin '{}' not found. Use `/plugin list` to see installed plugins.",
                        name
                    ));
                }
                let mut settings = claurst_core::config::Settings::load_sync().unwrap_or_default();
                settings.enabled_plugins.insert(name.clone());
                settings.disabled_plugins.remove(&name);
                let _ = settings.save_sync();
                CommandResult::Message(format!(
                    "Plugin '{}' enabled. Run `/plugin reload` to apply changes in this session.",
                    name
                ))
            }
            claurst_plugins::PluginSubCommand::Disable(ref name) if name.is_empty() => {
                CommandResult::Error(
                    "Usage: /plugin disable <name>\nRun /plugin list to see installed plugins."
                        .to_string(),
                )
            }
            claurst_plugins::PluginSubCommand::Disable(name) => {
                let registry = get_registry(&project_dir).await;
                if registry.get(&name).is_none() {
                    return CommandResult::Error(format!(
                        "Plugin '{}' not found. Use `/plugin list` to see installed plugins.",
                        name
                    ));
                }
                let mut settings = claurst_core::config::Settings::load_sync().unwrap_or_default();
                settings.disabled_plugins.insert(name.clone());
                settings.enabled_plugins.remove(&name);
                let _ = settings.save_sync();
                CommandResult::Message(format!(
                    "Plugin '{}' disabled. Run `/plugin reload` to apply changes in this session.",
                    name
                ))
            }
            claurst_plugins::PluginSubCommand::Info(ref name) if name.is_empty() => {
                CommandResult::Error(
                    "Usage: /plugin info <name>\nRun /plugin list to see installed plugins."
                        .to_string(),
                )
            }
            claurst_plugins::PluginSubCommand::Info(name) => {
                let registry = get_registry(&project_dir).await;
                CommandResult::Message(claurst_plugins::format_plugin_info(&registry, &name))
            }
            claurst_plugins::PluginSubCommand::Install(ref path) if path.is_empty() => {
                CommandResult::Error(
                    "Usage: /plugin install <path>\nProvide the path to a local plugin directory."
                        .to_string(),
                )
            }
            claurst_plugins::PluginSubCommand::Install(path) => {
                let result = claurst_plugins::install_plugin_from_path(
                    std::path::Path::new(&path),
                );
                match result {
                    Ok(name) => CommandResult::Message(format!(
                        "Plugin '{}' installed successfully. Run `/plugin reload` to activate it.",
                        name
                    )),
                    Err(e) => CommandResult::Error(format!("Install failed: {}", e)),
                }
            }
            claurst_plugins::PluginSubCommand::Reload => {
                let old_registry = get_registry(&project_dir).await;
                let (new_registry, diff) =
                    claurst_plugins::reload_plugins(&old_registry, &project_dir, &[]).await;
                CommandResult::Message(claurst_plugins::format_reload_summary(&new_registry, &diff))
            }
            claurst_plugins::PluginSubCommand::Help => {
                CommandResult::Message(
                    "Plugin commands:\n\
                     /plugin              — list all installed plugins\n\
                     /plugin list         — list all installed plugins\n\
                     /plugin info <name>  — show plugin details\n\
                     /plugin enable <name>   — enable a plugin\n\
                     /plugin disable <name>  — disable a plugin\n\
                     /plugin install <path>  — install plugin from local path\n\
                     /plugin reload       — reload plugins from disk"
                        .to_string(),
                )
            }
        }
    }
}

// ---- /reload-plugins -----------------------------------------------------

#[async_trait]
impl SlashCommand for ReloadPluginsCommand {
    fn name(&self) -> &str { "reload-plugins" }
    fn description(&self) -> &str { "Reload all plugins without restarting" }
    fn help(&self) -> &str {
        "Usage: /reload-plugins\n\
         Reloads all plugins and shows what changed."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let project_dir = ctx.working_dir.clone();

        let old_registry = claurst_plugins::load_plugins(&project_dir, &[]).await;
        let (new_registry, diff) =
            claurst_plugins::reload_plugins(&old_registry, &project_dir, &[]).await;

        CommandResult::Message(claurst_plugins::format_reload_summary(&new_registry, &diff))
    }
}

// ---- Plugin slash command adapter ----------------------------------------

/// Wraps a plugin-defined `PluginCommandDef` so it can be executed like a
/// built-in slash command.  The adapter is created on-the-fly inside
/// `execute_command` when no built-in matches the input.
pub struct PluginSlashCommandAdapter {
    pub def: claurst_plugins::PluginCommandDef,
}

#[async_trait]
impl SlashCommand for PluginSlashCommandAdapter {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn description(&self) -> &str {
        &self.def.description
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        // Enforce capability grants before the action runs.
        if let Err(reason) = claurst_plugins::check_plugin_capability(&self.def) {
            return CommandResult::Error(reason);
        }

        match &self.def.run_action {
            claurst_plugins::CommandRunAction::StaticResponse(msg) => {
                CommandResult::Message(msg.clone())
            }
            claurst_plugins::CommandRunAction::MarkdownPrompt {
                file_path,
                plugin_root: _,
            } => {
                // Read the markdown file and inject it into the conversation
                match std::fs::read_to_string(file_path) {
                    Ok(content) => {
                        let full_prompt = if args.is_empty() {
                            content
                        } else {
                            format!("{}\n\nArguments: {}", content, args)
                        };
                        CommandResult::UserMessage(full_prompt)
                    }
                    Err(e) => CommandResult::Error(format!(
                        "Could not read plugin command file '{}': {}",
                        file_path, e
                    )),
                }
            }
            claurst_plugins::CommandRunAction::ShellCommand {
                command,
                plugin_root,
            } => {
                let full_cmd = if args.is_empty() {
                    command.clone()
                } else {
                    format!("{} {}", command, args)
                };
                let cmd_result = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
                    .args(if cfg!(windows) {
                        vec!["/C", &full_cmd]
                    } else {
                        vec!["-c", &full_cmd]
                    })
                    .env("CLAUDE_PLUGIN_ROOT", plugin_root)
                    .output();
                match cmd_result {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        if out.status.success() {
                            CommandResult::Message(stdout.to_string())
                        } else {
                            CommandResult::Error(format!("Command failed:\n{}", stderr))
                        }
                    }
                    Err(e) => CommandResult::Error(format!("Failed to run command: {}", e)),
                }
            }
        }
    }
}
