// Single-shot (non-agentic) query: one API call, no tool loop.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use crate::*;

/// Run a single (non-agentic) query – no tool loop, just one API call.
pub async fn run_single_query(
    client: &claurst_api::AnthropicClient,
    messages: Vec<Message>,
    config: &QueryConfig,
) -> Result<Message, ClaudeError> {
    let api_messages: Vec<ApiMessage> = messages.iter().map(ApiMessage::from).collect();
    let system = build_system_prompt(config);

    let request = CreateMessageRequest::builder(&config.model, config.max_tokens)
        .messages(api_messages)
        .system(system)
        .build();

    let handler: Arc<dyn StreamHandler> = Arc::new(claurst_api::streaming::NullStreamHandler);

    let mut rx = client.create_message_stream(request, handler).await?;
    let mut acc = StreamAccumulator::new();

    while let Some(evt) = rx.recv().await {
        acc.on_event(&evt);
        if matches!(evt, AnthropicStreamEvent::MessageStop) {
            break;
        }
    }

    let (msg, _usage, _stop) = acc.finish();
    Ok(msg)
}
