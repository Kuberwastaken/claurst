// providers/openai_compat.rs — OpenAI-Compatible generic provider adapter.
//
// A configurable OpenAI Chat Completions adapter that can target any
// provider exposing an OpenAI-compatible API.  Configure base URL, auth,
// extra headers, and per-provider behavioural quirks via the builder API.

use std::pin::Pin;

use async_stream::stream;
use async_trait::async_trait;
use claurst_core::provider_id::{ModelId, ProviderId};
use claurst_core::types::ContentBlock;
use futures::Stream;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error_handling::parse_error_response;
use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus,
    StreamEvent, SystemPromptStyle,
};

// Re-use the message transformation helpers from openai.rs.
use super::openai::OpenAiProvider;
use super::request_options::merge_openai_compatible_options;

// ---------------------------------------------------------------------------
// ProviderQuirks
// ---------------------------------------------------------------------------

/// Provider-specific behavioural quirks that alter how the generic adapter
/// builds and interprets requests/responses.
#[derive(Debug, Clone)]
pub struct ProviderQuirks {
    /// Whether this provider supports SSE streaming.  Default: `true`.
    /// When `false`, the query engine falls back to non-streaming requests.
    pub streaming: bool,

    /// Truncate tool call IDs to at most this many characters before sending.
    /// For example, Mistral requires tool IDs of at most 9 characters.
    pub tool_id_max_len: Option<usize>,

    /// If `true`, strip all non-alphanumeric characters from tool IDs.
    pub tool_id_alphanumeric_only: bool,

    /// Extra error-message substrings (or regex-like patterns) that indicate
    /// the request exceeded the model's context window.
    pub overflow_patterns: Vec<String>,

    /// Whether to send `{"stream_options": {"include_usage": true}}` when
    /// streaming.  Required by some providers to receive token counts.
    pub include_usage_in_stream: bool,

    /// Override the sampling temperature when the request does not specify one.
    pub default_temperature: Option<f64>,

    /// Some providers (e.g. older Mistral releases) reject a message sequence
    /// that goes …tool_result → user… without an intervening assistant turn.
    /// When `true`, an `{"role":"assistant","content":"Done."}` message is
    /// inserted between any `role: tool` message and a following `role: user`
    /// message.
    pub fix_tool_user_sequence: bool,

    /// Name of the JSON field in the assistant message that carries extended
    /// reasoning / thinking text.  `None` means the provider does not expose
    /// reasoning output.  Example: `Some("reasoning_content")` for DeepSeek.
    pub reasoning_field: Option<String>,

    /// Whether this provider requires reasoning_content to be echoed back on
    /// subsequent turns in multi-turn conversations.  DeepSeek V4 is currently
    /// the only provider with this requirement; most providers ignore this field.
    /// When false, reasoning is not included in outbound messages to save tokens.
    pub requires_reasoning_roundtrip: bool,

    /// Hard cap on `max_tokens` sent to this provider.  When the request
    /// carries a higher value it is silently clamped down to this limit.
    /// Use this for providers whose models have a lower output ceiling than
    /// the default we request (e.g. DeepSeek Chat caps at 8 192).
    pub max_tokens_cap: Option<u32>,

    /// Set to `true` for providers that never require an API key (e.g.
    /// Ollama, LM Studio, llama.cpp).  When `true`, `health_check()` will
    /// always attempt a live network probe regardless of whether the base URL
    /// points to a local or remote host, instead of short-circuiting with
    /// "No API key configured".
    pub no_api_key_required: bool,

    /// When set, `discover_models()` uses Ollama's native `/api/tags` endpoint
    /// (and optionally `/api/show` for per-model metadata) instead of the
    /// OpenAI-compatible `/v1/models` endpoint.  The value is the Ollama host
    /// root (e.g. `"http://localhost:11434"`) so the native API can be called
    /// independently of the `/v1` base URL used for chat completions.
    pub ollama_native_host: Option<String>,

    /// Native LM Studio host used to discover loaded model instances and their
    /// configured context lengths from `/api/v1/models`.
    pub lm_studio_native_host: Option<String>,
}

impl Default for ProviderQuirks {
    fn default() -> Self {
        Self {
            streaming: true,
            tool_id_max_len: None,
            tool_id_alphanumeric_only: false,
            overflow_patterns: Vec::new(),
            include_usage_in_stream: false,
            default_temperature: None,
            fix_tool_user_sequence: false,
            reasoning_field: None,
            requires_reasoning_roundtrip: false,
            max_tokens_cap: None,
            no_api_key_required: false,
            ollama_native_host: None,
            lm_studio_native_host: None,
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAiCompatProvider
// ---------------------------------------------------------------------------

pub struct OpenAiCompatProvider {
    id: ProviderId,
    name: String,
    base_url: String,
    api_key: Option<String>,
    extra_headers: Vec<(String, String)>,
    quirks: ProviderQuirks,
    http_client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct LmStudioModelsResponse {
    #[serde(default)]
    models: Vec<LmStudioModel>,
}

#[derive(Debug, Deserialize)]
struct LmStudioModel {
    #[serde(rename = "type")]
    model_type: String,
    key: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    loaded_instances: Vec<LmStudioLoadedInstance>,
    #[serde(default)]
    max_context_length: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct LmStudioLoadedInstance {
    id: String,
    config: LmStudioInstanceConfig,
}

#[derive(Debug, Deserialize)]
struct LmStudioInstanceConfig {
    #[serde(default)]
    context_length: Option<u32>,
}

impl OpenAiCompatProvider {
    /// Create a new compat provider.  `base_url` should already include any
    /// path prefix (e.g. `"https://api.groq.com/openai/v1"`).
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(crate::request_timeout())
            .build()
            .expect("failed to build reqwest client");

        Self {
            id: ProviderId::new(id),
            name: name.into(),
            base_url: base_url.into(),
            api_key: None,
            extra_headers: Vec::new(),
            quirks: ProviderQuirks::default(),
            http_client,
        }
    }

    /// Set an API key that will be sent as `Authorization: Bearer <key>`.
    pub fn with_api_key(mut self, key: String) -> Self {
        self.api_key = if key.is_empty() { None } else { Some(key) };
        self
    }

    /// Append a custom header sent on every request.
    pub fn with_header(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    /// Apply provider-specific quirks.
    pub fn with_quirks(mut self, quirks: ProviderQuirks) -> Self {
        self.quirks = quirks;
        self
    }

    /// Override the base URL (e.g. from a user-supplied --api-base flag).
    ///
    /// Keep any native API host in sync with the OpenAI-compatible base URL.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        if self.quirks.ollama_native_host.is_some()
            || self.quirks.lm_studio_native_host.is_some()
        {
            let native_host = self
                .base_url
                .trim_end_matches('/')
                .trim_end_matches("/v1")
                .trim_end_matches('/')
                .to_string();
            if self.quirks.ollama_native_host.is_some() {
                self.quirks.ollama_native_host = Some(native_host.clone());
            }
            if self.quirks.lm_studio_native_host.is_some() {
                self.quirks.lm_studio_native_host = Some(native_host);
            }
        }
        self
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Returns `true` when the provider has no usable API key.
    fn has_no_key(&self) -> bool {
        self.api_key.is_none()
    }

    /// Scrub a tool-call ID according to the configured quirks.
    fn scrub_tool_id(&self, id: &str) -> String {
        let mut s = id.to_string();
        if self.quirks.tool_id_alphanumeric_only {
            s = s.chars().filter(|c| c.is_alphanumeric()).collect();
        }
        if let Some(max_len) = self.quirks.tool_id_max_len {
            let truncated: String = s.chars().take(max_len).collect();
            s = format!("{:0<width$}", truncated, width = max_len);
        }
        s
    }

    /// Apply `scrub_tool_id` to every tool-call id/tool_call_id in a messages
    /// array that was already built by `OpenAiProvider::to_openai_messages`.
    fn apply_tool_id_quirks(&self, messages: &mut [Value]) {
        if self.quirks.tool_id_max_len.is_none() && !self.quirks.tool_id_alphanumeric_only {
            return;
        }
        for msg in messages.iter_mut() {
            // assistant message tool_calls[].id
            if let Some(tcs) = msg.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
                for tc in tcs.iter_mut() {
                    if let Some(id_val) = tc.get("id").and_then(|v| v.as_str()) {
                        let scrubbed = self.scrub_tool_id(id_val);
                        if let Some(obj) = tc.as_object_mut() {
                            obj.insert("id".to_string(), json!(scrubbed));
                        }
                    }
                }
            }
            // tool message tool_call_id
            if let Some(id_val) = msg.get("tool_call_id").and_then(|v| v.as_str()) {
                let scrubbed = self.scrub_tool_id(id_val);
                if let Some(obj) = msg.as_object_mut() {
                    obj.insert("tool_call_id".to_string(), json!(scrubbed));
                }
            }
        }
    }

    /// Insert `{"role":"assistant","content":"Done."}` between any
    /// `role: tool` message that is immediately followed by a `role: user`
    /// message.
    fn apply_fix_tool_user_sequence(messages: &mut Vec<Value>) {
        let mut i = 0;
        while i + 1 < messages.len() {
            let current_is_tool = messages[i]
                .get("role")
                .and_then(|v| v.as_str())
                == Some("tool");
            let next_is_user = messages[i + 1]
                .get("role")
                .and_then(|v| v.as_str())
                == Some("user");

            if current_is_tool && next_is_user {
                messages.insert(
                    i + 1,
                    json!({ "role": "assistant", "content": "Done." }),
                );
                i += 2; // skip past the inserted message and the user message
            } else {
                i += 1;
            }
        }
    }

    /// Build the full messages array, applying all quirks.
    fn build_messages(&self, request: &ProviderRequest) -> Vec<Value> {
        let mut messages = OpenAiProvider::to_openai_messages_pub(
            &request.messages,
            request.system_prompt.as_ref(),
        );

        self.apply_tool_id_quirks(&mut messages);

        if self.quirks.fix_tool_user_sequence {
            Self::apply_fix_tool_user_sequence(&mut messages);
        }

        // For providers that require reasoning_content in multi-turn conversations
        // (e.g. DeepSeek V4), inject reasoning text back into assistant messages
        // that contain tool calls. Non-tool-call turns omit the field to save tokens.
        // Only providers with requires_reasoning_roundtrip=true need this.
        if self.quirks.requires_reasoning_roundtrip {
            if let Some(ref field) = self.quirks.reasoning_field {
                Self::inject_reasoning_for_tool_turns(
                    &mut messages,
                    &request.messages,
                    field,
                );
            }
        }

        // Some providers (DeepSeek when reasoning_roundtrip enabled, Ollama) reject
        // `content: null` on assistant messages — replace with an empty string.
        if self.quirks.requires_reasoning_roundtrip || self.quirks.no_api_key_required {
            Self::ensure_content_not_null(&mut messages);
        }

        messages
    }

    /// For providers that expose a reasoning field, inject the reasoning
    /// text into assistant messages that contain tool calls.
    ///
    /// DeepSeek's thinking mode requires `reasoning_content` to be sent back
    /// on turns where tool calls occurred. Turns without tool calls omit it —
    /// the API ignores it anyway and skipping saves tokens.
    fn inject_reasoning_for_tool_turns(
        json_messages: &mut [Value],
        original_messages: &[claurst_core::types::Message],
        field: &str,
    ) {
        use claurst_core::types::{MessageContent, Role};

        // Collect reasoning texts from assistant messages that have both
        // Thinking blocks and ToolUse blocks, preserving order.
        let reasoning_texts: Vec<String> = original_messages
            .iter()
            .filter_map(|msg| {
                if msg.role != Role::Assistant {
                    return None;
                }
                let blocks = match &msg.content {
                    MessageContent::Blocks(b) => b,
                    _ => return None,
                };
                let has_tool_use = blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
                if !has_tool_use {
                    return None;
                }
                let thinking: Vec<&str> = blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                        _ => None,
                    })
                    .collect();
                if thinking.is_empty() {
                    None
                } else {
                    Some(thinking.join(""))
                }
            })
            .collect();

        if reasoning_texts.is_empty() {
            return;
        }

        // Inject into JSON messages: for each assistant message that carries
        // tool_calls, add the reasoning field from the collected texts.
        let mut reasoning_idx = 0;
        for msg in json_messages.iter_mut() {
            if reasoning_idx >= reasoning_texts.len() {
                break;
            }
            let is_assistant =
                msg.get("role").and_then(|r| r.as_str()) == Some("assistant");
            let has_tool_calls = msg
                .get("tool_calls")
                .and_then(|tc| tc.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            if is_assistant && has_tool_calls {
                if let Some(obj) = msg.as_object_mut() {
                    obj.insert(
                        field.to_string(),
                        Value::String(reasoning_texts[reasoning_idx].clone()),
                    );
                }
                reasoning_idx += 1;
            }
        }
    }

    /// Replace `content: null` with `content: ""` on all assistant messages.
    ///
    /// DeepSeek's API rejects assistant messages that have `content: null`
    /// (it treats null as absent and then complains that neither content nor
    /// tool_calls is set).  Replacing with an empty string satisfies the
    /// validation while preserving semantics.
    fn ensure_content_not_null(messages: &mut [Value]) {
        for msg in messages.iter_mut() {
            let is_assistant =
                msg.get("role").and_then(|r| r.as_str()) == Some("assistant");
            if !is_assistant {
                continue;
            }
            if let Some(obj) = msg.as_object_mut() {
                if let Some(content) = obj.get("content") {
                    if content.is_null() {
                        obj.insert("content".to_string(), Value::String(String::new()));
                    }
                }
            }
        }
    }

    /// Resolve the temperature to use: request value takes priority, then
    /// the quirk default, then nothing (let the API default apply).
    fn resolve_temperature(&self, request: &ProviderRequest) -> Option<f64> {
        request.temperature.or(self.quirks.default_temperature)
    }

    /// Attach the authorization header if an API key is configured.
    fn apply_auth(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        if let Some(key) = &self.api_key {
            builder.header("Authorization", format!("Bearer {}", key))
        } else {
            builder
        }
    }

    /// Attach all configured extra headers.
    fn apply_extra_headers(
        &self,
        mut builder: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        for (name, value) in &self.extra_headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder
    }

    fn map_http_error(&self, status: u16, body: &str) -> ProviderError {
        parse_error_response(status, body, &self.id)
    }

    // -----------------------------------------------------------------------
    // Non-streaming
    // -----------------------------------------------------------------------

    async fn create_message_non_streaming(
        &self,
        request: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let messages = self.build_messages(request);
        let tools = OpenAiProvider::to_openai_tools_pub(&request.tools);

        let max_tokens = match self.quirks.max_tokens_cap {
            Some(cap) => request.max_tokens.min(cap),
            None => request.max_tokens,
        };
        let mut body = json!({
            "model": request.model,
            "max_tokens": max_tokens,
            "messages": messages,
            "stream": false,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(t) = self.resolve_temperature(request) {
            body["temperature"] = json!(t);
        }
        if let Some(p) = request.top_p {
            body["top_p"] = json!(p);
        }
        if !request.stop_sequences.is_empty() {
            body["stop"] = json!(request.stop_sequences);
        }
        merge_openai_compatible_options(&mut body, &request.provider_options);

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let builder = self
            .http_client
            .post(&url)
            .header("Content-Type", "application/json");
        let builder = self.apply_auth(builder);
        let builder = self.apply_extra_headers(builder);

        let resp = builder
            .json(&body)
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

        let json: Value =
            serde_json::from_str(&text).map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("Failed to parse response JSON: {}", e),
                status: Some(status),
                body: Some(text.clone()),
            })?;

        OpenAiProvider::parse_non_streaming_response_pub(&json, &self.id)
    }

    // -----------------------------------------------------------------------
    // Streaming
    // -----------------------------------------------------------------------

    async fn do_streaming(
        &self,
        request: &ProviderRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let messages = self.build_messages(request);
        let tools = OpenAiProvider::to_openai_tools_pub(&request.tools);

        let max_tokens = match self.quirks.max_tokens_cap {
            Some(cap) => request.max_tokens.min(cap),
            None => request.max_tokens,
        };
        let mut body = json!({
            "model": request.model,
            "max_tokens": max_tokens,
            "messages": messages,
            "stream": true,
        });

        if self.quirks.include_usage_in_stream {
            body["stream_options"] = json!({ "include_usage": true });
        }

        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(t) = self.resolve_temperature(request) {
            body["temperature"] = json!(t);
        }
        if let Some(p) = request.top_p {
            body["top_p"] = json!(p);
        }
        if !request.stop_sequences.is_empty() {
            body["stop"] = json!(request.stop_sequences);
        }
        merge_openai_compatible_options(&mut body, &request.provider_options);

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let builder = self
            .http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream");
        let builder = self.apply_auth(builder);
        let builder = self.apply_extra_headers(builder);

        let resp = builder
            .json(&body)
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

    // -----------------------------------------------------------------------
    // Ollama native model discovery
    // -----------------------------------------------------------------------

    /// List models using Ollama's native `/api/tags` endpoint, then enrich
    /// each model with metadata from `/api/show` (context window, parameter
    /// size, quantization level).
    ///
    /// Models are sorted with coding-oriented models first (names containing
    /// "code" or "coder"), then by parameter size descending, so the best
    /// local coding model naturally appears at the top.
    async fn discover_models_ollama_native(
        &self,
        ollama_host: &str,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let tags_url = format!("{}/api/tags", ollama_host.trim_end_matches('/'));

        let resp = self.http_client.get(&tags_url).send().await.map_err(|e| {
            ProviderError::Other {
                provider: self.id.clone(),
                message: format!("Ollama /api/tags request failed: {}", e),
                status: None,
                body: None,
            }
        })?;

        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to read /api/tags response: {}", e),
            status: Some(status),
            body: None,
        })?;

        if !(200..300).contains(&(status as usize)) {
            return Err(self.map_http_error(status, &text));
        }

        let json: Value = serde_json::from_str(&text).map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to parse /api/tags JSON: {}", e),
            status: Some(status),
            body: Some(text),
        })?;

        let models_arr = match json.get("models").and_then(|m| m.as_array()) {
            Some(m) => m,
            None => return Ok(vec![]),
        };

        // Collect model names from /api/tags.
        let model_names: Vec<String> = models_arr
            .iter()
            .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();

        // Fetch detailed metadata for each model via /api/show.
        let show_url_base = format!("{}/api/show", ollama_host.trim_end_matches('/'));
        let provider_id = self.id.clone();

        let mut models: Vec<(ModelInfo, bool, u64)> = Vec::with_capacity(model_names.len());

        for name in &model_names {
            let (context_window, max_output, is_coder, param_size) =
                self.fetch_ollama_model_info(&show_url_base, name).await;

            models.push((
                ModelInfo {
                    id: ModelId::new(name.as_str()),
                    provider_id: provider_id.clone(),
                    name: Self::ollama_display_name(name),
                    context_window,
                    max_output_tokens: max_output,
                    ..Default::default()
                },
                is_coder,
                param_size,
            ));
        }

        // Sort: coding models first, then by parameter size descending.
        models.sort_by(|a, b| {
            b.1.cmp(&a.1) // coders first
                .then_with(|| b.2.cmp(&a.2)) // larger models first
        });

        Ok(models.into_iter().map(|(info, _, _)| info).collect())
    }

    /// Return loaded LM Studio LLM instances. `None` means the native endpoint
    /// is unavailable, so the caller should fall back to `/v1/models`.
    async fn discover_models_lm_studio_native(
        &self,
        lm_studio_host: &str,
    ) -> Option<Vec<ModelInfo>> {
        let url = format!("{}/api/v1/models", lm_studio_host.trim_end_matches('/'));
        let builder = self.apply_auth(self.http_client.get(url));
        let response = self.apply_extra_headers(builder).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let response: LmStudioModelsResponse = response.json().await.ok()?;
        Some(Self::parse_lm_studio_native_models(response, &self.id))
    }

    fn parse_lm_studio_native_models(
        response: LmStudioModelsResponse,
        provider_id: &ProviderId,
    ) -> Vec<ModelInfo> {
        let mut discovered = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for model in response.models {
            if model.model_type != "llm" {
                continue;
            }
            let display_name = model.display_name.unwrap_or(model.key);
            let max_context = model.max_context_length.unwrap_or(128_000);

            for instance in model.loaded_instances {
                if !seen.insert(instance.id.clone()) {
                    continue;
                }
                let context_window = instance
                    .config
                    .context_length
                    .unwrap_or(max_context);
                discovered.push(ModelInfo {
                    id: ModelId::new(instance.id),
                    provider_id: provider_id.clone(),
                    name: display_name.clone(),
                    context_window,
                    max_output_tokens: (context_window / 2).min(16_384),
                    ..Default::default()
                });
            }
        }
        discovered
    }

    /// Call `/api/show` for a single model to extract its actual context
    /// window, parameter count, and whether it's coding-oriented.
    ///
    /// Returns `(context_window, max_output_tokens, is_coder, param_size_bytes)`.
    /// Falls back to sensible defaults if the request fails.
    async fn fetch_ollama_model_info(
        &self,
        show_url: &str,
        model_name: &str,
    ) -> (u32, u32, bool, u64) {
        let default_ctx = 4_096u32;
        let default_out = 2_048u32;
        let lower = model_name.to_lowercase();
        let is_coder_by_name = lower.contains("code")
            || lower.contains("coder")
            || lower.contains("codestral")
            || lower.contains("starcoder")
            || lower.contains("deepseek-coder")
            || lower.contains("qwen2.5-coder");

        let body = serde_json::json!({ "name": model_name });
        let resp = match self.http_client.post(show_url).json(&body).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => return (default_ctx, default_out, is_coder_by_name, 0),
        };

        let json: Value = match resp.json().await {
            Ok(j) => j,
            Err(_) => return (default_ctx, default_out, is_coder_by_name, 0),
        };

        // Extract parameter size from model_info.
        let param_size = json
            .get("model_info")
            .and_then(|mi| {
                mi.get("general.parameter_count")
                    .and_then(|v| v.as_u64())
            })
            .unwrap_or(0);

        // Extract num_ctx from the modelfile parameters or model_info.
        let num_ctx = Self::extract_num_ctx(&json).unwrap_or(default_ctx);

        // Max output is typically a fraction of context window for local
        // models.  Use half the context or 4096, whichever is smaller.
        let max_output = std::cmp::min(num_ctx / 2, 4_096);

        // Check if the model family or template indicates coding capability.
        let family = json
            .get("model_info")
            .and_then(|mi| mi.get("general.basename").and_then(|v| v.as_str()))
            .unwrap_or("");
        let is_coder = is_coder_by_name
            || family.contains("code")
            || family.contains("coder");

        (num_ctx, max_output, is_coder, param_size)
    }

    /// Extract `num_ctx` (context window) from the `/api/show` response.
    ///
    /// Ollama stores this in the modelfile parameters string (e.g.
    /// `"num_ctx 32768"`) or in `model_info` under context-length keys.
    fn extract_num_ctx(json: &Value) -> Option<u32> {
        // 1. Check model_info for context length keys.
        if let Some(mi) = json.get("model_info") {
            for key in &[
                "llama.context_length",
                "qwen2.context_length",
                "gemma.context_length",
                "gemma2.context_length",
                "phi3.context_length",
                "mistral.context_length",
                "starcoder2.context_length",
                "deepseek2.context_length",
                "command-r.context_length",
                "granite.context_length",
            ] {
                if let Some(v) = mi.get(*key).and_then(|v| v.as_u64()) {
                    return Some(v as u32);
                }
            }

            // Fallback: scan all keys ending in ".context_length"
            if let Some(obj) = mi.as_object() {
                for (k, v) in obj {
                    if k.ends_with(".context_length") {
                        if let Some(n) = v.as_u64() {
                            return Some(n as u32);
                        }
                    }
                }
            }
        }

        // 2. Parse from the modelfile parameters string.
        if let Some(params) = json.get("parameters").and_then(|p| p.as_str()) {
            for line in params.lines() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix("num_ctx") {
                    if let Ok(n) = rest.trim().parse::<u32>() {
                        return Some(n);
                    }
                }
            }
        }

        None
    }

    /// Produce a human-readable display name from an Ollama model name.
    ///
    /// `"qwen2.5-coder:32b-instruct-q4_K_M"` → `"Qwen 2.5 Coder (32B, Q4_K_M)"`
    fn ollama_display_name(raw: &str) -> String {
        let (base, tag) = raw.split_once(':').unwrap_or((raw, "latest"));

        let pretty_base = base
            .replace(['-', '_'], " ")
            .split_whitespace()
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    None => String::new(),
                    Some(c) => {
                        let upper: String = c.to_uppercase().collect();
                        format!("{}{}", upper, chars.as_str())
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ");

        if tag == "latest" {
            return pretty_base;
        }

        let tag_parts: Vec<&str> = tag.split('-').collect();
        let mut size_part = None;
        let mut quant_part = None;
        for part in &tag_parts {
            let lower = part.to_lowercase();
            if lower.ends_with('b') && lower.trim_end_matches('b').parse::<f64>().is_ok() {
                size_part = Some(part.to_uppercase());
            } else if lower.starts_with('q') && lower.len() > 1 {
                quant_part = Some(part.to_uppercase());
            }
        }

        match (size_part, quant_part) {
            (Some(s), Some(q)) => format!("{} ({}, {})", pretty_base, s, q),
            (Some(s), None) => format!("{} ({})", pretty_base, s),
            (None, Some(q)) => format!("{} ({})", pretty_base, q),
            (None, None) => format!("{} ({})", pretty_base, tag),
        }
    }
}

// ---------------------------------------------------------------------------
// LlmProvider impl
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        if self.has_no_key() {
            // Providers that have no key set are considered unconfigured.
            // We allow the call to proceed in case the provider genuinely needs
            // no auth (e.g. Ollama), but callers that gate on health_check()
            // will see Unavailable first.
        }
        self.create_message_non_streaming(&request).await
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let resp = self.do_streaming(&request).await?;
        let provider_id = self.id.clone();
        let reasoning_field = self.quirks.reasoning_field.clone();

        let s = stream! {
            use futures::StreamExt;

            let mut byte_stream = resp.bytes_stream();
            // Byte-buffering line decoder (#228): complete UTF-8 lines only, so
            // a multibyte codepoint straddling a chunk boundary is never lost.
            let mut byte_decoder = crate::SseByteDecoder::new();
            // Sans-IO OpenAI-Chat protocol decoder (#228): owns all message /
            // reasoning / tool-call / finish-reason decoding for this wire
            // format (extracted from this loop into `protocol::openai_chat`).
            let mut chat_decoder = crate::OpenAiChatDecoder::new(reasoning_field);

            // Bound infinite mid-stream stalls (issue #185): some
            // OpenAI-compatible providers begin a streamed tool call and then
            // pause indefinitely before sending the arguments. Wrap each chunk
            // read in a generous idle timeout so a stall surfaces as an error
            // instead of hanging forever. Each chunk resets the timer, so
            // slow-but-progressing local models are never cut off.
            let idle_timeout = crate::stream_idle_timeout();
            loop {
                let chunk_result = match tokio::time::timeout(
                    idle_timeout,
                    byte_stream.next(),
                )
                .await
                {
                    Ok(Some(chunk_result)) => chunk_result,
                    // Stream ended normally.
                    Ok(None) => break,
                    // No bytes for `idle_timeout` — provider stalled mid-stream.
                    Err(_) => {
                        yield Err(ProviderError::StreamError {
                            provider: provider_id.clone(),
                            message: format!(
                                "Stream stalled: no data received for {}s; aborting to avoid hanging",
                                idle_timeout.as_secs()
                            ),
                            partial_response: None,
                        });
                        return;
                    }
                };
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

                for line in byte_decoder.push(&chunk) {
                    let mut events = Vec::new();
                    let stop = chat_decoder.feed_line(&line, &mut events);
                    for event in events {
                        yield Ok(event);
                    }
                    if stop {
                        return;
                    }
                }
            }

            // Byte stream ended: flush a trailing MessageStop if content began
            // but no explicit `[DONE]` sentinel arrived.
            let mut tail = Vec::new();
            chat_decoder.finish(&mut tail);
            for event in tail {
                yield Ok(event);
            }
        };

        Ok(Box::pin(s))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        // Use Ollama native API when configured — provides richer metadata
        // (parameter size, quantization, actual context window) than the
        // generic OpenAI-compat /v1/models endpoint.
        if let Some(ref ollama_host) = self.quirks.ollama_native_host {
            return self.discover_models_ollama_native(ollama_host).await;
        }
        if let Some(ref lm_studio_host) = self.quirks.lm_studio_native_host {
            if let Some(models) = self
                .discover_models_lm_studio_native(lm_studio_host)
                .await
            {
                return Ok(models);
            }
        }

        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let builder = self.http_client.get(&url);
        let builder = self.apply_auth(builder);
        let builder = self.apply_extra_headers(builder);

        let resp = builder.send().await.map_err(|e| ProviderError::Other {
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

        let json: Value =
            serde_json::from_str(&text).map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("Failed to parse models JSON: {}", e),
                status: Some(status),
                body: Some(text),
            })?;

        let data = match json.get("data").and_then(|d| d.as_array()) {
            Some(d) => d,
            None => return Ok(vec![]),
        };

        let provider_id = self.id.clone();
        let models: Vec<ModelInfo> = data
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(|v| v.as_str())?;
                Some(ModelInfo {
                    id: ModelId::new(id),
                    provider_id: provider_id.clone(),
                    name: id.to_string(),
                    context_window: match id {
                        "gpt-5" | "gpt-5.4" | "gpt-5.2" | "gpt-5-mini" | "gpt-5-nano"
                        | "gpt-5-chat-latest"
                        | "gpt-5.2-codex" | "gpt-5.1-codex" | "gpt-5.1-codex-mini"
                        | "gpt-5.1-codex-max" => 400_000,
                        "o3" | "o3-mini" | "o4-mini" => 200_000,
                        _ => 128_000,
                    },
                    max_output_tokens: 16_384,
                    ..Default::default()
                })
            })
            .collect();

        Ok(models)
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        // Providers that need an API key but have none configured are
        // immediately unavailable without making a network call.
        if self.has_no_key() {
            // Providers that never require an API key (Ollama, LM Studio,
            // llama.cpp) should always proceed to the live health probe,
            // regardless of whether the base URL is local or remote.  This
            // allows remote/VPS-hosted instances to be used without a key.
            //
            // For all other providers a missing key means the env var was
            // absent or empty; report that without making a network call,
            // distinguishing only by URL when the quirk is not set.
            if !self.quirks.no_api_key_required {
                let is_local = self.base_url.contains("localhost")
                    || self.base_url.contains("127.0.0.1")
                    || self.base_url.contains("::1");

                if !is_local {
                    return Ok(ProviderStatus::Unavailable {
                        reason: "No API key configured".to_string(),
                    });
                }
            }
        }

        // For Ollama, prefer the native `/api/tags` endpoint over the
        // OpenAI-compatible `/v1/models` one — older Ollama versions do not
        // expose `/v1/models` and would return 404.
        let url = if let Some(ref host) = self.quirks.ollama_native_host {
            format!("{}/api/tags", host.trim_end_matches('/'))
        } else {
            format!("{}/models", self.base_url.trim_end_matches('/'))
        };
        let builder = self.http_client.get(&url);
        let builder = self.apply_auth(builder);
        let builder = self.apply_extra_headers(builder);

        match builder.send().await {
            Ok(r) if r.status().is_success() => Ok(ProviderStatus::Healthy),
            Ok(r) => Ok(ProviderStatus::Unavailable {
                reason: format!("models endpoint returned {}", r.status()),
            }),
            Err(e) => Ok(ProviderStatus::Unavailable {
                reason: e.to_string(),
            }),
        }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: self.quirks.streaming,
            tool_calling: true,
            thinking: self.quirks.reasoning_field.is_some(),
            image_input: true,
            pdf_input: false,
            audio_input: false,
            video_input: false,
            caching: false,
            structured_output: true,
            system_prompt_style: SystemPromptStyle::SystemMessage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mistral_tool_ids_match_opencode_style() {
        let provider = OpenAiCompatProvider::new("mistral", "Mistral", "https://example.com")
            .with_quirks(ProviderQuirks {
                tool_id_max_len: Some(9),
                tool_id_alphanumeric_only: true,
                ..Default::default()
            });

        assert_eq!(provider.scrub_tool_id("call-123456789abc"), "call12345");
        assert_eq!(provider.scrub_tool_id("x"), "x00000000");
    }

    #[test]
    fn fix_tool_user_sequence_inserts_done_between_tool_and_user() {
        let mut messages = vec![
            json!({"role": "tool", "tool_call_id": "call_1", "content": "ok"}),
            json!({"role": "user", "content": "continue"}),
        ];

        OpenAiCompatProvider::apply_fix_tool_user_sequence(&mut messages);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], json!("assistant"));
        assert_eq!(messages[1]["content"], json!("Done."));
    }

    #[test]
    fn with_base_url_retargets_ollama_native_host() {
        // Mirror the ollama() factory default: both the /v1 base URL and the
        // native host point at localhost initially.
        let provider = OpenAiCompatProvider::new("ollama", "Ollama", "http://localhost:11434/v1")
            .with_quirks(ProviderQuirks {
                no_api_key_required: true,
                ollama_native_host: Some("http://localhost:11434".to_string()),
                ..Default::default()
            });

        // Overriding the base URL with a configured remote api_base (as the
        // registry does for `providers.ollama.api_base`) must also retarget the
        // native host used by health_check() and native model discovery.
        let provider = provider.with_base_url("http://192.0.2.10:11434/v1");

        assert_eq!(provider.base_url, "http://192.0.2.10:11434/v1");
        assert_eq!(
            provider.quirks.ollama_native_host.as_deref(),
            Some("http://192.0.2.10:11434"),
        );
    }

    #[test]
    fn lm_studio_native_models_only_include_loaded_llms() {
        let response = json!({
            "models": [
                {
                    "type": "llm",
                    "key": "qwen/unloaded",
                    "display_name": "Unloaded Qwen",
                    "loaded_instances": [],
                    "max_context_length": 262144
                },
                {
                    "type": "llm",
                    "key": "organization/omnicoder-9b",
                    "display_name": "Omnicoder 9B",
                    "loaded_instances": [{
                        "id": "omnicoder-9b",
                        "config": { "context_length": 131072 }
                    }],
                    "max_context_length": 262144
                },
                {
                    "type": "embedding",
                    "key": "nomic-embed",
                    "loaded_instances": [{
                        "id": "nomic-embed",
                        "config": { "context_length": 2048 }
                    }]
                }
            ]
        });

        let response: LmStudioModelsResponse =
            serde_json::from_value(response).expect("valid LM Studio fixture");
        let models = OpenAiCompatProvider::parse_lm_studio_native_models(
            response,
            &ProviderId::new(ProviderId::LM_STUDIO),
        );

        assert_eq!(models.len(), 1);
        assert_eq!(&*models[0].id, "omnicoder-9b");
        assert_eq!(models[0].name, "Omnicoder 9B");
        assert_eq!(models[0].context_window, 131_072);
    }

    #[test]
    fn with_base_url_retargets_lm_studio_native_host() {
        let provider = OpenAiCompatProvider::new(
            ProviderId::LM_STUDIO,
            "LM Studio",
            "http://localhost:1234/v1",
        )
        .with_quirks(ProviderQuirks {
            include_usage_in_stream: true,
            lm_studio_native_host: Some("http://localhost:1234".to_string()),
            ..Default::default()
        })
        .with_base_url("http://192.0.2.20:1234/v1");

        assert_eq!(
            provider.quirks.lm_studio_native_host.as_deref(),
            Some("http://192.0.2.20:1234"),
        );
        assert!(provider.quirks.include_usage_in_stream);
    }
}
