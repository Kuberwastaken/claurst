// Provider/agent commands: `/providers`, `/connect`, `/agent`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct ProvidersCommand;
pub struct ConnectCommand;
pub struct AgentCommand;

// ---- /providers -------------------------------------------------------------

#[async_trait]
impl SlashCommand for ProvidersCommand {
    fn name(&self) -> &str { "providers" }
    fn description(&self) -> &str { "List available AI providers and their status" }
    fn help(&self) -> &str {
        "Usage: /providers\n\nList all providers registered in the model registry with their\nmodel counts, context windows, and pricing information."
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let registry = claurst_api::ModelRegistry::new();
        let all = registry.list_all();

        if all.is_empty() {
            return CommandResult::Message("No providers available.".to_string());
        }

        // Group by provider
        use std::collections::HashMap;
        let mut by_provider: HashMap<String, Vec<_>> = HashMap::new();
        for entry in &all {
            by_provider
                .entry(entry.info.provider_id.to_string())
                .or_default()
                .push(entry);
        }

        // Sort providers alphabetically for stable output
        let mut provider_keys: Vec<String> = by_provider.keys().cloned().collect();
        provider_keys.sort();

        let mut lines = vec!["Available providers:\n".to_string()];
        for provider in &provider_keys {
            let models = &by_provider[provider];
            lines.push(format!("\n{} ({} model{})", provider.to_uppercase(), models.len(),
                if models.len() == 1 { "" } else { "s" }));
            for m in models.iter().take(3) {
                let cost_str = match (m.cost_input, m.cost_output) {
                    (Some(i), Some(o)) => format!("${:.2}/${:.2} per 1M", i, o),
                    _ => "free/local".to_string(),
                };
                lines.push(format!("  {} — {}K ctx, {}",
                    m.info.id, m.info.context_window / 1000, cost_str));
            }
            if models.len() > 3 {
                lines.push(format!("  ... and {} more", models.len() - 3));
            }
        }

        CommandResult::Message(lines.join("\n"))
    }
}

// ---- /connect -------------------------------------------------------------

#[async_trait]
impl SlashCommand for ConnectCommand {
    fn name(&self) -> &str { "connect" }
    fn description(&self) -> &str { "Connect an AI provider" }
    fn help(&self) -> &str {
        "Usage: /connect\n\nOpens the interactive provider picker dialog.\nSelect a provider to see setup instructions."
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        // This is handled by the TUI interceptor — opening the connect dialog.
        CommandResult::Message("Use the connect dialog to set up a provider.".to_string())
    }
}

// ---- /agent ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for AgentCommand {
    fn name(&self) -> &str { "agent" }
    fn description(&self) -> &str { "List available agents or get info about a specific agent" }
    fn help(&self) -> &str {
        "Usage: /agent [name]\n\nWithout arguments, lists all available named agents.\nWith a name, shows details for that agent.\n\nTo use an agent, start Claurst with: --agent <name>"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        use std::collections::HashMap;

        // Merge built-in defaults with user-defined agents (user wins on collision).
        let mut all_agents: HashMap<String, claurst_core::AgentDefinition> =
            claurst_core::default_agents();
        all_agents.extend(ctx.config.agents.clone());

        let agent_name = args.trim();

        if agent_name.is_empty() {
            // List all visible agents.
            let mut keys: Vec<&String> = all_agents
                .iter()
                .filter(|(_, d)| d.visible)
                .map(|(k, _)| k)
                .collect();
            keys.sort();

            let mut output = "Available agents:\n\n".to_string();
            for name in keys {
                let def = &all_agents[name];
                output.push_str(&format!(
                    "  @{} — {}\n    access: {}{}\n",
                    name,
                    def.description.as_deref().unwrap_or(""),
                    def.access,
                    def.max_turns
                        .map(|t| format!(", max_turns: {}", t))
                        .unwrap_or_default(),
                ));
            }
            output.push_str("\nUse --agent <name> when starting Claurst to activate an agent.");
            CommandResult::Message(output)
        } else if let Some(def) = all_agents.get(agent_name) {
            // Show details for the named agent.
            let mut output = format!("Agent: @{}\n", agent_name);
            if let Some(ref desc) = def.description {
                output.push_str(&format!("Description: {}\n", desc));
            }
            output.push_str(&format!("Access: {}\n", def.access));
            if let Some(ref model) = def.model {
                output.push_str(&format!("Model: {}\n", model));
            }
            if let Some(t) = def.max_turns {
                output.push_str(&format!("Max turns: {}\n", t));
            }
            if let Some(ref color) = def.color {
                output.push_str(&format!("Color: {}\n", color));
            }
            if let Some(ref prompt) = def.prompt {
                output.push_str(&format!("\nSystem prompt prefix:\n  {}\n", prompt));
            }
            output.push_str(&format!(
                "\nTo activate: claurst --agent {}", agent_name
            ));
            CommandResult::Message(output)
        } else {
            CommandResult::Error(format!(
                "Unknown agent '{}'. Run /agent to see available agents.",
                agent_name
            ))
        }
    }
}

// ---- /add -------------------------------------------------------------

/// Add a custom OpenAI-compatible provider to settings.
///
/// Usage: `/add <id> <apiBase> [apiKey]`
///
/// The `id` becomes the provider id used in `provider/model` routing.
/// The `apiBase` is the OpenAI-compatible base URL.
/// The optional `apiKey` supports `{env:VAR}` substitution.
pub struct AddCustomProviderCommand;

#[async_trait]
impl SlashCommand for AddCustomProviderCommand {
    fn name(&self) -> &str { "add" }
    fn description(&self) -> &str { "Add a custom OpenAI-compatible provider" }
    fn help(&self) -> &str {
        "Usage: /add <id> <apiBase> [apiKey]\n\n\
         Add a custom OpenAI-compatible provider to settings.json.\n\n\
         Arguments:\n  \
         id       \u{2014} unique provider id (e.g. \"my-gateway\")\n  \
         apiBase  \u{2014} OpenAI-compatible base URL\n  \
         apiKey   \u{2014} optional API key or {env:VAR} pattern\n\n\
         Example:\n  \
         /add my-gw https://gw.example.com/v1 {env:GW_API_KEY}\n\n\
         The provider appears in /providers after restart. Models can be\
         added via the customProviders.models map in settings.json."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let parts: Vec<&str> = args.split_whitespace().collect();
        if parts.len() < 2 {
            return CommandResult::Message(
                "Usage: /add <id> <apiBase> [apiKey]\n\
                 Example: /add my-gw https://gw.example.com/v1"
                    .to_string(),
            );
        }

        let id = parts[0].trim();
        let api_base = parts[1].trim();
        let api_key = parts.get(2).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

        if id.is_empty() {
            return CommandResult::Message("Error: provider id must not be empty.".to_string());
        }
        if api_base.is_empty() {
            return CommandResult::Message("Error: apiBase must not be empty.".to_string());
        }

        // Basic URL validation.
        if !api_base.starts_with("http://") && !api_base.starts_with("https://") {
            return CommandResult::Message(format!(
                "Error: apiBase must start with http:// or https:// (got: {})",
                api_base
            ));
        }

        // Load current settings from disk (fresh read, not in-memory cache).
        let mut settings = match claurst_core::Settings::load_sync() {
            Ok(s) => s,
            Err(e) => {
                return CommandResult::Message(format!(
                    "Error loading settings: {}",
                    e
                ));
            }
        };

        // Check for duplicate id.
        if settings.custom_providers.contains_key(id) {
            return CommandResult::Message(format!(
                "Error: custom provider '{}' already exists. Use a different id or remove it from settings.json first.",
                id
            ));
        }

        // Use the id as the display name if no explicit name is provided.
        let display_name = id.to_string();

        // Insert the new provider.
        let provider_def = claurst_core::config::CustomProviderDef {
            name: display_name,
            api_base: api_base.to_string(),
            api_key,
            headers: std::collections::HashMap::new(),
            models: std::collections::HashMap::new(),
            request_timeout_secs: None,
            streaming: None,
            include_usage_in_stream: None,
            reasoning_field: None,
        };
        settings.custom_providers.insert(id.to_string(), provider_def);

        // Save atomically.
        if let Err(e) = settings.save_sync() {
            return CommandResult::Message(format!("Error saving settings: {}", e));
        }

        CommandResult::Message(format!(
            "Added custom provider '{}'.\n\
             Base URL: {}\n\
             Restart or reload settings to use it. Add models via the\n\
             customProviders.{}.models map in settings.json.",
            id, api_base, id
        ))
    }
}
