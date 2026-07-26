// Session-control commands: `/plan`, `/tasks`, `/session`, `/fork`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct PlanCommand;
pub struct TasksCommand;
pub struct SessionCommand;
pub struct ForkCommand;

// ---- /plan ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for PlanCommand {
    fn name(&self) -> &str { "plan" }
    fn description(&self) -> &str { "Enter plan mode – model outputs a plan for approval before acting" }
    fn help(&self) -> &str {
        "Usage: /plan [description]\n\n\
         Switches to plan mode where the model will create a detailed plan before executing.\n\
         The plan must be approved before any file writes or command executions are performed.\n\
         Use /plan exit to leave plan mode."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        if args.trim() == "exit" {
            return CommandResult::UserMessage(
                "[Exiting plan mode. Resuming normal execution.]".to_string()
            );
        }
        let task_desc = if args.is_empty() {
            "the current task".to_string()
        } else {
            args.to_string()
        };
        CommandResult::UserMessage(format!(
            "[Entering plan mode for: {}]\n\
             Please create a detailed step-by-step plan. Do not execute any commands or \
             write any files until the plan has been reviewed and approved.",
            task_desc
        ))
    }
}

// ---- /tasks --------------------------------------------------------------

#[async_trait]
impl SlashCommand for TasksCommand {
    fn name(&self) -> &str { "tasks" }
    fn aliases(&self) -> Vec<&str> { vec!["bashes"] }
    fn description(&self) -> &str { "List and manage background tasks" }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        CommandResult::UserMessage(
            "Please list all current tasks using the TaskList tool and show their status.".to_string()
        )
    }
}

// ---- /session ------------------------------------------------------------

#[async_trait]
impl SlashCommand for SessionCommand {
    fn name(&self) -> &str { "session" }
    fn aliases(&self) -> Vec<&str> { vec!["remote"] }
    fn description(&self) -> &str { "Show or manage conversation sessions" }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        match args.trim() {
            "list" => {
                let sessions = claurst_core::history::list_sessions().await;
                if sessions.is_empty() {
                    CommandResult::Message("No saved sessions found.".to_string())
                } else {
                    let mut output = String::from("Recent sessions:\n\n");
                    for sess in sessions.iter().take(10) {
                        let updated = sess.updated_at.format("%Y-%m-%d %H:%M").to_string();
                        let id_short = &sess.id[..sess.id.len().min(8)];
                        output.push_str(&format!(
                            "  {} | {} | {} messages | {}\n",
                            id_short,
                            updated,
                            sess.messages.len(),
                            sess.title.as_deref().unwrap_or("(untitled)")
                        ));
                    }
                    output.push_str("\nUse /resume <id> to resume a session.");
                    CommandResult::Message(output)
                }
            }
            "" => {
                // If a bridge remote URL is active, show it prominently.
                if let Some(ref url) = ctx.remote_session_url {
                    let border = "─".repeat(url.len().min(60) + 4);
                    let display_url = if url.len() > 60 {
                        format!("{}…", &url[..60])
                    } else {
                        url.clone()
                    };
                    CommandResult::Message(format!(
                        "Remote session active\n\
                         ┌{border}┐\n\
                         │  {display_url}  │\n\
                         └{border}┘\n\n\
                         Open the URL above on any device to connect remotely.\n\
                         Session ID: {}",
                        ctx.session_id,
                    ))
                } else {
                    // Show current session info + recent sessions list.
                    let sessions = claurst_core::history::list_sessions().await;
                    let mut output = format!(
                        "Current session\n\
                         ───────────────\n\
                         ID:       {}\n\
                         Title:    {}\n\
                         Messages: {}\n\
                         Model:    {}\n",
                        ctx.session_id,
                        ctx.session_title.as_deref().unwrap_or("(untitled)"),
                        ctx.messages.len(),
                        ctx.config.effective_model()
                    );

                    if !sessions.is_empty() {
                        output.push_str("\nRecent sessions:\n\n");
                        for sess in sessions.iter().take(5) {
                            let updated = sess.updated_at.format("%Y-%m-%d %H:%M").to_string();
                            let id_short = &sess.id[..sess.id.len().min(8)];
                            let marker = if sess.id == ctx.session_id { " ◀ current" } else { "" };
                            output.push_str(&format!(
                                "  {} | {} | {} messages | {}{}\n",
                                id_short,
                                updated,
                                sess.messages.len(),
                                sess.title.as_deref().unwrap_or("(untitled)"),
                                marker,
                            ));
                        }
                        output.push_str("\nUse /session list for all sessions, /resume <id> to switch.");
                    }

                    CommandResult::Message(output)
                }
            }
            _ => CommandResult::Error(format!("Unknown subcommand: {}\n\nUsage: /session [list]", args)),
        }
    }
}

// ---- /fork ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for ForkCommand {
    fn name(&self) -> &str { "fork" }
    fn description(&self) -> &str { "Fork the current session into a new branch" }
    fn help(&self) -> &str {
        "Usage: /fork [message_index]\n\n\
         Fork the current session at the specified message index (or at the\n\
         current point if no index is given).  Creates a new session containing\n\
         messages up to the fork point.\n\n\
         Examples:\n\
           /fork        \u{2014} fork at the current end of the conversation\n\
           /fork 5      \u{2014} fork after message 5"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let fork_index: Option<usize> = args.trim().parse().ok();
        let messages = &ctx.messages;
        let fork_at = fork_index.unwrap_or(messages.len()).min(messages.len());
        let forked_messages: Vec<_> = messages[..fork_at].to_vec();

        let mut new_session = claurst_core::history::ConversationSession::new(
            ctx.config.effective_model().to_string(),
        );
        new_session.messages = forked_messages;
        new_session.parent_session_id = Some(ctx.session_id.clone());
        new_session.fork_point_message_index = Some(fork_at);
        new_session.title = Some(format!(
            "Fork of {}",
            ctx.session_title.as_deref().unwrap_or("session")
        ));
        new_session.working_dir = Some(
            ctx.working_dir.to_string_lossy().to_string(),
        );

        let new_id = new_session.id.clone();
        match claurst_core::history::save_session(&new_session).await {
            Ok(()) => CommandResult::Message(format!(
                "Session forked at message {}. New session: {}\nUse /resume {} to switch to it.",
                fork_at, new_id, new_id
            )),
            Err(e) => CommandResult::Error(format!("Failed to save forked session: {}", e)),
        }
    }
}
