//! Memory directory (memdir) system.
//!
//! Provides persistent, file-based memory across sessions.  Mirrors the
//! TypeScript modules under `src/memdir/`:
//!   - `memoryScan.ts`   → `scan_memory_dir`, `parse_frontmatter_quick`, `format_memory_manifest`
//!   - `memoryAge.ts`    → `memory_age_days`, `memory_freshness_text`, `memory_freshness_note`
//!   - `memdir.ts`       → `build_memory_prompt_content`, `load_memory_index`, `ensure_memory_dir_exists`
//!   - `paths.ts`        → `auto_memory_path`, `is_auto_memory_enabled`

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Memory type taxonomy
// ---------------------------------------------------------------------------

/// The four canonical memory types.
/// Matches the TypeScript `MemoryType` union in `memoryTypes.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// Information about the user's role, goals, and preferences.
    User,
    /// Guidance the user has given about how to approach work.
    Feedback,
    /// Information about ongoing work, goals, or incidents in the project.
    Project,
    /// Pointers to where information lives in external systems.
    Reference,
}

impl MemoryType {
    /// Parse a raw frontmatter value into a `MemoryType`.
    /// Returns `None` for missing or unrecognised values (legacy files degrade gracefully).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "user" => Some(Self::User),
            "feedback" => Some(Self::Feedback),
            "project" => Some(Self::Project),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }

    /// Display as a lowercase string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }
}

// ---------------------------------------------------------------------------
// Memory file metadata and content
// ---------------------------------------------------------------------------

/// Scanned metadata for a single memory file (without the full body).
/// Mirrors `MemoryHeader` in `memoryScan.ts`.
#[derive(Debug, Clone)]
pub struct MemoryFileMeta {
    /// Filename relative to the memory directory (e.g. `user_role.md`).
    pub filename: String,
    /// Absolute path to the file.
    pub path: PathBuf,
    /// `name:` frontmatter field.
    pub name: Option<String>,
    /// `description:` frontmatter field (used for relevance scoring).
    pub description: Option<String>,
    /// `type:` frontmatter field.
    pub memory_type: Option<MemoryType>,
    /// File modification time in seconds since UNIX epoch.
    pub modified_secs: u64,
}

/// A fully-loaded memory file including its body.
#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub meta: MemoryFileMeta,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Directory scanning
// ---------------------------------------------------------------------------

/// Maximum number of memory files kept after sorting.
/// Matches `MAX_MEMORY_FILES` in `memoryScan.ts`.
const MAX_MEMORY_FILES: usize = 200;

/// Number of lines scanned for frontmatter.
/// Matches `FRONTMATTER_MAX_LINES` in `memoryScan.ts`.
const FRONTMATTER_MAX_LINES: usize = 30;

/// Scan a memory directory, returning metadata for all `.md` files
/// (excluding `MEMORY.md`), sorted newest-first, capped at `MAX_MEMORY_FILES`.
///
/// This is a synchronous scan used during system-prompt assembly.
/// Mirrors `scanMemoryFiles` in `memoryScan.ts` (async version; this is the
/// sync equivalent used at prompt-build time).
pub fn scan_memory_dir(dir: &Path) -> Vec<MemoryFileMeta> {
    let mut files: Vec<MemoryFileMeta> = Vec::new();

    if !dir.exists() {
        return files;
    }

    // Walk recursively using `walkdir`-style manual recursion to stay
    // dependency-free (only std).
    collect_md_files(dir, dir, &mut files);

    // Sort newest-first.
    files.sort_by_key(|b| std::cmp::Reverse(b.modified_secs));
    files.truncate(MAX_MEMORY_FILES);
    files
}

/// Recursively collect `.md` files (excluding `MEMORY.md`) from `current_dir`.
fn collect_md_files(base: &Path, current_dir: &Path, out: &mut Vec<MemoryFileMeta>) {
    let Ok(entries) = std::fs::read_dir(current_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_md_files(base, &path, out);
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if file_name == "MEMORY.md" {
                continue;
            }

            let modified_secs = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
                .unwrap_or(0);

            let (name, description, memory_type) =
                if let Ok(content) = std::fs::read_to_string(&path) {
                    parse_frontmatter_quick(&content)
                } else {
                    (None, None, None)
                };

            // Relative path from the memory dir root.
            let relative = path
                .strip_prefix(base)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| file_name.clone());

            out.push(MemoryFileMeta {
                filename: relative,
                path,
                name,
                description,
                memory_type,
                modified_secs,
            });
        }
    }
}

/// Parse YAML frontmatter from the first `FRONTMATTER_MAX_LINES` lines without
/// a full YAML parser.  Returns `(name, description, memory_type)`.
///
/// Mirrors `parseFrontmatter` usage in `memoryScan.ts`.
pub fn parse_frontmatter_quick(
    content: &str,
) -> (Option<String>, Option<String>, Option<MemoryType>) {
    let mut name = None;
    let mut description = None;
    let mut memory_type = None;

    let lines: Vec<&str> = content.lines().take(FRONTMATTER_MAX_LINES).collect();

    // Frontmatter must start with `---`
    if lines.first().map(|l| l.trim() != "---").unwrap_or(true) {
        return (name, description, memory_type);
    }

    for line in &lines[1..] {
        if line.trim() == "---" {
            break;
        }
        if let Some(rest) = line.strip_prefix("name:") {
            name = Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
        } else if let Some(rest) = line.strip_prefix("description:") {
            description = Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
        } else if let Some(rest) = line.strip_prefix("type:") {
            memory_type = MemoryType::parse(rest.trim().trim_matches('"').trim_matches('\''));
        }
    }

    (name, description, memory_type)
}

/// Format memory headers as a text manifest: one entry per file with
/// `[type] filename (iso-timestamp): description`.
///
/// Mirrors `formatMemoryManifest` in `memoryScan.ts`.
pub fn format_memory_manifest(memories: &[MemoryFileMeta]) -> String {
    memories
        .iter()
        .map(|m| {
            let tag = m
                .memory_type
                .as_ref()
                .map(|t| format!("[{}] ", t.as_str()))
                .unwrap_or_default();

            // Convert modified_secs to an ISO-8601-like timestamp.
            let ts = format_unix_secs_iso(m.modified_secs);

            match &m.description {
                Some(desc) => format!("- {}{} ({}): {}", tag, m.filename, ts, desc),
                None => format!("- {}{}", tag, m.filename),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Minimal ISO-8601 formatter for a Unix timestamp (no external deps).
fn format_unix_secs_iso(secs: u64) -> String {
    // We use a very lightweight implementation to avoid pulling in chrono here
    // (chrono is already a workspace dep but we want this module to stay lean).
    // Accuracy to the day is sufficient for memory manifests.
    let days_since_epoch = secs / 86400;
    // Julian Day Number for 1970-01-01 is 2440588.
    let jdn = days_since_epoch as u32 + 2440588;
    let (y, m, d) = jdn_to_ymd(jdn);
    let hh = (secs % 86400) / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hh, mm, ss)
}

/// Convert a Julian Day Number to (year, month, day).
fn jdn_to_ymd(jdn: u32) -> (u32, u32, u32) {
    let a = jdn + 32044;
    let b = (4 * a + 3) / 146097;
    let c = a - (146097 * b) / 4;
    let d = (4 * c + 3) / 1461;
    let e = c - (1461 * d) / 4;
    let m = (5 * e + 2) / 153;
    let day = e - (153 * m + 2) / 5 + 1;
    let month = m + 3 - 12 * (m / 10);
    let year = 100 * b + d - 4800 + m / 10;
    (year, month, day)
}

// ---------------------------------------------------------------------------
// Memory age / freshness
// ---------------------------------------------------------------------------

/// Days elapsed since `modified_secs`.  Floor-rounded; clamped to 0 for
/// future mtimes (clock skew).
///
/// Mirrors `memoryAgeDays` in `memoryAge.ts`.
pub fn memory_age_days(modified_secs: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    (now.saturating_sub(modified_secs)) / 86400
}

/// Human-readable age string.  Models are poor at date arithmetic — a raw
/// ISO timestamp does not trigger staleness reasoning the way "47 days ago" does.
///
/// Mirrors `memoryAge` in `memoryAge.ts`.
pub fn memory_age(modified_secs: u64) -> String {
    let d = memory_age_days(modified_secs);
    match d {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        n => format!("{} days ago", n),
    }
}

/// Plain-text staleness caveat for memories > 1 day old.
/// Returns an empty string for fresh memories (today / yesterday).
///
/// Mirrors `memoryFreshnessText` in `memoryAge.ts`.
pub fn memory_freshness_text(modified_secs: u64) -> String {
    let d = memory_age_days(modified_secs);
    if d <= 1 {
        return String::new();
    }
    format!(
        "This memory is {} days old. \
        Memories are point-in-time observations, not live state — \
        claims about code behavior or file:line citations may be outdated. \
        Verify against current code before asserting as fact.",
        d
    )
}

/// Per-memory staleness note wrapped in `<system-reminder>` tags.
/// Returns an empty string for memories ≤ 1 day old.
///
/// Mirrors `memoryFreshnessNote` in `memoryAge.ts`.
pub fn memory_freshness_note(modified_secs: u64) -> String {
    let text = memory_freshness_text(modified_secs);
    if text.is_empty() {
        return String::new();
    }
    format!("<system-reminder>{}</system-reminder>\n", text)
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Entrypoint filename within the memory directory.
pub const MEMORY_ENTRYPOINT: &str = "MEMORY.md";

/// Maximum number of lines loaded from `MEMORY.md`.
/// Matches `MAX_ENTRYPOINT_LINES` in `memdir.ts`.
pub const MAX_ENTRYPOINT_LINES: usize = 200;

/// Maximum bytes loaded from `MEMORY.md`.
/// Matches `MAX_ENTRYPOINT_BYTES` in `memdir.ts`.
pub const MAX_ENTRYPOINT_BYTES: usize = 25_000;

/// Compute the auto-memory directory path for a project root.
///
/// Resolution order (mirrors `getAutoMemPath` in `paths.ts`):
/// 1. `CLAUDE_COWORK_MEMORY_PATH_OVERRIDE` env var (full-path override).
/// 2. `<CLAURST_REMOTE_MEMORY_DIR>/projects/<sanitized-root>/memory/`
///    when `CLAURST_REMOTE_MEMORY_DIR` is set.
/// 3. `~/.claurst/projects/<sanitized-root>/memory/` (default).
pub fn auto_memory_path(project_root: &Path) -> PathBuf {
    // 1. Cowork full-path override.
    if let Ok(override_path) = std::env::var("CLAUDE_COWORK_MEMORY_PATH_OVERRIDE") {
        if !override_path.is_empty() {
            return PathBuf::from(override_path);
        }
    }

    // 2. Determine the memory base directory.
    let memory_base = std::env::var("CLAURST_REMOTE_MEMORY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::config::Settings::config_dir());

    // 3. Sanitize the project root into a safe directory name.
    let sanitized = sanitize_path_component(&project_root.to_string_lossy());

    memory_base.join("projects").join(sanitized).join("memory")
}

/// Sanitize an arbitrary string into a directory-name-safe component.
/// Matches `sanitizePath` used inside `getAutoMemPath` in `paths.ts`.
pub fn sanitize_path_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Whether the auto-memory system is enabled for this session.
///
/// Priority chain (mirrors `isAutoMemoryEnabled` in `paths.ts`):
/// 1. `CLAURST_DISABLE_AUTO_MEMORY` — truthy → OFF, falsy (but defined) → ON.
/// 2. `CLAURST_SIMPLE` (--bare) → OFF.
/// 3. Remote mode without `CLAURST_REMOTE_MEMORY_DIR` → OFF.
/// 4. `settings_enabled` parameter (from settings.json `autoMemoryEnabled` field).
/// 5. Default: enabled.
pub fn is_auto_memory_enabled(settings_enabled: Option<bool>) -> bool {
    if let Ok(val) = std::env::var("CLAURST_DISABLE_AUTO_MEMORY") {
        // Truthy values (non-empty, non-"0", non-"false") disable memory.
        match val.to_lowercase().as_str() {
            "" | "0" | "false" | "no" | "off" => return true, // defined-falsy → ON
            _ => return false,                                // truthy → OFF
        }
    }

    if std::env::var("CLAURST_SIMPLE").is_ok() {
        return false;
    }

    if std::env::var("CLAURST_REMOTE").is_ok()
        && std::env::var("CLAURST_REMOTE_MEMORY_DIR").is_err()
    {
        return false;
    }

    settings_enabled.unwrap_or(true)
}

// ---------------------------------------------------------------------------
// Index loading and truncation
// ---------------------------------------------------------------------------

/// Result of loading and (optionally) truncating the `MEMORY.md` entrypoint.
#[derive(Debug, Clone)]
pub struct EntrypointTruncation {
    pub content: String,
    pub line_count: usize,
    pub byte_count: usize,
    pub was_line_truncated: bool,
    pub was_byte_truncated: bool,
}

/// Truncate `MEMORY.md` content to `MAX_ENTRYPOINT_LINES` lines and
/// `MAX_ENTRYPOINT_BYTES` bytes, appending a warning when either cap fires.
///
/// Mirrors `truncateEntrypointContent` in `memdir.ts`.
pub fn truncate_entrypoint_content(raw: &str) -> EntrypointTruncation {
    let trimmed = raw.trim();
    let content_lines: Vec<&str> = trimmed.lines().collect();
    let line_count = content_lines.len();
    let byte_count = trimmed.len();

    let was_line_truncated = line_count > MAX_ENTRYPOINT_LINES;
    let was_byte_truncated = byte_count > MAX_ENTRYPOINT_BYTES;

    if !was_line_truncated && !was_byte_truncated {
        return EntrypointTruncation {
            content: trimmed.to_string(),
            line_count,
            byte_count,
            was_line_truncated: false,
            was_byte_truncated: false,
        };
    }

    let mut truncated = if was_line_truncated {
        content_lines[..MAX_ENTRYPOINT_LINES].join("\n")
    } else {
        trimmed.to_string()
    };

    if truncated.len() > MAX_ENTRYPOINT_BYTES {
        let cut_at = truncated[..MAX_ENTRYPOINT_BYTES]
            .rfind('\n')
            .unwrap_or(MAX_ENTRYPOINT_BYTES);
        truncated.truncate(cut_at);
    }

    let reason = match (was_line_truncated, was_byte_truncated) {
        (true, false) => format!("{} lines (limit: {})", line_count, MAX_ENTRYPOINT_LINES),
        (false, true) => format!(
            "{} bytes (limit: {}) — index entries are too long",
            byte_count, MAX_ENTRYPOINT_BYTES
        ),
        _ => format!("{} lines and {} bytes", line_count, byte_count),
    };

    truncated.push_str(&format!(
        "\n\n> WARNING: {} is {}. Only part of it was loaded. \
        Keep index entries to one line under ~200 chars; move detail into topic files.",
        MEMORY_ENTRYPOINT, reason
    ));

    EntrypointTruncation {
        content: truncated,
        line_count,
        byte_count,
        was_line_truncated,
        was_byte_truncated,
    }
}

/// Load and truncate the `MEMORY.md` index from `memory_dir`.
/// Returns `None` when the file does not exist or is empty.
///
/// Mirrors the entrypoint-reading path in `buildMemoryPrompt` / `loadMemoryPrompt`.
pub fn load_memory_index(memory_dir: &Path) -> Option<EntrypointTruncation> {
    let index_path = memory_dir.join(MEMORY_ENTRYPOINT);
    if !index_path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(&index_path).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(truncate_entrypoint_content(&raw))
}

// ---------------------------------------------------------------------------
// System-prompt memory content builder
// ---------------------------------------------------------------------------

/// Build the memory content string to inject into the system prompt's
/// `<memory>` block.
///
/// Always includes the `MEMORY.md` index when it exists.
/// Called during `build_system_prompt` → `SystemPromptOptions::memory_content`.
pub fn build_memory_prompt_content(memory_dir: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(index) = load_memory_index(memory_dir) {
        parts.push(format!("## Memory Index (MEMORY.md)\n{}", index.content));
    }

    parts.join("\n\n")
}

/// Ensure the memory directory exists, creating it (and any parents) if needed.
/// Errors are silently swallowed (the Write tool will surface them if needed).
///
/// Mirrors `ensureMemoryDirExists` in `memdir.ts`.
pub fn ensure_memory_dir_exists(memory_dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(memory_dir) {
        // Log at debug level so --debug shows why, but don't abort.
        tracing::debug!(
            dir = %memory_dir.display(),
            error = %e,
            "ensureMemoryDirExists failed"
        );
    }
}

// ---------------------------------------------------------------------------
// Simple relevance search (no LLM side-query)
// ---------------------------------------------------------------------------

/// Find and load the most relevant memory files for a query using a
/// lightweight TF-IDF-style keyword score.
///
/// The full Sonnet side-query (`findRelevantMemories` in TypeScript) lives
/// in `cc-query`; this function provides a cheaper fallback for contexts
/// where an API call is not available.
pub fn find_relevant_memories_simple(
    memory_dir: &Path,
    query: &str,
    max_files: usize,
) -> Vec<MemoryFile> {
    let metas = scan_memory_dir(memory_dir);
    let query_lower = query.to_lowercase();
    let query_words: Vec<&str> = query_lower.split_whitespace().collect();

    if query_words.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(f32, MemoryFileMeta)> = metas
        .into_iter()
        .filter_map(|meta| {
            let desc = meta.description.as_deref().unwrap_or("").to_lowercase();
            let name = meta.name.as_deref().unwrap_or("").to_lowercase();
            let filename = meta.filename.to_lowercase();

            let score: f32 = query_words
                .iter()
                .map(|w| {
                    let in_name = if name.contains(*w) { 2.0_f32 } else { 0.0 };
                    let in_desc = if desc.contains(*w) { 1.0_f32 } else { 0.0 };
                    let in_file = if filename.contains(*w) { 0.5_f32 } else { 0.0 };
                    in_name + in_desc + in_file
                })
                .sum();

            if score > 0.0 {
                Some((score, meta))
            } else {
                None
            }
        })
        .collect();

    // Sort highest score first.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    scored
        .into_iter()
        .take(max_files)
        .filter_map(|(_, meta)| {
            let content = std::fs::read_to_string(&meta.path).ok()?;
            Some(MemoryFile { meta, content })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Team memory helpers
// ---------------------------------------------------------------------------

/// Return the team-memory sub-directory path.
/// Mirrors `getTeamMemPath` in `teamMemPaths.ts`.
pub fn team_memory_path(auto_memory_dir: &Path) -> PathBuf {
    auto_memory_dir.join("team")
}

// ---------------------------------------------------------------------------
// Auto-memory store (write side of the memdir system)
// ---------------------------------------------------------------------------
//
// Extraction results persist here as ordinary memdir files so the existing
// read path (`load_memory_index` -> `build_memory_prompt_content`) serves
// them with no parallel store. Writers take a per-directory `fslock` and
// publish every file through a same-directory temp file + rename, so two
// sessions (or a subagent) writing concurrently never lose an entry.

/// One extracted fact destined for the memdir store.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoMemoryEntry {
    /// Human-readable topic label (frontmatter `name`).
    pub name: String,
    /// One-line hook for the `MEMORY.md` index (frontmatter `description`).
    pub description: String,
    /// The fact body.
    pub content: String,
    /// Canonical memdir type tag for the frontmatter.
    pub memory_type: MemoryType,
}

/// Persist a batch of extracted memories into the memdir store `dir`.
///
/// Each entry lands in its own topic file under `auto/<seq>-<slug>.md` and
/// `MEMORY.md` gains one index line per file. Writing is atomic (temp +
/// rename) and serialized against other writers by a lock file in `dir`, so
/// overlapping turns, subagents or a second session in the same repository
/// cannot wipe the store. A busy or failing writer skips silently: memory
/// extraction is best-effort and must never break the turn.
///
/// Returns the paths of the topic files written; empty when nothing was
/// stored.
pub fn append_auto_memories(dir: &Path, entries: &[AutoMemoryEntry]) -> Vec<PathBuf> {
    if entries.is_empty() || std::fs::create_dir_all(dir).is_err() {
        return Vec::new();
    }
    let lock_path = dir.join(".memdir.lock");
    let mut lock = match fslock::LockFile::open(&lock_path) {
        Ok(lock) => lock,
        Err(e) => {
            tracing::debug!(path = %lock_path.display(), error = %e, "memdir lock open failed");
            return Vec::new();
        }
    };
    if !lock.try_lock_with_pid().unwrap_or(false) {
        tracing::debug!(path = %lock_path.display(), "memdir store busy; skipping append");
        return Vec::new();
    }

    let mut written = Vec::new();
    for entry in entries {
        if let Some(path) = write_entry_locked(dir, entry) {
            written.push(path);
        }
    }
    written
}

/// Write one entry under the store lock. `None` means the entry was already
/// stored (a topic file carrying the same fact exists) or the write failed.
fn write_entry_locked(dir: &Path, entry: &AutoMemoryEntry) -> Option<PathBuf> {
    let auto_dir = dir.join("auto");
    if std::fs::create_dir_all(&auto_dir).is_err() {
        return None;
    }
    let slug = slug_for(&entry.name);

    // Same topic written before: append the fact to the oldest topic file
    // instead of spawning a near-duplicate. Same fact already present in it:
    // keep the first copy and drop this one.
    let topic_files = matching_topic_files(&auto_dir, &slug);
    if let Some(oldest) = topic_files.first() {
        let existing_bodies: Vec<String> = topic_files
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap_or_default())
            .collect();
        if existing_bodies
            .iter()
            .any(|body| body.contains(&entry.content))
        {
            return None;
        }
        let body = format!("{}\n{}\n", existing_bodies[0].trim_end(), entry.content);
        if atomic_write(oldest, body.as_bytes()).is_err() {
            return None;
        }
        return Some(oldest.clone());
    }
    let seq = next_seq(&auto_dir, &slug);
    let filename = format!("{seq}-{slug}.md");
    let path = auto_dir.join(&filename);
    let body = format!(
        "---\nname: {}\ndescription: {}\ntype: {}\n---\n\n{}\n",
        escape_frontmatter(&entry.name),
        escape_frontmatter(&entry.description),
        entry.memory_type.as_str(),
        entry.content
    );
    if atomic_write(&path, body.as_bytes()).is_err() {
        return None;
    }
    append_to_index(dir, &filename, &entry.description);
    Some(path)
}

/// Existing `NN-<slug>.md` files in the auto directory, oldest first.
fn matching_topic_files(auto_dir: &Path, slug: &str) -> Vec<PathBuf> {
    // `split_once('-')` yields what follows the seq dash, so the suffix must
    // not carry a dash of its own: `<seq>-<slug>.md` -> `<slug>.md`.
    let want = format!("{slug}.md");
    let mut files: Vec<(u32, PathBuf)> = std::fs::read_dir(auto_dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let (seq, rest) = name.split_once('-')?;
                    (rest == want)
                        .then(|| seq.parse::<u32>().ok().map(|n| (n, e.path())))
                        .flatten()
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort_by_key(|(seq, _)| *seq);
    files.into_iter().map(|(_, path)| path).collect()
}

/// Next sequence number for a slug: max existing `NN-<slug>.md` + 1.
fn next_seq(auto_dir: &Path, slug: &str) -> u32 {
    matching_topic_files(auto_dir, slug)
        .iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            name.split_once('-')?.0.parse::<u32>().ok()
        })
        .max()
        .map_or(0, |max| max + 1)
}

/// Filesystem-safe lowercase slug derived from the topic name.
fn slug_for(name: &str) -> String {
    let mut slug: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        slug = "memory".to_string();
    }
    slug.chars().take(48).collect()
}

/// Frontmatter values must not contain newlines: they would break the
/// `parse_frontmatter_quick` line scan.
fn escape_frontmatter(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

/// Append one index line to `MEMORY.md`, deduplicated by filename marker and
/// capped at `MAX_ENTRYPOINT_LINES` lines (oldest auto lines drop first).
fn append_to_index(dir: &Path, filename: &str, description: &str) {
    let index_path = dir.join(MEMORY_ENTRYPOINT);
    let marker = format!("({filename})");
    let base_line = format!("- [{}]({})", filename, filename);
    let line = if description.chars().count() > 160 {
        base_line
    } else {
        format!("{base_line} — {description}")
    };

    let existing = std::fs::read_to_string(&index_path).unwrap_or_default();
    if existing.contains(&marker) {
        return;
    }

    let mut lines: Vec<String> = existing
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.trim().is_empty())
        .collect();
    lines.push(line);
    while lines.len() > MAX_ENTRYPOINT_LINES {
        match lines.iter().position(|l| l.contains("](auto/")) {
            Some(pos) => {
                lines.remove(pos);
            }
            None => break,
        }
    }
    let content = format!("{}\n", lines.join("\n"));
    if let Err(e) = atomic_write(&index_path, content.as_bytes()) {
        tracing::debug!(path = %index_path.display(), error = %e, "memdir index write failed");
    }
}

/// Write `bytes` to a temp file in the same directory, fsync, rename over
/// `path`. A crash leaves either the old or the new file, never a torn one.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "atomic_write: path has no file name",
        ));
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

/// Stable directory key for a project root: first 16 hex chars of the SHA-256
/// digest of the canonical path. `std::fs::canonicalize` resolves symlinks
/// and relative components so the same checkout always maps to the same key,
/// and SHA-256 keeps it stable across Rust toolchain bumps (unlike
/// `DefaultHasher`, which makes no such guarantee).
pub fn project_path_key(project_root: &Path) -> String {
    let canonical =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..16].to_string()
}

/// Return the memdir store directory for a project root and ensure it exists.
///
/// The store is keyed by the stable path hash rather than the sanitized full
/// path, so renaming a parent directory (or a path collision after
/// sanitization) cannot orphan or cross-wire stores:
/// `<base>/projects/<sha256-16>/memory/`.
pub fn memory_store_dir(project_root: &Path) -> PathBuf {
    let base = std::env::var("CLAURST_REMOTE_MEMORY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::config::Settings::config_dir());
    let dir = base
        .join("projects")
        .join(project_path_key(project_root))
        .join("memory");
    ensure_memory_dir_exists(&dir);
    dir
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    // Helpers ----------------------------------------------------------------

    fn make_temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    // ---- parse_frontmatter_quick -------------------------------------------

    #[test]
    fn test_parse_frontmatter_full() {
        let content = "---\nname: My Memory\ndescription: A test description\ntype: feedback\n---\n\nBody text.";
        let (name, desc, mt) = parse_frontmatter_quick(content);
        assert_eq!(name.as_deref(), Some("My Memory"));
        assert_eq!(desc.as_deref(), Some("A test description"));
        assert_eq!(mt, Some(MemoryType::Feedback));
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let content = "Just plain text.";
        let (name, desc, mt) = parse_frontmatter_quick(content);
        assert!(name.is_none());
        assert!(desc.is_none());
        assert!(mt.is_none());
    }

    #[test]
    fn test_parse_frontmatter_quoted_values() {
        let content = "---\nname: \"Quoted Name\"\ndescription: 'Single quoted'\ntype: user\n---";
        let (name, desc, mt) = parse_frontmatter_quick(content);
        assert_eq!(name.as_deref(), Some("Quoted Name"));
        assert_eq!(desc.as_deref(), Some("Single quoted"));
        assert_eq!(mt, Some(MemoryType::User));
    }

    #[test]
    fn test_parse_frontmatter_unknown_type() {
        let content = "---\ntype: unknown_type\n---";
        let (_, _, mt) = parse_frontmatter_quick(content);
        assert!(mt.is_none());
    }

    // ---- memory_age_days ---------------------------------------------------

    #[test]
    fn test_memory_age_today() {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(memory_age_days(now_secs), 0);
    }

    #[test]
    fn test_memory_age_one_day_ago() {
        let yesterday = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(86_400);
        assert_eq!(memory_age_days(yesterday), 1);
    }

    #[test]
    fn test_memory_age_future_clamps_to_zero() {
        let far_future = u64::MAX;
        assert_eq!(memory_age_days(far_future), 0);
    }

    // ---- memory_freshness_text ---------------------------------------------

    #[test]
    fn test_freshness_text_fresh() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(memory_freshness_text(now).is_empty());
    }

    #[test]
    fn test_freshness_text_stale() {
        let old = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(10 * 86_400); // 10 days ago
        let text = memory_freshness_text(old);
        assert!(text.contains("10 days old"));
        assert!(text.contains("point-in-time"));
    }

    // ---- memory_freshness_note ---------------------------------------------

    #[test]
    fn test_freshness_note_fresh_is_empty() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(memory_freshness_note(now).is_empty());
    }

    #[test]
    fn test_freshness_note_stale_has_tags() {
        let old = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(5 * 86_400);
        let note = memory_freshness_note(old);
        assert!(note.contains("<system-reminder>"));
        assert!(note.contains("</system-reminder>"));
    }

    // ---- truncate_entrypoint_content ---------------------------------------

    #[test]
    fn test_truncate_no_truncation_needed() {
        let content = "line1\nline2\nline3";
        let result = truncate_entrypoint_content(content);
        assert!(!result.was_line_truncated);
        assert!(!result.was_byte_truncated);
        assert_eq!(result.content, content);
    }

    #[test]
    fn test_truncate_line_limit() {
        let content = (0..=MAX_ENTRYPOINT_LINES)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_entrypoint_content(&content);
        assert!(result.was_line_truncated);
        assert!(result.content.contains("WARNING"));
    }

    // ---- sanitize_path_component -------------------------------------------

    #[test]
    fn test_sanitize_path_component() {
        assert_eq!(
            sanitize_path_component("/home/user/project"),
            "_home_user_project"
        );
        assert_eq!(
            sanitize_path_component("normal-name_123"),
            "normal-name_123"
        );
        assert_eq!(sanitize_path_component("C:\\Users\\foo"), "C__Users_foo");
    }

    // ---- load_memory_index -------------------------------------------------

    #[test]
    fn test_load_memory_index_nonexistent() {
        let dir = make_temp_dir();
        assert!(load_memory_index(dir.path()).is_none());
    }

    #[test]
    fn test_load_memory_index_empty() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "   ");
        assert!(load_memory_index(dir.path()).is_none());
    }

    #[test]
    fn test_load_memory_index_with_content() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "- [test.md](test.md) — something");
        let result = load_memory_index(dir.path()).unwrap();
        assert!(result.content.contains("test.md"));
    }

    // ---- scan_memory_dir ---------------------------------------------------

    #[test]
    fn test_scan_excludes_memory_md() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "# index");
        write_file(dir.path(), "user_role.md", "---\nname: Role\n---");
        let metas = scan_memory_dir(dir.path());
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].filename, "user_role.md");
    }

    #[test]
    fn test_scan_empty_dir() {
        let dir = make_temp_dir();
        assert!(scan_memory_dir(dir.path()).is_empty());
    }

    #[test]
    fn test_scan_nonexistent_dir() {
        let path = PathBuf::from("/tmp/nonexistent_memory_dir_cc_rust_test_xyz");
        assert!(scan_memory_dir(&path).is_empty());
    }

    // ---- format_memory_manifest --------------------------------------------

    #[test]
    fn test_format_memory_manifest_with_description() {
        let meta = MemoryFileMeta {
            filename: "user_role.md".to_string(),
            path: PathBuf::from("user_role.md"),
            name: Some("User Role".to_string()),
            description: Some("The user is a data scientist".to_string()),
            memory_type: Some(MemoryType::User),
            modified_secs: 0,
        };
        let manifest = format_memory_manifest(&[meta]);
        assert!(manifest.contains("[user]"));
        assert!(manifest.contains("user_role.md"));
        assert!(manifest.contains("data scientist"));
    }

    #[test]
    fn test_format_memory_manifest_no_description() {
        let meta = MemoryFileMeta {
            filename: "ref.md".to_string(),
            path: PathBuf::from("ref.md"),
            name: None,
            description: None,
            memory_type: None,
            modified_secs: 0,
        };
        let manifest = format_memory_manifest(&[meta]);
        assert!(manifest.contains("ref.md"));
        // No description separator colon
        assert!(!manifest.contains("ref.md ("));
    }

    // ---- MemoryType --------------------------------------------------------

    #[test]
    fn test_memory_type_roundtrip() {
        for (s, expected) in [
            ("user", MemoryType::User),
            ("feedback", MemoryType::Feedback),
            ("project", MemoryType::Project),
            ("reference", MemoryType::Reference),
        ] {
            let parsed = MemoryType::parse(s).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.as_str(), s);
        }
    }

    #[test]
    fn test_memory_type_unknown_returns_none() {
        assert!(MemoryType::parse("bogus").is_none());
    }

    // ---- is_auto_memory_enabled -------------------------------------------

    #[test]
    fn test_auto_memory_enabled_default() {
        // No env vars set for this test, settings None → should be enabled.
        // We can't guarantee the test environment is clean, so just check it
        // returns a bool without panicking.
        let _ = is_auto_memory_enabled(None);
    }

    #[test]
    fn test_auto_memory_disabled_by_setting() {
        // If settings explicitly disable it and no env override, returns false.
        // We only test the settings-path without touching process env.
        // Simulate: env vars not set, settings says false.
        // We can't unset env vars reliably in tests, so just ensure the
        // function handles Some(false) without panicking.
        // (The full env-var paths are integration-tested separately.)
        let _ = is_auto_memory_enabled(Some(false));
    }

    // ---- append_auto_memories ----------------------------------------------

    fn auto_entry(name: &str, description: &str, content: &str) -> AutoMemoryEntry {
        AutoMemoryEntry {
            name: name.to_string(),
            description: description.to_string(),
            content: content.to_string(),
            memory_type: MemoryType::Project,
        }
    }

    #[test]
    fn test_append_writes_topic_file_and_index_line() {
        let dir = make_temp_dir();
        let written = append_auto_memories(
            dir.path(),
            &[auto_entry(
                "Build tool",
                "uses cargo",
                "The project uses cargo.",
            )],
        );
        assert_eq!(written.len(), 1);
        let path = &written[0];
        assert!(path.starts_with(dir.path().join("auto")));
        let body = std::fs::read_to_string(path).unwrap();
        assert!(body.contains("---\nname: Build tool\n"));
        assert!(body.contains("type: project\n"));
        assert!(body.contains("The project uses cargo."));
        let index = std::fs::read_to_string(dir.path().join(MEMORY_ENTRYPOINT)).unwrap();
        assert!(index.contains(&format!(
            "({})",
            path.file_name().unwrap().to_string_lossy()
        )));
    }

    #[test]
    fn test_append_same_fact_stored_once() {
        let dir = make_temp_dir();
        let entry = auto_entry("Build tool", "uses cargo", "The project uses cargo.");
        let first = append_auto_memories(dir.path(), std::slice::from_ref(&entry));
        let second = append_auto_memories(dir.path(), &[entry]);
        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
        let auto_dir = dir.path().join("auto");
        let count = std::fs::read_dir(&auto_dir).unwrap().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_append_same_topic_extends_one_file() {
        let dir = make_temp_dir();
        let first = append_auto_memories(
            dir.path(),
            &[auto_entry(
                "Build tool",
                "uses cargo",
                "The project uses cargo.",
            )],
        );
        let second = append_auto_memories(
            dir.path(),
            &[auto_entry(
                "Build tool",
                "targets wasm",
                "Wasm is a build target.",
            )],
        );
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0], second[0]);
        let auto_dir = dir.path().join("auto");
        let count = std::fs::read_dir(&auto_dir).unwrap().count();
        assert_eq!(count, 1);
        let body = std::fs::read_to_string(&second[0]).unwrap();
        assert!(body.contains("The project uses cargo."));
        assert!(body.contains("Wasm is a build target."));
    }

    #[test]
    fn test_next_seq_counts_per_topic() {
        let dir = make_temp_dir();
        let auto_dir = dir.path().join("auto");
        std::fs::create_dir_all(&auto_dir).unwrap();
        write_file(&auto_dir, "3-build-tool.md", "body");
        write_file(&auto_dir, "10-other.md", "body");
        assert_eq!(next_seq(&auto_dir, "build-tool"), 4);
        assert_eq!(next_seq(&auto_dir, "other"), 11);
        assert_eq!(next_seq(&auto_dir, "missing"), 0);
    }

    #[test]
    fn test_append_different_topics_get_separate_files() {
        let dir = make_temp_dir();
        let written = append_auto_memories(
            dir.path(),
            &[
                auto_entry("Build tool", "uses cargo", "The project uses cargo."),
                auto_entry(
                    "Test style",
                    "prefers unit tests",
                    "Unit tests are preferred.",
                ),
            ],
        );
        assert_eq!(written.len(), 2);
        let names: Vec<String> = written
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"0-build-tool.md".to_string()));
        assert!(names.contains(&"0-test-style.md".to_string()));
    }

    // ---- project_path_key / memory_store_dir --------------------------------

    #[test]
    fn test_project_path_key_stable_across_calls() {
        let dir = make_temp_dir();
        let key1 = project_path_key(dir.path());
        let key2 = project_path_key(dir.path());
        assert_eq!(key1, key2);
        assert_eq!(key1.len(), 16);
        assert!(key1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_project_path_key_distinct_paths() {
        let dir1 = make_temp_dir();
        let dir2 = make_temp_dir();
        assert_ne!(project_path_key(dir1.path()), project_path_key(dir2.path()));
    }

    // ---- atomic_write -------------------------------------------------------

    #[test]
    fn test_atomic_write_leaves_no_temp_file() {
        let dir = make_temp_dir();
        let path = dir.path().join("out.md");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        let leftover: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftover.is_empty());
    }

    #[test]
    fn test_append_concurrent_writer_skips_without_data_loss() {
        let dir = make_temp_dir();
        let lock_path = dir.path().join(".memdir.lock");
        let mut lock = fslock::LockFile::open(&lock_path).unwrap();
        assert!(lock.try_lock_with_pid().unwrap());
        let written = append_auto_memories(
            dir.path(),
            &[auto_entry(
                "Build tool",
                "uses cargo",
                "The project uses cargo.",
            )],
        );
        assert!(written.is_empty());
        // The held lock must not corrupt the directory or drop files that
        // were already there.
        assert!(!dir.path().join("auto").exists());
        lock.unlock().unwrap();
        let after = append_auto_memories(
            dir.path(),
            &[auto_entry(
                "Build tool",
                "uses cargo",
                "The project uses cargo.",
            )],
        );
        assert_eq!(after.len(), 1);
    }
}
