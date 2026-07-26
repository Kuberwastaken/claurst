// `/memory` command (AGENTS.md memory files).
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct MemoryCommand;

// ---- /memory -------------------------------------------------------------

#[async_trait]
impl SlashCommand for MemoryCommand {
    fn name(&self) -> &str { "memory" }
    fn description(&self) -> &str { "View, edit, or clear AGENTS.md memory files" }
    fn help(&self) -> &str {
        "Usage: /memory [edit|clear] [global]\n\n\
         Shows the content of AGENTS.md files that provide project context to Claurst.\n\
         Claurst reads these files automatically at session start.\n\n\
         Subcommands:\n\
           /memory              — show all AGENTS.md files\n\
           /memory edit         — open project AGENTS.md in your editor\n\
           /memory edit global  — open global ~/.claurst/AGENTS.md in your editor\n\
           /memory clear        — clear the project AGENTS.md\n\
           /memory clear global — clear the global ~/.claurst/AGENTS.md\n\n\
         Locations checked (in priority order):\n\
           1. <project>/.claurst/AGENTS.md\n\
           2. <project>/AGENTS.md\n\
           3. ~/.claurst/AGENTS.md  (global)\n\n\
         Use /init to create a new AGENTS.md from a template."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let project_claude_dir = ctx.working_dir.join(".claurst").join("AGENTS.md");
        let project_root = ctx.working_dir.join("AGENTS.md");
        let global_path = claurst_core::config::Settings::config_dir().join("AGENTS.md");

        let locations = [
            ("project (.claurst/AGENTS.md)", project_claude_dir.clone()),
            ("project (AGENTS.md)", project_root.clone()),
            ("global (~/.claurst/AGENTS.md)", global_path.clone()),
        ];

        let cmd = args.trim();

        // ---- /memory edit [global|project] ------------------------------------
        if cmd == "edit" || cmd.starts_with("edit ") {
            let target_hint = cmd.strip_prefix("edit").map(|s| s.trim()).unwrap_or("project");
            let target = match target_hint {
                "global" => {
                    // Ensure global dir exists
                    if let Some(parent) = global_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    global_path.clone()
                }
                _ => {
                    // Best project AGENTS.md
                    if project_root.exists() {
                        project_root.clone()
                    } else if project_claude_dir.exists() {
                        project_claude_dir.clone()
                    } else {
                        project_root.clone() // will be created by editor
                    }
                }
            };
            // Create file if it doesn't exist yet
            if !target.exists() {
                if let Some(parent) = target.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&target, "");
            }
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| {
                    if cfg!(target_os = "windows") {
                        "notepad".to_string()
                    } else {
                        "vi".to_string()
                    }
                });
            let editor_hint = if let Ok(visual) = std::env::var("VISUAL") {
                format!("Using $VISUAL=\"{}\".", visual)
            } else if let Ok(ed) = std::env::var("EDITOR") {
                format!("Using $EDITOR=\"{}\".", ed)
            } else {
                "To use a different editor, set the $EDITOR or $VISUAL environment variable.".to_string()
            };
            let spawn_result = std::process::Command::new(&editor)
                .arg(&target)
                .status();
            return match spawn_result {
                Ok(_) => CommandResult::Message(format!(
                    "Opened {} in your editor.\n{}",
                    target.display(),
                    editor_hint
                )),
                Err(e) => CommandResult::Message(format!(
                    "Could not launch '{}': {}. Edit {} manually.\n{}",
                    editor, e, target.display(), editor_hint
                )),
            };
        }

        // ---- /memory clear [global|project] -----------------------------------
        if cmd == "clear" || cmd.starts_with("clear ") {
            let target_hint = cmd.strip_prefix("clear").map(|s| s.trim()).unwrap_or("project");
            let (label, target) = match target_hint {
                "global" => ("global (~/.claurst/AGENTS.md)", global_path.clone()),
                _ => {
                    if project_claude_dir.exists() {
                        ("project (.claurst/AGENTS.md)", project_claude_dir.clone())
                    } else {
                        ("project (AGENTS.md)", project_root.clone())
                    }
                }
            };
            if !target.exists() {
                return CommandResult::Message(format!(
                    "No {} memory file found (nothing to clear).",
                    label
                ));
            }
            return match tokio::fs::write(&target, "").await {
                Ok(_) => CommandResult::Message(format!(
                    "Cleared {} memory file at {}.\n\
                     Claurst will no longer see this content at session start.",
                    label,
                    target.display()
                )),
                Err(e) => CommandResult::Error(format!(
                    "Failed to clear {}: {}", target.display(), e
                )),
            };
        }

        // ---- /memory (show all) -----------------------------------------------
        let mut output = String::from("AGENTS.md Memory Files\n══════════════════════\n");
        let mut found_any = false;

        for (label, path) in &locations {
            if path.exists() {
                found_any = true;
                match tokio::fs::read_to_string(path).await {
                    Ok(content) => {
                        let lines: usize = content.lines().count();
                        let chars = content.len();
                        output.push_str(&format!(
                            "\n[{label}]\nPath: {path}\nSize: {lines} lines, {chars} chars\n\
                             ─────────────────────────────────\n\
                             {content}\n",
                            label = label,
                            path = path.display(),
                            lines = lines,
                            chars = chars,
                            content = if content.len() > 2000 {
                                format!("{}…\n(truncated — file is {} chars)", &content[..2000], chars)
                            } else {
                                content.clone()
                            }
                        ));
                    }
                    Err(e) => output.push_str(&format!(
                        "\n[{label}] — Error reading {}: {}\n",
                        path.display(), e, label = label
                    )),
                }
            }
        }

        if !found_any {
            output.push_str(
                "\nNo AGENTS.md files found.\n\
                 Use /init to create one in the current project.\n\
                 Use /memory edit to create and open a memory file."
            );
        } else {
            output.push_str(
                "\nSubcommands:\n\
                 /memory edit          — edit project AGENTS.md\n\
                 /memory edit global   — edit global ~/.claurst/AGENTS.md\n\
                 /memory clear         — clear project AGENTS.md\n\
                 /memory clear global  — clear global AGENTS.md"
            );
        }

        CommandResult::Message(output)
    }
}
