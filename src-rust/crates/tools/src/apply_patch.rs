// ApplyPatch tool: apply a unified diff patch to files.
//
// Parses standard unified diff format (as produced by `git diff` or `diff -u`).
// Supports dry_run mode which validates the patch without writing any files.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

pub struct ApplyPatchTool;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ApplyPatchInput {
    patch: String,
    #[serde(default)]
    dry_run: bool,
}

// ---------------------------------------------------------------------------
// Internal diff representation
// ---------------------------------------------------------------------------

/// A single `@@` hunk within a file diff.
#[derive(Debug)]
struct Hunk {
    /// Index in the *original* file where the hunk's old lines start (0-based).
    /// For a pure insertion (`-l,0`) this is the index after line `l`.
    orig_start: usize,
    /// Lines in this hunk: `' '` = context, `'-'` = remove, `'+'` = add.
    lines: Vec<(char, String)>,
}

/// All hunks for a single file.
#[derive(Debug)]
struct FilePatch {
    /// Path from the `--- ` header (`a/` stripped); `None` for `/dev/null`.
    old_path: Option<String>,
    /// Path from the `+++ ` header (`b/` stripped); `None` for `/dev/null`.
    new_path: Option<String>,
    hunks: Vec<Hunk>,
}

/// What a `FilePatch` does to its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileOp {
    Create,
    Modify,
    Delete,
}

impl FilePatch {
    /// The file the patch acts on and what it does to it.
    fn target(&self) -> Result<(&str, FileOp), String> {
        match (&self.old_path, &self.new_path) {
            (None, Some(new)) => Ok((new, FileOp::Create)),
            (Some(old), None) => Ok((old, FileOp::Delete)),
            (Some(_), Some(new)) => Ok((new, FileOp::Modify)),
            (None, None) => Err("both the --- and +++ headers are /dev/null".to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Unified diff parser
// ---------------------------------------------------------------------------

/// Path from a `--- ` / `+++ ` header: drops a tab-separated timestamp and
/// one `a/` / `b/` prefix; `/dev/null` becomes `None`.
fn parse_header_path(raw: &str, prefix: &str) -> Option<String> {
    let path = raw.split('\t').next().unwrap_or(raw).trim();
    if path == "/dev/null" {
        return None;
    }
    Some(path.strip_prefix(prefix).unwrap_or(path).to_string())
}

/// Parse a unified diff string into a list of `FilePatch` objects.
///
/// A hunk body is delimited by the line counts in its `@@` header, as in
/// `git apply`: a removed `-- x` line (`--- x` in the diff), an added `++ x`
/// line (`+++ x`) and an empty context line whose leading space was stripped
/// all stay in the hunk instead of being read as a file header or skipped.
fn parse_unified_diff(patch: &str) -> Result<Vec<FilePatch>, String> {
    let mut file_patches: Vec<FilePatch> = Vec::new();
    let mut current_file: Option<FilePatch> = None;
    let mut current_hunk: Option<Hunk> = None;
    let mut pending_old: Option<Option<String>> = None;
    // Old / new lines the current hunk's header still expects.
    let mut remaining_old = 0usize;
    let mut remaining_new = 0usize;

    for (idx, line) in patch.lines().enumerate() {
        let line_no = idx + 1;
        if let Some(hunk) = current_hunk.as_mut().filter(|_| remaining_old + remaining_new > 0) {
            if line.starts_with('\\') {
                // "\ No newline at end of file"
                continue;
            }
            let (kind, text) = match line.chars().next() {
                Some(c @ ('+' | '-' | ' ')) => (c, &line[1..]),
                // A context line for an empty source line, its space stripped.
                None => (' ', ""),
                Some(_) => {
                    return Err(format!(
                        "line {}: '{}' is inside a hunk that still expects {} old and {} new \
                         line(s); every hunk line starts with ' ', '+' or '-', and the @@ \
                         header counts must match the hunk",
                        line_no, line, remaining_old, remaining_new
                    ));
                }
            };
            let (old, new) = match kind {
                '+' => (0, 1),
                '-' => (1, 0),
                _ => (1, 1),
            };
            if old > remaining_old || new > remaining_new {
                return Err(format!(
                    "line {}: the hunk has more lines than its @@ header declares",
                    line_no
                ));
            }
            remaining_old -= old;
            remaining_new -= new;
            hunk.lines.push((kind, text.to_string()));
            continue;
        }

        if let Some(raw) = line.strip_prefix("--- ") {
            // Start of a new file section; finalise previous hunk/file.
            if let Some(h) = current_hunk.take() {
                if let Some(ref mut f) = current_file {
                    f.hunks.push(h);
                }
            }
            if let Some(f) = current_file.take() {
                file_patches.push(f);
            }
            pending_old = Some(parse_header_path(raw, "a/"));
        } else if let Some(raw) = line.strip_prefix("+++ ") {
            let Some(old_path) = pending_old.take() else {
                return Err(format!("line {}: '+++' header without a '---' header before it", line_no));
            };
            current_file = Some(FilePatch {
                old_path,
                new_path: parse_header_path(raw, "b/"),
                hunks: Vec::new(),
            });
        } else if line.starts_with("@@ ") {
            // Finalise the previous hunk.
            if let Some(h) = current_hunk.take() {
                if let Some(ref mut f) = current_file {
                    f.hunks.push(h);
                }
            }
            if current_file.is_none() {
                return Err(format!("line {}: hunk header before any ---/+++ file header", line_no));
            }
            let header = parse_hunk_header(line)?;
            remaining_old = header.old_count;
            remaining_new = header.new_count;
            current_hunk = Some(Hunk {
                orig_start: header.orig_start,
                lines: Vec::new(),
            });
        } else if current_hunk.is_some()
            && (line.starts_with('+') || line.starts_with('-') || line.starts_with(' '))
        {
            // A change line after the hunk's declared lines ran out would
            // otherwise be dropped silently.
            return Err(format!(
                "line {}: '{}' is outside any hunk; the @@ header before it declares fewer \
                 lines than the hunk has",
                line_no, line
            ));
        }
        // Anything else (`diff --git`, `index`, mode lines, blank lines
        // between files) is ignored.
    }

    if remaining_old + remaining_new > 0 {
        return Err(format!(
            "the patch ended inside a hunk that still expects {} old and {} new line(s)",
            remaining_old, remaining_new
        ));
    }

    // Flush remaining hunk / file.
    if let Some(h) = current_hunk.take() {
        if let Some(ref mut f) = current_file {
            f.hunks.push(h);
        }
    }
    if let Some(f) = current_file.take() {
        file_patches.push(f);
    }

    Ok(file_patches)
}

/// A parsed `@@ -l,s +l,s @@` header.
#[derive(Debug, PartialEq, Eq)]
struct HunkHeader {
    /// Index of the first old line (0-based); for `-l,0` the index after line `l`.
    orig_start: usize,
    old_count: usize,
    new_count: usize,
}

/// Parse a `@@ -l[,s] +l[,s] @@` header; an omitted count is 1.
fn parse_hunk_header(line: &str) -> Result<HunkHeader, String> {
    let malformed = || format!("Malformed hunk header: {}", line);
    let after_at = line.strip_prefix("@@ ").ok_or_else(malformed)?;
    let mut parts = after_at.split_whitespace();
    let old = parts.next().and_then(|p| p.strip_prefix('-')).ok_or_else(malformed)?;
    let new = parts.next().and_then(|p| p.strip_prefix('+')).ok_or_else(malformed)?;

    let range = |spec: &str| -> Result<(usize, usize), String> {
        let (start, count) = match spec.split_once(',') {
            Some((start, count)) => (start, count.parse().map_err(|_| malformed())?),
            None => (spec, 1),
        };
        Ok((start.parse().map_err(|_| malformed())?, count))
    };
    let (old_start, old_count) = range(old)?;
    let (_, new_count) = range(new)?;

    // Header lines are 1-based. `-l,0` names the line the new lines follow,
    // so the insertion index is `l`; otherwise the hunk starts at line `l`.
    let orig_start = if old_count == 0 { old_start } else { old_start.saturating_sub(1) };
    Ok(HunkHeader { orig_start, old_count, new_count })
}

// ---------------------------------------------------------------------------
// Hunk application
// ---------------------------------------------------------------------------

/// Apply a single `Hunk` to `lines` (the split lines of the file content),
/// matching its context at the occurrence nearest `hint`.
///
/// Returns the modified line vector, or an error describing why the context
/// did not match.
fn apply_hunk(mut lines: Vec<String>, hunk: &Hunk, hint: usize) -> Result<Vec<String>, String> {
    // Collect the context / removal lines we expect to find.
    let expected: Vec<&str> = hunk
        .lines
        .iter()
        .filter(|(c, _)| *c == ' ' || *c == '-')
        .map(|(_, l)| l.as_str())
        .collect();

    let pos = find_context_position(&lines, &expected, hint).ok_or_else(|| {
        format!(
            "Context not found near line {} (looking for {} lines of context/removes)",
            hunk.orig_start + 1,
            expected.len()
        )
    })?;

    let replacement = hunk
        .lines
        .iter()
        .filter(|(c, _)| *c != '-')
        .map(|(_, content)| content.clone());
    lines.splice(pos..pos + expected.len(), replacement);
    Ok(lines)
}

/// Find where `expected` occurs in `lines`, preferring the occurrence nearest
/// `hint` (the header's start line shifted by the earlier hunks' net change).
fn find_context_position(lines: &[String], expected: &[&str], hint: usize) -> Option<usize> {
    if expected.is_empty() {
        // A pure-insertion hunk always applies at the hint position.
        return Some(hint.min(lines.len()));
    }
    let max_start = lines.len().checked_sub(expected.len())?;
    let hint = hint.min(max_start);
    let matches_at = |start: usize| {
        lines[start..start + expected.len()]
            .iter()
            .zip(expected)
            .all(|(l, e)| l.as_str() == *e)
    };
    (0..=max_start)
        .flat_map(|distance| {
            let after = Some(hint + distance).filter(|&i| i <= max_start);
            let before = hint.checked_sub(distance).filter(|_| distance > 0);
            after.into_iter().chain(before)
        })
        .find(|&start| matches_at(start))
}

// ---------------------------------------------------------------------------
// Tool implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for ApplyPatchTool {
    // Gates itself: calls `ctx.check_permission` in `execute()` (#210).
    fn self_gates(&self) -> bool { true }

    fn name(&self) -> &str {
        claurst_core::constants::TOOL_NAME_APPLY_PATCH
    }

    fn description(&self) -> &str {
        "Apply a unified diff patch to files. The patch must be in standard unified \
         diff format (as produced by `git diff` or `diff -u`). Set dry_run=true to \
         validate the patch without writing any changes."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "Unified diff patch content"
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "If true, validate the patch without applying it (default: false)"
                }
            },
            "required": ["patch"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: ApplyPatchInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        if params.patch.trim().is_empty() {
            return ToolResult::error("patch must not be empty".to_string());
        }

        // Parse the unified diff.
        let file_patches = match parse_unified_diff(&params.patch) {
            Ok(fp) => fp,
            Err(e) => return ToolResult::error(format!("Failed to parse patch: {}", e)),
        };

        if file_patches.is_empty() {
            return ToolResult::error(
                "No file diffs found in patch (expected --- / +++ headers)".to_string(),
            );
        }

        // Permission check.
        if !params.dry_run {
            if let Err(e) = ctx.check_permission(
                self.name(),
                &format!("ApplyPatch to {} file(s)", file_patches.len()),
                false,
            ) {
                return ToolResult::error(e.to_string());
            }
        }

        // ----------------------------------------------------------------
        // Process each file in the patch
        // ----------------------------------------------------------------

        // Resolve each file's target before locking.
        let mut targets: Vec<(&str, FileOp, std::path::PathBuf)> = Vec::with_capacity(file_patches.len());
        for fp in &file_patches {
            match fp.target() {
                Ok((name, op)) => targets.push((name, op, ctx.resolve_path(name))),
                Err(e) => return ToolResult::error(format!("Failed to parse patch: {}", e)),
            }
        }

        // Lock every target file from the reads through the writes (#443).
        let _locks = crate::lock_files(targets.iter().map(|(_, _, p)| p.as_path())).await;

        let mut total_added: i64 = 0;
        let mut total_removed: i64 = 0;
        let mut file_summaries: Vec<Value> = Vec::new();
        // (path, original, new content or `None` to delete) — built during
        // validation, used for writes.
        let mut to_write: Vec<(&std::path::Path, Vec<u8>, Option<String>)> = Vec::new();

        for (fp, (name, op, path)) in file_patches.iter().zip(&targets) {
            debug!(path = %path.display(), "ApplyPatch processing file");

            // Read current content (empty for a file the patch creates).
            let original_content = match op {
                FileOp::Create if path.exists() => {
                    return ToolResult::error(format!(
                        "Cannot create {}: the file already exists",
                        path.display()
                    ));
                }
                FileOp::Create => String::new(),
                FileOp::Modify | FileOp::Delete => match tokio::fs::read_to_string(path).await {
                    Ok(c) => c,
                    Err(e) => {
                        return ToolResult::error(format!("Cannot read {}: {}", path.display(), e))
                    }
                },
            };

            // Detect the file's dominant line ending BEFORE editing so we can
            // rejoin with it instead of collapsing everything to LF (#225).
            // `str::lines()` strips both `\n` and `\r\n`, so a CRLF file would
            // otherwise be silently rewritten to LF throughout.
            let eol = crate::line_endings::LineEnding::detect(&original_content);

            // Split into lines (line endings are re-applied on join below).
            let mut lines: Vec<String> = original_content
                .lines()
                .map(|l| l.to_string())
                .collect();

            let mut file_added: i64 = 0;
            let mut file_removed: i64 = 0;

            // Apply hunks in order, shifting each header's start line by the
            // net change of the hunks before it.
            for (hunk_idx, hunk) in fp.hunks.iter().enumerate() {
                let added = hunk.lines.iter().filter(|(c, _)| *c == '+').count() as i64;
                let removed = hunk.lines.iter().filter(|(c, _)| *c == '-').count() as i64;
                let hint = usize::try_from(hunk.orig_start as i64 + file_added - file_removed)
                    .unwrap_or(0);

                lines = match apply_hunk(lines, hunk, hint) {
                    Ok(l) => l,
                    Err(e) => {
                        return ToolResult::error(format!(
                            "Failed to apply hunk {} in {}: {}",
                            hunk_idx + 1,
                            name,
                            e
                        ));
                    }
                };

                file_added += added;
                file_removed += removed;
            }

            total_added += file_added;
            total_removed += file_removed;

            let new_content = if *op == FileOp::Delete {
                if !lines.is_empty() {
                    return ToolResult::error(format!(
                        "Cannot delete {}: the patch removes it (+++ /dev/null) but leaves {} \
                         line(s) that its hunks do not remove",
                        name,
                        lines.len()
                    ));
                }
                None
            } else if lines.is_empty() {
                Some(String::new())
            } else {
                // Re-join with the file's original line ending; preserve a
                // trailing newline if the original had one.
                let mut s = lines.join(eol.as_str());
                if original_content.ends_with('\n') || original_content.is_empty() {
                    s.push_str(eol.as_str());
                }
                Some(s)
            };

            file_summaries.push(json!({
                "path": name,
                "hunks": fp.hunks.len(),
                "lines_added": file_added,
                "lines_removed": file_removed,
            }));

            to_write.push((path, original_content.into_bytes(), new_content));
        }

        // ----------------------------------------------------------------
        // Dry-run: return summary without writing
        // ----------------------------------------------------------------

        if params.dry_run {
            return ToolResult::success(format!(
                "Dry run: patch would modify {} file(s) (+{} -{} lines).",
                to_write.len(),
                total_added,
                total_removed,
            ))
            .with_metadata(json!({
                "dry_run": true,
                "files": file_summaries,
                "total_lines_added": total_added,
                "total_lines_removed": total_removed,
            }));
        }

        // ----------------------------------------------------------------
        // Write all modified files
        // ----------------------------------------------------------------

        for (path, original_bytes, new_content) in &to_write {
            let written = match new_content {
                Some(content) => {
                    let parent_ready = match path.parent() {
                        Some(parent) if !parent.as_os_str().is_empty() => {
                            tokio::fs::create_dir_all(parent).await
                        }
                        _ => Ok(()),
                    };
                    match parent_ready {
                        Ok(()) => crate::write_atomic(path, content.as_bytes()).await,
                        Err(e) => Err(e),
                    }
                }
                None => tokio::fs::remove_file(path).await,
            };
            if let Err(e) = written {
                return ToolResult::error(format!(
                    "Failed to write {}: {}",
                    path.display(),
                    e
                ));
            }
            ctx.record_file_change(
                path.to_path_buf(),
                original_bytes,
                new_content.as_deref().unwrap_or_default().as_bytes(),
                self.name(),
            );
        }

        ToolResult::success(format!(
            "Applied patch to {} file(s) (+{} -{} lines).",
            to_write.len(),
            total_added,
            total_removed,
        ))
        .with_metadata(json!({
            "dry_run": false,
            "files": file_summaries,
            "total_lines_added": total_added,
            "total_lines_removed": total_removed,
        }))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hunk_header() {
        let header = |line| parse_hunk_header(line).unwrap();
        assert_eq!(header("@@ -12,5 +12,6 @@"), HunkHeader { orig_start: 11, old_count: 5, new_count: 6 });
        assert_eq!(header("@@ -1,3 +1,4 @@ fn foo()"), HunkHeader { orig_start: 0, old_count: 3, new_count: 4 });
        assert_eq!(header("@@ -0,0 +1 @@"), HunkHeader { orig_start: 0, old_count: 0, new_count: 1 });
        // `-5,0` inserts after line 5.
        assert_eq!(header("@@ -5,0 +6,2 @@"), HunkHeader { orig_start: 5, old_count: 0, new_count: 2 });
        assert!(parse_hunk_header("@@ @@").is_err());
    }

    #[test]
    fn test_apply_hunk_simple() {
        let lines: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let hunk = Hunk {
            orig_start: 1,
            lines: vec![(' ', "b".into()), ('-', "c".into()), ('+', "C".into())],
        };
        let result = apply_hunk(lines, &hunk, 1).unwrap();
        assert_eq!(result, vec!["a", "b", "C"]);
    }

    #[test]
    fn test_apply_hunk_context_mismatch() {
        let lines: Vec<String> = vec!["x".into(), "y".into()];
        let hunk = Hunk {
            orig_start: 0,
            lines: vec![('-', "z".into())],
        };
        assert!(apply_hunk(lines, &hunk, 0).is_err());
    }

    #[test]
    fn test_parse_unified_diff_basic() {
        let patch = "\
--- a/foo.txt
+++ b/foo.txt
@@ -1,2 +1,2 @@
 hello
-world
+rust
";
        let fps = parse_unified_diff(patch).unwrap();
        assert_eq!(fps.len(), 1);
        assert_eq!(fps[0].target().unwrap(), ("foo.txt", FileOp::Modify));
        assert_eq!(fps[0].hunks.len(), 1);
        let hunk = &fps[0].hunks[0];
        assert_eq!(hunk.orig_start, 0);
        assert_eq!(hunk.lines.len(), 3);
    }

    #[test]
    fn test_parse_unified_diff_two_files() {
        let patch = "\
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-old
+new
--- a/b.rs
+++ b/b.rs
@@ -1 +1 @@
-foo
+bar
";
        let fps = parse_unified_diff(patch).unwrap();
        assert_eq!(fps.len(), 2);
        assert_eq!(fps[0].target().unwrap().0, "a.rs");
        assert_eq!(fps[1].target().unwrap().0, "b.rs");
    }

    /// Apply `patch` in a temp dir holding `files`; returns the dir and result.
    async fn run_patch(files: &[(&str, &str)], patch: &str) -> (tempfile::TempDir, ToolResult) {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        let res = ApplyPatchTool.execute(json!({ "patch": patch }), &ctx).await;
        (dir, res)
    }

    /// #448: removing a line that reads `-- comment` puts `--- comment` in the
    /// diff; it used to end the file section and apply the hunk half-parsed.
    #[tokio::test]
    async fn removed_double_dash_line_stays_in_hunk() {
        let patch = "--- a/q.sql\n+++ b/q.sql\n@@ -1,3 +1,2 @@\n select 1;\n--- comment\n select 2;\n";
        let (dir, res) = run_patch(&[("q.sql", "select 1;\n-- comment\nselect 2;\n")], patch).await;
        assert!(!res.is_error, "apply patch failed: {}", res.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("q.sql")).unwrap(), "select 1;\nselect 2;\n");
    }

    /// #448: adding a line that reads `++ x` puts `+++ x` in the diff; it used
    /// to be taken for a new file header.
    #[tokio::test]
    async fn added_double_plus_line_stays_in_hunk() {
        let patch = "--- a/n.txt\n+++ b/n.txt\n@@ -1,2 +1,3 @@\n a\n+++ x\n b\n";
        let (dir, res) = run_patch(&[("n.txt", "a\nb\n")], patch).await;
        assert!(!res.is_error, "apply patch failed: {}", res.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("n.txt")).unwrap(), "a\n++ x\nb\n");
    }

    /// #448: an empty context line whose leading space was stripped (editors
    /// and models trim trailing whitespace) used to be skipped, so the
    /// context never matched or matched the wrong place.
    #[tokio::test]
    async fn blank_context_line_without_space_is_context() {
        let patch = "--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,3 @@\n fn a() {}\n\n-fn b() {}\n+fn c() {}\n";
        let (dir, res) = run_patch(&[("f.rs", "fn a() {}\n\nfn b() {}\n")], patch).await;
        assert!(!res.is_error, "apply patch failed: {}", res.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("f.rs")).unwrap(), "fn a() {}\n\nfn c() {}\n");
    }

    /// #448: `+++ /dev/null` deletes the file instead of writing a file
    /// literally named `/dev/null`; `--- /dev/null` creates one.
    #[tokio::test]
    async fn dev_null_headers_delete_and_create() {
        let patch = "\
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-one
-two
--- /dev/null
+++ b/sub/new.txt
@@ -0,0 +1,2 @@
+fresh
+file
";
        let (dir, res) = run_patch(&[("gone.txt", "one\ntwo\n")], patch).await;
        assert!(!res.is_error, "apply patch failed: {}", res.content);
        assert!(!dir.path().join("gone.txt").exists());
        assert_eq!(std::fs::read_to_string(dir.path().join("sub/new.txt")).unwrap(), "fresh\nfile\n");
    }

    /// A deletion whose hunks leave lines behind is refused, not applied.
    #[tokio::test]
    async fn dev_null_delete_must_remove_every_line() {
        let patch = "--- a/keep.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-one\n";
        let (dir, res) = run_patch(&[("keep.txt", "one\ntwo\n")], patch).await;
        assert!(res.is_error);
        assert_eq!(std::fs::read_to_string(dir.path().join("keep.txt")).unwrap(), "one\ntwo\n");
    }

    /// A hunk shorter than its header used to swallow nothing and apply what
    /// it had; now the count mismatch is an error and nothing is written.
    #[tokio::test]
    async fn miscounted_hunk_is_an_error() {
        let patch = "--- a/c.txt\n+++ b/c.txt\n@@ -1,2 +1,2 @@\n-a\n+A\n";
        let (dir, res) = run_patch(&[("c.txt", "a\nb\n")], patch).await;
        assert!(res.is_error);
        assert!(res.content.contains("still expects"), "{}", res.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("c.txt")).unwrap(), "a\nb\n");

        let patch = "--- a/c.txt\n+++ b/c.txt\n@@ -1 +1 @@\n-a\n+A\n b\n";
        let (_dir, res) = run_patch(&[("c.txt", "a\nb\n")], patch).await;
        assert!(res.is_error);
        assert!(res.content.contains("outside any hunk"), "{}", res.content);
    }

    /// Later hunks search from their header line shifted by earlier hunks, so
    /// a repeated context block is patched where the header says.
    #[tokio::test]
    async fn later_hunk_uses_shifted_start_line() {
        let original = "a\nend\na\nend\n";
        let patch = "\
--- a/r.txt
+++ b/r.txt
@@ -0,0 +1,2 @@
+top1
+top2
@@ -3,2 +5,2 @@
 a
-end
+END
";
        let (dir, res) = run_patch(&[("r.txt", original)], patch).await;
        assert!(!res.is_error, "apply patch failed: {}", res.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("r.txt")).unwrap(),
            "top1\ntop2\na\nend\na\nEND\n"
        );
    }

    /// #225: applying a patch to a CRLF file must keep CRLF, not collapse the
    /// whole file to LF (str::lines() strips the `\r`).
    #[tokio::test]
    async fn apply_patch_crlf_file_preserves_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        let original = "one\r\ntwo\r\nthree\r\n";
        std::fs::write(&path, original).unwrap();

        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        // Patch content itself uses LF (as a real unified diff would); the
        // target file's CRLF endings must survive the round-trip.
        let patch = "--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        let res = ApplyPatchTool
            .execute(json!({ "patch": patch }), &ctx)
            .await;
        assert!(!res.is_error, "apply patch failed: {}", res.content);

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "one\r\nTWO\r\nthree\r\n");
        assert_eq!(after.matches('\n').count(), after.matches("\r\n").count());
    }

    /// #226: ApplyPatch writes through `write_atomic`. A successful patch must
    /// leave the file with the right content and NO `.claurst-tmp-*` scratch
    /// file lingering in the directory.
    #[tokio::test]
    async fn apply_patch_writes_atomically_no_tmp_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foo.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();

        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        let patch = "--- a/foo.txt\n+++ b/foo.txt\n@@ -1,2 +1,2 @@\n hello\n-world\n+rust\n";
        let res = ApplyPatchTool.execute(json!({ "patch": patch }), &ctx).await;
        assert!(!res.is_error, "apply patch failed: {}", res.content);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nrust\n");
        let tmp_left = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".claurst-tmp-"));
        assert!(!tmp_left, "atomic write must not leave a temp file behind");
    }
}
