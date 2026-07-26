// Remote-control commands: `/remote-control` (`/rc`) and `/remote-env`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct RemoteControlCommand;
pub struct RemoteEnvCommand;

// ---- /remote-control (/rc) -----------------------------------------------

#[async_trait]
impl SlashCommand for RemoteControlCommand {
    fn name(&self) -> &str { "remote-control" }
    fn aliases(&self) -> Vec<&str> { vec!["rc"] }
    fn description(&self) -> &str { "Show or manage the remote control (Bridge) connection" }
    fn help(&self) -> &str {
        "Usage: /remote-control [start|stop|status]\n\n\
         The Bridge feature lets you connect your local Claurst CLI to the\n\
         claude.ai web UI or mobile app.\n\n\
         Subcommands:\n\
         /remote-control          Show current bridge status and connection URL\n\
         /remote-control start    Start the remote-control bridge listener\n\
         /remote-control stop     Stop the bridge listener\n\
         /remote-control status   Show bridge status"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let settings = match claurst_core::config::Settings::load().await {
            Ok(s) => s,
            Err(e) => return CommandResult::Error(format!("Failed to load settings: {}", e)),
        };

        let remote_at_startup = settings.remote_control_at_startup;

        match args.trim() {
            "" | "status" => {
                let hostname = hostname::get()
                    .map(|h| h.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "(unknown host)".to_string());

                let bridge_url = std::env::var("CLAURST_BRIDGE_URL")
                    .unwrap_or_else(|_| "https://claude.ai".to_string());

                let token_status = if std::env::var("CLAURST_BRIDGE_TOKEN").is_ok()
                    || std::env::var("CLAUDE_BRIDGE_OAUTH_TOKEN").is_ok()
                {
                    "configured via environment variable"
                } else {
                    "not set (required to connect)"
                };

                let startup_status =
                    if remote_at_startup { "enabled at startup" } else { "disabled" };

                // Active session info from context
                let session_section = if let Some(ref url) = ctx.remote_session_url {
                    format!(
                        "\nActive Session\n\
                         ──────────────\n\
                         Session URL:  {url}\n\
                         Share this URL or QR code with others to let them connect\n\
                         to this Claurst session from the claude.ai web UI.\n",
                        url = url
                    )
                } else {
                    "\nNo active bridge session in this process.\n".to_string()
                };

                // Device fingerprint (first 12 chars are enough for display)
                let fingerprint = claurst_bridge::device_fingerprint();
                let fp_short = &fingerprint[..fingerprint.len().min(12)];

                CommandResult::Message(format!(
                    "Remote Control (Bridge)\n\
                     ═══════════════════════\n\
                     What it does: lets you connect the claude.ai web UI or mobile app\n\
                     to this running Claurst CLI session on your local machine.\n\
                     All prompts and responses are relayed bidirectionally.\n\
                     \n\
                     Local Machine\n\
                     ─────────────\n\
                     Hostname:     {hostname}\n\
                     Device ID:    {fp_short}… (SHA-256 fingerprint)\n\
                     \n\
                     Bridge Configuration\n\
                     ────────────────────\n\
                     Bridge server:   {bridge_url}\n\
                     Session token:   {token_status}\n\
                     Startup mode:    {startup_status}\n\
                     {session_section}\n\
                     How to connect\n\
                     ──────────────\n\
                     1. Obtain a session token from claude.ai (Settings → Remote Control)\n\
                     2. Set it:  export CLAURST_BRIDGE_TOKEN=<your-token>\n\
                     3. Enable:  /remote-control start\n\
                     4. Restart Claurst — the bridge will connect automatically\n\
                     5. Open {bridge_url}/claude-code in your browser\n\
                     \n\
                     Note: Full bridge polling requires server-side session infrastructure.\n\
                     The cc-bridge crate implements the complete protocol (register → poll\n\
                     → events) and is ready to use once a valid session token is provided.\n\
                     \n\
                     Use /remote-control start   to enable bridge at next startup\n\
                     Use /remote-control stop    to disable bridge at startup",
                    hostname = hostname,
                    fp_short = fp_short,
                    bridge_url = bridge_url,
                    token_status = token_status,
                    startup_status = startup_status,
                    session_section = session_section,
                ))
            }
            "start" => {
                if let Err(e) = save_settings_mutation(|s| s.remote_control_at_startup = true) {
                    return CommandResult::Error(format!("Failed to save settings: {}", e));
                }
                let bridge_url = std::env::var("CLAURST_BRIDGE_URL")
                    .unwrap_or_else(|_| "https://claude.ai".to_string());
                let token_note = if std::env::var("CLAURST_BRIDGE_TOKEN").is_ok()
                    || std::env::var("CLAUDE_BRIDGE_OAUTH_TOKEN").is_ok()
                {
                    "Session token detected in environment — bridge will connect on next start."
                        .to_string()
                } else {
                    format!(
                        "No session token found.\n\
                         Get a token from {bridge_url} (Settings → Remote Control)\n\
                         then run:  export CLAURST_BRIDGE_TOKEN=<token>",
                        bridge_url = bridge_url
                    )
                };
                CommandResult::Message(format!(
                    "Remote control bridge enabled at startup.\n\
                     Restart Claurst to activate the bridge connection.\n\n\
                     {token_note}",
                    token_note = token_note
                ))
            }
            "stop" => {
                if let Err(e) = save_settings_mutation(|s| s.remote_control_at_startup = false) {
                    return CommandResult::Error(format!("Failed to save settings: {}", e));
                }
                CommandResult::Message(
                    "Remote control bridge disabled.\n\
                     The bridge will not start on next launch."
                        .to_string(),
                )
            }
            other => CommandResult::Error(format!(
                "Unknown subcommand: '{}'\nUsage: /remote-control [start|stop|status]",
                other
            )),
        }
    }
}

// ---- /remote-env ---------------------------------------------------------

#[async_trait]
impl SlashCommand for RemoteEnvCommand {
    fn name(&self) -> &str { "remote-env" }
    fn description(&self) -> &str { "Show and manage environment variables for remote sessions" }
    fn help(&self) -> &str {
        "Usage: /remote-env [set <KEY> <VALUE> | unset <KEY> | list]\n\n\
         Manages env vars stored in config that are forwarded to remote Claurst sessions.\n\
         These are persisted to settings under the 'env' key."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();

        if args.is_empty() || args == "list" {
            if ctx.config.env.is_empty() {
                return CommandResult::Message(
                    "No remote environment variables configured.\n\
                     Use /remote-env set <KEY> <VALUE> to add one."
                        .to_string(),
                );
            }
            let mut lines = vec!["Remote environment variables:".to_string()];
            let mut keys: Vec<_> = ctx.config.env.keys().collect();
            keys.sort();
            for key in keys {
                let val = &ctx.config.env[key];
                // Mask values that look like secrets
                let display = if key.to_uppercase().contains("KEY")
                    || key.to_uppercase().contains("TOKEN")
                    || key.to_uppercase().contains("SECRET")
                    || key.to_uppercase().contains("PASSWORD")
                {
                    format!("{}***", &val[..val.len().min(4)])
                } else {
                    val.clone()
                };
                lines.push(format!("  {} = {}", key, display));
            }
            return CommandResult::Message(lines.join("\n"));
        }

        let mut parts = args.splitn(3, ' ');
        let sub = parts.next().unwrap_or("").trim();
        let key = parts.next().unwrap_or("").trim();
        let val = parts.next().unwrap_or("").trim();

        match sub {
            "set" => {
                if key.is_empty() || val.is_empty() {
                    return CommandResult::Error(
                        "Usage: /remote-env set <KEY> <VALUE>".to_string(),
                    );
                }
                let key_owned = key.to_string();
                let val_owned = val.to_string();
                if let Err(e) = save_settings_mutation(|s| {
                    s.config.env.insert(key_owned.clone(), val_owned.clone());
                }) {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                let mut new_config = ctx.config.clone();
                new_config.env.insert(key.to_string(), val.to_string());
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!("Set remote env: {} = {}", key, val),
                )
            }
            "unset" | "remove" | "delete" => {
                if key.is_empty() {
                    return CommandResult::Error(
                        "Usage: /remote-env unset <KEY>".to_string(),
                    );
                }
                if !ctx.config.env.contains_key(key) {
                    return CommandResult::Message(format!("Key '{}' is not set.", key));
                }
                let key_owned = key.to_string();
                if let Err(e) = save_settings_mutation(|s| {
                    s.config.env.remove(&key_owned);
                }) {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                let mut new_config = ctx.config.clone();
                new_config.env.remove(key);
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!("Removed remote env var: {}", key),
                )
            }
            other => CommandResult::Error(format!(
                "Unknown subcommand: '{}'\nUsage: /remote-env [list|set <K> <V>|unset <K>]",
                other
            )),
        }
    }
}
