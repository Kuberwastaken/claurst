// providers/free.rs — Composite "Free" provider.
//
// Stacks multiple upstream free-tier providers behind a single
// `free/auto` synthetic model id. The chain is iterated in priority
// order on every request — if an upstream fails (auth, rate limit,
// server error, request error) *before* any data has been streamed,
// the same request is retried against the next upstream. Mid-stream
// failures are surfaced as-is; we don't replay partial conversations.
//
// Inspired by https://github.com/tashfeenahmed/freellmapi — the same
// "aggregate the free tiers from many providers behind one OpenAI-
// compatible endpoint" idea, ported into claurst's native provider
// trait.
//
// Routing:
//   * `free` / `free/auto` / `auto`  →  try each configured upstream
//     in catalog order, using that upstream's `default_model`.
//   * `<upstream_id>/<rest>`         →  pin that upstream, then
//     fall through to the rest of the chain on transient errors.
//   * anything else                  →  passed through verbatim
//     to the first upstream in the chain.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use claurst_core::provider_id::{ModelId, ProviderId};
use futures::Stream;

use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StreamEvent,
    SystemPromptStyle,
};

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// One upstream provider in the free-mode chain.
///
/// `id` is the canonical claurst `ProviderId` string — the auth store key the
/// dialog writes to, and the prefix the user types for `<id>/<model>` pinning.
#[derive(Debug, Clone, Copy)]
pub struct FreeUpstream {
    pub id: &'static str,
    pub title: &'static str,
    pub key_url: &'static str,
    pub default_model: &'static str,
    pub note: &'static str,
}

/// Ordered priority of providers we stack into Free mode. Order matters —
/// `free/auto` tries each in turn, so put the fastest / most generous tiers
/// first. Mirrors the priority list in freellmapi's router.
pub const FREE_CATALOG: &[FreeUpstream] = &[
    FreeUpstream {
        id: "groq",
        title: "Groq",
        key_url: "console.groq.com/keys",
        default_model: "llama-3.3-70b-versatile",
        note: "fast — Llama 3.3, GPT-OSS, Qwen3",
    },
    FreeUpstream {
        id: "cerebras",
        title: "Cerebras",
        key_url: "cloud.cerebras.ai",
        default_model: "qwen-3-235b-a22b-instruct-2507",
        note: "wafer-scale — Qwen3 235B",
    },
    FreeUpstream {
        id: "google",
        title: "Google Gemini",
        key_url: "aistudio.google.com/app/apikey",
        default_model: "gemini-2.5-flash",
        note: "Gemini 2.5 Flash",
    },
    FreeUpstream {
        id: "mistral",
        title: "Mistral",
        key_url: "console.mistral.ai/api-keys",
        default_model: "mistral-large-latest",
        note: "Large · Medium · Codestral · Devstral",
    },
    FreeUpstream {
        id: "sambanova",
        title: "SambaNova",
        key_url: "cloud.sambanova.ai",
        default_model: "Meta-Llama-3.3-70B-Instruct",
        note: "DeepSeek V3 · Llama 4 · Gemma 3",
    },
    FreeUpstream {
        id: "nvidia",
        title: "NVIDIA NIM",
        key_url: "build.nvidia.com",
        default_model: "meta/llama-3.3-70b-instruct",
        note: "NIM endpoints (trial)",
    },
    FreeUpstream {
        id: "cohere",
        title: "Cohere",
        key_url: "dashboard.cohere.com/api-keys",
        default_model: "command-r-plus",
        note: "Command R+ (trial)",
    },
    FreeUpstream {
        id: "openrouter",
        title: "OpenRouter",
        key_url: "openrouter.ai/keys",
        default_model: "openrouter/free",
        note: "19 free-tier models — $10 top-up lifts caps",
    },
    FreeUpstream {
        id: "opencode-zen",
        title: "OpenCode Zen",
        key_url: "opencode.ai/auth",
        default_model: "minimax-m2.5-free",
        note: "MiniMax M2.5 · Big Pickle · Ring 2.6",
    },
    FreeUpstream {
        id: "zai",
        title: "Z.AI",
        key_url: "z.ai/manage-apikey/apikey-list",
        default_model: "glm-4.6",
        note: "GLM-4.6 · GLM-4.7",
    },
    FreeUpstream {
        id: "zhipuai",
        title: "Zhipu",
        key_url: "open.bigmodel.cn",
        default_model: "glm-4.5",
        note: "GLM-4.5 (CN endpoint)",
    },
];

/// Look up a catalog entry by its `id`.
pub fn catalog_entry(id: &str) -> Option<&'static FreeUpstream> {
    FREE_CATALOG.iter().find(|e| e.id == id)
}

// ---------------------------------------------------------------------------
// FreeProvider
// ---------------------------------------------------------------------------

/// One configured entry in a [`FreeProvider`]'s chain.
pub struct FreeEntry {
    pub upstream: FreeUpstream,
    pub provider: Arc<dyn LlmProvider>,
}

/// Composite provider that stacks free-tier upstreams behind a single
/// `free/auto` model id.
pub struct FreeProvider {
    id: ProviderId,
    chain: Vec<FreeEntry>,
}

#[derive(Debug)]
enum Route {
    /// Try every entry in order, substituting its `default_model`.
    Auto,
    /// Try the entry at `start_idx` first (with `pinned_model`), then fall
    /// through to the remaining entries in catalog order.
    Pinned {
        start_idx: usize,
        pinned_model: String,
    },
}

impl FreeProvider {
    pub fn new(chain: Vec<FreeEntry>) -> Self {
        Self {
            id: ProviderId::new(ProviderId::FREE),
            chain,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.chain.is_empty()
    }

    pub fn chain_len(&self) -> usize {
        self.chain.len()
    }

    /// Decide how to route a user-facing model id into the chain.
    fn resolve_route(&self, model: &str) -> Route {
        let trimmed = model.trim();
        if trimmed.is_empty()
            || trimmed == "free"
            || trimmed == "auto"
            || trimmed == "free/auto"
        {
            return Route::Auto;
        }

        // Legacy alias: `zen/...` was the old Free-mode pin prefix.
        let normalized: String = if let Some(rest) = trimmed.strip_prefix("zen/") {
            format!("opencode-zen/{}", rest)
        } else {
            trimmed.to_string()
        };

        // Find a chain entry whose id is a prefix.
        for (idx, entry) in self.chain.iter().enumerate() {
            let prefix = format!("{}/", entry.upstream.id);
            if let Some(rest) = normalized.strip_prefix(&prefix) {
                // OpenRouter is unusual: its model ids are themselves
                // `vendor/model` strings (e.g. `meta-llama/llama-3-8b:free`)
                // and the free-pool router model is literally `openrouter/free`.
                // Pass the post-prefix portion through; for OpenRouter's
                // built-in free router we restore the full id.
                let pinned_model = if entry.upstream.id == "openrouter"
                    && (rest == "free" || rest == "auto" || rest.is_empty())
                {
                    "openrouter/free".to_string()
                } else {
                    rest.to_string()
                };
                return Route::Pinned {
                    start_idx: idx,
                    pinned_model,
                };
            }
        }

        // No prefix matched — treat as a raw model id for the first upstream.
        Route::Auto
    }

    /// Build the per-attempt (provider, model) sequence for a given request.
    fn attempt_plan(&self, route: &Route) -> Vec<(usize, String)> {
        match route {
            Route::Auto => self
                .chain
                .iter()
                .enumerate()
                .map(|(idx, entry)| (idx, entry.upstream.default_model.to_string()))
                .collect(),
            Route::Pinned {
                start_idx,
                pinned_model,
            } => {
                let mut plan = Vec::with_capacity(self.chain.len());
                plan.push((*start_idx, pinned_model.clone()));
                for (idx, entry) in self.chain.iter().enumerate() {
                    if idx == *start_idx {
                        continue;
                    }
                    plan.push((idx, entry.upstream.default_model.to_string()));
                }
                plan
            }
        }
    }

    fn should_fallback(err: &ProviderError) -> bool {
        // Don't fall back on user-fixable problems — they would behave the
        // same on every upstream.
        !matches!(
            err,
            ProviderError::InvalidRequest { .. } | ProviderError::ContentFiltered { .. }
        )
    }
}

#[async_trait]
impl LlmProvider for FreeProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "Free (multi-provider)"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        if self.chain.is_empty() {
            return Err(ProviderError::AuthFailed {
                provider: self.id.clone(),
                message: "Free mode has no configured upstreams — add at least one API key via /connect."
                    .to_string(),
            });
        }

        let route = self.resolve_route(&request.model);
        let plan = self.attempt_plan(&route);
        let mut last_err: Option<ProviderError> = None;

        for (idx, upstream_model) in plan {
            let entry = &self.chain[idx];
            let mut req = request.clone();
            req.model = upstream_model;
            match entry.provider.create_message(req).await {
                Ok(resp) => return Ok(resp),
                Err(err) if Self::should_fallback(&err) => {
                    tracing::warn!(
                        "FreeProvider: {} failed: {} — trying next upstream",
                        entry.upstream.id,
                        err,
                    );
                    last_err = Some(err);
                    continue;
                }
                Err(err) => return Err(err),
            }
        }

        Err(last_err.unwrap_or_else(|| ProviderError::ServerError {
            provider: self.id.clone(),
            status: None,
            message: "all free-mode upstreams exhausted".to_string(),
            is_retryable: false,
        }))
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        if self.chain.is_empty() {
            return Err(ProviderError::AuthFailed {
                provider: self.id.clone(),
                message: "Free mode has no configured upstreams — add at least one API key via /connect."
                    .to_string(),
            });
        }

        let route = self.resolve_route(&request.model);
        let plan = self.attempt_plan(&route);
        let mut last_err: Option<ProviderError> = None;

        for (idx, upstream_model) in plan {
            let entry = &self.chain[idx];
            let mut req = request.clone();
            req.model = upstream_model;
            match entry.provider.create_message_stream(req).await {
                Ok(stream) => return Ok(stream),
                Err(err) if Self::should_fallback(&err) => {
                    tracing::warn!(
                        "FreeProvider: {} stream failed: {} — trying next upstream",
                        entry.upstream.id,
                        err,
                    );
                    last_err = Some(err);
                    continue;
                }
                Err(err) => return Err(err),
            }
        }

        Err(last_err.unwrap_or_else(|| ProviderError::ServerError {
            provider: self.id.clone(),
            status: None,
            message: "all free-mode upstreams exhausted".to_string(),
            is_retryable: false,
        }))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let provider_id = self.id.clone();
        let mk = |id: &str, name: &str, ctx: u32| ModelInfo {
            id: ModelId::new(id),
            provider_id: provider_id.clone(),
            name: name.to_string(),
            context_window: ctx,
            max_output_tokens: 8_192,
            ..Default::default()
        };

        let mut models = vec![mk(
            "free/auto",
            "Free \u{2014} Auto (round-robin across configured providers)",
            200_000,
        )];

        for entry in &self.chain {
            let label = format!("{} \u{2014} {}", entry.upstream.title, entry.upstream.default_model);
            models.push(mk(
                &format!("{}/{}", entry.upstream.id, entry.upstream.default_model),
                &label,
                128_000,
            ));
        }

        Ok(models)
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        // Healthy as long as any upstream is reachable.
        let mut last: Result<ProviderStatus, ProviderError> = Ok(ProviderStatus::Unavailable {
            reason: "no upstreams configured".to_string(),
        });
        for entry in &self.chain {
            let res = entry.provider.health_check().await;
            if matches!(res, Ok(ProviderStatus::Healthy)) {
                return res;
            }
            last = res;
        }
        last
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            thinking: false,
            image_input: false,
            pdf_input: false,
            audio_input: false,
            video_input: false,
            caching: false,
            structured_output: false,
            system_prompt_style: SystemPromptStyle::SystemMessage,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::types::{Message, UsageInfo};

    use crate::provider_types::StopReason;

    struct StubProvider {
        id: ProviderId,
        ok: bool,
    }

    #[async_trait]
    impl LlmProvider for StubProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }

        fn name(&self) -> &str {
            "stub"
        }

        async fn create_message(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            if self.ok {
                Ok(ProviderResponse {
                    id: "msg".to_string(),
                    model: request.model,
                    content: Vec::new(),
                    stop_reason: StopReason::EndTurn,
                    usage: UsageInfo::default(),
                })
            } else {
                Err(ProviderError::RateLimited {
                    provider: self.id.clone(),
                    retry_after: None,
                })
            }
        }

        async fn create_message_stream(
            &self,
            _request: ProviderRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            Err(ProviderError::ServerError {
                provider: self.id.clone(),
                status: None,
                message: "stub".into(),
                is_retryable: false,
            })
        }

        async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }

        async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
            Ok(ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: false,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: SystemPromptStyle::SystemMessage,
            }
        }
    }

    fn entry(id: &'static str, ok: bool) -> FreeEntry {
        let upstream = *catalog_entry(id).expect("catalog entry");
        FreeEntry {
            upstream,
            provider: Arc::new(StubProvider {
                id: ProviderId::new(id),
                ok,
            }),
        }
    }

    fn dummy_request(model: &str) -> ProviderRequest {
        ProviderRequest {
            model: model.to_string(),
            messages: vec![Message::user("hi")],
            system_prompt: None,
            tools: Vec::new(),
            max_tokens: 8,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            provider_options: serde_json::Value::Null,
        }
    }

    #[test]
    fn route_auto_for_free_aliases() {
        let provider = FreeProvider::new(vec![entry("groq", true), entry("cerebras", true)]);
        assert!(matches!(provider.resolve_route("free"), Route::Auto));
        assert!(matches!(provider.resolve_route("free/auto"), Route::Auto));
        assert!(matches!(provider.resolve_route("auto"), Route::Auto));
        assert!(matches!(provider.resolve_route(""), Route::Auto));
    }

    #[test]
    fn route_pinned_for_prefix() {
        let provider = FreeProvider::new(vec![entry("groq", true), entry("cerebras", true)]);
        let route = provider.resolve_route("cerebras/qwen-3-235b");
        match route {
            Route::Pinned { start_idx, pinned_model } => {
                assert_eq!(start_idx, 1);
                assert_eq!(pinned_model, "qwen-3-235b");
            }
            other => panic!("expected pinned, got {:?}", other),
        }
    }

    #[test]
    fn legacy_zen_prefix_routes_to_opencode_zen() {
        let provider = FreeProvider::new(vec![
            entry("opencode-zen", true),
            entry("openrouter", true),
        ]);
        let route = provider.resolve_route("zen/big-pickle");
        match route {
            Route::Pinned { start_idx, pinned_model } => {
                assert_eq!(start_idx, 0);
                assert_eq!(pinned_model, "big-pickle");
            }
            other => panic!("expected pinned, got {:?}", other),
        }
    }

    #[test]
    fn openrouter_free_keeps_full_id() {
        let provider = FreeProvider::new(vec![entry("openrouter", true)]);
        let route = provider.resolve_route("openrouter/free");
        match route {
            Route::Pinned { pinned_model, .. } => {
                assert_eq!(pinned_model, "openrouter/free");
            }
            other => panic!("expected pinned, got {:?}", other),
        }
    }

    #[test]
    fn attempt_plan_auto_uses_each_default() {
        let provider = FreeProvider::new(vec![entry("groq", true), entry("cerebras", true)]);
        let plan = provider.attempt_plan(&Route::Auto);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].0, 0);
        assert_eq!(plan[0].1, "llama-3.3-70b-versatile");
        assert_eq!(plan[1].0, 1);
        assert_eq!(plan[1].1, "qwen-3-235b-a22b-instruct-2507");
    }

    #[test]
    fn attempt_plan_pinned_tries_pin_then_others() {
        let provider = FreeProvider::new(vec![
            entry("groq", true),
            entry("cerebras", true),
            entry("google", true),
        ]);
        let plan = provider.attempt_plan(&Route::Pinned {
            start_idx: 2,
            pinned_model: "gemini-2.5-pro".into(),
        });
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].0, 2);
        assert_eq!(plan[0].1, "gemini-2.5-pro");
        // Order of remaining = catalog order minus the pinned index.
        assert_eq!(plan[1].0, 0);
        assert_eq!(plan[2].0, 1);
    }

    #[test]
    fn should_fallback_on_transient_errors() {
        let pid = ProviderId::new("groq");
        assert!(FreeProvider::should_fallback(&ProviderError::RateLimited {
            provider: pid.clone(),
            retry_after: None,
        }));
        assert!(FreeProvider::should_fallback(&ProviderError::AuthFailed {
            provider: pid.clone(),
            message: "bad key".into(),
        }));
        assert!(FreeProvider::should_fallback(&ProviderError::ServerError {
            provider: pid.clone(),
            status: Some(500),
            message: "boom".into(),
            is_retryable: true,
        }));
        assert!(!FreeProvider::should_fallback(
            &ProviderError::InvalidRequest {
                provider: pid.clone(),
                message: "bad request".into(),
            }
        ));
        assert!(!FreeProvider::should_fallback(
            &ProviderError::ContentFiltered {
                provider: pid,
                message: "filtered".into(),
            }
        ));
    }

    #[tokio::test]
    async fn create_message_falls_back_to_next_upstream() {
        let provider = FreeProvider::new(vec![entry("groq", false), entry("cerebras", true)]);
        let resp = provider
            .create_message(dummy_request("free/auto"))
            .await
            .expect("should succeed via cerebras");
        assert_eq!(resp.model, "qwen-3-235b-a22b-instruct-2507");
    }

    #[tokio::test]
    async fn empty_chain_returns_auth_error() {
        let provider = FreeProvider::new(vec![]);
        let err = provider
            .create_message(dummy_request("free/auto"))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::AuthFailed { .. }));
    }
}
