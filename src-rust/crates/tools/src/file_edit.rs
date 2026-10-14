// FileEdit tool: string replacement with old/new strings (like sed but
// deterministic).  Matching is exact first; when that finds nothing, a
// second tier matches whole lines while ignoring line endings and trailing
// whitespace, and a miss reports the closest region of the file (#443).

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
        "Performs exact string replacements in files. You MUST Read the file \
         first. When copying text from Read output, keep the exact indentation \
         and omit the line-number prefix (the \"12<TAB>\" before each line); \
         never include it in old_string or new_string. Write line breaks as \\n; \
         the file's existing CRLF or LF endings are preserved. If there is no \
         exact match, whole lines are matched ignoring line endings and trailing \
         whitespace, and the result says so. Either way the match must be unique \
         (unless replace_all=true for an exact match); if old_string is not \
         found, the error shows the closest region of the file with line \
         numbers. Add surrounding context to disambiguate, or set \
         replace_all=true to change every occurrence (e.g. renaming a symbol)."
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

        // A BOM copied into the strings would be duplicated or never match;
        // the file's own BOM is handled below.
        let old_string = params.old_string.strip_prefix('\u{feff}').unwrap_or(&params.old_string);
        let new_string = params.new_string.strip_prefix('\u{feff}').unwrap_or(&params.new_string);

        // Validate old != new
        if old_string == new_string {
            return ToolResult::error("old_string and new_string must be different".to_string());
        }

        if old_string.is_empty() {
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

        // Serialise with other tools changing this file (#443); held through
        // the write and the formatter.
        let _lock = crate::lock_files([path.as_path()]).await;

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
        let mut original = String::new();
        if let Err(e) = (&file).read_to_string(&mut original) {
            return ToolResult::error(format!("Failed to read file {}: {}", path.display(), e));
        }

        // Keep a UTF-8 BOM out of matching and put it back on write (#443).
        let (bom, content) = match original.strip_prefix('\u{feff}') {
            Some(rest) => ("\u{feff}", rest),
            None => ("", original.as_str()),
        };

        // Detect the file's original/dominant line ending BEFORE editing so we
        // can re-apply it on write (#225).  Matching is done against an
        // LF-normalized view so CRLF/LF differences never affect the match, but
        // only the lines the edit actually changes are ever rewritten.
        let eol = crate::line_endings::LineEnding::detect(content);
        let normalized = content.replace("\r\n", "\n");
        let old_string = old_string.replace("\r\n", "\n");
        let new_string = new_string.replace("\r\n", "\n");

        // Tier 1: exact occurrences.
        let count = normalized.matches(&old_string).count();

        if count > 1 && !params.replace_all {
            return ToolResult::error(format!(
                "old_string appears {} times in {}. Either provide a larger string \
                 with more surrounding context to make it unique, or set replace_all \
                 to true to replace every occurrence.",
                count,
                path.display()
            ));
        }

        let (body, replacements, tier) = if count > 0 {
            // Perform the replacement on the ORIGINAL bytes, preserving every
            // untouched region's line endings and re-rendering inserted lines
            // with the file's dominant line ending.
            let (body, _) = crate::line_endings::replace_preserving_eol(
                content,
                &old_string,
                &new_string,
                eol,
                params.replace_all,
            );
            (body, if params.replace_all { count } else { 1 }, "exact")
        } else {
            // Tier 2: whole lines, ignoring line endings and trailing whitespace.
            match find_whitespace_normalized(content, &old_string) {
                LineMatch::One { start, end } => {
                    let body = format!("{}{}{}", &content[..start], eol.apply(&new_string), &content[end..]);
                    (body, 1, "trailing_whitespace_normalized")
                }
                LineMatch::Many(n) => {
                    return ToolResult::error(format!(
                        "old_string has no exact match in {}, and ignoring trailing whitespace \
                         it matches {} places. Copy old_string exactly from the file, with \
                         enough surrounding lines to make it unique.",
                        path.display(),
                        n
                    ));
                }
                LineMatch::None => {
                    let hint = match closest_region(&normalized, &old_string) {
                        Some(region) => format!("\n\n{}", region),
                        None => "\n\nNo similar region was found; re-read the file.".to_string(),
                    };
                    return ToolResult::error(format!(
                        "old_string not found in {}, even ignoring line endings and trailing \
                         whitespace. Make sure the string matches exactly, including \
                         indentation.{}",
                        path.display(),
                        hint
                    ));
                }
            }
        };
        let new_content = format!("{}{}", bom, body);

        // Write back atomically into the directory that holds the opened file.
        if let Err(e) = crate::write_atomic_bound(&file, &real, new_content.as_bytes()).await {
            return ToolResult::error(format!("Failed to write file {}: {}", path.display(), e));
        }

        ctx.record_file_change(
            path.clone(),
            original.as_bytes(),
            new_content.as_bytes(),
            self.name(),
        );

        // Run any configured formatter for this file type.
        // Format the file that was written, not whatever `path` names now.
        crate::try_format_file(&real.to_string_lossy(), ctx).await;

        let msg = format!(
            "Successfully edited {} ({} replacement{}{}).",
            path.display(),
            replacements,
            if replacements != 1 { "s" } else { "" },
            if tier == "exact" { "" } else { "; matched after ignoring trailing whitespace" }
        );

        ToolResult::success(msg).with_metadata(json!({
            "file_path": path.display().to_string(),
            "replacements": replacements,
            "match": tier,
        }))
    }
}

/// Result of the second-tier, whole-line match.
#[derive(Debug, PartialEq, Eq)]
enum LineMatch {
    None,
    /// The single match, as a byte span of the content.
    One { start: usize, end: usize },
    /// The number of places that matched; ambiguous, so not applied.
    Many(usize),
}

/// Second tier (#443): match the LF-normalized `old` against whole lines of
/// `content`, ignoring the line ending and trailing whitespace of every line.
///
/// The span covers the matched lines without the last line's terminator,
/// unless `old` itself ends with a newline, mirroring what an exact match of
/// the same text would replace.
fn find_whitespace_normalized(content: &str, old: &str) -> LineMatch {
    let (body, with_terminator) = match old.strip_suffix('\n') {
        Some(body) => (body, true),
        None => (old, false),
    };
    // Whitespace-only text would match any blank line; leave it to the
    // exact tier.
    if body.trim().is_empty() {
        return LineMatch::None;
    }
    let wanted: Vec<&str> = body.split('\n').map(str::trim_end).collect();

    // (byte offset, line including its terminator)
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0;
    for raw in content.split_inclusive('\n') {
        lines.push((offset, raw));
        offset += raw.len();
    }
    if wanted.len() > lines.len() {
        return LineMatch::None;
    }

    let mut first = None;
    let mut matches = 0;
    for i in 0..=lines.len() - wanted.len() {
        if wanted.iter().zip(&lines[i..]).all(|(w, (_, raw))| raw.trim_end() == *w) {
            matches += 1;
            first.get_or_insert(i);
        }
    }
    match (matches, first) {
        (1, Some(i)) => {
            let (last_offset, last) = lines[i + wanted.len() - 1];
            let kept = if with_terminator {
                last
            } else {
                last.trim_end_matches(['\n', '\r'])
            };
            LineMatch::One { start: lines[i].0, end: last_offset + kept.len() }
        }
        (0, _) | (_, None) => LineMatch::None,
        (n, Some(_)) => LineMatch::Many(n),
    }
}

/// Wall-time budget for [`closest_region`]. The hint runs synchronously
/// while the file's lock is held, so it must stay cheap whatever the file
/// holds (minified code, base64, one huge line).
const HINT_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);
/// Files with more lines than this get no hint.
const MAX_HINT_FILE_LINES: usize = 100_000;

/// The window of `normalized` most similar to `old`, rendered as numbered
/// lines (as Read shows them, long lines cut) with two lines of context, or
/// `None` when no window is at least half similar.
///
/// Bounded by [`HINT_BUDGET`]: each per-line character diff carries the
/// deadline, a pair whose lengths differ more than 3x is scored from the
/// lengths alone (its ratio is then below 0.5 anyway), and the window scan
/// stops at the deadline and keeps the best window found so far.
fn closest_region(normalized: &str, old: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + HINT_BUDGET;
    let file: Vec<&str> = normalized.lines().collect();
    let wanted: Vec<(&str, usize)> = old
        .trim_end_matches('\n')
        .split('\n')
        .map(|w| (w.trim(), w.trim().chars().count()))
        .collect();
    if file.is_empty() || file.len() > MAX_HINT_FILE_LINES {
        return None;
    }
    let window = wanted.len().min(file.len());
    let probe = &wanted[..window];

    let line_similarity = |(w, w_len): (&str, usize), line: &str| -> f32 {
        let line = line.trim();
        if w == line {
            return 1.0;
        }
        let line_len = line.chars().count();
        let (short, long) = (w_len.min(line_len), w_len.max(line_len));
        if long > 3 * short {
            // ratio = 2 * matches / (short + long), and matches <= short.
            return 2.0 * short as f32 / (short + long) as f32;
        }
        similar::TextDiff::configure().deadline(deadline).diff_chars(w, line).ratio()
    };
    let mut best = (0usize, 0f32);
    for i in 0..=file.len() - window {
        if std::time::Instant::now() > deadline {
            break;
        }
        let score = probe
            .iter()
            .zip(&file[i..])
            .map(|(w, l)| line_similarity(*w, l))
            .sum::<f32>()
            / probe.len() as f32;
        if score > best.1 {
            best = (i, score);
        }
    }
    let (at, score) = best;
    if score < 0.5 {
        return None;
    }

    let from = at.saturating_sub(2);
    let to = (at + window + 2).min(file.len()).min(from + 30);
    let width = format!("{}", to).len();
    let mut out = format!(
        "Closest match is lines {}-{} ({:.0}% similar):\n",
        at + 1,
        at + window,
        score * 100.0
    );
    for (n, line) in file[from..to].iter().enumerate() {
        out.push_str(&format!(
            "{:>width$}\t{}\n",
            from + n + 1,
            crate::file_read::cut_line(line),
            width = width
        ));
    }
    out.push_str("Copy old_string from the file exactly and retry.");
    Some(out)
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

    async fn edit(path: &std::path::Path, old: &str, new: &str) -> ToolResult {
        let ctx = allow_all_context(path.parent().unwrap().to_path_buf());
        FileEditTool
            .execute(
                json!({ "file_path": path.to_string_lossy(), "old_string": old, "new_string": new }),
                &ctx,
            )
            .await
    }

    /// #443: concurrent edits to one file serialise on the per-path lock, so
    /// every edit lands instead of a later write dropping an earlier one (or
    /// being refused because the file was replaced under its handle).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_edits_to_one_file_all_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.txt");
        let original: String = (0..16).map(|i| format!("line {:02}\n", i)).collect();
        std::fs::write(&path, &original).unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let tasks: Vec<_> = (0..16)
            .map(|i| {
                let ctx = ctx.clone();
                let input = json!({
                    "file_path": path.to_string_lossy(),
                    "old_string": format!("line {:02}", i),
                    "new_string": format!("LINE {:02}", i),
                });
                tokio::spawn(async move { FileEditTool.execute(input, &ctx).await })
            })
            .collect();
        for task in tasks {
            let res = task.await.unwrap();
            assert!(!res.is_error, "concurrent edit failed: {}", res.content);
        }

        let expected: String = (0..16).map(|i| format!("LINE {:02}\n", i)).collect();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    }

    /// #443: a UTF-8 BOM survives an edit and does not stop a first-line match.
    #[tokio::test]
    async fn edit_preserves_utf8_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom.txt");
        std::fs::write(&path, "\u{feff}first\r\nsecond\r\n").unwrap();

        let res = edit(&path, "first", "FIRST").await;
        assert!(!res.is_error, "edit failed: {}", res.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{feff}FIRST\r\nsecond\r\n");

        // A BOM copied into old_string still matches and is not doubled.
        let res = edit(&path, "\u{feff}FIRST", "\u{feff}first").await;
        assert!(!res.is_error, "edit failed: {}", res.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{feff}first\r\nsecond\r\n");
    }

    /// #443: old_string with different trailing whitespace and line endings
    /// matches whole lines in the second tier; untouched lines keep CRLF.
    #[tokio::test]
    async fn edit_matches_ignoring_trailing_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ws.rs");
        std::fs::write(&path, "fn a() {\r\n    let x = 1;   \r\n    x\r\n}\r\n").unwrap();

        let res = edit(&path, "fn a() {  \n    let x = 1;\n", "fn a() {\n    let x = 2;\n").await;
        assert!(!res.is_error, "edit failed: {}", res.content);
        assert!(res.content.contains("ignoring trailing whitespace"), "{}", res.content);
        assert_eq!(res.metadata.unwrap()["match"], "trailing_whitespace_normalized");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn a() {\r\n    let x = 2;\r\n    x\r\n}\r\n"
        );

        // Without a trailing newline in old_string the last line's ending stays.
        let res = edit(&path, "    x  ", "    x + 1").await;
        assert!(!res.is_error, "edit failed: {}", res.content);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn a() {\r\n    let x = 2;\r\n    x + 1\r\n}\r\n"
        );
    }

    /// #443: a second-tier match that hits more than one place is rejected.
    #[tokio::test]
    async fn edit_rejects_ambiguous_normalized_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dup.txt");
        let original = "value = 1 \nother\nvalue = 1\t\n";
        std::fs::write(&path, original).unwrap();

        let res = edit(&path, "value = 1  ", "value = 2").await;
        assert!(res.is_error, "ambiguous edit applied: {}", res.content);
        assert!(res.content.contains("matches 2 places"), "{}", res.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// #443: a miss names the closest region with line numbers and context.
    #[tokio::test]
    async fn edit_miss_reports_closest_region() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hint.rs");
        let original = "use std::io;\n\nfn main() {\n    let total = compute(1, 2);\n    println!(\"{}\", total);\n}\n\nfn other() {}\n";
        std::fs::write(&path, original).unwrap();

        let res = edit(&path, "    let totl = compute(1, 3);\n    println!(\"{}\", totl);", "x").await;
        assert!(res.is_error);
        assert!(res.content.contains("Closest match is lines 4-5"), "{}", res.content);
        assert!(res.content.contains("4\t    let total = compute(1, 2);"), "{}", res.content);
        assert!(res.content.contains("3\tfn main() {"), "context before: {}", res.content);
        assert!(res.content.contains("6\t}"), "context after: {}", res.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        let res = edit(&path, "zzzz qqqq 9999", "x").await;
        assert!(res.is_error);
        assert!(res.content.contains("No similar region"), "{}", res.content);
    }

    /// #443 review: the closest-match hint stays fast on a file holding one
    /// huge line, and that line is cut in the hint rather than echoed whole.
    #[tokio::test]
    async fn edit_miss_hint_is_bounded_on_huge_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("min.js");
        let huge = "a".repeat(1024 * 1024);
        let original = format!("let first = 1;\n{}\nlet last = 2;\n", huge);
        std::fs::write(&path, &original).unwrap();

        let started = std::time::Instant::now();
        let res = edit(&path, "let frist = 1;\nlet lsat = 2;", "x").await;
        let elapsed = started.elapsed();
        assert!(res.is_error);
        assert!(elapsed < std::time::Duration::from_secs(1), "hint took {:?}", elapsed);
        assert!(res.content.len() < 8 * 1024, "hint echoed {} bytes", res.content.len());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// #443 review: whitespace-only old_string must not tier-2-match a blank line.
    #[tokio::test]
    async fn edit_whitespace_only_old_string_does_not_match_blank_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blank.txt");
        std::fs::write(&path, "a\n\nb\n").unwrap();
        let res = edit(&path, "   ", "x").await;
        assert!(res.is_error, "whitespace-only edit applied: {}", res.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n\nb\n");
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
