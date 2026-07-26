// `/copy` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct CopyCommand;

// ---- /copy ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for CopyCommand {
    fn name(&self) -> &str { "copy" }
    fn description(&self) -> &str { "Copy the last assistant response to the clipboard" }
    fn help(&self) -> &str {
        "Usage: /copy [n]\n\n\
         Copies the most recent assistant response to the system clipboard.\n\
         Optionally pass a number to copy the Nth most-recent response."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let n: usize = args.trim().parse().unwrap_or(1).max(1);

        // Find the Nth most recent assistant message
        let assistant_msgs: Vec<&claurst_core::types::Message> = ctx
            .messages
            .iter()
            .rev()
            .filter(|m| m.role == claurst_core::types::Role::Assistant)
            .take(n)
            .collect();

        let msg = match assistant_msgs.last() {
            Some(m) => m,
            None => {
                return CommandResult::Message(
                    "No assistant messages found in conversation.".to_string(),
                )
            }
        };

        let text = msg.get_all_text();
        if text.is_empty() {
            return CommandResult::Message("Last assistant message is empty.".to_string());
        }

        // Try system clipboard via arboard
        #[cfg(not(target_os = "linux"))]
        {
            match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.clone())) {
                Ok(()) => {
                    let preview: String = text.chars().take(80).collect();
                    let ellipsis = if text.len() > 80 { "…" } else { "" };
                    return CommandResult::Message(format!(
                        "Copied {} chars to clipboard.\nPreview: {}{}",
                        text.len(),
                        preview,
                        ellipsis
                    ));
                }
                Err(e) => {
                    tracing::warn!("Clipboard write failed: {}", e);
                    // Fall through to file fallback
                }
            }
        }

        // Fallback: write to a temp file and inform the user
        let tmp_path = std::env::temp_dir().join("claude_copy.md");
        match std::fs::write(&tmp_path, &text) {
            Ok(()) => {
                let preview: String = text.chars().take(80).collect();
                let ellipsis = if text.len() > 80 { "…" } else { "" };
                CommandResult::Message(format!(
                    "Clipboard not available; saved {} chars to {}\nPreview: {}{}",
                    text.len(),
                    tmp_path.display(),
                    preview,
                    ellipsis
                ))
            }
            Err(e) => CommandResult::Error(format!("Failed to copy: {}", e)),
        }
    }
}
