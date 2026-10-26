use claurst_core::types::{ContentBlock, Message, MessageContent};

pub(crate) fn remove_empty_messages(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(remove_empty_message)
        .collect()
}

pub fn normalize_anthropic_messages(messages: &[Message]) -> Vec<Message> {
    scrub_tool_ids(
        &remove_empty_messages(&drop_foreign_thinking(messages)),
        scrub_anthropic_tool_id,
    )
}

/// Native Anthropic signed and redacted blocks remain untouched. Responses
/// reasoning has an explicit backend origin and cannot cross this boundary;
/// unsigned legacy thinking is also unverifiable to Anthropic.
fn drop_foreign_thinking(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|message| {
            let MessageContent::Blocks(blocks) = &message.content else {
                return Some(message.clone());
            };
            let has_foreign = blocks.iter().any(|block| match block {
                ContentBlock::ResponsesReasoning { .. } => true,
                ContentBlock::Thinking { signature, .. } => signature.is_empty(),
                _ => false,
            });
            if !has_foreign {
                return Some(message.clone());
            }
            let kept: Vec<ContentBlock> = blocks
                .iter()
                .filter(|block| match block {
                    ContentBlock::ResponsesReasoning { .. } => false,
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

    /// Provider-boundary filtering: Responses reasoning and unsigned
    /// legacy Thinking blocks must not reach an Anthropic-format
    /// endpoint. Native signed and redacted Anthropic blocks survive.
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
                ContentBlock::ResponsesReasoning {
                    provider: claurst_core::types::ResponsesReasoningProvider::Codex,
                    item_id: Some("rs_foreign".into()),
                    summary: "foreign".into(),
                    encrypted_content: Some("gAAAAA".into()),
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
            Message::assistant_blocks(vec![ContentBlock::ResponsesReasoning {
                provider: claurst_core::types::ResponsesReasoningProvider::Copilot,
                item_id: None,
                summary: String::new(),
                encrypted_content: Some("only-foreign".into()),
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
    #[test]
    fn native_anthropic_redacted_thinking_survives_normalization() {
        let messages = vec![Message::assistant_blocks(vec![
            ContentBlock::RedactedThinking {
                data: "native-opaque-state".into(),
            },
            ContentBlock::Thinking {
                thinking: "native summary".into(),
                signature: "native-signature".into(),
            },
            ContentBlock::Text {
                text: "answer".into(),
            },
        ])];
        let normalized = normalize_anthropic_messages(&messages);
        assert_eq!(
            serde_json::to_value(&normalized).unwrap(),
            serde_json::to_value(&messages).unwrap()
        );
    }
}
