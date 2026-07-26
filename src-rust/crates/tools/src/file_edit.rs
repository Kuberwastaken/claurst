// FileEdit tool: exact string replacement with old/new strings (like sed but
// deterministic).  Mirrors the TypeScript Edit tool behaviour.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
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
        if let Err(e) = ctx.check_permission_for_path(
            self.name(),
            &format!("Edit {}", path.display()),
            path.clone(),
            false,
        ) {
            return ToolResult::error(e.to_string());
        }

        // Read current content
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                return ToolResult::error(format!("Failed to read file {}: {}", path.display(), e));
            }
        };

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

        // Write back
        if let Err(e) = crate::write_atomic(&path, new_content.as_bytes()).await {
            return ToolResult::error(format!("Failed to write file {}: {}", path.display(), e));
        }

        ctx.record_file_change(
            path.clone(),
            content.as_bytes(),
            new_content.as_bytes(),
            self.name(),
        );

        // Run any configured formatter for this file type.
        crate::try_format_file(&path.to_string_lossy(), ctx).await;

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
}
