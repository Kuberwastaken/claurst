// `/managed-agents` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct ManagedAgentsCommand;

// ---- /managed-agents -----------------------------------------------------

#[async_trait]
impl SlashCommand for ManagedAgentsCommand {
    fn name(&self) -> &str { "managed-agents" }
    fn description(&self) -> &str { "Configure and manage the manager-executor agent architecture" }
    fn help(&self) -> &str {
        "Usage: /managed-agents [subcommand]\n\n\
         Subcommands:\n\
           (none) | status                        — show current config\n\
           presets                                — list built-in presets\n\
           preset <name>                          — apply a named preset\n\
           setup                                  — show setup instructions\n\
           configure manager-model <value>        — set manager model\n\
           configure executor-model <value>       — set executor model\n\
           configure executor-turns <n>           — set executor max turns\n\
           configure concurrent <n>               — set max concurrent executors\n\
           configure isolation on|off             — set executor isolation\n\
           configure budget-split shared|percentage:<pct>|fixed:<mgr>:<exe>\n\
           budget <amount>                        — set total budget in USD (0 to clear)\n\
           enable                                 — enable managed agents\n\
           disable                                — disable managed agents\n\
           reset                                  — remove config entirely"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        use claurst_core::{BudgetSplitPolicy, ManagedAgentConfig, builtin_managed_agent_presets};

        let args = args.trim();

        // Helper to format current config as status string
        fn format_status(cfg: &Option<ManagedAgentConfig>) -> String {
            match cfg {
                None => "Managed Agents: NOT CONFIGURED\n\nRun /managed-agents setup to get started.".to_string(),
                Some(c) => {
                    let state = if c.enabled { "ACTIVE" } else { "CONFIGURED but inactive" };
                    let budget_str = match c.total_budget_usd {
                        Some(b) => format!("${:.2} total", b),
                        None => "no cap".to_string(),
                    };
                    let split_str = match &c.budget_split {
                        BudgetSplitPolicy::SharedPool => "shared pool".to_string(),
                        BudgetSplitPolicy::Percentage { manager_pct } => format!("{}% manager", manager_pct),
                        BudgetSplitPolicy::FixedCaps { manager_usd, executor_usd } => {
                            format!("${:.2} mgr / ${:.2} exe", manager_usd, executor_usd)
                        }
                    };
                    let preset = c.preset_name.as_deref().unwrap_or("custom");
                    let isolation = if c.executor_isolation { "on" } else { "off" };
                    format!(
                        "Managed Agents: {}\n  Manager:    {}\n  Executor:   {}\n  Preset:     {}\n  Budget:     {}  |  split: {}\n  Exec limits: {} turns, {} concurrent, isolation: {}\n\nRun /managed-agents <subcommand> — presets | setup | configure | enable | disable | budget | reset",
                        state,
                        c.manager_model,
                        c.executor_model,
                        preset,
                        budget_str,
                        split_str,
                        c.executor_max_turns,
                        c.max_concurrent_executors,
                        isolation,
                    )
                }
            }
        }

        if args.is_empty() || args == "status" {
            return CommandResult::Message(format_status(&ctx.config.managed_agents));
        }

        if args == "presets" {
            let presets = builtin_managed_agent_presets();
            let mut out = "Built-in managed agent presets:\n\n".to_string();
            for p in &presets {
                out.push_str(&format!(
                    "  {:<28} — {}\n    Manager:  {}\n    Executor: {}\n\n",
                    p.name, p.description, p.manager_model, p.executor_model
                ));
            }
            out.push_str("Use: /managed-agents preset <name> to apply a preset.");
            return CommandResult::Message(out);
        }

        if args == "setup" {
            let presets = builtin_managed_agent_presets();
            let mut out = "Managed Agents Setup\n\nQuickstart — apply a preset:\n\n".to_string();
            for p in &presets {
                out.push_str(&format!("  /managed-agents preset {}\n    {}\n\n", p.name, p.description));
            }
            out.push_str("\nOr configure manually:\n  /managed-agents configure manager-model <provider/model>\n  /managed-agents configure executor-model <provider/model>\n  /managed-agents enable\n\nModel format: provider/model (e.g. anthropic/claude-opus-4-6, openai/gpt-4o, google/gemini-2.5-flash)\nAny provider registered in the ProviderRegistry can be used.");
            return CommandResult::Message(out);
        }

        if let Some(preset_name) = args.strip_prefix("preset ").map(str::trim) {
            let presets = builtin_managed_agent_presets();
            let found = presets.iter().find(|p| p.name.eq_ignore_ascii_case(preset_name));
            match found {
                None => {
                    let names: Vec<&str> = presets.iter().map(|p| p.name).collect();
                    return CommandResult::Error(format!(
                        "Unknown preset '{}'. Available: {}",
                        preset_name,
                        names.join(", ")
                    ));
                }
                Some(p) => {
                    let new_cfg = ManagedAgentConfig {
                        enabled: true,
                        manager_model: p.manager_model.to_string(),
                        executor_model: p.executor_model.to_string(),
                        executor_max_turns: p.executor_max_turns,
                        max_concurrent_executors: p.max_concurrent_executors,
                        budget_split: BudgetSplitPolicy::SharedPool,
                        total_budget_usd: None,
                        preset_name: Some(p.name.to_string()),
                        executor_isolation: false,
                    };
                    let name = p.name.to_string();
                    if let Err(e) = save_settings_mutation(|settings| {
                        settings.managed_agents = Some(new_cfg.clone());
                        settings.config.managed_agents = Some(new_cfg.clone());
                    }) {
                        return CommandResult::Error(format!("Failed to save: {}", e));
                    }
                    let mut new_config = ctx.config.clone();
                    new_config.managed_agents = Some(new_cfg);
                    return CommandResult::ConfigChangeMessage(
                        new_config,
                        format!("Applied preset '{}'. Managed agents ENABLED.", name),
                    );
                }
            }
        }

        if let Some(rest) = args.strip_prefix("configure ").map(str::trim) {
            let mut cfg = ctx.config.managed_agents.clone().unwrap_or(ManagedAgentConfig {
                enabled: false,
                manager_model: String::new(),
                executor_model: String::new(),
                executor_max_turns: 10,
                max_concurrent_executors: 4,
                budget_split: BudgetSplitPolicy::SharedPool,
                total_budget_usd: None,
                preset_name: None,
                executor_isolation: false,
            });

            if let Some(val) = rest.strip_prefix("manager-model ").map(str::trim) {
                cfg.manager_model = val.to_string();
                cfg.preset_name = None;
            } else if let Some(val) = rest.strip_prefix("executor-model ").map(str::trim) {
                cfg.executor_model = val.to_string();
                cfg.preset_name = None;
            } else if let Some(val) = rest.strip_prefix("executor-turns ").map(str::trim) {
                match val.parse::<u32>() {
                    Ok(n) => cfg.executor_max_turns = n,
                    Err(_) => return CommandResult::Error(format!("Invalid number: '{}'", val)),
                }
            } else if let Some(val) = rest.strip_prefix("concurrent ").map(str::trim) {
                match val.parse::<u32>() {
                    Ok(n) => cfg.max_concurrent_executors = n,
                    Err(_) => return CommandResult::Error(format!("Invalid number: '{}'", val)),
                }
            } else if let Some(val) = rest.strip_prefix("isolation ").map(str::trim) {
                match val {
                    "on" => cfg.executor_isolation = true,
                    "off" => cfg.executor_isolation = false,
                    _ => return CommandResult::Error("Use 'on' or 'off'".to_string()),
                }
            } else if let Some(val) = rest.strip_prefix("budget-split ").map(str::trim) {
                if val == "shared" {
                    cfg.budget_split = BudgetSplitPolicy::SharedPool;
                } else if let Some(pct_str) = val.strip_prefix("percentage:") {
                    match pct_str.parse::<u8>() {
                        Ok(pct) => cfg.budget_split = BudgetSplitPolicy::Percentage { manager_pct: pct },
                        Err(_) => return CommandResult::Error(format!("Invalid percentage: '{}'", pct_str)),
                    }
                } else if let Some(caps_str) = val.strip_prefix("fixed:") {
                    let parts: Vec<&str> = caps_str.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        match (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                            (Ok(m), Ok(e)) => cfg.budget_split = BudgetSplitPolicy::FixedCaps { manager_usd: m, executor_usd: e },
                            _ => return CommandResult::Error("Invalid fixed caps format. Use fixed:<manager>:<executor>".to_string()),
                        }
                    } else {
                        return CommandResult::Error("Invalid fixed caps format. Use fixed:<manager>:<executor>".to_string());
                    }
                } else {
                    return CommandResult::Error("Use: shared | percentage:<pct> | fixed:<manager>:<executor>".to_string());
                }
            } else {
                return CommandResult::Error(format!(
                    "Unknown configure option: '{}'\nOptions: manager-model, executor-model, executor-turns, concurrent, isolation, budget-split",
                    rest
                ));
            }

            if let Err(e) = save_settings_mutation(|settings| {
                settings.managed_agents = Some(cfg.clone());
                settings.config.managed_agents = Some(cfg.clone());
            }) {
                return CommandResult::Error(format!("Failed to save: {}", e));
            }
            let mut new_config = ctx.config.clone();
            new_config.managed_agents = Some(cfg);
            return CommandResult::ConfigChangeMessage(new_config, "Managed agents configuration updated.".to_string());
        }

        if let Some(amount_str) = args.strip_prefix("budget ").map(str::trim) {
            match amount_str.parse::<f64>() {
                Err(_) => return CommandResult::Error(format!("Invalid amount: '{}'", amount_str)),
                Ok(amount) => {
                    let mut cfg = match ctx.config.managed_agents.clone() {
                        None => return CommandResult::Error("No managed agents config. Run /managed-agents setup first.".to_string()),
                        Some(c) => c,
                    };
                    cfg.total_budget_usd = if amount <= 0.0 { None } else { Some(amount) };
                    if let Err(e) = save_settings_mutation(|settings| {
                        settings.managed_agents = Some(cfg.clone());
                        settings.config.managed_agents = Some(cfg.clone());
                    }) {
                        return CommandResult::Error(format!("Failed to save: {}", e));
                    }
                    let mut new_config = ctx.config.clone();
                    let msg = if amount <= 0.0 {
                        "Budget cap cleared.".to_string()
                    } else {
                        format!("Budget set to ${:.2}.", amount)
                    };
                    new_config.managed_agents = Some(cfg);
                    return CommandResult::ConfigChangeMessage(new_config, msg);
                }
            }
        }

        if args == "enable" {
            let mut cfg = match ctx.config.managed_agents.clone() {
                None => return CommandResult::Error("No managed agents config. Run /managed-agents setup first.".to_string()),
                Some(c) => c,
            };
            if cfg.manager_model.is_empty() || cfg.executor_model.is_empty() {
                return CommandResult::Error("manager_model and executor_model must be set before enabling.".to_string());
            }
            cfg.enabled = true;
            if let Err(e) = save_settings_mutation(|settings| {
                settings.managed_agents = Some(cfg.clone());
                settings.config.managed_agents = Some(cfg.clone());
            }) {
                return CommandResult::Error(format!("Failed to save: {}", e));
            }
            let mut new_config = ctx.config.clone();
            new_config.managed_agents = Some(cfg);
            return CommandResult::ConfigChangeMessage(new_config, "Managed agents ENABLED.".to_string());
        }

        if args == "disable" {
            let mut cfg = match ctx.config.managed_agents.clone() {
                None => return CommandResult::Error("No managed agents config.".to_string()),
                Some(c) => c,
            };
            cfg.enabled = false;
            if let Err(e) = save_settings_mutation(|settings| {
                settings.managed_agents = Some(cfg.clone());
                settings.config.managed_agents = Some(cfg.clone());
            }) {
                return CommandResult::Error(format!("Failed to save: {}", e));
            }
            let mut new_config = ctx.config.clone();
            new_config.managed_agents = Some(cfg);
            return CommandResult::ConfigChangeMessage(new_config, "Managed agents disabled.".to_string());
        }

        if args == "reset" {
            if let Err(e) = save_settings_mutation(|settings| {
                settings.managed_agents = None;
                settings.config.managed_agents = None;
            }) {
                return CommandResult::Error(format!("Failed to save: {}", e));
            }
            let mut new_config = ctx.config.clone();
            new_config.managed_agents = None;
            return CommandResult::ConfigChangeMessage(new_config, "Managed agents configuration removed.".to_string());
        }

        CommandResult::Error(format!(
            "Unknown subcommand: '{}'\nRun /managed-agents to see usage.",
            args
        ))
    }
}
