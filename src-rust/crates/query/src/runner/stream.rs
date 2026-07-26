// Provider stream-event mapping to the Anthropic event shape.
// Extracted from lib.rs (issue #232). Behavior-preserving move.

/// Map a unified `StreamEvent` (from a non-Anthropic provider) onto the
/// equivalent `AnthropicStreamEvent` so that the TUI stream consumer sees a
/// single, consistent event type regardless of which provider produced it.
pub(crate) fn map_to_anthropic_event(
    evt: &claurst_api::StreamEvent,
) -> Option<claurst_api::AnthropicStreamEvent> {
    use claurst_api::streaming::{AnthropicStreamEvent, ContentDelta};
    use claurst_api::StreamEvent;

    match evt {
        StreamEvent::MessageStart { id, model, usage } => {
            Some(AnthropicStreamEvent::MessageStart {
                id: id.clone(),
                model: model.clone(),
                usage: usage.clone(),
            })
        }
        StreamEvent::ContentBlockStart { index, content_block } => {
            Some(AnthropicStreamEvent::ContentBlockStart {
                index: *index,
                content_block: content_block.clone(),
            })
        }
        StreamEvent::TextDelta { index, text } => {
            Some(AnthropicStreamEvent::ContentBlockDelta {
                index: *index,
                delta: ContentDelta::TextDelta { text: text.clone() },
            })
        }
        StreamEvent::ThinkingDelta { index, thinking } => {
            Some(AnthropicStreamEvent::ContentBlockDelta {
                index: *index,
                delta: ContentDelta::ThinkingDelta { thinking: thinking.clone() },
            })
        }
        StreamEvent::ReasoningDelta { index, reasoning } => {
            Some(AnthropicStreamEvent::ContentBlockDelta {
                index: *index,
                delta: ContentDelta::ThinkingDelta { thinking: reasoning.clone() },
            })
        }
        StreamEvent::InputJsonDelta { index, partial_json } => {
            Some(AnthropicStreamEvent::ContentBlockDelta {
                index: *index,
                delta: ContentDelta::InputJsonDelta { partial_json: partial_json.clone() },
            })
        }
        StreamEvent::SignatureDelta { index, signature } => {
            Some(AnthropicStreamEvent::ContentBlockDelta {
                index: *index,
                delta: ContentDelta::SignatureDelta { signature: signature.clone() },
            })
        }
        StreamEvent::ContentBlockStop { index } => {
            Some(AnthropicStreamEvent::ContentBlockStop { index: *index })
        }
        StreamEvent::MessageDelta { stop_reason, usage } => {
            // Convert the unified StopReason to the string form used by
            // AnthropicStreamEvent::MessageDelta.
            let stop_reason_str = stop_reason.as_ref().map(|r| match r {
                claurst_api::provider_types::StopReason::ToolUse => "tool_use".to_string(),
                claurst_api::provider_types::StopReason::MaxTokens => "max_tokens".to_string(),
                claurst_api::provider_types::StopReason::StopSequence => "stop_sequence".to_string(),
                claurst_api::provider_types::StopReason::EndTurn => "end_turn".to_string(),
                claurst_api::provider_types::StopReason::ContentFiltered => "content_filtered".to_string(),
                claurst_api::provider_types::StopReason::Other(s) => s.clone(),
            });
            Some(AnthropicStreamEvent::MessageDelta {
                stop_reason: stop_reason_str,
                usage: usage.clone(),
            })
        }
        StreamEvent::MessageStop => Some(AnthropicStreamEvent::MessageStop),
        StreamEvent::Error { error_type, message } => {
            Some(AnthropicStreamEvent::Error {
                error_type: error_type.clone(),
                message: message.clone(),
            })
        }
    }
}
