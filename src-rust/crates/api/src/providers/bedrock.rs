// providers/bedrock.rs — Amazon Bedrock provider adapter.
//
// Uses the Bedrock Converse Streaming API which accepts a unified message
// format similar to Anthropic's, making it straightforward to map from
// our internal ProviderRequest.
//
// Endpoint:
//   POST https://bedrock-runtime.{region}.amazonaws.com/model/{model_id}/converse-stream
//
// Auth:
//   - If AWS_BEARER_TOKEN_BEDROCK is set: Authorization: Bearer <token>
//   - Otherwise: AWS SigV4 signed request using access key + secret
//
// Only Claude models on Bedrock are officially supported by this adapter.

use std::pin::Pin;

use async_stream::stream;
use async_trait::async_trait;
use claurst_core::provider_id::ProviderId;
use claurst_core::types::{ContentBlock, MessageContent, Role, ToolResultContent, UsageInfo};
use futures::Stream;
use serde_json::{json, Value};
use tracing::debug;

use crate::error_handling::parse_error_response;
use crate::provider::LlmProvider;
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StopReason,
    StreamEvent, SystemPrompt, SystemPromptStyle,
};

use super::message_normalization::remove_empty_messages;
use super::request_options::merge_bedrock_options;

// ---------------------------------------------------------------------------
// BedrockProvider
// ---------------------------------------------------------------------------

pub struct BedrockProvider {
    id: ProviderId,
    region: String,
    http_client: reqwest::Client,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    bearer_token: Option<String>,
}

impl BedrockProvider {
    pub fn from_env() -> Option<Self> {
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());

        let http_client = reqwest::Client::builder()
            .timeout(crate::request_timeout())
            .build()
            .expect("failed to build reqwest client");

        // Bearer token takes priority over SigV4 credentials.
        if let Ok(token) = std::env::var("AWS_BEARER_TOKEN_BEDROCK") {
            return Some(Self {
                id: ProviderId::new(ProviderId::AMAZON_BEDROCK),
                region,
                http_client,
                access_key_id: None,
                secret_access_key: None,
                session_token: None,
                bearer_token: Some(token),
            });
        }

        // Standard SigV4 credentials.
        let key = std::env::var("AWS_ACCESS_KEY_ID").ok()?;
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok()?;
        let session = std::env::var("AWS_SESSION_TOKEN").ok();

        Some(Self {
            id: ProviderId::new(ProviderId::AMAZON_BEDROCK),
            region,
            http_client,
            access_key_id: Some(key),
            secret_access_key: Some(secret),
            session_token: session,
            bearer_token: None,
        })
    }

    /// Add a regional cross-inference prefix for models that support it.
    fn model_id_with_prefix(&self, model: &str) -> String {
        // Skip if already has a dot-separated prefix (e.g. "us.anthropic.claude-...")
        if model.contains('.') {
            return model.to_string();
        }
        let region = &self.region;
        if region.starts_with("us-") && !region.contains("gov") {
            if model.contains("claude") || model.contains("nova") {
                return format!("us.{}", model);
            }
        } else if region.starts_with("eu-") && model.contains("claude") {
            return format!("eu.{}", model);
        }
        model.to_string()
    }

    fn endpoint_url(&self, model_id: &str) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse-stream",
            self.region,
            urlencoding::encode(model_id)
        )
    }

    // -----------------------------------------------------------------------
    // AWS SigV4 signing
    // -----------------------------------------------------------------------

    fn sign_request(
        &self,
        method: &str,
        url_str: &str,
        body: &str,
        date: &chrono::DateTime<chrono::Utc>,
    ) -> std::collections::HashMap<String, String> {
        use hmac::{Hmac, Mac};
        use sha2::{Digest, Sha256};

        type HmacSha256 = Hmac<Sha256>;

        let mut headers = std::collections::HashMap::new();

        // If we have a bearer token, skip SigV4.
        if let Some(ref token) = self.bearer_token {
            headers.insert("Authorization".to_string(), format!("Bearer {}", token));
            return headers;
        }

        let access_key = match &self.access_key_id {
            Some(k) => k.clone(),
            None => return headers,
        };
        let secret_key = match &self.secret_access_key {
            Some(s) => s.clone(),
            None => return headers,
        };

        let date_str = date.format("%Y%m%d").to_string();
        let datetime_str = date.format("%Y%m%dT%H%M%SZ").to_string();
        let service = "bedrock";
        let region = &self.region;

        // Parse path and query from URL.
        let parsed = url::Url::parse(url_str).unwrap_or_else(|_| {
            url::Url::parse("https://bedrock-runtime.us-east-1.amazonaws.com/").unwrap()
        });
        let canonical_uri = {
            let p = parsed.path();
            if p.is_empty() { "/".to_string() } else { p.to_string() }
        };
        let canonical_query = parsed.query().unwrap_or("").to_string();

        // Body hash.
        let body_hash = hex::encode(Sha256::digest(body.as_bytes()));

        // Canonical headers (must be sorted, lowercased).
        let host = parsed.host_str().unwrap_or_default().to_string();
        let content_type = "application/json";

        // Build canonical headers string and signed headers list.
        // Include: content-type, host, x-amz-content-sha256, x-amz-date,
        // and optionally x-amz-security-token.
        let mut canonical_headers = format!(
            "content-type:{}\nhost:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
            content_type, host, body_hash, datetime_str
        );
        let mut signed_headers =
            "content-type;host;x-amz-content-sha256;x-amz-date".to_string();

        if let Some(ref tok) = self.session_token {
            canonical_headers.push_str(&format!("x-amz-security-token:{}\n", tok));
            signed_headers.push_str(";x-amz-security-token");
        }

        // Canonical request.
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method,
            canonical_uri,
            canonical_query,
            canonical_headers,
            signed_headers,
            body_hash
        );

        // String to sign.
        let credential_scope =
            format!("{}/{}/{}/aws4_request", date_str, region, service);
        let canonical_request_hash =
            hex::encode(Sha256::digest(canonical_request.as_bytes()));
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            datetime_str, credential_scope, canonical_request_hash
        );

        // Signing key: HMAC-SHA256 chain.
        let sign_key = {
            let k_date = {
                let mut mac = HmacSha256::new_from_slice(
                    format!("AWS4{}", secret_key).as_bytes(),
                )
                .expect("HMAC init failed");
                mac.update(date_str.as_bytes());
                mac.finalize().into_bytes()
            };
            let k_region = {
                let mut mac = HmacSha256::new_from_slice(&k_date)
                    .expect("HMAC init failed");
                mac.update(region.as_bytes());
                mac.finalize().into_bytes()
            };
            let k_service = {
                let mut mac = HmacSha256::new_from_slice(&k_region)
                    .expect("HMAC init failed");
                mac.update(service.as_bytes());
                mac.finalize().into_bytes()
            };
            
            {
                let mut mac = HmacSha256::new_from_slice(&k_service)
                    .expect("HMAC init failed");
                mac.update(b"aws4_request");
                mac.finalize().into_bytes()
            }
        };

        let signature = {
            let mut mac =
                HmacSha256::new_from_slice(&sign_key).expect("HMAC init failed");
            mac.update(string_to_sign.as_bytes());
            hex::encode(mac.finalize().into_bytes())
        };

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            access_key, credential_scope, signed_headers, signature
        );

        headers.insert("Authorization".to_string(), authorization);
        headers.insert("x-amz-date".to_string(), datetime_str);
        headers.insert("x-amz-content-sha256".to_string(), body_hash);
        if let Some(ref tok) = self.session_token {
            headers.insert("x-amz-security-token".to_string(), tok.clone());
        }

        headers
    }

    // -----------------------------------------------------------------------
    // Request body builders
    // -----------------------------------------------------------------------

    fn build_converse_body(request: &ProviderRequest) -> Value {
        let messages = Self::build_converse_messages(request);
        let mut body = json!({
            "messages": messages,
            "inferenceConfig": {
                "maxTokens": request.max_tokens,
                "temperature": request.temperature.unwrap_or(0.7),
                "topP": request.top_p.unwrap_or(0.9),
                "stopSequences": request.stop_sequences,
            }
        });

        // System prompt.
        if let Some(sys) = &request.system_prompt {
            let sys_text = match sys {
                SystemPrompt::Text(t) => t.clone(),
                SystemPrompt::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| b.text.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            body["system"] = json!([{ "text": sys_text }]);
        }

        // Tool definitions.
        if !request.tools.is_empty() {
            let tool_specs: Vec<Value> = request
                .tools
                .iter()
                .map(|td| {
                    json!({
                        "toolSpec": {
                            "name": td.name,
                            "description": td.description,
                            "inputSchema": {
                                "json": td.input_schema
                            }
                        }
                    })
                })
                .collect();
            body["toolConfig"] = json!({ "tools": tool_specs });
        }

        if let Some(thinking) = &request.thinking {
            body["reasoningConfig"] = json!({
                "type": "enabled",
                "budgetTokens": thinking.budget_tokens,
            });
        }

        merge_bedrock_options(&mut body, &request.provider_options);

        body
    }

    fn build_converse_messages(request: &ProviderRequest) -> Vec<Value> {
        remove_empty_messages(&request.messages)
            .iter()
            .map(|msg| {
                let role = match msg.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                let content = Self::message_content_to_converse(&msg.content, &msg.role);
                json!({ "role": role, "content": content })
            })
            .collect()
    }

    fn message_content_to_converse(content: &MessageContent, role: &Role) -> Vec<Value> {
        match content {
            MessageContent::Text(t) => vec![json!({ "text": t })],
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| Self::content_block_to_converse(b, role))
                .collect(),
        }
    }

    // `role` is threaded through to the recursive calls for tool-result blocks
    // to keep the Converse mapping symmetric; keep the parameter rather than
    // dropping it and changing the recursion's shape.
    #[allow(clippy::only_used_in_recursion)]
    fn content_block_to_converse(block: &ContentBlock, role: &Role) -> Option<Value> {
        match block {
            ContentBlock::Text { text } => Some(json!({ "text": text })),
            ContentBlock::Image { source } => {
                // Bedrock Converse image format.
                let media_type = source
                    .media_type
                    .as_deref()
                    .unwrap_or("image/png")
                    .replace("image/", "");
                if let Some(data) = &source.data {
                    Some(json!({
                        "image": {
                            "format": media_type,
                            "source": {
                                "bytes": data
                            }
                        }
                    }))
                } else if let Some(url) = &source.url {
                    // Bedrock doesn't support URL images natively; skip.
                    debug!("Bedrock does not support URL images: {}", url);
                    None
                } else {
                    None
                }
            }
            ContentBlock::ToolUse { id, name, input, .. } => Some(json!({
                "toolUse": {
                    "toolUseId": id,
                    "name": name,
                    "input": input
                }
            })),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let result_content = match content {
                    ToolResultContent::Text(t) => vec![json!({ "text": t })],
                    ToolResultContent::Blocks(inner) => inner
                        .iter()
                        .filter_map(|b| Self::content_block_to_converse(b, role))
                        .collect(),
                };
                let status = if is_error.unwrap_or(false) {
                    "error"
                } else {
                    "success"
                };
                Some(json!({
                    "toolResult": {
                        "toolUseId": tool_use_id,
                        "content": result_content,
                        "status": status
                    }
                }))
            }
            ContentBlock::Thinking { thinking, .. } => Some(json!({ "text": thinking })),
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // HTTP helpers
    // -----------------------------------------------------------------------

    fn map_http_error(&self, status: u16, body: &str) -> ProviderError {
        parse_error_response(status, body, &self.id)
    }

    // -----------------------------------------------------------------------
    // Send helpers
    // -----------------------------------------------------------------------

    async fn send_streaming(
        &self,
        request: &ProviderRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let bedrock_model = self.model_id_with_prefix(&request.model);
        let url = self.endpoint_url(&bedrock_model);

        let body = Self::build_converse_body(request);
        let body_str = serde_json::to_string(&body).unwrap_or_default();

        let now = chrono::Utc::now();
        let auth_headers = self.sign_request("POST", &url, &body_str, &now);

        let mut req_builder = self
            .http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/vnd.amazon.eventstream");

        for (k, v) in &auth_headers {
            req_builder = req_builder.header(k.as_str(), v.as_str());
        }

        let resp = req_builder
            .body(body_str)
            .send()
            .await
            .map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("HTTP request failed: {}", e),
                status: None,
                body: None,
            })?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&(status as usize)) {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.map_http_error(status, &text));
        }

        Ok(resp)
    }

    async fn send_non_streaming(
        &self,
        request: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let bedrock_model = self.model_id_with_prefix(&request.model);
        // Non-streaming uses /converse (not /converse-stream)
        let url = format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse",
            self.region,
            urlencoding::encode(&bedrock_model)
        );

        let body = Self::build_converse_body(request);
        let body_str = serde_json::to_string(&body).unwrap_or_default();

        let now = chrono::Utc::now();
        let auth_headers = self.sign_request("POST", &url, &body_str, &now);

        let mut req_builder = self
            .http_client
            .post(&url)
            .header("Content-Type", "application/json");

        for (k, v) in &auth_headers {
            req_builder = req_builder.header(k.as_str(), v.as_str());
        }

        let resp = req_builder
            .body(body_str)
            .send()
            .await
            .map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("HTTP request failed: {}", e),
                status: None,
                body: None,
            })?;

        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to read response body: {}", e),
            status: Some(status),
            body: None,
        })?;

        if !(200..300).contains(&(status as usize)) {
            return Err(self.map_http_error(status, &text));
        }

        let json_val: Value = serde_json::from_str(&text).map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to parse response JSON: {}", e),
            status: Some(status),
            body: Some(text.clone()),
        })?;

        Self::parse_converse_response(&json_val, &self.id)
    }

    fn parse_converse_response(
        json: &Value,
        provider_id: &ProviderId,
    ) -> Result<ProviderResponse, ProviderError> {
        // Bedrock Converse non-streaming response shape:
        // { "output": { "message": { "role": "assistant", "content": [...] } },
        //   "stopReason": "end_turn",
        //   "usage": { "inputTokens": N, "outputTokens": M } }

        let message = json
            .get("output")
            .and_then(|o| o.get("message"))
            .ok_or_else(|| ProviderError::Other {
                provider: provider_id.clone(),
                message: "No output.message in Bedrock response".to_string(),
                status: None,
                body: None,
            })?;

        let content_blocks = Self::parse_converse_content(
            message.get("content").and_then(|c| c.as_array()),
        );

        let stop_reason_str = json
            .get("stopReason")
            .and_then(|v| v.as_str())
            .unwrap_or("end_turn");
        let stop_reason = Self::map_stop_reason(stop_reason_str);

        let usage = Self::parse_converse_usage(json.get("usage"));

        Ok(ProviderResponse {
            id: uuid::Uuid::new_v4().to_string(),
            content: content_blocks,
            stop_reason,
            usage,
            model: json
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
    }

    fn parse_converse_content(content: Option<&Vec<Value>>) -> Vec<ContentBlock> {
        let blocks = match content {
            Some(b) => b,
            None => return vec![],
        };

        blocks
            .iter()
            .filter_map(|b| {
                if let Some(text) = b.get("text").and_then(|v| v.as_str()) {
                    return Some(ContentBlock::Text {
                        text: text.to_string(),
                    });
                }
                if let Some(tu) = b.get("toolUse") {
                    let id = tu
                        .get("toolUseId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = tu
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input = tu.get("input").cloned().unwrap_or(json!({}));
                    return Some(ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        thought_signature: None,
                    });
                }
                None
            })
            .collect()
    }

    fn map_stop_reason(reason: &str) -> StopReason {
        match reason {
            "end_turn" => StopReason::EndTurn,
            "max_tokens" => StopReason::MaxTokens,
            "tool_use" => StopReason::ToolUse,
            "stop_sequence" => StopReason::StopSequence,
            "content_filtered" => StopReason::ContentFiltered,
            other => StopReason::Other(other.to_string()),
        }
    }

    fn parse_converse_usage(usage: Option<&Value>) -> UsageInfo {
        let u = match usage {
            Some(v) => v,
            None => return UsageInfo::default(),
        };
        UsageInfo {
            input_tokens: u
                .get("inputTokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            output_tokens: u
                .get("outputTokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// LlmProvider impl
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmProvider for BedrockProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "Amazon Bedrock"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        self.send_non_streaming(&request).await
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let resp = self.send_streaming(&request).await?;
        let provider_id = self.id.clone();

        // TODO(#228): this AWS event-stream framing + Converse event decode is the
        // **BedrockConverse** wire protocol; it should move into a
        // `protocol::bedrock_converse` decoder (its binary framing means it does
        // not share the SSE byte-line decoder the other protocols use).
        //
        // Bedrock Converse streaming uses the AWS event-stream binary framing
        // (`vnd.amazon.eventstream`). Each message on the wire is:
        //
        //   total_length(u32 BE) | headers_length(u32 BE) | prelude_crc(u32 BE)
        //     | headers | payload | message_crc(u32 BE)
        //
        // The payload is the JSON body for the `:event-type` named in the
        // headers (`messageStart`, `contentBlockDelta`, `messageStop`,
        // `metadata`, ...). We parse the prelude to learn the exact frame
        // length, extract the payload, advance the buffer by exactly
        // `total_length` bytes, and hand a `{ <event-type>: <payload> }` object
        // to parse_bedrock_event. Partial frames are kept in the buffer until a
        // later chunk completes them. See `parse_event_stream_frame` below.
        let s = stream! {
            use futures::StreamExt;

            let mut byte_stream = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            let mut message_started = false;
            let mut message_stopped = false;

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(ProviderError::StreamError {
                            provider: provider_id.clone(),
                            message: format!("Stream read error: {}", e),
                            partial_response: None,
                        });
                        return;
                    }
                };

                buf.extend_from_slice(&chunk);

                // Pull every complete event-stream frame out of the buffer.
                for ev in drain_event_stream_frames(&mut buf, &provider_id, &mut message_started) {
                    if matches!(ev, Ok(StreamEvent::MessageStop)) {
                        message_stopped = true;
                    }
                    yield ev;
                }
            }

            // Safety net: if the stream ended without an explicit `messageStop`
            // event, close the message so downstream consumers still finalize.
            if message_started && !message_stopped {
                yield Ok(StreamEvent::MessageStop);
            }
        };

        Ok(Box::pin(s))
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        // Lightweight check: GET the list-foundation-models endpoint.
        let url = format!(
            "https://bedrock.{}.amazonaws.com/foundation-models",
            self.region
        );
        let now = chrono::Utc::now();
        // For health check, sign an empty GET body.
        let auth_headers = self.sign_request("GET", &url, "", &now);

        let mut req_builder = self.http_client.get(&url);
        for (k, v) in &auth_headers {
            req_builder = req_builder.header(k.as_str(), v.as_str());
        }

        let resp = req_builder.send().await;
        match resp {
            Ok(r) if r.status().is_success() => Ok(ProviderStatus::Healthy),
            Ok(r) if r.status().as_u16() == 401 || r.status().as_u16() == 403 => {
                Ok(ProviderStatus::Unavailable {
                    reason: "authentication failed".to_string(),
                })
            }
            Ok(r) => Ok(ProviderStatus::Degraded {
                reason: format!("foundation-models returned {}", r.status()),
            }),
            Err(e) => Ok(ProviderStatus::Unavailable {
                reason: e.to_string(),
            }),
        }
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
            structured_output: false,
            system_prompt_style: SystemPromptStyle::TopLevel,
        }
    }
}

// ---------------------------------------------------------------------------
// Bedrock event parsing helper (free function so it can be used in stream!)
// ---------------------------------------------------------------------------

fn parse_bedrock_event(
    val: &Value,
    provider_id: &ProviderId,
    message_started: &mut bool,
) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut events = Vec::new();

    // Bedrock Converse streaming events come in several shapes.
    // We check for the most common ones:

    // messageStart
    if let Some(msg_start) = val.get("messageStart") {
        let role = msg_start
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("assistant");
        let _ = role;
        if !*message_started {
            events.push(Ok(StreamEvent::MessageStart {
                id: uuid::Uuid::new_v4().to_string(),
                model: String::new(),
                usage: UsageInfo::default(),
            }));
            *message_started = true;
        }
        return events;
    }

    // contentBlockStart
    if let Some(cb_start) = val.get("contentBlockStart") {
        let index = cb_start
            .get("contentBlockIndex")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        if !*message_started {
            events.push(Ok(StreamEvent::MessageStart {
                id: uuid::Uuid::new_v4().to_string(),
                model: String::new(),
                usage: UsageInfo::default(),
            }));
            *message_started = true;
        }
        let start_val = cb_start.get("start");
        if let Some(tool_use) = start_val.and_then(|s| s.get("toolUse")) {
            let id = tool_use
                .get("toolUseId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = tool_use
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            events.push(Ok(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlock::ToolUse {
                    id,
                    name,
                    input: json!({}),
                    thought_signature: None,
                },
            }));
        } else {
            events.push(Ok(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlock::Text { text: String::new() },
            }));
        }
        return events;
    }

    // contentBlockDelta
    if let Some(cb_delta) = val.get("contentBlockDelta") {
        let index = cb_delta
            .get("contentBlockIndex")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        if !*message_started {
            events.push(Ok(StreamEvent::MessageStart {
                id: uuid::Uuid::new_v4().to_string(),
                model: String::new(),
                usage: UsageInfo::default(),
            }));
            events.push(Ok(StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text { text: String::new() },
            }));
            *message_started = true;
        }
        if let Some(delta) = cb_delta.get("delta") {
            if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    events.push(Ok(StreamEvent::TextDelta {
                        index,
                        text: text.to_string(),
                    }));
                }
            } else if let Some(json_frag) = delta
                .get("toolUse")
                .and_then(|tu| tu.get("input"))
                .and_then(|v| v.as_str())
            {
                if !json_frag.is_empty() {
                    events.push(Ok(StreamEvent::InputJsonDelta {
                        index,
                        partial_json: json_frag.to_string(),
                    }));
                }
            }
        }
        return events;
    }

    // contentBlockStop
    if let Some(cb_stop) = val.get("contentBlockStop") {
        let index = cb_stop
            .get("contentBlockIndex")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        events.push(Ok(StreamEvent::ContentBlockStop { index }));
        return events;
    }

    // messageStop
    if let Some(msg_stop) = val.get("messageStop") {
        let stop_reason_str = msg_stop
            .get("stopReason")
            .and_then(|v| v.as_str())
            .unwrap_or("end_turn");
        let stop_reason = match stop_reason_str {
            "end_turn" => StopReason::EndTurn,
            "max_tokens" => StopReason::MaxTokens,
            "tool_use" => StopReason::ToolUse,
            "stop_sequence" => StopReason::StopSequence,
            other => StopReason::Other(other.to_string()),
        };
        events.push(Ok(StreamEvent::MessageDelta {
            stop_reason: Some(stop_reason),
            usage: None,
        }));
        events.push(Ok(StreamEvent::MessageStop));
        return events;
    }

    // metadata (usage)
    if let Some(metadata) = val.get("metadata") {
        if let Some(usage_val) = metadata.get("usage") {
            let usage = UsageInfo {
                input_tokens: usage_val
                    .get("inputTokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                output_tokens: usage_val
                    .get("outputTokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            };
            events.push(Ok(StreamEvent::MessageDelta {
                stop_reason: None,
                usage: Some(usage),
            }));
        }
        return events;
    }

    // internalServerException / throttlingException
    if let Some(err) = val
        .get("internalServerException")
        .or_else(|| val.get("throttlingException"))
        .or_else(|| val.get("modelStreamErrorException"))
        .or_else(|| val.get("validationException"))
    {
        let message = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown Bedrock error")
            .to_string();
        events.push(Err(ProviderError::StreamError {
            provider: provider_id.clone(),
            message,
            partial_response: None,
        }));
    }

    events
}

// ---------------------------------------------------------------------------
// AWS event-stream (vnd.amazon.eventstream) framing parser
// ---------------------------------------------------------------------------

/// Parse a single AWS event-stream frame from the front of `buf`.
///
/// Wire layout of one message (all integers big-endian):
///
/// ```text
///   total_length   : u32   whole frame, including this field and trailing CRC
///   headers_length : u32
///   prelude_crc    : u32   CRC32 of the first 8 bytes  (not validated here)
///   headers        : headers_length bytes
///   payload        : total_length - headers_length - 16 bytes
///   message_crc    : u32   CRC32 of everything before it (not validated here)
/// ```
///
/// Returns `Some((event_type, payload, frame_len))` once a whole frame is
/// buffered, where `event_type` is the `:event-type` header value (falling back
/// to `:exception-type` for error frames) and `frame_len == total_length`.
/// Returns `None` when more bytes are required to complete the current frame.
///
/// We parse strictly by the length declared in the prelude — this is
/// deterministic and keeps the buffer frame-aligned — and deliberately skip
/// CRC32 validation. `crc32fast` is only present transitively in the workspace
/// lockfile (not a direct dependency of this crate), and length-based framing is
/// sufficient because the prelude gives the exact frame boundary.
fn parse_event_stream_frame(buf: &[u8]) -> Option<(String, &[u8], usize)> {
    // The 12-byte prelude must be present before we can learn the frame length.
    if buf.len() < 12 {
        return None;
    }

    let total_length = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let headers_length = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;

    // A frame is at least prelude(12) + message_crc(4) = 16 bytes of fixed
    // overhead, and the headers must fit between the prelude and the CRC. Bail
    // on a structurally impossible prelude rather than slicing out of bounds.
    if total_length < 16 || headers_length > total_length - 16 {
        return None;
    }

    // Wait until the entire frame has arrived.
    if buf.len() < total_length {
        return None;
    }

    let headers = &buf[12..12 + headers_length];
    let payload = &buf[12 + headers_length..total_length - 4];
    let event_type = event_type_from_headers(headers).unwrap_or_default();

    Some((event_type, payload, total_length))
}

/// Walk an AWS event-stream headers block and return the value of the
/// `:event-type` header (or `:exception-type` for error frames).
///
/// Each header is `name_len(u8) | name | value_type(u8) | value`, where the
/// value encoding depends on `value_type`. We only need the string headers but
/// still skip the other value types by their fixed / length-prefixed sizes so
/// the walk stays aligned across the whole block.
fn event_type_from_headers(mut headers: &[u8]) -> Option<String> {
    let mut event_type: Option<String> = None;
    let mut exception_type: Option<String> = None;

    while !headers.is_empty() {
        let name_len = *headers.first()? as usize;
        headers = headers.get(1..)?;
        if headers.len() < name_len {
            break;
        }
        let (name, rest) = headers.split_at(name_len);
        headers = rest;

        let value_type = *headers.first()?;
        headers = headers.get(1..)?;

        let value = match value_type {
            // bool true / false: no value bytes.
            0 | 1 => None,
            // byte / short / int / long: fixed-width, skipped.
            2 => {
                headers = headers.get(1..)?;
                None
            }
            3 => {
                headers = headers.get(2..)?;
                None
            }
            4 => {
                headers = headers.get(4..)?;
                None
            }
            5 => {
                headers = headers.get(8..)?;
                None
            }
            // byte-array (6) / string (7): u16 length prefix + that many bytes.
            6 | 7 => {
                if headers.len() < 2 {
                    break;
                }
                let len = u16::from_be_bytes([headers[0], headers[1]]) as usize;
                headers = headers.get(2..)?;
                if headers.len() < len {
                    break;
                }
                let (bytes, rest) = headers.split_at(len);
                headers = rest;
                if value_type == 7 {
                    std::str::from_utf8(bytes).ok().map(str::to_string)
                } else {
                    None
                }
            }
            // timestamp (8): i64, uuid (9): 16 bytes.
            8 => {
                headers = headers.get(8..)?;
                None
            }
            9 => {
                headers = headers.get(16..)?;
                None
            }
            // Unknown value type — alignment can no longer be trusted.
            _ => break,
        };

        match name {
            b":event-type" => event_type = value,
            b":exception-type" => exception_type = value,
            _ => {}
        }
    }

    event_type.or(exception_type)
}

/// Drain every complete event-stream frame currently buffered in `buf`, mapping
/// each to zero or more [`StreamEvent`]s via [`parse_bedrock_event`]. Consumed
/// frames are removed from `buf`; a trailing partial frame is left untouched for
/// a later network chunk to complete.
fn drain_event_stream_frames(
    buf: &mut Vec<u8>,
    provider_id: &ProviderId,
    message_started: &mut bool,
) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut out = Vec::new();

    // Kept as an explicit `loop`: each frame's payload is parsed into an owned
    // value inside the match arm to release the `buf` borrow before we drain the
    // consumed frame below — a `while let` binding on `buf` would hold that
    // borrow across the mutation and fail to compile.
    #[allow(clippy::while_let_loop)]
    loop {
        // Parse the payload into an owned value inside the match arm so the
        // borrow on `buf` is released before we drain the consumed frame below.
        let (event_type, payload_val, frame_len) = match parse_event_stream_frame(buf) {
            Some((event_type, payload, frame_len)) => (
                event_type,
                serde_json::from_slice::<Value>(payload).ok(),
                frame_len,
            ),
            None => break,
        };

        // Skip control frames without an event type (e.g. an initial response)
        // but still advance past them so the buffer stays frame-aligned.
        if !event_type.is_empty() {
            if let Some(payload_val) = payload_val {
                // parse_bedrock_event expects the `{ <event-type>: <payload> }`
                // shape, matching how events nest in the non-stream Converse
                // response — so we re-wrap the payload under its event type.
                let mut wrapped = serde_json::Map::new();
                wrapped.insert(event_type, payload_val);
                out.extend(parse_bedrock_event(
                    &Value::Object(wrapped),
                    provider_id,
                    message_started,
                ));
            }
        }

        buf.drain(..frame_len);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::provider_id::ProviderId;
    use serde_json::Value;

    /// Build a valid AWS event-stream frame carrying a single `:event-type`
    /// string header plus the given JSON payload (prelude + headers + payload +
    /// trailing CRC). The CRC fields are zero-filled because the parser reads by
    /// length and does not validate them.
    fn build_event_stream_frame(event_type: &str, payload: &str) -> Vec<u8> {
        // One header: `:event-type` as a string (value type 7).
        let name = b":event-type";
        let value = event_type.as_bytes();
        let mut headers = Vec::new();
        headers.push(name.len() as u8);
        headers.extend_from_slice(name);
        headers.push(7u8); // string value type
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value);

        let payload_bytes = payload.as_bytes();
        let headers_len = headers.len();
        let total_len = 12 + headers_len + payload_bytes.len() + 4;

        let mut frame = Vec::with_capacity(total_len);
        frame.extend_from_slice(&(total_len as u32).to_be_bytes());
        frame.extend_from_slice(&(headers_len as u32).to_be_bytes());
        frame.extend_from_slice(&0u32.to_be_bytes()); // prelude CRC (unvalidated)
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload_bytes);
        frame.extend_from_slice(&0u32.to_be_bytes()); // message CRC (unvalidated)
        frame
    }

    fn collect_text(events: &[Result<StreamEvent, ProviderError>]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::TextDelta { text, .. }) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parses_single_content_block_delta_frame() {
        let frame = build_event_stream_frame(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"Hello"}}"#,
        );

        // The framing parser reports the exact event type and frame length.
        let (event_type, payload, frame_len) =
            parse_event_stream_frame(&frame).expect("frame should be complete");
        assert_eq!(event_type, "contentBlockDelta");
        assert_eq!(frame_len, frame.len());
        let payload_val: Value = serde_json::from_slice(payload).unwrap();
        assert_eq!(payload_val["delta"]["text"], Value::from("Hello"));

        // End-to-end: draining maps it to a TextDelta and consumes the frame.
        let provider_id = ProviderId::new(ProviderId::AMAZON_BEDROCK);
        let mut buf = frame.clone();
        let mut started = false;
        let events = drain_event_stream_frames(&mut buf, &provider_id, &mut started);
        assert!(buf.is_empty(), "the frame should be fully consumed");
        assert_eq!(collect_text(&events), "Hello");
    }

    #[test]
    fn handles_frame_split_across_two_chunks() {
        let frame = build_event_stream_frame(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"world"}}"#,
        );
        let split = frame.len() / 2;

        let provider_id = ProviderId::new(ProviderId::AMAZON_BEDROCK);
        let mut buf: Vec<u8> = Vec::new();
        let mut started = false;

        // First chunk: an incomplete frame — nothing emitted, bytes retained.
        buf.extend_from_slice(&frame[..split]);
        assert!(
            parse_event_stream_frame(&buf).is_none(),
            "a partial frame must not parse"
        );
        let events = drain_event_stream_frames(&mut buf, &provider_id, &mut started);
        assert!(events.is_empty());
        assert_eq!(buf.len(), split, "the partial frame must be retained");

        // Second chunk completes the frame.
        buf.extend_from_slice(&frame[split..]);
        let events = drain_event_stream_frames(&mut buf, &provider_id, &mut started);
        assert!(buf.is_empty(), "the completed frame should be consumed");
        assert_eq!(collect_text(&events), "world");
    }

    #[test]
    fn consumes_multiple_frames_from_one_buffer() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&build_event_stream_frame(
            "messageStart",
            r#"{"role":"assistant"}"#,
        ));
        buf.extend_from_slice(&build_event_stream_frame(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"hi"}}"#,
        ));
        buf.extend_from_slice(&build_event_stream_frame(
            "messageStop",
            r#"{"stopReason":"end_turn"}"#,
        ));

        let provider_id = ProviderId::new(ProviderId::AMAZON_BEDROCK);
        let mut started = false;
        let events = drain_event_stream_frames(&mut buf, &provider_id, &mut started);

        assert!(buf.is_empty(), "all three frames should be consumed");
        assert!(matches!(
            events.first(),
            Some(Ok(StreamEvent::MessageStart { .. }))
        ));
        assert_eq!(collect_text(&events), "hi");
        // Exactly one MessageStop, from the explicit `messageStop` event.
        let stops = events
            .iter()
            .filter(|e| matches!(e, Ok(StreamEvent::MessageStop)))
            .count();
        assert_eq!(stops, 1);
    }

    #[test]
    fn reports_incomplete_when_prelude_missing() {
        // Fewer than the 12 prelude bytes: cannot know the frame length yet.
        assert!(parse_event_stream_frame(&[0, 0, 0]).is_none());
    }
}
