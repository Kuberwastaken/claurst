// memory_extraction.rs — Turn-end memory extraction over the MemoryManager.
//
// Ported from jcode's `memory_agent.rs` final-extraction path and
// `memory.rs::extract_from_transcript`, adapted to claurst:
//
//   1. At end-turn (the query loop's "end_turn" branch), build a compact
//      transcript from the session messages.
//   2. Gate on the settings memory block (memoryEnabled and
//      memorySidecarEnabled, with env overrides).
//   3. Dispatch extraction through the MemorySidecar (branch 3), which routes
//      to whatever provider claurst can reach — no Anthropic-only path.
//   4. Store extracted entries in the project MemoryManager store (branch 1)
//      with category/trust mapped from the extraction output.
//
// Fire-and-forget: the query loop spawns a detached Tokio task and never
// blocks on extraction. Failures are logged at debug level, mirroring
// jcode's "optional memory extraction failed" behavior.

use claurst_api::memory_sidecar::{ExtractedMemory, MemorySidecar};
use claurst_core::memory_config::MemorySettings;
use claurst_core::memory_types::{MemoryCategory, MemoryEntry, MemoryManager, TrustLevel};
use claurst_core::types::{ContentBlock, Message, MessageContent, Role};
use std::path::Path;

/// Minimum transcript length before extraction is attempted, mirroring
/// jcode's `trigger_final_extraction_with_dir` guard.
const MIN_TRANSCRIPT_CHARS: usize = 200;

/// Whether the memory system (extraction side) is enabled and a sidecar
/// backend is reachable. Mirrors jcode's `memory_llm_judge_available`.
pub fn memory_extraction_available(settings: &MemorySettings) -> bool {
    settings.memory_enabled && settings.memory_sidecar_enabled
}

/// Build a compact transcript for extraction, mirroring jcode's
/// `build_transcript_for_extraction`: text blocks joined with role labels,
/// tool uses summarised, tool results truncated to a 200-char preview.
/// System reminders are skipped.
pub fn build_transcript_for_extraction(messages: &[Message]) -> String {
    let mut transcript = String::new();
    for msg in messages {
        if msg.role != Role::User && msg.role != Role::Assistant {
            continue;
        }
        let role = match msg.role {
            Role::User => "User",
            _ => "Assistant",
        };
        transcript.push_str(&format!("**{}:**\n", role));
        match &msg.content {
            MessageContent::Text(text) => {
                if !text.trim_start().starts_with("<system-reminder>") {
                    transcript.push_str(text);
                    transcript.push('\n');
                }
            }
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            if text.trim_start().starts_with("<system-reminder>") {
                                continue;
                            }
                            transcript.push_str(text);
                            transcript.push('\n');
                        }
                        ContentBlock::ToolUse { name, .. } => {
                            transcript.push_str(&format!("[Used tool: {}]\n", name));
                        }
                        ContentBlock::ToolResult { content, .. } => {
                            let preview = tool_result_preview(content);
                            transcript.push_str(&format!("[Result: {}]\n", preview));
                        }
                        ContentBlock::Image { .. } => {
                            transcript.push_str("[Image]\n");
                        }
                        _ => {}
                    }
                }
            }
        }
        transcript.push('\n');
    }
    transcript
}

/// Truncate a tool result to a short preview for the transcript.
fn tool_result_preview(content: &claurst_core::types::ToolResultContent) -> String {
    let text = match content {
        claurst_core::types::ToolResultContent::Text(t) => t.clone(),
        claurst_core::types::ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    if text.len() > 200 {
        let mut end = 200;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &text[..end])
    } else {
        text
    }
}

/// Map an extracted trust string to a TrustLevel, mirroring jcode's
/// `extract_from_transcript` mapping ("high" / "medium" / anything else → Low).
fn trust_from_extracted(trust: &str) -> TrustLevel {
    match trust {
        "high" => TrustLevel::High,
        "medium" => TrustLevel::Medium,
        _ => TrustLevel::Low,
    }
}

/// Extract memories from a transcript and store them in the project-scoped
/// MemoryManager, returning the ids of the stored entries.
///
/// Mirrors jcode's `MemoryManager::extract_from_transcript`, minus the
/// Jev-only event bookkeeping (no event bus in claurst yet; the MemoryEvent
/// surface was deliberately left out of the branch 1 port).
pub async fn extract_and_store(
    transcript: &str,
    session_id: &str,
    working_dir: &Path,
) -> anyhow::Result<Vec<String>> {
    let settings = MemorySettings::load();
    if !memory_extraction_available(&settings) {
        tracing::debug!("Memory transcript extraction skipped: disabled in settings");
        return Ok(Vec::new());
    }

    let sidecar = MemorySidecar::new(&settings, None);
    if sidecar.is_dormant() {
        tracing::debug!("Memory transcript extraction skipped: no usable sidecar backend");
        return Ok(Vec::new());
    }

    let extracted = sidecar.extract_memories(transcript).await?;

    let manager = MemoryManager::new().with_project_dir(working_dir);
    let mut ids = Vec::new();
    for memory in extracted {
        let entry = extracted_to_entry(&memory, session_id);
        match manager.remember_project(entry) {
            Ok(id) => ids.push(id),
            Err(err) => {
                // One bad entry should not fail the whole batch; keep going
                // and report at the end.
                tracing::warn!(
                    error = %err,
                    "Failed to store extracted memory"
                );
            }
        }
    }

    if !ids.is_empty() {
        tracing::info!(count = ids.len(), "Memory extraction stored new entries");
    }
    Ok(ids)
}

/// Convert a sidecar-extracted memory into a MemoryManager entry.
fn extracted_to_entry(memory: &ExtractedMemory, session_id: &str) -> MemoryEntry {
    MemoryEntry::new(
        MemoryCategory::from_extracted(&memory.category),
        memory.content.clone(),
    )
    .with_source(session_id)
    .with_trust(trust_from_extracted(&memory.trust))
}

/// Fire-and-forget end-of-turn extraction: spawn a detached task unless the
/// transcript is too short or extraction is disabled. Mirrors jcode's
/// `trigger_final_extraction_with_dir`.
///
/// Must be called from within a Tokio runtime (the query loop is).
pub fn trigger_final_extraction(transcript: String, session_id: String, working_dir: &Path) {
    if transcript.len() < MIN_TRANSCRIPT_CHARS {
        return;
    }

    let working_dir = working_dir.to_path_buf();
    tokio::spawn(async move {
        if let Err(err) = extract_and_store(&transcript, &session_id, &working_dir).await {
            tracing::debug!(error = %err, "Optional memory extraction failed");
        }
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_transcript_labels_roles_and_skips_reminders() {
        // Reminder blocks are skipped only when the block itself starts with
        // the marker (jcode semantics); a reminder appended mid-text stays.
        let blocks = vec![
            ContentBlock::Text {
                text: "<system-reminder>noise</system-reminder>".to_string(),
            },
            ContentBlock::Text {
                text: "hello world".to_string(),
            },
        ];
        let msg = Message::user_blocks(blocks);
        let transcript = build_transcript_for_extraction(&[msg]);
        assert!(transcript.contains("**User:**"));
        assert!(transcript.contains("hello world"));
        assert!(!transcript.contains("system-reminder"));
    }

    #[test]
    fn build_transcript_summarises_tool_use() {
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
        let transcript = build_transcript_for_extraction(&[msg]);
        assert!(transcript.contains("[Used tool: Bash]"));
        assert!(transcript.contains("[Result: "));
        // Preview is truncated to 200 chars + "...".
        assert!(transcript.contains("..."));
    }

    #[test]
    fn trust_mapping_matches_jcode() {
        assert_eq!(trust_from_extracted("high"), TrustLevel::High);
        assert_eq!(trust_from_extracted("medium"), TrustLevel::Medium);
        assert_eq!(trust_from_extracted("anything"), TrustLevel::Low);
    }

    #[test]
    fn extraction_gates_off_on_settings() {
        let settings = MemorySettings {
            memory_enabled: false,
            ..Default::default()
        };
        assert!(!memory_extraction_available(&settings));
        let settings = MemorySettings {
            memory_sidecar_enabled: false,
            ..Default::default()
        };
        assert!(!memory_extraction_available(&settings));
    }

    #[test]
    fn extracted_to_entry_maps_category_trust_source() {
        let memory = ExtractedMemory {
            category: "preference".to_string(),
            content: "User prefers short answers".to_string(),
            trust: "high".to_string(),
        };
        let entry = extracted_to_entry(&memory, "sess-1");
        assert_eq!(entry.category, MemoryCategory::Preference);
        assert_eq!(entry.trust, TrustLevel::High);
        assert_eq!(entry.source.as_deref(), Some("sess-1"));
    }
}
