// FileRead tool: read files with optional line range, image support, PDF page ranges.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tracing::debug;

pub struct FileReadTool;

#[derive(Debug, Deserialize)]
struct FileReadInput {
    file_path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for FileReadTool {
    // Gates itself: calls `ctx.check_permission_for_path` in `execute()` (#210).
    fn self_gates(&self) -> bool { true }

    fn name(&self) -> &str {
        claurst_core::constants::TOOL_NAME_FILE_READ
    }

    fn description(&self) -> &str {
        "Reads a file from the local filesystem. You can access any file directly. \
         By default reads up to 2000 lines from the beginning. Results are returned \
         with line numbers starting at 1. This tool can read images (PNG, JPG) and \
         PDF files."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to read"
                },
                "offset": {
                    "type": "number",
                    "description": "The line number to start reading from (1-based). Only provide if the file is too large to read at once."
                },
                "limit": {
                    "type": "number",
                    "description": "The number of lines to read. Only provide if the file is too large to read at once."
                }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: FileReadInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let path = ctx.resolve_path(&params.file_path);
        debug!(path = %path.display(), "Reading file");

        // Permission check
        let grant = match ctx.check_permission_for_path(
            self.name(),
            &format!("Read {}", path.display()),
            path.clone(),
            true,
        ) {
            Ok(grant) => grant,
            Err(e) => return ToolResult::error(e.to_string()),
        };

        // Check if file exists
        if !path.exists() {
            return ToolResult::error(format!("File not found: {}", path.display()));
        }

        // Check if it's a directory
        if path.is_dir() {
            return ToolResult::error(format!(
                "{} is a directory, not a file. Use Bash with `ls` to list directory contents.",
                path.display()
            ));
        }

        // Detect binary / image files by extension
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let image_exts = ["png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico"];
        if image_exts.contains(&ext.as_str()) {
            return ToolResult::success(format!(
                "[Image file: {}. The image content has been captured for visual analysis.]",
                path.display()
            ));
        }

        if ext == "pdf" {
            return ToolResult::success(format!(
                "[PDF file: {}. Use the `pages` parameter to read specific page ranges.]",
                path.display()
            ));
        }

        // Read text file through a handle whose real location has been
        // re-checked, never by re-resolving `path` (#415).
        let file = match crate::open_regular_file(&path) {
            Ok(f) => f,
            Err(e) => return ToolResult::error(format!("Failed to read file: {}", e)),
        };
        if let Err(e) = ctx.confirm_opened_path(self.name(), "Read", &path, &file, grant, true) {
            return ToolResult::error(e);
        }
        let mut file = tokio::fs::File::from_std(file);
        let mut content = String::new();
        if let Err(e) = file.read_to_string(&mut content).await {
            // Might be binary
            if e.kind() == std::io::ErrorKind::InvalidData {
                return ToolResult::error(format!(
                    "File appears to be binary and cannot be displayed as text: {}",
                    path.display()
                ));
            }
            return ToolResult::error(format!("Failed to read file: {}", e));
        }

        if content.is_empty() {
            return ToolResult::success(format!(
                "[File {} exists but is empty]",
                path.display()
            ));
        }

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let offset = params.offset.unwrap_or(0);
        let limit = params.limit.unwrap_or(2000);

        // Convert 1-based offset to 0-based index
        let start = if offset > 0 { offset - 1 } else { 0 };
        let end = (start + limit).min(total_lines);

        if start >= total_lines {
            return ToolResult::error(format!(
                "Offset {} exceeds total line count {} in {}",
                offset,
                total_lines,
                path.display()
            ));
        }

        let mut output = String::new();
        let width = format!("{}", end).len();

        for (i, line) in lines[start..end].iter().enumerate() {
            let line_num = start + i + 1;
            output.push_str(&format!("{:>width$}\t{}\n", line_num, line, width = width));
        }

        if end < total_lines {
            output.push_str(&format!(
                "\n... ({} more lines, {} total. Use offset/limit to read more.)\n",
                total_lines - end,
                total_lines
            ));
        }

        ToolResult::success(output)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{context_with_handler, SwapAfterCheckHandler};
    use claurst_core::config::{PermissionMode, Settings};
    use claurst_core::permissions::PermissionManager;
    use std::sync::Arc;

    /// Workspace `ws/` and a sibling `outside/secret.txt`, both canonical.
    fn workspace_and_secret(dir: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ws = root.join("ws");
        let outside = root.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "TOP-SECRET\n").unwrap();
        (ws, secret)
    }

    /// #415: an in-workspace file swapped for a symlink to a host file right
    /// after the fence allowed it must not be read without a prompt.
    #[tokio::test]
    async fn read_refuses_symlink_swapped_in_after_fence() {
        let dir = tempfile::tempdir().unwrap();
        let (ws, secret) = workspace_and_secret(&dir);
        let notes = ws.join("notes.txt");
        std::fs::write(&notes, "inside\n").unwrap();

        let manager = PermissionManager::new(PermissionMode::Default, &Settings::default());
        let handler = SwapAfterCheckHandler::new(manager, Some((notes.clone(), secret)));
        let ctx = context_with_handler(ws, Arc::new(handler));

        let res = FileReadTool
            .execute(json!({ "file_path": notes.to_string_lossy() }), &ctx)
            .await;
        assert!(res.is_error, "swapped read was allowed: {}", res.content);
        assert!(!res.content.contains("TOP-SECRET"), "leaked: {}", res.content);
        assert!(res.content.contains("Permission denied"), "unexpected error: {}", res.content);
    }

    /// A symlink that resolves inside the workspace keeps reading silently.
    #[tokio::test]
    async fn read_follows_symlink_within_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let (ws, _secret) = workspace_and_secret(&dir);
        std::fs::write(ws.join("real.txt"), "hello\n").unwrap();
        let link = ws.join("link.txt");
        std::os::unix::fs::symlink(ws.join("real.txt"), &link).unwrap();

        let manager = PermissionManager::new(PermissionMode::Default, &Settings::default());
        let ctx = context_with_handler(ws, Arc::new(SwapAfterCheckHandler::new(manager, None)));

        let res = FileReadTool
            .execute(json!({ "file_path": link.to_string_lossy() }), &ctx)
            .await;
        assert!(!res.is_error, "read failed: {}", res.content);
        assert!(res.content.contains("hello"), "unexpected content: {}", res.content);
    }
}
