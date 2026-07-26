// History commands: `/undo`, `/revert`, `/checkpoints`, `/snapshot`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct UndoCommand;
pub struct RevertCommand;
pub struct CheckpointsCommand;
pub struct SnapshotDiffCommand;

// ---- /undo (alias for /revert targeting the most recent assistant turn) ----

#[async_trait]
impl SlashCommand for UndoCommand {
    fn name(&self) -> &str { "undo" }
    fn aliases(&self) -> Vec<&str> { vec![] }
    fn description(&self) -> &str { "Revert all file changes from the last assistant turn (alias: /revert)" }
    fn help(&self) -> &str {
        "Usage: /undo\n\nReverts all file changes made during the most recent assistant turn.\n\
         For finer control use /revert. To list what changed, use /checkpoints."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        RevertCommand.execute("", ctx).await
    }
}

// ---- /revert ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for RevertCommand {
    fn name(&self) -> &str { "revert" }
    fn description(&self) -> &str { "Revert file changes from an assistant turn back to pre-turn state" }
    fn help(&self) -> &str {
        "Usage: /revert [<n>|<uuid>]\n\n\
         Without args: revert the most recent assistant turn.\n\
         With a number n: revert the n-th most recent assistant turn (1 = latest).\n\
         With a uuid: revert the turn whose message id starts with that string.\n\n\
         This uses the shadow-git snapshot to restore all files that were\n\
         changed during the target turn, and removes that turn (and any later\n\
         turns) from the session transcript.\n\n\
         Examples:\n\
           /revert        — revert last turn\n\
           /revert 2      — revert the second-to-last turn\n\
           /revert abc123 — revert the turn with uuid starting 'abc123'"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let snap = match claurst_core::snapshot::get_or_create(&ctx.working_dir) {
            Some(s) => s,
            None => return CommandResult::Error(
                "Snapshot system unavailable (git not found or not a git repo).".into()
            ),
        };

        // Collect assistant messages that have a snapshot patch (newest last).
        let checkpoints: Vec<&claurst_core::types::Message> = ctx.messages.iter()
            .filter(|m| {
                m.role == claurst_core::types::Role::Assistant
                    && m.snapshot_patch.is_some()
            })
            .collect();

        if checkpoints.is_empty() {
            return CommandResult::Message(
                "No revertible turns found. Run /checkpoints to see recorded file changes.".into()
            );
        }

        // Select the target turn.
        let args = args.trim();
        let target = if args.is_empty() {
            checkpoints.last().copied()
        } else if let Ok(n) = args.parse::<usize>() {
            if n == 0 || n > checkpoints.len() {
                return CommandResult::Error(format!(
                    "Turn {} out of range (1–{}).", n, checkpoints.len()
                ));
            }
            Some(checkpoints[checkpoints.len() - n])
        } else {
            checkpoints.iter().copied()
                .find(|m| m.uuid.as_deref().is_some_and(|u| u.starts_with(args)))
        };

        let target = match target {
            Some(m) => m,
            None => return CommandResult::Error(format!("No turn found matching '{args}'.")),
        };

        // Collect all patches from this turn onward to revert.
        let target_uuid = match target.uuid.clone() {
            Some(u) => u,
            None => return CommandResult::Error("Target turn has no uuid; cannot revert.".into()),
        };

        let patches: Vec<claurst_core::snapshot::Patch> = ctx.messages.iter()
            .skip_while(|m| m.uuid.as_deref() != Some(&target_uuid))
            .filter_map(|m| m.snapshot_patch.clone())
            .collect();

        if patches.is_empty() {
            return CommandResult::Message("No file changes recorded for that turn.".into());
        }

        // Revert files.
        snap.revert(&patches).await;

        // Record the revert in the session transcript. NON-DESTRUCTIVE (#234):
        // rather than truncating, point the active leaf at the turn *before* the
        // target so the reverted turn (and everything after it) is retained on a
        // sibling branch that can be returned to. `branch_before` only falls
        // back to a destructive truncate for legacy/unchained transcripts.
        let project_root = claurst_core::git_utils::get_repo_root(&ctx.working_dir)
            .unwrap_or_else(|| ctx.working_dir.clone());
        let path = match claurst_core::session_storage::transcript_path(&project_root, &ctx.session_id) {
            Ok(p) => p,
            Err(e) => return CommandResult::Error(format!("Invalid session ID: {e}")),
        };
        if path.exists() {
            if let Err(e) = claurst_core::session_storage::branch_before(&path, &target_uuid).await {
                return CommandResult::Error(format!("Reverted files but could not update transcript: {e}"));
            }
        }

        let file_count: usize = patches.iter().map(|p| p.files.len()).sum();
        CommandResult::Message(format!(
            "Reverted {} file(s) changed during turn {}. Later turns kept on a branch.",
            file_count,
            &target_uuid[..target_uuid.len().min(8)],
        ))
    }
}

// ---- /checkpoints ----------------------------------------------------------

#[async_trait]
impl SlashCommand for CheckpointsCommand {
    fn name(&self) -> &str { "checkpoints" }
    fn description(&self) -> &str { "List assistant turns that have recorded file changes" }
    fn help(&self) -> &str {
        "Usage: /checkpoints\n\nShows all assistant turns in this session that modified files,\n\
         with file counts.  Use /revert <n> to roll back to a specific turn."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let checkpoints: Vec<(usize, &claurst_core::types::Message)> = ctx.messages.iter()
            .enumerate()
            .filter(|(_, m)| {
                m.role == claurst_core::types::Role::Assistant
                    && m.snapshot_patch.is_some()
            })
            .collect();

        if checkpoints.is_empty() {
            return CommandResult::Message(
                "No file-change checkpoints recorded yet for this session.\n\
                 Checkpoints are created automatically when the assistant modifies files.".into()
            );
        }

        let total = checkpoints.len();
        let mut lines = vec![format!("{} checkpoint(s):", total)];
        for (rank, (_, msg)) in checkpoints.iter().rev().enumerate() {
            let uuid_short = msg.uuid.as_deref()
                .map(|u| &u[..u.len().min(8)])
                .unwrap_or("?");
            let file_count = msg.snapshot_patch.as_ref().map_or(0, |p| p.files.len());
            let preview: Vec<String> = msg.snapshot_patch.as_ref()
                .map(|p| {
                    p.files.iter().take(3)
                        .map(|f| f.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default())
                        .collect()
                })
                .unwrap_or_default();
            let preview_str = if preview.len() == file_count {
                preview.join(", ")
            } else {
                format!("{}, …", preview.join(", "))
            };
            lines.push(format!(
                "  [{}] {} — {} file(s): {}",
                rank + 1, uuid_short, file_count, preview_str
            ));
        }
        lines.push(String::new());
        lines.push("Use /revert <n> to revert to before turn [n].".into());
        CommandResult::Message(lines.join("\n"))
    }
}

// ---- /snapshot (show snapshot diff for a recorded turn) ------------------

#[async_trait]
impl SlashCommand for SnapshotDiffCommand {
    fn name(&self) -> &str { "snapshot" }
    fn description(&self) -> &str { "Show shadow-git diff of file changes from an assistant turn" }
    fn help(&self) -> &str {
        "Usage: /snapshot [<n>|<hash>]\n\n\
         Without args: show unified diff for the most recent assistant turn.\n\
         With a number: show diff for the n-th most recent turn (1 = latest).\n\
         With a hash: show diff against that explicit snapshot tree hash.\n\n\
         See also: /checkpoints (list turns), /revert (roll back files)."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let snap = match claurst_core::snapshot::get_or_create(&ctx.working_dir) {
            Some(s) => s,
            None => return CommandResult::Error(
                "Snapshot system unavailable (git not found or not a git repo).".into()
            ),
        };

        let args = args.trim();

        // If a raw hash was passed, use it directly.
        let hash = if !args.is_empty() && args.chars().all(|c| c.is_ascii_hexdigit()) && args.len() >= 8 {
            args.to_string()
        } else {
            // Otherwise find the n-th most recent checkpoint.
            let checkpoints: Vec<&claurst_core::snapshot::Patch> = ctx.messages.iter()
                .filter_map(|m| {
                    if m.role == claurst_core::types::Role::Assistant {
                        m.snapshot_patch.as_ref()
                    } else {
                        None
                    }
                })
                .collect();

            if checkpoints.is_empty() {
                return CommandResult::Message(
                    "No snapshot checkpoints recorded yet. File changes will appear here after the next assistant turn.".into()
                );
            }

            let idx = if args.is_empty() {
                0
            } else {
                match args.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= checkpoints.len() => n - 1,
                    _ => return CommandResult::Error(format!(
                        "Turn '{}' out of range (1–{}).", args, checkpoints.len()
                    )),
                }
            };
            // Reverse so idx=0 is newest.
            let patch = checkpoints[checkpoints.len() - 1 - idx];
            patch.hash.clone()
        };

        let diff = snap.diff(&hash).await;
        if diff.is_empty() {
            CommandResult::Message(format!("No changes since snapshot {}.", &hash[..hash.len().min(8)]))
        } else {
            CommandResult::Message(diff)
        }
    }
}
