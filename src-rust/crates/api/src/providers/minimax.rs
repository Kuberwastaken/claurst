// providers/minimax.rs — MinimaxProvider: Anthropic-compatible provider for MiniMax.
// Minimax requires API key in Authorization header without Bearer prefix.

use std::pin::Pin;

use async_stream::stream;
use async_trait::async_trait;
use claurst_core::provider_id::ProviderId;
use claurst_core::types::{ContentBlock, UsageInfo};
use futures::Stream;
use reqwest::{Client, header};
use serde_json::Value;

use crate::provider::LlmProvider;
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StopReason,
    StreamBlockAccumulator, StreamEvent, SystemPromptStyle,
};
use crate::types::{ApiMessage, ApiToolDefinition, CreateMessageRequest, ThinkingConfig};

use super::message_normalization::normalize_anthropic_messages;

pub struct MinimaxProvider {
    http_client: Client,
    api_key: String,
    api_base: String,
    service_tier: Option<String>,
    id: ProviderId,
}

impl MinimaxProvider {
    pub fn new(api_key: String) -> Self {
        let api_base = std::env::var("MINIMAX_BASE_URL")
            .unwrap_or_else(|_| claurst_core::constants::MINIMAX_ANTHROPIC_API_BASE.to_string());
        let mut headers = header::HeaderMap::new();
        headers.insert("X-Api-Key", header::HeaderValue::from_str(&api_key).expect("unable to parse api key for http header"));
        let http_client = Client::builder()
            .default_headers(headers)
            .timeout(crate::request_timeout())
            .build()
            .expect("MinimaxProvider: failed to build HTTP client");

        Self {
            http_client,
            api_key,
            api_base,
            service_tier: None,
            id: ProviderId::new(ProviderId::MINIMAX),
        }
    }

    pub fn with_base_url(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }

    pub fn with_service_tier(mut self, service_tier: impl Into<String>) -> Self {
        self.service_tier = Some(service_tier.into());
        self
    }

    fn messages_url(api_base: &str) -> String {
        format!("{}/v1/messages", api_base.trim_end_matches('/'))
    }

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
        if request.model.eq_ignore_ascii_case("MiniMax-M3") && request.thinking.is_some() {
            builder = builder.thinking(ThinkingConfig::adaptive());
        }

        builder.build()
    }

    fn build_request_body(
        &self,
        request: &ProviderRequest,
    ) -> Result<Value, serde_json::Error> {
        let mut body = serde_json::to_value(Self::build_request(request))?;
        if let Some(service_tier) = &self.service_tier {
            body["service_tier"] = Value::String(service_tier.clone());
        }
        Ok(body)
    }

    fn map_stop_reason(s: &str) -> StopReason {
        match s {
            "end_turn" => StopReason::EndTurn,
            "stop_sequence" => StopReason::StopSequence,
            "max_tokens" => StopReason::MaxTokens,
            "tool_use" => StopReason::ToolUse,
            other => StopReason::Other(other.to_string()),
        }
    }

    fn map_anthropic_event(value: Value) -> Option<StreamEvent> {
        let event_type = value.get("type")?.as_str()?;

        match event_type {
            "message_start" => {
                let id = value.get("message")?.get("id")?.as_str()?.to_string();
                let model = value.get("message")?.get("model")?.as_str()?.to_string();
                let usage = UsageInfo {
                    input_tokens: value.get("message")?.get("usage")?.get("input_tokens")?.as_u64()?,
                    output_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                };
                Some(StreamEvent::MessageStart { id, model, usage })
            }
            "content_block_start" => {
                let index = value.get("index")?.as_u64()? as usize;
                let content_type = value.get("content_block")?.get("type")?.as_str()?;

                let content_block = match content_type {
                    "text" => ContentBlock::Text {
                        text: String::new(),
                    },
                    "tool_use" => {
                        let id = value.get("content_block")?.get("id")?.as_str()?.to_string();
                        let name = value.get("content_block")?.get("name")?.as_str()?.to_string();
                        ContentBlock::ToolUse {
                            id,
                            name,
                            input: serde_json::Value::Object(Default::default()),
                        }
                    }
                    _ => return None,
                };

                Some(StreamEvent::ContentBlockStart { index, content_block })
            }
            "content_block_delta" => {
                let index = value.get("index")?.as_u64()? as usize;
                let delta_type = value.get("delta")?.get("type")?.as_str()?;

                match delta_type {
                    "text_delta" => {
                        let text = value.get("delta")?.get("text")?.as_str()?.to_string();
                        Some(StreamEvent::TextDelta { index, text })
                    }
                    "thinking_delta" => {
                        let thinking = value.get("delta")?.get("thinking")?.as_str()?.to_string();
                        Some(StreamEvent::ThinkingDelta { index, thinking })
                    }
                    "signature_delta" => {
                        let signature = value.get("delta")?.get("signature")?.as_str()?.to_string();
                        Some(StreamEvent::SignatureDelta { index, signature })
                    }
                    "input_json_delta" => {
                        let partial_json = value.get("delta")?.get("partial_json")?.as_str()?.to_string();
                        Some(StreamEvent::InputJsonDelta { index, partial_json })
                    }
                    _ => None,
                }
            }
            "content_block_stop" => {
                let index = value.get("index")?.as_u64()? as usize;
                Some(StreamEvent::ContentBlockStop { index })
            }
            "message_delta" => {
                let stop_reason = value.get("delta")?
                    .get("stop_reason")?
                    .as_str()
                    .map(Self::map_stop_reason);

                let usage = value.get("delta")?.get("usage")
                    .and_then(|u| {
                        Some(UsageInfo {
                            input_tokens: u.get("input_tokens")?.as_u64()?,
                            output_tokens: u.get("output_tokens")?.as_u64()?,
                            cache_creation_input_tokens: u.get("cache_creation_input_tokens")?.as_u64().unwrap_or(0),
                            cache_read_input_tokens: u.get("cache_read_input_tokens")?.as_u64().unwrap_or(0),
                        })
                    });

                Some(StreamEvent::MessageDelta {
                    stop_reason,
                    usage,
                })
            }
            "message_stop" => Some(StreamEvent::MessageStop),
            "error" => {
                let error_type = value.get("error")?.get("type")?.as_str()?.to_string();
                let message = value.get("error")?.get("message")?.as_str()?.to_string();
                Some(StreamEvent::Error { error_type, message })
            }
            "ping" => None,
            _ => None,
        }
    }
}

#[async_trait]
impl LlmProvider for MinimaxProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "MiniMax"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let mut stream = self.create_message_stream(request).await?;

        let mut id = String::from("unknown");
        let mut model = String::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = UsageInfo::default();

        // Accumulate every content block keyed by its stream index — captures
        // thinking/signature/reasoning deltas and preserves interleave order via
        // a single ordered pass, instead of appending non-text blocks with
        // usize::MAX. Same-class fix as the Anthropic aggregator. See issue #217.
        let mut blocks = StreamBlockAccumulator::new();

        use futures::StreamExt;
        while let Some(result) = stream.next().await {
            match result {
                Err(e) => return Err(e),
                Ok(evt) => {
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

        let final_content: Vec<ContentBlock> = blocks.finish();

        Ok(ProviderResponse {
            id,
            content: final_content,
            stop_reason,
            usage,
            model,
        })
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let body = self
            .build_request_body(&request)
            .map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("Failed to serialize request: {}", e),
                status: None,
                body: None,
            })?;

        let url = Self::messages_url(&self.api_base);
        let api_key = self.api_key.clone();
        let http_client = self.http_client.clone();
        let provider_id = self.id.clone();

        let resp = http_client
            .post(&url)
            .header("Authorization", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Other {
                provider: provider_id.clone(),
                message: format!("HTTP request failed: {}", e),
                status: None,
                body: None,
            })?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let error_body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Other {
                provider: provider_id.clone(),
                message: format!("API error: {}", error_body),
                status: Some(status),
                body: Some(error_body),
            });
        }

        let provider_id_inner = provider_id.clone();
        // TODO(#228): MiniMax streams the **Anthropic** messages wire format
        // (decoded via `map_anthropic_event`), so it belongs to the future
        // `AnthropicMessages` protocol — not `OpenAiChatDecoder`.
        let s = stream! {
            let byte_stream = resp.bytes_stream();
            // Shared byte-buffering decoder (#228): complete lines only, so a
            // multibyte codepoint straddling a chunk boundary is never corrupted.
            let mut decoder = crate::SseByteDecoder::new();

            use futures::StreamExt;
            let mut stream = std::pin::pin!(byte_stream);

            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        for line in decoder.push(&chunk) {
                            let line = line.trim_end_matches('\r').trim();
                            if line.is_empty() {
                                continue;
                            }

                            let data = if let Some(rest) = line.strip_prefix("data:") {
                                rest.trim()
                            } else {
                                line
                            };

                            match serde_json::from_str::<Value>(data) {
                                Ok(value) => {
                                    if let Some(stream_evt) = Self::map_anthropic_event(value) {
                                        yield Ok(stream_evt);
                                    }
                                }
                                Err(e) => {
                                    yield Err(ProviderError::Other {
                                        provider: provider_id_inner.clone(),
                                        message: format!("Failed to parse event: {}", e),
                                        status: None,
                                        body: None,
                                    });
                                }
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(ProviderError::Other {
                            provider: provider_id_inner.clone(),
                            message: format!("Stream error: {}", e),
                            status: None,
                            body: None,
                        });
                    }
                }
            }
        };

        Ok(Box::pin(s))
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        Ok(ProviderStatus::Healthy)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            thinking: true,
            image_input: true,
            pdf_input: false,
            audio_input: false,
            video_input: false,
            caching: true,
            structured_output: true,
            system_prompt_style: SystemPromptStyle::TopLevel,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn request(model: &str, thinking: bool) -> ProviderRequest {
        ProviderRequest {
            model: model.to_string(),
            messages: Vec::new(),
            system_prompt: None,
            tools: Vec::new(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: thinking.then(|| ThinkingConfig::enabled(4096)),
            provider_options: serde_json::json!({}),
        }
    }

    #[test]
    fn default_base_builds_exact_messages_url() {
        assert_eq!(
            MinimaxProvider::messages_url(claurst_core::constants::MINIMAX_ANTHROPIC_API_BASE),
            "https://api.minimax.io/anthropic/v1/messages"
        );
        assert_eq!(
            MinimaxProvider::messages_url("https://api.minimaxi.com/anthropic/"),
            "https://api.minimaxi.com/anthropic/v1/messages"
        );
    }

    #[tokio::test]
    async fn configured_base_and_service_tier_reach_the_wire_request() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener should have an address");
        let capture = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request should connect");
            let mut raw_request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let mut expected_len = None;

            loop {
                let read = socket.read(&mut buffer).await.expect("request should read");
                assert!(read > 0, "request ended before the body was complete");
                raw_request.extend_from_slice(&buffer[..read]);

                if expected_len.is_none() {
                    if let Some(header_end) = raw_request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                    {
                        let headers = std::str::from_utf8(&raw_request[..header_end])
                            .expect("headers should be UTF-8");
                        let content_length = headers
                            .lines()
                            .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                            .and_then(|line| line.split_once(':'))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                            .expect("request should include content-length");
                        expected_len = Some(header_end + 4 + content_length);
                    }
                }

                if expected_len.is_some_and(|len| raw_request.len() >= len) {
                    break;
                }
            }

            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .expect("response should write");
            raw_request
        });

        let provider = MinimaxProvider::new("test-key".to_string())
            .with_base_url(format!("http://{address}/anthropic"))
            .with_service_tier("priority");
        let _stream = provider
            .create_message_stream(request("MiniMax-M3", false))
            .await
            .expect("request should succeed");

        let raw_request = capture.await.expect("capture task should succeed");
        let request_text = String::from_utf8(raw_request).expect("request should be UTF-8");
        let (headers, body) = request_text
            .split_once("\r\n\r\n")
            .expect("request should contain headers and body");
        assert_eq!(
            headers.lines().next(),
            Some("POST /anthropic/v1/messages HTTP/1.1")
        );
        let body: Value = serde_json::from_str(body).expect("body should be JSON");
        assert_eq!(body["service_tier"], serde_json::json!("priority"));
    }

    #[test]
    fn m3_uses_adaptive_thinking_without_a_budget() {
        let body =
            serde_json::to_value(MinimaxProvider::build_request(&request("MiniMax-M3", true)))
                .expect("request should serialize");

        assert_eq!(body["thinking"], serde_json::json!({ "type": "adaptive" }));
        let parsed: ThinkingConfig = serde_json::from_value(body["thinking"].clone())
            .expect("adaptive thinking should deserialize");
        assert_eq!(parsed.thinking_type, "adaptive");
        assert_eq!(parsed.budget_tokens, 0);
    }

    #[test]
    fn always_on_m2_models_do_not_receive_anthropic_budget_config() {
        let body = serde_json::to_value(MinimaxProvider::build_request(&request(
            "MiniMax-M2.7",
            true,
        )))
        .expect("request should serialize");

        assert!(body.get("thinking").is_none());
    }
}
