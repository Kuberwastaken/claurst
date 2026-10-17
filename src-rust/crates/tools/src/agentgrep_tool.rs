// AgentGrep: structure-aware code search with grep, find, outline, and trace
// modes (port of jcode's agentgrep tool, adapted to claurst's tool idioms).

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use tracing::debug;
use walkdir::WalkDir;

pub struct AgentGrepTool;

/// Default cap on rendered grep regions. Generous enough for ordinary code
/// searches while bounding a common string inside large data files. The
/// header reports the true total so a caller can raise the cap deliberately.
const DEFAULT_GREP_MAX_REGIONS: usize = 200;

/// Default caps for find and trace, mirroring jcode's defaults.
const DEFAULT_FIND_MAX_FILES: usize = 10;
const DEFAULT_TRACE_MAX_FILES: usize = 5;
const DEFAULT_TRACE_MAX_REGIONS: usize = 6;

/// Hard ceiling on one match excerpt before it is trimmed with an ellipsis.
const REGION_EXCERPT_LIMIT: usize = 400;

#[derive(Debug, Deserialize)]
struct AgentGrepInput {
    #[serde(default)]
    intent: Option<String>,
    #[serde(default = "default_mode")]
    mode: String,
    // `pattern` accepted because agents frequently pass it instead of `query`.
    #[serde(default, alias = "pattern")]
    query: Option<String>,
    // `file_path` accepted for the same reason.
    #[serde(default, alias = "file_path")]
    file: Option<String>,
    #[serde(default)]
    terms: Option<Vec<String>>,
    #[serde(default)]
    regex: Option<bool>,
    #[serde(default)]
    path: Option<String>,
    // `include` accepted for legacy grep-tool style calls.
    #[serde(default, alias = "include")]
    glob: Option<String>,
    #[serde(default, rename = "type")]
    file_type: Option<String>,
    #[serde(default)]
    hidden: Option<bool>,
    #[serde(default)]
    no_ignore: Option<bool>,
    #[serde(default)]
    max_files: Option<usize>,
    #[serde(default)]
    max_regions: Option<usize>,
    #[serde(default)]
    paths_only: Option<bool>,
}

fn default_mode() -> String {
    "grep".to_string()
}

/// One symbol extracted for outline mode.
struct OutlineItem {
    line: usize,
    kind: &'static str,
    name: String,
}

/// One matched region for grep and trace modes.
struct Region {
    start_line: usize,
    end_line: usize,
}

#[allow(dead_code)]
impl Region {
    fn is_multiline(&self) -> bool {
        self.end_line > self.start_line
    }
}

#[async_trait]
impl Tool for AgentGrepTool {
    // Gates itself: calls `ctx.check_permission_for_path` in `execute()`.
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "AgentGrep"
    }

    fn description(&self) -> &str {
        "Search code and file names. Defaults to grep mode when mode is omitted."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": {
                    "type": "string",
                    "description": "Required short label shown in the UI: why this call is being made."
                },
                "mode": {
                    "type": "string",
                    "enum": ["grep", "find", "outline", "trace"],
                    "description": "Mode: grep (default), find (file names), outline (one file), trace (relationship DSL)."
                },
                "query": {
                    "type": "string",
                    "description": "Search query. Required for grep (literal unless regex=true); optional ranking terms for find."
                },
                "file": {
                    "type": "string",
                    "description": "Single file to inspect. Required for outline."
                },
                "terms": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Trace DSL terms, e.g. [\"subject:auth_status\", \"relation:rendered\"]. Not for grep/find; use query."
                },
                "regex": {
                    "type": "boolean",
                    "description": "In grep mode, treat query as a regex. Defaults to false (literal)."
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search, relative to the workspace. Omit to search the whole workspace."
                },
                "glob": {
                    "type": "string",
                    "description": "Optional file glob filter such as **/*.rs. Omit to search everything."
                },
                "type": {
                    "type": "string",
                    "description": "Optional file type filter, such as rs, py, js, ts, or md."
                },
                "max_files": {
                    "type": "integer",
                    "description": "Maximum number of files to return for find/trace-style modes."
                },
                "max_regions": {
                    "type": "integer",
                    "description": "Maximum number of matching regions to return."
                },
                "paths_only": {
                    "type": "boolean",
                    "description": "Return only matching paths instead of match excerpts where supported."
                }
            },
            "required": ["intent"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: AgentGrepInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        if params
            .intent
            .as_deref()
            .is_none_or(|intent| intent.trim().is_empty())
        {
            return ToolResult::error(
                "AgentGrep requires an 'intent' label explaining why the call is being made.",
            );
        }

        // The whole search is blocking file IO with no async yield points.
        // Offload it to the blocking pool so a poll of this future cannot
        // stall a tokio worker thread.
        let params = std::sync::Arc::new(params);
        let ctx2 = ctx.clone();
        let handle = tokio::task::spawn_blocking(move || run_agentgrep(&params, &ctx2));
        match handle.await {
            Ok(result) => result,
            Err(e) => ToolResult::error(format!("AgentGrep task failed: {e}")),
        }
    }
}

fn run_agentgrep(params: &AgentGrepInput, ctx: &ToolContext) -> ToolResult {
    match params.mode.as_str() {
        "grep" => run_grep_mode(params, ctx),
        "find" => run_find_mode(params, ctx),
        "outline" => run_outline_mode(params, ctx),
        "trace" => run_trace_mode(params, ctx),
        other => ToolResult::error(format!(
            "Unsupported AgentGrep mode: {other}. Use grep, find, outline, or trace."
        )),
    }
}

/// Scope a search: `file` scopes to one exact file when `path` is absent, and
/// a file-valued `path` scopes to that file's parent directory plus its name.
fn resolved_search_scope(
    ctx: &ToolContext,
    path: Option<&str>,
    file: Option<&str>,
    glob: Option<&str>,
) -> (Option<PathBuf>, Option<String>) {
    let path = path.or(file);
    let Some(path) = path else {
        return (None, normalized_glob(glob).map(ToOwned::to_owned));
    };
    let resolved = ctx.resolve_path(path);
    if resolved.is_file() {
        let root = resolved
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let glob = resolved
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        return (Some(root), glob);
    }
    (Some(resolved), normalized_glob(glob).map(ToOwned::to_owned))
}

/// Treat match-all globs as absent so they never narrow a search.
fn normalized_glob(glob: Option<&str>) -> Option<&str> {
    let glob = glob?.trim();
    if glob.is_empty() || matches!(glob, "*" | "**" | "**/*" | "./*" | "./**" | "./**/*") {
        return None;
    }
    Some(glob)
}

fn search_root(ctx: &ToolContext, path: Option<PathBuf>) -> PathBuf {
    path.unwrap_or_else(|| ctx.working_dir.clone())
}

fn extensions_for_type(file_type: &str) -> Vec<&'static str> {
    match file_type {
        "rust" | "rs" => vec!["rs"],
        "js" => vec!["js", "jsx", "mjs", "cjs"],
        "ts" => vec!["ts", "tsx"],
        "py" | "python" => vec!["py", "pyi"],
        "go" => vec!["go"],
        "java" => vec!["java"],
        "c" => vec!["c", "h"],
        "cpp" => vec!["cpp", "hpp", "cc"],
        "rb" | "ruby" => vec!["rb"],
        "swift" => vec!["swift"],
        "kt" | "kotlin" => vec!["kt", "kts"],
        "css" => vec!["css", "scss"],
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

/// Walk filters shared by every scoped mode. `max_files` bounds how many
/// matching files are collected; `usize::MAX` walks fully when output is
/// capped elsewhere (region caps).
#[derive(Debug)]
struct WalkFilters<'a> {
    glob: Option<&'a str>,
    file_type: Option<&'a str>,
    hidden: bool,
    no_ignore: bool,
    max_files: usize,
}

impl WalkFilters<'_> {
    fn from_params<'a>(
        params: &'a AgentGrepInput,
        glob: Option<&'a str>,
        max_files: usize,
    ) -> WalkFilters<'a> {
        WalkFilters {
            glob: glob.or_else(|| normalized_glob(params.glob.as_deref())),
            file_type: params.file_type.as_deref(),
            hidden: params.hidden.unwrap_or(false),
            no_ignore: params.no_ignore.unwrap_or(false),
            max_files,
        }
    }
}

/// Candidate files for a scoped walk, respecting hidden/no_ignore/glob/type
/// filters, permission fenced through `open_walked_file` (#415).
fn collect_files(
    ctx: &ToolContext,
    tool_name: &str,
    root: &Path,
    filters: &WalkFilters<'_>,
) -> Result<Vec<PathBuf>, String> {
    let roots = ctx.workspace_roots();
    let type_exts = filters.file_type.map(extensions_for_type).unwrap_or_default();
    let mut files = Vec::new();
    let walker = WalkDir::new(root).follow_links(true).into_iter();
    let it: Box<dyn Iterator<Item = walkdir::Result<walkdir::DirEntry>>> = if !filters.hidden {
        Box::new(walker.filter_entry(|e: &walkdir::DirEntry| {
            // The walk root itself always passes: a root the caller named
            // deliberately (even a dot-prefixed temp dir) is in scope, and
            // dropping it would silently skip the whole subtree.
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !name.starts_with('.')
                && name != "node_modules"
                && name != "target"
                && name != "__pycache__"
                && name != ".git"
        }))
    } else {
        Box::new(walker)
    };
    let mut ignore_patterns = Vec::new();
    if !filters.no_ignore && root.join(".gitignore").is_file() {
        if let Ok(content) = std::fs::read_to_string(root.join(".gitignore")) {
            for line in content.lines() {
                let line = line.trim();
                if !line.is_empty() && !line.starts_with('#') {
                    ignore_patterns.push(line.to_string());
                }
            }
        }
    }

    for entry in it {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !type_exts.is_empty() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !type_exts.contains(&ext) {
                continue;
            }
        }
        if let Some(pattern) = filters.glob {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if let Ok(m) = glob::Pattern::new(pattern) {
                if !m.matches(name) {
                    continue;
                }
            }
        }
        if !ignore_patterns.is_empty() {
            if let Ok(rel) = path.strip_prefix(root) {
                let rel_str = rel.display().to_string();
                if ignore_patterns.iter().any(|p| {
                    if p.contains('/') {
                        rel_str.contains(p.trim_start_matches('/'))
                    } else {
                        rel_str.split('/').any(|segment| {
                            segment == *p
                                || (p.starts_with('*')
                                    && segment.ends_with(p.trim_start_matches('*')))
                        })
                    }
                }) {
                    continue;
                }
            }
        }
        // Only now open, fencing the handle's real location (#415).
        ctx.open_walked_file(tool_name, "AgentGrep result", path, &roots)?;
        files.push(path.to_path_buf());
        if files.len() >= filters.max_files {
            break;
        }
    }
    Ok(files)
}

fn read_file_content(path: &Path) -> Option<String> {
    let mut content = String::new();
    let file = crate::open_regular_file(path).ok()?;
    (&file).read_to_string(&mut content).ok()?;
    Some(content)
}

// ---------------------------------------------------------------------------
// grep mode
// ---------------------------------------------------------------------------

fn run_grep_mode(params: &AgentGrepInput, ctx: &ToolContext) -> ToolResult {
    let Some(query) = params.query.as_deref() else {
        return ToolResult::error("AgentGrep grep requires 'query'");
    };

    let (scope_root, scope_glob) = resolved_search_scope(
        ctx,
        params.path.as_deref(),
        params.file.as_deref(),
        params.glob.as_deref(),
    );
    let root = search_root(ctx, scope_root);

    let grant = match ctx.check_permission_for_path(
        "AgentGrep",
        &format!("AgentGrep {} in {}", query, root.display()),
        root.clone(),
        true,
    ) {
        Ok(g) => g,
        Err(e) => return ToolResult::error(e.to_string()),
    };

    debug!(query = query, path = %root.display(), "Running agentgrep grep");

    let matcher: GrepMatcher = match params.regex.unwrap_or(false) {
        true => match regex::Regex::new(query) {
            Ok(re) => GrepMatcher::Regex(re),
            Err(e) => return ToolResult::error(format!("Invalid regex: {e}")),
        },
        false => GrepMatcher::Literal(query),
    };

    let max_regions = params.max_regions.unwrap_or(DEFAULT_GREP_MAX_REGIONS);

    if root.is_file() {
        let file = match crate::open_regular_file(&root) {
            Ok(f) => f,
            Err(e) => {
                return ToolResult::error(format!("Failed to read {}: {}", root.display(), e));
            }
        };
        if let Err(e) = ctx.confirm_opened_path("AgentGrep", "AgentGrep", &root, &file, grant, true)
        {
            return ToolResult::error(e);
        }
        drop(file);
        let Some(content) = read_file_content(&root) else {
            return ToolResult::error(format!("Failed to read {}", root.display()));
        };
        let (total_matches, rendered) = grep_in_content(
            &content,
            &matcher,
            max_regions,
            params.paths_only.unwrap_or(false),
        );
        return ToolResult::success(format_grep_output(
            query,
            &root,
            total_matches,
            &rendered,
            max_regions,
        ));
    }

    let files = match collect_files(
        ctx,
        "AgentGrep",
        &root,
        &WalkFilters::from_params(
            params,
            scope_glob.as_deref().or_else(|| normalized_glob(params.glob.as_deref())),
            // Walk fully; region cap governs output volume.
            usize::MAX,
        ),
    ) {
        Ok(f) => f,
        Err(e) => return ToolResult::error(e),
    };

    let mut total_matches = 0usize;
    let mut files_with_matches = 0usize;
    let mut rendered: Vec<String> = Vec::new();
    let paths_only = params.paths_only.unwrap_or(false);

    for path in &files {
        let Some(content) = read_file_content(path) else {
            continue;
        };
        let (file_total, file_rendered) =
            grep_in_content(&content, &matcher, max_regions, paths_only);
        if file_total == 0 {
            continue;
        }
        total_matches += file_total;
        files_with_matches += 1;
        if !paths_only {
            for line in file_rendered {
                if rendered.len() < max_regions {
                    rendered.push(format!("{}:{}", path.display(), line));
                }
            }
        } else if rendered.len() < max_regions {
            rendered.push(path.display().to_string());
        }
        if total_matches >= max_regions && !paths_only {
            break;
        }
    }

    if files_with_matches == 0 {
        return ToolResult::success(format!(
            "No matches found for query \"{query}\" in {}",
            root.display()
        ));
    }

    ToolResult::success(format_grep_output(
        query,
        &root,
        total_matches,
        &rendered,
        max_regions,
    ))
}

enum GrepMatcher<'a> {
    Literal(&'a str),
    Regex(regex::Regex),
}

impl GrepMatcher<'_> {
    fn is_match(&self, line: &str) -> bool {
        match self {
            GrepMatcher::Literal(needle) => line.contains(needle),
            GrepMatcher::Regex(re) => re.is_match(line),
        }
    }
}

/// Return (total match count, rendered excerpt lines) for one file's content.
/// Rendering stops at `render_cap` lines so the total stays honest while the
/// payload stays bounded.
fn grep_in_content(
    content: &str,
    matcher: &GrepMatcher,
    render_cap: usize,
    _paths_only: bool,
) -> (usize, Vec<String>) {
    let mut total = 0usize;
    let mut rendered = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if matcher.is_match(line) {
            total += 1;
            if rendered.len() < render_cap {
                let excerpt = if line.len() > REGION_EXCERPT_LIMIT {
                    format!("{}...", &line[..REGION_EXCERPT_LIMIT])
                } else {
                    line.to_string()
                };
                rendered.push(format!("{}:{}", i + 1, excerpt));
            }
        }
    }
    (total, rendered)
}

fn format_grep_output(
    query: &str,
    root: &Path,
    total_matches: usize,
    rendered: &[String],
    max_regions: usize,
) -> String {
    let mut out = format!(
        "query: {}\nmatches: {} in {}\n\n",
        query,
        total_matches,
        root.display()
    );
    if rendered.is_empty() {
        return out;
    }
    out.push_str(&rendered.join("\n"));
    if total_matches > max_regions {
        out.push_str(&format!(
            "\n\n... and {} more matches (raise max_regions to see them)",
            total_matches - max_regions
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// find mode
// ---------------------------------------------------------------------------

fn run_find_mode(params: &AgentGrepInput, ctx: &ToolContext) -> ToolResult {
    let query = params.query.as_deref().unwrap_or_default();
    if query.trim().is_empty()
        && params.path.as_deref().is_none_or(str::is_empty)
        && params.file.as_deref().is_none_or(str::is_empty)
        && normalized_glob(params.glob.as_deref()).is_none()
        && params.file_type.as_deref().is_none_or(str::is_empty)
    {
        return ToolResult::error(
            "AgentGrep find requires 'query' unless path, glob, or type narrows the search",
        );
    }

    let (scope_root, scope_glob) = resolved_search_scope(
        ctx,
        params.path.as_deref(),
        params.file.as_deref(),
        params.glob.as_deref(),
    );
    let root = search_root(ctx, scope_root);

    if let Err(e) = ctx.check_permission_for_path(
        "AgentGrep",
        &format!("AgentGrep find in {}", root.display()),
        root.clone(),
        true,
    ) {
        return ToolResult::error(e.to_string());
    }

    debug!(path = %root.display(), "Running agentgrep find");

    let max_files = params.max_files.unwrap_or(DEFAULT_FIND_MAX_FILES);
    let files = match collect_files(
        ctx,
        "AgentGrep",
        &root,
        &WalkFilters::from_params(
            params,
            scope_glob
                .as_deref()
                .or_else(|| normalized_glob(params.glob.as_deref())),
            max_files,
        ),
    ) {
        Ok(f) => f,
        Err(e) => return ToolResult::error(e),
    };

    // Rank by fuzzy coverage of the query parts in the file name, then by
    // shallow depth (closer to the root wins). Files whose path carries no
    // evidence of the query (full query or any token in the relative path)
    // are dropped first, mirroring agentgrep's has_path_evidence gate.
    let query_lower = query.to_lowercase();
    let query_parts: Vec<String> = query
        .split_whitespace()
        .map(|part| part.to_lowercase())
        .collect();
    let mut ranked: Vec<(usize, usize, &PathBuf)> = files
        .iter()
        .filter(|path| {
            let Ok(rel) = path.strip_prefix(&root) else {
                return false;
            };
            if rel.as_os_str().is_empty() {
                return false;
            }
            let rel_lower = rel.display().to_string().to_lowercase();
            rel_lower.contains(&query_lower)
                || query_parts
                    .iter()
                    .any(|part| !part.is_empty() && rel_lower.contains(part.as_str()))
        })
        .map(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let hits = query_parts
                .iter()
                .filter(|part| !part.is_empty() && name.contains(part.as_str()))
                .count();
            let depth = path.components().count();
            (usize::MAX - hits, depth, path)
        })
        .collect();
    ranked.sort();

    let mut out = format!("Found {} files under {}\n", ranked.len(), root.display());
    for (_, _, path) in ranked.into_iter().take(max_files) {
        out.push_str(&path.display().to_string());
        out.push('\n');
    }
    ToolResult::success(out.trim_end().to_string())
}

// ---------------------------------------------------------------------------
// outline mode
// ---------------------------------------------------------------------------

fn run_outline_mode(params: &AgentGrepInput, ctx: &ToolContext) -> ToolResult {
    // Agents sometimes point `path` at the file itself, either instead of or in
    // addition to `file`. Treat a file-valued `path` as the outline target so
    // the file argument is not joined onto it.
    let target: PathBuf = if let Some(path) = params.path.as_deref() {
        let resolved = ctx.resolve_path(path);
        if resolved.is_file() {
            resolved
        } else {
            outline_file_arg(params, ctx)
        }
    } else {
        outline_file_arg(params, ctx)
    };

    let grant = match ctx.check_permission_for_path(
        "AgentGrep",
        &format!("AgentGrep outline {}", target.display()),
        target.clone(),
        true,
    ) {
        Ok(g) => g,
        Err(e) => return ToolResult::error(e.to_string()),
    };

    let file = match crate::open_regular_file(&target) {
        Ok(f) => f,
        Err(e) => {
            return ToolResult::error(format!("Failed to open {}: {}", target.display(), e));
        }
    };
    if let Err(e) = ctx.confirm_opened_path(
        "AgentGrep",
        "AgentGrep outline",
        &target,
        &file,
        grant,
        true,
    ) {
        return ToolResult::error(e);
    }
    drop(file);

    let Some(content) = read_file_content(&target) else {
        return ToolResult::error(format!(
            "Failed to read {}: not valid UTF-8 or unreadable",
            target.display()
        ));
    };

    let items = extract_outline(&content);
    if items.is_empty() {
        return ToolResult::success(format!("No outline items found in {}", target.display()));
    }

    let mut out = format!("Outline of {}\n", target.display());
    for item in items {
        out.push_str(&format!(
            "  {:>4}  {} {}\n",
            item.line, item.kind, item.name
        ));
    }
    ToolResult::success(out.trim_end().to_string())
}

fn outline_file_arg(params: &AgentGrepInput, ctx: &ToolContext) -> PathBuf {
    params
        .file
        .as_deref()
        .or(params.query.as_deref())
        .or_else(|| {
            params
                .terms
                .as_ref()
                .and_then(|terms| terms.first().map(String::as_str))
        })
        .map(|file| ctx.resolve_path(file))
        .unwrap_or_else(|| ctx.working_dir.clone())
}

/// Extract top-level symbol declarations with line numbers. Language-aware
/// for Rust, with a generic fallback for other languages (function/class/def
/// shaped lines).
fn extract_outline(content: &str) -> Vec<OutlineItem> {
    let mut items = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        let (kind, name) = if let Some(rest) = trimmed.strip_prefix("pub ") {
            outline_item_after_vis(rest, i, &mut items)
        } else {
            outline_item_after_vis(trimmed, i, &mut items)
        };
        if let Some(name) = name {
            items.push(OutlineItem {
                line: i + 1,
                kind: kind.unwrap_or("item"),
                name,
            });
        }
    }
    items
}

fn outline_item_after_vis(
    trimmed: &str,
    _i: usize,
    _items: &mut Vec<OutlineItem>,
) -> (Option<&'static str>, Option<String>) {
    let mut rest = trimmed;
    while let Some(stripped) = rest
        .strip_prefix("pub ")
        .or_else(|| rest.strip_prefix("async "))
    {
        rest = stripped;
    }
    if let Some(rest) = rest.strip_prefix("fn ") {
        let name = identifier_of(rest);
        return (Some("function"), name.map(ToOwned::to_owned));
    }
    if let Some(rest) = rest.strip_prefix("struct ") {
        let name = identifier_of(rest);
        return (Some("struct"), name.map(ToOwned::to_owned));
    }
    if let Some(rest) = rest.strip_prefix("enum ") {
        let name = identifier_of(rest);
        return (Some("enum"), name.map(ToOwned::to_owned));
    }
    if let Some(rest) = rest.strip_prefix("trait ") {
        let name = identifier_of(rest);
        return (Some("trait"), name.map(ToOwned::to_owned));
    }
    if let Some(rest) = rest.strip_prefix("impl ") {
        let name = rest
            .split(['<', '{', ' '])
            .next();
        let name = name.filter(|n| !n.is_empty());
        return (Some("impl"), name.map(ToOwned::to_owned));
    }
    if let Some(rest) = rest.strip_prefix("mod ") {
        let name = identifier_of(rest);
        return (Some("module"), name.map(ToOwned::to_owned));
    }
    // Generic fallback for other languages: def, class, function.
    if let Some(rest) = rest.strip_prefix("def ") {
        let name = identifier_of(rest);
        return (Some("function"), name.map(ToOwned::to_owned));
    }
    if let Some(rest) = rest.strip_prefix("class ") {
        let name = identifier_of(rest);
        return (Some("class"), name.map(ToOwned::to_owned));
    }
    if let Some(rest) = rest.strip_prefix("function ") {
        let name = identifier_of(rest);
        return (Some("function"), name.map(ToOwned::to_owned));
    }
    (None, None)
}

fn identifier_of(rest: &str) -> Option<&str> {
    let end = rest.find(['(', '<', '{', ':', ' ']).unwrap_or(rest.len());
    let name = &rest[..end];
    (!name.is_empty()).then_some(name)
}

// ---------------------------------------------------------------------------
// trace mode
// ---------------------------------------------------------------------------

fn run_trace_mode(params: &AgentGrepInput, ctx: &ToolContext) -> ToolResult {
    let terms = match trace_terms(params) {
        Ok(terms) => terms,
        Err(e) => return ToolResult::error(e),
    };

    let parsed = match parse_trace_terms(&terms) {
        Ok(parsed) => parsed,
        Err(e) => return ToolResult::error(e),
    };

    let (scope_root, scope_glob) = resolved_search_scope(
        ctx,
        params.path.as_deref(),
        params.file.as_deref(),
        params.glob.as_deref(),
    );
    let root = search_root(ctx, scope_root);

    if let Err(e) = ctx.check_permission_for_path(
        "AgentGrep",
        &format!("AgentGrep trace in {}", root.display()),
        root.clone(),
        true,
    ) {
        return ToolResult::error(e.to_string());
    }

    debug!(path = %root.display(), "Running agentgrep trace");

    let max_files = params.max_files.unwrap_or(DEFAULT_TRACE_MAX_FILES);
    let max_regions = params.max_regions.unwrap_or(DEFAULT_TRACE_MAX_REGIONS);

    let files = match collect_files(
        ctx,
        "AgentGrep",
        &root,
        &WalkFilters::from_params(
            params,
            scope_glob
                .as_deref()
                .or_else(|| normalized_glob(params.glob.as_deref())),
            // Walk fully; region and file caps govern output volume.
            usize::MAX,
        ),
    ) {
        Ok(f) => f,
        Err(e) => return ToolResult::error(e),
    };

    let mut total_regions = 0usize;
    let mut files_matched = Vec::new();
    let mut rendered: Vec<String> = Vec::new();

    for path in &files {
        if files_matched.len() >= max_files {
            break;
        }
        let Some(content) = read_file_content(path) else {
            continue;
        };
        let mut file_regions = 0usize;
        let mut file_lines: Vec<String> = Vec::new();
        let lines: Vec<&str> = content.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if trace_line_matches(&parsed, line) {
                file_regions += 1;
                if file_lines.len() < max_regions {
                    file_lines.push(format!("  {}-{}: {}", i + 1, i + 1, trim_excerpt(line)));
                }
            }
        }
        if file_regions == 0 {
            continue;
        }
        total_regions += file_regions;
        files_matched.push(path.display().to_string());
        if !params.paths_only.unwrap_or(false) {
            rendered.push(format!("{}\n{}", path.display(), file_lines.join("\n")));
        }
    }

    let mut out = format!(
        "query: {}\nmatches: {} in {} files\n",
        terms.join(" "),
        total_regions,
        files_matched.len()
    );
    if params.paths_only.unwrap_or(false) {
        for path in &files_matched {
            out.push_str(path);
            out.push('\n');
        }
    } else {
        for block in &rendered {
            out.push('\n');
            out.push_str(block);
        }
    }
    ToolResult::success(out.trim_end().to_string())
}

fn trim_excerpt(line: &str) -> String {
    if line.len() > REGION_EXCERPT_LIMIT {
        format!("{}...", &line[..REGION_EXCERPT_LIMIT])
    } else {
        line.to_string()
    }
}

fn trace_terms(params: &AgentGrepInput) -> Result<Vec<String>, String> {
    if let Some(terms) = params.terms.as_ref().filter(|terms| !terms.is_empty()) {
        return Ok(terms.clone());
    }
    if let Some(query) = params.query.as_deref() {
        let split_terms: Vec<String> = query
            .split_whitespace()
            .filter(|term| !term.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if !split_terms.is_empty() {
            return Ok(split_terms);
        }
    }
    Err("AgentGrep trace requires non-empty 'terms'".to_string())
}

#[derive(Default)]
struct TraceQuery {
    subjects: Vec<String>,
    relations: Vec<String>,
    supports: Vec<String>,
    plain: Vec<String>,
}

fn parse_trace_terms(terms: &[String]) -> Result<TraceQuery, String> {
    let mut query = TraceQuery::default();
    for term in terms {
        if let Some(value) = term.strip_prefix("subject:") {
            query.subjects.push(value.to_lowercase());
        } else if let Some(value) = term.strip_prefix("relation:") {
            query.relations.push(value.to_lowercase());
        } else if let Some(value) = term.strip_prefix("support:") {
            query.supports.push(value.to_lowercase());
        } else if term.contains(':') {
            return Err(format!(
                "unknown trace field in '{term}'. Use subject:, relation:, or support:"
            ));
        } else {
            query.plain.push(term.to_lowercase());
        }
    }
    Ok(query)
}

fn trace_line_matches(query: &TraceQuery, line: &str) -> bool {
    let lower = line.to_lowercase();
    let subject_ok = query
        .subjects
        .iter()
        .all(|subject| lower.contains(subject.as_str()));
    let relation_ok = query
        .relations
        .iter()
        .all(|relation| lower.contains(relation.as_str()));
    let support_ok = query
        .supports
        .iter()
        .all(|support| lower.contains(support.as_str()));
    let plain_ok = query.plain.iter().all(|term| lower.contains(term.as_str()));
    subject_ok && relation_ok && support_ok && plain_ok
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;
    use serde_json::json;

    fn write_workspace(root: &Path) {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/auth.rs"),
            "pub fn auth_status() -> bool {\n    true\n}\n\nfn render_auth() {\n    // rendered here\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "mod auth;\n\nfn main() {\n    let ok = auth::auth_status();\n    println!(\"{}\", ok);\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("notes.md"),
            "# Notes\nSome prose about nothing.\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".hidden_dir")).unwrap();
        std::fs::write(root.join(".hidden_dir/secret.rs"), "pub fn secret() {}\n").unwrap();
    }

    #[tokio::test]
    async fn grep_mode_literal_and_regex() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        write_workspace(&root);
        let ctx = allow_all_context(root.clone());

        let res = AgentGrepTool
            .execute(
                json!({
                    "intent": "find auth users",
                    "mode": "grep",
                    "query": "auth_status",
                    "path": root.display().to_string()
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "grep failed: {}", res.content);
        assert!(
            res.content.contains("auth.rs:1"),
            "missing match: {}",
            res.content
        );
        assert!(
            res.content.contains("main.rs:4"),
            "missing match: {}",
            res.content
        );

        let res = AgentGrepTool
            .execute(
                json!({
                    "intent": "find render fns",
                    "mode": "grep",
                    "query": "render_[a-z]+",
                    "regex": true,
                    "path": root.display().to_string()
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "grep failed: {}", res.content);
        assert!(
            res.content.contains("render_auth"),
            "missing match: {}",
            res.content
        );
    }

    #[tokio::test]
    async fn grep_mode_missing_query_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = allow_all_context(std::fs::canonicalize(dir.path()).unwrap());
        let res = AgentGrepTool
            .execute(json!({"intent": "why", "mode": "grep"}), &ctx)
            .await;
        assert!(
            res.is_error,
            "grep without query should error: {}",
            res.content
        );
    }

    #[tokio::test]
    async fn intent_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = allow_all_context(std::fs::canonicalize(dir.path()).unwrap());
        let res = AgentGrepTool
            .execute(json!({"mode": "grep", "query": "x"}), &ctx)
            .await;
        assert!(res.is_error, "missing intent should error: {}", res.content);
    }

    #[tokio::test]
    async fn find_mode_ranks_matching_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        write_workspace(&root);
        let ctx = allow_all_context(root.clone());

        let res = AgentGrepTool
            .execute(
                json!({
                    "intent": "locate auth file",
                    "mode": "find",
                    "query": "auth",
                    "path": root.display().to_string()
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "find failed: {}", res.content);
        assert!(
            res.content.contains("auth.rs"),
            "auth.rs not found: {}",
            res.content
        );
        assert!(
            !res.content.contains("main.rs"),
            "main.rs should not rank for 'auth': {}",
            res.content
        );
    }

    #[tokio::test]
    async fn outline_mode_lists_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        write_workspace(&root);
        let ctx = allow_all_context(root.clone());

        let res = AgentGrepTool
            .execute(
                json!({
                    "intent": "see auth structure",
                    "mode": "outline",
                    "file": root.join("src/auth.rs").display().to_string()
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "outline failed: {}", res.content);
        assert!(
            res.content.contains("function auth_status"),
            "missing fn: {}",
            res.content
        );
        assert!(
            res.content.contains("function render_auth"),
            "missing fn: {}",
            res.content
        );
    }

    #[tokio::test]
    async fn trace_mode_matches_dsl_terms() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        write_workspace(&root);
        let ctx = allow_all_context(root.clone());

        let res = AgentGrepTool
            .execute(
                json!({
                    "intent": "trace auth rendering",
                    "mode": "trace",
                    "terms": ["subject:auth_status", "relation:rendered"],
                    "path": root.display().to_string()
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "trace failed: {}", res.content);
        // auth.rs only: auth_status and "rendered here" comment never share a line.
        assert!(
            res.content.contains("matches: 0"),
            "unexpected match: {}",
            res.content
        );
    }

    #[tokio::test]
    async fn trace_mode_plain_terms_match() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        write_workspace(&root);
        let ctx = allow_all_context(root.clone());

        let res = AgentGrepTool
            .execute(
                json!({
                    "intent": "trace auth usage",
                    "mode": "trace",
                    "terms": ["auth_status"],
                    "path": root.display().to_string()
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "trace failed: {}", res.content);
        assert!(
            res.content.contains("auth.rs"),
            "auth.rs missing: {}",
            res.content
        );
    }

    #[tokio::test]
    async fn trace_unknown_field_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = allow_all_context(std::fs::canonicalize(dir.path()).unwrap());
        let res = AgentGrepTool
            .execute(
                json!({
                    "intent": "bad query",
                    "mode": "trace",
                    "terms": ["bogus:x"],
                }),
                &ctx,
            )
            .await;
        assert!(res.is_error, "unknown field should error: {}", res.content);
    }

    #[tokio::test]
    async fn hidden_dirs_skipped_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        write_workspace(&root);
        let ctx = allow_all_context(root.clone());

        let res = AgentGrepTool
            .execute(
                json!({
                    "intent": "check hidden skip",
                    "mode": "grep",
                    "query": "secret",
                    "path": root.display().to_string()
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "grep failed: {}", res.content);
        assert!(
            !res.content.contains("hidden_dir"),
            "hidden dir searched: {}",
            res.content
        );
    }

    #[tokio::test]
    async fn grep_respects_max_regions_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let mut content = String::new();
        for i in 0..50 {
            content.push_str(&format!("line {i} needle\n"));
        }
        std::fs::write(root.join("data.txt"), content).unwrap();
        let ctx = allow_all_context(root.clone());

        let res = AgentGrepTool
            .execute(
                json!({
                    "intent": "bounded search",
                    "mode": "grep",
                    "query": "needle",
                    "max_regions": 5,
                    "path": root.display().to_string()
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "grep failed: {}", res.content);
        assert!(
            res.content.contains("matches: 50"),
            "total lost: {}",
            res.content
        );
        assert!(
            res.content.contains("45 more"),
            "cap hint missing: {}",
            res.content
        );
    }

    #[test]
    fn parse_trace_terms_fields() {
        let parsed = parse_trace_terms(&[
            "subject:auth_status".to_string(),
            "relation:rendered".to_string(),
            "plain".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed.subjects, vec!["auth_status"]);
        assert_eq!(parsed.relations, vec!["rendered"]);
        assert_eq!(parsed.plain, vec!["plain"]);
    }

    #[test]
    fn normalized_glob_folds_match_all() {
        assert_eq!(normalized_glob(Some("**")), None);
        assert_eq!(normalized_glob(Some("  ")), None);
        assert_eq!(normalized_glob(Some("**/*.rs")), Some("**/*.rs"));
    }

    #[test]
    fn identifier_extraction() {
        assert_eq!(identifier_of("foo(bar: T) -> bool {"), Some("foo"));
        assert_eq!(identifier_of("Bar<T> {"), Some("Bar"));
        assert_eq!(identifier_of(""), None);
    }
}
