// Maintenance commands: `/voice`, `/upgrade`, `/release-notes`, `/rate-limit-options`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct VoiceCommand;
pub struct UpgradeCommand;
pub struct ReleaseNotesCommand;
pub struct RateLimitOptionsCommand;

// ---- /voice --------------------------------------------------------------

#[async_trait]
impl SlashCommand for VoiceCommand {
    fn name(&self) -> &str { "voice" }
    fn description(&self) -> &str { "Toggle voice input mode on/off" }
    fn help(&self) -> &str {
        "Usage: /voice [on|off|status]\n\n\
         Enables or disables voice input (push-to-talk).\n\
         Setting is persisted to ~/.claurst/ui-settings.json.\n\n\
         Transcription is performed via a Whisper-compatible API.\n\
         Set one of these env vars for the API key:\n\
           OPENAI_API_KEY   — OpenAI Whisper (default endpoint)\n\
           ANTHROPIC_API_KEY — used as a fallback key\n\n\
         To use a local Whisper server instead of OpenAI:\n\
           export WHISPER_ENDPOINT_URL=http://localhost:8080/v1/audio/transcriptions\n\
           export OPENAI_API_KEY=any-value  (local servers often ignore the key)\n\n\
         On Linux, ALSA must be set up: sudo apt install libasound2-dev\n\
         Check available devices with: arecord -l\n\n\
         Controls:\n\
           Alt+V — start recording; Alt+V or Esc — stop and transcribe"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let current = load_ui_settings();
        let currently_enabled = current.voice_enabled.unwrap_or(false);

        let enable = match args.trim() {
            "on" | "enable" | "enabled" | "true" | "1" => true,
            "off" | "disable" | "disabled" | "false" | "0" => false,
            "" => !currently_enabled, // toggle
            "status" => {
                let state = if currently_enabled { "enabled" } else { "disabled" };
                let endpoint = std::env::var("WHISPER_ENDPOINT_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1/audio/transcriptions (default)".to_string());
                let key_source = if std::env::var("OPENAI_API_KEY").is_ok() {
                    "OPENAI_API_KEY"
                } else if std::env::var("ANTHROPIC_API_KEY").is_ok() {
                    "ANTHROPIC_API_KEY"
                } else {
                    "(none — transcription will fail)"
                };
                return CommandResult::Message(format!(
                    "Voice mode: {}\n\
                     Endpoint:   {}\n\
                     API key:    {}",
                    state, endpoint, key_source
                ));
            }
            other => {
                return CommandResult::Error(format!(
                    "Unknown argument '{}'. Use: /voice [on|off|status]",
                    other
                ))
            }
        };

        match mutate_ui_settings(|s| s.voice_enabled = Some(enable)) {
            Ok(_) => {
                if enable {
                    let endpoint = std::env::var("WHISPER_ENDPOINT_URL")
                        .unwrap_or_else(|_| "OpenAI Whisper (default)".to_string());
                    let key_hint = if std::env::var("OPENAI_API_KEY").is_ok()
                        || std::env::var("ANTHROPIC_API_KEY").is_ok()
                    {
                        String::new()
                    } else {
                        "\nWarning: no OPENAI_API_KEY found — transcription will fail. \
                         Set OPENAI_API_KEY or WHISPER_ENDPOINT_URL for a local server."
                            .to_string()
                    };
                    CommandResult::Message(format!(
                        "Voice recording activated.\n\
                         Press Alt+V to start recording; Alt+V or Esc to stop and transcribe.\n\
                         Endpoint: {}{}",
                        endpoint, key_hint
                    ))
                } else {
                    CommandResult::Message(
                        "Voice recording deactivated.".to_string(),
                    )
                }
            }
            Err(e) => CommandResult::Error(format!("Failed to save voice setting: {}", e)),
        }
    }
}

// ---- /upgrade ------------------------------------------------------------

#[async_trait]
impl SlashCommand for UpgradeCommand {
    fn name(&self) -> &str { "update" }
    fn aliases(&self) -> Vec<&str> { vec!["upgrade"] }
    fn description(&self) -> &str { "Check for updates and download the latest release" }
    fn help(&self) -> &str {
        "Usage: /update\n\n\
         Checks GitHub releases for the latest version of Claurst.\n\
         If a newer version is available, shows where to download it."
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let current = claurst_core::constants::APP_VERSION;

        // Check GitHub releases API for latest version
        let client = reqwest::Client::builder()
            .user_agent(format!("claurst/{}", current))
            .timeout(std::time::Duration::from_secs(8))
            .build();

        let client = match client {
            Ok(c) => c,
            Err(e) => {
                return CommandResult::Message(format!(
                    "Current version: {current}\n\
                     Could not check for updates (HTTP client error: {e})\n\
                     Visit https://github.com/kuberwastaken/claurst/releases for updates."
                ))
            }
        };

        let resp = client
            .get("https://api.github.com/repos/kuberwastaken/claurst/releases/latest")
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let json: serde_json::Value =
                    r.json().await.unwrap_or(serde_json::Value::Null);

                let tag = json
                    .get("tag_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .trim_start_matches('v');

                let url = json
                    .get("html_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("https://github.com/kuberwastaken/claurst/releases");

                if tag == current || tag == "unknown" {
                    CommandResult::Message(format!(
                        "Claurst v{current} - you are up to date.\n\
                         Release page: {url}"
                    ))
                } else {
                    CommandResult::Message(format!(
                        "Update available!\n\
                         Current version:  v{current}\n\
                         Latest version:   v{tag}\n\
                         Release page:     {url}\n\n\
                         Upgrade in place (recommended):\n\
                           claurst upgrade\n\n\
                         Or reinstall with your original method:\n\
                           npm install -g claurst\n\
                           curl -fsSL https://github.com/kuberwastaken/claurst/releases/latest/download/install.sh | bash   (macOS/Linux)\n\
                           irm https://github.com/kuberwastaken/claurst/releases/latest/download/install.ps1 | iex          (Windows)"
                    ))
                }
            }
            Ok(r) => {
                let status = r.status();
                CommandResult::Message(format!(
                    "Current version: v{current}\n\
                     Could not check for updates (HTTP {status}).\n\
                     Visit https://github.com/kuberwastaken/claurst/releases for updates."
                ))
            }
            Err(e) => CommandResult::Message(format!(
                "Current version: v{current}\n\
                 Could not check for updates: {e}\n\
                 Visit https://github.com/kuberwastaken/claurst/releases for updates."
            )),
        }
    }
}

// ---- /release-notes ------------------------------------------------------

#[async_trait]
impl SlashCommand for ReleaseNotesCommand {
    fn name(&self) -> &str { "release-notes" }
    fn description(&self) -> &str { "Show release notes for the current version" }
    fn help(&self) -> &str {
        "Usage: /release-notes [version]\n\n\
         Fetches and displays release notes from GitHub.\n\
         Without an argument, shows notes for the current version."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let current = claurst_core::constants::APP_VERSION;
        let version = args.trim();

        let tag = if version.is_empty() {
            format!("v{}", current)
        } else if version.starts_with('v') {
            version.to_string()
        } else {
            format!("v{}", version)
        };

        let client = reqwest::Client::builder()
            .user_agent(format!("claurst/{}", current))
            .timeout(std::time::Duration::from_secs(8))
            .build();

        let client = match client {
            Ok(c) => c,
            Err(_) => {
                return CommandResult::Message(format!(
                    "Claurst {tag} release notes:\n\
                     Visit https://github.com/kuberwastaken/claurst/releases/tag/{tag}"
                ))
            }
        };

        let url = format!(
            "https://api.github.com/repos/kuberwastaken/claurst/releases/tags/{}",
            tag
        );

        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let json: serde_json::Value =
                    r.json().await.unwrap_or(serde_json::Value::Null);

                let body = json
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("No release notes found.");

                let published = json
                    .get("published_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown date");

                let html_url = json
                    .get("html_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                CommandResult::Message(format!(
                    "Release Notes: Claurst {tag}\n\
                     Published: {published}\n\
                     URL: {html_url}\n\
                     ─────────────────────────────────\n\
                     {body}"
                ))
            }
            Ok(r) if r.status().as_u16() == 404 => CommandResult::Message(format!(
                "No release found for {tag}.\n\
                 View all releases: https://github.com/kuberwastaken/claurst/releases"
            )),
            Ok(r) => CommandResult::Message(format!(
                "Could not fetch release notes (HTTP {}).\n\
                 View at: https://github.com/kuberwastaken/claurst/releases/tag/{}",
                r.status(),
                tag
            )),
            Err(e) => CommandResult::Message(format!(
                "Could not fetch release notes: {e}\n\
                 View at: https://github.com/kuberwastaken/claurst/releases/tag/{tag}"
            )),
        }
    }
}

// ---- /rate-limit-options -------------------------------------------------

#[async_trait]
impl SlashCommand for RateLimitOptionsCommand {
    fn name(&self) -> &str { "rate-limit-options" }
    fn description(&self) -> &str { "Show rate limit tiers and current rate limit status" }
    fn help(&self) -> &str {
        "Usage: /rate-limit-options\n\n\
         Displays available rate limit tiers and the current tier for your account.\n\
         Rate limits depend on your Claurst plan (Free, Pro, Max, API)."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        // Try to read from OAuth tokens file to get subscription/tier info
        let tier_info = match claurst_core::oauth::OAuthTokens::load().await {
            Some(tokens) => {
                let sub_type = tokens.subscription_type.as_deref().unwrap_or("unknown");
                format!(
                    "Account type:    {}\n\
                     Scopes:          {}",
                    sub_type,
                    if tokens.scopes.is_empty() { "none".to_string() } else { tokens.scopes.join(", ") }
                )
            }
            None => {
                // Check for API key auth
                if ctx.config.resolve_api_key().is_some() {
                    "Account type:    API key (Console)\n\
                     Rate limit tier: Depends on your API plan tier"
                        .to_string()
                } else {
                    "Not logged in. Run /login to see your rate limit tier.".to_string()
                }
            }
        };

        CommandResult::Message(format!(
            "Rate Limit Status\n\
             ─────────────────\n\
             {tier_info}\n\n\
             Available tiers:\n\
             ┌─────────────────────────────────────────────────┐\n\
             │ Free          │ Limited daily usage             │\n\
             │ Pro           │ Higher limits, faster resets    │\n\
             │ Max (5x)      │ 5× Pro limits                   │\n\
             │ Max (20x)     │ 20× Pro limits (highest tier)   │\n\
             │ API / Console │ Usage-billed, no hard cap       │\n\
             └─────────────────────────────────────────────────┘\n\n\
             To upgrade: /upgrade\n\
             Manage billing: https://claude.ai/settings/billing",
            tier_info = tier_info,
        ))
    }
}
