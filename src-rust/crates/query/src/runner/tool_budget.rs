// Tool-result budgeting helpers used when trimming/compacting the transcript.
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use crate::*;

/// Return the combined character count of all tool-result content blocks found
/// in `messages`.  Only user messages are examined (tool results always live
/// in user turns).
pub(crate) fn total_tool_result_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| m.role == claurst_core::types::Role::User)
        .flat_map(|m| match &m.content {
            claurst_core::types::MessageContent::Blocks(blocks) => blocks.as_slice(),
            _ => &[],
        })
        .filter_map(|b| {
            if let ContentBlock::ToolResult { content, .. } = b {
                Some(match content {
                    ToolResultContent::Text(t) => t.len(),
                    ToolResultContent::Blocks(blocks) => blocks.iter().map(|b| {
                        if let ContentBlock::Text { text } = b { text.len() } else { 0 }
                    }).sum(),
                })
            } else {
                None
            }
        })
        .sum()
}

/// When the cumulative tool-result content exceeds `budget` characters, walk
/// the message list from oldest to newest and replace individual
/// `ToolResult` content with a placeholder until the running total is back
/// under budget.  Returns the (possibly modified) message list and the
/// number of results that were truncated.
///
/// Mirrors the spirit of the TypeScript `applyToolResultBudget` /
/// `enforceToolResultBudget` logic, simplified to a straightforward
/// oldest-first eviction without the session-persistence layer.
pub(crate) fn apply_tool_result_budget(messages: Vec<Message>, budget: usize) -> (Vec<Message>, usize) {
    let total = total_tool_result_chars(&messages);
    if total <= budget {
        return (messages, 0);
    }

    let mut to_shed = total - budget;
    let mut truncated = 0usize;
    let mut result = messages;

    'outer: for msg in result.iter_mut() {
        if msg.role != claurst_core::types::Role::User {
            continue;
        }
        let blocks = match &mut msg.content {
            claurst_core::types::MessageContent::Blocks(b) => b,
            _ => continue,
        };
        for block in blocks.iter_mut() {
            if let ContentBlock::ToolResult { content, .. } = block {
                let size = match &*content {
                    ToolResultContent::Text(t) => t.len(),
                    ToolResultContent::Blocks(inner) => inner.iter().map(|b| {
                        if let ContentBlock::Text { text } = b { text.len() } else { 0 }
                    }).sum(),
                };
                if size == 0 {
                    continue;
                }
                *content = ToolResultContent::Text(
                    "[tool result truncated to save context]".to_string(),
                );
                truncated += 1;
                if size > to_shed {
                    break 'outer;
                }
                to_shed -= size;
            }
        }
    }

    (result, truncated)
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
