// `/sandbox-toggle` (alias `/sandbox`): show or change the OS sandbox that
// Bash commands run in (#470). The state lives in the `sandbox` block of the
// global settings file and reaches the Bash tool through `Config::sandbox`.

use super::*;
use async_trait::async_trait;
use claurst_core::sandbox::{seatbelt_available, SandboxMode, SandboxSettings};

pub struct SandboxToggleCommand;

const USAGE: &str =
    "Usage: /sandbox-toggle [status|on|off|workspace-write|read-only|network on|network off]";

// ---- /sandbox-toggle -----------------------------------------------------

#[async_trait]
impl SlashCommand for SandboxToggleCommand {
    fn name(&self) -> &str { "sandbox-toggle" }
    fn aliases(&self) -> Vec<&str> { vec!["sandbox"] }
    fn description(&self) -> &str { "Show or change the OS sandbox for Bash commands" }
    fn help(&self) -> &str {
        "Usage: /sandbox-toggle [status|on|off|workspace-write|read-only|network on|network off]\n\n\
         On macOS, Bash commands run under Seatbelt (sandbox-exec) unless the sandbox is off.\n\
           workspace-write  writes only inside the workspace, --add-dir directories and the\n\
                            temp directories; .git and .claurst stay read-only (default)\n\
           read-only        no writes\n\
           network on|off   network access for sandboxed commands (default off)\n\n\
         With no argument, toggles between off and workspace-write. Changes are saved to the\n\
         `sandbox` block of the global settings file and apply to the next command.\n\
         Other platforms have no OS sandbox yet: the setting is kept but has no effect."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let next = match next_settings(args.trim(), &ctx.config.sandbox) {
            Ok(Some(next)) => next,
            Ok(None) => return CommandResult::Message(describe(&ctx.config.sandbox)),
            Err(e) => return CommandResult::Error(e),
        };
        let mut settings = match Settings::load_sync() {
            Ok(settings) => settings,
            Err(e) => return CommandResult::Error(format!("Failed to load settings: {}", e)),
        };
        settings.sandbox.mode = next.mode;
        settings.sandbox.network = next.network;
        if let Err(e) = settings.save_sync() {
            return CommandResult::Error(format!("Failed to save sandbox setting: {}", e));
        }
        let mut config = ctx.config.clone();
        config.sandbox = next;
        let message = describe(&config.sandbox);
        CommandResult::ConfigChangeMessage(config, message)
    }
}

/// The settings `args` asks for, or `None` for a status query.
fn next_settings(args: &str, current: &SandboxSettings) -> Result<Option<SandboxSettings>, String> {
    let mut next = current.clone();
    match args {
        "status" => return Ok(None),
        "" => {
            next.mode = if current.mode == SandboxMode::Off {
                SandboxMode::WorkspaceWrite
            } else {
                SandboxMode::Off
            };
        }
        "on" | "workspace-write" => next.mode = SandboxMode::WorkspaceWrite,
        "off" => next.mode = SandboxMode::Off,
        "read-only" => next.mode = SandboxMode::ReadOnly,
        "network on" => next.network = true,
        "network off" => next.network = false,
        other => return Err(format!("Unknown argument '{}'. {}", other, USAGE)),
    }
    Ok(Some(next))
}

fn describe(settings: &SandboxSettings) -> String {
    let mode = settings.mode.as_str();
    let state = if settings.mode == SandboxMode::Off {
        "off: Bash commands run with your full privileges".to_string()
    } else if seatbelt_available() {
        format!("{} (macOS Seatbelt)", mode)
    } else {
        format!(
            "{} (inactive: no OS sandbox on {} yet)",
            mode,
            std::env::consts::OS
        )
    };
    let prefixes = if settings.unsandboxed_prefixes.is_empty() {
        "(none)".to_string()
    } else {
        settings.unsandboxed_prefixes.join(", ")
    };
    format!(
        "Sandbox: {}\nNetwork: {}\nAlways run outside the sandbox: {}",
        state,
        if settings.network { "on" } else { "off" },
        prefixes
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_map_to_settings() {
        let default = SandboxSettings::default();
        assert_eq!(next_settings("status", &default), Ok(None));
        let off = next_settings("", &default).unwrap().unwrap();
        assert_eq!(off.mode, SandboxMode::Off);
        assert_eq!(
            next_settings("", &off).unwrap().unwrap().mode,
            SandboxMode::WorkspaceWrite
        );
        assert_eq!(
            next_settings("read-only", &default).unwrap().unwrap().mode,
            SandboxMode::ReadOnly
        );
        assert_eq!(
            next_settings("on", &off).unwrap().unwrap().mode,
            SandboxMode::WorkspaceWrite
        );
        let network = next_settings("network on", &default).unwrap().unwrap();
        assert!(network.network);
        assert_eq!(network.mode, SandboxMode::WorkspaceWrite);
        assert!(!next_settings("network off", &network).unwrap().unwrap().network);
        assert!(next_settings("exclude npm", &default).is_err());
    }

    #[test]
    fn status_names_mode_network_and_prefixes() {
        let settings = SandboxSettings {
            mode: SandboxMode::Off,
            network: true,
            unsandboxed_prefixes: vec!["cargo".to_string(), "npm".to_string()],
        };
        let text = describe(&settings);
        assert!(text.contains("Sandbox: off"));
        assert!(text.contains("Network: on"));
        assert!(text.contains("cargo, npm"));
    }
}
