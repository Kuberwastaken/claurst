// Goal command: durable long-running autonomous goals (`/goal`).
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::{CommandContext, CommandResult, SlashCommand};
use async_trait::async_trait;

pub struct GoalCommand;

// ---- /goal ---------------------------------------------------------------

/// Parse a soft token budget from strings like "250K", "1M", "500000".
fn parse_token_budget(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('K').or_else(|| s.strip_suffix('k')) {
        (n, 1_000u64)
    } else if let Some(n) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        (n, 1_000_000u64)
    } else {
        (s, 1u64)
    };
    num_str.trim().parse::<u64>().ok().map(|n| n * multiplier)
}

#[async_trait]
impl SlashCommand for GoalCommand {
    fn name(&self) -> &str { "goal" }
    fn description(&self) -> &str { "Set or manage a durable long-running goal for autonomous work" }
    fn help(&self) -> &str {
        "Usage:\n\
         /goal <objective>              — set a new goal and begin working autonomously\n\
         /goal --tokens 250K <text>     — set a goal with a soft token budget\n\
         /goal                          — show current goal status\n\
         /goal status                   — show current goal status\n\
         /goal pause                    — pause the active goal\n\
         /goal resume                   — resume a paused goal\n\
         /goal clear                    — delete the current goal\n\
         /goal complete                 — request a completion audit\n\n\
         Goals let Claurst work autonomously across turns toward a single\n\
         verifiable objective. Claurst will keep iterating until the goal is\n\
         complete, you pause it, or the 200-turn runaway guard fires.\n\n\
         Examples:\n\
         /goal Migrate the project from Express to Fastify, keeping all routes passing\n\
         /goal --tokens 500K Fix all TypeScript errors in src/ without breaking tests"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        if !claurst_core::goals_enabled() {
            return CommandResult::Message(
                "Goals are disabled. Unset CLAURST_GOALS=0 (or remove it) to re-enable.".to_string(),
            );
        }

        let args = args.trim();
        let session_id = &ctx.session_id;

        // Parse subcommands with no objective
        match args {
            "" | "status" => return goal_status(session_id),
            "pause" => {
                let store = match open_goal_store() {
                    Some(s) => s,
                    None => return CommandResult::Error("Could not open goal store.".to_string()),
                };
                match store.get_goal(session_id) {
                    None => return CommandResult::Message("No active goal.".to_string()),
                    Some(g) if g.status == claurst_core::GoalStatus::Complete => {
                        return CommandResult::Message("Goal is already complete.".to_string());
                    }
                    Some(g) if g.status == claurst_core::GoalStatus::Paused => {
                        return CommandResult::Message(
                            "Goal is already paused. Use /goal resume to continue.".to_string(),
                        );
                    }
                    _ => {}
                }
                if let Err(e) = store.set_status(session_id, claurst_core::GoalStatus::Paused) {
                    return CommandResult::Error(format!("Failed to pause goal: {}", e));
                }
                return CommandResult::Message("Goal paused. Use /goal resume to continue.".to_string());
            }
            "resume" => {
                let store = match open_goal_store() {
                    Some(s) => s,
                    None => return CommandResult::Error("Could not open goal store.".to_string()),
                };
                match store.get_goal(session_id) {
                    None => return CommandResult::Message("No goal to resume.".to_string()),
                    Some(g) if g.status == claurst_core::GoalStatus::Active => {
                        return CommandResult::Message("Goal is already active.".to_string());
                    }
                    Some(g) if g.status == claurst_core::GoalStatus::Complete => {
                        return CommandResult::Message(
                            "Goal is complete. Use /goal <objective> to set a new one.".to_string(),
                        );
                    }
                    _ => {}
                }
                if let Err(e) = store.set_status(session_id, claurst_core::GoalStatus::Active) {
                    return CommandResult::Error(format!("Failed to resume goal: {}", e));
                }
                return CommandResult::Message("Goal resumed. Claurst will continue on the next message.".to_string());
            }
            "clear" => {
                let store = match open_goal_store() {
                    Some(s) => s,
                    None => return CommandResult::Error("Could not open goal store.".to_string()),
                };
                store.clear_goal(session_id).unwrap_or_default();
                return CommandResult::Message("Goal cleared.".to_string());
            }
            "complete" => {
                // Inject a completion-audit user message.
                let store = match open_goal_store() {
                    Some(s) => s,
                    None => return CommandResult::Error("Could not open goal store.".to_string()),
                };
                match store.get_active_goal(session_id) {
                    None => {
                        return CommandResult::Message(
                            "No active goal. Set one with /goal <objective>.".to_string(),
                        );
                    }
                    Some(goal) => {
                        let audit_msg = format!(
                            "[User requested goal completion audit]\n\
                             Please review your active goal:\n\
                             <objective>\n{}\n</objective>\n\n\
                             Run through the completion audit:\n\
                             1. Restate the objective as concrete deliverables.\n\
                             2. Check that all deliverables have been achieved.\n\
                             3. Run any tests or validation commands.\n\
                             4. If fully complete, call GoalComplete with audit_summary and evidence.\n\
                             5. If not complete, describe what remains.",
                            goal.objective
                        );
                        return CommandResult::UserMessage(audit_msg);
                    }
                }
            }
            _ => {} // fall through to parse as objective (possibly with --tokens)
        }

        // Parse optional --tokens flag
        let (token_budget, objective) = if args.starts_with("--tokens") {
            // Expected: --tokens <budget> <objective>
            let rest = args.trim_start_matches("--tokens").trim();
            let mut parts = rest.splitn(2, char::is_whitespace);
            let budget_str = parts.next().unwrap_or("");
            let obj = parts.next().unwrap_or("").trim();
            let budget = parse_token_budget(budget_str);
            (budget, obj)
        } else {
            (None, args)
        };

        if objective.is_empty() {
            return CommandResult::Message(
                "Usage: /goal <objective> [--tokens 250K]\n\
                 Or: /goal status|pause|resume|clear|complete"
                    .to_string(),
            );
        }

        let store = match open_goal_store() {
            Some(s) => s,
            None => return CommandResult::Error("Could not open goal store.".to_string()),
        };

        match store.set_goal(session_id, objective, token_budget) {
            Err(claurst_core::GoalError::ObjectiveTooLong { len, max }) => {
                CommandResult::Error(format!(
                    "Objective too long ({} chars). Max {} chars.",
                    len, max
                ))
            }
            Err(e) => CommandResult::Error(format!("Failed to set goal: {}", e)),
            Ok(goal) => {
                // Return UserMessage so the query loop fires immediately and the
                // model begins working toward the goal without user needing to
                // send another message.
                CommandResult::UserMessage(claurst_core::goal_kickoff_message(&goal))
            }
        }
    }
}

fn open_goal_store() -> Option<claurst_core::GoalStore> {
    claurst_core::GoalStore::open_default()
}

fn goal_status(session_id: &str) -> CommandResult {
    let store = match open_goal_store() {
        Some(s) => s,
        None => return CommandResult::Error("Could not open goal store.".to_string()),
    };
    match store.get_goal(session_id) {
        None => CommandResult::Message(
            "No active goal. Set one with:\n  /goal <objective>".to_string(),
        ),
        Some(g) => {
            let budget_line = g
                .budget_display()
                .map(|b| format!("\nBudget:  {}", b))
                .unwrap_or_default();
            CommandResult::Message(format!(
                "Goal status\n\
                 ───────────\n\
                 Status:  {}\n\
                 Turns:   {}\n\
                 Elapsed: {}{}\n\
                 Objective:\n  {}",
                g.status.as_str(),
                g.turns_used,
                g.elapsed_display(),
                budget_line,
                g.objective,
            ))
        }
    }
}
