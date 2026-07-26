// provider_types.rs — Unified request/response types shared across all
// provider implementations.
//
// These types form a provider-agnostic layer that every concrete provider
// adapter (Anthropic, OpenAI, Google, …) maps to/from.

use claurst_core::types::{ContentBlock, Message, ToolDefinition, UsageInfo};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// Re-export ThinkingConfig and SystemPrompt from the api types module so
// callers only need to import from this module.
pub use crate::types::{ThinkingConfig, SystemPrompt};

// ---------------------------------------------------------------------------
// StopReason
// ---------------------------------------------------------------------------

/// The reason a model stopped generating tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum StopReason {
    /// The model reached a natural stopping point.
    #[default]
    EndTurn,
    /// The model generated a stop sequence.
    StopSequence,
    /// The model hit the max_tokens limit.
    MaxTokens,
    /// The model made a tool/function call.
    ToolUse,
    /// Content was filtered by the provider's safety system.
    ContentFiltered,
    /// The provider returned an unknown or unrecognised stop reason.
    Other(String),
}


// ---------------------------------------------------------------------------
// ProviderRequest
// ---------------------------------------------------------------------------

/// A normalised request that any provider adapter can consume.
///
/// Provider-specific parameters that cannot be expressed through the common
/// fields can be passed via `provider_options` as an arbitrary JSON object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRequest {
    /// The model identifier (e.g. `"claude-opus-4-5"`, `"gpt-4o"`).
    pub model: String,

    /// The conversation history to send to the model.
    pub messages: Vec<Message>,

    /// An optional system / developer prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<SystemPrompt>,

    /// Tool definitions available to the model for this turn.
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,

    /// Maximum number of tokens to generate.
    pub max_tokens: u32,

    /// Sampling temperature (provider-dependent range, typically 0.0–1.0 or 0.0–2.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,

    /// Nucleus sampling probability mass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,

    /// Top-k sampling cutoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,

    /// Sequences that cause the model to stop generating.
    #[serde(default)]
    pub stop_sequences: Vec<String>,

    /// Extended thinking / chain-of-thought configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,

    /// Arbitrary provider-specific options merged into the request body.
    /// Defaults to an empty JSON object `{}`.
    #[serde(default)]
    pub provider_options: Value,
}

// ---------------------------------------------------------------------------
// ProviderResponse
// ---------------------------------------------------------------------------

/// A normalised response returned by any provider adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderResponse {
    /// Provider-assigned message / request identifier.
    pub id: String,

    /// The generated content blocks.
    pub content: Vec<ContentBlock>,

    /// Why the model stopped generating.
    pub stop_reason: StopReason,

    /// Token usage for billing / budget tracking.
    pub usage: UsageInfo,

    /// The model that produced this response (as reported by the provider).
    pub model: String,
}

// ---------------------------------------------------------------------------
// StreamEvent
// ---------------------------------------------------------------------------

/// Events emitted by the provider-agnostic streaming layer.
///
/// Each provider's SSE/websocket parser maps its wire format onto these events
/// so that the rest of the application can consume a single unified stream.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// The message has started; carries the provider-assigned id and model.
    MessageStart {
        id: String,
        model: String,
        usage: UsageInfo,
    },

    /// A new content block is beginning.
    ContentBlockStart {
        index: usize,
        content_block: ContentBlock,
    },

    /// Incremental text delta for an in-progress block.
    TextDelta {
        index: usize,
        text: String,
    },

    /// Incremental thinking / reasoning delta.
    ThinkingDelta {
        index: usize,
        thinking: String,
    },

    /// Incremental delta for tool-call JSON arguments.
    InputJsonDelta {
        index: usize,
        partial_json: String,
    },

    /// Incremental delta for a cryptographic signature block.
    SignatureDelta {
        index: usize,
        signature: String,
    },

    /// An in-progress content block is now complete.
    ContentBlockStop {
        index: usize,
    },

    /// Final message-level delta carrying the stop reason and updated usage.
    MessageDelta {
        stop_reason: Option<StopReason>,
        usage: Option<UsageInfo>,
    },

    /// The message stream is fully complete.
    MessageStop,

    /// A provider-level error occurred mid-stream.
    Error {
        error_type: String,
        message: String,
    },

    /// Incremental reasoning / scratchpad delta (alias used by some providers).
    ReasoningDelta {
        index: usize,
        reasoning: String,
    },
}

// ---------------------------------------------------------------------------
// StreamBlockAccumulator
// ---------------------------------------------------------------------------

/// Partial content-block state accumulated while draining a `StreamEvent`
/// stream. Mirrors the streaming `StreamAccumulator` in `lib.rs`, but over the
/// provider-level [`StreamEvent`]/[`ContentBlock`] types.
enum PartialBlock {
    Text(String),
    Thinking {
        thinking_buf: String,
        signature_buf: String,
    },
    ToolUse {
        id: String,
        name: String,
        json_buf: String,
        /// Opaque provider signature (e.g. Gemini's `thoughtSignature`) captured
        /// on `ContentBlockStart`; echoed back on the finalized block (#311).
        thought_signature: Option<String>,
    },
    /// Any block that arrives fully-formed on `ContentBlockStart` and needs no
    /// delta accumulation (e.g. `RedactedThinking`, images).
    Passthrough(ContentBlock),
}

impl PartialBlock {
    fn empty_thinking() -> Self {
        PartialBlock::Thinking {
            thinking_buf: String::new(),
            signature_buf: String::new(),
        }
    }

    fn from_start(block: ContentBlock) -> Self {
        match block {
            ContentBlock::Text { text } => PartialBlock::Text(text),
            ContentBlock::Thinking { thinking, signature } => PartialBlock::Thinking {
                thinking_buf: thinking,
                signature_buf: signature,
            },
            ContentBlock::ToolUse { id, name, thought_signature, .. } => PartialBlock::ToolUse {
                id,
                name,
                json_buf: String::new(),
                thought_signature,
            },
            other => PartialBlock::Passthrough(other),
        }
    }

    fn finish(self) -> ContentBlock {
        match self {
            PartialBlock::Text(text) => ContentBlock::Text { text },
            PartialBlock::Thinking {
                thinking_buf,
                signature_buf,
            } => ContentBlock::Thinking {
                thinking: thinking_buf,
                signature: signature_buf,
            },
            PartialBlock::ToolUse { id, name, json_buf, thought_signature } => {
                let input =
                    serde_json::from_str(&json_buf).unwrap_or(Value::Object(Default::default()));
                ContentBlock::ToolUse { id, name, input, thought_signature }
            }
            PartialBlock::Passthrough(block) => block,
        }
    }
}

/// Aggregates provider-level [`StreamEvent`] content-block events into ordered,
/// finalized [`ContentBlock`]s. Shared by the non-streaming `create_message`
/// aggregators (Anthropic, MiniMax, …) so that `ThinkingDelta` / `SignatureDelta`
/// / `ReasoningDelta` are captured into their block (rather than dropped) and
/// every block — text, thinking, tool_use, and anything else — keeps the stream
/// index it was emitted at. `finish()` is a single index-ordered pass, so the
/// model's interleave order (thinking-first / signed-thinking replay) survives.
/// See issue #217.
#[derive(Default)]
pub(crate) struct StreamBlockAccumulator {
    partials: std::collections::BTreeMap<usize, PartialBlock>,
}

impl StreamBlockAccumulator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed one `StreamEvent`. Only content-block events mutate state; message
    /// lifecycle events (`MessageStart`, `MessageDelta`, `MessageStop`,
    /// `ContentBlockStop`, `Error`) are ignored so callers still handle those
    /// (id / model / usage / stop_reason) themselves.
    pub(crate) fn on_event(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                self.partials
                    .insert(*index, PartialBlock::from_start(content_block.clone()));
            }
            StreamEvent::TextDelta { index, text } => {
                if let Some(PartialBlock::Text(buf)) = self.partials.get_mut(index) {
                    buf.push_str(text);
                }
            }
            StreamEvent::ThinkingDelta { index, thinking } => {
                if let PartialBlock::Thinking { thinking_buf, .. } = self
                    .partials
                    .entry(*index)
                    .or_insert_with(PartialBlock::empty_thinking)
                {
                    thinking_buf.push_str(thinking);
                }
            }
            StreamEvent::ReasoningDelta { index, reasoning } => {
                // `ReasoningDelta` is an alias for thinking text used by some
                // providers; fold it into the same thinking block.
                if let PartialBlock::Thinking { thinking_buf, .. } = self
                    .partials
                    .entry(*index)
                    .or_insert_with(PartialBlock::empty_thinking)
                {
                    thinking_buf.push_str(reasoning);
                }
            }
            StreamEvent::SignatureDelta { index, signature } => {
                if let PartialBlock::Thinking { signature_buf, .. } = self
                    .partials
                    .entry(*index)
                    .or_insert_with(PartialBlock::empty_thinking)
                {
                    signature_buf.push_str(signature);
                }
            }
            StreamEvent::InputJsonDelta {
                index,
                partial_json,
            } => {
                if let Some(PartialBlock::ToolUse { json_buf, .. }) = self.partials.get_mut(index) {
                    json_buf.push_str(partial_json);
                }
            }
            _ => {}
        }
    }

    /// Consume the accumulator, producing finalized content blocks in the
    /// stream-index order the model emitted them.
    pub(crate) fn finish(self) -> Vec<ContentBlock> {
        self.partials
            .into_values()
            .map(PartialBlock::finish)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// ProviderCapabilities
// ---------------------------------------------------------------------------

/// Describes the features supported by a particular provider/model combination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    /// Provider supports streaming responses via SSE or websocket.
    pub streaming: bool,

    /// Provider supports function / tool calling.
    pub tool_calling: bool,

    /// Provider supports extended thinking / chain-of-thought tokens.
    pub thinking: bool,

    /// Provider accepts image inputs.
    pub image_input: bool,

    /// Provider accepts PDF document inputs.
    pub pdf_input: bool,

    /// Provider accepts audio inputs.
    pub audio_input: bool,

    /// Provider accepts video inputs.
    pub video_input: bool,

    /// Provider supports prompt caching.
    pub caching: bool,

    /// Provider supports JSON-schema-constrained structured output.
    pub structured_output: bool,

    /// How the provider expects the system prompt to be delivered.
    pub system_prompt_style: SystemPromptStyle,
}

/// Describes where/how a provider expects the system prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemPromptStyle {
    /// Delivered as a top-level `system` field in the request body (Anthropic style).
    TopLevel,
    /// Delivered as a `{"role": "system", "content": "…"}` message at index 0 (OpenAI style).
    SystemMessage,
    /// Delivered as a `system_instruction` field (Google Gemini style).
    SystemInstruction,
}

// ---------------------------------------------------------------------------
// ProviderStatus
// ---------------------------------------------------------------------------

/// The current health status of a provider endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ProviderStatus {
    /// The provider is operating normally.
    Healthy,
    /// The provider is reachable but experiencing elevated errors or latency.
    Degraded { reason: String },
    /// The provider is unreachable or has been disabled.
    Unavailable { reason: String },
}

// ---------------------------------------------------------------------------
// AuthMethod
// ---------------------------------------------------------------------------

/// The authentication mechanism used to talk to a provider endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AuthMethod {
    /// A static API key sent as an HTTP header.
    ApiKey {
        key: String,
        header: ApiKeyHeader,
    },

    /// A bearer token sent in the `Authorization` header.
    Bearer {
        token: String,
    },

    /// AWS Signature V4 credentials for Amazon Bedrock.
    AwsCredentials {
        #[serde(skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        region: String,
        /// Optional bearer token for cross-account or SSO scenarios.
        #[serde(skip_serializing_if = "Option::is_none")]
        bearer_token: Option<String>,
    },

    /// OAuth 2.0 access + refresh token pair.
    OAuth {
        access_token: String,
        refresh_token: String,
        /// Unix timestamp (seconds) when the access token expires.
        expires_at: u64,
    },

    /// No authentication required (e.g. local Ollama).
    None,
}

/// Which HTTP header carries the API key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyHeader {
    /// `x-api-key: <key>` (Anthropic, Mistral, …)
    XApiKey,
    /// `Authorization: Bearer <key>` (OpenAI, Groq, …)
    Authorization,
    /// A custom header name.
    Custom(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for issue #217. The non-streaming aggregators used by
    /// the Anthropic and MiniMax providers delegate to `StreamBlockAccumulator`.
    /// It must (a) fold `ThinkingDelta`/`SignatureDelta` into the thinking block
    /// rather than dropping them, and (b) preserve the model's interleave order
    /// (thinking-first), instead of appending non-text blocks last.
    #[test]
    fn stream_block_accumulator_keeps_thinking_signature_and_order() {
        let mut acc = StreamBlockAccumulator::new();

        // Block 0: a signed thinking block, streamed as thinking-then-signature.
        acc.on_event(&StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Thinking {
                thinking: String::new(),
                signature: String::new(),
            },
        });
        acc.on_event(&StreamEvent::ThinkingDelta {
            index: 0,
            thinking: "Let me ".into(),
        });
        acc.on_event(&StreamEvent::ThinkingDelta {
            index: 0,
            thinking: "reason.".into(),
        });
        acc.on_event(&StreamEvent::SignatureDelta {
            index: 0,
            signature: "sig-".into(),
        });
        acc.on_event(&StreamEvent::SignatureDelta {
            index: 0,
            signature: "abc123".into(),
        });
        acc.on_event(&StreamEvent::ContentBlockStop { index: 0 });

        // Block 1: visible text.
        acc.on_event(&StreamEvent::ContentBlockStart {
            index: 1,
            content_block: ContentBlock::Text {
                text: String::new(),
            },
        });
        acc.on_event(&StreamEvent::TextDelta {
            index: 1,
            text: "Hello world".into(),
        });
        acc.on_event(&StreamEvent::ContentBlockStop { index: 1 });

        // Block 2: a tool call, streamed as partial JSON.
        acc.on_event(&StreamEvent::ContentBlockStart {
            index: 2,
            content_block: ContentBlock::ToolUse {
                id: "tool_1".into(),
                name: "get_weather".into(),
                input: Value::Null,
                thought_signature: Some("sig-tool-xyz".into()),
            },
        });
        acc.on_event(&StreamEvent::InputJsonDelta {
            index: 2,
            partial_json: r#"{"city":"#.into(),
        });
        acc.on_event(&StreamEvent::InputJsonDelta {
            index: 2,
            partial_json: r#""Paris"}"#.into(),
        });
        acc.on_event(&StreamEvent::ContentBlockStop { index: 2 });

        let blocks = acc.finish();
        assert_eq!(blocks.len(), 3, "all three blocks survive");

        // Order preserved: thinking → text → tool_use. The old aggregator
        // appended non-text blocks last (usize::MAX), so thinking would have
        // landed *after* the text block.
        match &blocks[0] {
            ContentBlock::Thinking { thinking, signature } => {
                assert_eq!(thinking, "Let me reason.", "thinking text captured");
                assert_eq!(signature, "sig-abc123", "signature preserved verbatim");
            }
            other => panic!("expected Thinking block first, got {other:?}"),
        }
        match &blocks[1] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello world"),
            other => panic!("expected Text block second, got {other:?}"),
        }
        match &blocks[2] {
            ContentBlock::ToolUse { id, name, input, thought_signature } => {
                assert_eq!(id, "tool_1");
                assert_eq!(name, "get_weather");
                assert_eq!(
                    input.get("city").and_then(Value::as_str),
                    Some("Paris"),
                    "tool_use JSON assembled from partial deltas"
                );
                assert_eq!(
                    thought_signature.as_deref(),
                    Some("sig-tool-xyz"),
                    "opaque tool-call signature survives stream assembly"
                );
            }
            other => panic!("expected ToolUse block third, got {other:?}"),
        }
    }

    /// The `ReasoningDelta` alias (used by some providers) must also land in the
    /// thinking block rather than being dropped.
    #[test]
    fn stream_block_accumulator_folds_reasoning_delta_into_thinking() {
        let mut acc = StreamBlockAccumulator::new();
        acc.on_event(&StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Thinking {
                thinking: String::new(),
                signature: String::new(),
            },
        });
        acc.on_event(&StreamEvent::ReasoningDelta {
            index: 0,
            reasoning: "scratch pad".into(),
        });
        acc.on_event(&StreamEvent::ContentBlockStop { index: 0 });

        let blocks = acc.finish();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Thinking { thinking, .. } => assert_eq!(thinking, "scratch pad"),
            other => panic!("expected Thinking block, got {other:?}"),
        }
    }
}
