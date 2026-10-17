// registry.rs — Registry of all available LLM providers.
//
// Holds an `Arc<dyn LlmProvider>` for each registered provider and exposes
// lookup, health-check, and default-provider helpers.

use std::collections::HashMap;
use std::sync::Arc;

use claurst_core::ProviderId;

use crate::client::ClientConfig;
use crate::provider::LlmProvider;
use crate::provider_types::ProviderStatus;
use crate::providers::{
    AnthropicProvider, AzureProvider, BedrockProvider, CodexProvider, CohereProvider,
    CopilotProvider, FreeEntry, FreeProvider, FREE_CATALOG, GoogleProvider, MinimaxProvider,
    OpenAiProvider,
};

fn normalize_openai_compat_base(override_base: &str) -> String {
    let trimmed = override_base.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{}/v1", trimmed)
    }
}

fn normalize_openai_base(override_base: &str) -> String {
    let trimmed = override_base.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.trim_end_matches("/v1").to_string()
    } else {
        trimmed.to_string()
    }
}

fn canonical_local_provider_id(provider_id: &str) -> &str {
    match provider_id {
        "lmstudio" => ProviderId::LM_STUDIO,
        "llamacpp" | "llama-server" => ProviderId::LLAMA_CPP,
        _ => provider_id,
    }
}

pub fn resolve_provider_api_base(
    config: &claurst_core::config::Config,
    provider_id: &str,
) -> Option<String> {
    let base = config.resolve_provider_api_base(provider_id)?;
    if provider_id == "openai" {
        Some(normalize_openai_base(&base))
    } else if crate::providers::openai_compat_providers::provider_for_id(provider_id).is_some() {
        Some(normalize_openai_compat_base(&base))
    } else {
        Some(base)
    }
}

/// Registry of all available LLM providers.
/// Holds `Arc<dyn LlmProvider>` for each registered provider.
pub struct ProviderRegistry {
    providers: HashMap<ProviderId, Arc<dyn LlmProvider>>,
    default_provider_id: ProviderId,
}

fn provider_from_key(provider_id: &str, key: String) -> Option<Arc<dyn LlmProvider>> {
    use crate::providers::openai_compat_providers as p;

    if let Some(provider) = p::provider_for_id(provider_id) {
        return Some(Arc::new(provider.with_api_key(key)));
    }

    match provider_id {
        "anthropic" => Some(Arc::new(AnthropicProvider::from_config(
            ClientConfig { api_key: key, ..Default::default() },
        ))),
        "minimax" => Some(Arc::new(MinimaxProvider::new(key))),
        "openai" => Some(Arc::new(OpenAiProvider::new(key))),
        "google" => Some(Arc::new(GoogleProvider::new(key))),
        "github-copilot" => Some(Arc::new(CopilotProvider::new(key))),
        "codex" | "openai-codex" => {
            // The Codex provider is OAuth-based; the `key` field is not used.
            // Load from the stored token file instead.
            CodexProvider::from_stored().map(|p| Arc::new(p) as Arc<dyn LlmProvider>)
        }
        "cohere" => Some(Arc::new(CohereProvider::new(key))),
        "custom-openai" => Some(Arc::new(p::custom_openai().with_api_key(key))),
        // "free" needs two keys (Zen + OpenRouter) — single-key path doesn't
        // apply.  The auth-store-aware path `runtime_provider_for` handles it.
        "free" => build_free_provider(),
        _ => None,
    }
}

/// Whether `provider_id` names a provider this build can dispatch to.
///
/// OpenAI-compatible ids are answered by `provider_for_id` itself, so new
/// compat providers are accepted automatically. The remaining ids are the
/// bespoke backends dispatched by `provider_from_key`, `provider_from_config`
/// and `ProviderRegistry::from_environment_with_auth_store`; keep them in sync
/// with those match arms.
pub fn is_known_provider_id(provider_id: &str) -> bool {
    crate::providers::openai_compat_providers::provider_for_id(provider_id).is_some()
        || matches!(
            provider_id,
            ProviderId::ANTHROPIC
                | ProviderId::OPENAI
                | ProviderId::GOOGLE
                | ProviderId::GITHUB_COPILOT
                | ProviderId::CODEX
                | "openai-codex"
                | ProviderId::COHERE
                | ProviderId::MINIMAX
                | ProviderId::AZURE
                | ProviderId::AMAZON_BEDROCK
                | "custom-openai"
                | ProviderId::FREE
        )
}

/// Build a [`FreeProvider`] by walking [`FREE_CATALOG`] and pulling any keys
/// the user has stored in the auth store. Each catalog entry whose upstream
/// has a key becomes one link in the fallback chain.
///
/// Returns `None` only if *no* catalog entry has a configured key — a single
/// key is enough to run, and more is better.
pub fn build_free_provider() -> Option<Arc<dyn LlmProvider>> {
    let auth_store = claurst_core::AuthStore::load();
    let mut chain: Vec<FreeEntry> = Vec::new();

    for upstream in FREE_CATALOG {
        let key = match upstream.id {
            // OpenCode Zen and Go share `OPENCODE_API_KEY`; accept either slot.
            "opencode-zen" => auth_store
                .api_key_for(claurst_core::ProviderId::OPENCODE_ZEN)
                .or_else(|| auth_store.api_key_for(claurst_core::ProviderId::OPENCODE_GO)),
            other => auth_store.api_key_for(other),
        }
        .filter(|k| !k.trim().is_empty());

        let Some(key) = key else {
            continue;
        };

        let provider: Option<Arc<dyn LlmProvider>> = match upstream.id {
            "google" => Some(Arc::new(GoogleProvider::new(key))),
            "cohere" => Some(Arc::new(CohereProvider::new(key))),
            id => crate::providers::openai_compat_providers::provider_for_id(id)
                .map(|p| Arc::new(p.with_api_key(key)) as Arc<dyn LlmProvider>),
        };

        if let Some(provider) = provider {
            chain.push(FreeEntry {
                upstream: *upstream,
                provider,
            });
        }
    }

    if chain.is_empty() {
        return None;
    }
    Some(Arc::new(FreeProvider::new(chain)) as Arc<dyn LlmProvider>)
}

pub fn provider_from_config(
    config: &claurst_core::config::Config,
    provider_id: &str,
) -> Option<Arc<dyn LlmProvider>> {
    let provider_cfg = config.provider_configs.get(provider_id);
    if provider_cfg.is_some_and(|provider| !provider.enabled) {
        return None;
    }

    let api_key = config.resolve_provider_api_key(provider_id);
    let api_base = resolve_provider_api_base(config, provider_id).filter(|base| !base.is_empty());

    use crate::providers;

    match provider_id {
        "anthropic" => None,
        // Composite "Free" provider — two keys are pulled internally from the
        // auth store; the `api_key` resolved above is ignored.
        "free" => build_free_provider(),
        "openai" => {
            let mut provider = OpenAiProvider::new(api_key.unwrap_or_default());
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "google" => api_key.map(|key| Arc::new(GoogleProvider::new(key)) as Arc<dyn LlmProvider>),
        "minimax" => api_key.map(|key| {
            let mut provider = MinimaxProvider::new(key);
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            if let Some(service_tier) = provider_cfg
                .and_then(|config| config.options.get("service_tier"))
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
            {
                provider = provider.with_service_tier(service_tier);
            }
            Arc::new(provider) as Arc<dyn LlmProvider>
        }),
        "azure" => {
            let resource_name = provider_cfg
                .and_then(|provider| provider.options.get("resource_name"))
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    std::env::var("AZURE_RESOURCE_NAME")
                        .ok()
                        .filter(|value| !value.is_empty())
                });

            match (resource_name, api_key) {
                (Some(resource_name), Some(key)) => Some(
                    Arc::new(AzureProvider::new(resource_name, key)) as Arc<dyn LlmProvider>
                ),
                _ => None,
            }
        }
        "ollama" => {
            let mut provider = providers::ollama();
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "lmstudio" | "lm-studio" => {
            let mut provider = providers::lm_studio();
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "llamacpp" | "llama-cpp" | "llama-server" => {
            let mut provider = providers::llama_cpp();
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "deepseek" => {
            let mut provider = providers::deepseek();
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "groq" => {
            let mut provider = providers::groq();
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "xai" => {
            let mut provider = providers::xai();
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "openrouter" => {
            let mut provider = providers::openrouter();
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "cohere" => api_key.map(|key| Arc::new(CohereProvider::new(key)) as Arc<dyn LlmProvider>),
        "github-copilot" => {
            api_key.map(|key| Arc::new(CopilotProvider::new(key)) as Arc<dyn LlmProvider>)
        }
        "codex" | "openai-codex" => {
            CodexProvider::from_stored().map(|provider| Arc::new(provider) as Arc<dyn LlmProvider>)
        }
        _ => api_key.and_then(|key| provider_from_key(provider_id, key)),
    }
}

pub fn runtime_provider_for(provider_id: &str) -> Option<Arc<dyn LlmProvider>> {
    use crate::providers::openai_compat_providers as p;

    // Local providers never require an API key — build them directly so that
    // the auth-store bypass below doesn't silently drop them.
    // Accept both the hyphenated canonical IDs ("llama-cpp", "lm-studio") and
    // the non-hyphenated aliases ("llamacpp", "lmstudio") used throughout the
    // TUI / connect dialog.
    match provider_id {
        "ollama" => return Some(Arc::new(p::ollama())),
        "lmstudio" | "lm-studio" => return Some(Arc::new(p::lm_studio())),
        // "llama-server" is the binary name for the modern llama.cpp server.
        "llamacpp" | "llama-cpp" | "llama-server" => return Some(Arc::new(p::llama_cpp())),
        "codex" | "openai-codex" => {
            return CodexProvider::from_stored().map(|p| Arc::new(p) as Arc<dyn LlmProvider>);
        }
        // "free" pulls two keys (Zen + OpenRouter) from the auth store and
        // wraps them in a fallback composite — handled here so the generic
        // single-key path below doesn't short-circuit on a missing key.
        "free" => return build_free_provider(),
        _ => {}
    }

    let auth_store = claurst_core::AuthStore::load();
    let key = auth_store.api_key_for(provider_id)?;
    if key.is_empty() {
        return None;
    }
    provider_from_key(provider_id, key)
}

impl ProviderRegistry {
    /// Create an empty registry with Anthropic as the default provider ID.
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            default_provider_id: ProviderId::new(ProviderId::ANTHROPIC),
        }
    }

    /// Register a provider. Returns `&mut self` for builder chaining.
    pub fn register(&mut self, provider: Arc<dyn LlmProvider>) -> &mut Self {
        let id = provider.id().clone();
        self.providers.insert(id, provider);
        self
    }

    /// Set the default provider by ID.
    ///
    /// # Panics
    /// Panics if no provider with that ID has been registered.
    pub fn set_default(&mut self, id: ProviderId) -> &mut Self {
        let canonical_id = ProviderId::new(canonical_local_provider_id(&id));
        assert!(
            self.providers.contains_key(&canonical_id),
            "set_default: provider '{}' is not registered",
            id,
        );
        self.default_provider_id = canonical_id;
        self
    }

    /// Get a provider by ID.
    pub fn get(&self, id: &ProviderId) -> Option<&Arc<dyn LlmProvider>> {
        self.providers.get(id).or_else(|| {
            let canonical_id = canonical_local_provider_id(id);
            (canonical_id != &**id)
                .then(|| self.providers.get(&ProviderId::new(canonical_id)))
                .flatten()
        })
    }

    /// Get the default provider.
    pub fn default_provider(&self) -> Option<&Arc<dyn LlmProvider>> {
        self.providers.get(&self.default_provider_id)
    }

    /// Get the default provider ID.
    pub fn default_provider_id(&self) -> &ProviderId {
        &self.default_provider_id
    }

    /// List all registered provider IDs.
    pub fn provider_ids(&self) -> Vec<&ProviderId> {
        self.providers.keys().collect()
    }

    /// Check health of all providers sequentially.
    /// Returns `(provider_id, status)` pairs.
    pub async fn check_all_health(&self) -> Vec<(ProviderId, ProviderStatus)> {
        let mut results = Vec::new();
        for (id, provider) in &self.providers {
            let status = provider
                .health_check()
                .await
                .unwrap_or(ProviderStatus::Unavailable {
                    reason: "health check failed".to_string(),
                });
            results.push((id.clone(), status));
        }
        results
    }

    /// Convenience: build a registry with just Anthropic registered as the
    /// default provider.  Takes the same [`ClientConfig`] that
    /// [`AnthropicClient`] takes.
    ///
    /// [`AnthropicClient`]: crate::client::AnthropicClient
    pub fn with_anthropic(config: ClientConfig) -> Self {
        let mut registry = Self::new();
        let provider = Arc::new(AnthropicProvider::from_config(config));
        registry.register(provider);
        registry
    }

    pub fn from_config(
        config: &claurst_core::config::Config,
        anthropic_config: ClientConfig,
    ) -> Self {
        // Apply the user-configured request timeout (issue #175) before any
        // provider HTTP clients are built, so they all honour it. Uses the
        // active provider's resolved value (per-provider override or global).
        crate::set_request_timeout_secs(
            config.resolve_request_timeout_secs(config.selected_provider_id()),
        );
        let mut registry = Self::from_environment_with_auth_store(anthropic_config);
        let active_provider = config.selected_provider_id();

        let mut configured_provider_ids: Vec<String> = config
            .provider_configs
            .keys()
            .cloned()
            .collect();
        if configured_provider_ids.iter().all(|id| id != active_provider) {
            configured_provider_ids.push(active_provider.to_string());
        }

        for provider_id in configured_provider_ids {
            if let Some(provider) = provider_from_config(config, &provider_id) {
                registry.register(provider);
            }
        }

        let default_provider_id = ProviderId::new(active_provider);
        if registry.get(&default_provider_id).is_some() {
            registry.set_default(default_provider_id);
        }

        registry
    }

    /// Register [`GoogleProvider`] if `GOOGLE_API_KEY` or
    /// `GOOGLE_GENERATIVE_AI_API_KEY` is set in the environment.
    /// Returns `&mut self` for builder chaining.
    pub fn with_google_if_key_set(&mut self) -> &mut Self {
        let key = std::env::var("GOOGLE_API_KEY")
            .or_else(|_| std::env::var("GOOGLE_GENERATIVE_AI_API_KEY"));
        if let Ok(key) = key {
            let provider = Arc::new(GoogleProvider::new(key));
            self.register(provider);
        }
        self
    }

    /// Register [`OpenAiProvider`] if `OPENAI_API_KEY` is set in the
    /// environment.  Returns `&mut self` for builder chaining.
    pub fn with_openai_if_key_set(&mut self) -> &mut Self {
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            let provider = Arc::new(OpenAiProvider::new(key));
            self.register(provider);
        }
        self
    }

    /// Register [`AzureProvider`] if `AZURE_API_KEY` and `AZURE_RESOURCE_NAME`
    /// are set in the environment.  Returns `&mut self` for builder chaining.
    pub fn with_azure_if_configured(&mut self) -> &mut Self {
        if let Some(p) = AzureProvider::from_env() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register [`BedrockProvider`] if AWS credentials are available in the
    /// environment (`AWS_ACCESS_KEY_ID`+`AWS_SECRET_ACCESS_KEY` or
    /// `AWS_BEARER_TOKEN_BEDROCK`).  Returns `&mut self` for builder chaining.
    pub fn with_bedrock_if_configured(&mut self) -> &mut Self {
        if let Some(p) = BedrockProvider::from_env() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register [`CopilotProvider`] if `GITHUB_TOKEN` is set in the environment.
    /// Returns `&mut self` for builder chaining.
    pub fn with_copilot_if_configured(&mut self) -> &mut Self {
        if let Some(p) = CopilotProvider::from_env() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register [`CodexProvider`] if stored Codex OAuth tokens are available in
    /// `~/.claurst/codex_tokens.json`.  Returns `&mut self` for builder chaining.
    pub fn with_codex_if_configured(&mut self) -> &mut Self {
        if let Some(p) = CodexProvider::from_stored() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register [`CohereProvider`] if `COHERE_API_KEY` is set in the environment.
    /// Returns `&mut self` for builder chaining.
    pub fn with_cohere_if_key_set(&mut self) -> &mut Self {
        if let Some(p) = CohereProvider::from_env() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Build a registry with **all** providers that have credentials configured
    /// in the environment.  Anthropic is always the default provider.
    ///
    /// This is the recommended constructor for production use.
    pub fn from_environment(anthropic_config: ClientConfig) -> Self {
        let mut registry = Self::with_anthropic(anthropic_config);
        registry
            .with_openai_if_key_set()
            .with_google_if_key_set()
            .with_azure_if_configured()
            .with_bedrock_if_configured()
            .with_copilot_if_configured()
            .with_codex_if_configured()
            .with_cohere_if_key_set()
            .with_available_providers();
        registry
    }

    /// Build a registry that checks **both** environment variables and the
    /// persistent [`AuthStore`] (`~/.claurst/auth.json`) for credentials.
    ///
    /// This ensures that API keys stored via `/connect` or `claurst auth` are
    /// picked up at startup, not just env vars.  Falls back to
    /// `from_environment` for providers that only support env-var config, and
    /// adds any extra providers that have keys in the auth store.
    ///
    /// [`AuthStore`]: claurst_core::AuthStore
    pub fn from_environment_with_auth_store(anthropic_config: ClientConfig) -> Self {
        // Start with env-based registration.
        let mut registry = Self::from_environment(anthropic_config);

        // Now check the auth store for providers that weren't registered from
        // env vars.
        let auth_store = claurst_core::AuthStore::load();

        for provider_id in auth_store.credentials.keys() {
            let pid = claurst_core::ProviderId::new(provider_id.as_str());
            // Skip if already registered from env vars.
            if registry.get(&pid).is_some() {
                continue;
            }
            // Try to get a usable key from the auth store.
            if let Some(key) = auth_store.api_key_for(provider_id) {
                if key.is_empty() {
                    continue;
                }
                let provider = provider_from_key(provider_id, key);
                if let Some(p) = provider {
                    registry.register(p);
                }
            }
        }

        registry
    }

    /// Register all providers that have environment variable credentials set.
    ///
    /// Local providers (Ollama, LM Studio, llama.cpp) are always registered
    /// regardless of credentials — `health_check()` will report them as
    /// unavailable if the server is not running.
    ///
    /// Remote API-key providers are only registered when their respective
    /// environment variables are set (non-empty).
    ///
    /// Returns `&mut self` for builder chaining.
    pub fn with_available_providers(&mut self) -> &mut Self {
        use crate::providers::openai_compat_providers as p;

        // Local providers — always try to register.
        self.register(Arc::new(p::ollama()));
        self.register(Arc::new(p::lm_studio()));
        self.register(Arc::new(p::llama_cpp()));

        // Remote providers — only register when an API key is present.
        if std::env::var("DEEPSEEK_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::deepseek()));
        }
        if std::env::var("GROQ_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::groq()));
        }
        if std::env::var("XAI_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::xai()));
        }
        if std::env::var("OPENROUTER_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::openrouter()));
        }
        if std::env::var("TOGETHER_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::together_ai()));
        }
        if std::env::var("PERPLEXITY_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::perplexity()));
        }
        if std::env::var("CEREBRAS_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::cerebras()));
        }
        if std::env::var("DEEPINFRA_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::deepinfra()));
        }
        if std::env::var("VENICE_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::venice()));
        }
        if std::env::var("DASHSCOPE_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::qwen()));
        }
        if std::env::var("MISTRAL_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::mistral()));
        }
        if std::env::var("SAMBANOVA_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::sambanova()));
        }
        if std::env::var("HF_TOKEN").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::huggingface()));
        }
        if std::env::var("MINIMAX_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            let key = std::env::var("MINIMAX_API_KEY").unwrap_or_default();
            self.register(Arc::new(MinimaxProvider::new(key)));
        }
        if std::env::var("NVIDIA_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::nvidia()));
        }
        if std::env::var("SILICONFLOW_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::siliconflow()));
        }
        if std::env::var("MOONSHOT_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::moonshot()));
        }
        if std::env::var("ZHIPU_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::zhipu()));
        }
        if std::env::var("ZAI_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::zai()));
        }
        if std::env::var("NEBIUS_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::nebius()));
        }
        if std::env::var("NOVITA_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::novita()));
        }
        if std::env::var("OVHCLOUD_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::ovhcloud()));
        }
        if std::env::var("SCALEWAY_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::scaleway()));
        }
        if std::env::var("VULTR_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::vultr_ai()));
        }
        if std::env::var("BASETEN_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::baseten()));
        }
        if std::env::var("FRIENDLI_TOKEN").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::friendli()));
        }
        if std::env::var("UPSTAGE_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::upstage()));
        }
        if std::env::var("STEPFUN_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::stepfun()));
        }
        if std::env::var("FIREWORKS_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::fireworks()));
        }
        if std::env::var("OPENCODE_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
            self.register(Arc::new(p::opencode_go()));
        }
        self
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Provider ids that can be offered as interactive failover targets.
///
/// `anthropic-oauth` is excluded on purpose: it dispatches through the raw
/// Anthropic client with a Bearer flag managed by the TUI, not through the
/// provider registry, so an automatic switch to it could not rebuild the
/// auth path reliably.
pub const FAILOVER_CANDIDATE_PROVIDERS: &[&str] = &[
    "codex",
    "github-copilot",
    "free",
    "openai",
    "google",
    "anthropic",
    "minimax",
    "cohere",
];

/// Providers that need no credential at all (local servers and the keyless
/// upstreams). They are always viable failover targets.
pub const KEYLESS_FAILOVER_PROVIDERS: &[&str] = &["ollama", "lmstudio", "llamacpp"];

/// Whether `provider_id` has a stored OAuth login that makes it a usable
/// failover target even without an API key.
///
/// - `codex` / `openai-codex`: per-profile `codex_tokens.json` (via the
///   account registry, legacy single-file fallback included).
/// - `github-copilot`: an `OAuthToken` credential written by `/connect`,
///   which `api_key_for` already surfaces as a key.
fn has_stored_oauth_login(provider_id: &str, auth_store: &claurst_core::AuthStore) -> bool {
    match provider_id {
        "codex" | "openai-codex" => claurst_core::oauth_config::get_codex_tokens()
            .is_some_and(|tokens| !tokens.access_token.is_empty()),
        "github-copilot" => auth_store.get(provider_id).is_some_and(
            |credential| matches!(credential, claurst_core::StoredCredential::OAuthToken { .. }),
        ),
        _ => false,
    }
}

/// Collect the provider ids that have usable credentials right now, for
/// the interactive failover ranking. Reads the auth store and environment
/// once per call.
///
/// `with_env` injects extra `(provider_id, credential-present)` pairs; the
/// query loop uses it to fold in the pre-built Anthropic client's auth
/// state so a provider configured only through `ANTHROPIC_API_KEY` (never
/// stored in the auth store) still shows up. Tests inject a fixture list.
///
/// OAuth-backed ids (`codex`, `github-copilot`) also count when a stored
/// login exists even though no API key is present for them.
pub fn failover_candidate_providers(
    with_env: impl Fn(&str) -> Option<String>,
) -> Vec<&'static str> {
    let auth_store = claurst_core::AuthStore::load();
    let mut out: Vec<&'static str> = Vec::new();
    for id in KEYLESS_FAILOVER_PROVIDERS {
        out.push(id);
    }
    for id in FAILOVER_CANDIDATE_PROVIDERS {
        if auth_store.api_key_for(id).is_some() {
            out.push(id);
            continue;
        }
        if with_env(id).is_some_and(|key| !key.is_empty()) {
            out.push(id);
            continue;
        }
        if has_stored_oauth_login(id, &auth_store) {
            out.push(id);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers;

    #[test]
    fn local_provider_aliases_resolve_to_canonical_registrations() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(providers::lm_studio()));
        registry.register(Arc::new(providers::llama_cpp()));

        let lm_studio = registry
            .get(&ProviderId::new("lmstudio"))
            .expect("lmstudio alias should resolve");
        let llama_cpp = registry
            .get(&ProviderId::new("llamacpp"))
            .expect("llamacpp alias should resolve");

        assert_eq!(&**lm_studio.id(), ProviderId::LM_STUDIO);
        assert_eq!(&**llama_cpp.id(), ProviderId::LLAMA_CPP);
    }

    #[test]
    fn alias_can_select_canonical_default_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(providers::lm_studio()));
        registry.set_default(ProviderId::new("lmstudio"));

        assert_eq!(&**registry.default_provider_id(), ProviderId::LM_STUDIO);
    }

    #[test]
    fn known_provider_ids_cover_every_dispatchable_backend() {
        for id in [
            ProviderId::ANTHROPIC,
            ProviderId::OPENAI,
            ProviderId::AMAZON_BEDROCK,
            ProviderId::CODEX,
            ProviderId::FREE,
            ProviderId::TOGETHER_AI,
            ProviderId::FIREWORKS,
            ProviderId::NEURALWATT,
            ProviderId::LM_STUDIO,
            "lmstudio",
            "togetherai",
            "custom-openai",
        ] {
            assert!(is_known_provider_id(id), "{id} should be accepted");
        }
        for id in ["anthropik", "", ProviderId::GOOGLE_VERTEX, ProviderId::GITLAB] {
            assert!(!is_known_provider_id(id), "{id} has no backend and should be rejected");
        }
    }

    /// Points `CLAURST_HOME` at a fresh temp dir for the test body and
    /// restores the previous value. Without it the candidate scan would
    /// read the real `~/.claurst` auth store. Returns the dir path.
    fn scoped_home() -> (std::path::PathBuf, Option<std::ffi::OsString>) {
        let home = std::env::temp_dir().join(format!("claurst-failover-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).expect("create temp home");
        let saved = std::env::var_os("CLAURST_HOME");
        std::env::set_var("CLAURST_HOME", &home);
        (home, saved)
    }

    fn restore_home(home: std::path::PathBuf, saved: Option<std::ffi::OsString>) {
        match saved {
            Some(v) => std::env::set_var("CLAURST_HOME", v),
            None => std::env::remove_var("CLAURST_HOME"),
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Run `body` with `CLAURST_HOME` pointed at a fresh temp dir, restoring
    /// the previous value afterwards even when the body panics, so a failed
    /// assertion cannot leak the temp dir into other tests.
    fn with_scoped_home(body: impl FnOnce()) {
        let (home, saved) = scoped_home();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        restore_home(home, saved);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn stored_codex_login_is_a_failover_candidate_without_any_api_key() {
        let _guard = crate::test_env_lock::acquire();
        with_scoped_home(|| {
            // The production login path registers the profile in the
            // account registry; get_codex_tokens reads the registry
            // first, so the register variant is the realistic save.
            claurst_core::oauth_config::save_codex_tokens_and_register(
                &claurst_core::oauth_config::CodexTokens {
                    access_token: "access".to_string(),
                    refresh_token: Some("refresh".to_string()),
                    ..Default::default()
                },
                Some("work"),
            )
            .expect("save codex tokens");

            let candidates = failover_candidate_providers(|_| None);
            assert!(
                candidates.contains(&"codex"),
                "a stored Codex OAuth login must be offered even with no API key: {candidates:?}"
            );
            assert!(
                !candidates.contains(&"free"),
                "free mode needs upstream keys, not an OAuth login: {candidates:?}"
            );
        });
    }

    #[test]
    fn no_stored_login_means_no_oauth_failover_candidate() {
        let _guard = crate::test_env_lock::acquire();
        with_scoped_home(|| {
            let candidates = failover_candidate_providers(|_| None);
            assert!(
                !candidates.contains(&"codex"),
                "no tokens on disk means codex must not be offered: {candidates:?}"
            );
        });
    }

    #[test]
    fn env_key_still_promotes_a_candidate() {
        let _guard = crate::test_env_lock::acquire();
        with_scoped_home(|| {
            let candidates = failover_candidate_providers(|id| {
                (id == "openai").then(|| "sk-test".to_string())
            });
            assert!(candidates.contains(&"openai"));
            assert!(
                !candidates.contains(&"google"),
                "providers without keys, tokens or env vars stay out: {candidates:?}"
            );
        });
    }
}
