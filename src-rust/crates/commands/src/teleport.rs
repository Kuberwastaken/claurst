// `/teleport` command and its bundle format (`teleport_bundle`).
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct TeleportCommand;

// ---- /teleport -----------------------------------------------------------

/// Serialisable bundle written to / read from a `.teleport` file.
mod teleport_bundle {
    use claurst_core::permissions::{PermissionAction, SerializedPermissionRule};
    use claurst_core::types::Message;
    use serde::{Deserialize, Serialize};

    pub const BUNDLE_VERSION: &str = "1";

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct TeleportBundle {
        /// Always `"1"`.
        pub version: String,
        pub session_id: String,
        pub messages: Vec<Message>,
        pub working_dir: String,
        pub permissions: TeleportPermissions,
        pub model: Option<String>,
        pub effort: Option<String>,
        /// Recently accessed file paths extracted from tool-use blocks.
        pub files: Vec<String>,
        /// Environment variables — configured provider API key env vars are excluded for security.
        pub env: std::collections::HashMap<String, String>,
        pub exported_at: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    pub struct TeleportPermissions {
        pub allowed: Vec<String>,
        pub denied: Vec<String>,
        pub rules: Vec<SerializedPermissionRule>,
    }

    impl TeleportPermissions {
        #[allow(dead_code)]
        pub fn from_rules(rules: &[SerializedPermissionRule]) -> Self {
            let mut allowed = Vec::new();
            let mut denied = Vec::new();
            for r in rules {
                let name = r.tool_name.clone().unwrap_or_else(|| "*".to_string());
                match r.action {
                    PermissionAction::Allow => allowed.push(name),
                    PermissionAction::Deny => denied.push(name),
                }
            }
            TeleportPermissions {
                allowed,
                denied,
                rules: rules.to_vec(),
            }
        }
    }
}

#[async_trait]
impl SlashCommand for TeleportCommand {
    fn name(&self) -> &str { "teleport" }
    fn description(&self) -> &str { "Export/import/link session context as a portable bundle" }
    fn help(&self) -> &str {
        "Usage:\n\
         \n\
         /teleport export [--output <file>]\n\
         \x20 Serialize the current session to a .teleport JSON bundle.\n\
         \x20 Defaults to ~/.claurst/teleport_<session_id>.json\n\
         \n\
         /teleport import <file>\n\
         \x20 Load a .teleport bundle and restore messages, working dir, and\n\
         \x20 tool permissions into the current session.\n\
         \n\
         /teleport link\n\
         \x20 Generate a teleport:// deep link (base64-encoded bundle) for sharing."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        use teleport_bundle::{TeleportBundle, TeleportPermissions, BUNDLE_VERSION};

        let args = args.trim();

        // Dispatch on first token.
        let (sub, rest) = match args.split_once(|c: char| c.is_whitespace()) {
            Some((s, r)) => (s, r.trim()),
            None => (args, ""),
        };

        match sub {
            "export" => {
                // ---- determine output path --------------------------------
                let output_path: std::path::PathBuf = {
                    // Parse --output <file>
                    let explicit = if let Some(stripped) = rest.strip_prefix("--output") {
                        let path_str = stripped.trim();
                        if !path_str.is_empty() {
                            Some(std::path::PathBuf::from(path_str))
                        } else {
                            None
                        }
                    } else if !rest.is_empty() {
                        // Bare path without --output flag is also accepted.
                        Some(std::path::PathBuf::from(rest))
                    } else {
                        None
                    };

                    if let Some(p) = explicit {
                        p
                    } else {
                        // Default: <claurst home>/teleport_<session_id>.json
                        let base = claurst_core::config::Settings::config_dir();
                        let _ = std::fs::create_dir_all(&base);
                        base.join(format!("teleport_{}.json", ctx.session_id))
                    }
                };

                // ---- collect recently accessed file paths from messages ----
                let files: Vec<String> = {
                    use claurst_core::types::{ContentBlock, MessageContent};
                    let mut seen: Vec<String> = Vec::new();
                    for msg in &ctx.messages {
                        if let MessageContent::Blocks(blocks) = &msg.content {
                            for block in blocks {
                                match block {
                                    ContentBlock::ToolUse { input, .. } => {
                                        // Read/Write/Edit/Glob/Grep all take a
                                        // "path" or "file_path" argument.
                                        let candidates = ["path", "file_path", "filePath"];
                                        for key in &candidates {
                                            if let Some(v) = input.get(key) {
                                                if let Some(s) = v.as_str() {
                                                    if !s.is_empty() && !seen.contains(&s.to_string()) {
                                                        seen.push(s.to_string());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    ContentBlock::CollapsedReadSearch { paths, .. } => {
                                        for p in paths {
                                            if !seen.contains(p) {
                                                seen.push(p.clone());
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    seen.into_iter().take(50).collect()
                };

                // ---- collect env vars (exclude configured provider secrets) --
                let mut redacted_env_vars: std::collections::HashSet<String> = ctx
                    .config
                    .provider_configs
                    .keys()
                    .flat_map(|provider_id| {
                        claurst_core::config::api_key_env_vars_for_provider(provider_id)
                            .iter()
                            .copied()
                    })
                    .map(str::to_string)
                    .collect();
                redacted_env_vars.extend(
                    claurst_core::config::api_key_env_vars_for_provider(ctx.config.selected_provider_id())
                        .iter()
                        .copied()
                        .map(str::to_string),
                );
                let env: std::collections::HashMap<String, String> = std::env::vars()
                    .filter(|(k, _)| !redacted_env_vars.contains(k))
                    .collect();

                // ---- build permissions snapshot from config ----------------
                // The config holds allowed_tools / disallowed_tools as plain
                // tool-name strings; we also pull any serialized permission rules
                // from the settings if accessible.
                let permissions = {
                    let allowed: Vec<String> = ctx.config.allowed_tools.clone();
                    let denied: Vec<String> = ctx.config.disallowed_tools.clone();
                    // Build minimal SerializedPermissionRule list from config lists.
                    let mut rules = Vec::new();
                    use claurst_core::permissions::{PermissionAction, SerializedPermissionRule};
                    for name in &allowed {
                        rules.push(SerializedPermissionRule {
                            tool_name: Some(name.clone()),
                            path_pattern: None,
                            action: PermissionAction::Allow,
                        });
                    }
                    for name in &denied {
                        rules.push(SerializedPermissionRule {
                            tool_name: Some(name.clone()),
                            path_pattern: None,
                            action: PermissionAction::Deny,
                        });
                    }
                    TeleportPermissions { allowed, denied, rules }
                };

                // ---- build bundle -----------------------------------------
                let bundle = TeleportBundle {
                    version: BUNDLE_VERSION.to_string(),
                    session_id: ctx.session_id.clone(),
                    messages: ctx.messages.clone(),
                    working_dir: ctx.working_dir.to_string_lossy().into_owned(),
                    permissions,
                    model: ctx.config.model.clone(),
                    effort: None, // EffortLevel not stored in CommandContext directly
                    files,
                    env,
                    exported_at: chrono::Utc::now().to_rfc3339(),
                };

                // ---- serialize and write ----------------------------------
                let json = match serde_json::to_string_pretty(&bundle) {
                    Ok(j) => j,
                    Err(e) => return CommandResult::Error(format!("Failed to serialize bundle: {}", e)),
                };

                if let Err(e) = std::fs::write(&output_path, &json) {
                    return CommandResult::Error(format!(
                        "Failed to write teleport bundle to {}: {}",
                        output_path.display(),
                        e
                    ));
                }

                CommandResult::Message(format!(
                    "Teleport bundle exported.\n\
                     File:     {}\n\
                     Session:  {}\n\
                     Messages: {}\n\
                     Files:    {}\n\
                     Model:    {}\n\
                     Time:     {}",
                    output_path.display(),
                    bundle.session_id,
                    bundle.messages.len(),
                    bundle.files.len(),
                    bundle.model.as_deref().unwrap_or("(default)"),
                    bundle.exported_at,
                ))
            }

            "import" => {
                if rest.is_empty() {
                    return CommandResult::Error(
                        "Usage: /teleport import <file>".to_string(),
                    );
                }

                let path = std::path::PathBuf::from(rest);

                let data = match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(e) => return CommandResult::Error(format!(
                        "Cannot read teleport bundle '{}': {}",
                        path.display(),
                        e
                    )),
                };

                let bundle: TeleportBundle = match serde_json::from_str(&data) {
                    Ok(b) => b,
                    Err(e) => return CommandResult::Error(format!(
                        "Failed to parse teleport bundle: {}",
                        e
                    )),
                };

                // ---- validate version ------------------------------------
                if bundle.version != BUNDLE_VERSION {
                    return CommandResult::Error(format!(
                        "Unsupported teleport bundle version '{}' (expected '{}').",
                        bundle.version, BUNDLE_VERSION
                    ));
                }

                // ---- restore working directory ---------------------------
                let restored_dir = std::path::PathBuf::from(&bundle.working_dir);
                if restored_dir.exists() {
                    ctx.working_dir = restored_dir.clone();
                    let _ = std::env::set_current_dir(&restored_dir);
                }

                // ---- restore tool permissions ----------------------------
                let mut new_config = ctx.config.clone();
                new_config.allowed_tools = bundle.permissions.allowed.clone();
                new_config.disallowed_tools = bundle.permissions.denied.clone();
                if let Some(ref model) = bundle.model {
                    new_config.model = Some(model.clone());
                }
                ctx.config = new_config.clone();

                // ---- restore messages ------------------------------------
                // Capture summary fields before moving bundle.messages.
                let msg_count = bundle.messages.len();
                let files_count = bundle.files.len();
                let working_dir_display = bundle.working_dir.clone();
                let session_id = bundle.session_id.clone();
                let exported_at = bundle.exported_at.clone();
                let allowed_count = bundle.permissions.allowed.len();
                let denied_count = bundle.permissions.denied.len();
                let dir_restored = restored_dir.exists();

                // Directly replace messages in the live context; the caller's
                // REPL will see the updated ctx.messages on the next turn.
                ctx.messages = bundle.messages;

                CommandResult::Message(format!(
                    "Teleport bundle imported.\n\
                     Source session: {}\n\
                     Exported at:    {}\n\
                     Messages:       {} restored\n\
                     Working dir:    {}{}\n\
                     Permissions:    {} allowed, {} denied\n\
                     Files tracked:  {}",
                    session_id,
                    exported_at,
                    msg_count,
                    working_dir_display,
                    if dir_restored { " (restored)" } else { " (path not found, skipped)" },
                    allowed_count,
                    denied_count,
                    files_count,
                ))
            }

            "link" => {
                // ---- build a minimal bundle for the link (no env vars) ---
                use teleport_bundle::TeleportBundle;
                use base64::Engine as _;

                let permissions = {
                    let allowed = ctx.config.allowed_tools.clone();
                    let denied = ctx.config.disallowed_tools.clone();
                    use claurst_core::permissions::{PermissionAction, SerializedPermissionRule};
                    let mut rules = Vec::new();
                    for name in &allowed {
                        rules.push(SerializedPermissionRule {
                            tool_name: Some(name.clone()),
                            path_pattern: None,
                            action: PermissionAction::Allow,
                        });
                    }
                    for name in &denied {
                        rules.push(SerializedPermissionRule {
                            tool_name: Some(name.clone()),
                            path_pattern: None,
                            action: PermissionAction::Deny,
                        });
                    }
                    TeleportPermissions { allowed, denied, rules }
                };

                let bundle = TeleportBundle {
                    version: BUNDLE_VERSION.to_string(),
                    session_id: ctx.session_id.clone(),
                    messages: ctx.messages.clone(),
                    working_dir: ctx.working_dir.to_string_lossy().into_owned(),
                    permissions,
                    model: ctx.config.model.clone(),
                    effort: None,
                    files: Vec::new(), // keep link compact
                    env: std::collections::HashMap::new(), // omit env for security
                    exported_at: chrono::Utc::now().to_rfc3339(),
                };

                let json = match serde_json::to_string(&bundle) {
                    Ok(j) => j,
                    Err(e) => return CommandResult::Error(format!("Failed to serialize bundle: {}", e)),
                };

                let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes());
                let link = format!("teleport://{}", encoded);

                // Warn if the link is very long.
                let size_hint = if link.len() > 8192 {
                    format!("\n(Link is {} bytes — consider /teleport export for large sessions)", link.len())
                } else {
                    String::new()
                };

                CommandResult::Message(format!(
                    "Teleport link generated for session {}:\n\n{}{}\n\n\
                     Share this link or use: /teleport import <link-url>",
                    ctx.session_id,
                    link,
                    size_hint,
                ))
            }

            "" => {
                // No subcommand — show usage.
                CommandResult::Message(
                    "Usage:\n\
                     \x20 /teleport export [--output <file>]   export session to .teleport bundle\n\
                     \x20 /teleport import <file>              restore a .teleport bundle\n\
                     \x20 /teleport link                       generate a teleport:// deep link\n\
                     \nSee /help teleport for details.".to_string()
                )
            }

            other => CommandResult::Error(format!(
                "Unknown /teleport subcommand '{}'. Valid: export, import, link",
                other
            )),
        }
    }
}
