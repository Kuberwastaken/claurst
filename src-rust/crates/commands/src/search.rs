// `/search` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct SearchCommand;

// ---- /search -------------------------------------------------------------

#[async_trait]
impl SlashCommand for SearchCommand {
    fn name(&self) -> &str { "search" }
    fn description(&self) -> &str { "Search across all sessions" }
    fn help(&self) -> &str {
        "Usage: /search <query>\n\n\
         Searches session titles and message content in the local SQLite\n\
         session database (~/.claurst/sessions.db).  Returns the 50 best\n\
         matching sessions, ordered by most recently updated.\n\n\
         Example: /search refactor authentication"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let query = args.trim();
        if query.is_empty() {
            return CommandResult::Error(
                "Usage: /search <query>\n\
                 Provide a search term to look up across all sessions."
                    .to_string(),
            );
        }

        let db_path = claurst_core::config::Settings::config_dir().join("sessions.db");

        let store = match claurst_core::SqliteSessionStore::open(&db_path) {
            Ok(s) => s,
            Err(e) => {
                return CommandResult::Error(format!(
                    "Failed to open session database: {}\n\
                     The database is created automatically once sessions are stored.",
                    e
                ))
            }
        };

        let results = match store.search_sessions(query) {
            Ok(r) => r,
            Err(e) => {
                return CommandResult::Error(format!(
                    "Search failed: {}",
                    e
                ))
            }
        };

        if results.is_empty() {
            return CommandResult::Message(format!(
                "No sessions found matching \"{}\".",
                query
            ));
        }

        let mut out = format!(
            "Search results for \"{}\": {} session(s)\n\n",
            query,
            results.len()
        );
        for s in &results {
            let title = s.title.as_deref().unwrap_or("(untitled)");
            out.push_str(&format!(
                "  [{}] {} — {} ({} messages, updated {})\n",
                &s.id[..s.id.len().min(12)],
                title,
                s.model,
                s.message_count,
                &s.updated_at[..s.updated_at.len().min(10)],
            ));
        }
        out.push_str("\nTip: use /resume <session-id> to continue a session.");
        CommandResult::Message(out)
    }
}
