// Provider route resolution for one request.
//
// Extracted from `run_query_loop` so the route is resolved once per loop
// iteration, before the pre-request compaction check, which summarises through
// the same provider the request is about to use (issue #441).

use crate::*;

/// Where one request is dispatched.
pub(crate) struct ProviderRoute {
    pub(crate) provider_id: String,
    pub(crate) model_id: String,
    /// Dispatch through `provider` rather than the raw Anthropic client.
    pub(crate) use_provider_dispatch: bool,
    /// The provider to dispatch through; `None` when no credentials exist.
    pub(crate) provider: Option<Arc<dyn claurst_api::LlmProvider>>,
}

/// Resolve the provider route for `effective_model`.
///
/// Provider resolution priority:
///   1. Explicit "provider/model" format in the model string
///   2. config.provider setting (from --provider flag or settings.json)
///   3. Model registry lookup (e.g. "gemini-3-flash-preview" → google)
///   4. Default to "anthropic"
pub(crate) fn resolve_provider_route(
    registry: &claurst_api::ProviderRegistry,
    config: &QueryConfig,
    tool_ctx: &ToolContext,
    effective_model: &str,
    client: &claurst_api::AnthropicClient,
) -> ProviderRoute {
    let (provider_id_str, model_id_str) = if let Some(p) = tool_ctx.config.provider.as_deref().filter(|p| *p != "anthropic") {
        // Explicit non-Anthropic provider in config — use it.
        // If the stored model is in canonical "provider/model" form,
        // strip the top-level provider prefix before sending it to the
        // provider adapter. If it contains an additional slash
        // (e.g. "meta-llama/Llama-3.3..." on OpenRouter), preserve it.
        let provider_prefix = format!("{}/", p);
        let model_id = effective_model
            .strip_prefix(&provider_prefix)
            .unwrap_or(effective_model)
            .to_string();
        (p.to_string(), model_id)
    } else if let Some((p, m)) = effective_model.split_once('/') {
        // No explicit provider but model has "provider/model" format.
        // Check whether `p` is a known provider or just a model
        // namespace (e.g. "meta-llama/Llama-3" on OpenRouter).
        let known_providers = [
            // Native (non-OpenAI-compat) providers
            "anthropic", "openai", "google", "azure", "amazon-bedrock",
            "github-copilot", "codex", "openai-codex", "cohere", "minimax",
            // Local / self-hosted
            "ollama",
            "lmstudio", "lm-studio",
            "llamacpp", "llama-cpp", "llama-server",
            // OpenAI-compat cloud providers
            "groq", "mistral", "deepseek", "xai", "perplexity", "cerebras",
            "openrouter", "togetherai", "together-ai", "deepinfra", "venice",
            "huggingface", "nvidia", "fireworks", "sambanova",
            // Additional OpenAI-compat providers
            "qwen", "alibaba", "siliconflow",
            "moonshot", "moonshotai",
            "zhipu", "zhipuai",
            "zai",
            "nebius", "novita", "ovhcloud", "scaleway",
            "vultr", "vultr-ai",
            "baseten", "friendli", "upstage", "stepfun",
        ];
        if known_providers.contains(&p) {
            (p.to_string(), m.to_string())
        } else {
            // Treat the whole string as the model ID, fall through
            // to auto-detection below.
            let fallback_provider = tool_ctx.config.provider.as_deref().unwrap_or("anthropic");
            (fallback_provider.to_string(), effective_model.to_string())
        }
    } else {
        // No explicit provider set (or set to "anthropic"): try the
        // model registry to auto-detect provider from the model name.
        // Use the shared model registry from QueryConfig if available;
        // otherwise construct a temporary one.
        let temp_reg;
        let model_reg: &claurst_api::ModelRegistry = if let Some(ref shared) = config.model_registry {
            shared
        } else {
            temp_reg = {
                let mut r = claurst_api::ModelRegistry::new();
                if let Some(cache_dir) = dirs::cache_dir() {
                    let cache_path = cache_dir.join("claurst").join("models_dev.json");
                    r.load_cache(&cache_path);
                }
                r
            };
            &temp_reg
        };
        if let Some(detected_pid) = model_reg.find_provider_for_model(effective_model) {
            let pid_str = detected_pid.to_string();
            if pid_str != "anthropic" {
                (pid_str, effective_model.to_string())
            } else {
                ("anthropic".to_string(), effective_model.to_string())
            }
        } else {
            // Fall back to config.provider (may be "anthropic" or None→"anthropic")
            let p = tool_ctx.config.provider.as_deref().unwrap_or("anthropic");
            (p.to_string(), effective_model.to_string())
        }
    };

    // Dispatch through the provider path for non-Anthropic providers,
    // AND for Anthropic when the pre-built client has no API key
    // (user started without ANTHROPIC_API_KEY but added one via /connect).
    let use_provider_dispatch = provider_id_str != "anthropic"
        || client.api_key_is_empty();

    let provider = if use_provider_dispatch {
        let pid = claurst_core::provider_id::ProviderId::new(&provider_id_str);

        // Always prefer a fresh provider built from the auth_store so
        // that keys added at runtime via /connect are picked up
        // immediately — even when the provider was pre-registered at
        // startup with a stale or missing key.
        let runtime_provider =
            claurst_api::registry::runtime_provider_for(&provider_id_str);

        let registry_provider = if runtime_provider.is_some() {
            // Fresh auth_store key available — use it instead of the
            // (possibly stale) registry entry.
            None
        } else {
            registry.get(&pid).cloned()
        };

        let mut provider = runtime_provider.or(registry_provider);

        // Rebuild providers using the unified base resolver so overrides
        // from settings/env/defaults are applied consistently.
        if claurst_api::registry::resolve_provider_api_base(
            &tool_ctx.config,
            &provider_id_str,
        ).is_some() {
            if let Some(overridden) = claurst_api::registry::provider_from_config(
                &tool_ctx.config,
                &provider_id_str,
            ) {
                provider = Some(overridden);
            }
        }
        provider
    } else {
        None
    };

    ProviderRoute {
        provider_id: provider_id_str,
        model_id: model_id_str,
        use_provider_dispatch,
        provider,
    }
}
