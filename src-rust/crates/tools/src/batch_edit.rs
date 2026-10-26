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

struct PreparedEdit {
    path: std::path::PathBuf,
    real: std::path::PathBuf,
    file: std::fs::File,
    before: String,
    after: String,
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

        let description = params.description.as_deref().unwrap_or("batch file edits");
        let paths: Vec<std::path::PathBuf> = params
            .edits
            .iter()
            .map(|edit| ctx.resolve_path(&edit.file_path))
            .collect();
        let mut grants = Vec::with_capacity(paths.len());
        for path in &paths {
            match ctx.check_permission_for_path(
                self.name(),
                &format!("BatchEdit: {} ({})", description, path.display()),
                path.clone(),
                false,
            ) {
                Ok(grant) => grants.push(grant),
                Err(e) => return ToolResult::error(e.to_string()),
            }
        }
        let _locks = crate::lock_files(paths.iter().map(|p| p.as_path())).await;
        let mut prepared: Vec<PreparedEdit> = Vec::new();
        let mut by_path: HashMap<std::path::PathBuf, usize> = HashMap::new();
        let mut errors = Vec::new();
        for (i, ((edit, path), grant)) in params.edits.iter().zip(&paths).zip(grants).enumerate() {
            debug!(path = %path.display(), index = i, "BatchEdit pre-check");
            let file = match crate::open_regular_file(path) {
                Ok(file) => file,
                Err(e) => {
                    errors.push(format!("Edit {}: cannot read {}: {}", i, path.display(), e));
                    continue;
                }
            };
            let real = match ctx.confirm_opened_path(
                self.name(),
                "BatchEdit",
                path,
                &file,
                grant,
                false,
            ) {
                Ok(real) => real,
                Err(e) => {
                    errors.push(format!("Edit {}: {}", i, e));
                    continue;
                }
            };
            let index = match by_path.get(&real) {
                Some(index) => *index,
                None => {
                    let mut before = String::new();
                    if let Err(e) = std::io::Read::read_to_string(&mut &file, &mut before) {
                        errors.push(format!("Edit {}: cannot read {}: {}", i, path.display(), e));
                        continue;
                    }
                    let index = prepared.len();
                    by_path.insert(real.clone(), index);
                    prepared.push(PreparedEdit {
                        path: path.clone(),
                        real,
                        file,
                        after: before.clone(),
                        before,
                    });
                    index
                }
            };
            match crate::file_edit::resolve_edit(
                &prepared[index].after,
                &edit.old_string,
                &edit.new_string,
                false,
            ) {
                Ok((after, _, _)) => prepared[index].after = after,
                Err(e) => errors.push(format!("Edit {} in {}: {}", i, path.display(), e)),
            }
        }
        if !errors.is_empty() {
            return ToolResult::error(format!(
                "BatchEdit aborted — {} validation error(s):\n{}",
                errors.len(),
                errors.join("\n")
            ));
        }
        // Writes and rollback both retain the inode they are allowed to replace.
        // Each successful atomic rename returns its new handle for rollback.
        let mut written: Vec<(usize, std::fs::File)> = Vec::new();
        for (index, edit) in prepared.iter().enumerate() {
            match crate::write_atomic_bound(&edit.file, &edit.real, edit.after.as_bytes()).await {
                Ok(installed) => {
                    written.push((index, installed));
                    ctx.record_file_change(
                        edit.path.clone(),
                        edit.before.as_bytes(),
                        edit.after.as_bytes(),
                        self.name(),
                    );
                }
                Err(error) => {
                    let mut restored = 0;
                    let mut rollback_errors = Vec::new();
                    for (index, installed) in written.iter().rev() {
                        let previous = &prepared[*index];
                        match crate::write_atomic_bound(
                            installed,
                            &previous.real,
                            previous.before.as_bytes(),
                        )
                        .await
                        {
                            Ok(_) => {
                                restored += 1;
                                ctx.record_file_change(
                                    previous.path.clone(),
                                    previous.after.as_bytes(),
                                    previous.before.as_bytes(),
                                    self.name(),
                                );
                            }
                            Err(error) => rollback_errors.push(format!(
                                "rollback {}: {}",
                                previous.path.display(),
                                error
                            )),
                        }
                    }
                    let mut message = format!(
                        "BatchEdit failed while writing {} ({}). Rolled back {} file(s).",
                        edit.path.display(),
                        error,
                        restored
                    );
                    if !rollback_errors.is_empty() {
                        message.push_str(&format!(
                            "\nRollback errors:\n{}",
                            rollback_errors.join("\n")
                        ));
                    }
                    return ToolResult::error(message);
                }
            }
        }
        let edit_count = params.edits.len();
        let file_count = prepared.len();
        ToolResult::success(format!(
            "BatchEdit applied {} edit{} across {} file{}.",
            edit_count,
            if edit_count == 1 { "" } else { "s" },
            file_count,
            if file_count == 1 { "" } else { "s" },
        ))
        .with_metadata(json!({
            "edits_applied": edit_count,
            "files_modified": file_count,
            "files": prepared.iter().map(|edit| &edit.path).collect::<Vec<_>>(),
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
    #[tokio::test]
    async fn batch_and_single_edit_share_bom_and_whitespace_matching() {
        let dir = tempfile::tempdir().unwrap();
        let single = dir.path().join("single.txt");
        let batch = dir.path().join("batch.txt");
        let original = "\u{feff}first  \r\nsecond\r\n";
        std::fs::write(&single, original).unwrap();
        std::fs::write(&batch, original).unwrap();
        let ctx = allow_all_context(dir.path().to_path_buf());
        let one = crate::FileEditTool
            .execute(
                json!({"file_path":single,"old_string":"\u{feff}first   ","new_string":"FIRST"}),
                &ctx,
            )
            .await;
        let many = BatchEditTool.execute(json!({"edits":[{"file_path":batch,"old_string":"\u{feff}first   ","new_string":"FIRST"}]}), &ctx).await;
        assert!(!one.is_error, "{}", one.content);
        assert!(!many.is_error, "{}", many.content);
        assert_eq!(
            std::fs::read(&single).unwrap(),
            std::fs::read(&batch).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(&batch).unwrap(),
            "\u{feff}FIRST\r\nsecond\r\n"
        );
    }

    #[tokio::test]
    async fn ambiguous_batch_edit_leaves_every_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        std::fs::write(&first, "old\n").unwrap();
        std::fs::write(&second, "value \nvalue\t\n").unwrap();
        let ctx = allow_all_context(dir.path().to_path_buf());
        let result = BatchEditTool
            .execute(
                json!({"edits":[
                    {"file_path":first,"old_string":"old","new_string":"new"},
                    {"file_path":second,"old_string":"value  ","new_string":"new"}
                ]}),
                &ctx,
            )
            .await;
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("matches 2 places"),
            "{}",
            result.content
        );
        assert_eq!(std::fs::read_to_string(first).unwrap(), "old\n");
        assert_eq!(
            std::fs::read_to_string(second).unwrap(),
            "value \nvalue\t\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn aliases_in_a_batch_share_staged_content_and_one_write() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        let alias = dir.path().join("alias.txt");
        std::fs::write(&file, "first\nsecond\n").unwrap();
        std::os::unix::fs::symlink(&file, &alias).unwrap();
        let ctx = allow_all_context(dir.path().to_path_buf());
        let result = BatchEditTool
            .execute(
                json!({"edits":[
                    {"file_path":file,"old_string":"first","new_string":"FIRST"},
                    {"file_path":alias,"old_string":"FIRST\nsecond","new_string":"FINAL"}
                ]}),
                &ctx,
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.metadata.unwrap()["files_modified"], 1);
        assert_eq!(std::fs::read_to_string(file).unwrap(), "FINAL\n");
        assert!(std::fs::symlink_metadata(alias)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
