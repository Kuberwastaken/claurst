//! Message-history invariant pass (issue #229 / MI-2).
//!
//! Several code paths — auto/reactive compaction, `max_tokens` recovery, and
//! command-queue / pending-message injection — can each independently mutate the
//! conversation into a shape that violates the provider API's structural rules.
//! The most damaging violation is a broken `tool_use` ↔ `tool_result` pairing:
//!
//!   * an orphan `tool_result` block whose originating `tool_use` was sliced away
//!     by a compaction cut, or
//!   * a dangling `tool_use` whose answering `tool_result` never arrived (the
//!     turn was interrupted, or a plain user message was injected before it could
//!     be answered).
//!
//! Anthropic and OpenAI both reject such histories with HTTP 400. There is no
//! single choke point in the pipeline that guarantees the invariants — this
//! module is that choke point. [`sanitize_history`] is a pure function over the
//! `Vec<Message>` that is about to be dispatched; the query loop runs it at the
//! request boundary so a malformed history can never reach the model, regardless
//! of which path produced it.
//!
//! This is a *safety net*: it does not (and must not) change compaction /
//! recovery / command-queue logic. It only repairs whatever those paths hand it.

use claurst_core::types::{ContentBlock, Message, MessageContent, Role, ToolResultContent};

/// Content used for a synthesized placeholder `tool_result` that answers a
/// dangling `tool_use`. Marked `is_error` so the model can tell it apart from a
/// genuine result.
const UNAVAILABLE_RESULT_MSG: &str = "[tool result unavailable]";

/// Enforce the provider-API message invariants on `messages`, returning a
/// repaired copy with balanced `tool_use` ↔ `tool_result` pairing.
///
/// Invariants enforced:
///
/// 1. **Pairing.** Every `tool_result` block must answer a `tool_use` (matched
///    by id) in the *immediately preceding* assistant message; orphan
///    `tool_result` blocks (whose `tool_use` is gone) are dropped. Every
///    `tool_use` in an assistant message must be answered by a `tool_result` in
///    the *immediately following* user message; for a **dangling** `tool_use`
///    (no matching result) a placeholder `tool_result` is **synthesized** rather
///    than dropping the `tool_use`. Dropping a `tool_use` risks desyncing the
///    turn (the assistant "said" it called a tool); synthesizing a placeholder
///    keeps the pairing balanced without rewriting the assistant's output.
/// 2. **No empty messages.** A message whose block list becomes empty after
///    orphan removal is dropped.
/// 3. **Order preserved.** Real turns are neither reordered nor merged; a
///    non-`tool_result` first message is preserved intact. Synthesized results
///    are only ever inserted directly after the assistant `tool_use` they answer.
///
/// The function is idempotent: a well-formed history passes through unchanged.
pub fn sanitize_history(messages: Vec<Message>) -> Vec<Message> {
    let n = messages.len();
    let mut out: Vec<Message> = Vec::with_capacity(n);
    let mut i = 0usize;

    while i < n {
        let msg = &messages[i];

        match msg.role {
            Role::Assistant => {
                let tool_use_ids = collect_tool_use_ids(msg);
                out.push(msg.clone());

                if tool_use_ids.is_empty() {
                    i += 1;
                    continue;
                }

                // The tool_use blocks in this assistant message MUST be answered
                // by tool_result blocks in the immediately following user message.
                let next_is_user = i + 1 < n && messages[i + 1].role == Role::User;

                if next_is_user {
                    // Merge (clean orphans + synthesize missing) into the
                    // existing following user message. This keeps a single
                    // answering user turn and avoids inserting a redundant one.
                    let answered = answer_user_message(&messages[i + 1], &tool_use_ids);
                    out.push(answered);
                    i += 2; // both this assistant and its answering user consumed
                } else {
                    // No user message follows (end of history, or an assistant
                    // message follows — itself already malformed). Insert a fresh
                    // user message carrying a synthesized result for every
                    // tool_use so the pairing is balanced.
                    let synth: Vec<ContentBlock> =
                        tool_use_ids.iter().map(|id| synth_tool_result(id)).collect();
                    out.push(Message::user_blocks(synth));
                    i += 1;
                }
            }
            Role::User => {
                // A user message reaching this arm is NOT the immediate answer to
                // an assistant `tool_use` (those are consumed in the Assistant
                // arm above). Therefore ANY `tool_result` block here is an orphan
                // — its `tool_use` is absent or non-adjacent — and must be dropped.
                if let MessageContent::Blocks(blocks) = &msg.content {
                    if blocks
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
                    {
                        let kept: Vec<ContentBlock> = blocks
                            .iter()
                            .filter(|b| !matches!(b, ContentBlock::ToolResult { .. }))
                            .cloned()
                            .collect();
                        // Invariant 2: drop messages emptied by block removal.
                        if !kept.is_empty() {
                            let mut m = msg.clone();
                            m.content = MessageContent::Blocks(kept);
                            out.push(m);
                        }
                        i += 1;
                        continue;
                    }
                }
                // No tool_result blocks — pass through unchanged (Text messages,
                // the first user task, injected commands, etc.).
                out.push(msg.clone());
                i += 1;
            }
        }
    }

    out
}

/// Build the user message that answers `tool_use_ids`, starting from the
/// existing `following` user message: drop any `tool_result` whose id is not in
/// `tool_use_ids` (orphans), keep every other block, then append a synthesized
/// placeholder for each id that is still unanswered (dangling).
///
/// A `Text` user message is promoted to blocks so the synthesized results sit
/// alongside the preserved text — this answers the `tool_use` without inserting
/// an extra user turn (which would break role alternation).
fn answer_user_message(following: &Message, tool_use_ids: &[String]) -> Message {
    let mut kept: Vec<ContentBlock> = match &following.content {
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    tool_use_ids.iter().any(|id| id == tool_use_id)
                }
                _ => true,
            })
            .cloned()
            .collect(),
        MessageContent::Text(t) => {
            if t.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text { text: t.clone() }]
            }
        }
    };

    // Which ids are already answered by a surviving tool_result?
    let answered: Vec<String> = kept
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect();

    // Synthesize a placeholder for every dangling (unanswered) tool_use.
    for id in tool_use_ids {
        if !answered.iter().any(|a| a == id) {
            kept.push(synth_tool_result(id));
        }
    }

    let mut answered_msg = following.clone();
    answered_msg.content = MessageContent::Blocks(kept);
    answered_msg
}

/// Collect the ids of every `tool_use` block in `msg`, in order.
fn collect_tool_use_ids(msg: &Message) -> Vec<String> {
    match &msg.content {
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect(),
        MessageContent::Text(_) => Vec::new(),
    }
}

/// Build a synthesized placeholder `tool_result` for the given `tool_use` id.
fn synth_tool_result(tool_use_id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: ToolResultContent::Text(UNAVAILABLE_RESULT_MSG.to_string()),
        is_error: Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers ---------------------------------------------------------

    fn tool_use(id: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_string(),
            name: "Read".to_string(),
            input: serde_json::json!({ "path": "/tmp/x" }),
            thought_signature: None,
        }
    }

    fn tool_result(id: &str, text: &str) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: ToolResultContent::Text(text.to_string()),
            is_error: None,
        }
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text {
            text: text.to_string(),
        }
    }

    /// Collect (in order) every tool_use id and every tool_result id in a
    /// history, so tests can assert exact pairing.
    fn pairing(messages: &[Message]) -> (Vec<String>, Vec<String>) {
        let mut uses = Vec::new();
        let mut results = Vec::new();
        for m in messages {
            if let MessageContent::Blocks(blocks) = &m.content {
                for b in blocks {
                    match b {
                        ContentBlock::ToolUse { id, .. } => uses.push(id.clone()),
                        ContentBlock::ToolResult { tool_use_id, .. } => {
                            results.push(tool_use_id.clone())
                        }
                        _ => {}
                    }
                }
            }
        }
        (uses, results)
    }

    /// Assert the API's core invariant: every tool_use is answered by a
    /// tool_result in the *immediately following* user message, and every
    /// tool_result answers a tool_use in the *immediately preceding* assistant
    /// message. Returns nothing; panics with context on violation.
    fn assert_balanced(messages: &[Message]) {
        for (i, m) in messages.iter().enumerate() {
            let uses = collect_tool_use_ids(m);
            if m.role == Role::Assistant && !uses.is_empty() {
                let next = messages
                    .get(i + 1)
                    .unwrap_or_else(|| panic!("tool_use at {i} has no following message"));
                assert_eq!(next.role, Role::User, "tool_use at {i} not followed by user");
                let answered: Vec<String> = collect_tool_result_ids(next);
                for id in &uses {
                    assert!(
                        answered.contains(id),
                        "tool_use {id} at msg {i} is unanswered"
                    );
                }
            }
            if m.role == Role::User {
                let prev_uses = i
                    .checked_sub(1)
                    .map(|p| collect_tool_use_ids(&messages[p]))
                    .unwrap_or_default();
                for id in collect_tool_result_ids(m) {
                    assert!(
                        prev_uses.contains(&id),
                        "tool_result {id} at msg {i} has no preceding tool_use"
                    );
                }
            }
        }
    }

    fn collect_tool_result_ids(msg: &Message) -> Vec<String> {
        match &msg.content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    // ---- tests -----------------------------------------------------------

    /// A well-formed history — user task, assistant tool_use, answering user
    /// tool_result, final assistant text — must pass through UNCHANGED.
    #[test]
    fn wellformed_history_unchanged() {
        let messages = vec![
            Message::user("do the thing"),
            Message::assistant_blocks(vec![text_block("calling"), tool_use("t1")]),
            Message::user_blocks(vec![tool_result("t1", "done")]),
            Message::assistant("all set"),
        ];

        let out = sanitize_history(messages.clone());

        assert_eq!(out.len(), messages.len(), "no messages added or dropped");
        assert_eq!(pairing(&out), (vec!["t1".to_string()], vec!["t1".to_string()]));
        assert_balanced(&out);
        // Idempotent: running twice yields the same result.
        assert_eq!(pairing(&sanitize_history(out.clone())), pairing(&out));
    }

    /// An orphan tool_result (its originating tool_use was sliced away by a
    /// compaction cut) must be dropped. This is the `snip_compact` /
    /// `calculate_messages_to_keep_index` failure mode: the tail begins with a
    /// user tool_result whose assistant tool_use is gone.
    #[test]
    fn orphan_tool_result_is_dropped() {
        let messages = vec![
            // Compaction kept messages[0] (system/first) then sliced the tail,
            // stranding this tool_result whose tool_use no longer exists.
            Message::user("original task"),
            Message::user_blocks(vec![tool_result("gone", "orphaned output"), text_block("and more")]),
            Message::assistant("continuing"),
        ];

        let out = sanitize_history(messages);

        let (uses, results) = pairing(&out);
        assert!(uses.is_empty(), "no tool_use present");
        assert!(results.is_empty(), "orphan tool_result must be dropped");
        assert_balanced(&out);
        // The non-tool_result text in that message survives; nothing empty dropped.
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].get_text(), Some("original task"));
    }

    /// A user message that becomes EMPTY after orphan removal (it held only an
    /// orphan tool_result) is dropped entirely.
    #[test]
    fn message_emptied_by_removal_is_dropped() {
        let messages = vec![
            Message::user("task"),
            Message::user_blocks(vec![tool_result("gone", "orphan")]),
            Message::assistant("ok"),
        ];

        let out = sanitize_history(messages);

        assert_eq!(out.len(), 2, "the emptied user message is dropped");
        assert_eq!(out[0].get_text(), Some("task"));
        assert_eq!(out[1].get_text(), Some("ok"));
        assert_balanced(&out);
    }

    /// A dangling tool_use at the END of the history (turn interrupted by a
    /// max_tokens cut or cancellation before the result was appended) gets a
    /// SYNTHESIZED placeholder result rather than being dropped.
    #[test]
    fn dangling_tool_use_at_end_is_synthesized() {
        let messages = vec![
            Message::user("task"),
            Message::assistant_blocks(vec![tool_use("t1")]),
        ];

        let out = sanitize_history(messages);

        assert_eq!(out.len(), 3, "a synthesized answering user message is appended");
        assert_eq!(pairing(&out), (vec!["t1".to_string()], vec!["t1".to_string()]));
        assert_balanced(&out);
        // The synthesized result is flagged is_error with the placeholder text.
        if let MessageContent::Blocks(blocks) = &out[2].content {
            match &blocks[0] {
                ContentBlock::ToolResult { content, is_error, .. } => {
                    assert_eq!(*is_error, Some(true));
                    match content {
                        ToolResultContent::Text(t) => assert_eq!(t, UNAVAILABLE_RESULT_MSG),
                        _ => panic!("expected text content"),
                    }
                }
                other => panic!("expected synthesized tool_result, got {other:?}"),
            }
        } else {
            panic!("expected blocks");
        }
    }

    /// Command-queue / pending injection can drop a plain user text message in
    /// right after an assistant tool_use, before it was answered. The synthesized
    /// result must be MERGED into that user message (preserving the injected
    /// text) so we do not create two consecutive user turns.
    #[test]
    fn dangling_tool_use_before_injected_user_text_is_merged() {
        let messages = vec![
            Message::user("task"),
            Message::assistant_blocks(vec![tool_use("t1")]),
            Message::user("newly injected command"),
        ];

        let out = sanitize_history(messages);

        assert_eq!(out.len(), 3, "no extra turn inserted");
        assert_eq!(pairing(&out), (vec!["t1".to_string()], vec!["t1".to_string()]));
        assert_balanced(&out);
        // The injected text is preserved alongside the synthesized result.
        if let MessageContent::Blocks(blocks) = &out[2].content {
            assert!(blocks.iter().any(|b| matches!(
                b,
                ContentBlock::Text { text } if text == "newly injected command"
            )));
            assert!(blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. })));
        } else {
            panic!("expected user blocks");
        }
    }

    /// A partially-answered multi-tool turn: two tool_use blocks but only one
    /// result survived. The missing one is synthesized; the present one is kept
    /// untouched. (max_tokens recovery / partial cancellation shape.)
    #[test]
    fn partial_multi_tool_turn_synthesizes_only_the_missing() {
        let messages = vec![
            Message::user("task"),
            Message::assistant_blocks(vec![tool_use("t1"), tool_use("t2")]),
            Message::user_blocks(vec![tool_result("t1", "real output")]),
        ];

        let out = sanitize_history(messages);

        assert_eq!(pairing(&out).0, vec!["t1".to_string(), "t2".to_string()]);
        assert_balanced(&out);
        // t1 keeps its real result; t2 gets the placeholder.
        if let MessageContent::Blocks(blocks) = &out[2].content {
            let t1 = blocks.iter().find_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, content, .. } if tool_use_id == "t1" => {
                    Some(content)
                }
                _ => None,
            });
            match t1 {
                Some(ToolResultContent::Text(t)) => assert_eq!(t, "real output"),
                _ => panic!("t1's real result must be preserved"),
            }
        } else {
            panic!("expected blocks");
        }
    }

    /// A user message carrying a MIX of a valid result and an orphan result:
    /// the orphan (not requested by the preceding assistant) is dropped, the
    /// valid one is kept.
    #[test]
    fn mixed_valid_and_orphan_results_in_answer() {
        let messages = vec![
            Message::user("task"),
            Message::assistant_blocks(vec![tool_use("t1")]),
            Message::user_blocks(vec![
                tool_result("t1", "good"),
                tool_result("stale", "orphan from an earlier sliced turn"),
            ]),
        ];

        let out = sanitize_history(messages);

        assert_eq!(pairing(&out), (vec!["t1".to_string()], vec!["t1".to_string()]));
        assert_balanced(&out);
    }

    /// The system / first message is preserved intact and turns are not
    /// reordered.
    #[test]
    fn first_message_preserved() {
        let messages = vec![
            Message::user("FIRST — the task"),
            Message::assistant_blocks(vec![tool_use("t1")]),
            Message::user_blocks(vec![tool_result("t1", "done")]),
        ];

        let out = sanitize_history(messages);

        assert_eq!(out[0].role, Role::User);
        assert_eq!(out[0].get_text(), Some("FIRST — the task"));
        assert_balanced(&out);
    }

    /// An empty history is a no-op.
    #[test]
    fn empty_history_is_noop() {
        assert!(sanitize_history(Vec::new()).is_empty());
    }
}
