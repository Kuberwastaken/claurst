// FileEdit tool: exact string replacement with old/new strings (like sed but
// deterministic).  Mirrors the TypeScript Edit tool behaviour.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;
use tracing::debug;

pub struct FileEditTool;

#[derive(Debug, Deserialize)]
struct FileEditInput {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for FileEditTool {
    // Gates itself: calls `ctx.check_permission_for_path` in `execute()` (#210).
    fn self_gates(&self) -> bool { true }

    fn name(&self) -> &str {
        claurst_core::constants::TOOL_NAME_FILE_EDIT
    }

    fn description(&self) -> &str {
        "Performs exact string replacements in files. The edit will FAIL if \
         `old_string` is not unique in the file (unless `replace_all` is true). \
         You MUST read the file first before editing. Preserve the exact \
         indentation as it appears in the file."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to modify"
                },
                "old_string": {
                    "type": "string",
                    "description": "The text to replace (must be unique in the file unless replace_all is true)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The text to replace it with (must be different from old_string)"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences of old_string (default false)"
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: FileEditInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        // Validate old != new
        if params.old_string == params.new_string {
            return ToolResult::error("old_string and new_string must be different".to_string());
        }

        if params.old_string.is_empty() {
            return ToolResult::error("old_string must not be empty".to_string());
        }

        let path = ctx.resolve_path(&params.file_path);
        debug!(path = %path.display(), "Editing file");

        // Permission check
        let grant = match ctx.check_permission_for_path(
            self.name(),
            &format!("Edit {}", path.display()),
            path.clone(),
            false,
        ) {
            Ok(grant) => grant,
            Err(e) => return ToolResult::error(e.to_string()),
        };

        // Open once and re-check where the handle really points; the read
        // and the write below are bound to this handle, so a symlink swapped
        // in after the permission check cannot redirect the edit (#415).
        let file = match crate::open_regular_file(&path) {
            Ok(f) => f,
            Err(e) => {
                return ToolResult::error(format!("Failed to read file {}: {}", path.display(), e));
            }
        };
        let real = match ctx.confirm_opened_path(self.name(), "Edit", &path, &file, grant, false) {
            Ok(real) => real,
            Err(e) => return ToolResult::error(e),
        };

        // Read current content
        let mut content = String::new();
        if let Err(e) = (&file).read_to_string(&mut content) {
            return ToolResult::error(format!("Failed to read file {}: {}", path.display(), e));
        }

        // Detect the file's original/dominant line ending BEFORE editing so we
        // can re-apply it on write (#225).  Matching is done against an
        // LF-normalized view so CRLF/LF differences never affect the match, but
        // only the lines the edit actually changes are ever rewritten.
        let eol = crate::line_endings::LineEnding::detect(&content);
        let normalized = content.replace("\r\n", "\n");
        let old_string = params.old_string.replace("\r\n", "\n");
        let new_string = params.new_string.replace("\r\n", "\n");

        // Count occurrences
        let count = normalized.matches(&old_string).count();

        if count == 0 {
            return ToolResult::error(format!(
                "old_string not found in {}. Make sure the string matches exactly, \
                 including whitespace and indentation.",
                path.display()
            ));
        }

        if count > 1 && !params.replace_all {
            return ToolResult::error(format!(
                "old_string appears {} times in {}. Either provide a larger string \
                 with more surrounding context to make it unique, or set replace_all \
                 to true to replace every occurrence.",
                count,
                path.display()
            ));
        }

        // Perform the replacement on the ORIGINAL bytes, preserving every
        // untouched region's line endings and re-rendering inserted lines with
        // the file's dominant line ending.
        let (new_content, _replacements) = crate::line_endings::replace_preserving_eol(
            &content,
            &old_string,
            &new_string,
            eol,
            params.replace_all,
        );

        // Write back atomically into the directory that holds the opened file.
        if let Err(e) = crate::write_atomic_bound(&file, &real, new_content.as_bytes()).await {
            return ToolResult::error(format!("Failed to write file {}: {}", path.display(), e));
        }

        ctx.record_file_change(
            path.clone(),
            content.as_bytes(),
            new_content.as_bytes(),
            self.name(),
        );

        // Run any configured formatter for this file type.
        // Format the file that was written, not whatever `path` names now.
        crate::try_format_file(&real.to_string_lossy(), ctx).await;

        // Build a diff snippet for the response
        let replacements = if params.replace_all { count } else { 1 };
        let msg = format!(
            "Successfully edited {} ({} replacement{}).",
            path.display(),
            replacements,
            if replacements != 1 { "s" } else { "" }
        );

        ToolResult::success(msg).with_metadata(json!({
            "file_path": path.display().to_string(),
            "replacements": replacements,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;

    /// #225: editing a CRLF file must keep CRLF; only the edited line changes.
    #[tokio::test]
    async fn edit_crlf_file_preserves_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.txt");
        let original = "line one\r\nline two\r\nline three\r\n";
        std::fs::write(&path, original).unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = FileEditTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "old_string": "line two",
                    "new_string": "LINE TWO",
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "edit failed: {}", res.content);

        let after = std::fs::read_to_string(&path).unwrap();
        // Only the target changed; every other line kept its CRLF.
        assert_eq!(after, "line one\r\nLINE TWO\r\nline three\r\n");
        // No line ending was flipped to a bare LF.
        assert_eq!(after.matches('\n').count(), after.matches("\r\n").count());
    }

    /// #225: an LF file must stay LF (no stray CR introduced).
    #[tokio::test]
    async fn edit_lf_file_stays_lf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lf.txt");
        let original = "line one\nline two\nline three\n";
        std::fs::write(&path, original).unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = FileEditTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "old_string": "line two",
                    "new_string": "LINE TWO",
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "edit failed: {}", res.content);

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "line one\nLINE TWO\nline three\n");
        assert!(!after.contains('\r'), "LF file gained a CR: {:?}", after);
    }

    /// Workspace `ws/` with `notes.txt`, a sibling `outside/secret.txt` with
    /// the same text, and a manager whose only allowance is a session rule for
    /// `Edit` on `ws/*`.
    #[cfg(unix)]
    fn edit_fixture(
        dir: &tempfile::TempDir,
    ) -> (
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        claurst_core::permissions::PermissionManager,
    ) {
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ws = root.join("ws");
        let outside = root.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let notes = ws.join("notes.txt");
        let secret = outside.join("secret.txt");
        std::fs::write(&notes, "token = old\n").unwrap();
        std::fs::write(&secret, "token = old\n").unwrap();
        let mut manager = claurst_core::permissions::PermissionManager::new(
            claurst_core::config::PermissionMode::Default,
            &claurst_core::config::Settings::default(),
        );
        manager.add_session_allow_path("Edit", &format!("{}/*", ws.display()));
        (ws, notes, secret, manager)
    }

    /// #415: a file swapped for a symlink to a host file right after the
    /// permission check must not have the edit land on that host file.
    #[cfg(unix)]
    #[tokio::test]
    async fn edit_refuses_symlink_swapped_in_after_check() {
        let dir = tempfile::tempdir().unwrap();
        let (ws, notes, secret, manager) = edit_fixture(&dir);
        let handler = crate::test_support::SwapAfterCheckHandler::new(
            manager,
            Some((notes.clone(), secret.clone())),
        );
        let ctx = crate::test_support::context_with_handler(ws, std::sync::Arc::new(handler));

        let res = FileEditTool
            .execute(
                json!({
                    "file_path": notes.to_string_lossy(),
                    "old_string": "old",
                    "new_string": "pwned",
                }),
                &ctx,
            )
            .await;
        assert!(res.is_error, "swapped edit was allowed: {}", res.content);
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "token = old\n");
    }

    /// The same rule still allows a normal in-workspace edit, written back
    /// atomically through the directory of the opened file.
    #[cfg(unix)]
    #[tokio::test]
    async fn edit_allowed_by_rule_still_applies() {
        let dir = tempfile::tempdir().unwrap();
        let (ws, notes, _secret, manager) = edit_fixture(&dir);
        let handler = crate::test_support::SwapAfterCheckHandler::new(manager, None);
        let ctx = crate::test_support::context_with_handler(ws.clone(), std::sync::Arc::new(handler));

        let res = FileEditTool
            .execute(
                json!({
                    "file_path": notes.to_string_lossy(),
                    "old_string": "old",
                    "new_string": "new",
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "edit failed: {}", res.content);
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "token = new\n");
        let leftovers = std::fs::read_dir(&ws)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".claurst-tmp-"))
            .count();
        assert_eq!(leftovers, 0, "temp file left behind");
    }

    /// #415 review: an edit a human approved for a lexical path must not land
    /// on the outside file a final-component symlink points to. The second,
    /// real-path prompt is denied here, so the edit must fail.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn edit_user_approval_does_not_follow_symlink_outside() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ws = root.join("ws");
        let outside = root.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "token = old\n").unwrap();
        let notes = ws.join("notes.txt");
        std::os::unix::fs::symlink(&secret, &notes).unwrap();

        let manager = claurst_core::permissions::PermissionManager::new(
            claurst_core::config::PermissionMode::Default,
            &claurst_core::config::Settings::default(),
        );
        let handler = claurst_core::permissions::ManagedInteractivePermissionHandler::new(
            std::sync::Arc::new(std::sync::Mutex::new(manager)),
        );
        let mut ctx = crate::test_support::context_with_handler(ws, std::sync::Arc::new(handler));
        let store = std::sync::Arc::new(parking_lot::Mutex::new(crate::PendingPermissionStore::default()));
        ctx.non_interactive = false;
        ctx.pending_permissions = Some(store.clone());

        // A user who approves the prompt for the lexical path only.
        let approved = notes.display().to_string();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let responder_done = done.clone();
        let responder = std::thread::spawn(move || {
            let mut prompts = Vec::new();
            while !responder_done.load(std::sync::atomic::Ordering::Relaxed) {
                let next = store.lock().queue.pop_front();
                let Some(mut pending) = next else {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                };
                let path = pending.request.path.clone().unwrap_or_default();
                let decision = if path == approved {
                    claurst_core::permissions::PermissionDecision::Allow
                } else {
                    claurst_core::permissions::PermissionDecision::Deny
                };
                prompts.push(path);
                if let Some(tx) = pending.decision_tx.take() {
                    let _ = tx.send(decision);
                }
            }
            prompts
        });

        let res = FileEditTool
            .execute(
                json!({
                    "file_path": notes.to_string_lossy(),
                    "old_string": "old",
                    "new_string": "pwned",
                }),
                &ctx,
            )
            .await;
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let prompts = responder.join().unwrap();

        assert!(res.is_error, "edit through the symlink was applied: {}", res.content);
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "token = old\n");
        assert_eq!(prompts.len(), 2, "expected a second prompt for the real path: {:?}", prompts);
        assert_eq!(prompts[1], secret.display().to_string());
    }

    /// #415 review: a rule written with a path through a symlinked directory
    /// still allows the edit after the tool re-checks the canonical location.
    #[cfg(unix)]
    #[tokio::test]
    async fn edit_rule_through_symlinked_dir_still_applies() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&ws, &alias).unwrap();
        std::fs::write(ws.join("notes.txt"), "token = old\n").unwrap();

        let mut manager = claurst_core::permissions::PermissionManager::new(
            claurst_core::config::PermissionMode::Default,
            &claurst_core::config::Settings::default(),
        );
        manager.add_session_allow_path("Edit", &format!("{}/*", alias.display()));
        let handler = crate::test_support::SwapAfterCheckHandler::new(manager, None);
        let ctx = crate::test_support::context_with_handler(ws.clone(), std::sync::Arc::new(handler));

        let res = FileEditTool
            .execute(
                json!({
                    "file_path": alias.join("notes.txt").to_string_lossy(),
                    "old_string": "old",
                    "new_string": "new",
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "edit failed: {}", res.content);
        assert_eq!(std::fs::read_to_string(ws.join("notes.txt")).unwrap(), "token = new\n");
        assert!(std::fs::symlink_metadata(&alias).unwrap().file_type().is_symlink());
    }

    /// #415 review: re-pointing the aliased directory after the rule exists
    /// must not let the rule cover the new target.
    #[cfg(unix)]
    #[tokio::test]
    async fn edit_rule_alias_repointed_after_creation_asks() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ws = root.join("ws");
        let outside = root.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&ws, &alias).unwrap();
        std::fs::write(outside.join("notes.txt"), "token = old\n").unwrap();

        let mut manager = claurst_core::permissions::PermissionManager::new(
            claurst_core::config::PermissionMode::Default,
            &claurst_core::config::Settings::default(),
        );
        manager.add_session_allow_path("Edit", &format!("{}/*", alias.display()));
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&outside, &alias).unwrap();

        let handler = crate::test_support::SwapAfterCheckHandler::new(manager, None);
        let ctx = crate::test_support::context_with_handler(ws, std::sync::Arc::new(handler));
        let res = FileEditTool
            .execute(
                json!({
                    "file_path": alias.join("notes.txt").to_string_lossy(),
                    "old_string": "old",
                    "new_string": "pwned",
                }),
                &ctx,
            )
            .await;
        assert!(res.is_error, "re-pointed alias edit was allowed: {}", res.content);
        assert_eq!(
            std::fs::read_to_string(outside.join("notes.txt")).unwrap(),
            "token = old\n"
        );
    }
}
