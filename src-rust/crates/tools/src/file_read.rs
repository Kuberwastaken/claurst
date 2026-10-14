// FileRead tool: read a text file a bounded page at a time, with offset/limit
// continuation. Images and PDFs are not decoded; see `execute`.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tracing::debug;

pub struct FileReadTool;

/// Lines returned when the caller gives no `limit`.
const DEFAULT_LINE_LIMIT: usize = 2000;
/// Most bytes of file text one call returns, so a huge file cannot flood the
/// context; the trailer says where to continue.
const MAX_OUTPUT_BYTES: usize = 50 * 1024;
/// A single line longer than this is cut (on a char boundary).
pub(crate) const MAX_LINE_BYTES: usize = 2000;

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
        "Reads a text file from the local filesystem. Each line is prefixed with \
         its number and a tab (\"12<TAB>\"), starting at 1. Returns at most 2000 \
         lines and about 50 KB per call; lines longer than 2000 bytes are cut. When more \
         remains, a trailer gives the offset to continue from: keep calling Read \
         with that offset until the file is fully read, rather than using cat or \
         sed. Images and PDFs are not decoded: the tool only confirms the file \
         exists and you do not see its content. Binary files are rejected."
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
                    "description": "The line number to start reading from (1-based). Use the offset from a truncation trailer to continue a large file."
                },
                "limit": {
                    "type": "number",
                    "description": "The maximum number of lines to read (default 2000, at least 1). Output is also capped at about 50 KB."
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

        // The tool result is text only (`ToolResult::content`), so image and
        // PDF bytes cannot reach the model from here. Say so instead of
        // implying the content was seen. SVG is XML text and is read as such.
        let image_exts = ["png", "jpg", "jpeg", "gif", "bmp", "webp", "ico"];
        if image_exts.contains(&ext.as_str()) {
            return ToolResult::success(format!(
                "[Image file: {}. Read returns text only, so the image content was not \
                 loaded and you have not seen it. Do not describe it; ask the user to \
                 attach the image to the conversation if you need to see it.]",
                path.display()
            ));
        }

        if ext == "pdf" {
            return ToolResult::success(format!(
                "[PDF file: {}. Read does not extract PDF content, so you have not seen \
                 it. If a PDF-to-text tool such as `pdftotext` is installed, run it with \
                 Bash; otherwise ask the user for the text.]",
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

        let start = params.offset.unwrap_or(0).saturating_sub(1);
        let limit = params.limit.unwrap_or(DEFAULT_LINE_LIMIT);
        if limit == 0 {
            return ToolResult::error("limit must be at least 1".to_string());
        }
        let page = match read_page(tokio::fs::File::from_std(file), start, limit).await {
            Ok(page) => page,
            Err(PageError::Binary) => {
                return ToolResult::error(format!(
                    "File appears to be binary and cannot be displayed as text: {}",
                    path.display()
                ));
            }
            Err(PageError::Io(e)) => return ToolResult::error(format!("Failed to read file: {}", e)),
        };

        if page.total_lines == 0 {
            return ToolResult::success(format!(
                "[File {} exists but is empty]",
                path.display()
            ));
        }

        if start >= page.total_lines {
            return ToolResult::error(format!(
                "Offset {} exceeds total line count {} in {}",
                start + 1,
                page.total_lines,
                path.display()
            ));
        }

        let end = start + page.lines.len();
        let mut output = String::with_capacity(page.bytes + page.lines.len() * 8);
        let width = format!("{}", end).len();

        for (i, line) in page.lines.iter().enumerate() {
            output.push_str(&format!("{:>width$}\t{}\n", start + i + 1, line, width = width));
        }

        if end < page.total_lines {
            output.push_str(&format!(
                "\n[truncated: {} more lines; call Read with offset={} to continue]\n",
                page.total_lines - end,
                end + 1
            ));
        }

        ToolResult::success(output)
    }
}

/// `text` cut to [`MAX_LINE_BYTES`] on a char boundary, with a marker giving
/// the full length; borrowed unchanged when it already fits.
pub(crate) fn cut_line(text: &str) -> std::borrow::Cow<'_, str> {
    if text.len() <= MAX_LINE_BYTES {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut cut = MAX_LINE_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    std::borrow::Cow::Owned(format!("{}... [line truncated: {} bytes]", &text[..cut], text.len()))
}

/// One page of a text file: the lines from the requested start that fit the
/// line limit and [`MAX_OUTPUT_BYTES`], plus the file's total line count.
struct Page {
    lines: Vec<String>,
    /// Bytes of text in `lines`, excluding separators.
    bytes: usize,
    total_lines: usize,
}

enum PageError {
    Binary,
    Io(std::io::Error),
}

/// Stream `file` line by line, keeping only lines `start..start + limit` that
/// fit the byte cap; later lines are only counted, so memory stays bounded by
/// the page (and the longest single line) rather than the file. A UTF-8 BOM
/// on the first line is dropped so it is never copied into an edit.
async fn read_page(file: tokio::fs::File, start: usize, limit: usize) -> Result<Page, PageError> {
    let mut reader = tokio::io::BufReader::new(file);
    let mut buf = Vec::new();
    let mut page = Page { lines: Vec::new(), bytes: 0, total_lines: 0 };
    let mut full = false;
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf).await.map_err(PageError::Io)? == 0 {
            break;
        }
        let index = page.total_lines;
        page.total_lines += 1;
        if full || index < start {
            continue;
        }
        let mut raw = buf.as_slice();
        if index == 0 {
            raw = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw);
        }
        raw = raw.strip_suffix(b"\n").unwrap_or(raw);
        raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.contains(&0) {
            return Err(PageError::Binary);
        }
        let text = std::str::from_utf8(raw).map_err(|_| PageError::Binary)?;
        let line = cut_line(text).into_owned();
        if !page.lines.is_empty() && page.bytes + line.len() > MAX_OUTPUT_BYTES {
            full = true;
            continue;
        }
        page.bytes += line.len();
        page.lines.push(line);
        full = page.lines.len() >= limit;
    }
    Ok(page)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{context_with_handler, SwapAfterCheckHandler};
    use claurst_core::config::{PermissionMode, Settings};
    use claurst_core::permissions::PermissionManager;
    use std::sync::Arc;
    use crate::test_support::allow_all_context;

    async fn read(path: &std::path::Path, extra: Value) -> ToolResult {
        let mut input = json!({ "file_path": path.to_string_lossy() });
        if let (Some(obj), Some(more)) = (input.as_object_mut(), extra.as_object()) {
            obj.extend(more.clone());
        }
        let dir = path.parent().unwrap().to_path_buf();
        FileReadTool.execute(input, &allow_all_context(dir)).await
    }

    /// #443: a file longer than the default line limit is cut with a trailer
    /// naming the offset to continue from, and that offset resumes exactly.
    #[tokio::test]
    async fn read_caps_lines_and_continues_from_trailer_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.txt");
        let text: String = (1..=2500).map(|i| format!("line {}\n", i)).collect();
        std::fs::write(&path, text).unwrap();

        let first = read(&path, json!({})).await;
        assert!(!first.is_error, "{}", first.content);
        assert!(first.content.contains("2000\tline 2000\n"));
        assert!(!first.content.contains("line 2001"));
        assert!(
            first.content.contains("[truncated: 500 more lines; call Read with offset=2001 to continue]"),
            "missing trailer: {}",
            &first.content[first.content.len().saturating_sub(200)..]
        );

        let rest = read(&path, json!({ "offset": 2001 })).await;
        assert!(!rest.is_error, "{}", rest.content);
        assert!(rest.content.starts_with("2001\tline 2001\n"), "{}", &rest.content[..40]);
        assert!(rest.content.contains("2500\tline 2500\n"));
        assert!(!rest.content.contains("truncated"), "last page must not have a trailer");
    }

    /// #443: the byte cap applies before the line limit, and over-long lines
    /// are cut rather than returned whole.
    #[tokio::test]
    async fn read_caps_bytes_and_long_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.txt");
        let wide = "x".repeat(1000);
        let mut text: String = (0..200).map(|_| format!("{}\n", wide)).collect();
        text.push_str(&"y".repeat(10_000));
        text.push('\n');
        std::fs::write(&path, text).unwrap();

        let res = read(&path, json!({})).await;
        assert!(!res.is_error, "{}", res.content);
        assert!(res.content.len() < MAX_OUTPUT_BYTES + 4096, "output not capped: {}", res.content.len());
        let shown = res.content.lines().filter(|l| l.contains("\tx")).count();
        assert!(shown < 200 && shown > 0, "shown {} lines", shown);
        assert!(
            res.content.contains(&format!("call Read with offset={} to continue]", shown + 1)),
            "trailer: {}",
            &res.content[res.content.len().saturating_sub(120)..]
        );

        let last = read(&path, json!({ "offset": 201 })).await;
        assert!(last.content.contains("[line truncated: 10000 bytes]"), "{}", last.content.len());
        assert!(last.content.len() < 2200);
    }

    /// #443: images and PDFs return an honest note, not a claim that the
    /// content was captured.
    #[tokio::test]
    async fn read_image_and_pdf_placeholders_are_honest() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        let pdf = dir.path().join("a.pdf");
        std::fs::write(&png, [0x89, b'P', b'N', b'G']).unwrap();
        std::fs::write(&pdf, b"%PDF-1.4").unwrap();

        let img = read(&png, json!({})).await;
        assert!(img.content.contains("you have not seen it"), "{}", img.content);
        assert!(!img.content.contains("captured"), "{}", img.content);
        let doc = read(&pdf, json!({})).await;
        assert!(doc.content.contains("you have not seen"), "{}", doc.content);
        assert!(!doc.content.contains("`pages`"), "{}", doc.content);
    }

    /// `limit: 0` is rejected rather than silently read as 1.
    #[tokio::test]
    async fn read_rejects_zero_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let res = read(&path, json!({ "limit": 0 })).await;
        assert!(res.is_error && res.content.contains("limit must be at least 1"), "{}", res.content);
    }

    /// A UTF-8 BOM is not shown as part of the first line.
    #[tokio::test]
    async fn read_drops_utf8_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom.txt");
        std::fs::write(&path, "\u{feff}first\nsecond\n").unwrap();
        let res = read(&path, json!({})).await;
        assert!(res.content.starts_with("1\tfirst\n"), "{:?}", res.content);
    }

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
