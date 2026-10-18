//! Memory extraction: turn-end facts into the memdir store.
//!
//! After a turn ends, a single extra request through the session's ACTIVE
//! provider and model extracts facts worth remembering. Results are written
//! into the memdir store (`claurst_core::memdir`), so the existing read path
//! (`MEMORY.md` index injected into the system prompt) serves them; there is
//! no parallel store and no separate memory model.
//!
//! Cost stays bounded: extraction is off unless the global setting
//! `memoryExtractionEnabled` is on AND the existing auto-memory switches
//! (`CLAURST_DISABLE_AUTO_MEMORY`, `CLAURST_SIMPLE`, `autoMemoryEnabled`)
//! allow it; only subagents of the top-level loop opt in, so detached loops
//! (subagents, cron, ACP, auto-dream) never extract; the request sees only
//! messages since the last extraction, capped in messages and characters.
//! Failures are logged at debug and never break the turn.

use claurst_api::{AnthropicClient, LlmProvider, ProviderRequest, SystemPrompt};
use claurst_core::config::Config;
use claurst_core::memdir::{self, AutoMemoryEntry, MemoryType};
use claurst_core::types::{Message, Role};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Minimum messages before the FIRST extraction of a session. Mirrors the
/// original threshold from the TypeScript extractor.
const MIN_MESSAGES_TO_EXTRACT: usize = 20;

/// Messages sent per extraction request: everything since the last
/// extraction, newest kept, bounded so cost grows linearly, not
/// quadratically.
const MAX_MESSAGES_PER_EXTRACTION: usize = 20;

/// Hard character cap on the transcript body sent to the model.
const MAX_TRANSCRIPT_CHARS: usize = 8_000;

/// Tool-result preview length in the transcript.
const TOOL_RESULT_PREVIEW_CHARS: usize = 200;

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

/// Whether turn-end extraction may run at all.
///
/// All of the following must hold:
/// - `memoryExtractionEnabled` is `true` in the merged settings (global-only
///   in the merge, so a project file cannot flip it on);
/// - the memdir auto-memory switches allow it: `CLAURST_DISABLE_AUTO_MEMORY`
///   truthy or `CLAURST_SIMPLE` disables, `autoMemoryEnabled: false`
///   disables (all via `claurst_core::memdir::is_auto_memory_enabled`);
/// - this loop is not a subagent (subagents, cron, ACP and auto-dream all
///   reach the query loop; only the top-level session may spend the extra
///   call).
pub fn extraction_allowed(config: &Config, is_subagent: bool) -> bool {
    if !config.memory_extraction_enabled {
        return false;
    }
    if is_subagent {
        return false;
    }
    memdir::is_auto_memory_enabled(config.auto_memory_enabled)
}

// ---------------------------------------------------------------------------
// Incremental cursor
// ---------------------------------------------------------------------------

/// Which messages a run has already extracted, persisted next to the store so
/// a second extraction only sees messages added since the last one.
///
/// The cursor stores the last message's UUID plus the history length at the
/// time. UUIDs survive compaction rewrites better than indices; the length
/// detects a compacted/truncated history where the UUID no longer resolves
/// (fall back to re-extracting the capped tail).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ExtractionCursor {
    /// UUID of the last message covered by the previous extraction.
    pub last_uuid: Option<String>,
    /// History length at the previous extraction.
    pub message_count: usize,
}

impl ExtractionCursor {
    /// The slice of `messages` the next extraction should see.
    ///
    /// Slicing happens before any transcript building, so the gate and the
    /// cursor are the only work done when extraction does not fire.
    pub fn window<'a>(&self, messages: &'a [Message]) -> &'a [Message] {
        let start = match &self.last_uuid {
            None => 0,
            Some(uuid) => {
                let pos = messages
                    .iter()
                    .position(|m| m.uuid.as_deref() == Some(uuid.as_str()));
                match pos {
                    // Cursor resolved: everything strictly after it is new.
                    Some(pos) if self.message_count <= messages.len() => pos + 1,
                    // History was rewritten (compaction, rewind): fall back
                    // to the capped tail; the cap bounds the cost of the
                    // re-extraction.
                    _ => messages.len().saturating_sub(MAX_MESSAGES_PER_EXTRACTION),
                }
            }
        };
        let start = start.min(messages.len());
        // Cap at the tail: never more than MAX_MESSAGES_PER_EXTRACTION
        // messages. `max` (not `min`) so an empty window (nothing new since
        // the cursor) stays empty instead of being re-expanded to the cap.
        let start = start.max(messages.len().saturating_sub(MAX_MESSAGES_PER_EXTRACTION));
        &messages[start..]
    }

    /// Advance to `messages` (the full history, not the window).
    pub fn advance(&mut self, messages: &[Message]) {
        self.last_uuid = messages.last().and_then(|m| m.uuid.clone());
        self.message_count = messages.len();
    }
}

// ---------------------------------------------------------------------------
// Transcript building (runs only after the gate passes)
// ---------------------------------------------------------------------------

/// Build the extraction transcript from a window of messages: text with role
/// labels, tool uses summarised, tool results cut to a preview, system
/// reminders skipped, capped to `MAX_MESSAGES_PER_EXTRACTION` messages and
/// `MAX_TRANSCRIPT_CHARS` characters.
pub fn build_transcript(messages: &[Message]) -> String {
    let window = &messages[messages.len().saturating_sub(MAX_MESSAGES_PER_EXTRACTION)..];
    let mut transcript = String::new();
    for msg in window {
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        let mut block = format!("{role}: ");
        push_message_text(&mut block, msg);
        if block.trim() == format!("{role}:").trim_end() {
            continue; // nothing model-visible in this message
        }
        transcript.push_str(&block);
        transcript.push_str("\n\n");
        if transcript.len() >= MAX_TRANSCRIPT_CHARS {
            break;
        }
    }
    if transcript.len() > MAX_TRANSCRIPT_CHARS {
        let mut cut = MAX_TRANSCRIPT_CHARS;
        while !transcript.is_char_boundary(cut) {
            cut -= 1;
        }
        transcript.truncate(cut);
        transcript.push_str("\n[...]");
    }
    transcript
}

/// Append the model-visible content of one message to `out`, skipping
/// `<system-reminder>` blocks (internal instrumentation, not user signal).
fn push_message_text(out: &mut String, msg: &Message) {
    use claurst_core::types::{ContentBlock, MessageContent};
    match &msg.content {
        MessageContent::Text(text) => {
            if !text.trim_start().starts_with("<system-reminder>") {
                out.push_str(text);
            }
        }
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text { text } => {
                        if !text.trim_start().starts_with("<system-reminder>") {
                            out.push_str(text);
                        }
                    }
                    ContentBlock::ToolUse { name, .. } => {
                        out.push_str(&format!("[used tool: {name}]"));
                    }
                    ContentBlock::ToolResult { content, .. } => {
                        let preview = tool_result_preview(content);
                        out.push_str(&format!("[result: {preview}]"));
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Cut a tool result to a short preview for the transcript.
fn tool_result_preview(content: &claurst_core::types::ToolResultContent) -> String {
    use claurst_core::types::{ContentBlock, ToolResultContent};
    let text = match content {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    if text.chars().count() <= TOOL_RESULT_PREVIEW_CHARS {
        return text;
    }
    let mut end = TOOL_RESULT_PREVIEW_CHARS;
    for (idx, _) in text.char_indices() {
        if idx >= end {
            end = idx;
            break;
        }
    }
    format!("{}...", text[..end].trim_end())
}

// ---------------------------------------------------------------------------
// Extraction call (ACTIVE provider and model only)
// ---------------------------------------------------------------------------

/// Where one extraction request goes and where its results land.
#[derive(Clone)]
pub struct ExtractionRoute {
    /// Provider id and model of the session, resolved the same way the main
    /// request resolves them. No fallback: if the active provider cannot
    /// serve the request, extraction silently skips.
    pub provider_id: String,
    pub model_id: String,
    /// Dispatch target: the provider when set, otherwise the raw Anthropic
    /// client (mirrors the main loop's `use_provider_dispatch`).
    pub provider: Option<Arc<dyn LlmProvider>>,
    pub anthropic_client: AnthropicClient,
    /// memdir store directory for the project.
    pub store_dir: std::path::PathBuf,
}

/// Send one extraction request through the active route and return the raw
/// model text. Any failure maps to `None` (debug log, turn unaffected).
pub async fn run_extraction_request(
    route: &ExtractionRoute,
    transcript: &str,
    working_dir: &Path,
) -> Option<String> {
    let prompt = build_extraction_prompt(transcript, &working_dir.display().to_string());
    let user_msg = Message::user(prompt);

    match &route.provider {
        Some(provider) => {
            let request = ProviderRequest {
                model: route.model_id.clone(),
                messages: vec![user_msg],
                system_prompt: Some(SystemPrompt::Text(EXTRACTION_SYSTEM_PROMPT.to_string())),
                tools: Vec::new(),
                max_tokens: 1024,
                temperature: Some(0.0),
                top_p: None,
                top_k: None,
                stop_sequences: Vec::new(),
                thinking: None,
                provider_options: serde_json::Value::Object(serde_json::Map::new()),
            };
            match provider.create_message(request).await {
                Ok(response) => {
                    let text = response
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            claurst_core::types::ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    Some(text)
                }
                Err(e) => {
                    debug!(
                        provider = %route.provider_id,
                        error = %e,
                        "memory extraction skipped: active provider failed"
                    );
                    None
                }
            }
        }
        None => {
            use claurst_api::{ApiMessage, CreateMessageRequest};
            let request = CreateMessageRequest::builder(&route.model_id, 1024)
                .messages(vec![ApiMessage {
                    role: "user".to_string(),
                    content: serde_json::Value::String(user_msg.get_all_text()),
                }])
                .system(SystemPrompt::Text(EXTRACTION_SYSTEM_PROMPT.to_string()))
                .temperature(0.0)
                .build();
            match route.anthropic_client.create_message(request).await {
                Ok(response) => {
                    let text = response
                        .content
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("");
                    Some(text)
                }
                Err(e) => {
                    debug!(
                        error = %e,
                        "memory extraction skipped: active provider failed"
                    );
                    None
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing into memdir entries
// ---------------------------------------------------------------------------

/// Parse the model's `MEMORY:` lines into memdir entries, mapping the
/// extraction categories onto the canonical memdir types.
///
/// Expected line format (unchanged from the previous engine):
/// `MEMORY: <category> | <confidence 0-10> | <concise fact>`
pub fn parse_extraction_response(response: &str) -> Vec<AutoMemoryEntry> {
    let mut entries = Vec::new();
    for line in response.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("MEMORY:") else {
            continue;
        };
        let parts: Vec<&str> = rest.splitn(3, '|').collect();
        if parts.len() < 3 {
            warn!("Skipping malformed memory line: {line}");
            continue;
        }
        let (category, _confidence, fact) = (parts[0].trim(), parts[1].trim(), parts[2].trim());
        let fact = fact.trim();
        if fact.is_empty() {
            continue;
        }
        entries.push(AutoMemoryEntry {
            name: topic_name(category, fact),
            description: one_line(fact),
            content: fact.to_string(),
            memory_type: category_to_memdir_type(category),
        });
    }
    entries
}

/// Map an extraction category onto a memdir `MemoryType`.
fn category_to_memdir_type(category: &str) -> MemoryType {
    match category.to_lowercase().trim() {
        "user_preference" | "userpreference" | "preference" => MemoryType::User,
        "code_pattern" | "codepattern" | "pattern" | "constraint" => MemoryType::Feedback,
        "decision" => MemoryType::Project,
        // "project_fact" and anything unrecognized: a fact about the project.
        _ => MemoryType::Project,
    }
}

/// Short topic label used as the file slug and frontmatter name.
fn topic_name(category: &str, fact: &str) -> String {
    format!("{category}: {}", first_words(fact, 6))
}

/// First `n` words of `s`, for slug-sized labels.
fn first_words(s: &str, n: usize) -> String {
    let words: Vec<&str> = s.split_whitespace().collect();
    words[..words.len().min(n)].join(" ")
}

/// Single-line, length-capped hook for the MEMORY.md index.
fn one_line(fact: &str) -> String {
    let flat: String = fact
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = flat.trim();
    if trimmed.chars().count() > 160 {
        first_words(trimmed, 20)
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// Extraction prompt (unchanged output contract)
// ---------------------------------------------------------------------------

const EXTRACTION_SYSTEM_PROMPT: &str = "You are a memory extraction assistant. Your job is to \
identify key facts, preferences, patterns, and decisions from a conversation that would be \
useful to remember for future interactions. Be precise, concise, and only extract genuinely \
useful information. Do not extract trivial or transient details.";

fn build_extraction_prompt(transcript: &str, working_dir: &str) -> String {
    format!(
        "Please analyze the following conversation transcript from a coding session in \
directory `{}` and extract key memories that would be useful to remember for future \
interactions.\n\
\n\
For each memory, output a line in this exact format:\n\
MEMORY: <category> | <confidence 0-10> | <concise fact>\n\
\n\
Where <category> is one of:\n\
- user_preference: how the user likes to work, communication style, tool preferences\n\
- project_fact: facts about the codebase, architecture, languages, frameworks\n\
- code_pattern: coding patterns, idioms, or styles used in this project\n\
- decision: key decisions made during the session\n\
- constraint: constraints, requirements, or limitations discovered\n\
\n\
Only output MEMORY: lines — no other text.  If there are no useful memories, output nothing.\n\
\n\
<conversation>\n\
{}\n\
</conversation>",
        working_dir, transcript
    )
}

// ---------------------------------------------------------------------------
// End-to-end turn-end hook
// ---------------------------------------------------------------------------

/// Turn-end extraction entry point, called from the query loop's `end_turn`
/// branch AFTER the gate (`extraction_allowed`) has passed.
///
/// Runs synchronously (the loop spawns it as a detached task): builds the
/// incremental window, makes the single request through the active route,
/// writes new facts into the memdir store, and persists the cursor. Returns
/// the number of new entries stored.
pub async fn extract_and_store(
    route: &ExtractionRoute,
    messages: &[Message],
    working_dir: &Path,
) -> usize {
    let cursor_path = route.store_dir.join(".extraction_cursor.json");
    let cursor: ExtractionCursor = tokio::fs::read_to_string(&cursor_path)
        .await
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let window = cursor.window(messages);
    if window.is_empty() {
        return 0;
    }
    // The session must be long enough to be worth a request at all; the
    // cursor keeps later extractions cheap.
    if messages.len() < MIN_MESSAGES_TO_EXTRACT {
        return 0;
    }

    let transcript = build_transcript(window);
    if transcript.trim().is_empty() {
        return 0;
    }

    let Some(response) = run_extraction_request(route, &transcript, working_dir).await else {
        return 0;
    };
    let entries = parse_extraction_response(&response);
    let new_count = entries.len();
    if new_count > 0 {
        let written = tokio::task::spawn_blocking({
            let dir = route.store_dir.clone();
            move || memdir::append_auto_memories(&dir, &entries)
        })
        .await
        .unwrap_or_default();
        info!(
            count = written.len(),
            "Memory extraction stored new entries"
        );
    }

    // Advance the cursor whether or not the model produced anything: the
    // window was seen. Write through the same atomic path as the store.
    let mut advanced = cursor;
    advanced.advance(messages);
    if let Ok(json) = serde_json::to_string(&advanced) {
        let tmp = cursor_path.with_extension("json.tmp");
        if tokio::fs::write(&tmp, &json).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &cursor_path).await;
        }
    }
    new_count
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::types::{ContentBlock, MessageContent};

    fn make_messages(n: usize) -> Vec<Message> {
        (0..n)
            .map(|i| {
                let mut msg = if i % 2 == 0 {
                    Message::user(format!("user message {i}"))
                } else {
                    Message::assistant(format!("assistant reply {i}"))
                };
                msg.uuid = Some(format!("uuid-{i}"));
                msg
            })
            .collect()
    }

    // ---- gate -----------------------------------------------------------

    #[test]
    fn gate_off_by_default_and_by_setting_and_env() {
        // Default config: extraction disabled (memoryExtractionEnabled
        // defaults to false).
        let config = Config::default();
        assert!(!extraction_allowed(&config, false), "off by default");

        // Explicitly enabled.
        let config = Config {
            memory_extraction_enabled: true,
            ..Config::default()
        };
        assert!(extraction_allowed(&config, false));

        // Subagents never extract.
        assert!(!extraction_allowed(&config, true));

        // autoMemoryEnabled false disables extraction.
        let config = Config {
            memory_extraction_enabled: true,
            auto_memory_enabled: Some(false),
            ..Config::default()
        };
        assert!(!extraction_allowed(&config, false));

        // CLAURST_DISABLE_AUTO_MEMORY truthy disables extraction.
        // (Sandboxed: can't set env vars per-test safely in the same
        // process; is_auto_memory_enabled's own logic covers this branch.)
    }

    // ---- incremental cursor ---------------------------------------------

    #[test]
    fn cursor_first_extraction_sees_all_messages() {
        let cursor = ExtractionCursor::default();
        let msgs = make_messages(30);
        let window = cursor.window(&msgs);
        assert_eq!(window.len(), 20, "first extraction caps at the tail");
        assert_eq!(
            window.last().and_then(|m| m.uuid.as_deref()),
            Some("uuid-29")
        );
    }

    #[test]
    fn cursor_second_extraction_sees_only_new_messages() {
        let msgs = make_messages(30);
        let mut cursor = ExtractionCursor::default();
        cursor.advance(&msgs);

        // Ten new messages arrive.
        let mut msgs2 = msgs.clone();
        for i in 30..40 {
            let mut msg = Message::user(format!("user message {i}"));
            msg.uuid = Some(format!("uuid-{i}"));
            msgs2.push(msg);
        }
        let window = cursor.window(&msgs2);
        assert_eq!(window.len(), 10);
        assert_eq!(
            window.first().and_then(|m| m.uuid.as_deref()),
            Some("uuid-30")
        );
        assert_eq!(
            window.last().and_then(|m| m.uuid.as_deref()),
            Some("uuid-39")
        );
    }

    #[test]
    fn cursor_no_new_messages_yields_empty_window() {
        let msgs = make_messages(30);
        let mut cursor = ExtractionCursor::default();
        cursor.advance(&msgs);
        assert!(cursor.window(&msgs).is_empty());
    }

    #[test]
    fn cursor_unresolved_uuid_falls_back_to_capped_tail() {
        // Cursor names a UUID that no longer exists (compaction rewrote the
        // history) and a length longer than the history: re-extract the
        // capped tail rather than everything.
        let cursor = ExtractionCursor {
            last_uuid: Some("gone".to_string()),
            message_count: 50,
        };
        let msgs = make_messages(30);
        let window = cursor.window(&msgs);
        assert_eq!(window.len(), 20);
        assert_eq!(
            window.last().and_then(|m| m.uuid.as_deref()),
            Some("uuid-29")
        );
    }

    // ---- transcript ------------------------------------------------------

    #[test]
    fn transcript_skips_system_reminders_and_labels_roles() {
        let blocks = vec![
            ContentBlock::Text {
                text: "<system-reminder>noise</system-reminder>".to_string(),
            },
            ContentBlock::Text {
                text: "hello world".to_string(),
            },
        ];
        let msg = Message::user_blocks(blocks);
        let transcript = build_transcript(&[msg]);
        assert!(transcript.contains("User:"));
        assert!(transcript.contains("hello world"));
        assert!(!transcript.contains("system-reminder"));
    }

    #[test]
    fn transcript_summarises_tool_use_and_previews_results() {
        let blocks = vec![
            ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "Bash".to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            },
            ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: claurst_core::types::ToolResultContent::Text("x".repeat(500)),
                is_error: None,
            },
        ];
        let msg = Message::user_blocks(blocks);
        let transcript = build_transcript(&[msg]);
        assert!(transcript.contains("[used tool: Bash]"));
        assert!(transcript.contains("[result: "));
        assert!(transcript.contains("..."));
        assert!(transcript.len() < 400, "preview must be short");
    }

    #[test]
    fn transcript_caps_messages_and_chars() {
        let mut msgs = make_messages(60);
        // Force one long assistant message over the char cap.
        msgs.push(Message::assistant("y".repeat(20_000)));
        let transcript = build_transcript(&msgs);
        assert!(
            transcript.len() <= MAX_TRANSCRIPT_CHARS + 8,
            "hard cap holds"
        );
        assert!(transcript.ends_with("[...]"));
    }

    #[test]
    fn transcript_char_cap_respects_multibyte_boundaries() {
        let mut msg = Message::user("é".repeat(10_000));
        msg.content = MessageContent::Text("é".repeat(10_000));
        let transcript = build_transcript(&[msg]);
        assert!(transcript.len() <= MAX_TRANSCRIPT_CHARS + 8);
        assert!(String::from_utf8(transcript.into_bytes()).is_ok());
    }

    // ---- parsing ---------------------------------------------------------

    #[test]
    fn parse_empty_response() {
        assert!(parse_extraction_response("").is_empty());
    }

    #[test]
    fn parse_maps_categories_to_memdir_types() {
        let response = "\
MEMORY: user_preference | 9 | User prefers verbose error messages\n\
MEMORY: decision | 7 | Decided to use tokio async runtime\n\
MEMORY: constraint | 6 | Must support Windows paths\n\
MEMORY: project_fact | 8 | The project uses Rust 2021 edition\n\
MEMORY: code_pattern | 7 | Uses builder pattern";
        let entries = parse_extraction_response(response);
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].memory_type, MemoryType::User);
        assert_eq!(entries[1].memory_type, MemoryType::Project);
        assert_eq!(entries[2].memory_type, MemoryType::Feedback);
        assert_eq!(entries[3].memory_type, MemoryType::Project);
        assert_eq!(entries[4].memory_type, MemoryType::Feedback);
        assert!(entries[0].content.contains("verbose error messages"));
    }

    #[test]
    fn parse_ignores_non_memory_and_malformed_lines() {
        let response = "Here are the memories:\n\
MEMORY: project_fact | 8 | Uses serde for JSON\n\
MEMORY: only_two_parts | no_confidence\n\
extra text\n\
MEMORY: code_pattern | 7 | Uses builder pattern";
        let entries = parse_extraction_response(response);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn parse_empty_fact_is_skipped() {
        let response = "MEMORY: project_fact | 8 |    ";
        assert!(parse_extraction_response(response).is_empty());
    }

    #[test]
    fn parse_entry_shape_is_single_line() {
        let response =
            "MEMORY: project_fact | 8 | First line\nsecond line that is quite long indeed";
        let entries = parse_extraction_response(response);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].description.contains('\n'));
    }
}