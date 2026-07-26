// providers/anthropic.rs — AnthropicProvider: wraps AnthropicClient in the
// unified LlmProvider trait.
//
// Phase 2A: create_message and create_message_stream are fully implemented by
// mapping ProviderRequest → CreateMessageRequest and mapping
// AnthropicStreamEvent → provider_types::StreamEvent.

use std::pin::Pin;
use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use claurst_core::provider_id::{ModelId, ProviderId};
use claurst_core::types::{ContentBlock, UsageInfo};
use futures::Stream;

use crate::client::{AnthropicClient, ClientConfig};
use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StopReason,
    StreamBlockAccumulator, StreamEvent, SystemPromptStyle,
};
use crate::streaming::{AnthropicStreamEvent, ContentDelta, NullStreamHandler};
use crate::types::{ApiMessage, ApiToolDefinition, CreateMessageRequest};

use super::message_normalization::normalize_anthropic_messages;

// ---------------------------------------------------------------------------
// AnthropicProvider
// ---------------------------------------------------------------------------

/// Wraps [`AnthropicClient`] so it can be held in a [`ProviderRegistry`] behind
/// `Arc<dyn LlmProvider>`.
pub struct AnthropicProvider {
    client: Arc<AnthropicClient>,
    id: ProviderId,
}

impl AnthropicProvider {
    /// Wrap an already-constructed (and Arc-wrapped) [`AnthropicClient`].
    pub fn new(client: Arc<AnthropicClient>) -> Self {
        Self {
            client,
            id: ProviderId::new(ProviderId::ANTHROPIC),
        }
    }

    /// Construct directly from a [`ClientConfig`], creating the inner client.
    pub fn from_config(config: ClientConfig) -> Self {
        let client = AnthropicClient::new(config)
            .expect("AnthropicProvider::from_config: failed to create AnthropicClient");
        Self {
            client: Arc::new(client),
            id: ProviderId::new(ProviderId::ANTHROPIC),
        }
    }

    /// Build a [`CreateMessageRequest`] from a [`ProviderRequest`].
    fn build_request(request: &ProviderRequest) -> CreateMessageRequest {
        let normalized_messages = normalize_anthropic_messages(&request.messages);
        let api_messages: Vec<ApiMessage> = normalized_messages
            .iter()
            .map(ApiMessage::from)
            .collect();

        let api_tools: Option<Vec<ApiToolDefinition>> = if request.tools.is_empty() {
            None
        } else {
            Some(request.tools.iter().map(ApiToolDefinition::from).collect())
        };

        let system = request.system_prompt.clone();

        let mut builder = CreateMessageRequest::builder(&request.model, request.max_tokens)
            .messages(api_messages);

        if let Some(sys) = system {
            builder = builder.system(sys);
        }
        if let Some(tools) = api_tools {
            builder = builder.tools(tools);
        }
        if let Some(t) = request.temperature {
            builder = builder.temperature(t as f32);
        }
        if let Some(p) = request.top_p {
            builder = builder.top_p(p as f32);
        }
        if let Some(k) = request.top_k {
            builder = builder.top_k(k);
        }
        if !request.stop_sequences.is_empty() {
            builder = builder.stop_sequences(request.stop_sequences.clone());
        }
        if let Some(tc) = request.thinking.clone() {
            builder = builder.thinking(tc);
        }

        builder.build()
    }

    /// Map a string stop_reason from Anthropic wire format to [`StopReason`].
    fn map_stop_reason(s: &str) -> StopReason {
        match s {
            "end_turn" => StopReason::EndTurn,
            "stop_sequence" => StopReason::StopSequence,
            "max_tokens" => StopReason::MaxTokens,
            "tool_use" => StopReason::ToolUse,
            other => StopReason::Other(other.to_string()),
        }
    }

    /// Map an [`AnthropicStreamEvent`] to the provider-agnostic [`StreamEvent`].
    fn map_stream_event(evt: AnthropicStreamEvent) -> Option<StreamEvent> {
        match evt {
            AnthropicStreamEvent::MessageStart { id, model, usage } => {
                Some(StreamEvent::MessageStart { id, model, usage })
            }
            AnthropicStreamEvent::ContentBlockStart { index, content_block } => {
                Some(StreamEvent::ContentBlockStart { index, content_block })
            }
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => match delta {
                ContentDelta::TextDelta { text } => {
                    Some(StreamEvent::TextDelta { index, text })
                }
                ContentDelta::ThinkingDelta { thinking } => {
                    Some(StreamEvent::ThinkingDelta { index, thinking })
                }
                ContentDelta::SignatureDelta { signature } => {
                    Some(StreamEvent::SignatureDelta { index, signature })
                }
                ContentDelta::InputJsonDelta { partial_json } => {
                    Some(StreamEvent::InputJsonDelta { index, partial_json })
                }
            },
            AnthropicStreamEvent::ContentBlockStop { index } => {
                Some(StreamEvent::ContentBlockStop { index })
            }
            AnthropicStreamEvent::MessageDelta { stop_reason, usage } => {
                let mapped_stop = stop_reason.as_deref().map(Self::map_stop_reason);
                Some(StreamEvent::MessageDelta {
                    stop_reason: mapped_stop,
                    usage,
                })
            }
            AnthropicStreamEvent::MessageStop => Some(StreamEvent::MessageStop),
            AnthropicStreamEvent::Error { error_type, message } => {
                Some(StreamEvent::Error { error_type, message })
            }
            AnthropicStreamEvent::Ping => None,
        }
    }
}

// ---------------------------------------------------------------------------
// LlmProvider impl
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "Anthropic"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        // Collect stream events to build a complete response.
        let mut stream = self.create_message_stream(request).await?;

        let mut id = String::from("unknown");
        let mut model = String::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = UsageInfo::default();

        // Accumulate every content block (text, thinking, tool_use, …) keyed by
        // its stream index. Captures thinking/signature/reasoning deltas (which
        // the previous `_ => {}` arm silently dropped) and preserves interleave
        // order via a single ordered pass. See issue #217.
        let mut blocks = StreamBlockAccumulator::new();

        use futures::StreamExt;
        while let Some(result) = stream.next().await {
            match result {
                Err(e) => return Err(e),
                Ok(evt) => {
                    // Content-block events feed the accumulator; message
                    // lifecycle events update the local id/model/usage/stop.
                    blocks.on_event(&evt);
                    match evt {
                        StreamEvent::MessageStart {
                            id: msg_id,
                            model: msg_model,
                            usage: msg_usage,
                        } => {
                            id = msg_id;
                            model = msg_model;
                            usage = msg_usage;
                        }
                        StreamEvent::MessageDelta {
                            stop_reason: sr,
                            usage: delta_usage,
                        } => {
                            if let Some(r) = sr {
                                stop_reason = r;
                            }
                            if let Some(u) = delta_usage {
                                usage.output_tokens += u.output_tokens;
                            }
                        }
                        StreamEvent::MessageStop => break,
                        StreamEvent::Error { error_type, message } => {
                            return Err(ProviderError::StreamError {
                                provider: self.id.clone(),
                                message: format!("[{}] {}", error_type, message),
                                partial_response: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }

        // Finalize every block in stream-index order — a single ordered pass
        // that preserves interleave (thinking → text → tool_use, …). No block is
        // appended out of band, so signed-thinking replay order is intact. #217.
        let final_content: Vec<ContentBlock> = blocks.finish();

        Ok(ProviderResponse {
            id,
            content: final_content,
            stop_reason,
            usage,
            model,
        })
    }

    // TODO(#228): `AnthropicProvider` is a thin adapter over the legacy
    // `AnthropicClient` (crate::client, in lib.rs): it delegates SSE decoding to
    // `client.create_message_stream` and then maps the Anthropic-typed
    // `AnthropicStreamEvent`s to `StreamEvent` via `map_stream_event`. These are
    // the two Anthropic stacks #228 wants collapsed under one `AnthropicMessages`
    // protocol (see the matching TODO on `AnthropicClient::process_sse_stream`).
    // Not done in this pass because `AnthropicClient` is a public type the TUI and
    // other crates depend on directly — deferring keeps everything green.
    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let api_request = Self::build_request(&request);
        let handler = Arc::new(NullStreamHandler);

        let provider_id = self.id.clone();

        let mut rx = self
            .client
            .create_message_stream(api_request, handler)
            .await
            .map_err(|e| ProviderError::Other {
                provider: provider_id.clone(),
                message: e.to_string(),
                status: None,
                body: None,
            })?;

        let s = stream! {
            while let Some(anthropic_evt) = rx.recv().await {
                if let Some(unified_evt) = AnthropicProvider::map_stream_event(anthropic_evt) {
                    yield Ok(unified_evt);
                }
            }
        };

        Ok(Box::pin(s))
    }

    /// Live model discovery via `GET /v1/models`, returning exactly the models
    /// the active credential can use. For a Claude Pro/Max OAuth token this is
    /// the curated subscription set (no legacy claude-3.x); for an API key it is
    /// the org's accessible models. On any failure (offline, expired/absent
    /// token) returns an empty vec so the picker keeps the models.dev catalog
    /// projection instead of surfacing an error.
    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let available = match self.client.fetch_available_models().await {
            Ok(m) => m,
            Err(_) => return Ok(Vec::new()),
        };
        let models = available
            .into_iter()
            .map(|m| {
                let name = m.display_name.unwrap_or_else(|| m.id.clone());
                ModelInfo {
                    id: ModelId::new(&m.id),
                    provider_id: self.id.clone(),
                    name,
                    // Sensible Claude defaults; the picker overlays richer
                    // context/cost from the models.dev catalog for known ids.
                    context_window: 200_000,
                    max_output_tokens: 64_000,
                    ..Default::default()
                }
            })
            .collect();
        Ok(models)
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        // Client was successfully constructed with a non-empty API key.
        Ok(ProviderStatus::Healthy)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            thinking: true,
            image_input: true,
            pdf_input: true,
            audio_input: false,
            video_input: false,
            caching: true,
            structured_output: true,
            system_prompt_style: SystemPromptStyle::TopLevel,
        }
    }
}
