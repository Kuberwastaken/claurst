//! Session analytics: read persisted JSONL transcripts under
//! `~/.claurst/projects/<base64url(cwd)>/<session>.jsonl` and produce
//! token / cost / tool-usage summaries.
//!
//! This is the persisted complement to the in-memory `/stats` slash command
//! at [`crate::StatsCommand`] (defined in `lib.rs`).
//!
//! Design cribbed (in concept, not code) from:
//!   - `refs/pi/scripts/stats.ts` (per-day tokens + cost)
//!   - `refs/pi/scripts/tool-stats.ts` (per-tool calls + histograms)
//!   - `refs/pi/scripts/cost.ts` (per-day cost breakdown)
//!   - `refs/opencode/packages/opencode/src/cli/cmd/stats.ts` (table layout,
//!     formatNumber, bar chart, top-N sorting)
//!
//! The whole pipeline is sync (std::fs + serde_json) so it works inside the
//! existing sync `NamedCommand::execute_named` trait without runtime gymnastics.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;

use claurst_core::session_storage::TranscriptEntry;
use claurst_core::types::ContentBlock;

use crate::{CommandContext, CommandResult};

// ---------------------------------------------------------------------------
// Arg parsing
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Subcommand {
    Summary,
    Sessions,
    Tools,
    Daily,
    SessionDetail,
}

#[derive(Debug)]
struct Args {
    sub: Subcommand,
    days: Option<u32>,
    top: Option<usize>,
    all_projects: bool,
    json: bool,
    session_id: Option<String>,
}

fn parse_args(raw: &[&str]) -> Result<Args, String> {
    let mut sub = Subcommand::Summary;
    let mut days = None;
    let mut top = None;
    let mut all_projects = false;
    let mut json = false;
    let mut session_id = None;
    let mut positional: Vec<&str> = Vec::new();

    let mut i = 0;
    while i < raw.len() {
        let arg = raw[i];
        match arg {
            "--days" | "-n" => {
                let v = raw.get(i + 1).ok_or_else(|| {
                    format!("{arg} requires a number, e.g. `--days 7`")
                })?;
                days = Some(v.parse::<u32>().map_err(|_| {
                    format!("Invalid value for {arg}: {v}")
                })?);
                i += 2;
            }
            "--top" | "-t" => {
                let v = raw.get(i + 1).ok_or_else(|| {
                    format!("{arg} requires a number, e.g. `--top 10`")
                })?;
                top = Some(v.parse::<usize>().map_err(|_| {
                    format!("Invalid value for {arg}: {v}")
                })?);
                i += 2;
            }
            "--all-projects" | "-a" => {
                all_projects = true;
                i += 1;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "--help" | "-h" => {
                return Err(help_text().to_string());
            }
            s if s.starts_with("--") => {
                return Err(format!("Unknown flag: {s}"));
            }
            _ => {
                positional.push(arg);
                i += 1;
            }
        }
    }

    if let Some(first) = positional.first() {
        sub = match *first {
            "summary" | "" => Subcommand::Summary,
            "sessions" => Subcommand::Sessions,
            "tools" => Subcommand::Tools,
            "daily" | "cost" | "costs" => Subcommand::Daily,
            "session" => {
                session_id = positional.get(1).map(|s| s.to_string());
                if session_id.is_none() {
                    return Err(
                        "Usage: claurst stats session <session-id>".to_string(),
                    );
                }
                Subcommand::SessionDetail
            }
            other => return Err(format!("Unknown subcommand: '{other}'")),
        };
    }

    Ok(Args {
        sub,
        days,
        top,
        all_projects,
        json,
        session_id,
    })
}

fn help_text() -> &'static str {
    "Usage: claurst stats [subcommand] [flags]\n\
     \n\
     Reads persisted JSONL transcripts under ~/.claurst/projects/ and produces\n\
     token, cost, and tool-usage summaries.\n\
     \n\
     Subcommands:\n  \
       summary               (default) overview of sessions in scope\n  \
       sessions              per-session breakdown\n  \
       tools                 tool invocation counts and reach\n  \
       daily                 per-day cost + token breakdown\n  \
       session <id>          drill into one session\n\
     \n\
     Flags:\n  \
       --days N, -n N        only include sessions modified in the last N days\n  \
       --top N, -t N         show only the top N rows where applicable\n  \
       --all-projects, -a    aggregate across every project, not just the cwd\n  \
       --json                emit machine-readable JSON\n  \
       --help, -h            show this help\n\
     \n\
     Note: `/stats` (slash command) shows the *current* in-memory session\n\
     instead; this command reads what's already on disk."
}

// ---------------------------------------------------------------------------
// Per-session aggregated stats
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Serialize)]
struct SessionStats {
    session_id: String,
    /// Decoded project root path (best-effort).
    project_dir: String,
    /// Absolute path to the .jsonl file.
    #[serde(skip)]
    path: PathBuf,
    title: Option<String>,
    last_prompt: Option<String>,
    /// Earliest message timestamp encountered.
    first_ts: Option<DateTime<Utc>>,
    /// Latest message timestamp encountered.
    last_ts: Option<DateTime<Utc>>,
    /// File mtime, used as a fallback last-activity marker.
    #[serde(skip)]
    mtime: Option<SystemTime>,
    user_turns: u64,
    assistant_turns: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    cost_usd: f64,
    tool_calls: u64,
    /// tool name → invocation count
    tool_counts: BTreeMap<String, u64>,
}

impl SessionStats {
    fn total_tokens(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_tokens
            + self.cache_read_tokens
    }

    fn duration_secs(&self) -> Option<i64> {
        match (self.first_ts, self.last_ts) {
            (Some(a), Some(b)) => Some((b - a).num_seconds().max(0)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregation pipeline (sync, std::fs based)
// ---------------------------------------------------------------------------

/// Maximum file size we will parse — mirrors `core::session_storage::MAX_TRANSCRIPT_BYTES`.
const MAX_PARSE_BYTES: u64 = 50 * 1024 * 1024;

/// Tail window for cheap metadata extraction (last-prompt, custom-title).
const TAIL_WINDOW: u64 = 64 * 1024;

fn projects_dir() -> PathBuf {
    // Same convention as core: ~/.claurst/projects/
    claurst_core::config::Settings::config_dir().join("projects")
}

fn encoded_dir_for_cwd(cwd: &Path) -> String {
    URL_SAFE_NO_PAD.encode(cwd.to_string_lossy().as_bytes())
}

fn decode_project_dir(encoded: &str) -> Option<String> {
    URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
}

/// Walk the project tree and return `(project_dir_decoded, jsonl_path)` tuples
/// for every `.jsonl` file in scope.
fn collect_jsonl_paths(cwd: &Path, all_projects: bool) -> Vec<(String, PathBuf)> {
    let root = projects_dir();
    let mut out = Vec::new();

    let project_dirs: Vec<(String, PathBuf)> = if all_projects {
        let mut dirs = Vec::new();
        if let Ok(read) = fs::read_dir(&root) {
            for entry in read.flatten() {
                let p = entry.path();
                if !p.is_dir() {
                    continue;
                }
                let encoded = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                let decoded = decode_project_dir(&encoded).unwrap_or(encoded);
                dirs.push((decoded, p));
            }
        }
        dirs
    } else {
        let encoded = encoded_dir_for_cwd(cwd);
        let dir = root.join(&encoded);
        let decoded = cwd.to_string_lossy().to_string();
        vec![(decoded, dir)]
    };

    for (decoded, dir) in project_dirs {
        let read = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            out.push((decoded.clone(), path));
        }
    }

    out
}

/// Parse a JSONL transcript with std::fs, returning the non-tombstoned entries.
fn parse_jsonl_sync(path: &Path) -> Vec<TranscriptEntry> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    if meta.len() > MAX_PARSE_BYTES {
        return Vec::new();
    }
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    // First pass: collect tombstoned uuids.
    let mut tombstoned: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.contains("\"type\":\"tombstone\"")
            && !trimmed.contains("\"type\": \"tombstone\"")
        {
            continue;
        }
        if let Ok(TranscriptEntry::Tombstone(t)) =
            serde_json::from_str::<TranscriptEntry>(trimmed)
        {
            tombstoned.insert(t.deleted_uuid);
        }
    }

    // Second pass: keep useful entries.
    let mut entries = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: TranscriptEntry = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue,
        };
        match &entry {
            TranscriptEntry::Tombstone(_) | TranscriptEntry::Unknown => continue,
            _ => {}
        }
        if let Some(uuid) = entry.uuid() {
            if tombstoned.contains(uuid) {
                continue;
            }
        }
        entries.push(entry);
    }
    entries
}

/// Cheap tail read to pull `last-prompt` and `custom-title` without a full parse —
/// used when we don't need to walk all messages.
fn read_session_tail_metadata_sync(path: &Path) -> (Option<String>, Option<String>) {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, None),
    };
    let len = match file.metadata().map(|m| m.len()) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    if len == 0 {
        return (None, None);
    }
    let offset = len.saturating_sub(TAIL_WINDOW);
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return (None, None);
    }
    let mut buf = Vec::with_capacity((len - offset) as usize);
    if file.read_to_end(&mut buf).is_err() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&buf);

    let mut last_prompt = None;
    let mut title = None;
    for line in text.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if last_prompt.is_none()
            && (trimmed.contains("\"type\":\"last-prompt\"")
                || trimmed.contains("\"type\": \"last-prompt\""))
        {
            if let Ok(TranscriptEntry::LastPrompt(lp)) =
                serde_json::from_str::<TranscriptEntry>(trimmed)
            {
                last_prompt = Some(lp.last_prompt);
            }
        }
        if title.is_none()
            && (trimmed.contains("\"type\":\"custom-title\"")
                || trimmed.contains("\"type\": \"custom-title\""))
        {
            if let Ok(TranscriptEntry::CustomTitle(ct)) =
                serde_json::from_str::<TranscriptEntry>(trimmed)
            {
                title = Some(ct.custom_title);
            }
        }
        if last_prompt.is_some() && title.is_some() {
            break;
        }
    }

    (last_prompt, title)
}

/// Compute per-session stats from a parsed entry list.
fn session_stats_from_entries(
    session_id: String,
    project_dir: String,
    path: PathBuf,
    mtime: Option<SystemTime>,
    entries: &[TranscriptEntry],
) -> SessionStats {
    let mut s = SessionStats {
        session_id,
        project_dir,
        path,
        mtime,
        ..Default::default()
    };

    for entry in entries {
        match entry {
            TranscriptEntry::User(m) => {
                s.user_turns += 1;
                update_timestamps(&mut s, &m.timestamp);
                // User messages don't carry cost.
            }
            TranscriptEntry::Assistant(m) => {
                s.assistant_turns += 1;
                update_timestamps(&mut s, &m.timestamp);
                if let Some(cost) = &m.message.cost {
                    s.input_tokens += cost.input_tokens;
                    s.output_tokens += cost.output_tokens;
                    s.cache_creation_tokens += cost.cache_creation_input_tokens;
                    s.cache_read_tokens += cost.cache_read_input_tokens;
                    s.cost_usd += cost.cost_usd;
                }
                for block in m.message.get_tool_use_blocks() {
                    if let ContentBlock::ToolUse { name, .. } = block {
                        s.tool_calls += 1;
                        *s.tool_counts.entry(name.clone()).or_insert(0) += 1;
                    }
                }
            }
            TranscriptEntry::CustomTitle(ct) => {
                s.title = Some(ct.custom_title.clone());
            }
            TranscriptEntry::AiTitle(ai) if s.title.is_none() => {
                s.title = Some(ai.ai_title.clone());
            }
            TranscriptEntry::LastPrompt(lp) => {
                s.last_prompt = Some(lp.last_prompt.clone());
            }
            _ => {}
        }
    }

    s
}

fn update_timestamps(s: &mut SessionStats, raw_ts: &str) {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw_ts) {
        let utc = parsed.with_timezone(&Utc);
        s.first_ts = Some(match s.first_ts {
            Some(prev) if prev < utc => prev,
            _ => utc,
        });
        s.last_ts = Some(match s.last_ts {
            Some(prev) if prev > utc => prev,
            _ => utc,
        });
    }
}

/// Decide whether a session falls inside the `--days N` window.
fn within_window(s: &SessionStats, days: Option<u32>) -> bool {
    let Some(days) = days else {
        return true;
    };
    let cutoff = Utc::now() - chrono::Duration::days(days as i64);
    if let Some(last) = s.last_ts {
        return last >= cutoff;
    }
    // Fallback to mtime when no timestamps were extractable.
    if let Some(mtime) = s.mtime {
        if let Ok(d) = mtime.duration_since(SystemTime::UNIX_EPOCH) {
            let dt = DateTime::<Utc>::from_timestamp(d.as_secs() as i64, 0);
            if let Some(dt) = dt {
                return dt >= cutoff;
            }
        }
    }
    false
}

#[derive(Debug, Default, Serialize)]
struct Aggregated {
    sessions: Vec<SessionStats>,
    /// Reflects `--days` filter applied to `sessions`.
    days_window: Option<u32>,
    all_projects: bool,
}

impl Aggregated {
    fn totals(&self) -> SessionStats {
        let mut t = SessionStats::default();
        for s in &self.sessions {
            t.user_turns += s.user_turns;
            t.assistant_turns += s.assistant_turns;
            t.input_tokens += s.input_tokens;
            t.output_tokens += s.output_tokens;
            t.cache_creation_tokens += s.cache_creation_tokens;
            t.cache_read_tokens += s.cache_read_tokens;
            t.cost_usd += s.cost_usd;
            t.tool_calls += s.tool_calls;
            for (name, count) in &s.tool_counts {
                *t.tool_counts.entry(name.clone()).or_insert(0) += count;
            }
        }
        t
    }
}

fn aggregate(args: &Args, ctx: &CommandContext) -> Aggregated {
    let paths = collect_jsonl_paths(&ctx.working_dir, args.all_projects);
    let mut sessions: Vec<SessionStats> = Vec::with_capacity(paths.len());

    for (project_dir, path) in paths {
        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();

        let entries = parse_jsonl_sync(&path);
        let mut stats =
            session_stats_from_entries(session_id, project_dir, path.clone(), mtime, &entries);

        // Tail-read fallback for last_prompt / title when no metadata entries
        // appeared in the parsed payload.
        if stats.last_prompt.is_none() || stats.title.is_none() {
            let (lp, title) = read_session_tail_metadata_sync(&path);
            if stats.last_prompt.is_none() {
                stats.last_prompt = lp;
            }
            if stats.title.is_none() {
                stats.title = title;
            }
        }

        sessions.push(stats);
    }

    // Apply --days filter
    sessions.retain(|s| within_window(s, args.days));

    Aggregated {
        sessions,
        days_window: args.days,
        all_projects: args.all_projects,
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn fmt_num(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn fmt_cost(usd: f64) -> String {
    if usd >= 1.0 {
        format!("${usd:.2}")
    } else if usd >= 0.001 {
        format!("${usd:.4}")
    } else if usd > 0.0 {
        format!("${usd:.6}")
    } else {
        "$0.00".to_string()
    }
}

fn fmt_duration_secs(secs: i64) -> String {
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn fmt_relative_time(ts: DateTime<Utc>) -> String {
    let now = Utc::now();
    let delta = (now - ts).num_seconds();
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else if delta < 86_400 * 7 {
        format!("{}d ago", delta / 86_400)
    } else {
        ts.format("%Y-%m-%d").to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else if max <= 1 {
        chars.iter().take(max).collect::<String>()
    } else {
        let mut out: String = chars.iter().take(max - 1).collect();
        out.push('…');
        out
    }
}

fn bar(value: u64, max: u64, width: usize) -> String {
    if max == 0 {
        return " ".repeat(width);
    }
    let filled = ((value as f64 / max as f64) * width as f64).round() as usize;
    let filled = filled.min(width);
    let mut s = String::with_capacity(width);
    for _ in 0..filled {
        s.push('█');
    }
    for _ in 0..(width - filled) {
        s.push(' ');
    }
    s
}

fn session_label(s: &SessionStats) -> String {
    if let Some(t) = &s.title {
        if !t.trim().is_empty() {
            return truncate(t, 40);
        }
    }
    if let Some(p) = &s.last_prompt {
        if !p.trim().is_empty() {
            return truncate(p.trim(), 40);
        }
    }
    truncate(&s.session_id, 40)
}

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

fn header(title: &str) -> String {
    format!("{title}\n{}", "═".repeat(title.chars().count().max(4)))
}

fn render_scope_line(agg: &Aggregated, ctx: &CommandContext) -> String {
    let scope = if agg.all_projects {
        "all projects".to_string()
    } else {
        format!("project {}", ctx.working_dir.display())
    };
    let window = match agg.days_window {
        Some(d) => format!("last {d} day{}", if d == 1 { "" } else { "s" }),
        None => "all time".to_string(),
    };
    format!("Scope: {scope} · {window} · {} sessions", agg.sessions.len())
}

fn render_summary(agg: &Aggregated, ctx: &CommandContext) -> String {
    if agg.sessions.is_empty() {
        return format!(
            "{}\n\n{}\n\nNo sessions found.\n\nLooked under {}.\n\
             Try `claurst stats --all-projects` to widen the scope.",
            header("Claurst Session Stats"),
            render_scope_line(agg, ctx),
            projects_dir().display(),
        );
    }

    let totals = agg.totals();
    let mut out = String::new();
    out.push_str(&header("Claurst Session Stats"));
    out.push('\n');
    out.push_str(&render_scope_line(agg, ctx));
    out.push_str("\n\n");

    // Totals block
    out.push_str("Totals\n");
    out.push_str("──────\n");
    out.push_str(&format!(
        "  Sessions:           {:>10}\n",
        agg.sessions.len()
    ));
    out.push_str(&format!(
        "  User turns:         {:>10}\n",
        totals.user_turns
    ));
    out.push_str(&format!(
        "  Assistant turns:    {:>10}\n",
        totals.assistant_turns
    ));
    out.push_str(&format!(
        "  Tool calls:         {:>10}\n",
        totals.tool_calls
    ));
    out.push_str(&format!(
        "  Cost:               {:>10}\n",
        fmt_cost(totals.cost_usd)
    ));

    // Token block
    out.push_str("\nTokens\n──────\n");
    out.push_str(&format!(
        "  Input:              {:>10}\n",
        fmt_num(totals.input_tokens)
    ));
    out.push_str(&format!(
        "  Output:             {:>10}\n",
        fmt_num(totals.output_tokens)
    ));
    if totals.cache_creation_tokens > 0 || totals.cache_read_tokens > 0 {
        out.push_str(&format!(
            "  Cache write:        {:>10}\n",
            fmt_num(totals.cache_creation_tokens)
        ));
        out.push_str(&format!(
            "  Cache read:         {:>10}\n",
            fmt_num(totals.cache_read_tokens)
        ));
    }
    out.push_str(&format!(
        "  Total:              {:>10}\n",
        fmt_num(totals.total_tokens())
    ));

    // Top tools
    if !totals.tool_counts.is_empty() {
        out.push_str("\nTop tools\n─────────\n");
        let mut tools: Vec<_> = totals.tool_counts.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1));
        let max_count = tools.first().map(|(_, c)| **c).unwrap_or(1);
        for (name, count) in tools.iter().take(10) {
            out.push_str(&format!(
                "  {:<24} {:>6}  {}\n",
                truncate(name, 24),
                count,
                bar(**count, max_count, 20)
            ));
        }
    }

    // Recent sessions
    let mut recent: Vec<&SessionStats> = agg.sessions.iter().collect();
    recent.sort_by(|a, b| b.last_ts.cmp(&a.last_ts).then(b.mtime.cmp(&a.mtime)));
    let recent_n = recent.iter().take(5).collect::<Vec<_>>();
    if !recent_n.is_empty() {
        out.push_str("\nRecent sessions\n───────────────\n");
        for s in recent_n {
            let when = s
                .last_ts
                .map(fmt_relative_time)
                .unwrap_or_else(|| "—".to_string());
            out.push_str(&format!(
                "  {:>10}  {:<40}  {}\n",
                when,
                session_label(s),
                fmt_cost(s.cost_usd)
            ));
        }
    }

    out.push_str(
        "\nTry: claurst stats sessions · claurst stats tools · claurst stats daily\n",
    );
    out
}

fn render_sessions(agg: &Aggregated, top: Option<usize>, ctx: &CommandContext) -> String {
    if agg.sessions.is_empty() {
        return format!(
            "{}\n\n{}\n\nNo sessions in scope.",
            header("Sessions"),
            render_scope_line(agg, ctx),
        );
    }

    let mut sessions: Vec<&SessionStats> = agg.sessions.iter().collect();
    // Sort by cost desc, then total tokens desc, then last_ts desc.
    sessions.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.total_tokens().cmp(&a.total_tokens()))
            .then_with(|| b.last_ts.cmp(&a.last_ts))
    });

    let shown: Vec<&&SessionStats> = match top {
        Some(n) => sessions.iter().take(n).collect(),
        None => sessions.iter().collect(),
    };

    let mut out = String::new();
    out.push_str(&header("Sessions"));
    out.push('\n');
    out.push_str(&render_scope_line(agg, ctx));
    out.push_str("\n\n");
    out.push_str(&format!(
        "  {:<10}  {:<40}  {:>7}  {:>10}  {:>5}\n",
        "When", "Title / prompt", "Cost", "Tokens", "Tools"
    ));
    out.push_str(&format!(
        "  {}  {}  {}  {}  {}\n",
        "─".repeat(10),
        "─".repeat(40),
        "─".repeat(7),
        "─".repeat(10),
        "─".repeat(5),
    ));
    for s in &shown {
        let when = s
            .last_ts
            .map(fmt_relative_time)
            .unwrap_or_else(|| "—".to_string());
        out.push_str(&format!(
            "  {:<10}  {:<40}  {:>7}  {:>10}  {:>5}\n",
            truncate(&when, 10),
            session_label(s),
            fmt_cost(s.cost_usd),
            fmt_num(s.total_tokens()),
            s.tool_calls,
        ));
    }
    if let Some(n) = top {
        if sessions.len() > n {
            out.push_str(&format!(
                "\n  … {} more session(s) hidden. Use `claurst stats sessions` (no --top) to see all.\n",
                sessions.len() - n
            ));
        }
    }
    out.push_str(
        "\nUse `claurst stats session <id>` to drill into a session.\n",
    );
    out
}

fn render_tools(agg: &Aggregated, top: Option<usize>, ctx: &CommandContext) -> String {
    let totals = agg.totals();
    if totals.tool_counts.is_empty() {
        return format!(
            "{}\n\n{}\n\nNo tool calls recorded in scope.",
            header("Tool usage"),
            render_scope_line(agg, ctx),
        );
    }

    let mut tools: Vec<(&String, &u64)> = totals.tool_counts.iter().collect();
    tools.sort_by(|a, b| b.1.cmp(a.1));
    let shown: Vec<&(&String, &u64)> = match top {
        Some(n) => tools.iter().take(n).collect(),
        None => tools.iter().collect(),
    };
    let max_count = tools.first().map(|(_, c)| **c).unwrap_or(1);

    // Per-tool session reach.
    let mut session_reach: HashMap<&String, u64> = HashMap::new();
    for s in &agg.sessions {
        for name in s.tool_counts.keys() {
            *session_reach.entry(name).or_insert(0) += 1;
        }
    }

    let mut out = String::new();
    out.push_str(&header("Tool usage"));
    out.push('\n');
    out.push_str(&render_scope_line(agg, ctx));
    out.push_str("\n\n");
    out.push_str(&format!(
        "  {:<24}  {:>7}  {:>9}  {:>6}  {}\n",
        "Tool", "Calls", "Sessions", "Share", "Distribution"
    ));
    out.push_str(&format!(
        "  {}  {}  {}  {}  {}\n",
        "─".repeat(24),
        "─".repeat(7),
        "─".repeat(9),
        "─".repeat(6),
        "─".repeat(20),
    ));
    let grand_calls = totals.tool_calls.max(1);
    for (name, count) in &shown {
        let pct = (**count as f64 / grand_calls as f64) * 100.0;
        let reach = session_reach.get(name).copied().unwrap_or(0);
        out.push_str(&format!(
            "  {:<24}  {:>7}  {:>9}  {:>5.1}%  {}\n",
            truncate(name, 24),
            count,
            reach,
            pct,
            bar(**count, max_count, 20)
        ));
    }

    if let Some(n) = top {
        if tools.len() > n {
            out.push_str(&format!(
                "\n  … {} more tool(s) hidden.\n",
                tools.len() - n
            ));
        }
    }
    out
}

fn render_daily(agg: &Aggregated, ctx: &CommandContext) -> String {
    if agg.sessions.is_empty() {
        return format!(
            "{}\n\n{}\n\nNo sessions in scope.",
            header("Daily cost"),
            render_scope_line(agg, ctx),
        );
    }

    #[derive(Default)]
    struct DayRow {
        sessions: std::collections::HashSet<String>,
        assistant_turns: u64,
        input: u64,
        output: u64,
        cache_w: u64,
        cache_r: u64,
        cost: f64,
    }

    let mut days: BTreeMap<NaiveDate, DayRow> = BTreeMap::new();
    for s in &agg.sessions {
        // Bucket by last_ts day; fall back to mtime if missing.
        let date = if let Some(ts) = s.last_ts {
            ts.date_naive()
        } else if let Some(m) = s.mtime {
            let secs = m.duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            DateTime::<Utc>::from_timestamp(secs, 0)
                .map(|dt| dt.date_naive())
                .unwrap_or_else(|| Utc::now().date_naive())
        } else {
            continue;
        };
        let row = days.entry(date).or_default();
        row.sessions.insert(s.session_id.clone());
        row.assistant_turns += s.assistant_turns;
        row.input += s.input_tokens;
        row.output += s.output_tokens;
        row.cache_w += s.cache_creation_tokens;
        row.cache_r += s.cache_read_tokens;
        row.cost += s.cost_usd;
    }

    let mut out = String::new();
    out.push_str(&header("Daily cost"));
    out.push('\n');
    out.push_str(&render_scope_line(agg, ctx));
    out.push_str("\n\n");
    out.push_str(&format!(
        "  {:<10}  {:>8}  {:>8}  {:>8}  {:>8}  {:>9}  {:>7}\n",
        "Day", "Sessions", "Turns", "Input", "Output", "Cache R/W", "Cost"
    ));
    out.push_str(&format!(
        "  {}  {}  {}  {}  {}  {}  {}\n",
        "─".repeat(10),
        "─".repeat(8),
        "─".repeat(8),
        "─".repeat(8),
        "─".repeat(8),
        "─".repeat(9),
        "─".repeat(7),
    ));

    let mut grand = DayRow::default();
    for (date, row) in &days {
        let cache_rw = if row.cache_r > 0 || row.cache_w > 0 {
            format!("{}/{}", fmt_num(row.cache_r), fmt_num(row.cache_w))
        } else {
            "—".to_string()
        };
        out.push_str(&format!(
            "  {:<10}  {:>8}  {:>8}  {:>8}  {:>8}  {:>9}  {:>7}\n",
            date,
            row.sessions.len(),
            row.assistant_turns,
            fmt_num(row.input),
            fmt_num(row.output),
            cache_rw,
            fmt_cost(row.cost),
        ));
        grand.sessions.extend(row.sessions.iter().cloned());
        grand.assistant_turns += row.assistant_turns;
        grand.input += row.input;
        grand.output += row.output;
        grand.cache_w += row.cache_w;
        grand.cache_r += row.cache_r;
        grand.cost += row.cost;
    }
    out.push_str(&format!(
        "  {}  {}  {}  {}  {}  {}  {}\n",
        "═".repeat(10),
        "═".repeat(8),
        "═".repeat(8),
        "═".repeat(8),
        "═".repeat(8),
        "═".repeat(9),
        "═".repeat(7),
    ));
    let cache_rw = if grand.cache_r > 0 || grand.cache_w > 0 {
        format!("{}/{}", fmt_num(grand.cache_r), fmt_num(grand.cache_w))
    } else {
        "—".to_string()
    };
    out.push_str(&format!(
        "  {:<10}  {:>8}  {:>8}  {:>8}  {:>8}  {:>9}  {:>7}\n",
        "Total",
        grand.sessions.len(),
        grand.assistant_turns,
        fmt_num(grand.input),
        fmt_num(grand.output),
        cache_rw,
        fmt_cost(grand.cost),
    ));

    out
}

fn render_session_detail(
    agg: &Aggregated,
    session_id: &str,
    ctx: &CommandContext,
) -> String {
    let s = match agg.sessions.iter().find(|s| s.session_id == session_id) {
        Some(s) => s,
        None => {
            return format!(
                "{}\n\n{}\n\nSession '{}' not found in scope.\n\
                 If it's in another project, try --all-projects.",
                header("Session detail"),
                render_scope_line(agg, ctx),
                session_id,
            );
        }
    };

    let mut out = String::new();
    out.push_str(&header(&format!("Session {}", s.session_id)));
    out.push_str("\n\n");
    out.push_str(&format!("  Project:        {}\n", s.project_dir));
    if let Some(t) = &s.title {
        out.push_str(&format!("  Title:          {}\n", truncate(t, 60)));
    }
    if let Some(lp) = &s.last_prompt {
        out.push_str(&format!(
            "  Last prompt:    {}\n",
            truncate(lp.trim(), 60)
        ));
    }
    if let Some(first) = s.first_ts {
        out.push_str(&format!(
            "  Started:        {}  ({})\n",
            first.format("%Y-%m-%d %H:%M:%S UTC"),
            fmt_relative_time(first),
        ));
    }
    if let Some(last) = s.last_ts {
        out.push_str(&format!(
            "  Last activity:  {}  ({})\n",
            last.format("%Y-%m-%d %H:%M:%S UTC"),
            fmt_relative_time(last),
        ));
    }
    if let Some(d) = s.duration_secs() {
        out.push_str(&format!("  Duration:       {}\n", fmt_duration_secs(d)));
    }
    out.push_str(&format!("  Path:           {}\n", s.path.display()));

    out.push_str("\nConversation\n────────────\n");
    out.push_str(&format!("  User turns:        {:>8}\n", s.user_turns));
    out.push_str(&format!(
        "  Assistant turns:   {:>8}\n",
        s.assistant_turns
    ));
    out.push_str(&format!("  Tool calls:        {:>8}\n", s.tool_calls));

    out.push_str("\nTokens\n──────\n");
    out.push_str(&format!(
        "  Input:             {:>8}\n",
        fmt_num(s.input_tokens)
    ));
    out.push_str(&format!(
        "  Output:            {:>8}\n",
        fmt_num(s.output_tokens)
    ));
    if s.cache_creation_tokens > 0 || s.cache_read_tokens > 0 {
        out.push_str(&format!(
            "  Cache write:       {:>8}\n",
            fmt_num(s.cache_creation_tokens)
        ));
        out.push_str(&format!(
            "  Cache read:        {:>8}\n",
            fmt_num(s.cache_read_tokens)
        ));
    }
    out.push_str(&format!(
        "  Total:             {:>8}\n",
        fmt_num(s.total_tokens())
    ));

    out.push_str(&format!("\nCost: {}\n", fmt_cost(s.cost_usd)));

    if !s.tool_counts.is_empty() {
        out.push_str("\nTools used\n──────────\n");
        let mut tools: Vec<_> = s.tool_counts.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1));
        let max_count = tools.first().map(|(_, c)| **c).unwrap_or(1);
        for (name, count) in tools {
            out.push_str(&format!(
                "  {:<24} {:>6}  {}\n",
                truncate(name, 24),
                count,
                bar(*count, max_count, 20)
            ));
        }
    }

    out
}

#[derive(Debug, Serialize)]
struct JsonOutput<'a> {
    scope: JsonScope,
    totals: &'a SessionStats,
    sessions: &'a [SessionStats],
}

#[derive(Debug, Serialize)]
struct JsonScope {
    cwd: String,
    all_projects: bool,
    days_window: Option<u32>,
}

fn render_json(agg: &Aggregated, ctx: &CommandContext) -> String {
    let totals = agg.totals();
    let payload = JsonOutput {
        scope: JsonScope {
            cwd: ctx.working_dir.to_string_lossy().to_string(),
            all_projects: agg.all_projects,
            days_window: agg.days_window,
        },
        totals: &totals,
        sessions: &agg.sessions,
    };
    serde_json::to_string_pretty(&payload)
        .unwrap_or_else(|_| "{\"error\":\"failed to serialise stats\"}".to_string())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(raw: &[&str], ctx: &CommandContext) -> CommandResult {
    let args = match parse_args(raw) {
        Ok(a) => a,
        Err(e) => {
            // help_text() is returned via the Err path so users get it through
            // the same channel as parse errors.
            return CommandResult::Message(e);
        }
    };

    let agg = aggregate(&args, ctx);

    if args.json {
        return CommandResult::Message(render_json(&agg, ctx));
    }

    let out = match args.sub {
        Subcommand::Summary => render_summary(&agg, ctx),
        Subcommand::Sessions => render_sessions(&agg, args.top, ctx),
        Subcommand::Tools => render_tools(&agg, args.top, ctx),
        Subcommand::Daily => render_daily(&agg, ctx),
        Subcommand::SessionDetail => {
            let id = args.session_id.as_deref().unwrap_or("");
            render_session_detail(&agg, id, ctx)
        }
    };
    CommandResult::Message(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::session_storage::{
        AiTitleEntry, CustomTitleEntry, LastPromptEntry, TranscriptMessage,
    };
    use claurst_core::types::{Message, MessageContent, MessageCost, Role};
    use tempfile::TempDir;

    fn make_assistant_with_cost(
        tokens_in: u64,
        tokens_out: u64,
        cost: f64,
        tools: &[&str],
        timestamp: &str,
    ) -> TranscriptEntry {
        let mut content_blocks = vec![ContentBlock::Text {
            text: "assistant reply".to_string(),
        }];
        for (i, t) in tools.iter().enumerate() {
            content_blocks.push(ContentBlock::ToolUse {
                id: format!("tu-{i}"),
                name: t.to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            });
        }
        TranscriptEntry::Assistant(TranscriptMessage {
            uuid: Some(format!("test-{}", timestamp)),
            parent_uuid: None,
            timestamp: timestamp.to_string(),
            session_id: "sess".to_string(),
            cwd: "/proj".to_string(),
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(content_blocks),
                uuid: None,
                cost: Some(MessageCost {
                    input_tokens: tokens_in,
                    output_tokens: tokens_out,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    cost_usd: cost,
                }),
                snapshot_patch: None,
            },
            is_sidechain: false,
            user_type: "external".to_string(),
            version: "test".to_string(),
            git_branch: None,
            agent_role: None,
            managed_session_id: None,
            extra: Default::default(),
        })
    }

    fn make_user(timestamp: &str) -> TranscriptEntry {
        TranscriptEntry::User(TranscriptMessage {
            uuid: Some(format!("test-{}", timestamp)),
            parent_uuid: None,
            timestamp: timestamp.to_string(),
            session_id: "sess".to_string(),
            cwd: "/proj".to_string(),
            message: Message::user("hi"),
            is_sidechain: false,
            user_type: "external".to_string(),
            version: "test".to_string(),
            git_branch: None,
            agent_role: None,
            managed_session_id: None,
            extra: Default::default(),
        })
    }

    async fn build_fixture_session(
        dir: &Path,
        session_id: &str,
        entries: Vec<TranscriptEntry>,
    ) -> PathBuf {
        let path = dir.join(format!("{session_id}.jsonl"));
        // Write the whole fixture synchronously in one shot. The reader
        // (`aggregate_from_dir` / `parse_jsonl_sync`) uses blocking `std::fs`,
        // so writing each line via the async `write_transcript_entry`
        // (open/append/close per entry on the blocking pool) left a
        // write-then-read visibility gap that dropped trailing lines on the
        // loaded Linux CI runner — non-deterministically failing the turn count
        // and last-prompt assertions. A single sync write removes that race.
        let mut buf = String::new();
        for e in &entries {
            buf.push_str(&serde_json::to_string(e).unwrap());
            buf.push('\n');
        }
        std::fs::write(&path, buf).unwrap();
        path
    }

    fn aggregate_from_dir(dir: &Path) -> Aggregated {
        let entries_paths: Vec<(String, PathBuf)> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
                    .then_some(("test-proj".to_string(), p))
            })
            .collect();

        let mut sessions = Vec::new();
        for (project_dir, path) in entries_paths {
            let session_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap()
                .to_string();
            let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
            let entries = parse_jsonl_sync(&path);
            let mut stats = session_stats_from_entries(
                session_id,
                project_dir,
                path.clone(),
                mtime,
                &entries,
            );
            if stats.last_prompt.is_none() || stats.title.is_none() {
                let (lp, t) = read_session_tail_metadata_sync(&path);
                if stats.last_prompt.is_none() {
                    stats.last_prompt = lp;
                }
                if stats.title.is_none() {
                    stats.title = t;
                }
            }
            sessions.push(stats);
        }
        Aggregated {
            sessions,
            days_window: None,
            all_projects: false,
        }
    }

    #[tokio::test]
    async fn aggregates_tokens_cost_and_tools() {
        let tmp = TempDir::new().unwrap();
        build_fixture_session(
            tmp.path(),
            "sess-a",
            vec![
                make_user("2024-01-15T10:00:00Z"),
                make_assistant_with_cost(
                    100,
                    50,
                    0.0025,
                    &["bash", "read"],
                    "2024-01-15T10:00:05Z",
                ),
                make_user("2024-01-15T10:01:00Z"),
                make_assistant_with_cost(
                    200,
                    80,
                    0.005,
                    &["bash"],
                    "2024-01-15T10:01:10Z",
                ),
            ],
        )
        .await;

        let agg = aggregate_from_dir(tmp.path());
        assert_eq!(agg.sessions.len(), 1);
        let s = &agg.sessions[0];
        assert_eq!(s.user_turns, 2);
        assert_eq!(s.assistant_turns, 2);
        assert_eq!(s.input_tokens, 300);
        assert_eq!(s.output_tokens, 130);
        assert!((s.cost_usd - 0.0075).abs() < 1e-9);
        assert_eq!(s.tool_calls, 3);
        assert_eq!(s.tool_counts.get("bash"), Some(&2));
        assert_eq!(s.tool_counts.get("read"), Some(&1));
        // Timestamps were parsed.
        assert!(s.first_ts.is_some());
        assert!(s.last_ts.is_some());
        assert!(s.duration_secs().unwrap() >= 60);
    }

    #[tokio::test]
    async fn empty_session_yields_zero_totals() {
        let tmp = TempDir::new().unwrap();
        build_fixture_session(tmp.path(), "empty", vec![]).await;
        let agg = aggregate_from_dir(tmp.path());
        // An empty file produces zero sessions because read_dir returns no
        // .jsonl when no entries were ever written.
        // Actually `write_transcript_entry` with empty Vec writes nothing, so
        // the file may not exist — accept either 0 or a zeroed entry.
        if !agg.sessions.is_empty() {
            let s = &agg.sessions[0];
            assert_eq!(s.user_turns, 0);
            assert_eq!(s.assistant_turns, 0);
            assert_eq!(s.input_tokens, 0);
        }
    }

    #[tokio::test]
    async fn tail_metadata_falls_back_to_explicit_entries() {
        let tmp = TempDir::new().unwrap();
        build_fixture_session(
            tmp.path(),
            "with-title",
            vec![
                make_assistant_with_cost(10, 10, 0.0, &[], "2024-01-15T10:00:00Z"),
                TranscriptEntry::CustomTitle(CustomTitleEntry {
                    session_id: "with-title".to_string(),
                    custom_title: "My research session".to_string(),
                }),
                TranscriptEntry::LastPrompt(LastPromptEntry {
                    session_id: "with-title".to_string(),
                    last_prompt: "Last thing the user asked".to_string(),
                }),
            ],
        )
        .await;

        let agg = aggregate_from_dir(tmp.path());
        let s = &agg.sessions[0];
        assert_eq!(s.title.as_deref(), Some("My research session"));
        assert_eq!(s.last_prompt.as_deref(), Some("Last thing the user asked"));
    }

    #[tokio::test]
    async fn ai_title_fills_when_no_custom_title() {
        let tmp = TempDir::new().unwrap();
        build_fixture_session(
            tmp.path(),
            "with-ai-title",
            vec![
                make_assistant_with_cost(10, 10, 0.0, &[], "2024-01-15T10:00:00Z"),
                TranscriptEntry::AiTitle(AiTitleEntry {
                    session_id: "with-ai-title".to_string(),
                    ai_title: "Refactor query loop".to_string(),
                }),
            ],
        )
        .await;
        let agg = aggregate_from_dir(tmp.path());
        let s = &agg.sessions[0];
        assert_eq!(s.title.as_deref(), Some("Refactor query loop"));
    }

    #[test]
    fn fmt_num_buckets() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(999), "999");
        assert_eq!(fmt_num(1_000), "1.0K");
        assert_eq!(fmt_num(1_500), "1.5K");
        assert_eq!(fmt_num(1_500_000), "1.5M");
        assert_eq!(fmt_num(2_500_000_000), "2.5B");
    }

    #[test]
    fn fmt_cost_precision() {
        assert_eq!(fmt_cost(0.0), "$0.00");
        assert_eq!(fmt_cost(0.0001), "$0.000100");
        assert_eq!(fmt_cost(0.005), "$0.0050");
        assert_eq!(fmt_cost(12.345), "$12.35");
    }

    #[test]
    fn bar_width_clamp() {
        assert_eq!(bar(10, 10, 5), "█████");
        assert_eq!(bar(0, 10, 5), "     ");
        assert_eq!(bar(5, 10, 4).chars().filter(|c| *c == '█').count(), 2);
        // max=0 → empty.
        assert_eq!(bar(5, 0, 3), "   ");
    }

    #[test]
    fn truncate_respects_max() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        // "héllo" = 5 chars → take 3 ("hél") + "…" = "hél…"
        assert_eq!(truncate("héllo", 4), "hél…");
    }

    #[test]
    fn parse_args_defaults_to_summary() {
        let a = parse_args(&[]).unwrap();
        assert_eq!(a.sub, Subcommand::Summary);
        assert!(!a.all_projects);
        assert!(!a.json);
        assert!(a.days.is_none());
        assert!(a.top.is_none());
    }

    #[test]
    fn parse_args_flags() {
        let a = parse_args(&["sessions", "--days", "14", "--top", "5", "-a", "--json"]).unwrap();
        assert_eq!(a.sub, Subcommand::Sessions);
        assert_eq!(a.days, Some(14));
        assert_eq!(a.top, Some(5));
        assert!(a.all_projects);
        assert!(a.json);
    }

    #[test]
    fn parse_args_session_detail_requires_id() {
        let err = parse_args(&["session"]).unwrap_err();
        assert!(err.contains("session-id"));
        let a = parse_args(&["session", "abc-123"]).unwrap();
        assert_eq!(a.sub, Subcommand::SessionDetail);
        assert_eq!(a.session_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn parse_args_unknown_subcommand_errors() {
        let err = parse_args(&["weird"]).unwrap_err();
        assert!(err.contains("Unknown subcommand"));
    }

    #[test]
    fn parse_args_help_returns_help_text() {
        let err = parse_args(&["--help"]).unwrap_err();
        assert!(err.contains("Subcommands"));
    }
}
