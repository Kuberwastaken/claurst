//! `/productivity` command (alias `/wrapped`): scan saved session transcripts,
//! fold them into a shareable usage report, and render it as Markdown in the
//! chat.
//!
//! The heavy lifting lives in the `claurst-productivity` crate; this module is
//! just the slash-command surface. The command runs synchronously inside the
//! existing `SlashCommand::execute` flow, same as the persisted `/stats`.

use crate::{CommandContext, CommandResult};
use async_trait::async_trait;

pub struct ProductivityCommand;

#[async_trait]
impl crate::SlashCommand for ProductivityCommand {
    fn name(&self) -> &str {
        "productivity"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["wrapped"]
    }
    fn description(&self) -> &str {
        "Generate a shareable productivity report from your saved sessions"
    }
    fn help(&self) -> &str {
        "Usage: /productivity\n\n\
         Scans every saved session transcript under ~/.claurst/projects/ and\n\
         renders a report: total prompts, tool calls, code edits, tokens, active\n\
         days, streaks, top projects and tools, daily rhythm, and a power score.\n\
         First run parses every transcript (cached afterwards, so re-runs are fast)."
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        match claurst_productivity::generate() {
            Ok(out) => CommandResult::Message(out.markdown),
            Err(err) => {
                CommandResult::Error(format!("Failed to generate productivity report: {err}"))
            }
        }
    }
}
