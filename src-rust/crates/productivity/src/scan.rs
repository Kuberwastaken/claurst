//! Transcript scanning with an on-disk incremental cache.
//!
//! Walks every `.jsonl` under `~/.claurst/projects/` (all projects, matching
//! `claurst stats --all-projects`). A sidecar cache
//! (`~/.claurst/cache/productivity/summaries.json`) keyed by
//! `(file_len, mtime_ns)` skips files whose fingerprint is unchanged, so a
//! re-run only re-parses what actually moved.

use crate::model::SessionSummary;
use anyhow::Result;
use chrono::{DateTime, Datelike, Local, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// One cached entry: the file fingerprint plus the computed summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    len: u64,
    mtime_ns: i128,
    summary: SessionSummary,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    /// Cache format version; bump to invalidate when summary semantics change.
    #[serde(default)]
    version: u32,
    entries: HashMap<String, CacheEntry>,
}

const CACHE_VERSION: u32 = 1;

/// Maximum file size we will parse — mirrors `core::session_storage::MAX_TRANSCRIPT_BYTES`.
const MAX_PARSE_BYTES: u64 = 50 * 1024 * 1024;

fn cache_path() -> Result<PathBuf> {
    let dir = claurst_core::config::Settings::config_dir()
        .join("cache")
        .join("productivity");
    let _ = std::fs::create_dir_all(&dir);
    Ok(dir.join("summaries.json"))
}

fn projects_dir() -> PathBuf {
    // Same convention as core / `claurst stats`: ~/.claurst/projects/
    claurst_core::config::Settings::config_dir().join("projects")
}

/// Result of a full scan: every session summary plus scan diagnostics.
pub struct ScanResult {
    pub summaries: Vec<SessionSummary>,
    pub scanned_files: u64,
    pub parse_errors: u64,
    pub cache_hits: u64,
    pub scan_secs: f64,
}

/// Scan all session transcripts across every project, using and refreshing the
/// incremental cache. Never fails: unreadable files are counted in
/// `parse_errors` instead.
pub fn scan_all() -> ScanResult {
    let started = Instant::now();

    let mut cache: Cache = cache_path()
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if cache.version != CACHE_VERSION {
        cache = Cache::default();
        cache.version = CACHE_VERSION;
    }

    let mut files: Vec<(String, u64, i128)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(projects_dir()) {
        for project in rd.flatten() {
            if !project.path().is_dir() {
                continue;
            }
            let Ok(inner) = std::fs::read_dir(project.path()) else {
                continue;
            };
            for entry in inner.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                // Key includes the project dir so same-named files in
                // different projects do not collide.
                let key = format!("{}/{}", project.file_name().to_string_lossy(), name);
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                files.push((key, meta.len(), mtime_ns(&meta)));
            }
        }
    }

    let mut cache_hits = 0u64;
    let mut parse_errors = 0u64;

    let mut new_entries: HashMap<String, CacheEntry> = HashMap::with_capacity(files.len());
    let mut summaries: Vec<SessionSummary> = Vec::with_capacity(files.len());

    for (key, len, mtime) in files {
        if let Some(prev) = cache.entries.get(&key) {
            if prev.len == len && prev.mtime_ns == mtime {
                cache_hits += 1;
                summaries.push(prev.summary.clone());
                new_entries.insert(key, prev.clone());
                continue;
            }
        }
        let path = transcript_path(&key);
        match parse_session_file(&path) {
            Ok(summary) => {
                summaries.push(summary.clone());
                new_entries.insert(
                    key,
                    CacheEntry {
                        len,
                        mtime_ns: mtime,
                        summary,
                    },
                );
            }
            Err(_) => parse_errors += 1,
        }
    }

    let new_cache = Cache {
        version: CACHE_VERSION,
        entries: new_entries,
    };
    if let (Ok(bytes), Ok(path)) = (serde_json::to_vec(&new_cache), cache_path()) {
        let _ = std::fs::write(path, bytes);
    }

    ScanResult {
        scanned_files: summaries.len() as u64,
        parse_errors,
        cache_hits,
        scan_secs: started.elapsed().as_secs_f64(),
        summaries,
    }
}

/// Rebuild the absolute path of a transcript from its cache key
/// (`<encoded-project>/<file>.jsonl`).
fn transcript_path(key: &str) -> PathBuf {
    projects_dir().join(key)
}

#[cfg(unix)]
fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    (meta.mtime() as i128) * 1_000_000_000 + (meta.mtime_nsec() as i128)
}

#[cfg(not(unix))]
fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Transcript parsing
//
// We reuse the canonical `TranscriptEntry` types from core (same tolerance for
// tombstones / unknown entries as the persisted `/stats` command) and fold each
// entry into the summary as we walk it.
// ---------------------------------------------------------------------------

fn parse_session_file(path: &Path) -> Result<SessionSummary> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_PARSE_BYTES {
        anyhow::bail!("transcript too large: {} bytes", meta.len());
    }
    let raw = std::fs::read_to_string(path)?;

    use claurst_core::session_storage::TranscriptEntry;
    use claurst_core::types::{ContentBlock, MessageContent, Role};

    // First pass: tombstoned uuids.
    let mut tombstoned: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || (!trimmed.contains("\"type\":\"tombstone\"")
                && !trimmed.contains("\"type\": \"tombstone\""))
        {
            continue;
        }
        if let Ok(TranscriptEntry::Tombstone(t)) = serde_json::from_str::<TranscriptEntry>(trimmed)
        {
            tombstoned.insert(t.deleted_uuid);
        }
    }

    let mut s = SessionSummary::default();
    let mut active_dates: BTreeSet<String> = BTreeSet::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: TranscriptEntry = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if matches!(
            entry,
            TranscriptEntry::Tombstone(_) | TranscriptEntry::Unknown
        ) {
            continue;
        }
        if let Some(uuid) = entry.uuid() {
            if tombstoned.contains(uuid) {
                continue;
            }
        }

        let msg = match &entry {
            TranscriptEntry::User(m) | TranscriptEntry::Assistant(m) => m,
            _ => continue,
        };

        let is_user = msg.message.role == Role::User;
        if is_user {
            s.user_msgs += 1;
        } else {
            s.assistant_msgs += 1;
        }

        if !msg.timestamp.is_empty() {
            if s.first_ts.is_none() {
                s.first_ts = Some(msg.timestamp.clone());
            }
            s.last_ts = Some(msg.timestamp.clone());
            if let Ok(dt) = DateTime::parse_from_rfc3339(&msg.timestamp) {
                let local = dt.with_timezone(&Local);
                s.hour_hist[local.hour() as usize] += 1;
                s.weekday_hist[local.weekday().num_days_from_monday() as usize] += 1;
                active_dates.insert(local.format("%Y-%m-%d").to_string());
            }
        }

        if let Some(cost) = &msg.message.cost {
            s.input_tokens += cost.input_tokens;
            s.output_tokens += cost.output_tokens;
            s.cache_read_tokens += cost.cache_read_input_tokens;
            s.cache_creation_tokens += cost.cache_creation_input_tokens;
        }

        if !msg.cwd.is_empty() {
            s.working_dir = Some(msg.cwd.clone());
        }

        match &msg.message.content {
            MessageContent::Text(text) => {
                let len = text.chars().count() as u64;
                if is_user {
                    s.user_chars += len;
                } else {
                    s.assistant_chars += len;
                }
            }
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            let len = text.chars().count() as u64;
                            if is_user {
                                // Skip giant tool-result-ish or reminder blobs
                                // from the human "typed" proxy.
                                if !text.trim_start().starts_with("<system-reminder>") {
                                    s.user_chars += len;
                                }
                            } else {
                                s.assistant_chars += len;
                            }
                        }
                        ContentBlock::ToolUse { name, input, .. } => {
                            count_tool(&mut s.tools, name, input);
                        }
                        ContentBlock::Image { .. } => s.images += 1,
                        _ => {}
                    }
                }
            }
        }

        if s.model.is_none() {
            s.model = model_from_extra(&msg.extra);
        }
    }

    if let Some(dir) = s.working_dir.as_deref() {
        s.project = Some(project_name(dir));
    }
    s.active_dates = active_dates.into_iter().collect();
    Ok(s)
}

/// Pull the model id out of the catch-all `extra` map if the TS CLI wrote it.
fn model_from_extra(
    extra: &std::collections::HashMap<String, serde_json::Value>,
) -> Option<String> {
    extra
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|m| !m.is_empty())
        .map(String::from)
}

/// Expand a single tool invocation into the histogram.
fn count_tool(tools: &mut BTreeMap<String, u32>, name: &str, input: &serde_json::Value) {
    let canonical = canonical_tool_name(name);
    *tools.entry(canonical.to_string()).or_insert(0) += 1;

    if canonical == "batch" {
        if let Some(calls) = input.get("tool_calls").and_then(|v| v.as_array()) {
            for call in calls {
                if let Some(inner) = call.get("tool").and_then(|v| v.as_str()) {
                    let inner_canon = canonical_tool_name(inner);
                    *tools.entry(inner_canon.to_string()).or_insert(0) += 1;
                }
            }
        }
    }
}

/// Normalize legacy/alias tool names to their canonical identity.
fn canonical_tool_name(name: &str) -> &str {
    match name {
        "file_read" => "read",
        "file_write" => "write",
        "file_edit" => "edit",
        "file_grep" => "grep",
        "todowrite" => "todo",
        other => other,
    }
}

fn project_name(working_dir: &str) -> String {
    let trimmed = working_dir.trim_end_matches('/');
    Path::new(trimmed)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

/// Encode a project cwd the way `~/.claurst/projects/` names its directories,
/// used by tests to build realistic project layouts.
#[cfg(test)]
pub(crate) fn encode_project_dir(cwd: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(cwd.as_bytes())
}

#[cfg(test)]
pub(crate) fn write_fixture(
    root: &Path,
    cwd: &str,
    session: &str,
    lines: &[serde_json::Value],
) -> std::io::Result<PathBuf> {
    let dir = root.join("projects").join(encode_project_dir(cwd));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{session}.jsonl"));
    let body = lines
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, body)?;
    Ok(path)
}
