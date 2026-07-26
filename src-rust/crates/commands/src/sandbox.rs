// `/sandbox-toggle` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct SandboxToggleCommand;

// ---- /sandbox-toggle -----------------------------------------------------

#[async_trait]
impl SlashCommand for SandboxToggleCommand {
    fn name(&self) -> &str { "sandbox-toggle" }
    fn aliases(&self) -> Vec<&str> { vec!["sandbox"] }
    fn description(&self) -> &str { "Enable or disable sandboxed execution of shell commands" }
    fn help(&self) -> &str {
        "Usage: /sandbox-toggle [on|off|exclude <pattern>|status]\n\n\
         Toggles sandboxed execution of bash/shell commands.\n\
         When sandbox mode is enabled, shell commands run in an isolated\n\
         environment to prevent unintended side effects.\n\n\
         Subcommands:\n\
           /sandbox-toggle           — toggle the current state\n\
           /sandbox-toggle on        — enable sandbox mode\n\
           /sandbox-toggle off       — disable sandbox mode\n\
           /sandbox-toggle status    — show current state and excluded patterns\n\
           /sandbox-toggle exclude <pattern>  — add a command pattern to exclusions\n\n\
         Sandbox is supported on macOS, Linux, and WSL2.\n\
         Note: A restart is recommended for full effect."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();

        // Platform support check: sandbox requires macOS or Linux (not Windows native).
        let platform = std::env::consts::OS;
        let is_wsl = std::env::var("WSL_DISTRO_NAME").is_ok()
            || std::env::var("WSL_INTEROP").is_ok();
        let is_supported = matches!(platform, "linux" | "macos") || is_wsl;

        // Handle subcommand: status
        if args == "status" {
            let ui = load_ui_settings();
            let mode = if ui.sandbox_mode.unwrap_or(false) { "enabled" } else { "disabled" };
            let excl = if ui.sandbox_excluded_commands.is_empty() {
                "(none)".to_string()
            } else {
                ui.sandbox_excluded_commands
                    .iter()
                    .map(|p| format!("  - {}", p))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let platform_note = if is_supported {
                format!("\u{2713} Supported on this platform ({})", platform)
            } else {
                format!("\u{2717} Not supported on this platform ({}). Requires macOS, Linux, or WSL2.", platform)
            };
            return CommandResult::Message(format!(
                "Sandbox mode: {}\n\
                 Platform:     {}\n\
                 Excluded command patterns:\n{}\n\n\
                 Use /sandbox-toggle [on|off] to change mode.\n\
                 Use /sandbox-toggle exclude <pattern> to add exclusions.",
                mode, platform_note, excl
            ));
        }

        // Handle subcommand: exclude <pattern>
        if let Some(rest) = args.strip_prefix("exclude").map(str::trim) {
            if rest.is_empty() {
                return CommandResult::Error(
                    "Usage: /sandbox-toggle exclude <command-pattern>\n\
                     Example: /sandbox-toggle exclude \"npm run test:*\"".to_string()
                );
            }
            // Strip surrounding quotes if present
            let pattern = rest.trim_matches(|c| c == '"' || c == '\'').to_string();
            if pattern.is_empty() {
                return CommandResult::Error("Pattern cannot be empty.".to_string());
            }
            match mutate_ui_settings(|s| {
                if !s.sandbox_excluded_commands.contains(&pattern) {
                    s.sandbox_excluded_commands.push(pattern.clone());
                }
            }) {
                Ok(_) => {
                    let settings_path = ui_settings_path();
                    return CommandResult::Message(format!(
                        "Added \"{}\" to sandbox excluded commands.\n\
                         Saved to: {}",
                        pattern,
                        settings_path.display()
                    ));
                }
                Err(e) => return CommandResult::Error(format!("Failed to save exclusion: {}", e)),
            }
        }

        // Platform guard for toggling on/off
        if !is_supported && (args == "on" || args == "enable" || args == "enabled"
            || args == "true" || args == "1" || args.is_empty())
        {
            let msg = if is_wsl {
                "Error: Sandboxing requires WSL2. WSL1 is not supported.".to_string()
            } else {
                format!(
                    "Error: Sandboxing is currently only supported on macOS, Linux, and WSL2.\n\
                     Current platform: {}",
                    platform
                )
            };
            // Only hard-block enabling; allow off/status even on unsupported platforms.
            if args != "off" && args != "disable" && args != "disabled"
                && args != "false" && args != "0"
            {
                return CommandResult::Error(msg);
            }
        }

        // Read current sandbox state from ui-settings
        let current_ui = load_ui_settings();
        let currently_enabled = current_ui.sandbox_mode.unwrap_or(false);

        let enable = match args {
            "on" | "enable" | "enabled" | "true" | "1" => true,
            "off" | "disable" | "disabled" | "false" | "0" => false,
            "" => !currently_enabled,
            other => {
                return CommandResult::Error(format!(
                    "Unknown argument '{}'. Use: /sandbox-toggle [on|off|status|exclude <pattern>]",
                    other
                ))
            }
        };

        match mutate_ui_settings(|s| s.sandbox_mode = Some(enable)) {
            Ok(_) => {
                let state = if enable { "enabled" } else { "disabled" };
                CommandResult::Message(format!(
                    "Sandbox mode {}. Restart recommended for full effect.\n\
                     Use /sandbox-toggle exclude <pattern> to bypass sandboxing for specific commands.",
                    state
                ))
            }
            Err(e) => CommandResult::Error(format!("Failed to save sandbox setting: {}", e)),
        }
    }
}
