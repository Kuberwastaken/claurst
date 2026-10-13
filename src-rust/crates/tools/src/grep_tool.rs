// Grep tool: content search with ripgrep-style options.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use tracing::debug;
use walkdir::WalkDir;

pub struct GrepTool;

#[derive(Debug, Deserialize)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default, rename = "type")]
    file_type: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default = "default_output_mode")]
    output_mode: String,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default, rename = "-i")]
    case_insensitive: bool,
    #[serde(default, rename = "-n")]
    show_line_numbers: Option<bool>,
    #[serde(default)]
    head_limit: Option<usize>,
    #[serde(default)]
    multiline: bool,
}

fn default_output_mode() -> String {
    "files_with_matches".to_string()
}

/// Map file type shorthand to extensions (similar to ripgrep --type).
fn extensions_for_type(t: &str) -> Vec<&'static str> {
    match t {
        "rust" | "rs" => vec!["rs"],
        "js" => vec!["js", "jsx", "mjs", "cjs"],
        "ts" => vec!["ts", "tsx", "mts", "cts"],
        "py" | "python" => vec!["py", "pyi"],
        "go" => vec!["go"],
        "java" => vec!["java"],
        "c" => vec!["c", "h"],
        "cpp" => vec!["cpp", "hpp", "cc", "hh", "cxx"],
        "rb" | "ruby" => vec!["rb"],
        "php" => vec!["php"],
        "swift" => vec!["swift"],
        "kt" | "kotlin" => vec!["kt", "kts"],
        "css" => vec!["css", "scss", "sass", "less"],
        "html" => vec!["html", "htm"],
        "json" => vec!["json"],
        "yaml" | "yml" => vec!["yaml", "yml"],
        "toml" => vec!["toml"],
        "xml" => vec!["xml"],
        "md" | "markdown" => vec!["md", "markdown"],
        "sh" | "shell" | "bash" => vec!["sh", "bash", "zsh"],
        _ => vec![],
    }
}

#[async_trait]
impl Tool for GrepTool {
    // Gates itself: calls `ctx.check_permission_for_path` in `execute()` (#210).
    fn self_gates(&self) -> bool { true }

    fn name(&self) -> &str {
        claurst_core::constants::TOOL_NAME_GREP
    }

    fn description(&self) -> &str {
        "A powerful search tool built on regex. Supports full regex syntax. \
         Filter files with the `glob` parameter or `type` parameter. Output \
         modes: \"content\" shows matching lines, \"files_with_matches\" shows \
         only file paths (default), \"count\" shows match counts."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The regular expression pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in. Defaults to working directory."
                },
                "type": {
                    "type": "string",
                    "description": "File type to search (e.g. js, py, rust, go)"
                },
                "glob": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. \"*.js\")"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode (default: files_with_matches)"
                },
                "context": {
                    "type": "number",
                    "description": "Number of context lines before and after each match"
                },
                "-i": {
                    "type": "boolean",
                    "description": "Case insensitive search"
                },
                "-n": {
                    "type": "boolean",
                    "description": "Show line numbers (for content mode)"
                },
                "head_limit": {
                    "type": "number",
                    "description": "Limit output to first N entries (default 250)"
                },
                "multiline": {
                    "type": "boolean",
                    "description": "Enable multiline mode where . matches newlines"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: GrepInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let search_path = params
            .path
            .as_ref()
            .map(|p| ctx.resolve_path(p))
            .unwrap_or_else(|| ctx.working_dir.clone());

        let grant = match ctx.check_permission_for_path(
            self.name(),
            &format!("Grep {} in {}", params.pattern, search_path.display()),
            search_path.clone(),
            true,
        ) {
            Ok(grant) => grant,
            Err(e) => return ToolResult::error(e.to_string()),
        };

        debug!(pattern = %params.pattern, path = %search_path.display(), "Running grep");

        // Compile regex
        let regex = match RegexBuilder::new(&params.pattern)
            .case_insensitive(params.case_insensitive)
            .dot_matches_new_line(params.multiline)
            .multi_line(params.multiline)
            .build()
        {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("Invalid regex: {}", e)),
        };

        let head_limit = params.head_limit.unwrap_or(250);
        let context_lines = params.context.unwrap_or(0);
        let show_line_numbers = params.show_line_numbers.unwrap_or(true);

        // Collect candidate file extensions
        let type_exts: Vec<&str> = params
            .file_type
            .as_deref()
            .map(extensions_for_type)
            .unwrap_or_default();

        // Build glob matcher for filtering
        let glob_pattern = params.glob.as_deref();

        // If the search path is a single file, just search it, reading through
        // a handle whose real location has been re-checked (#415).
        if search_path.is_file() {
            let file = match crate::open_regular_file(&search_path) {
                Ok(f) => f,
                Err(e) => {
                    return ToolResult::error(format!("Failed to read {}: {}", search_path.display(), e))
                }
            };
            if let Err(e) =
                ctx.confirm_opened_path(self.name(), "Grep", &search_path, &file, grant, true)
            {
                return ToolResult::error(e);
            }
            let mut content = String::new();
            if let Err(e) = (&file).read_to_string(&mut content) {
                return ToolResult::error(format!("Failed to read {}: {}", search_path.display(), e));
            }
            return self.search_file(
                &search_path,
                &content,
                &regex,
                &params.output_mode,
                context_lines,
                show_line_numbers,
            );
        }

        // Walk directory tree
        let roots = ctx.workspace_roots();
        let mut results: Vec<String> = Vec::new();
        let mut match_count = 0usize;

        for entry in WalkDir::new(&search_path)
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| {
                // Skip hidden directories
                let name = e.file_name().to_string_lossy();
                !name.starts_with('.')
                    && name != "node_modules"
                    && name != "target"
                    && name != "__pycache__"
                    && name != ".git"
            })
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();

            // Type filter
            if !type_exts.is_empty() {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                if !type_exts.contains(&ext) {
                    continue;
                }
            }

            // Glob filter
            if let Some(pattern) = glob_pattern {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if let Ok(m) = glob::Pattern::new(pattern) {
                    if !m.matches(name) {
                        continue;
                    }
                }
            }

            // Only now open, then fence the handle's real location: a file that
            // resolves outside the workspace asks, as before, and a symlink
            // swapped in after this check cannot redirect the read (#415).
            let file = match ctx.open_walked_file(self.name(), "Grep result", path, &roots) {
                Ok(Some(file)) => file,
                Ok(None) => continue,
                Err(e) => return ToolResult::error(e),
            };

            // Read file (skip binary)
            let mut content = String::new();
            if (&file).read_to_string(&mut content).is_err() {
                continue;
            }

            let lines: Vec<&str> = content.lines().collect();
            let mut file_matches: Vec<(usize, &str)> = Vec::new();

            for (i, line) in lines.iter().enumerate() {
                if regex.is_match(line) {
                    file_matches.push((i, line));
                }
            }

            if file_matches.is_empty() {
                continue;
            }

            match params.output_mode.as_str() {
                "files_with_matches" => {
                    results.push(path.display().to_string());
                    match_count += 1;
                }
                "count" => {
                    results.push(format!("{}:{}", path.display(), file_matches.len()));
                    match_count += 1;
                }
                "content" => {
                    for (line_idx, _) in &file_matches {
                        let start = line_idx.saturating_sub(context_lines);
                        let end = (*line_idx + context_lines + 1).min(lines.len());

                        for (ci, line) in lines.iter().enumerate().take(end).skip(start) {
                            let prefix = if show_line_numbers {
                                format!("{}:{}:", path.display(), ci + 1)
                            } else {
                                format!("{}:", path.display())
                            };
                            results.push(format!("{}{}", prefix, line));
                        }

                        if context_lines > 0 {
                            results.push("--".to_string());
                        }

                        match_count += 1;
                    }
                }
                _ => {
                    results.push(path.display().to_string());
                    match_count += 1;
                }
            }

            if match_count >= head_limit {
                break;
            }
        }

        if results.is_empty() {
            return ToolResult::success(format!(
                "No matches found for pattern \"{}\" in {}",
                params.pattern,
                search_path.display()
            ));
        }

        let output = results.join("\n");
        ToolResult::success(output)
    }
}

impl GrepTool {
    fn search_file(
        &self,
        path: &Path,
        content: &str,
        regex: &regex::Regex,
        output_mode: &str,
        context_lines: usize,
        show_line_numbers: bool,
    ) -> ToolResult {
        let lines: Vec<&str> = content.lines().collect();
        let mut matching_lines: Vec<usize> = Vec::new();

        for (i, line) in lines.iter().enumerate() {
            if regex.is_match(line) {
                matching_lines.push(i);
            }
        }

        if matching_lines.is_empty() {
            return ToolResult::success(format!(
                "No matches found in {}",
                path.display()
            ));
        }

        match output_mode {
            "files_with_matches" => ToolResult::success(path.display().to_string()),
            "count" => ToolResult::success(format!(
                "{}:{}",
                path.display(),
                matching_lines.len()
            )),
            _ => {
                let mut results = Vec::new();
                for line_idx in &matching_lines {
                    let start = line_idx.saturating_sub(context_lines);
                    let end = (*line_idx + context_lines + 1).min(lines.len());
                    for (ci, line) in lines.iter().enumerate().take(end).skip(start) {
                        if show_line_numbers {
                            results.push(format!("{}:{}", ci + 1, line));
                        } else {
                            results.push(line.to_string());
                        }
                    }
                    if context_lines > 0 {
                        results.push("--".to_string());
                    }
                }
                ToolResult::success(results.join("\n"))
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{context_with_handler, SwapAfterCheckHandler};
    use claurst_core::config::{PermissionMode, Settings};
    use claurst_core::permissions::PermissionManager;
    use std::sync::Arc;

    /// #415: a single in-workspace file swapped for a symlink to a host file
    /// right after the fence allowed it must not be searched without a prompt.
    #[tokio::test]
    async fn grep_file_refuses_symlink_swapped_in_after_fence() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ws = root.join("ws");
        let outside = root.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "TOP-SECRET\n").unwrap();
        let notes = ws.join("notes.txt");
        std::fs::write(&notes, "inside\n").unwrap();

        let manager = PermissionManager::new(PermissionMode::Default, &Settings::default());
        let handler = SwapAfterCheckHandler::new(manager, Some((notes.clone(), secret)));
        let ctx = context_with_handler(ws, Arc::new(handler));

        let res = GrepTool
            .execute(
                json!({ "pattern": "SECRET", "path": notes.to_string_lossy(), "output_mode": "content" }),
                &ctx,
            )
            .await;
        assert!(res.is_error, "swapped grep was allowed: {}", res.content);
        assert!(!res.content.contains("TOP-SECRET"), "leaked: {}", res.content);
    }

    /// A walk still searches in-workspace files and asks (here: denies) for a
    /// symlink that leads outside the workspace.
    #[tokio::test]
    async fn grep_walk_fences_symlink_to_outside() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ws = root.join("ws");
        let outside = root.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "needle TOP-SECRET\n").unwrap();
        std::fs::write(ws.join("a.txt"), "needle inside\n").unwrap();

        let manager = PermissionManager::new(PermissionMode::Default, &Settings::default());
        let ctx = context_with_handler(
            ws.clone(),
            Arc::new(SwapAfterCheckHandler::new(manager, None)),
        );
        let res = GrepTool
            .execute(json!({ "pattern": "needle", "output_mode": "content" }), &ctx)
            .await;
        assert!(!res.is_error, "grep failed: {}", res.content);
        assert!(res.content.contains("needle inside"), "missing match: {}", res.content);

        std::os::unix::fs::symlink(outside.join("secret.txt"), ws.join("b.txt")).unwrap();
        let res = GrepTool
            .execute(json!({ "pattern": "needle", "output_mode": "content" }), &ctx)
            .await;
        assert!(res.is_error, "outside symlink was searched: {}", res.content);
        assert!(!res.content.contains("TOP-SECRET"), "leaked: {}", res.content);
    }
}
