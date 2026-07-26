// BatchEdit tool: apply multiple file edits atomically.
//
// All edits are validated before any change is written.  If any pre-check
// fails the tool returns an error and leaves every file untouched.  If a write
// fails after some files have already been written, the tool attempts to
// restore those files from in-memory backups.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use tracing::debug;

pub struct BatchEditTool;

#[derive(Debug, Deserialize)]
struct SingleEdit {
    file_path: String,
    old_string: String,
    new_string: String,
}

#[derive(Debug, Deserialize)]
struct BatchEditInput {
    edits: Vec<SingleEdit>,
    #[serde(default)]
    description: Option<String>,
}

#[async_trait]
impl Tool for BatchEditTool {
    // Gates itself: calls `ctx.check_permission` in `execute()` (#210).
    fn self_gates(&self) -> bool { true }

    fn name(&self) -> &str {
        claurst_core::constants::TOOL_NAME_BATCH_EDIT
    }

    fn description(&self) -> &str {
        "Apply multiple file edits atomically. All edits are validated before any \
         file is modified. If any edit would fail (old_string not found or not \
         unique) the entire batch is rejected with no changes made. If a write \
         fails mid-batch, already-written files are rolled back."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "description": "List of edits to apply atomically",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file_path": {
                                "type": "string",
                                "description": "Absolute path to the file to modify"
                            },
                            "old_string": {
                                "type": "string",
                                "description": "Text to replace (must occur exactly once in the file)"
                            },
                            "new_string": {
                                "type": "string",
                                "description": "Replacement text"
                            }
                        },
                        "required": ["file_path", "old_string", "new_string"]
                    }
                },
                "description": {
                    "type": "string",
                    "description": "Optional human-readable description of what this batch edit does"
                }
            },
            "required": ["edits"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: BatchEditInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        if params.edits.is_empty() {
            return ToolResult::error("edits array must not be empty".to_string());
        }

        // Permission check (one check covers the whole batch).
        let description = params.description.as_deref().unwrap_or("batch file edits");
        if let Err(e) =
            ctx.check_permission(self.name(), &format!("BatchEdit: {}", description), false)
        {
            return ToolResult::error(e.to_string());
        }

        // ----------------------------------------------------------------
        // Phase 1: read all files and validate every edit before writing
        // ----------------------------------------------------------------

        // (resolved_path_string, original_content, new_content)
        let mut prepared: Vec<(String, String, String)> = Vec::with_capacity(params.edits.len());
        let mut pre_check_errors: Vec<String> = Vec::new();
        let mut path_content: HashMap<String, String> = HashMap::new();

        for (i, edit) in params.edits.iter().enumerate() {
            let path = ctx.resolve_path(&edit.file_path);
            debug!(path = %path.display(), index = i, "BatchEdit pre-check");

            if edit.old_string.is_empty() {
                pre_check_errors.push(format!("Edit {}: old_string must not be empty", i));
                continue;
            }

            let original = if let Some(content) = path_content.get(&path.display().to_string()) {
                // use cached edited content if available
                content.clone()
            } else {
                match tokio::fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(e) => {
                        pre_check_errors.push(format!(
                            "Edit {}: cannot read {}: {}",
                            i,
                            path.display(),
                            e
                        ));
                        continue;
                    }
                }
            };

            // Detect the current content's dominant line ending BEFORE editing
            // so it survives the write (#225).  Match on an LF-normalized view
            // but splice the replacement into the original bytes so untouched
            // lines keep their exact endings.  `original` here is either the raw
            // on-disk content or a previous edit's output (both real EOLs).
            let eol = crate::line_endings::LineEnding::detect(&original);
            let normalized = original.replace("\r\n", "\n");
            let old_string = edit.old_string.replace("\r\n", "\n");
            let new_string = edit.new_string.replace("\r\n", "\n");

            let count = normalized.matches(&old_string).count();
            if count == 0 {
                pre_check_errors.push(format!(
                    "Edit {}: old_string not found in {}",
                    i,
                    path.display()
                ));
                continue;
            }
            if count > 1 {
                pre_check_errors.push(format!(
                    "Edit {}: old_string appears {} times in {} (must be unique)",
                    i,
                    count,
                    path.display()
                ));
                continue;
            }

            let (new_content, _replacements) = crate::line_endings::replace_preserving_eol(
                &original,
                &old_string,
                &new_string,
                eol,
                false,
            );
            prepared.push((path.display().to_string(), original, new_content.clone()));
            // update path_content so future edits see the new content
            path_content.insert(path.display().to_string(), new_content);
        }

        if !pre_check_errors.is_empty() {
            return ToolResult::error(format!(
                "BatchEdit aborted — {} validation error(s):\n{}",
                pre_check_errors.len(),
                pre_check_errors.join("\n")
            ));
        }

        let edit_count = prepared.len();
        let unique_files: std::collections::HashSet<&str> =
            prepared.iter().map(|(p, _, _)| p.as_str()).collect();
        let file_count = unique_files.len();

        // ----------------------------------------------------------------
        // Phase 2: write all files; roll back on any failure
        // ----------------------------------------------------------------

        // merge file writes into a single transaction
        let mut by_file_writing: HashMap<String, (String, String)> = HashMap::new();
        for (path_str, original, new_content) in prepared.into_iter() {
            by_file_writing
                .entry(path_str.clone())
                .and_modify(|v| v.1 = new_content.clone()) // only update new_content
                .or_insert((original, new_content));
        }
        let unique_writings = by_file_writing
            .into_iter()
            .map(|(path_str, (original, new_content))| (path_str, original, new_content))
            .collect::<Vec<_>>();

        for (i, (path_str, original, new_content)) in unique_writings.iter().enumerate() {
            let path = std::path::Path::new(path_str);
            match crate::write_atomic(path, new_content.as_bytes()).await {
                Ok(()) => {
                    ctx.record_file_change(
                        path.to_path_buf(),
                        original.as_bytes(),
                        new_content.as_bytes(),
                        self.name(),
                    );
                }
                Err(e) => {
                    // Attempt rollback of already-written files.
                    let mut rollback_errors: Vec<String> = Vec::new();

                    // rollback in reverse order to preserve original file state
                    for (rb_path, rb_original, rb_new_content) in unique_writings[0..i].iter().rev()
                    {
                        if let Err(re) =
                            crate::write_atomic(std::path::Path::new(rb_path), rb_original.as_bytes())
                                .await
                        {
                            rollback_errors.push(format!("  rollback {}: {}", rb_path, re));
                        } else {
                            let rb_path = std::path::Path::new(rb_path);
                            ctx.record_file_change(
                                rb_path.to_path_buf(),
                                rb_new_content.as_bytes(),
                                rb_original.as_bytes(),
                                self.name(),
                            );
                        }
                    }

                    let mut msg = format!(
                        "BatchEdit failed while writing {} ({}). Rolled back {} file(s).",
                        path_str, e, i
                    );
                    if !rollback_errors.is_empty() {
                        msg.push_str(&format!(
                            "\nRollback errors:\n{}",
                            rollback_errors.join("\n")
                        ));
                    }
                    return ToolResult::error(msg);
                }
            }
        }

        // ----------------------------------------------------------------
        // Build success response
        // ----------------------------------------------------------------

        let summary = format!(
            "BatchEdit applied {} edit{} across {} file{}.",
            edit_count,
            if edit_count != 1 { "s" } else { "" },
            file_count,
            if file_count != 1 { "s" } else { "" },
        );

        ToolResult::success(summary).with_metadata(json!({
            "edits_applied": edit_count,
            "files_modified": file_count,
            "files": unique_writings.iter().map(|(p, _, _)| p).collect::<Vec<_>>(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;

    /// #225: a multi-edit batch on a CRLF file keeps CRLF throughout; only the
    /// edited lines change.
    #[tokio::test]
    async fn batch_edit_crlf_file_preserves_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.txt");
        let original = "alpha\r\nbeta\r\ngamma\r\n";
        std::fs::write(&path, original).unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = BatchEditTool
            .execute(
                json!({
                    "edits": [
                        { "file_path": path.to_string_lossy(), "old_string": "alpha", "new_string": "ALPHA" },
                        { "file_path": path.to_string_lossy(), "old_string": "gamma", "new_string": "GAMMA" }
                    ]
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "batch edit failed: {}", res.content);

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "ALPHA\r\nbeta\r\nGAMMA\r\n");
        assert_eq!(after.matches('\n').count(), after.matches("\r\n").count());
    }

    /// #226: BatchEdit writes (and its rollback path) go through `write_atomic`.
    /// A successful multi-file batch must land the right content in every file
    /// and leave no `.claurst-tmp-*` scratch file behind. Because each file is
    /// swapped in atomically via rename, a mid-write crash can never leave one
    /// of these files partially written — so the rollback only ever has to
    /// restore fully-written files, never repair a torn one.
    #[tokio::test]
    async fn batch_edit_writes_atomically_no_tmp_left() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "one\n").unwrap();
        std::fs::write(&b, "two\n").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = BatchEditTool
            .execute(
                json!({
                    "edits": [
                        { "file_path": a.to_string_lossy(), "old_string": "one", "new_string": "ONE" },
                        { "file_path": b.to_string_lossy(), "old_string": "two", "new_string": "TWO" }
                    ]
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "batch edit failed: {}", res.content);

        assert_eq!(std::fs::read_to_string(&a).unwrap(), "ONE\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "TWO\n");
        let tmp_left = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".claurst-tmp-"));
        assert!(!tmp_left, "atomic write must not leave a temp file behind");
    }
}
