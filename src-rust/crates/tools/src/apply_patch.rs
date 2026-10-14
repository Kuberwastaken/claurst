// ApplyPatch tool: apply a patch to files.
//
// Accepts the `*** Begin Patch` envelope that OpenAI's Codex models emit
// (see `apply_patch/envelope.rs`) and standard unified diffs (as produced by
// `git diff` or `diff -u`); the format is detected from the first line.
// Supports dry_run mode which validates the patch without writing any files.

mod envelope;

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

/// Apply unified-diff `hunks` to `original` (the content of the file the
/// patch calls `name`). Returns the new content and the added / removed
/// line counts.
fn apply_unified(name: &str, original: &str, hunks: &[Hunk]) -> Result<(String, usize, usize), String> {
    // Detect the file's dominant line ending BEFORE editing so we can rejoin
    // with it instead of collapsing everything to LF (#225). `str::lines()`
    // strips both `\n` and `\r\n`, so a CRLF file would otherwise be silently
    // rewritten to LF throughout.
    let eol = crate::line_endings::LineEnding::detect(original);
    let mut lines: Vec<String> = original.lines().map(|l| l.to_string()).collect();

    let mut added = 0usize;
    let mut removed = 0usize;
    // Apply hunks in order, shifting each header's start line by the net
    // change of the hunks before it.
    for (hunk_idx, hunk) in hunks.iter().enumerate() {
        let hint = (hunk.orig_start + added).saturating_sub(removed);
        lines = apply_hunk(lines, hunk, hint)
            .map_err(|e| format!("Failed to apply hunk {} in {}: {}", hunk_idx + 1, name, e))?;
        added += hunk.lines.iter().filter(|(c, _)| *c == '+').count();
        removed += hunk.lines.iter().filter(|(c, _)| *c == '-').count();
    }

    let content = if lines.is_empty() {
        String::new()
    } else {
        // Re-join with the file's original line ending; preserve a trailing
        // newline if the original had one.
        let mut s = lines.join(eol.as_str());
        if original.ends_with('\n') || original.is_empty() {
            s.push_str(eol.as_str());
        }
        s
    };
    Ok((content, added, removed))
}

// ---------------------------------------------------------------------------
// Format-independent changes
// ---------------------------------------------------------------------------

/// How an update rewrites a file's content.
enum Edit<'p> {
    Unified(&'p [Hunk]),
    Envelope(&'p [envelope::UpdateFileChunk]),
}

impl Edit<'_> {
    /// The new content plus added / removed line counts.
    fn apply(&self, name: &str, original: &str) -> Result<(String, usize, usize), String> {
        match self {
            Edit::Unified(hunks) => apply_unified(name, original, hunks),
            Edit::Envelope(chunks) => {
                let content = envelope::apply_chunks(original, name, chunks)?;
                let (added, removed) = chunks.iter().fold((0, 0), |(a, r), chunk| {
                    let context = chunk.context_line_indices.len();
                    (a + chunk.new_lines.len() - context, r + chunk.old_lines.len() - context)
                });
                Ok((content, added, removed))
            }
        }
    }
}

/// One file-level change, whichever format it came from.
enum Change<'p> {
    /// Create the file. `overwrite` lets it replace an existing file (the
    /// envelope's `*** Add File:` does; a unified diff from `/dev/null` does not).
    Add { name: &'p str, content: String, overwrite: bool },
    /// Delete the file; unified-diff hunks must first remove every line.
    Delete { name: &'p str, hunks: &'p [Hunk] },
    /// Rewrite the file, moving it to `move_to` when set.
    Update { name: &'p str, edit: Edit<'p>, move_to: Option<&'p str> },
}

impl<'p> Change<'p> {
    fn name(&self) -> &'p str {
        match self {
            Change::Add { name, .. } | Change::Delete { name, .. } | Change::Update { name, .. } => name,
        }
    }

    /// The summary letter codex prints for this change.
    fn letter(&self) -> char {
        match self {
            Change::Add { .. } => 'A',
            Change::Delete { .. } => 'D',
            Change::Update { .. } => 'M',
        }
    }
}

/// A change checked against the file system and ready to write.
struct Prepared {
    /// The resolved path the patch names.
    path: std::path::PathBuf,
    /// The opened file and its real location, for updates and deletes. The
    /// write or delete is bound to this location (#415, #433).
    opened: Option<(std::fs::File, std::path::PathBuf)>,
    /// Content before the change (empty for a new file).
    before: Vec<u8>,
    /// Content after the change; `None` deletes the file.
    after: Option<String>,
    /// Move destination and its content before the move (empty if new).
    dest: Option<(std::path::PathBuf, Vec<u8>)>,
    added: usize,
    removed: usize,
}

/// Read what a path holds now so the change can be recorded for `/undo`;
/// empty if it does not exist. A directory is refused.
fn existing_content(path: &std::path::Path, name: &str) -> Result<Vec<u8>, String> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Err(format!("Failed to write file {}: it is a directory", name)),
        Ok(_) => std::fs::read(path).map_err(|e| format!("Failed to read {}: {}", name, e)),
        Err(_) => Ok(Vec::new()),
    }
}

/// Write `content` to `path`, creating missing parent directories.
async fn write_new(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
    }
    crate::write_atomic(path, content.as_bytes()).await
}

/// Apply `patch` (an envelope patch or a unified diff) with relative paths
/// resolved against `base_dir`.
///
/// Every file the patch touches is permission-checked as an Edit of that
/// path, locked for the whole call (#443), opened once and re-checked where
/// the handle really points, and written through that handle (#415, #433).
/// All changes are computed before anything is written.
pub(crate) async fn apply_patch_text(
    patch: &str,
    dry_run: bool,
    base_dir: &std::path::Path,
    ctx: &ToolContext,
) -> ToolResult {
    if patch.trim().is_empty() {
        return ToolResult::error("patch must not be empty".to_string());
    }

    let envelope_format = envelope::is_envelope(patch);
    let envelope_hunks: Vec<envelope::Hunk>;
    let file_patches: Vec<FilePatch>;
    let mut changes: Vec<Change> = Vec::new();
    if envelope_format {
        envelope_hunks = match envelope::parse_patch(patch) {
            Ok(hunks) => hunks,
            Err(e) => return ToolResult::error(e),
        };
        for hunk in &envelope_hunks {
            changes.push(match hunk {
                envelope::Hunk::Add { path, contents } => {
                    Change::Add { name: path, content: contents.clone(), overwrite: true }
                }
                envelope::Hunk::Delete { path } => Change::Delete { name: path, hunks: &[] },
                envelope::Hunk::Update { path, move_path, chunks } => Change::Update {
                    name: path,
                    edit: Edit::Envelope(chunks),
                    move_to: move_path.as_deref(),
                },
            });
        }
        if changes.is_empty() {
            return ToolResult::error("No files were modified.".to_string());
        }
    } else {
        file_patches = match parse_unified_diff(patch) {
            Ok(fp) => fp,
            Err(e) => return ToolResult::error(format!("Failed to parse patch: {}", e)),
        };
        for fp in &file_patches {
            changes.push(match fp.target() {
                Ok((name, FileOp::Create)) => match apply_unified(name, "", &fp.hunks) {
                    Ok((content, _, _)) => Change::Add { name, content, overwrite: false },
                    Err(e) => return ToolResult::error(e),
                },
                Ok((name, FileOp::Delete)) => Change::Delete { name, hunks: &fp.hunks },
                Ok((name, FileOp::Modify)) => {
                    Change::Update { name, edit: Edit::Unified(&fp.hunks), move_to: None }
                }
                Err(e) => return ToolResult::error(format!("Failed to parse patch: {}", e)),
            });
        }
        if changes.is_empty() {
            return ToolResult::error(
                "No file diffs found in patch (expected --- / +++ headers)".to_string(),
            );
        }
    }

    // (resolved path, resolved move destination) per change. Plain loops
    // rather than closures over `&str`: a higher-ranked closure kept alive
    // across the awaits below would make this future fail the `Send` check.
    let mut targets: Vec<(std::path::PathBuf, Option<std::path::PathBuf>)> =
        Vec::with_capacity(changes.len());
    for change in &changes {
        let (name, move_to) = match change {
            Change::Update { name, move_to, .. } => (*name, *move_to),
            other => (other.name(), None),
        };
        targets.push((base_dir.join(name), move_to.map(|dest| base_dir.join(dest))));
    }
    let mut all_paths: Vec<&std::path::Path> = Vec::with_capacity(targets.len());
    for (path, dest) in &targets {
        all_paths.push(path);
        if let Some(dest) = dest {
            all_paths.push(dest);
        }
    }

    // One operation per path: a second one would act on content the first
    // has already replaced.
    let mut seen = std::collections::HashSet::new();
    for path in &all_paths {
        if !seen.insert(path) {
            return ToolResult::error(format!(
                "invalid patch: multiple operations target {}",
                path.display()
            ));
        }
    }

    // Each path is checked like an Edit of that path, so Edit rules and the
    // accept-edits mode govern patches too. Dry runs only read.
    let edit_tool = claurst_core::constants::TOOL_NAME_FILE_EDIT;
    let mut grants = Vec::with_capacity(changes.len());
    for (change, (path, dest)) in changes.iter().zip(&targets) {
        if dry_run {
            grants.push(crate::PermissionGrant::Policy);
            continue;
        }
        let verb = match change {
            Change::Add { .. } => "add",
            Change::Delete { .. } => "delete",
            Change::Update { .. } => "update",
        };
        match ctx.check_permission_for_path(
            edit_tool,
            &format!("ApplyPatch: {} {}", verb, path.display()),
            path.clone(),
            false,
        ) {
            Ok(grant) => grants.push(grant),
            Err(e) => return ToolResult::error(e.to_string()),
        }
        if let Some(dest) = dest {
            if let Err(e) = ctx.check_permission_for_path(
                edit_tool,
                &format!("ApplyPatch: move {} to {}", path.display(), dest.display()),
                dest.clone(),
                false,
            ) {
                return ToolResult::error(e.to_string());
            }
        }
    }

    // Lock every path from the reads through the writes (#443); keys are
    // sorted, so this cannot deadlock against BatchEdit or another patch.
    let _locks = crate::lock_files(all_paths).await;

    // ----------------------------------------------------------------
    // Compute every change before writing anything
    // ----------------------------------------------------------------

    let mut prepared: Vec<Prepared> = Vec::with_capacity(changes.len());
    for ((change, (path, dest)), grant) in changes.iter().zip(targets).zip(grants) {
        let name = change.name();
        debug!(path = %path.display(), "ApplyPatch processing file");

        let (opened, before, after, added, removed) = match change {
            Change::Add { content, overwrite, .. } => {
                if !*overwrite && path.exists() {
                    return ToolResult::error(format!(
                        "Cannot create {}: the file already exists",
                        path.display()
                    ));
                }
                let before = match existing_content(&path, name) {
                    Ok(before) => before,
                    Err(e) => return ToolResult::error(e),
                };
                (None, before, Some(content.clone()), content.lines().count(), 0)
            }
            Change::Delete { .. } | Change::Update { .. } => {
                let file = match crate::open_regular_file(&path) {
                    Ok(file) => file,
                    Err(e) => {
                        let what = if matches!(change, Change::Delete { .. }) {
                            "delete file"
                        } else {
                            "read file to update"
                        };
                        return ToolResult::error(format!("Failed to {} {}: {}", what, name, e));
                    }
                };
                let real = if dry_run {
                    path.clone()
                } else {
                    match ctx.confirm_opened_path(edit_tool, "ApplyPatch:", &path, &file, grant, false) {
                        Ok(real) => real,
                        Err(e) => return ToolResult::error(e),
                    }
                };
                let mut original = String::new();
                if let Err(e) = std::io::Read::read_to_string(&mut &file, &mut original) {
                    return ToolResult::error(format!("Failed to read {}: {}", name, e));
                }
                let (after, added, removed) = match change {
                    Change::Delete { hunks, .. } => {
                        if !hunks.is_empty() {
                            match apply_unified(name, &original, hunks) {
                                Ok((left, _, _)) if !left.is_empty() => {
                                    return ToolResult::error(format!(
                                        "Cannot delete {}: the patch removes it (+++ /dev/null) \
                                         but its hunks leave {} line(s) in place",
                                        name,
                                        left.lines().count()
                                    ));
                                }
                                Ok(_) => {}
                                Err(e) => return ToolResult::error(e),
                            }
                        }
                        (None, 0, original.lines().count())
                    }
                    Change::Update { edit, .. } => match edit.apply(name, &original) {
                        Ok((content, added, removed)) => (Some(content), added, removed),
                        Err(e) => return ToolResult::error(e),
                    },
                    Change::Add { .. } => (None, 0, 0),
                };
                (Some((file, real)), original.into_bytes(), after, added, removed)
            }
        };

        let dest = match dest {
            Some(dest) => match existing_content(&dest, &dest.display().to_string()) {
                Ok(dest_before) => Some((dest, dest_before)),
                Err(e) => return ToolResult::error(e),
            },
            None => None,
        };
        prepared.push(Prepared { path, opened, before, after, dest, added, removed });
    }

    let total_added: usize = prepared.iter().map(|p| p.added).sum();
    let total_removed: usize = prepared.iter().map(|p| p.removed).sum();
    let file_summaries: Vec<Value> = changes
        .iter()
        .zip(&prepared)
        .map(|(change, p)| {
            json!({
                "path": change.name(),
                "action": match change {
                    Change::Add { .. } => "add",
                    Change::Delete { .. } => "delete",
                    Change::Update { move_to: Some(_), .. } => "move",
                    Change::Update { .. } => "update",
                },
                "lines_added": p.added,
                "lines_removed": p.removed,
            })
        })
        .collect();

    if dry_run {
        return ToolResult::success(format!(
            "Dry run: patch would modify {} file(s) (+{} -{} lines).",
            prepared.len(),
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
    // Write
    // ----------------------------------------------------------------

    let tool_name = claurst_core::constants::TOOL_NAME_APPLY_PATCH;
    let mut done: Vec<String> = Vec::new();
    for (change, p) in changes.iter().zip(&prepared) {
        let written = async {
            match (&p.after, &p.opened, &p.dest) {
                (Some(after), Some((file, real)), None) => {
                    crate::write_atomic_bound(file, real, after.as_bytes())
                        .await
                        .map_err(|e| (format!("Failed to write file {}", change.name()), e))?;
                    ctx.record_file_change(p.path.clone(), &p.before, after.as_bytes(), tool_name);
                }
                (Some(after), Some((_, real)), Some((dest, dest_before))) => {
                    write_new(dest, after)
                        .await
                        .map_err(|e| (format!("Failed to write file {}", dest.display()), e))?;
                    ctx.record_file_change(dest.clone(), dest_before, after.as_bytes(), tool_name);
                    tokio::fs::remove_file(real)
                        .await
                        .map_err(|e| (format!("Failed to remove original {}", change.name()), e))?;
                    ctx.record_file_change(p.path.clone(), &p.before, b"", tool_name);
                }
                (Some(after), None, _) => {
                    write_new(&p.path, after)
                        .await
                        .map_err(|e| (format!("Failed to write file {}", change.name()), e))?;
                    ctx.record_file_change(p.path.clone(), &p.before, after.as_bytes(), tool_name);
                }
                (None, opened, _) => {
                    let real = opened.as_ref().map_or(&p.path, |(_, real)| real);
                    tokio::fs::remove_file(real)
                        .await
                        .map_err(|e| (format!("Failed to delete file {}", change.name()), e))?;
                    ctx.record_file_change(p.path.clone(), &p.before, b"", tool_name);
                }
            }
            Ok::<(), (String, std::io::Error)>(())
        }
        .await;
        if let Err((context, e)) = written {
            let mut msg = format!("{}: {}", context, e);
            if !done.is_empty() {
                msg.push_str(&format!(
                    "\nThese changes were applied before the failure:\n{}",
                    done.join("\n")
                ));
            }
            return ToolResult::error(msg);
        }
        let shown = match change {
            Change::Update { move_to: Some(dest), .. } => dest,
            other => other.name(),
        };
        done.push(format!("{} {}", change.letter(), shown));
    }

    let metadata = json!({
        "dry_run": false,
        "files": file_summaries,
        "total_lines_added": total_added,
        "total_lines_removed": total_removed,
    });
    if envelope_format {
        // Codex's summary, which its models are trained to read: added, then
        // modified, then deleted files.
        let mut summary = String::from("Success. Updated the following files:\n");
        for letter in ['A', 'M', 'D'] {
            for line in done.iter().filter(|line| line.starts_with(letter)) {
                summary.push_str(line);
                summary.push('\n');
            }
        }
        ToolResult::success(summary).with_metadata(metadata)
    } else {
        ToolResult::success(format!(
            "Applied patch to {} file(s) (+{} -{} lines).",
            prepared.len(),
            total_added,
            total_removed,
        ))
        .with_metadata(metadata)
    }
}

// ---------------------------------------------------------------------------
// Tool implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for ApplyPatchTool {
    // Gates itself: checks every path in `apply_patch_text` (#210).
    fn self_gates(&self) -> bool { true }

    fn name(&self) -> &str {
        claurst_core::constants::TOOL_NAME_APPLY_PATCH
    }

    fn description(&self) -> &str {
        "Apply a patch to one or more files. Accepts either the apply_patch envelope \
         (`*** Begin Patch` / `*** Add File: <path>` / `*** Delete File: <path>` / \
         `*** Update File: <path>` with optional `*** Move to: <path>`, `@@ <anchor line>` \
         sections of ' ' context, '-' removed and '+' added lines, `*** End Patch`) or a \
         standard unified diff (as produced by `git diff` or `diff -u`). Envelope context \
         is matched exactly first, then ignoring whitespace, then ignoring Unicode \
         punctuation. Nothing is written unless every change applies. Set dry_run=true to \
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
                    "description": "An apply_patch envelope (*** Begin Patch ... *** End Patch) or a unified diff"
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
        apply_patch_text(&params.patch, params.dry_run, &ctx.working_dir, ctx).await
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

    // --- envelope format ---

    /// Every regular file under `dir`, keyed by its `/`-separated relative path.
    fn snapshot(dir: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                    out.insert(rel, std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    /// Scenario fixtures from openai/codex
    /// `codex-rs/apply-patch/tests/fixtures/scenarios` (Apache-2.0):
    /// `(name, input files, patch, expected files, should fail)`.
    /// `015` is adapted: codex leaves the first change of a failing patch in
    /// place, claurst computes every change before writing and applies none.
    #[allow(clippy::type_complexity)]
    const SCENARIOS: &[(&str, &[(&str, &str)], &str, &[(&str, &str)], bool)] = &[
        ("001_add_file", &[], "*** Begin Patch\n*** Add File: bar.md\n+This is a new file\n*** End Patch\n",
            &[("bar.md", "This is a new file\n")], false),
        ("002_multiple_operations", &[("delete.txt", "obsolete\n"), ("modify.txt", "line1\nline2\n")],
            "*** Begin Patch\n*** Add File: nested/new.txt\n+created\n*** Delete File: delete.txt\n*** Update File: modify.txt\n@@\n-line2\n+changed\n*** End Patch\n",
            &[("modify.txt", "line1\nchanged\n"), ("nested/new.txt", "created\n")], false),
        ("003_multiple_chunks", &[("multi.txt", "line1\nline2\nline3\nline4\n")],
            "*** Begin Patch\n*** Update File: multi.txt\n@@\n-line2\n+changed2\n@@\n-line4\n+changed4\n*** End Patch\n",
            &[("multi.txt", "line1\nchanged2\nline3\nchanged4\n")], false),
        ("004_move_to_new_directory", &[("old/name.txt", "old content\n"), ("old/other.txt", "unrelated file\n")],
            "*** Begin Patch\n*** Update File: old/name.txt\n*** Move to: renamed/dir/name.txt\n@@\n-old content\n+new content\n*** End Patch\n",
            &[("old/other.txt", "unrelated file\n"), ("renamed/dir/name.txt", "new content\n")], false),
        ("005_rejects_empty_patch", &[("foo.txt", "stable\n")], "*** Begin Patch\n*** End Patch\n",
            &[("foo.txt", "stable\n")], true),
        ("006_rejects_missing_context", &[("modify.txt", "line1\nline2\n")],
            "*** Begin Patch\n*** Update File: modify.txt\n@@\n-missing\n+changed\n*** End Patch\n",
            &[("modify.txt", "line1\nline2\n")], true),
        ("007_rejects_missing_file_delete", &[("foo.txt", "stable\n")],
            "*** Begin Patch\n*** Delete File: missing.txt\n*** End Patch\n", &[("foo.txt", "stable\n")], true),
        ("008_rejects_empty_update_hunk", &[("foo.txt", "stable\n")],
            "*** Begin Patch\n*** Update File: foo.txt\n*** End Patch\n", &[("foo.txt", "stable\n")], true),
        ("009_requires_existing_file_for_update", &[("foo.txt", "stable\n")],
            "*** Begin Patch\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch\n",
            &[("foo.txt", "stable\n")], true),
        ("010_move_overwrites_existing_destination",
            &[("old/name.txt", "from\n"), ("old/other.txt", "unrelated file\n"), ("renamed/dir/name.txt", "existing\n")],
            "*** Begin Patch\n*** Update File: old/name.txt\n*** Move to: renamed/dir/name.txt\n@@\n-from\n+new\n*** End Patch\n",
            &[("old/other.txt", "unrelated file\n"), ("renamed/dir/name.txt", "new\n")], false),
        ("011_add_overwrites_existing_file", &[("duplicate.txt", "old content\n")],
            "*** Begin Patch\n*** Add File: duplicate.txt\n+new content\n*** End Patch\n",
            &[("duplicate.txt", "new content\n")], false),
        ("012_delete_directory_fails", &[("dir/foo.txt", "stable\n")],
            "*** Begin Patch\n*** Delete File: dir\n*** End Patch\n", &[("dir/foo.txt", "stable\n")], true),
        ("013_rejects_invalid_hunk_header", &[("foo.txt", "stable\n")],
            "*** Begin Patch\n*** Frobnicate File: foo\n*** End Patch\n", &[("foo.txt", "stable\n")], true),
        ("014_update_file_appends_trailing_newline", &[("no_newline.txt", "no newline at end")],
            "*** Begin Patch\n*** Update File: no_newline.txt\n@@\n-no newline at end\n+first line\n+second line\n*** End Patch\n",
            &[("no_newline.txt", "first line\nsecond line\n")], false),
        ("015_failure_after_partial_success_applies_nothing", &[],
            "*** Begin Patch\n*** Add File: created.txt\n+hello\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch\n",
            &[], true),
        ("016_pure_addition_update_chunk", &[("input.txt", "line1\nline2\n")],
            "*** Begin Patch\n*** Update File: input.txt\n@@\n+added line 1\n+added line 2\n*** End Patch\n",
            &[("input.txt", "line1\nline2\nadded line 1\nadded line 2\n")], false),
        ("017_whitespace_padded_hunk_header", &[("foo.txt", "old\n")],
            "*** Begin Patch\n  *** Update File: foo.txt\n@@\n-old\n+new\n*** End Patch\n",
            &[("foo.txt", "new\n")], false),
        ("018_whitespace_padded_patch_markers", &[("file.txt", "one\n")],
            " *** Begin Patch\n*** Update File: file.txt\n@@\n-one\n+two\n*** End Patch \n",
            &[("file.txt", "two\n")], false),
        ("019_unicode_simple", &[("foo.txt", "line1\nna\u{ef}ve caf\u{e9}\nline3\n")],
            "*** Begin Patch\n*** Update File: foo.txt\n@@\n line1\n-na\u{ef}ve caf\u{e9}\n+na\u{ef}ve caf\u{e9} \u{2705}\n*** End Patch\n",
            &[("foo.txt", "line1\nna\u{ef}ve caf\u{e9} \u{2705}\nline3\n")], false),
        ("020_delete_file_success", &[("keep.txt", "keep\n"), ("obsolete.txt", "obsolete\n")],
            "*** Begin Patch\n*** Delete File: obsolete.txt\n*** End Patch\n", &[("keep.txt", "keep\n")], false),
        ("020_whitespace_padded_patch_marker_lines", &[("file.txt", "one\n")],
            "*** Begin Patch \n*** Update File: file.txt\n@@\n-one\n+two\n *** End Patch\n",
            &[("file.txt", "two\n")], false),
        ("021_update_file_deletion_only", &[("lines.txt", "line1\nline2\nline3\n")],
            "*** Begin Patch\n*** Update File: lines.txt\n@@\n line1\n-line2\n line3\n*** End Patch\n",
            &[("lines.txt", "line1\nline3\n")], false),
        ("022_update_file_end_of_file_marker", &[("tail.txt", "first\nsecond\n")],
            "*** Begin Patch\n*** Update File: tail.txt\n@@\n first\n-second\n+second updated\n*** End of File\n*** End Patch\n",
            &[("tail.txt", "first\nsecond updated\n")], false),
        ("023_preserves_crlf_line_endings", &[("lines.txt", "one\r\ntwo\r\nthree\r\n")],
            "*** Begin Patch\n*** Update File: lines.txt\n@@\n-one\n+ONE\n two\n+between\n three\n*** End Patch\n",
            &[("lines.txt", "ONE\r\ntwo\r\nbetween\r\nthree\r\n")], false),
        ("024_preserves_mixed_line_endings", &[("lines.txt", "one\r\ntwo\rthree\nfour\r\n")],
            "*** Begin Patch\n*** Update File: lines.txt\n@@\n one\n two\n-three\n+THREE\n four\n*** End Patch\n",
            &[("lines.txt", "one\r\ntwo\rTHREE\r\nfour\r\n")], false),
    ];

    #[tokio::test]
    async fn envelope_scenarios_from_codex() {
        for (name, inputs, patch, expected, should_fail) in SCENARIOS {
            let dir = tempfile::tempdir().unwrap();
            for (path, content) in *inputs {
                let path = dir.path().join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, content).unwrap();
            }
            let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
            let res = ApplyPatchTool.execute(json!({ "patch": patch }), &ctx).await;
            assert_eq!(res.is_error, *should_fail, "{}: {}", name, res.content);
            let want: std::collections::BTreeMap<String, Vec<u8>> = expected
                .iter()
                .map(|(path, content)| (path.to_string(), content.as_bytes().to_vec()))
                .collect();
            assert_eq!(snapshot(dir.path()), want, "{}", name);
        }
    }

    /// Codex's success summary, which its models are trained on.
    #[tokio::test]
    async fn envelope_success_summary_matches_codex() {
        let patch = "*** Begin Patch\n*** Delete File: d.txt\n*** Update File: m.txt\n@@\n-a\n+b\n*** Add File: a.txt\n+x\n*** End Patch\n";
        let (_dir, res) = run_patch(&[("d.txt", "gone\n"), ("m.txt", "a\n")], patch).await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(res.content, "Success. Updated the following files:\nA a.txt\nM m.txt\nD d.txt\n");
    }

    /// Codex lib.rs tests: interleaved chunks, a pure addition before a
    /// removal, and ASCII patch lines matching Unicode punctuation.
    #[tokio::test]
    async fn envelope_codex_update_cases() {
        let patch = "*** Begin Patch\n*** Update File: i.txt\n@@\n a\n-b\n+B\n@@\n c\n d\n-e\n+E\n@@\n f\n+g\n*** End of File\n*** End Patch";
        let (dir, res) = run_patch(&[("i.txt", "a\nb\nc\nd\ne\nf\n")], patch).await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("i.txt")).unwrap(), "a\nB\nc\nd\nE\nf\ng\n");

        let patch = "*** Begin Patch\n*** Update File: p.txt\n@@\n+after-context\n+second-line\n@@\n line1\n-line2\n-line3\n+line2-replacement\n*** End Patch";
        let (dir, res) = run_patch(&[("p.txt", "line1\nline2\nline3\n")], patch).await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("p.txt")).unwrap(),
            "line1\nline2-replacement\nafter-context\nsecond-line\n"
        );

        let original = "import asyncio  # local import \u{2013} avoids top\u{2011}level dep\n";
        let patch = "*** Begin Patch\n*** Update File: u.py\n@@\n-import asyncio  # local import - avoids top-level dep\n+import asyncio  # HELLO\n*** End Patch";
        let (dir, res) = run_patch(&[("u.py", original)], patch).await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("u.py")).unwrap(), "import asyncio  # HELLO\n");
    }

    /// `@@ <anchor>` narrows the search to after the anchor line.
    #[tokio::test]
    async fn envelope_anchor_selects_the_later_occurrence() {
        let original = "class A:\n    def f(self):\n        return 1\nclass B:\n    def f(self):\n        return 1\n";
        let patch = "*** Begin Patch\n*** Update File: c.py\n@@ class B:\n     def f(self):\n-        return 1\n+        return 2\n*** End Patch";
        let (dir, res) = run_patch(&[("c.py", original)], patch).await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("c.py")).unwrap(),
            "class A:\n    def f(self):\n        return 1\nclass B:\n    def f(self):\n        return 2\n"
        );
    }

    /// A context that matches only after ignoring whitespace, in two places,
    /// is refused instead of patching the first one.
    #[tokio::test]
    async fn envelope_ambiguous_fuzzy_match_is_refused() {
        let original = "  x = 1\nmid\n    x = 1\n";
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-x = 1\n+x = 2\n*** End Patch";
        let (dir, res) = run_patch(&[("a.txt", original)], patch).await;
        assert!(res.is_error);
        assert!(res.content.contains("match 2 places"), "{}", res.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), original);
    }

    /// Context is matched without the file's UTF-8 BOM, which is kept.
    #[tokio::test]
    async fn envelope_keeps_bom_out_of_matching() {
        let patch = "*** Begin Patch\n*** Update File: b.txt\n@@\n-first\n+FIRST\n second\n*** End Patch";
        let (dir, res) = run_patch(&[("b.txt", "\u{feff}first\nsecond\n")], patch).await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("b.txt")).unwrap(), "\u{feff}FIRST\nsecond\n");
    }

    #[tokio::test]
    async fn envelope_rejects_two_operations_on_one_path() {
        let patch = "*** Begin Patch\n*** Delete File: a.txt\n*** Add File: a.txt\n+x\n*** End Patch";
        let (dir, res) = run_patch(&[("a.txt", "a\n")], patch).await;
        assert!(res.is_error);
        assert!(res.content.contains("multiple operations target"), "{}", res.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "a\n");
    }

    #[tokio::test]
    async fn envelope_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-a\n+b\n*** Add File: n.txt\n+n\n*** End Patch";
        let res = ApplyPatchTool.execute(json!({ "patch": patch, "dry_run": true }), &ctx).await;
        assert!(!res.is_error, "{}", res.content);
        assert!(res.content.starts_with("Dry run: patch would modify 2 file(s)"), "{}", res.content);
        assert_eq!(snapshot(dir.path()).len(), 1);
    }

    /// Records every permission request and allows it.
    struct RecordingHandler(parking_lot::Mutex<Vec<(String, Option<String>)>>);

    impl claurst_core::permissions::PermissionHandler for RecordingHandler {
        fn check_permission(
            &self,
            request: &claurst_core::permissions::PermissionRequest,
        ) -> claurst_core::permissions::PermissionDecision {
            self.request_permission(request)
        }

        fn request_permission(
            &self,
            request: &claurst_core::permissions::PermissionRequest,
        ) -> claurst_core::permissions::PermissionDecision {
            self.0.lock().push((request.tool_name.clone(), request.path.clone()));
            claurst_core::permissions::PermissionDecision::Allow
        }
    }

    /// Every path the patch writes, the move destination included, is
    /// checked as an Edit of that path.
    #[tokio::test]
    async fn every_path_is_checked_as_an_edit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("src.txt"), "a\n").unwrap();
        let handler = std::sync::Arc::new(RecordingHandler(parking_lot::Mutex::new(Vec::new())));
        let ctx = crate::test_support::context_with_handler(dir.path().to_path_buf(), handler.clone());
        let patch = "*** Begin Patch\n*** Update File: src.txt\n*** Move to: dst.txt\n@@\n-a\n+b\n*** Add File: new.txt\n+n\n*** End Patch";
        let res = ApplyPatchTool.execute(json!({ "patch": patch }), &ctx).await;
        assert!(!res.is_error, "{}", res.content);

        let requests = handler.0.lock();
        assert!(requests.iter().all(|(tool, _)| tool == claurst_core::constants::TOOL_NAME_FILE_EDIT));
        for name in ["src.txt", "dst.txt", "new.txt"] {
            let path = dir.path().join(name).display().to_string();
            assert!(
                requests.iter().any(|(_, p)| p.as_deref() == Some(path.as_str())),
                "no permission check for {}: {:?}",
                name,
                requests
            );
        }
    }

    /// A denied path stops the whole patch before anything is written.
    #[tokio::test]
    async fn denied_path_writes_nothing() {
        struct DenyNamed(&'static str);
        impl claurst_core::permissions::PermissionHandler for DenyNamed {
            fn check_permission(
                &self,
                request: &claurst_core::permissions::PermissionRequest,
            ) -> claurst_core::permissions::PermissionDecision {
                self.request_permission(request)
            }
            fn request_permission(
                &self,
                request: &claurst_core::permissions::PermissionRequest,
            ) -> claurst_core::permissions::PermissionDecision {
                if request.path.as_deref().is_some_and(|p| p.ends_with(self.0)) {
                    claurst_core::permissions::PermissionDecision::Deny
                } else {
                    claurst_core::permissions::PermissionDecision::Allow
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.txt"), "a\n").unwrap();
        let ctx = crate::test_support::context_with_handler(
            dir.path().to_path_buf(),
            std::sync::Arc::new(DenyNamed("secret.txt")),
        );
        let patch = "*** Begin Patch\n*** Update File: ok.txt\n@@\n-a\n+b\n*** Add File: secret.txt\n+s\n*** End Patch";
        let res = ApplyPatchTool.execute(json!({ "patch": patch }), &ctx).await;
        assert!(res.is_error);
        assert_eq!(std::fs::read_to_string(dir.path().join("ok.txt")).unwrap(), "a\n");
        assert!(!dir.path().join("secret.txt").exists());
    }

    /// #443: ApplyPatch and BatchEdit lock their files in one sorted order, so
    /// running them concurrently on the same two files in opposite orders
    /// neither deadlocks nor loses an edit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_patch_and_batch_edit_share_the_locks() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        let lines: String = (0..20).map(|i| format!("line {:02}\n", i)).collect();
        std::fs::write(&a, &lines).unwrap();
        std::fs::write(&b, &lines).unwrap();
        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());

        let mut tasks = Vec::new();
        for i in 0..10 {
            let (ctx_patch, ctx_batch) = (ctx.clone(), ctx.clone());
            let patch = format!(
                "*** Begin Patch\n*** Update File: b.txt\n@@\n-line {i:02}\n+PATCH {i:02}\n*** Update File: a.txt\n@@\n-line {i:02}\n+PATCH {i:02}\n*** End Patch",
            );
            tasks.push(tokio::spawn(async move {
                ApplyPatchTool.execute(json!({ "patch": patch }), &ctx_patch).await
            }));
            let j = i + 10;
            let edits = json!({ "edits": [
                { "file_path": a.to_string_lossy(), "old_string": format!("line {j:02}"), "new_string": format!("BATCH {j:02}") },
                { "file_path": b.to_string_lossy(), "old_string": format!("line {j:02}"), "new_string": format!("BATCH {j:02}") },
            ]});
            tasks.push(tokio::spawn(async move {
                crate::BatchEditTool.execute(edits, &ctx_batch).await
            }));
        }
        let all = futures::future::join_all(tasks);
        let results = tokio::time::timeout(std::time::Duration::from_secs(30), all)
            .await
            .expect("ApplyPatch and BatchEdit deadlocked");
        for res in results {
            let res = res.unwrap();
            assert!(!res.is_error, "{}", res.content);
        }
        let expected: String = (0..20)
            .map(|i| if i < 10 { format!("PATCH {:02}\n", i) } else { format!("BATCH {:02}\n", i) })
            .collect();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), expected);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), expected);
    }
}
