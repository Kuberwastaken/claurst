// Tool-result sizing and compaction-result helpers used by the query loop.
// Extracted from lib.rs (issue #232); per-item tool-result cap from #441.

use crate::*;

/// Wrap a tool's output for the history, capped at `max_bytes` with the head
/// and tail kept and the omitted size noted (issue #441). Applied once when the
/// result is appended, so later requests never rewrite history.
/// `max_bytes == 0` disables the cap.
///
/// `images` (e.g. from MCP results, #451) follow the text as image blocks.
/// The cap applies to the text only: an image is never cut or stringified.
pub(crate) fn tool_result_for_history(
    content: String,
    images: Vec<ContentBlock>,
    max_bytes: usize,
) -> ToolResultContent {
    let text = if max_bytes == 0 || content.len() <= max_bytes {
        content
    } else {
        claurst_core::truncate::truncate_tool_output(&content, max_bytes).into_owned()
    };
    if images.is_empty() {
        return ToolResultContent::Text(text);
    }
    let mut blocks = Vec::with_capacity(images.len() + 1);
    blocks.push(ContentBlock::Text { text });
    blocks.extend(images);
    ToolResultContent::Blocks(blocks)
}

/// Apply the outcome of a reactive-compact / context-collapse call to the live
/// message list, preserving the conversation when compaction fails.
///
/// Fixes #213: the reactive paths used to `std::mem::take(messages)` before
/// calling `compact::reactive_compact` / `compact::context_collapse`. That left
/// `*messages` empty, and on ANY failure (API error, `Cancelled`, empty
/// summary) the drained messages were never restored — silently destroying the
/// live conversation. Here we only overwrite `*messages` when compaction
/// returns `Ok`; on `Err` the original messages are left completely untouched,
/// so a failed compaction can never wipe the session.
///
/// Returns `Ok(tokens_freed)` on success, or the original error on failure.
pub(crate) fn apply_compact_result<E>(
    messages: &mut Vec<Message>,
    outcome: Result<compact::CompactResult, E>,
) -> Result<u64, E> {
    match outcome {
        Ok(result) => {
            let tokens_freed = result.tokens_freed;
            *messages = result.messages;
            Ok(tokens_freed)
        }
        // Failure: leave `*messages` untouched so the conversation survives.
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::types::ImageSource;

    fn image() -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource {
                source_type: "base64".to_string(),
                media_type: Some("image/png".to_string()),
                data: Some("iVBORw0KGgo=".repeat(100)),
                url: None,
            },
        }
    }

    #[test]
    fn history_result_caps_text_and_keeps_images_whole() {
        let text = format!("HEAD{}TAIL", "x".repeat(10_000));
        match tool_result_for_history(text, vec![image()], 1_000) {
            ToolResultContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                match &blocks[0] {
                    ContentBlock::Text { text } => {
                        assert!(text.len() < 2_000, "text must be capped");
                        assert!(text.starts_with("HEAD") && text.ends_with("TAIL"));
                    }
                    other => panic!("expected text first, got {:?}", other),
                }
                match &blocks[1] {
                    ContentBlock::Image { source } => {
                        assert_eq!(source.data.as_deref().map(str::len), Some(1_200));
                    }
                    other => panic!("expected the image block, got {:?}", other),
                }
            }
            other => panic!("expected blocks, got {:?}", other),
        }
    }

    #[test]
    fn history_result_without_images_stays_text() {
        assert!(matches!(
            tool_result_for_history("ok".to_string(), Vec::new(), 1_000),
            ToolResultContent::Text(t) if t == "ok"
        ));
    }
}
