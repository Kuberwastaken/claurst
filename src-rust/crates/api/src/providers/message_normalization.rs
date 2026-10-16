use claurst_core::types::{ContentBlock, Message, MessageContent};

pub(crate) fn remove_empty_messages(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(remove_empty_message)
        .collect()
}

pub(crate) fn normalize_anthropic_messages(messages: &[Message]) -> Vec<Message> {
    scrub_tool_ids(
        &remove_empty_messages(&drop_foreign_thinking(messages)),
        scrub_anthropic_tool_id,
    )
}

/// Anthropic's wire format rejects thinking replayed from other providers:
/// `redacted_thinking` ciphertext is server-side state only the issuing
/// Responses backend can decrypt (Codex -> Anthropic turns must not forward
/// it, or the API answers 400), and a thinking block without a file
/// signature is unverifiable. Codex and Copilot translate reason streams
/// into unsigned Thinking blocks, so signature emptiness is what marks a
/// block as this crate's internal provenance rather than trail evidence
/// Anthropic itself signed.
fn drop_foreign_thinking(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|message| {
            let MessageContent::Blocks(blocks) = &message.content else {
                return Some(message.clone());
            };
            let has_foreign = blocks.iter().any(|block| match block {
                ContentBlock::RedactedThinking { .. } => true,
                ContentBlock::Thinking { signature, .. } => signature.is_empty(),
                _ => false,
            });
            if !has_foreign {
                return Some(message.clone());
            }
            let kept: Vec<ContentBlock> = blocks
                .iter()
                .filter(|block| match block {
                    ContentBlock::RedactedThinking { .. } => false,
                    ContentBlock::Thinking { signature, .. } => !signature.is_empty(),
                    _ => true,
                })
                .cloned()
                .collect();
            (!kept.is_empty()).then(|| {
                let mut cloned = message.clone();
                cloned.content = MessageContent::Blocks(kept);
                cloned
            })
        })
        .collect()
}

pub(crate) fn scrub_tool_ids<F>(messages: &[Message], scrub: F) -> Vec<Message>
where
    F: Fn(&str) -> String + Copy,
{
    messages
        .iter()
        .cloned()
        .map(|mut message| {
            if let MessageContent::Blocks(blocks) = &mut message.content {
                for block in blocks.iter_mut() {
                    match block {
                        ContentBlock::ToolUse { id, .. } => {
                            *id = scrub(id);
                        }
                        ContentBlock::ToolResult { tool_use_id, .. } => {
                            *tool_use_id = scrub(tool_use_id);
                        }
                        _ => {}
                    }
                }
            }
            message
        })
        .collect()
}

pub(crate) fn scrub_anthropic_tool_id(id: &str) -> String {
    id.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn remove_empty_message(message: &Message) -> Option<Message> {
    match &message.content {
        MessageContent::Text(text) if text.is_empty() => None,
        MessageContent::Text(_) => Some(message.clone()),
        MessageContent::Blocks(blocks) => {
            let filtered: Vec<ContentBlock> =
                blocks.iter().filter_map(remove_empty_block).collect();
            if filtered.is_empty() {
                None
            } else {
                let mut cloned = message.clone();
                cloned.content = MessageContent::Blocks(filtered);
                Some(cloned)
            }
        }
    }
}

fn remove_empty_block(block: &ContentBlock) -> Option<ContentBlock> {
    match block {
        ContentBlock::Text { text } if text.is_empty() => None,
        ContentBlock::Thinking { thinking, .. } if thinking.is_empty() => None,
        _ => Some(block.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::types::{Message, Role, ToolResultContent};
    use serde_json::json;

    #[test]
    fn remove_empty_messages_filters_empty_text_and_thinking() {
        let messages = vec![
            Message::user(""),
            Message::assistant_blocks(vec![
                ContentBlock::Text {
                    text: String::new(),
                },
                ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: "sig".to_string(),
                },
            ]),
            Message::user_blocks(vec![
                ContentBlock::Text {
                    text: "kept".to_string(),
                },
                ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: "sig".to_string(),
                },
            ]),
        ];

        let normalized = remove_empty_messages(&messages);
        assert_eq!(normalized.len(), 1);
        assert!(matches!(&normalized[0].role, Role::User));
        let MessageContent::Blocks(blocks) = &normalized[0].content else {
            panic!("expected block message");
        };
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "kept"));
    }

    #[test]
    fn normalize_anthropic_messages_scrubs_tool_ids() {
        let messages = vec![
            Message::assistant_blocks(vec![ContentBlock::ToolUse {
                id: "call:1/abc".to_string(),
                name: "search".to_string(),
                input: json!({"q": "test"}),
                thought_signature: None,
            }]),
            Message::user_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call:1/abc".to_string(),
                content: ToolResultContent::Text("done".to_string()),
                is_error: Some(false),
            }]),
        ];

        let normalized = normalize_anthropic_messages(&messages);
        let MessageContent::Blocks(assistant_blocks) = &normalized[0].content else {
            panic!("expected assistant blocks");
        };
        let MessageContent::Blocks(user_blocks) = &normalized[1].content else {
            panic!("expected user blocks");
        };

        assert!(matches!(
            &assistant_blocks[0],
            ContentBlock::ToolUse { id, .. } if id == "call_1_abc"
        ));
        assert!(matches!(
            &user_blocks[0],
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_1_abc"
        ));
    }

    /// Provider-boundary filtering: redacted_thinking ciphertext and unsigned
    /// (foreign-produced) Thinking blocks must not reach an Anthropic-format
    /// endpoint — redacted ciphertext 400s, unsigned thinking is unverifiable.
    /// Signed thinking and other blocks survive, and a message whose blocks
    /// are all foreign is dropped entirely.
    #[test]
    fn normalize_anthropic_messages_drops_foreign_thinking() {
        use claurst_core::types::Role;

        let messages = vec![
            Message::assistant_blocks(vec![
                ContentBlock::Thinking {
                    thinking: "codex replay".to_string(),
                    signature: String::new(),
                },
                ContentBlock::RedactedThinking {
                    data: "gAAAAA".to_string(),
                },
                ContentBlock::Thinking {
                    thinking: "native".to_string(),
                    signature: "sig-anthropic".to_string(),
                },
                ContentBlock::Text {
                    text: "answer".to_string(),
                },
            ]),
            Message::user("question"),
            Message::assistant_blocks(vec![ContentBlock::RedactedThinking {
                data: "only-foreign".to_string(),
            }]),
        ];

        let normalized = normalize_anthropic_messages(&messages);
        assert_eq!(normalized.len(), 2);
        assert!(matches!(&normalized[1].role, Role::User));

        let MessageContent::Blocks(blocks) = &normalized[0].content else {
            panic!("expected block message");
        };
        assert_eq!(blocks.len(), 2);
        assert!(matches!(
            &blocks[0],
            ContentBlock::Thinking { signature, .. } if signature == "sig-anthropic"
        ));
        assert!(matches!(&blocks[1], ContentBlock::Text { text } if text == "answer"));
    }
}
