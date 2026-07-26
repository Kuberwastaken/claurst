// Session & output tools: `/skills`, `/rewind`, `/stats`, `/files`, `/rename`, `/effort`, `/summary`, `/commit`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct SkillsCommand;
pub struct RewindCommand;
pub struct StatsCommand;
pub struct FilesCommand;
pub struct RenameCommand;
pub struct EffortCommand;
pub struct SummaryCommand;
pub struct CommitCommand;

// ---- /skills -------------------------------------------------------------

#[async_trait]
impl SlashCommand for SkillsCommand {
    fn name(&self) -> &str { "skills" }
    fn aliases(&self) -> Vec<&str> { vec!["skill"] }
    fn description(&self) -> &str { "List available skills in .claurst/commands/" }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let mut found: Vec<String> = Vec::new();
        let dirs = [
            ctx.working_dir.join(".claurst").join("commands"),
            claurst_core::config::Settings::config_dir().join("commands"),
        ];

        for dir in &dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().is_some_and(|e| e == "md") {
                        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                            let name = stem.to_string();
                            if !found.contains(&name) {
                                found.push(name);
                            }
                        }
                    }
                }
            }
        }

        // Include skills contributed by installed plugins.
        if let Some(registry) = claurst_plugins::global_plugin_registry() {
            for skill_dir in registry.all_skill_paths() {
                if let Ok(entries) = std::fs::read_dir(&skill_dir) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        // Skills can be individual .md files or subdirs with SKILL.md.
                        if p.is_dir() {
                            if p.join("SKILL.md").exists() || p.join("skill.md").exists() {
                                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                                    let skill_name = name.to_string();
                                    if !found.contains(&skill_name) {
                                        found.push(skill_name);
                                    }
                                }
                            }
                        } else if p.extension().is_some_and(|e| e == "md") {
                            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                                let name = stem.to_string();
                                if !found.contains(&name) {
                                    found.push(name);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Include discovered skills from .claurst/skills/ and configured paths/URLs.
        let discovered = claurst_core::discover_skills(
            &ctx.working_dir,
            &ctx.config.skills,
        );

        let mut output = if found.is_empty() && discovered.is_empty() {
            return CommandResult::Message(
                "No skills found.\nCreate .md files in .claurst/commands/ to define skills.\n\
                 Example: .claurst/commands/review.md".to_string(),
            );
        } else if found.is_empty() {
            String::new()
        } else {
            found.sort();
            format!(
                "Available skills ({}):\n{}",
                found.len(),
                found.iter().map(|s| format!("  /{}", s)).collect::<Vec<_>>().join("\n")
            )
        };

        if !discovered.is_empty() {
            let mut disc_list: Vec<(&String, &claurst_core::DiscoveredSkill)> =
                discovered.iter().collect();
            disc_list.sort_by_key(|(name, _)| name.as_str());

            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!("\nDiscovered skills ({}):\n", disc_list.len()));
            for (name, skill) in disc_list {
                output.push_str(&format!(
                    "  /{} — {} ({})\n",
                    name,
                    skill.description,
                    skill.source_path.display()
                ));
            }
        }

        CommandResult::Message(output.trim_end().to_string())
    }
}

// ---- /rewind -------------------------------------------------------------

#[async_trait]
impl SlashCommand for RewindCommand {
    fn name(&self) -> &str { "rewind" }
    fn description(&self) -> &str { "Interactively select a message to rewind to" }
    fn help(&self) -> &str {
        "Usage: /rewind\n\
         Opens an interactive overlay to select the message to rewind to.\n\
         Use ↑↓ to navigate, Enter to select, y/n to confirm."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        if ctx.messages.is_empty() {
            return CommandResult::Message("Nothing to rewind — conversation is empty.".to_string());
        }
        CommandResult::OpenRewindOverlay
    }
}

// ---- /stats --------------------------------------------------------------

#[async_trait]
impl SlashCommand for StatsCommand {
    fn name(&self) -> &str { "stats" }
    fn description(&self) -> &str { "Show token usage and cost statistics" }
    fn help(&self) -> &str {
        "Usage: /stats\n\n\
         Shows detailed token usage and cost breakdown for the current session,\n\
         including cache creation/read token counts, turn counts, and session duration.\n\
         Use /usage for quota and account info. Use /cost for a quick cost summary."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let input = ctx.cost_tracker.input_tokens();
        let output = ctx.cost_tracker.output_tokens();
        let cache_creation = ctx.cost_tracker.cache_creation_tokens();
        let cache_read = ctx.cost_tracker.cache_read_tokens();
        let total = ctx.cost_tracker.total_tokens();
        let cost = ctx.cost_tracker.total_cost_usd();
        let model = ctx.config.effective_model();

        // Count user/assistant turns separately.
        let user_turns = ctx.messages.iter()
            .filter(|m| m.role == claurst_core::types::Role::User)
            .count();
        let assistant_turns = ctx.messages.iter()
            .filter(|m| m.role == claurst_core::types::Role::Assistant)
            .count();

        // Count tool-use invocations.
        let tool_calls: usize = ctx.messages.iter()
            .map(|m| m.get_tool_use_blocks().len())
            .sum();

        // Cost breakdown note: cache-read tokens are cheaper than input, and
        // cache-creation tokens are slightly more expensive. Provide a note if
        // caching is active.
        let cache_note = if cache_creation > 0 || cache_read > 0 {
            format!(
                "\n  (Cache write: {:>10}    Cache read: {:>10})",
                cache_creation, cache_read
            )
        } else {
            String::new()
        };

        CommandResult::Message(format!(
            "Session Statistics\n\
             ══════════════════\n\
             Model:          {model}\n\
             \n\
             Conversation:\n\
               User turns:     {user_turns:>10}\n\
               Assistant turns:{assistant_turns:>10}\n\
               Tool calls:     {tool_calls:>10}\n\
             \n\
             Token usage:\n\
               Input:          {input:>10}\n\
               Output:         {output:>10}\n\
               Total:          {total:>10}{cache_note}\n\
             \n\
             Estimated cost:   ${cost:.4}\n\
             \n\
             Use /usage for quota info · /cost for quick cost · /extra-usage for per-call breakdown",
            model = model,
            user_turns = user_turns,
            assistant_turns = assistant_turns,
            tool_calls = tool_calls,
            input = input,
            output = output,
            total = total,
            cache_note = cache_note,
            cost = cost,
        ))
    }
}

// ---- /files --------------------------------------------------------------

#[async_trait]
impl SlashCommand for FilesCommand {
    fn name(&self) -> &str { "files" }
    fn description(&self) -> &str { "List files referenced in the current conversation" }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        use std::collections::HashSet;
        // Scan message content for file paths (simple heuristic)
        let mut files: HashSet<String> = HashSet::new();
        let path_re = regex::Regex::new(r#"(?m)([A-Za-z]:[\\/][^\s,;:"'<>]+|/[^\s,;:"'<>]{3,})"#).ok();

        for msg in &ctx.messages {
            let text = msg.get_all_text();
            if let Some(ref re) = path_re {
                for cap in re.captures_iter(&text) {
                    let path = cap[1].trim().to_string();
                    if std::path::Path::new(&path).exists() {
                        files.insert(path);
                    }
                }
            }
        }

        if files.is_empty() {
            return CommandResult::Message(
                "No referenced files detected in the conversation.".to_string(),
            );
        }

        let mut sorted: Vec<String> = files.into_iter().collect();
        sorted.sort();

        CommandResult::Message(format!(
            "Referenced files ({}):\n{}",
            sorted.len(),
            sorted.iter().map(|f| format!("  {}", f)).collect::<Vec<_>>().join("\n")
        ))
    }
}

// ---- /rename -------------------------------------------------------------

#[async_trait]
impl SlashCommand for RenameCommand {
    fn name(&self) -> &str { "rename" }
    fn description(&self) -> &str { "Rename the current session" }
    fn help(&self) -> &str {
        "Usage: /rename [new name]\n\n\
         With a name: sets the session title immediately.\n\
         With no argument: auto-generates a kebab-case name from the conversation.\n\n\
         Examples:\n\
           /rename fix-login-bug\n\
           /rename              — auto-generate from conversation history"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let name = args.trim();

        if !name.is_empty() {
            // Explicit name provided: rename immediately.
            return CommandResult::RenameSession(name.to_string());
        }

        // No name given — auto-generate from conversation context.
        if ctx.messages.is_empty() {
            return CommandResult::Error(
                "No conversation context yet. Usage: /rename <name>".to_string(),
            );
        }

        // Build a short conversation excerpt (up to ~2000 chars) for the model.
        let excerpt: String = ctx
            .messages
            .iter()
            .take(20)
            .filter_map(|m| {
                let text = m.get_all_text();
                if text.is_empty() { return None; }
                let role = match m.role {
                    claurst_core::types::Role::User => "User",
                    claurst_core::types::Role::Assistant => "Assistant",
                };
                Some(format!("{}: {}", role, text.chars().take(300).collect::<String>()))
            })
            .collect::<Vec<_>>()
            .join("\n");

        if excerpt.is_empty() {
            return CommandResult::Error(
                "No text content in conversation. Usage: /rename <name>".to_string(),
            );
        }

        let provider = match provider_for_config(&ctx.config).await {
            Some(provider) => provider,
            None => {
                return CommandResult::Error(
                    "Could not create a provider client for auto-naming.\n\
                     Use /rename <name> to set the name manually."
                        .to_string(),
                );
            }
        };
        let rename_model = resolve_fast_model_id(&ctx.config);

        let system_prompt = "Generate a short kebab-case name (2-4 words) that captures the \
            main topic of this conversation. Use lowercase words separated by hyphens. \
            Examples: fix-login-bug, add-auth-feature, refactor-api-client. \
            Respond with ONLY the name, nothing else.";

        let request = claurst_api::ProviderRequest {
            model: rename_model,
            messages: vec![Message::user(format!(
                "Conversation to name:\n\n{}",
                &excerpt[..excerpt.len().min(2000)]
            ))],
            system_prompt: Some(claurst_api::SystemPrompt::Text(system_prompt.to_string())),
            tools: vec![],
            max_tokens: 64,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            thinking: None,
            provider_options: serde_json::Value::Object(Default::default()),
        };

        match provider.create_message(request).await {
            Ok(response) => {
                let raw_text = text_from_content_blocks(&response.content).trim().to_string();

                let generated = raw_text
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '-')
                    .collect::<String>();

                // Trim leading/trailing hyphens and ensure non-empty.
                let cleaned = generated.trim_matches('-').to_string();
                if cleaned.is_empty() {
                    return CommandResult::Error(
                        "Could not generate a valid name from conversation. \
                         Use /rename <name> to set manually.".to_string(),
                    );
                }

                CommandResult::RenameSession(cleaned)
            }
            Err(e) => CommandResult::Error(format!(
                "Auto-name generation failed: {e}\n\
                 Use /rename <name> to set the name manually."
            )),
        }
    }
}

// ---- /effort -------------------------------------------------------------

#[async_trait]
impl SlashCommand for EffortCommand {
    fn name(&self) -> &str { "effort" }
    fn description(&self) -> &str { "Set the model's thinking effort (low | normal | high)" }
    fn help(&self) -> &str {
        "Usage: /effort [low|normal|high]\n\
         Sets how much computation the model uses for reasoning.\n\
         'high' enables extended thinking with a larger budget."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        match args.trim() {
            "" => CommandResult::Message("Current effort: normal\nUse /effort [low|normal|high] to change.".to_string()),
            "low" => {
                // Low effort: smaller max_tokens
                ctx.config.max_tokens = Some(4096);
                CommandResult::ConfigChange(ctx.config.clone())
            }
            "normal" => {
                ctx.config.max_tokens = None; // use default
                CommandResult::ConfigChange(ctx.config.clone())
            }
            "high" => {
                ctx.config.max_tokens = Some(32768);
                CommandResult::ConfigChange(ctx.config.clone())
            }
            other => CommandResult::Error(format!(
                "Unknown effort level '{}'. Use: low | normal | high",
                other
            )),
        }
    }
}

// ---- /summary ------------------------------------------------------------

#[async_trait]
impl SlashCommand for SummaryCommand {
    fn name(&self) -> &str { "summary" }
    fn description(&self) -> &str { "Generate a brief summary of the conversation so far" }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let count = ctx.messages.len();
        if count == 0 {
            return CommandResult::Message("No messages in conversation yet.".to_string());
        }

        // Ask the model to summarize by injecting a hidden user message
        CommandResult::UserMessage(
            "Please provide a brief (3-5 sentence) summary of our conversation so far, \
             focusing on what has been accomplished and the current state."
                .to_string(),
        )
    }
}

// ---- /commit -------------------------------------------------------------

#[async_trait]
impl SlashCommand for CommitCommand {
    fn name(&self) -> &str { "commit" }
    fn description(&self) -> &str { "Ask Claurst to commit staged changes" }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let extra = if args.trim().is_empty() {
            String::new()
        } else {
            format!(" with message: {}", args.trim())
        };

        CommandResult::UserMessage(format!(
            "Please commit the currently staged git changes{}. \
             Run `git diff --cached` to see what's staged, \
             write an appropriate commit message following the repository's conventions, \
             and run `git commit`.",
            extra
        ))
    }
}
