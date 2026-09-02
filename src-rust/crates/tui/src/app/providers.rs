//! Provider/model/config management and import-config flow.

use claurst_core::config::{Config, Settings};
use crate::dialog_select::{DialogSelectState, SelectItem};
use crate::import_config_dialog::ImportConfigDialogState;
use crate::model_picker::ModelPickerState;
use super::App;

/// Return the environment variable name for a given provider ID.
#[allow(dead_code)]
fn get_env_var_for_provider(id: &str) -> &'static str {
    match id {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "google" | "google-vertex" => "GOOGLE_API_KEY",
        "github-copilot" => "GITHUB_TOKEN",
        "groq" => "GROQ_API_KEY",
        "cerebras" => "CEREBRAS_API_KEY",
        "sambanova" => "SAMBANOVA_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "togetherai" => "TOGETHER_API_KEY",
        "perplexity" => "PERPLEXITY_API_KEY",
        "cohere" => "COHERE_API_KEY",
        "xai" => "XAI_API_KEY",
        "deepinfra" => "DEEPINFRA_API_KEY",
        "azure" => "AZURE_API_KEY",
        "amazon-bedrock" => "AWS_ACCESS_KEY_ID",
        "sap-ai-core" => "AICORE_SERVICE_KEY",
        "gitlab" => "GITLAB_TOKEN",
        "cloudflare-ai-gateway" | "cloudflare-workers-ai" => "CLOUDFLARE_API_TOKEN",
        "vercel" => "AI_GATEWAY_API_KEY",
        "helicone" => "HELICONE_API_KEY",
        "huggingface" => "HF_TOKEN",
        "nvidia" => "NVIDIA_API_KEY",
        "alibaba" => "DASHSCOPE_API_KEY",
        "venice" => "VENICE_API_KEY",
        "moonshotai" => "MOONSHOT_API_KEY",
        "zhipuai" => "ZHIPU_API_KEY",
        "zai" => "ZAI_API_KEY",
        "siliconflow" => "SILICONFLOW_API_KEY",
        "nebius" => "NEBIUS_API_KEY",
        "novita" => "NOVITA_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "ovhcloud" => "OVHCLOUD_API_KEY",
        "scaleway" => "SCALEWAY_API_KEY",
        "vultr" => "VULTR_API_KEY",
        "baseten" => "BASETEN_API_KEY",
        "friendli" => "FRIENDLI_TOKEN",
        "upstage" => "UPSTAGE_API_KEY",
        "stepfun" => "STEPFUN_API_KEY",
        "fireworks" => "FIREWORKS_API_KEY",
        _ => "API_KEY",
    }
}

/// Return a URL hint for obtaining an API key from a given provider.
#[allow(dead_code)]
fn get_url_for_provider(id: &str) -> &'static str {
    match id {
        "anthropic" => "console.anthropic.com",
        "openai" => "platform.openai.com/api-keys",
        "google" => "aistudio.google.com/apikey",
        "github-copilot" => "github.com/settings/tokens",
        "groq" => "console.groq.com/keys",
        "cerebras" => "cloud.cerebras.ai",
        "sambanova" => "cloud.sambanova.ai",
        "deepseek" => "platform.deepseek.com/api_keys",
        "mistral" => "console.mistral.ai/api-keys",
        "openrouter" => "openrouter.ai/keys",
        "togetherai" => "api.together.xyz/settings/api-keys",
        "perplexity" => "perplexity.ai/settings/api",
        "cohere" => "dashboard.cohere.com/api-keys",
        "xai" => "console.x.ai",
        "deepinfra" => "deepinfra.com/dash/api_keys",
        "azure" => "portal.azure.com",
        "amazon-bedrock" => "console.aws.amazon.com/bedrock",
        "minimax" => "platform.minimaxi.com",
        "huggingface" => "huggingface.co/settings/tokens",
        "nvidia" => "build.nvidia.com",
        "venice" => "venice.ai/settings/api",
        "zai" => "z.ai/manage-apikey/apikey-list",
        _ => "the provider's website",
    }
}

pub(super) fn import_config_picker_items() -> Vec<SelectItem> {
    vec![
        SelectItem {
            id: "claude-md".into(),
            title: "CLAUDE.md".into(),
            description: "Import ~/.claude/CLAUDE.md".into(),
            category: "Import".into(),
            badge: None,
        },
        SelectItem {
            id: "settings".into(),
            title: "settings.json".into(),
            description: "Import ~/.claude/settings.json".into(),
            category: "Import".into(),
            badge: None,
        },
        SelectItem {
            id: "both".into(),
            title: "Both".into(),
            description: "Import both CLAUDE.md and settings.json".into(),
            category: "Import".into(),
            badge: Some("SAFE".into()),
        },
    ]
}

pub(super) fn provider_picker_items() -> Vec<SelectItem> {
    vec![
        SelectItem { id: "free".into(), title: "Free Mode".into(), description: "OpenCode Zen → OpenRouter free fallback (no spend)".into(), category: "Popular".into(), badge: Some("FREE".into()) },
        SelectItem { id: "openai".into(), title: "OpenAI".into(), description: "(API key)".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "openai-codex".into(), title: "OpenAI Codex".into(), description: "(ChatGPT Plus/Pro — browser login)".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "github-copilot".into(), title: "GitHub Copilot".into(), description: "(GitHub subscription or token)".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "google".into(), title: "Google".into(), description: "(API key)".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "anthropic".into(), title: "Anthropic".into(), description: "(API key)".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "anthropic-oauth".into(), title: "Anthropic (Claude Pro/Max)".into(), description: "(subscription — browser login; draws from extra-usage)".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "custom-openai".into(), title: "Custom OpenAI-Compatible".into(), description: "Custom URL + API key".into(), category: "Advanced".into(), badge: None },
        SelectItem { id: "openrouter".into(), title: "OpenRouter".into(), description: "100+ models with one key".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "vercel".into(), title: "Vercel AI Gateway".into(), description: "Gateway for AI SDK models".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "groq".into(), title: "Groq".into(), description: "Fast hosted inference".into(), category: "Popular".into(), badge: Some("FREE".into()) },
        SelectItem { id: "ollama".into(), title: "Ollama".into(), description: "Local inference + cloud models".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "zai".into(), title: "Z.AI".into(), description: "GLM-5.1 / GLM-5 / GLM-4.7 Coding Plan".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "opencode-go".into(), title: "OpenCode Go".into(), description: "$10/mo flat-rate · Kimi · DeepSeek · GLM · MiniMax".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "opencode-zen".into(), title: "OpenCode Zen".into(), description: "Free models + paid · Nemotron · Ring · MiniMax · DeepSeek".into(), category: "Popular".into(), badge: Some("FREE".into()) },
        SelectItem { id: "synthetic".into(), title: "Synthetic.dev".into(), description: "Hosted open weights".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "routing".into(), title: "routing.run".into(), description: "Hosted open weights · DeepSeek · Llama · Mixtral · Qwen".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "neuralwatt".into(), title: "NeuralWatt".into(), description: "Hosted open weights - energy-efficient".into(), category: "Popular".into(), badge: None },
        SelectItem { id: "cerebras".into(), title: "Cerebras".into(), description: "Fast hosted inference".into(), category: "Other".into(), badge: Some("FREE".into()) },
        SelectItem { id: "sambanova".into(), title: "SambaNova".into(), description: "Fast hosted inference".into(), category: "Other".into(), badge: Some("FREE".into()) },
        SelectItem { id: "lmstudio".into(), title: "LM Studio".into(), description: "Local model server".into(), category: "Other".into(), badge: Some("LOCAL".into()) },
        SelectItem { id: "llamacpp".into(), title: "llama.cpp".into(), description: "Local inference server".into(), category: "Other".into(), badge: Some("LOCAL".into()) },
        SelectItem { id: "deepseek".into(), title: "DeepSeek".into(), description: "Reasoning and coding models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "mistral".into(), title: "Mistral".into(), description: "Hosted Mistral models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "togetherai".into(), title: "Together AI".into(), description: "Open model hosting".into(), category: "Other".into(), badge: None },
        SelectItem { id: "perplexity".into(), title: "Perplexity".into(), description: "Search-augmented models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "cohere".into(), title: "Cohere".into(), description: "Command models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "xai".into(), title: "xAI".into(), description: "Grok models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "deepinfra".into(), title: "DeepInfra".into(), description: "Hosted open models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "azure".into(), title: "Azure OpenAI".into(), description: "Enterprise OpenAI deployments".into(), category: "Other".into(), badge: None },
        SelectItem { id: "amazon-bedrock".into(), title: "AWS Bedrock".into(), description: "Enterprise foundation models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "google-vertex".into(), title: "Google Vertex AI".into(), description: "Enterprise Google models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "sap-ai-core".into(), title: "SAP AI Core".into(), description: "Enterprise AI platform".into(), category: "Other".into(), badge: None },
        SelectItem { id: "gitlab".into(), title: "GitLab Duo".into(), description: "AI in GitLab".into(), category: "Other".into(), badge: None },
        SelectItem { id: "cloudflare-ai-gateway".into(), title: "Cloudflare AI Gateway".into(), description: "Gateway for multiple providers".into(), category: "Other".into(), badge: None },
        SelectItem { id: "cloudflare-workers-ai".into(), title: "Cloudflare Workers AI".into(), description: "Edge AI inference".into(), category: "Other".into(), badge: None },
        SelectItem { id: "helicone".into(), title: "Helicone".into(), description: "AI gateway and observability".into(), category: "Other".into(), badge: None },
        SelectItem { id: "huggingface".into(), title: "Hugging Face".into(), description: "Hosted community models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "nvidia".into(), title: "NVIDIA".into(), description: "Hosted NVIDIA models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "alibaba".into(), title: "Alibaba".into(), description: "Qwen and hosted models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "venice".into(), title: "Venice AI".into(), description: "Privacy-first AI".into(), category: "Other".into(), badge: None },
        SelectItem { id: "moonshotai".into(), title: "Moonshot AI".into(), description: "Hosted Moonshot models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "zhipuai".into(), title: "Zhipu AI".into(), description: "Hosted GLM models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "siliconflow".into(), title: "SiliconFlow".into(), description: "Hosted open models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "nebius".into(), title: "Nebius".into(), description: "Cloud inference".into(), category: "Other".into(), badge: None },
        SelectItem { id: "novita".into(), title: "Novita".into(), description: "Cloud inference".into(), category: "Other".into(), badge: None },
        SelectItem { id: "minimax".into(), title: "MiniMax".into(), description: "Anthropic-compatible (M3)".into(), category: "Other".into(), badge: None },
        SelectItem { id: "ovhcloud".into(), title: "OVHcloud".into(), description: "EU-hosted AI".into(), category: "Other".into(), badge: None },
        SelectItem { id: "scaleway".into(), title: "Scaleway".into(), description: "EU cloud AI".into(), category: "Other".into(), badge: None },
        SelectItem { id: "vultr".into(), title: "Vultr".into(), description: "Cloud inference".into(), category: "Other".into(), badge: None },
        SelectItem { id: "baseten".into(), title: "Baseten".into(), description: "Model serving".into(), category: "Other".into(), badge: None },
        SelectItem { id: "friendli".into(), title: "Friendli".into(), description: "Serverless inference".into(), category: "Other".into(), badge: None },
        SelectItem { id: "upstage".into(), title: "Upstage".into(), description: "Hosted Upstage models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "stepfun".into(), title: "StepFun".into(), description: "Hosted reasoning models".into(), category: "Other".into(), badge: None },
        SelectItem { id: "fireworks".into(), title: "Fireworks AI".into(), description: "Fast inference".into(), category: "Other".into(), badge: None },
    ]
}

impl App {
    pub fn open_import_config_picker(&mut self) {
        self.import_config_picker = DialogSelectState::new("Import config", import_config_picker_items());
        self.import_config_picker.open();
    }

    pub(super) fn import_selection_from_picker(id: &str) -> Option<claurst_core::ImportSelection> {
        match id {
            "claude-md" => Some(claurst_core::ImportSelection::ClaudeMd),
            "settings" => Some(claurst_core::ImportSelection::Settings),
            "both" => Some(claurst_core::ImportSelection::Both),
            _ => None,
        }
    }

    pub(super) fn open_import_config_preview(&mut self, selection: claurst_core::ImportSelection) {
        match claurst_core::build_import_preview(selection) {
            Ok(preview) => {
                self.import_config_dialog.open(preview);
            }
            Err(err) => {
                self.status_message = Some(format!("Import failed: {}", err));
            }
        }
    }

    pub(super) fn perform_import_config(&mut self) {
        let Some(selection) = self.import_config_dialog.selection else {
            self.import_config_dialog.close();
            return;
        };
        match claurst_core::execute_import(selection) {
            Ok(result) => {
                let paths = claurst_core::ImportPaths::detect();
                let new_settings = Settings::load_sync().unwrap_or_default();
                let new_config = new_settings.effective_config();
                let result_message = claurst_core::summarize_import_result(&result, &paths);
                let imported_mcp = result.imported_fields.iter().any(|f| f == "mcpServers");
                self.config = new_config.clone();
                self.model_name = self.config.effective_model().to_string();
                self.cost_tracker.set_model(&self.model_name);
                self.refresh_context_window_size();
                self.context_used_tokens = 0;
                self.has_credentials = self.config.resolve_api_key().is_some();
                self.auth_store = claurst_core::AuthStore::load();
                self.plan_mode = matches!(
                    self.config.permission_mode,
                    claurst_core::config::PermissionMode::Plan
                );
                self.output_style = match self.config.output_style.as_deref() {
                    Some("stream") => "stream".to_string(),
                    Some("verbose") => "verbose".to_string(),
                    _ => "auto".to_string(),
                };
                if imported_mcp {
                    self.pending_mcp_reconnect = true;
                }
                self.status_message = Some(result_message);
                self.import_config_dialog.close();
            }
            Err(err) => {
                self.status_message = Some(format!("Import failed: {}", err));
                self.import_config_dialog.close();
            }
        }
    }

    pub(super) fn display_default_model_for_provider(&self, provider_id: &str) -> String {
        crate::model_picker::default_model_for_provider(provider_id, &self.model_registry)
    }

    pub(super) fn open_model_picker_for_provider(&mut self, provider_id: &str, title: Option<String>) {
        self.dismiss_error_notifications();

        let cache_path = dirs::cache_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("claurst")
            .join("models.json");
        if cache_path.exists() {
            self.model_registry.load_cache(&cache_path);
        }

        let models = crate::model_picker::models_for_provider_from_registry(
            provider_id,
            &self.model_registry,
        );
        self.model_picker.set_models(models);
        self.model_picker_provider_id = Some(provider_id.to_string());
        // Catalog-backed providers (Anthropic/OpenAI/Google) are a read-only
        // projection of the models.dev catalog — there is no live endpoint to
        // discover from, so skip the background fetch entirely and treat the
        // projection as final. Live-endpoint / curated-list providers still
        // fetch their real model list to overlay onto the projection.
        if crate::model_picker::provider_uses_catalog_projection(provider_id) {
            self.model_picker.loading_models = false;
            self.model_picker_fetch_pending = false;
        } else {
            self.model_picker.loading_models = true;
            self.model_picker_fetch_pending = true;
        }

        let provider_prefix = format!("{}/", provider_id);
        let current_model = if self.config.provider.as_deref() == Some(provider_id) {
            self.model_name
                .strip_prefix(&provider_prefix)
                .unwrap_or(self.model_name.as_str())
                .to_string()
        } else {
            let default_model = self.display_default_model_for_provider(provider_id);
            default_model
                .strip_prefix(&provider_prefix)
                .unwrap_or(default_model.as_str())
                .to_string()
        };

        self.model_picker.open_with_title(
            title.unwrap_or_else(|| "Select model".to_string()),
            &current_model,
            self.effort_level,
            self.fast_mode,
        );
    }

    pub(super) fn activate_provider(&mut self, provider_id: String, provider_name: String, status_prefix: &str) {
        let picker_title = provider_name.clone();
        self.fast_mode = false;
        self.set_provider_default(provider_id.clone());
        self.persist_provider_and_model();
        self.has_credentials = true;
        self.status_message = Some(format!("{} {}.", status_prefix, provider_name));
        self.open_model_picker_for_provider(&provider_id, Some(picker_title));
    }

    pub(super) fn persist_custom_provider_base_url(&self, base_url: &str) {
        let mut settings = Settings::load_sync().unwrap_or_default();
        let entry = settings.providers.entry("custom-openai".to_string()).or_default();
        entry.api_base = Some(base_url.to_string());
        entry.enabled = true;
        let _ = settings.save_sync();
    }

    pub(super) fn persist_provider_and_model(&self) {
        let mut settings = Settings::load_sync().unwrap_or_default();
        settings.provider = self.config.provider.clone();
        settings.config.provider = self.config.provider.clone();
        settings.config.model = self.config.model.clone();
        let _ = settings.save_sync();
    }

    pub(super) fn infer_provider_from_model(model: &str) -> Option<String> {
        // Free-mode synthetic IDs always route back through the "free"
        // composite provider so the Zen → OpenRouter fallback kicks in.
        if model == "free/auto"
            || model.starts_with("free/")
            || model.starts_with("zen/")
            || model.starts_with("opencode-zen/")
        {
            return Some("free".to_string());
        }
        if let Some((provider, _)) = model.split_once('/') {
            let known = [
                "anthropic",
                "openai",
                "google",
                "groq",
                "cerebras",
                "deepseek",
                "mistral",
                "xai",
                "openrouter",
                "github-copilot",
                "codex",
                "cohere",
                "perplexity",
                "togetherai",
                "together-ai",
                "deepinfra",
                "venice",
                "minimax",
                "ollama",
                "lmstudio",
                "llamacpp",
                "azure",
                "amazon-bedrock",
                "free",
                "opencode-zen",
            ];
            if known.contains(&provider) {
                return Some(provider.to_string());
            }
        }

        if model.starts_with("claude") {
            Some("anthropic".to_string())
        } else if model.starts_with("gpt-")
            || model.starts_with("o1")
            || model.starts_with("o3")
            || model.starts_with("o4")
        {
            Some("openai".to_string())
        } else if model.starts_with("gemini") || model.starts_with("gemma") {
            Some("google".to_string())
        } else {
            None
        }
    }

    /// Switch the active provider while clearing any explicit model override.
    pub(super) fn set_provider_default(&mut self, provider_id: String) {
        self.config.provider = Some(provider_id.clone());
        self.config.model = None;

        let model = self.display_default_model_for_provider(&provider_id);
        self.cost_tracker.set_model(&model);
        self.model_name = model;
        self.refresh_context_window_size();
        self.context_used_tokens = 0;
    }

    /// Update the active model name (also updates config + cost tracker).
    pub fn set_model(&mut self, model: String) {
        self.cost_tracker.set_model(&model);
        self.model_name = model.clone();
        self.config.model = Some(model.clone());
        if let Some(provider) = Self::infer_provider_from_model(&model) {
            self.config.provider = Some(provider);
        }
        self.refresh_context_window_size();
        // Reset used tokens when switching models (context is fresh).
        self.context_used_tokens = 0;
    }

    pub fn apply_provider_refresh(
        &mut self,
        config: Config,
        provider_registry: Option<std::sync::Arc<claurst_api::ProviderRegistry>>,
        auth_store: claurst_core::AuthStore,
        has_credentials: bool,
        status_message: String,
    ) {
        self.close_secondary_views();
        self.config = config;
        self.provider_registry = provider_registry;
        self.model_registry = claurst_api::ModelRegistry::new();
        // Re-layer user metadata overrides (issue #309) onto the fresh registry.
        self.model_registry.apply_model_overrides(&self.config.model_overrides);
        self.auth_store = auth_store;
        self.connect_dialog = DialogSelectState::new("Connect a provider", provider_picker_items());
        self.import_config_picker = DialogSelectState::new("Import config", import_config_picker_items());
        self.import_config_dialog = ImportConfigDialogState::new();
        self.model_picker = ModelPickerState::new();
        self.key_input_dialog = crate::key_input_dialog::KeyInputDialogState::new();
        self.custom_provider_dialog = crate::custom_provider_dialog::CustomProviderDialogState::new();
        self.free_mode_dialog = crate::free_mode_dialog::FreeModeDialogState::new();
        self.device_auth_dialog = crate::device_auth_dialog::DeviceAuthDialogState::new();
        self.device_auth_pending = None;
        self.pending_mcp_panel_auth = None;
        self.model_picker_fetch_pending = false;
        self.model_picker_provider_id = None;
        self.has_credentials = has_credentials;
        self.fast_mode = false;
        self.model_name = self.config.effective_model().to_string();
        self.cost_tracker.set_model(&self.model_name);
        self.status_message = Some(status_message);
        self.clear_prompt();
    }

}
