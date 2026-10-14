// Auto-compact service for cc-query.
//
// When the conversation context window fills up (~90%+), we automatically
// summarise older messages to free space. This mirrors the TypeScript
// autoCompact / compact service behaviour.
//
// Strategy:
//   1. Keep as many recent messages as fit a `KEEP_RECENT_TOKENS` budget
//      verbatim (mirrors pi's `keepRecentTokens`), rather than a fixed message
//      COUNT. The cut is snapped to a tool_use↔tool_result-safe round boundary.
//   2. Summarise everything older than that recent tail.
//   3. Replace the head of the conversation with a single synthetic
//      <compact-summary> user message, followed by the recent tail.
//
// The summary is generated in a single non-agentic, tool-less call through
// the active `LlmProvider`, so compaction works on every provider and doesn't
// trigger another compaction recursively.
//
// MicroCompact strategy (partial compaction):
//   When context is above `trigger_threshold` but not yet at the full
//   auto-compact level, we summarise only the oldest messages while keeping
//   the most recent `keep_recent_messages` intact.  This is lighter than a
//   full compaction and can fire proactively at 75 % capacity.

use claurst_api::provider_types::StopReason;
use claurst_api::{LlmProvider, ProviderRequest, StreamEvent, SystemPrompt};
use claurst_core::error::ClaudeError;
pub use claurst_core::history::{CompactionRecord, CompactionTrigger};
use claurst_core::types::{ContentBlock, Message, MessageContent, Role};
use futures::StreamExt;
use serde_json::Value;
use std::collections::BTreeSet;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Constants (mirrors TypeScript autoCompact.ts)
// ---------------------------------------------------------------------------

/// We target keeping this many context tokens free after compaction.
#[allow(dead_code)]
const AUTOCOMPACT_BUFFER_TOKENS: u64 = 13_000;

/// Start warning when this many tokens remain in the context window.
const WARNING_THRESHOLD_BUFFER_TOKENS: u64 = 20_000;

/// Fraction of the context window at which auto-compact triggers.
const AUTOCOMPACT_TRIGGER_FRACTION: f64 = 0.90;

/// Token budget for the recent tail we preserve verbatim after compaction.
///
/// Instead of keeping a fixed COUNT of recent messages, we keep as many recent
/// messages as fit within this many tokens (mirrors pi's `keepRecentTokens`,
/// which defaults to 20k). Keeping the tail token-budgeted means a handful of
/// huge tool results don't blow the kept context, and many tiny turns aren't
/// prematurely summarised. The cut is always snapped to a
/// tool_use↔tool_result-safe boundary via [`compute_keep_split_index`].
const KEEP_RECENT_TOKENS: u64 = 16_000;

/// Max consecutive auto-compact failures before giving up (circuit breaker).
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

// Percentage thresholds for token warning states (mirrors TS autoCompact.ts)
const WARNING_PCT: f64 = 0.80;   // 80 % full → yellow warning
const CRITICAL_PCT: f64 = 0.95;  // 95 % full → red critical

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Tracks auto-compact state across turns.
#[derive(Debug, Default, Clone)]
pub struct AutoCompactState {
    /// Total compactions performed this session.
    pub compaction_count: u32,
    /// Consecutive failures (reset on success).
    pub consecutive_failures: u32,
    /// Whether the circuit breaker is open (too many failures).
    pub disabled: bool,
}

impl AutoCompactState {
    /// Record a successful compaction.
    pub fn on_success(&mut self) {
        self.compaction_count += 1;
        self.consecutive_failures = 0;
    }

    /// Record a failed compaction; open circuit breaker if too many.
    pub fn on_failure(&mut self) {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            warn!(
                failures = self.consecutive_failures,
                "Auto-compact circuit breaker opened – disabling for this session"
            );
            self.disabled = true;
        }
    }
}

/// Token-usage state relative to the context window.
/// Matches the TypeScript TokenWarningState semantics:
///   Ok      = below 80 % of context window
///   Warning = 80–95 % ("yellow" in TUI)
///   Critical= above 95 % ("red" in TUI)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenWarningState {
    /// Plenty of space left.
    Ok,
    /// Getting close – warn the user (≥ 80 %).
    Warning,
    /// Critical – compact now (≥ 95 %).
    Critical,
}

// ---------------------------------------------------------------------------
// Message grouping (from TypeScript grouping.ts)
// ---------------------------------------------------------------------------

/// A semantically coherent chunk of messages suitable for individual
/// summarisation.  Groups are formed at API-round boundaries: one group per
/// assistant response, which naturally pairs every tool_use with its result.
#[derive(Debug, Clone)]
pub struct MessageGroup {
    pub messages: Vec<Message>,
    /// First file path or tool name mentioned in this group, if any.
    pub topic_hint: Option<String>,
    /// Rough token estimate for the group (chars / 4, padded by 4/3).
    pub token_estimate: usize,
}

impl MessageGroup {
    fn from_messages(messages: Vec<Message>) -> Self {
        let topic_hint = extract_topic_hint(&messages);
        let token_estimate = estimate_tokens_for_messages(&messages);
        Self { messages, topic_hint, token_estimate }
    }
}

/// Extract a short "topic hint" from a group: first file path or tool name
/// mentioned in any tool_use or tool_result block.
fn extract_topic_hint(messages: &[Message]) -> Option<String> {
    for msg in messages {
        let blocks = match &msg.content {
            MessageContent::Blocks(b) => b,
            _ => continue,
        };
        for block in blocks {
            if let ContentBlock::ToolUse { name, input, .. } = block {
                // Try to get a file_path from input, else use tool name
                if let Some(fp) = input.get("file_path").and_then(|v| v.as_str()) {
                    return Some(fp.to_string());
                }
                if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                    // Use first word of command as hint
                    let first_word = cmd.split_whitespace().next().unwrap_or(cmd);
                    return Some(first_word.to_string());
                }
                return Some(name.clone());
            }
        }
    }
    None
}

/// Rough token estimate: sum of character lengths divided by 4, padded by 4/3.
fn estimate_tokens_for_messages(messages: &[Message]) -> usize {
    let chars: usize = messages
        .iter()
        .map(|m| match &m.content {
            MessageContent::Text(t) => t.len(),
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .map(estimate_block_chars)
                .sum(),
        })
        .sum();
    // chars / 4 = rough tokens, then * 4/3 padding
    (chars / 4) * 4 / 3
}

fn estimate_block_chars(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.len(),
        ContentBlock::ToolUse { name, input, .. } => {
            name.len() + input.to_string().len()
        }
        ContentBlock::ToolResult { content, .. } => match content {
            claurst_core::types::ToolResultContent::Text(t) => t.len(),
            claurst_core::types::ToolResultContent::Blocks(blocks) => {
                blocks.iter().map(estimate_block_chars).sum()
            }
        },
        ContentBlock::Thinking { thinking, .. } => thinking.len(),
        ContentBlock::RedactedThinking { data } => data.len(),
        _ => 200, // default for images/documents
    }
}

/// Group messages at API-round boundaries: one group per assistant response.
/// This mirrors `groupMessagesByApiRound` from TypeScript grouping.ts.
///
/// Each group represents one complete API round:
///   [user_messages..., assistant_response]
///
/// Boundary detection:
/// - When messages have UUIDs, a new group fires at the START of each new
///   assistant message whose UUID differs from the previous one.
/// - When messages lack UUIDs (local / test messages), boundaries fire
///   when an assistant message follows a PREVIOUS assistant in the current
///   group — i.e. each assistant turn closes its own group.
///
/// The result is that user messages are grouped with the SUBSEQUENT assistant
/// response that replies to them (matching TypeScript round semantics).
pub fn group_messages_for_compact(messages: &[Message]) -> Vec<MessageGroup> {
    let mut groups: Vec<MessageGroup> = Vec::new();
    let mut current: Vec<Message> = Vec::new();

    for msg in messages {
        if msg.role == Role::Assistant {
            // Add this assistant message to the current group (with any
            // accumulated user messages from this round).
            current.push(msg.clone());

            // Close the group: the next user message(s) belong to the next round.
            groups.push(MessageGroup::from_messages(current.clone()));
            current.clear();
        } else {
            current.push(msg.clone());
        }
    }

    // Any trailing non-assistant messages (shouldn't happen in practice)
    // form their own group.
    if !current.is_empty() {
        groups.push(MessageGroup::from_messages(current));
    }

    groups
}

// ---------------------------------------------------------------------------
// MicroCompact configuration & logic
// ---------------------------------------------------------------------------

/// Configuration for micro-compaction (partial, proactive summarisation).
#[derive(Debug, Clone)]
pub struct MicroCompactConfig {
    /// Compact when context is this fraction full (e.g. 0.75 = 75 %).
    pub trigger_threshold: f32,
    /// Always keep this many recent messages verbatim.
    pub keep_recent_messages: usize,
    /// Target token count for the generated summary.
    pub summary_target_tokens: usize,
}

impl Default for MicroCompactConfig {
    fn default() -> Self {
        Self {
            trigger_threshold: 0.75,
            keep_recent_messages: 10,
            summary_target_tokens: 2048,
        }
    }
}

/// Attempt a micro-compact if the context is above `config.trigger_threshold`.
///
/// Returns `Some(new_messages)` when compaction occurred, `None` otherwise.
pub async fn micro_compact_if_needed(
    provider: &dyn LlmProvider,
    messages: &[Message],
    input_tokens: u64,
    model: &str,
    config: &MicroCompactConfig,
) -> Option<Vec<Message>> {
    let window = context_window_for_model(model);
    let pct_used = input_tokens as f64 / window as f64;

    if pct_used < config.trigger_threshold as f64 {
        return None;
    }

    let total = messages.len();
    if total <= config.keep_recent_messages + 1 {
        return None;
    }

    let split_at = total.saturating_sub(config.keep_recent_messages);

    info!(
        input_tokens,
        pct_used = format!("{:.1}%", pct_used * 100.0),
        split_at,
        keep = config.keep_recent_messages,
        "MicroCompact triggered"
    );

    let target_tokens = config.summary_target_tokens as u32;
    match summarise_head(provider, model, messages, split_at, target_tokens).await {
        Ok(new_msgs) => {
            info!(
                original = total,
                compacted = new_msgs.len(),
                "MicroCompact complete"
            );
            Some(new_msgs)
        }
        Err(e) => {
            warn!(error = %e, "MicroCompact failed");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Compaction prompt (matches TypeScript prompt.ts)
// ---------------------------------------------------------------------------

/// The critical preamble that prevents the summariser from making tool calls.
const NO_TOOLS_PREAMBLE: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\
\n\
- Do NOT use Read, Bash, Grep, Glob, Edit, Write, or ANY other tool.\n\
- You already have all the context you need in the conversation above.\n\
- Tool calls will be REJECTED and will waste your only turn — you will fail the task.\n\
- Your entire response must be plain text: an <analysis> block followed by a <summary> block.\n\
\n";

/// The trailing reminder that reinforces the no-tools instruction.
const NO_TOOLS_TRAILER: &str = "\n\nREMINDER: Do NOT call any tools. Respond with plain text only — \
an <analysis> block followed by a <summary> block. \
Tool calls will be rejected and you will fail the task.";

/// Opening of a from-scratch compaction prompt.
const BASE_COMPACT_INTRO: &str = "Your task is to summarize the conversation in the \
<conversation_to_summarize> block into a structured summary that will be given to another agent \
to resume the work.";

/// Opening of an iterative UPDATE compaction prompt, used when a prior
/// `<compact-summary>` already exists in the history: the model folds the new
/// activity into the previous summary instead of re-summarising from scratch.
/// Mirrors opencode's update prompt (packages/core/src/session/compaction.ts).
const UPDATE_COMPACT_INTRO: &str = "Your task is to UPDATE an existing conversation summary into one \
consolidated summary. The previous summary is provided in <previous-summary> tags; the newer \
messages to incorporate are in the <conversation_to_summarize> block.\n\
\n\
Newer history always takes precedence over the existing summary. Preserve previous information \
unless newer history clearly contradicts, supersedes, resolves, or makes it stale. If something is \
no longer relevant to continuing the work, you may remove it.\n\
\n\
Incorporate newer requirements, decisions, progress, and context. Reconcile Work State and Next \
Move: move completed work out of Active, remove resolved blockers and answered questions, and \
preserve unresolved or pending work.";

/// What the summary covers (opencode's scope sentence: without it summaries
/// restated conventions and grew on every update).
const SUMMARY_SCOPE: &str = "Summarize only what the user and the assistant said and did. Leave out \
instructions and setup the assistant was given rather than told by the user: repository \
conventions, instruction files such as AGENTS.md, and environment details like the session ID. The \
next agent receives current versions of all of these separately.";

/// The terse summary template (opencode's SUMMARY_TEMPLATE).
const SUMMARY_TEMPLATE: &str = "Use this format for the summary (you may omit sections that aren't \
applicable):\n\
\n\
## Objective\n\
- [one or two brief sentences describing what the user is trying to accomplish]\n\
\n\
## Requirements\n\
- [constraints, preferences, requirements, and scope boundaries stated by the user, or \"(none)\"]\n\
\n\
## Decisions\n\
- [decisions already made and why, or \"(none)\"]\n\
\n\
## Work State\n\
Break the objective into smaller goals and report which are completed, which are being worked on, \
and which are blocked.\n\
### Completed\n\
- [goals that have been completed; otherwise \"(none)\"]\n\
\n\
### Active\n\
- [goals currently being worked on; otherwise \"(none)\"]\n\
\n\
### Blocked\n\
- [anything blocking progress, and why; otherwise \"(none)\"]\n\
\n\
## Next Move\n\
1. [ordered list of next actions, or \"(none)\"]\n\
\n\
## Relevant Files\n\
List the files and directories, other than the current working directory, that another agent would \
need to open to continue this work. Include at most 15, most important first. Do not list every \
file that was read or changed. If none, write \"(none)\".\n\
- `[file or directory path]`: [brief reason it matters]\n\
\n\
## Important Context\n\
- [facts the next agent cannot continue without and cannot easily find on its own; or \"(none)\"]";

/// Rules for the summary body (opencode's SUMMARY_RULES).
const SUMMARY_RULES: &str = "Rules:\n\
- Keep each section concise. Use terse, single-line bullets, not prose paragraphs or nested lists.\n\
- Prefer short references over detailed restatement. It is fine to leave out information the next \
agent can recover from the code or the files listed above.\n\
- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers.\n\
- Carry forward only user questions or requests that remain unanswered or require further action. \
Do not repeat ones that newer history has answered or resolved. Preserve exact wording when \
carrying one forward.\n\
- Preserve consequential workflow state, including whether changes are uncommitted, committed, \
pushed, under review, or merged.\n\
- Do not mention the summary process or that context was compacted.";

/// Output framing: the <analysis> scratchpad is stripped by
/// [`format_compact_summary`]; the summary itself goes in <summary> tags.
const SUMMARY_OUTPUT_FORMAT: &str = "First think briefly inside <analysis> tags (this part is \
discarded). Then return only the structured summary inside <summary> tags, with no preamble or \
commentary. Do not continue the task.";

/// Build the compaction prompt, optionally with custom instructions appended.
///
/// When `previous_summary` is a non-empty prior summary, the iterative
/// [`UPDATE_COMPACT_INTRO`] variant is selected so the model folds the
/// previous summary forward rather than re-summarising from scratch.
/// Otherwise the from-scratch [`BASE_COMPACT_INTRO`] is used. Both share the
/// terse template, rules and scope sentence ported from opencode.
pub fn get_compact_prompt(
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
) -> String {
    let is_update = previous_summary
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let intro = if is_update {
        UPDATE_COMPACT_INTRO
    } else {
        BASE_COMPACT_INTRO
    };
    let mut prompt = format!(
        "{}{}\n\n{}\n\n{}\n\n{}\n\n{}",
        NO_TOOLS_PREAMBLE, intro, SUMMARY_SCOPE, SUMMARY_TEMPLATE, SUMMARY_RULES, SUMMARY_OUTPUT_FORMAT
    );

    if let Some(instructions) = custom_instructions {
        let trimmed = instructions.trim();
        if !trimmed.is_empty() {
            prompt.push_str(&format!("\n\nAdditional Instructions:\n{}", trimmed));
        }
    }

    prompt.push_str(NO_TOOLS_TRAILER);
    prompt
}

/// Scan a slice of messages for the most recent `<compact-summary>…</compact-summary>`
/// block and return its inner text. This is how a compaction detects that a
/// PRIOR summary already exists in the history (injected by an earlier
/// compaction), so it can fold it forward via the UPDATE prompt instead of
/// re-summarising from zero.
fn extract_previous_summary(messages: &[Message]) -> Option<String> {
    const OPEN: &str = "<compact-summary>";
    const CLOSE: &str = "</compact-summary>";
    // Search newest-first so the most recent summary wins.
    for msg in messages.iter().rev() {
        let text = msg.get_all_text();
        if let (Some(start), Some(end)) = (text.find(OPEN), text.find(CLOSE)) {
            if end > start {
                let inner = text[start + OPEN.len()..end].trim();
                if !inner.is_empty() {
                    return Some(inner.to_string());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Files-touched manifest (mirrors extractFileOperations / formatFileOperations)
// ---------------------------------------------------------------------------

/// Set of files the agent read / wrote / edited across a batch of history.
///
/// Sorted (`BTreeSet`) so the emitted manifest is deterministic, and unioned
/// across successive compactions so the agent never forgets what it worked on.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct FileOps {
    read: BTreeSet<String>,
    written: BTreeSet<String>,
    edited: BTreeSet<String>,
}

impl FileOps {
    fn is_empty(&self) -> bool {
        self.read.is_empty() && self.written.is_empty() && self.edited.is_empty()
    }

    /// Merge another manifest into this one (used to carry a prior manifest
    /// forward across compactions).
    fn union(&mut self, other: &FileOps) {
        self.read.extend(other.read.iter().cloned());
        self.written.extend(other.written.iter().cloned());
        self.edited.extend(other.edited.iter().cloned());
    }

    /// Compute the final `(read_only, modified)` lists: a file that was written
    /// or edited is "modified" (and dropped from the read-only list even if it
    /// was also read). Mirrors pi's `computeFileLists`.
    fn computed_lists(&self) -> (Vec<String>, Vec<String>) {
        let mut modified: BTreeSet<String> = self.edited.clone();
        modified.extend(self.written.iter().cloned());
        let read_only: Vec<String> = self
            .read
            .iter()
            .filter(|f| !modified.contains(*f))
            .cloned()
            .collect();
        (read_only, modified.into_iter().collect())
    }
}

/// Cap on how many files to list per bucket in the manifest; the overflow is
/// summarised as "(+N more)" so the manifest stays bounded across compactions.
const MAX_MANIFEST_FILES: usize = 20;

/// Header line that introduces the files-touched manifest inside a summary.
const FILES_TOUCHED_HEADER: &str = "Files touched:";

/// Delimiter between file paths in a manifest line. A ` | ` separator keeps the
/// manifest re-parseable (paths effectively never contain it).
const MANIFEST_SEP: &str = " | ";

/// Extract file read/write/edit operations from the tool calls in `messages`.
///
/// Classifies by tool name (`Read` → read, `Write` → written, `Edit` /
/// `BatchEdit` / `NotebookEdit` / `ApplyPatch` → edited) and pulls the path from
/// the tool input (`file_path`, falling back to `path` / `notebook_path`, and
/// the per-edit `file_path`s inside a `BatchEdit`).
fn extract_file_operations(messages: &[Message]) -> FileOps {
    let mut ops = FileOps::default();
    for msg in messages {
        if let MessageContent::Blocks(blocks) = &msg.content {
            for block in blocks {
                if let ContentBlock::ToolUse { name, input, .. } = block {
                    collect_file_op(name, input, &mut ops);
                }
            }
        }
    }
    ops
}

/// Pull the file path(s) touched by a single tool call into `ops`.
fn collect_file_op(name: &str, input: &Value, ops: &mut FileOps) {
    use claurst_core::constants::{
        TOOL_NAME_APPLY_PATCH, TOOL_NAME_BATCH_EDIT, TOOL_NAME_FILE_EDIT, TOOL_NAME_FILE_READ,
        TOOL_NAME_FILE_WRITE, TOOL_NAME_NOTEBOOK_EDIT,
    };

    // BatchEdit carries an array of edits, each with its own file_path.
    if name == TOOL_NAME_BATCH_EDIT {
        if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
            for edit in edits {
                if let Some(p) = edit.get("file_path").and_then(|v| v.as_str()) {
                    ops.edited.insert(p.to_string());
                }
            }
        }
        return;
    }

    let path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .or_else(|| input.get("path").and_then(|v| v.as_str()))
        .or_else(|| input.get("notebook_path").and_then(|v| v.as_str()));
    let path = match path {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return,
    };

    match name {
        TOOL_NAME_FILE_READ => {
            ops.read.insert(path);
        }
        TOOL_NAME_FILE_WRITE => {
            ops.written.insert(path);
        }
        TOOL_NAME_FILE_EDIT | TOOL_NAME_NOTEBOOK_EDIT | TOOL_NAME_APPLY_PATCH => {
            ops.edited.insert(path);
        }
        _ => {}
    }
}

/// Render one bounded, capped manifest line from a sorted file list.
fn format_manifest_line(files: &[String]) -> String {
    if files.len() <= MAX_MANIFEST_FILES {
        files.join(MANIFEST_SEP)
    } else {
        let shown = files[..MAX_MANIFEST_FILES].join(MANIFEST_SEP);
        format!("{} (+{} more)", shown, files.len() - MAX_MANIFEST_FILES)
    }
}

/// Format a compact "Files touched" manifest to append to a summary, or an
/// empty string when no files were touched. Bounded via [`MAX_MANIFEST_FILES`].
fn format_files_touched(ops: &FileOps) -> String {
    let (read_only, modified) = ops.computed_lists();
    if read_only.is_empty() && modified.is_empty() {
        return String::new();
    }
    let mut out = format!("\n\n{}\n", FILES_TOUCHED_HEADER);
    if !modified.is_empty() {
        out.push_str(&format!("Modified: {}\n", format_manifest_line(&modified)));
    }
    if !read_only.is_empty() {
        out.push_str(&format!("Read: {}\n", format_manifest_line(&read_only)));
    }
    out.trim_end().to_string()
}

/// Split a manifest line's value back into paths, dropping any `(+N more)` tail.
fn split_manifest_line(rest: &str) -> impl Iterator<Item = String> + '_ {
    let core = match rest.rfind("(+") {
        Some(idx) => rest[..idx].trim_end(),
        None => rest.trim(),
    };
    core.split(MANIFEST_SEP)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse a previously-emitted "Files touched" manifest out of a summary so it
/// can be carried forward and unioned with the current batch. Only the capped
/// (visible) entries survive — that is what keeps the manifest bounded.
fn parse_files_touched(summary: &str) -> FileOps {
    let mut ops = FileOps::default();
    let mut in_section = false;
    for line in summary.lines() {
        let trimmed = line.trim();
        if trimmed == FILES_TOUCHED_HEADER {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Modified:") {
            ops.edited.extend(split_manifest_line(rest));
        } else if let Some(rest) = trimmed.strip_prefix("Read:") {
            ops.read.extend(split_manifest_line(rest));
        } else {
            // Any other line (blank or the start of a new section) ends it.
            in_section = false;
        }
    }
    ops
}

/// Drop a trailing "Files touched" manifest from a summary. Used to keep the
/// prior manifest out of the UPDATE prompt (it is re-appended deterministically
/// from the parsed + unioned `FileOps`, so echoing it would only risk drift).
fn strip_files_touched_section(summary: &str) -> String {
    match summary.find(FILES_TOUCHED_HEADER) {
        Some(idx) => summary[..idx].trim_end().to_string(),
        None => summary.to_string(),
    }
}

/// Format the raw compact summary by stripping `<analysis>` and cleaning up
/// `<summary>` XML tags.  Mirrors `formatCompactSummary` from TypeScript
/// prompt.ts.
pub fn format_compact_summary(raw: &str) -> String {
    // Strip <analysis>…</analysis> block (scratchpad, not useful in context)
    let without_analysis = {
        if let (Some(start), Some(end)) = (raw.find("<analysis>"), raw.find("</analysis>")) {
            let before = &raw[..start];
            let after = &raw[end + "</analysis>".len()..];
            format!("{}{}", before, after)
        } else {
            raw.to_string()
        }
    };

    // Extract and reformat <summary>…</summary>
    let formatted = if let (Some(start), Some(end)) = (
        without_analysis.find("<summary>"),
        without_analysis.find("</summary>"),
    ) {
        let before = &without_analysis[..start];
        let content = without_analysis[start + "<summary>".len()..end].trim();
        let after = &without_analysis[end + "</summary>".len()..];
        format!("{}Summary:\n{}{}", before, content, after)
    } else {
        without_analysis
    };

    // Collapse multiple blank lines
    let mut result = String::new();
    let mut blank_count = 0usize;
    for line in formatted.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 1 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(line);
            result.push('\n');
        }
    }

    result.trim().to_string()
}

// ---------------------------------------------------------------------------
// Threshold helpers
// ---------------------------------------------------------------------------

/// Return the effective context-window size in tokens for the given model.
/// These are approximate; the API enforces the real limits server-side.
///
/// This is a Claude-centric heuristic and only recognises Anthropic models —
/// every other provider collapses to the ~100k default. Prefer
/// [`resolve_context_window`], which consults the models.dev-backed registry
/// first and only falls back to this heuristic.
pub fn context_window_for_model(model: &str) -> u64 {
    if model.contains("opus-4")
        || model.contains("sonnet-4")
        || model.contains("haiku-4")
        || model.contains("claude-3-5")
        || model.contains("claude-3.5")
    {
        200_000
    } else {
        100_000
    }
}

/// Resolve the effective context window for the active provider + model.
///
/// The models.dev-backed [`claurst_api::ModelRegistry`] is the source of truth:
/// it carries real per-model context windows for *every* provider (Gemini/GPT
/// 1M windows, 32k local models, …), so we prefer it. We fall back to the
/// Claude-only [`context_window_for_model`] heuristic only when the registry is
/// absent, has no matching entry, or only holds a placeholder value.
///
/// Key derivation (bare id, `"provider/model"`, Free-mode `"upstream/model"`
/// pins, gateway aliases) is delegated to
/// [`claurst_api::ModelRegistry::resolve_entry`], the same path the TUI footer
/// and the Free provider catalog use.
pub fn resolve_context_window(
    registry: Option<&claurst_api::ModelRegistry>,
    provider: &str,
    model: &str,
) -> u64 {
    registry
        .and_then(|registry| registry.resolve_entry(provider, model))
        .map_or_else(
            || context_window_for_model(model),
            |entry| entry.info.context_window as u64,
        )
}

/// Prompt-token count a provider reported for one request, plus how many
/// messages that request carried. Anchors [`estimate_context_tokens`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageMark {
    /// `UsageInfo::total_input()` of the request (input + cache-read +
    /// cache-creation): what the model actually saw.
    pub prompt_tokens: u64,
    /// `messages.len()` when the request was sent.
    pub message_count: usize,
}

/// Best-effort projection of the context size the NEXT request will carry.
///
/// Anchors on the REAL prompt-token count the provider reported for the last
/// request (`last_usage`), because that is what the model actually saw: the
/// chars/4 heuristic can be off by a wide margin, and with prompt caching the
/// bare `input_tokens` field massively *undercounts*. Everything appended since
/// that request (the assistant reply, tool results, injected user turns) is
/// added with the chars/4 estimate ([`estimate_tokens_for_messages`]). Without
/// a usable anchor (before the first response, after a compaction, or when the
/// provider reported no usage) the whole history is estimated.
///
/// Mirrors pi's `estimateContextTokens` and codex's pre-request estimate.
pub fn estimate_context_tokens(messages: &[Message], last_usage: Option<UsageMark>) -> u64 {
    match last_usage {
        Some(mark) if mark.prompt_tokens > 0 && mark.message_count <= messages.len() => {
            mark.prompt_tokens
                + estimate_tokens_for_messages(&messages[mark.message_count..]) as u64
        }
        _ => estimate_tokens_for_messages(messages) as u64,
    }
}

/// Determine token-warning state given current input token count and model.
///
/// Convenience wrapper that derives the window from the model-name heuristic.
/// Prefer [`calculate_token_warning_state_for_window`] with a window resolved
/// via [`resolve_context_window`] so non-Claude providers size correctly.
pub fn calculate_token_warning_state(input_tokens: u64, model: &str) -> TokenWarningState {
    calculate_token_warning_state_for_window(input_tokens, context_window_for_model(model))
}

/// Determine token-warning state against an explicit context window.
///
/// Thresholds (mirrors TypeScript autoCompact.ts):
///   ≥ 95 % → Critical (red warning)
///   ≥ 80 % → Warning  (yellow warning)
///   <  80 % → Ok
pub fn calculate_token_warning_state_for_window(
    input_tokens: u64,
    window: u64,
) -> TokenWarningState {
    let pct = input_tokens as f64 / window as f64;

    if pct >= CRITICAL_PCT {
        TokenWarningState::Critical
    } else if pct >= WARNING_PCT || window.saturating_sub(input_tokens) <= WARNING_THRESHOLD_BUFFER_TOKENS {
        TokenWarningState::Warning
    } else {
        TokenWarningState::Ok
    }
}

/// Return `true` when auto-compaction should fire.
///
/// Convenience wrapper that derives the window from the model-name heuristic.
/// Prefer [`should_auto_compact_for_window`] with a resolved window.
pub fn should_auto_compact(input_tokens: u64, model: &str, state: &AutoCompactState) -> bool {
    should_auto_compact_for_window(input_tokens, context_window_for_model(model), 0, state)
}

/// Token count at which auto-compaction fires for `window`, leaving room for
/// `output_reserve` tokens of model output.
///
/// The threshold is the lower of [`AUTOCOMPACT_TRIGGER_FRACTION`] of the window
/// and the window minus the output reserve. The reserve is capped at a quarter
/// of the window so a large `max_tokens` on a small local model can't push the
/// threshold to zero.
pub fn auto_compact_threshold(window: u64, output_reserve: u64) -> u64 {
    let by_fraction = (window as f64 * AUTOCOMPACT_TRIGGER_FRACTION) as u64;
    let by_reserve = window.saturating_sub(output_reserve.min(window / 4));
    by_fraction.min(by_reserve)
}

/// Return `true` when auto-compaction should fire, against an explicit window
/// and output reserve (see [`auto_compact_threshold`]).
pub fn should_auto_compact_for_window(
    input_tokens: u64,
    window: u64,
    output_reserve: u64,
    state: &AutoCompactState,
) -> bool {
    if state.disabled {
        return false;
    }
    input_tokens >= auto_compact_threshold(window, output_reserve)
}

// ---------------------------------------------------------------------------
// Core compaction logic
// ---------------------------------------------------------------------------

/// System prompt for every summarisation request.
const SUMMARISER_SYSTEM_PROMPT: &str =
    "You are a helpful assistant that creates concise yet thorough conversation summaries. \
     Preserve all technical details, file names, code snippets, and decisions that would \
     be important for continuing the work. Follow the structured format exactly.";

/// Send one tool-less, non-agentic summarisation request through `provider`
/// and return the generated text plus the reported stop reason.
///
/// The provider stream is drained into a string rather than forwarded to the
/// UI. Streaming is the one request path every adapter implements (the Codex
/// backend only accepts streamed requests, and some non-streaming paths are
/// stubs), so this works wherever the main loop works.
async fn request_summary(
    provider: &dyn LlmProvider,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<(String, Option<StopReason>), ClaudeError> {
    let request = ProviderRequest {
        model: model.to_string(),
        messages: vec![Message::user(prompt)],
        system_prompt: Some(SystemPrompt::Text(system.to_string())),
        tools: Vec::new(),
        max_tokens,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: Vec::new(),
        thinking: None,
        provider_options: Value::Object(Default::default()),
    };

    let mut stream = provider.create_message_stream(request).await?;
    let mut text = String::new();
    let mut stop_reason = None;
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::TextDelta { text: delta, .. } => text.push_str(&delta),
            StreamEvent::MessageDelta { stop_reason: Some(reason), .. } => {
                stop_reason = Some(reason);
            }
            StreamEvent::MessageStop => break,
            _ => {}
        }
    }
    Ok((text, stop_reason))
}

/// Upper bound on summary output tokens (mirrors TypeScript
/// MAX_OUTPUT_TOKENS_FOR_SUMMARY).
pub const MAX_SUMMARY_TOKENS: u32 = 20_000;

/// Summarisation attempts before a compaction fails (one retry).
const SUMMARY_ATTEMPTS: u32 = 2;

/// Tool results are cut to this many bytes (head and tail kept) in the
/// summariser transcript, so the summarisation request itself stays well
/// inside the window even when compacting after an overflow (pi: 2,000).
const TRANSCRIPT_TOOL_RESULT_CHARS: usize = 2_000;

/// How many of the most recent user turns in the summarised head are carried
/// verbatim into the summary message.
const RECENT_USER_TURNS: usize = 3;

/// Byte budget for those verbatim user turns; a turn that would exceed it is
/// left to the summary.
const RECENT_USER_TURNS_MAX_CHARS: usize = 16_000;

/// Summary output budget for a model whose output cap is `max_output_tokens`:
/// the cap itself, bounded by [`MAX_SUMMARY_TOKENS`], so small local models
/// are never asked for more than they can emit.
pub fn summary_token_budget(max_output_tokens: u32) -> u32 {
    max_output_tokens.clamp(1, MAX_SUMMARY_TOKENS)
}

/// A compaction applied to a conversation: the new history plus the entry
/// recorded in the session.
#[derive(Debug, Clone)]
pub struct Compaction {
    pub messages: Vec<Message>,
    pub record: CompactionRecord,
}

/// Render the summarisation prompt for `head`.
fn summary_prompt(
    head: &[Message],
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) -> String {
    let mut transcript = String::new();
    let original_count = head.len();
    let original_token_estimate = estimate_tokens_for_messages(head);

    for msg in head {
        let role_label = match msg.role {
            Role::User => "Human",
            Role::Assistant => "Assistant",
        };
        let text = msg.get_all_text();
        // Skip the prior compact summary itself — it is fed separately in a
        // <previous-summary> block, so rendering it here would duplicate it.
        if !text.is_empty() && !text.contains("<compact-summary>") {
            transcript.push_str(&format!("{}: {}\n\n", role_label, text));
        }
        // Also render tool use/result blocks
        if let MessageContent::Blocks(blocks) = &msg.content {
            for block in blocks {
                match block {
                    ContentBlock::ToolUse { name, input, id, .. } => {
                        transcript.push_str(&format!(
                            "[Tool Call: {} (id={})]\nInput: {}\n\n",
                            name, id, input
                        ));
                    }
                    ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                        let result_text = match content {
                            claurst_core::types::ToolResultContent::Text(t) => {
                                claurst_core::truncate::truncate_tool_output(
                                    t,
                                    TRANSCRIPT_TOOL_RESULT_CHARS,
                                )
                                .into_owned()
                            }
                            claurst_core::types::ToolResultContent::Blocks(_) => "[complex content]".to_string(),
                        };
                        let error_flag = if is_error.unwrap_or(false) { " [ERROR]" } else { "" };
                        transcript.push_str(&format!(
                            "[Tool Result (id={}){}]\n{}\n\n",
                            tool_use_id, error_flag, result_text
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    // Feed the prior summary WITHOUT its files-touched manifest: the manifest is
    // re-appended deterministically (parsed + unioned), so echoing it in the
    // prompt would only risk the model drifting the file list.
    let previous_summary_for_prompt = previous_summary.map(strip_files_touched_section);

    // Select the UPDATE prompt variant when a prior summary is present.
    let compact_prompt =
        get_compact_prompt(custom_instructions, previous_summary_for_prompt.as_deref());

    if let Some(prev) = previous_summary_for_prompt.as_deref() {
        format!(
            "{}\n\n<previous-summary>\n{}\n</previous-summary>\n\n<conversation_to_summarize original_messages=\"{}\" estimated_tokens=\"{}\">\n{}\n</conversation_to_summarize>",
            compact_prompt,
            prev,
            original_count,
            original_token_estimate,
            transcript
        )
    } else {
        format!(
            "{}\n\n<conversation_to_summarize original_messages=\"{}\" estimated_tokens=\"{}\">\n{}\n</conversation_to_summarize>",
            compact_prompt,
            original_count,
            original_token_estimate,
            transcript
        )
    }
}

/// Format a raw summariser reply (dropping the discarded `<analysis>` block)
/// and reject it when it is empty, was cut off by the output limit, or is not
/// shorter than the messages it replaces.
fn validate_summary(
    raw: &str,
    stop_reason: Option<StopReason>,
    head_tokens: usize,
) -> Result<String, ClaudeError> {
    if matches!(stop_reason, Some(StopReason::MaxTokens)) {
        return Err(ClaudeError::Other(
            "Compact summary was cut off at the output limit".to_string(),
        ));
    }
    let summary = format_compact_summary(raw);
    if summary.trim().is_empty() {
        return Err(ClaudeError::Other("Compact summary was empty".to_string()));
    }
    if (summary.len() / 4) * 4 / 3 >= head_tokens {
        return Err(ClaudeError::Other(
            "Compact summary is not shorter than the messages it replaces".to_string(),
        ));
    }
    Ok(summary)
}

/// Summarise `head` through `provider`, retrying once when the call fails or
/// the response is rejected by [`validate_summary`]. Returns the formatted
/// summary with its files-touched manifest.
async fn generate_summary(
    provider: &dyn LlmProvider,
    model: &str,
    head: &[Message],
    max_summary_tokens: u32,
    custom_instructions: Option<&str>,
) -> Result<String, ClaudeError> {
    // Iterative UPDATE mode: if a prior <compact-summary> already lives in the
    // head, fold it forward instead of re-summarising from scratch.
    let previous_summary = extract_previous_summary(head);
    let prompt = summary_prompt(head, previous_summary.as_deref(), custom_instructions);
    let head_tokens = estimate_tokens_for_messages(head);

    let mut attempt = 1;
    let formatted_summary = loop {
        let outcome = request_summary(
            provider,
            model,
            SUMMARISER_SYSTEM_PROMPT,
            &prompt,
            max_summary_tokens,
        )
        .await
        .and_then(|(raw, stop_reason)| validate_summary(&raw, stop_reason, head_tokens));
        match outcome {
            Ok(summary) => break summary,
            Err(e) if attempt < SUMMARY_ATTEMPTS => {
                warn!(attempt, error = %e, "Compaction summary failed; retrying");
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    };

    // Files-touched manifest: files this batch read/wrote/edited, unioned with
    // any manifest carried in the prior summary so the agent doesn't forget what
    // it worked on across successive compactions. Appended deterministically
    // (bounded via MAX_MANIFEST_FILES) rather than trusting the model.
    let mut file_ops = extract_file_operations(head);
    if let Some(prev) = &previous_summary {
        file_ops.union(&parse_files_touched(prev));
    }
    Ok(if file_ops.is_empty() {
        formatted_summary
    } else {
        format!("{}{}", formatted_summary, format_files_touched(&file_ops))
    })
}

/// The most recent user-authored turns in `head` (oldest first), at most
/// [`RECENT_USER_TURNS`] within [`RECENT_USER_TURNS_MAX_CHARS`]. Tool results
/// and earlier compaction summaries are skipped.
fn recent_user_turns(head: &[Message]) -> Vec<String> {
    let mut turns = Vec::new();
    let mut used = 0;
    for msg in head.iter().rev() {
        if turns.len() == RECENT_USER_TURNS {
            break;
        }
        if msg.role != Role::User || message_has_tool_result(msg) {
            continue;
        }
        let text = msg.get_all_text();
        if text.trim().is_empty()
            || text.contains("<compact-summary>")
            || text.contains("<recent-user-messages>")
        {
            continue;
        }
        if used + text.len() > RECENT_USER_TURNS_MAX_CHARS {
            break;
        }
        used += text.len();
        turns.push(text);
    }
    turns.reverse();
    turns
}

/// Build the synthetic user message that replaces `head`: `body` (the summary
/// notice) followed by the most recent user turns verbatim. It gets a fresh
/// uuid so the session's [`CompactionRecord`] can point at it.
///
/// The summary is wrapped in <compact-summary> tags by the callers so the NEXT
/// compaction can detect it (via extract_previous_summary) and fold it forward
/// in UPDATE mode.
fn summary_message(mut body: String, head: &[Message]) -> Message {
    let recent = recent_user_turns(head);
    if !recent.is_empty() {
        body.push_str(
            "\n\n<recent-user-messages>\nThe user's most recent messages from that portion, verbatim:",
        );
        for turn in recent {
            body.push_str("\n\n---\n");
            body.push_str(&turn);
        }
        body.push_str("\n</recent-user-messages>");
    }
    let mut message = Message::user(body);
    message.uuid = Some(uuid::Uuid::new_v4().to_string());
    message
}

/// Notice for a head that was summarised into `summary`.
fn summarised_notice(summary: &str, head: &[Message]) -> String {
    format!(
        "This session is being continued from a previous conversation that ran out of context. \
         The summary below covers the earlier portion of the conversation (originally {} messages, \
         ~{} tokens).\n\n<compact-summary>\n{}\n</compact-summary>",
        head.len(),
        estimate_tokens_for_messages(head),
        summary
    )
}

/// Summarise `messages[..split_at]` through the active provider.
/// Returns a new conversation: [summary user msg] + messages[split_at..].
async fn summarise_head(
    provider: &dyn LlmProvider,
    model: &str,
    messages: &[Message],
    split_at: usize,
    max_summary_tokens: u32,
) -> Result<Vec<Message>, ClaudeError> {
    if split_at == 0 {
        return Ok(messages.to_vec());
    }
    let head = &messages[..split_at];
    let summary = generate_summary(provider, model, head, max_summary_tokens, None).await?;
    let mut new_messages = vec![summary_message(summarised_notice(&summary, head), head)];
    new_messages.extend_from_slice(&messages[split_at..]);
    Ok(new_messages)
}

/// Does this message carry any `tool_result` blocks?
///
/// A `tool_result` always answers the `tool_use` in the message *immediately
/// before* it, so a compaction cut must never land on such a message: doing so
/// would orphan the result from its call in the kept tail (and, symmetrically,
/// leave a dangling `tool_use` at the end of the summarised head).
fn message_has_tool_result(msg: &Message) -> bool {
    match &msg.content {
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. })),
        _ => false,
    }
}

/// Snap a raw keep-index back to a pairing-safe round boundary.
///
/// A cut at index `k` keeps `messages[k..]` verbatim. It is pairing-safe iff
/// `messages[k]` carries no `tool_result` blocks (see [`message_has_tool_result`]).
/// We walk *backwards* (keeping MORE — never less — than the raw budget asked
/// for) until we land on a safe boundary. This preserves the round-aligned,
/// tool_use↔tool_result-paired history compaction must emit, independent of the
/// separate `sanitize_history` repair pass.
fn snap_to_pairing_boundary(messages: &[Message], idx: usize) -> usize {
    let len = messages.len();
    // Keep-nothing (idx == len): the tail is empty, so there is no boundary
    // message that could be orphaned — leave it as-is.
    let mut idx = idx.min(len);
    while idx > 0 && idx < len && message_has_tool_result(&messages[idx]) {
        idx -= 1;
    }
    idx
}

/// Decide how much of the recent tail to preserve verbatim, driven by a TOKEN
/// budget rather than a fixed message count.
///
/// Returns the split index: everything before it is summarised, everything at or
/// after it is kept verbatim. Larger `keep_recent_tokens` keeps more messages;
/// smaller keeps fewer. The index is snapped to a tool_use↔tool_result-safe
/// boundary so pairing is never broken.
///
/// When even the newest message alone exceeds the budget (typically the user
/// message carrying a large tool batch, at the pre-request check), the whole
/// last round — from the last assistant message on — is kept instead of
/// summarising everything, so the results the model just asked for survive.
fn compute_keep_split_index(messages: &[Message], keep_recent_tokens: u64) -> usize {
    if messages.is_empty() {
        return 0;
    }
    let mut raw = calculate_messages_to_keep_index(messages, keep_recent_tokens);
    if raw == messages.len() {
        raw = messages
            .iter()
            .rposition(|m| m.role == Role::Assistant)
            .unwrap_or(0);
    }
    snap_to_pairing_boundary(messages, raw)
}

/// Compact `messages`: summarise everything older than the most recent
/// ~[`KEEP_RECENT_TOKENS`] (cut on a pairing-safe boundary) into one summary
/// message that also carries the last few user turns verbatim, and return the
/// new history with the compaction entry for the session.
///
/// Returns `Ok(None)` when the whole conversation already fits the keep-recent
/// budget (nothing to summarise). When the summariser fails twice the error is
/// returned and the caller keeps its history, except for
/// [`CompactionTrigger::Overflow`]: the request cannot be sent as is, so the
/// head is dropped and only the untouched tail is kept (with any previous
/// summary carried over).
pub async fn compact_conversation(
    provider: &dyn LlmProvider,
    model: &str,
    messages: &[Message],
    trigger: CompactionTrigger,
    max_summary_tokens: u32,
    custom_instructions: Option<&str>,
) -> Result<Option<Compaction>, ClaudeError> {
    let total = messages.len();
    let split_at = compute_keep_split_index(messages, KEEP_RECENT_TOKENS);

    if split_at == 0 {
        debug!(
            total,
            keep_recent_tokens = KEEP_RECENT_TOKENS,
            "Whole conversation fits the keep-recent budget – keeping everything"
        );
        return Ok(None);
    }

    info!(
        total,
        split_at,
        keep_recent_tokens = KEEP_RECENT_TOKENS,
        ?trigger,
        "Compacting conversation (token-budget keep)"
    );

    let head = &messages[..split_at];
    let (notice, summarised) =
        match generate_summary(provider, model, head, max_summary_tokens, custom_instructions)
            .await
        {
            Ok(summary) => (summarised_notice(&summary, head), true),
            Err(e) if trigger == CompactionTrigger::Overflow => {
                warn!(error = %e, "Summariser failed after a context-window error; keeping only the recent tail");
                let mut notice = format!(
                    "This session is being continued from a previous conversation that ran out of context. \
                     The earlier portion of the conversation (originally {} messages, ~{} tokens) could not \
                     be summarised and was dropped.",
                    head.len(),
                    estimate_tokens_for_messages(head)
                );
                if let Some(previous) = extract_previous_summary(head) {
                    notice.push_str(&format!(
                        "\n\nSummary carried over from an earlier compaction:\n\n<compact-summary>\n{}\n</compact-summary>",
                        previous
                    ));
                }
                (notice, false)
            }
            Err(e) => return Err(e),
        };

    let summary = summary_message(notice, head);
    let summary_uuid = summary.uuid.clone().unwrap_or_default();
    let mut new_messages = Vec::with_capacity(1 + total - split_at);
    new_messages.push(summary);
    new_messages.extend_from_slice(&messages[split_at..]);

    let record = CompactionRecord {
        at: chrono::Utc::now(),
        trigger,
        summary_uuid,
        messages_before: total,
        messages_after: new_messages.len(),
        tokens_before: estimate_tokens_for_messages(messages) as u64,
        tokens_after: estimate_tokens_for_messages(&new_messages) as u64,
        summarised,
        // Filled by `history::apply_compaction` when the result is applied,
        // which owns the old history and can move the head instead of
        // copying it.
        replaced: Vec::new(),
    };
    Ok(Some(Compaction { messages: new_messages, record }))
}

/// Auto-compact `messages` if needed.  Updates `state` in place.
/// Returns the compaction if one ran, `None` otherwise.
///
/// `context_window` is the effective window for the active provider+model
/// (resolve it via [`resolve_context_window`]); `provider` and `model` serve
/// the summarisation call.
pub async fn auto_compact_if_needed(
    provider: &dyn LlmProvider,
    model: &str,
    messages: &[Message],
    input_tokens: u64,
    context_window: u64,
    max_output_tokens: u32,
    state: &mut AutoCompactState,
) -> Option<Compaction> {
    if !should_auto_compact_for_window(
        input_tokens,
        context_window,
        u64::from(max_output_tokens),
        state,
    ) {
        return None;
    }

    info!(
        input_tokens,
        model,
        compaction_count = state.compaction_count,
        "Auto-compact triggered"
    );

    match compact_conversation(
        provider,
        model,
        messages,
        CompactionTrigger::Threshold,
        summary_token_budget(max_output_tokens),
        None,
    )
    .await
    {
        Ok(None) => None,
        Ok(Some(compaction)) => {
            state.on_success();
            info!(
                original_count = messages.len(),
                new_count = compaction.messages.len(),
                "Auto-compact complete"
            );
            Some(compaction)
        }
        Err(e) => {
            warn!(error = %e, "Auto-compact failed");
            state.on_failure();
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Reactive Compact (T1-1) — fires on usage data, not after turn end
// ---------------------------------------------------------------------------
//
// The TypeScript source uses a `ReactiveCompact` class with GrowthBook
// feature flags and a subscription to the streaming API's token-usage
// events.  In the Rust port we model the same behaviour with plain async
// functions and an env-var feature gate (`CLAUDE_REACTIVE_COMPACT=1`).
//
// Phase overview (mirrors reactiveCompact.ts):
//   1. Check usage with `should_compact` / `should_context_collapse`.
//   2. Strip image blocks from the conversation before compacting
//      (reduces the size of the prompt sent to the summariser).
//   3. Call `summarise_head` to generate a compact summary.
//   4. Re-inject recently-modified files (up to 5) as context.
//      (In the Rust port this phase is a no-op stub — the TUI layer owns
//      file-tracking; this file intentionally avoids the filesystem.)

/// Trigger classification for reactive compact.
#[derive(Debug, Clone)]
pub enum CompactTrigger {
    /// Normal 90 %-threshold compact.
    TokenThreshold { tokens_used: u64, context_limit: u64 },
    /// Caller requested an unconditional compact.
    Forced,
}

/// Result returned by `reactive_compact` and `context_collapse`.
#[derive(Debug, Clone)]
pub struct CompactResult {
    /// The new (reduced) message list.
    pub messages: Vec<claurst_core::types::Message>,
    /// Formatted summary text injected at the head of `messages`.
    pub summary: String,
    /// Rough estimate of how many tokens were freed.
    pub tokens_freed: u64,
}

/// Return `true` when reactive compact should fire (≥ 90 % of context window).
///
/// Threshold is intentionally identical to `AUTOCOMPACT_TRIGGER_FRACTION` so
/// that exactly one of the two paths (proactive auto-compact vs reactive
/// compact) fires, chosen by the `CLAUDE_REACTIVE_COMPACT` gate.
pub fn should_compact(tokens_used: u64, context_limit: u64) -> bool {
    if context_limit == 0 {
        return false;
    }
    let threshold = (context_limit as f64 * REACTIVE_COMPACT_THRESHOLD) as u64;
    tokens_used >= threshold
}

/// Return `true` when the emergency context-collapse should fire (≥ 97 %).
///
/// Context-collapse is a last-resort measure: it produces an ultra-short
/// summary and keeps only the most recent user turn so that the next API call
/// can succeed even when the conversation is severely over-limit.
pub fn should_context_collapse(tokens_used: u64, context_limit: u64) -> bool {
    if context_limit == 0 {
        return false;
    }
    let threshold = (context_limit as f64 * CONTEXT_COLLAPSE_THRESHOLD) as u64;
    tokens_used >= threshold
}

/// Snip the middle of the conversation, keeping:
///   - the first message (usually the system/context bootstrap), and
///   - the `keep_n_newest` most-recent messages.
///
/// Returns `(new_messages, rough_tokens_freed)`.
///
/// Mirrors `snipCompact` from TypeScript (no API call required — purely local).
pub fn snip_compact(messages: Vec<claurst_core::types::Message>, keep_n_newest: usize) -> (Vec<claurst_core::types::Message>, u64) {
    let total = messages.len();
    if total <= keep_n_newest + 1 {
        // Nothing to snip.
        return (messages, 0);
    }

    // Keep: messages[0] (first/system message) + messages[total-keep_n_newest..]
    let snip_start = 1usize;
    let snip_end = total.saturating_sub(keep_n_newest);

    if snip_start >= snip_end {
        return (messages, 0);
    }

    // Estimate how many tokens the snipped range held.
    let snipped_tokens =
        estimate_tokens_for_messages(&messages[snip_start..snip_end]) as u64;

    let mut result = Vec::with_capacity(1 + keep_n_newest);
    result.push(messages[0].clone());
    result.extend_from_slice(&messages[snip_end..]);

    (result, snipped_tokens)
}

/// Compute the index into `messages` such that the tail starting at that
/// index fits within `token_budget` tokens.
///
/// Returns the cut index (0 = keep everything, messages.len() = keep nothing).
/// Iterates from the newest message backwards, accumulating token estimates
/// until the budget is exhausted.
pub fn calculate_messages_to_keep_index(messages: &[claurst_core::types::Message], token_budget: u64) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let mut accumulated: u64 = 0;
    let mut keep_from = messages.len(); // default: keep nothing (index past end)

    for (i, msg) in messages.iter().enumerate().rev() {
        let est = estimate_tokens_for_messages(std::slice::from_ref(msg)) as u64;
        if accumulated + est > token_budget {
            // This message would push us over budget — stop here.
            keep_from = i + 1;
            break;
        }
        accumulated += est;
        keep_from = i;
    }

    keep_from
}

/// Remove image blocks from a message list before compacting.
///
/// Image tokens are expensive and carry no information that a text summary
/// needs.  Mirrors the TypeScript `stripImages` helper used inside
/// `reactiveCompact.ts`.
fn strip_images(messages: Vec<claurst_core::types::Message>) -> Vec<claurst_core::types::Message> {
    use claurst_core::types::{ContentBlock, MessageContent};

    messages
        .into_iter()
        .map(|mut msg| {
            if let MessageContent::Blocks(ref mut blocks) = msg.content {
                blocks.retain(|b| !matches!(b, ContentBlock::Image { .. }));
                // If stripping left only an empty block list, collapse to a
                // placeholder text so the conversation remains parseable.
                if blocks.is_empty() {
                    msg.content = MessageContent::Text("[image removed for compaction]".to_string());
                }
            }
            msg
        })
        .collect()
}

/// Run reactive compact: summarise the oldest messages and return a trimmed
/// conversation.
///
/// Feature gate: only call this when
/// `claurst_core::feature_gates::is_feature_enabled("reactive_compact")` is true.
///
/// The `cancel` token is checked before the API call so the user can abort
/// a long-running compact.
pub async fn reactive_compact(
    messages: Vec<claurst_core::types::Message>,
    provider: &dyn LlmProvider,
    model: &str,
    cancel: tokio_util::sync::CancellationToken,
    recently_modified: &[std::path::PathBuf],
) -> Result<CompactResult, claurst_core::error::ClaudeError> {
    if cancel.is_cancelled() {
        return Err(claurst_core::error::ClaudeError::Cancelled);
    }

    let total = messages.len();
    if total == 0 {
        return Ok(CompactResult {
            messages: vec![],
            summary: String::new(),
            tokens_freed: 0,
        });
    }

    // Phase 2: strip images before the compact API call.
    let stripped = strip_images(messages.clone());

    // Phase 1 + 3: summarise the head (everything older than the ~KEEP_RECENT_TOKENS
    // recent tail, cut on a pairing-safe boundary), then replace the old head with
    // the summary message.
    let split_at = compute_keep_split_index(&stripped, KEEP_RECENT_TOKENS);
    if split_at == 0 {
        // Too few messages; nothing to summarise.
        return Ok(CompactResult {
            messages,
            summary: String::new(),
            tokens_freed: 0,
        });
    }

    let original_token_estimate =
        estimate_tokens_for_messages(&stripped[..split_at]) as u64;

    let mut new_messages =
        summarise_head(provider, model, &stripped, split_at, MAX_SUMMARY_TOKENS).await?;

    // The summary lives as the first message in new_messages.
    let summary_text = new_messages
        .first()
        .map(|m| m.get_all_text())
        .unwrap_or_default();

    // Phase 4: re-inject recently modified file context (up to 5 files, skip >50KB).
    const MAX_FILES: usize = 5;
    const MAX_FILE_BYTES: u64 = 50 * 1024;
    let mut injected = 0;
    for path in recently_modified.iter().take(MAX_FILES * 3) {
        if injected >= MAX_FILES {
            break;
        }
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let file_name = path.display().to_string();
        let text = format!("<file path=\"{}\">\n{}\n</file>", file_name, content);
        new_messages.push(claurst_core::types::Message::user(text));
        injected += 1;
    }

    let tokens_after = estimate_tokens_for_messages(&new_messages) as u64;
    let tokens_freed = original_token_estimate.saturating_sub(tokens_after);

    Ok(CompactResult {
        messages: new_messages,
        summary: summary_text,
        tokens_freed,
    })
}

/// Emergency context collapse: produce an ultra-short summary that distils
/// the entire conversation into the minimum needed to continue, then keep
/// only the most recent user turn.
///
/// Use only when `should_context_collapse()` returns `true` — i.e. the
/// context is at ≥ 97 % capacity and a regular reactive compact is unlikely
/// to free enough space.
pub async fn context_collapse(
    messages: Vec<claurst_core::types::Message>,
    provider: &dyn LlmProvider,
    model: &str,
) -> Result<CompactResult, claurst_core::error::ClaudeError> {
    let total = messages.len();
    if total == 0 {
        return Ok(CompactResult {
            messages: vec![],
            summary: String::new(),
            tokens_freed: 0,
        });
    }

    let original_tokens = estimate_tokens_for_messages(&messages) as u64;

    // Build a concise transcript for the collapse prompt.
    let mut transcript = String::new();
    for msg in &messages {
        let role = match msg.role {
            claurst_core::types::Role::User => "Human",
            claurst_core::types::Role::Assistant => "Assistant",
        };
        let text = msg.get_all_text();
        if !text.is_empty() {
            transcript.push_str(&format!("{}: {}\n\n", role, text));
        }
    }

    let collapse_prompt = format!(
        "EMERGENCY CONTEXT COLLAPSE — the conversation is at critical capacity.\n\
         Produce an ULTRA-SHORT (max 500 words) emergency summary that captures:\n\
         1. The user's most recent explicit request.\n\
         2. The single most important decision made so far.\n\
         3. Any file names or code snippets that are ESSENTIAL to continue.\n\
         4. What was being worked on immediately before this collapse.\n\
         Respond with plain text only — no XML tags, no tool calls.\n\n\
         <conversation>\n{}\n</conversation>",
        transcript
    );

    let (summary_text, _stop) = request_summary(
        provider,
        model,
        "You are a conversation summariser. Produce an emergency ultra-short \
         summary as instructed. Plain text only.",
        &collapse_prompt,
        1_000,
    )
    .await?;

    if summary_text.is_empty() {
        return Err(claurst_core::error::ClaudeError::Other(
            "Context-collapse summary was empty".to_string(),
        ));
    }

    // Keep only: the synthetic summary + the most recent user turn.
    let collapse_notice = claurst_core::types::Message::user(format!(
        "[EMERGENCY CONTEXT COLLAPSE — conversation condensed to stay within limits]\n\n{}",
        summary_text
    ));

    // Find the last user message in the original list.
    let last_user = messages
        .iter()
        .rev()
        .find(|m| m.role == claurst_core::types::Role::User)
        .cloned();

    let mut new_messages = vec![collapse_notice];
    if let Some(last) = last_user {
        new_messages.push(last);
    }

    let tokens_after = estimate_tokens_for_messages(&new_messages) as u64;
    let tokens_freed = original_tokens.saturating_sub(tokens_after);

    Ok(CompactResult {
        messages: new_messages,
        summary: summary_text,
        tokens_freed,
    })
}

// Threshold constants for reactive compact / context-collapse.
/// Reactive compact fires at 90 % of the context window.
const REACTIVE_COMPACT_THRESHOLD: f64 = 0.90;
/// Context collapse (emergency) fires at 97 % of the context window.
const CONTEXT_COLLAPSE_THRESHOLD: f64 = 0.97;

// ---------------------------------------------------------------------------
// T4-5: Collapse read/search results (mirrors src/utils/collapseReadSearch.ts)
// ---------------------------------------------------------------------------

/// Replace repeated reads of the same file with a single summary.
///
/// When the same file is read more than once in the conversation, replaces
/// all but the last read with `[Content shown N time(s); showing last occurrence only]`.
pub fn collapse_read_tool_results(messages: Vec<claurst_core::types::Message>) -> Vec<claurst_core::types::Message> {
    use claurst_core::types::{ContentBlock, MessageContent, ToolResultContent};
    use std::collections::HashMap;

    // Helper: extract a fingerprint string from ToolResultContent.
    fn fingerprint(content: &ToolResultContent) -> Option<String> {
        match content {
            ToolResultContent::Text(t) => Some(t.chars().take(120).collect()),
            ToolResultContent::Blocks(_) => None,
        }
    }

    // First pass: find all file-read tool results and count by fingerprint.
    let mut read_counts: HashMap<String, usize> = HashMap::new();
    for msg in &messages {
        if let MessageContent::Blocks(blocks) = &msg.content {
            for block in blocks {
                if let ContentBlock::ToolResult { content, .. } = block {
                    if let Some(key) = fingerprint(content) {
                        *read_counts.entry(key).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    // Second pass: replace intermediate (non-last) occurrences.
    let mut seen: HashMap<String, usize> = HashMap::new();
    messages
        .into_iter()
        .map(|mut msg| {
            if let MessageContent::Blocks(ref mut blocks) = msg.content {
                for block in blocks.iter_mut() {
                    if let ContentBlock::ToolResult { content, .. } = block {
                        if let Some(key) = fingerprint(content) {
                            let count = read_counts.get(&key).copied().unwrap_or(1);
                            if count > 1 {
                                let seen_count = seen.entry(key.clone()).or_insert(0);
                                *seen_count += 1;
                                if *seen_count < count {
                                    // Replace intermediate occurrences.
                                    *content = ToolResultContent::Text(format!(
                                        "[Content shown {} time(s); showing last occurrence only]",
                                        count
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            msg
        })
        .collect()
}

/// Deduplicate grep/glob search results that appear multiple times.
///
/// If the same search was run more than once (same query), keep only the
/// most recent result; replace earlier results with a truncation notice.
pub fn collapse_search_results(messages: Vec<claurst_core::types::Message>) -> Vec<claurst_core::types::Message> {
    use claurst_core::types::{ContentBlock, MessageContent, ToolResultContent};
    use std::collections::HashSet;

    fn fingerprint(content: &ToolResultContent) -> Option<String> {
        match content {
            ToolResultContent::Text(t) => Some(t.chars().take(200).collect()),
            ToolResultContent::Blocks(_) => None,
        }
    }

    let mut seen_results: HashSet<String> = HashSet::new();

    // Iterate in reverse to keep the latest occurrence.
    let mut result: Vec<claurst_core::types::Message> = messages
        .into_iter()
        .rev()
        .map(|mut msg| {
            if let MessageContent::Blocks(ref mut blocks) = msg.content {
                for block in blocks.iter_mut() {
                    if let ContentBlock::ToolResult { content, .. } = block {
                        if let Some(fp) = fingerprint(content) {
                            if !seen_results.insert(fp) {
                                *content = ToolResultContent::Text(
                                    "[Duplicate search result; content shown in a later turn]"
                                        .to_string(),
                                );
                            }
                        }
                    }
                }
            }
            msg
        })
        .collect();

    result.reverse();
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::types::{Message, ToolResultContent};

    fn make_user(text: &str) -> Message {
        Message::user(text)
    }

    fn make_assistant(text: &str) -> Message {
        // No UUID set — relies on the no-UUID grouping path in group_messages_for_compact.
        Message::assistant(text)
    }

    /// `n` bytes of filler text (≈ `n/4 * 4/3` tokens under the chars/4 estimate).
    fn filler(n: usize) -> String {
        "x".repeat(n)
    }

    /// An assistant message carrying a single `tool_use` block.
    fn assistant_tool_use(id: &str, name: &str, input: serde_json::Value) -> Message {
        Message::assistant_blocks(vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
            thought_signature: None,
        }])
    }

    /// A user message carrying a single `tool_result` block answering `id`.
    fn user_tool_result(id: &str, text: &str) -> Message {
        Message::user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: ToolResultContent::Text(text.to_string()),
            is_error: None,
        }])
    }

    // ---- TokenWarningState --------------------------------------------------

    #[test]
    fn test_warning_state_ok() {
        // 50 % of 200k = 100k tokens — should be Ok
        let state = calculate_token_warning_state(100_000, "claude-sonnet-4-6");
        assert_eq!(state, TokenWarningState::Ok);
    }

    #[test]
    fn test_warning_state_warning() {
        // 85 % of 200k = 170k tokens — should be Warning
        let state = calculate_token_warning_state(170_000, "claude-sonnet-4-6");
        assert_eq!(state, TokenWarningState::Warning);
    }

    #[test]
    fn test_warning_state_critical() {
        // 96 % of 200k = 192k tokens — should be Critical
        let state = calculate_token_warning_state(192_000, "claude-sonnet-4-6");
        assert_eq!(state, TokenWarningState::Critical);
    }

    #[test]
    fn test_warning_state_boundary_80pct() {
        // Exactly 80 % of 200k = 160k tokens — should be Warning (>= threshold)
        let state = calculate_token_warning_state(160_000, "claude-sonnet-4-6");
        assert_eq!(state, TokenWarningState::Warning);
    }

    #[test]
    fn test_warning_state_boundary_95pct() {
        // Exactly 95 % of 200k = 190k tokens — should be Critical
        let state = calculate_token_warning_state(190_000, "claude-sonnet-4-6");
        assert_eq!(state, TokenWarningState::Critical);
    }

    // ---- should_auto_compact ------------------------------------------------

    #[test]
    fn test_should_not_compact_when_disabled() {
        let state = AutoCompactState { disabled: true, ..Default::default() };
        assert!(!should_auto_compact(195_000, "claude-sonnet-4-6", &state));
    }

    #[test]
    fn test_should_compact_at_90pct() {
        let state = AutoCompactState::default();
        // 90 % of 200k = 180k — should trigger
        assert!(should_auto_compact(180_000, "claude-sonnet-4-6", &state));
    }

    #[test]
    fn test_should_not_compact_below_90pct() {
        let state = AutoCompactState::default();
        // 70 % of 200k = 140k — should NOT trigger
        assert!(!should_auto_compact(140_000, "claude-sonnet-4-6", &state));
    }

    // ---- Circuit breaker ----------------------------------------------------

    #[test]
    fn test_circuit_breaker_opens_after_failures() {
        let mut state = AutoCompactState::default();
        assert!(!state.disabled);
        for _ in 0..MAX_CONSECUTIVE_FAILURES {
            state.on_failure();
        }
        assert!(state.disabled);
    }

    #[test]
    fn test_circuit_breaker_resets_on_success() {
        let mut state = AutoCompactState::default();
        state.on_failure();
        state.on_failure();
        state.on_success();
        assert_eq!(state.consecutive_failures, 0);
        assert!(!state.disabled);
    }

    // ---- Message grouping ---------------------------------------------------

    #[test]
    fn test_group_messages_simple() {
        let messages = vec![
            make_user("Hello"),
            make_assistant("Hi there"),
            make_user("How are you?"),
            make_assistant("I'm fine"),
        ];

        let groups = group_messages_for_compact(&messages);
        // Should produce 2 groups: one per assistant turn boundary
        assert_eq!(groups.len(), 2);
        // First group: user + first assistant
        assert_eq!(groups[0].messages.len(), 2);
        // Second group: second user + second assistant
        assert_eq!(groups[1].messages.len(), 2);
    }

    #[test]
    fn test_group_empty() {
        let groups = group_messages_for_compact(&[]);
        assert!(groups.is_empty());
    }

    #[test]
    fn test_group_only_user_messages() {
        // No assistant messages → everything in one group
        let messages = vec![make_user("A"), make_user("B"), make_user("C")];
        let groups = group_messages_for_compact(&messages);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].messages.len(), 3);
    }

    // ---- format_compact_summary --------------------------------------------

    #[test]
    fn test_format_strips_analysis() {
        let raw = "<analysis>This is scratchpad text.</analysis>\n\
                   <summary>This is the real content.</summary>";
        let formatted = format_compact_summary(raw);
        assert!(!formatted.contains("<analysis>"));
        assert!(!formatted.contains("scratchpad text"));
        assert!(formatted.contains("real content"));
    }

    #[test]
    fn test_format_replaces_summary_tags() {
        let raw = "<summary>Content here</summary>";
        let formatted = format_compact_summary(raw);
        assert!(!formatted.contains("<summary>"));
        assert!(formatted.contains("Summary:"));
        assert!(formatted.contains("Content here"));
    }

    #[test]
    fn test_format_passthrough_when_no_tags() {
        let raw = "Plain text summary without any XML tags.";
        let formatted = format_compact_summary(raw);
        assert_eq!(formatted, raw);
    }

    // ---- get_compact_prompt ------------------------------------------------

    #[test]
    fn test_compact_prompt_contains_no_tools_preamble() {
        let prompt = get_compact_prompt(None, None);
        assert!(prompt.contains("CRITICAL: Respond with TEXT ONLY"));
        assert!(prompt.contains("Do NOT call any tools"));
    }

    #[test]
    fn test_compact_prompt_contains_sections() {
        let prompt = get_compact_prompt(None, None);
        for section in [
            "## Objective",
            "## Requirements",
            "## Decisions",
            "## Work State",
            "## Next Move",
            "## Relevant Files",
            "## Important Context",
        ] {
            assert!(prompt.contains(section), "missing {section}");
        }
        assert!(prompt.contains("Include at most 15"));
        assert!(prompt.contains("Leave out instructions and setup the assistant was given"));
        assert!(prompt.contains("<analysis>") && prompt.contains("<summary>"));
        // The long "full code snippets" template is gone.
        assert!(!prompt.contains("full code snippets"));
    }

    #[test]
    fn test_compact_prompt_with_custom_instructions() {
        let prompt = get_compact_prompt(Some("Focus on Rust type system changes."), None);
        assert!(prompt.contains("Additional Instructions:"));
        assert!(prompt.contains("Focus on Rust type system changes."));
    }

    #[test]
    fn test_compact_prompt_empty_custom_instructions_ignored() {
        let prompt_none = get_compact_prompt(None, None);
        let prompt_empty = get_compact_prompt(Some("   "), None);
        assert_eq!(prompt_none, prompt_empty);
    }

    // ---- context_window_for_model ------------------------------------------

    #[test]
    fn test_context_window_sonnet4() {
        assert_eq!(context_window_for_model("claude-sonnet-4-6"), 200_000);
    }

    #[test]
    fn test_context_window_opus4() {
        assert_eq!(context_window_for_model("claude-opus-4-0"), 200_000);
    }

    #[test]
    fn test_context_window_legacy() {
        assert_eq!(context_window_for_model("claude-2"), 100_000);
    }

    // ---- resolve_context_window (#216) -------------------------------------

    /// Build an in-memory `ModelRegistry` from a models.dev-style JSON snapshot
    /// by round-tripping it through the real `load_cache` parse path.
    fn registry_from_json(json: &str) -> claurst_api::ModelRegistry {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("models_dev.json");
        std::fs::write(&path, json).expect("write snapshot");
        let mut reg = claurst_api::ModelRegistry::new();
        reg.load_cache(&path);
        reg
    }

    // A fake provider with a genuine 1M window and a placeholder (no-limit)
    // model. Fake ids keep the fixture isolated from the bundled snapshot.
    const TEST_SNAPSHOT: &str = r#"{"testprov":{"id":"testprov","name":"Test Provider","env":[],"models":{"big-context-model":{"id":"big-context-model","name":"Big Context Model","limit":{"context":1000000,"output":65536}},"tiny-model":{"id":"tiny-model","name":"Tiny Model"}}}}"#;

    #[test]
    fn resolve_prefers_registry_for_large_context_model() {
        let reg = registry_from_json(TEST_SNAPSHOT);
        // Sanity: the registry really carries the 1M window.
        assert_eq!(
            reg.get("testprov", "big-context-model").unwrap().info.context_window,
            1_000_000
        );
        assert_eq!(
            resolve_context_window(Some(&reg), "testprov", "big-context-model"),
            1_000_000
        );
    }

    #[test]
    fn resolve_handles_canonical_provider_slash_model_string() {
        let reg = registry_from_json(TEST_SNAPSHOT);
        // Model string carries the provider prefix; still resolves to 1M.
        assert_eq!(
            resolve_context_window(Some(&reg), "testprov", "testprov/big-context-model"),
            1_000_000
        );
        // Provider arg is wrong but the "provider/model" string still resolves.
        assert_eq!(
            resolve_context_window(Some(&reg), "anthropic", "testprov/big-context-model"),
            1_000_000
        );
    }

    #[test]
    fn resolve_falls_back_to_heuristic_when_registry_none() {
        // No registry → heuristic. Claude-ish and legacy both come through.
        assert_eq!(
            resolve_context_window(None, "anthropic", "claude-opus-4-8"),
            context_window_for_model("claude-opus-4-8")
        );
        assert_eq!(resolve_context_window(None, "anthropic", "claude-opus-4-8"), 200_000);
        assert_eq!(resolve_context_window(None, "some-provider", "some-model"), 100_000);
    }

    #[test]
    fn resolve_falls_back_to_heuristic_when_no_registry_entry() {
        let reg = registry_from_json(TEST_SNAPSHOT);
        // Provider/model that isn't in the registry → heuristic default.
        assert_eq!(
            resolve_context_window(Some(&reg), "nope", "ghost-model"),
            context_window_for_model("ghost-model")
        );
        assert_eq!(resolve_context_window(Some(&reg), "nope", "ghost-model"), 100_000);
    }

    #[test]
    fn resolve_ignores_placeholder_4096_window() {
        let reg = registry_from_json(TEST_SNAPSHOT);
        // The registry stores the models.dev-omission placeholder (4096)...
        assert_eq!(
            reg.get("testprov", "tiny-model").unwrap().info.context_window,
            4096
        );
        // ...but resolve treats it as "unknown" and uses the heuristic instead
        // of compacting a real session at ~3.7k tokens.
        assert_eq!(
            resolve_context_window(Some(&reg), "testprov", "tiny-model"),
            context_window_for_model("tiny-model")
        );
        assert_eq!(resolve_context_window(Some(&reg), "testprov", "tiny-model"), 100_000);
    }

    #[test]
    fn resolve_sizes_free_mode_zen_pin_from_registry() {
        // Free mode pins carry the upstream inside the model id; the Zen
        // gateway lives under models.dev's "opencode" id (#393, #394).
        let reg = claurst_api::ModelRegistry::new();
        assert_eq!(
            resolve_context_window(Some(&reg), "free", "opencode-zen/minimax-m2.5-free"),
            204_800
        );
        assert_eq!(
            resolve_context_window(Some(&reg), "free", "zen/minimax-m2.5-free"),
            204_800
        );
        // `free/auto` has no single upstream; heuristic fallback applies.
        assert_eq!(resolve_context_window(Some(&reg), "free", "free/auto"), 100_000);
    }

    // ---- estimate_tokens_for_messages --------------------------------------

    #[test]
    fn test_token_estimate_nonempty() {
        let msgs = vec![make_user("Hello, world!")];
        let est = estimate_tokens_for_messages(&msgs);
        // "Hello, world!" = 13 chars → 13/4 = 3 rough tokens → 3*4/3 = 4 padded
        assert!(est > 0);
    }

    // ---- (1) token-budget keep (#231) --------------------------------------

    fn plain_convo(n: usize, size: usize) -> Vec<Message> {
        (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(filler(size))
                } else {
                    Message::assistant(filler(size))
                }
            })
            .collect()
    }

    #[test]
    fn keep_split_keeps_more_as_budget_grows() {
        // Eight plain messages, each filler(4000) ≈ 1333 tokens.
        let msgs = plain_convo(8, 4000);

        // Small budget keeps ~1 message; larger budget keeps ~3.
        let split_small = compute_keep_split_index(&msgs, 2000);
        let split_large = compute_keep_split_index(&msgs, 5000);

        // A larger budget keeps MORE messages ⇒ a smaller split index.
        assert!(
            split_large < split_small,
            "bigger budget must keep more (split_large={split_large}, split_small={split_small})"
        );
        assert_eq!(msgs.len() - split_small, 1, "2k budget keeps 1 message");
        assert_eq!(msgs.len() - split_large, 3, "5k budget keeps 3 messages");

        // Neither cut lands on a tool_result (trivially true for plain text).
        assert!(!message_has_tool_result(&msgs[split_small]));
        assert!(!message_has_tool_result(&msgs[split_large]));
    }

    #[test]
    fn keep_split_snaps_off_tool_result_boundary() {
        // A round whose tool_use is huge, so the raw budget cut lands right on
        // the tool_result — which would orphan it from its call.
        let msgs = vec![
            Message::user(filler(400)),                                       // 0
            assistant_tool_use("t1", "Bash", serde_json::json!({ "command": filler(8000) })), // 1 (BIG)
            user_tool_result("t1", &filler(400)),                            // 2 (tool_result)
            Message::assistant(filler(400)),                                 // 3
        ];

        // Raw token-budget index lands ON the tool_result (index 2).
        let raw = calculate_messages_to_keep_index(&msgs, 500);
        assert_eq!(raw, 2, "raw cut should land on the tool_result message");
        assert!(message_has_tool_result(&msgs[raw]));

        // The pairing-safe keep snaps back to the assistant tool_use boundary,
        // so the kept tail contains BOTH the tool_use and its result.
        let split = compute_keep_split_index(&msgs, 500);
        assert_eq!(split, 1);
        assert!(!message_has_tool_result(&msgs[split]));
        // tail = msgs[1..] carries tool_use(t1) AND its tool_result(t1).
        let tail = &msgs[split..];
        assert!(tail.iter().any(|m| !m.get_tool_use_blocks().is_empty()));
        assert!(tail.iter().any(message_has_tool_result));
    }

    #[test]
    fn snap_to_pairing_boundary_handles_keep_nothing() {
        let msgs = plain_convo(3, 100);
        // idx == len (keep nothing) is left as-is — the tail is empty, nothing to orphan.
        assert_eq!(snap_to_pairing_boundary(&msgs, msgs.len()), msgs.len());
    }

    // ---- (2) real-usage trigger (#231) -------------------------------------

    #[test]
    fn context_tokens_prefer_real_usage_over_estimate() {
        let msgs = vec![Message::user(filler(4000))]; // ≈ 1333 estimated tokens
        let estimate = estimate_tokens_for_messages(&msgs) as u64;
        let mark = |prompt_tokens, message_count| {
            Some(UsageMark { prompt_tokens, message_count })
        };

        // Real usage covering every message ⇒ used verbatim.
        assert_eq!(estimate_context_tokens(&msgs, mark(150_000, 1)), 150_000);

        // Messages appended after the anchored request are estimated on top.
        assert_eq!(estimate_context_tokens(&msgs, mark(150_000, 0)), 150_000 + estimate);

        // No usage / zero usage / stale anchor ⇒ fall back to the estimate.
        assert_eq!(estimate_context_tokens(&msgs, None), estimate);
        assert_eq!(estimate_context_tokens(&msgs, mark(0, 1)), estimate);
        assert_eq!(estimate_context_tokens(&msgs, mark(150_000, 5)), estimate);
    }

    #[test]
    fn threshold_leaves_room_for_the_output_reserve() {
        // Small reserve: the 90 % fraction wins.
        assert_eq!(auto_compact_threshold(200_000, 8_000), 180_000);
        // Large reserve: window minus reserve wins.
        assert_eq!(auto_compact_threshold(200_000, 32_000), 168_000);
        // Reserve is capped at a quarter of the window.
        assert_eq!(auto_compact_threshold(32_000, 64_000), 24_000);
        let state = AutoCompactState::default();
        assert!(should_auto_compact_for_window(170_000, 200_000, 32_000, &state));
        assert!(!should_auto_compact_for_window(170_000, 200_000, 8_000, &state));
    }

    // ---- (3) iterative UPDATE prompt (#231) --------------------------------

    #[test]
    fn extract_previous_summary_finds_compact_summary_block() {
        let notice = Message::user(
            "This session is being continued from a previous conversation.\n\n\
             <compact-summary>\nSummary:\n1. Primary Request: build X\n</compact-summary>",
        );
        let msgs = vec![notice, make_assistant("ok"), make_user("next")];
        let prev = extract_previous_summary(&msgs).expect("should detect prior summary");
        assert!(prev.contains("Primary Request: build X"));
        assert!(!prev.contains("<compact-summary>"));

        // No summary block ⇒ None.
        assert!(extract_previous_summary(&[make_user("hello"), make_assistant("hi")]).is_none());
    }

    #[test]
    fn update_prompt_selected_only_with_previous_summary() {
        let base = get_compact_prompt(None, None);
        let update = get_compact_prompt(None, Some("Summary:\n1. Primary Request: build X"));

        // UPDATE variant is distinct and references the previous summary.
        assert!(update.contains("UPDATE an existing conversation summary"));
        assert!(update.contains("<previous-summary>"));
        assert!(!base.contains("UPDATE an existing conversation summary"));

        // A blank previous summary is treated as "no previous summary".
        let blank = get_compact_prompt(None, Some("   "));
        assert_eq!(blank, base);

        // Newer history wins over the previous summary.
        assert!(update.contains("Newer history always takes precedence"));

        // Both variants share the terse template.
        for p in [&base, &update] {
            assert!(p.contains("## Objective"));
            assert!(p.contains("## Work State"));
            assert!(p.contains("## Next Move"));
            assert!(p.contains("## Relevant Files"));
        }
    }

    // ---- (4) files-touched manifest (#231) ---------------------------------

    fn read_use(id: &str, path: &str) -> Message {
        assistant_tool_use(id, "Read", serde_json::json!({ "file_path": path }))
    }
    fn edit_use(id: &str, path: &str) -> Message {
        assistant_tool_use(id, "Edit", serde_json::json!({ "file_path": path }))
    }
    fn write_use(id: &str, path: &str) -> Message {
        assistant_tool_use(id, "Write", serde_json::json!({ "file_path": path }))
    }

    #[test]
    fn manifest_lists_read_write_edit_files() {
        let msgs = vec![
            read_use("1", "/repo/a.rs"),
            edit_use("2", "/repo/b.rs"),
            write_use("3", "/repo/c.rs"),
            read_use("4", "/repo/a.rs"), // duplicate read — deduped
        ];
        let ops = extract_file_operations(&msgs);
        let manifest = format_files_touched(&ops);

        assert!(manifest.contains(FILES_TOUCHED_HEADER));
        // b.rs (edit) and c.rs (write) are "Modified"; a.rs is "Read".
        assert!(manifest.contains("Modified:"));
        assert!(manifest.contains("/repo/b.rs"));
        assert!(manifest.contains("/repo/c.rs"));
        assert!(manifest.contains("Read:"));
        assert!(manifest.contains("/repo/a.rs"));
    }

    #[test]
    fn manifest_edit_wins_over_read_for_same_file() {
        // A file that was both read and edited is reported only as Modified.
        let msgs = vec![read_use("1", "/repo/x.rs"), edit_use("2", "/repo/x.rs")];
        let (read_only, modified) = extract_file_operations(&msgs).computed_lists();
        assert_eq!(modified, vec!["/repo/x.rs".to_string()]);
        assert!(read_only.is_empty());
    }

    #[test]
    fn manifest_unions_with_prior_and_roundtrips() {
        // New batch touches new.rs (edit) and read_new.rs (read).
        let msgs = vec![
            edit_use("1", "/repo/new.rs"),
            read_use("2", "/repo/read_new.rs"),
        ];
        let mut ops = extract_file_operations(&msgs);

        // Prior manifest, as it would appear inside a previous <compact-summary>.
        let prior = "Summary:\n1. Primary Request: ...\n\nFiles touched:\n\
                     Modified: /repo/old.rs\nRead: /repo/read_old.rs";
        let parsed = parse_files_touched(prior);
        assert!(parsed.edited.contains("/repo/old.rs"));
        assert!(parsed.read.contains("/repo/read_old.rs"));

        ops.union(&parsed);
        let manifest = format_files_touched(&ops);

        // Both prior and new files survive the carry-forward.
        for f in [
            "/repo/new.rs",
            "/repo/old.rs",
            "/repo/read_new.rs",
            "/repo/read_old.rs",
        ] {
            assert!(manifest.contains(f), "manifest missing {f}:\n{manifest}");
        }

        // strip_files_touched_section removes the manifest for the UPDATE prompt.
        let stripped = strip_files_touched_section(prior);
        assert!(!stripped.contains(FILES_TOUCHED_HEADER));
        assert!(stripped.contains("Primary Request"));
    }

    #[test]
    fn manifest_is_bounded_with_overflow_marker() {
        // 25 edited files ⇒ list is capped at MAX_MANIFEST_FILES (20) with "+5 more".
        let msgs: Vec<Message> = (0..(MAX_MANIFEST_FILES + 5))
            .map(|i| edit_use(&format!("id{i}"), &format!("/repo/file_{i:02}.rs")))
            .collect();
        let manifest = format_files_touched(&extract_file_operations(&msgs));

        assert!(manifest.contains("(+5 more)"), "expected overflow marker:\n{manifest}");
        // Exactly MAX_MANIFEST_FILES paths are shown before the marker.
        let modified_line = manifest
            .lines()
            .find(|l| l.starts_with("Modified:"))
            .expect("Modified line");
        let listed = modified_line
            .trim_start_matches("Modified:")
            .split(" (+")
            .next()
            .unwrap()
            .matches(MANIFEST_SEP)
            .count()
            + 1;
        assert_eq!(listed, MAX_MANIFEST_FILES);
    }

    // ---- Issue #441: provider-agnostic, robust summariser --------------------

    /// One scripted summariser reply: `Some((text, stop))`, or `None` for a
    /// provider error.
    type Reply = Option<(&'static str, StopReason)>;

    /// A non-Anthropic provider that answers summarisation requests from a
    /// script and counts them.
    struct ScriptedSummariser {
        id: claurst_core::provider_id::ProviderId,
        replies: std::sync::Mutex<std::collections::VecDeque<Reply>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedSummariser {
        fn new(replies: Vec<Reply>) -> Self {
            Self {
                id: claurst_core::provider_id::ProviderId::new("openai"),
                replies: std::sync::Mutex::new(replies.into()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedSummariser {
        fn id(&self) -> &claurst_core::provider_id::ProviderId {
            &self.id
        }
        fn name(&self) -> &str {
            "scripted-summariser"
        }

        async fn create_message(
            &self,
            _request: ProviderRequest,
        ) -> Result<claurst_api::ProviderResponse, claurst_api::ProviderError> {
            unimplemented!("the summariser streams")
        }

        async fn create_message_stream(
            &self,
            request: ProviderRequest,
        ) -> Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = Result<StreamEvent, claurst_api::ProviderError>> + Send>,
            >,
            claurst_api::ProviderError,
        > {
            assert!(request.tools.is_empty(), "the summariser must not offer tools");
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let reply = self.replies.lock().unwrap().pop_front().flatten();
            let Some((text, stop)) = reply else {
                return Err(claurst_api::ProviderError::ServerError {
                    provider: self.id.clone(),
                    status: Some(500),
                    message: "boom".to_string(),
                    is_retryable: true,
                    retry_after: None,
                });
            };
            let events = vec![
                Ok(StreamEvent::TextDelta { index: 0, text: text.to_string() }),
                Ok(StreamEvent::MessageDelta { stop_reason: Some(stop), usage: None }),
                Ok(StreamEvent::MessageStop),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }

        async fn health_check(
            &self,
        ) -> Result<claurst_api::ProviderStatus, claurst_api::ProviderError> {
            Ok(claurst_api::ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> claurst_api::ProviderCapabilities {
            claurst_api::ProviderCapabilities {
                streaming: true,
                tool_calling: false,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: claurst_api::SystemPromptStyle::TopLevel,
            }
        }
    }

    const GOOD: Reply = Some(("<summary>Condensed history.</summary>", StopReason::EndTurn));

    /// Short user turns between large assistant replies: the keep-recent cut
    /// lands at index 8, leaving four user turns in the summarised head.
    fn turns_history() -> Vec<Message> {
        let users = ["alpha request", "beta request", "gamma request", "delta request", "epsilon request", "final request"];
        let mut out = Vec::new();
        for (i, user) in users.iter().enumerate() {
            out.push(make_user(user));
            if i + 1 < users.len() {
                out.push(make_assistant(&filler(30_000)));
            }
        }
        out
    }

    async fn compact(
        provider: &ScriptedSummariser,
        trigger: CompactionTrigger,
    ) -> Result<Option<Compaction>, ClaudeError> {
        compact_conversation(provider, "gpt-test", &turns_history(), trigger, 4_000, None).await
    }

    #[tokio::test]
    async fn non_anthropic_provider_compacts_and_records_the_entry() {
        let provider = ScriptedSummariser::new(vec![GOOD]);
        let history = turns_history();
        let compaction = compact(&provider, CompactionTrigger::Threshold)
            .await
            .unwrap()
            .expect("history over the keep budget compacts");

        assert_eq!(provider.calls(), 1);
        let msgs = &compaction.messages;
        let record = &compaction.record;
        assert!(msgs[0].get_all_text().contains("Condensed history."));
        assert_eq!(record.trigger, CompactionTrigger::Threshold);
        assert_eq!(msgs[0].uuid.as_deref(), Some(record.summary_uuid.as_str()));
        assert_eq!(record.messages_before, history.len());
        assert_eq!(record.messages_after, msgs.len());
        assert!(record.tokens_after < record.tokens_before);
        assert!(record.summarised);
        // The untouched tail follows the summary.
        assert_eq!(msgs.last().unwrap().get_all_text(), "final request");
    }

    #[tokio::test]
    async fn last_user_turns_are_kept_verbatim() {
        let provider = ScriptedSummariser::new(vec![GOOD]);
        let compaction = compact(&provider, CompactionTrigger::Threshold).await.unwrap().unwrap();
        let summary = compaction.messages[0].get_all_text();
        let recent = &summary[summary.find("<recent-user-messages>").expect("recent block")..];
        for kept in ["beta request", "gamma request", "delta request"] {
            assert!(recent.contains(kept), "missing {kept}: {recent}");
        }
        assert!(!recent.contains("alpha request"), "only the last {RECENT_USER_TURNS} turns");
    }

    #[tokio::test]
    async fn summariser_retries_once_after_a_rejected_summary() {
        // Empty, then good.
        let provider = ScriptedSummariser::new(vec![Some(("  ", StopReason::EndTurn)), GOOD]);
        assert!(compact(&provider, CompactionTrigger::Threshold).await.unwrap().is_some());
        assert_eq!(provider.calls(), 2);

        // Provider error, then good.
        let provider = ScriptedSummariser::new(vec![None, GOOD]);
        assert!(compact(&provider, CompactionTrigger::Manual).await.unwrap().is_some());
        assert_eq!(provider.calls(), 2);
    }

    #[tokio::test]
    async fn truncated_or_failing_summaries_fail_the_compaction() {
        let truncated = Some(("<summary>cut off", StopReason::MaxTokens));
        let provider = ScriptedSummariser::new(vec![truncated.clone(), truncated]);
        assert!(compact(&provider, CompactionTrigger::Threshold).await.is_err());
        assert_eq!(provider.calls(), 2, "one retry, then give up");
    }

    #[test]
    fn over_long_summary_is_rejected() {
        let head = vec![make_user(&filler(400))];
        let head_tokens = estimate_tokens_for_messages(&head);
        assert!(validate_summary(&filler(4_000), Some(StopReason::EndTurn), head_tokens).is_err());
        assert!(validate_summary("short", Some(StopReason::EndTurn), head_tokens).is_ok());
        // The discarded <analysis> block does not count against the summary.
        let reply = format!("<analysis>{}</analysis><summary>short</summary>", filler(4_000));
        assert_eq!(
            validate_summary(&reply, Some(StopReason::EndTurn), head_tokens).unwrap().trim(),
            format_compact_summary("<summary>short</summary>").trim()
        );
    }

    /// At the pre-request check the newest message can be a tool batch larger
    /// than the whole keep budget: the last round must survive compaction.
    #[test]
    fn keep_split_keeps_an_oversized_last_round() {
        let mut msgs = plain_convo(5, 30_000);
        msgs.push(assistant_tool_use("t1", "Read", serde_json::json!({"file_path": "a"})));
        msgs.push(Message::user_blocks(
            ["t1", "t2", "t3"]
                .iter()
                .map(|id| ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content: ToolResultContent::Text(filler(40_000)),
                    is_error: None,
                })
                .collect(),
        ));
        let split = compute_keep_split_index(&msgs, KEEP_RECENT_TOKENS);
        assert_eq!(split, msgs.len() - 2, "tail keeps the tool_use and its results");
    }

    #[tokio::test]
    async fn overflow_falls_back_to_the_untouched_tail_when_summaries_fail() {
        let provider = ScriptedSummariser::new(vec![None, None]);
        let history = turns_history();
        let compaction = compact(&provider, CompactionTrigger::Overflow)
            .await
            .unwrap()
            .expect("overflow recovery always yields a history");

        assert!(!compaction.record.summarised);
        assert_eq!(compaction.record.trigger, CompactionTrigger::Overflow);
        let notice = compaction.messages[0].get_all_text();
        assert!(notice.contains("could not be summarised"));
        assert!(notice.contains("delta request"), "recent user turns survive the fallback");
        // Everything after the notice is the original tail, untouched.
        let tail = &history[history.len() - (compaction.messages.len() - 1)..];
        let kept: Vec<String> = compaction.messages[1..].iter().map(|m| m.get_all_text()).collect();
        let expected: Vec<String> = tail.iter().map(|m| m.get_all_text()).collect();
        assert_eq!(kept, expected);
    }

    #[tokio::test]
    async fn nothing_to_compact_returns_none() {
        let provider = ScriptedSummariser::new(vec![GOOD]);
        let small = vec![make_user("hi"), make_assistant("hello")];
        let out = compact_conversation(&provider, "gpt-test", &small, CompactionTrigger::Manual, 4_000, None)
            .await
            .unwrap();
        assert!(out.is_none());
        assert_eq!(provider.calls(), 0);
    }
}
