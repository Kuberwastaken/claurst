// `/config` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct ConfigCommand;

// ---- /config -------------------------------------------------------------

#[async_trait]
impl SlashCommand for ConfigCommand {
    fn name(&self) -> &str { "config" }
    fn aliases(&self) -> Vec<&str> { vec!["settings"] }
    fn description(&self) -> &str { "Show or modify configuration settings" }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();
        if args.is_empty() || matches!(args, "show" | "get") {
            let json = serde_json::to_string_pretty(&ctx.config).unwrap_or_default();
            return CommandResult::Message(format!(
                "Current configuration:\n{}\n\nUsage:\n  /config\n  /config set theme <default|dark|light>\n  /config set output-style <default|concise|explanatory|learning|formal|casual>\n  /config set model <model>\n  /config set permission-mode <default|accept-edits|bypass-permissions|plan>\n  /config unset <model|output-style>",
                json
            ));
        }

        if let Some(key) = args.strip_prefix("get ").map(str::trim) {
            return match key {
                "theme" => CommandResult::Message(format!("theme = {:?}", ctx.config.theme)),
                "output-style" | "output_style" => CommandResult::Message(format!(
                    "output-style = {}",
                    current_output_style_name(&ctx.config)
                )),
                "model" => CommandResult::Message(format!(
                    "model = {}",
                    ctx.config.effective_model()
                )),
                "permission-mode" | "permission_mode" => CommandResult::Message(format!(
                    "permission-mode = {:?}",
                    ctx.config.permission_mode
                )),
                other => CommandResult::Error(format!("Unknown config key '{}'", other)),
            };
        }

        if let Some(key) = args.strip_prefix("unset ").map(str::trim) {
            return match key {
                "model" => {
                    let mut new_config = ctx.config.clone();
                    new_config.model = None;
                    if let Err(err) = save_settings_mutation(|settings| settings.config.model = None)
                    {
                        return CommandResult::Error(format!(
                            "Failed to save configuration: {}",
                            err
                        ));
                    }
                    CommandResult::ConfigChangeMessage(
                        new_config,
                        "Model reset to the default for new sessions.".to_string(),
                    )
                }
                "output-style" | "output_style" => {
                    let mut new_config = ctx.config.clone();
                    new_config.output_style = None;
                    if let Err(err) =
                        save_settings_mutation(|settings| settings.config.output_style = None)
                    {
                        return CommandResult::Error(format!(
                            "Failed to save configuration: {}",
                            err
                        ));
                    }
                    CommandResult::ConfigChangeMessage(
                        new_config,
                        "Output style reset to default.".to_string(),
                    )
                }
                other => CommandResult::Error(format!("Unknown config key '{}'", other)),
            };
        }

        let mut parts = args.splitn(3, ' ');
        let command = parts.next().unwrap_or_default();
        let key = parts.next().unwrap_or_default().trim();
        let value = parts.next().unwrap_or_default().trim();
        if command != "set" || key.is_empty() || value.is_empty() {
            return CommandResult::Error("Usage: /config set <key> <value>".to_string());
        }

        match key {
            "theme" => {
                let Some(theme) = parse_theme(value) else {
                    return CommandResult::Error(
                        "Theme must be one of: default, dark, light".to_string(),
                    );
                };
                let mut new_config = ctx.config.clone();
                new_config.theme = theme.clone();
                if let Err(err) =
                    save_settings_mutation(|settings| settings.config.theme = theme.clone())
                {
                    return CommandResult::Error(format!("Failed to save configuration: {}", err));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!("Theme set to {}.", value.trim().to_lowercase()),
                )
            }
            "output-style" | "output_style" => {
                let normalized = value.trim().to_lowercase();
                let valid = available_output_style_names();
                if !valid.iter().any(|name| name == &normalized) {
                    return CommandResult::Error(format!(
                        "Unsupported output style '{}'. Use one of: {}",
                        value,
                        valid.join(", ")
                    ));
                }

                let mut new_config = ctx.config.clone();
                new_config.output_style =
                    (normalized != "default").then(|| normalized.clone());
                if let Err(err) = save_settings_mutation(|settings| {
                    settings.config.output_style =
                        (normalized != "default").then(|| normalized.clone());
                }) {
                    return CommandResult::Error(format!("Failed to save configuration: {}", err));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!(
                        "Output style set to {}. Changes take effect on the next request.",
                        normalized
                    ),
                )
            }
            "model" => {
                let mut new_config = ctx.config.clone();
                new_config.model = Some(value.to_string());
                let inferred_provider = value
                    .split_once('/')
                    .map(|(provider, _)| provider.to_string());
                if let Some(ref provider) = inferred_provider {
                    new_config.provider = Some(provider.clone());
                }
                if let Err(err) = save_settings_mutation(|settings| {
                    settings.config.model = Some(value.to_string());
                    if let Some(ref provider) = inferred_provider {
                        settings.provider = Some(provider.clone());
                        settings.config.provider = Some(provider.clone());
                    }
                }) {
                    return CommandResult::Error(format!("Failed to save configuration: {}", err));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!("Model set to {}.", value),
                )
            }
            "permission-mode" | "permission_mode" => {
                let mode = match value.trim().to_lowercase().as_str() {
                    "default" => claurst_core::config::PermissionMode::Default,
                    "accept-edits" | "accept_edits" => {
                        claurst_core::config::PermissionMode::AcceptEdits
                    }
                    "bypass-permissions" | "bypass_permissions" => {
                        claurst_core::config::PermissionMode::BypassPermissions
                    }
                    "plan" => claurst_core::config::PermissionMode::Plan,
                    _ => {
                        return CommandResult::Error(
                            "Permission mode must be one of: default, accept-edits, bypass-permissions, plan"
                                .to_string(),
                        )
                    }
                };

                let mut new_config = ctx.config.clone();
                new_config.permission_mode = mode.clone();
                if let Err(err) = save_settings_mutation(|settings| {
                    settings.config.permission_mode = mode.clone();
                }) {
                    return CommandResult::Error(format!("Failed to save configuration: {}", err));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!("Permission mode set to {}.", value.trim().to_lowercase()),
                )
            }
            other => CommandResult::Error(format!("Unknown config key '{}'", other)),
        }
    }
}
