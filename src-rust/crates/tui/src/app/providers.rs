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

/// Human-readable name for a provider ID (used by the `/provider` switch
/// dialog). Falls back to the raw ID for anything not listed here.
pub(super) fn provider_display_name(id: &str) -> String {
    match id {
        "anthropic" => "Anthropic",
        "openai" => "OpenAI",
        "openai-codex" | "codex" => "OpenAI Codex",
        "github-copilot" => "GitHub Copilot",
        "google" => "Google",
        "custom-openai" => "Custom OpenAI-Compatible",
        "openrouter" => "OpenRouter",
        "groq" => "Groq",
        "cerebras" => "Cerebras",
        "sambanova" => "SambaNova",
        "deepseek" => "DeepSeek",
        "mistral" => "Mistral",
        "xai" => "xAI",
        "togetherai" | "together-ai" => "Together AI",
        "perplexity" => "Perplexity",
        "cohere" => "Cohere",
        "deepinfra" => "DeepInfra",
        "azure" => "Azure OpenAI",
        "minimax" => "MiniMax",
        "nvidia" => "NVIDIA",
        "huggingface" => "Hugging Face",
        "zai" => "Z.AI",
        "zhipuai" | "zhipu" => "Zhipu AI",
        "moonshotai" | "moonshot" => "Moonshot AI",
        "siliconflow" => "SiliconFlow",
        "nebius" => "Nebius",
        "novita" => "Novita",
        "ovhcloud" => "OVHcloud",
        "scaleway" => "Scaleway",
        "vultr" => "Vultr",
        "baseten" => "Baseten",
        "friendli" => "Friendli",
        "upstage" => "Upstage",
        "stepfun" => "StepFun",
        "fireworks" => "Fireworks AI",
        "opencode-zen" => "OpenCode Zen",
        "opencode-go" => "OpenCode Go",
        "synthetic" => "Synthetic.dev",
        "routing" => "routing.run",
        "neuralwatt" => "NeuralWatt",
        "free" => "Free Mode",
        "ollama" => "Ollama",
        "lmstudio" => "LM Studio",
        "llamacpp" => "llama.cpp",
        other => other,
    }
    .to_string()
}

/// Short description for a provider ID (used by the `/provider` switch dialog).
pub(super) fn provider_description(id: &str) -> &'static str {
    match id {
        "anthropic" => "Claude models",
        "openai" => "GPT models",
        "openai-codex" | "codex" => "ChatGPT subscription",
        "github-copilot" => "GitHub subscription or token",
        "google" => "Gemini models",
        "custom-openai" => "Custom URL + API key",
        "openrouter" => "100+ models with one key",
        "groq" => "Fast hosted inference",
        "cerebras" => "Fast hosted inference",
        "sambanova" => "Fast hosted inference",
        "deepseek" => "Reasoning and coding models",
        "mistral" => "Hosted Mistral models",
        "xai" => "Grok models",
        "togetherai" | "together-ai" => "Open model hosting",
        "perplexity" => "Search-augmented models",
        "cohere" => "Command models",
        "deepinfra" => "Hosted open models",
        "azure" => "Enterprise OpenAI deployments",
        "minimax" => "Anthropic-compatible (M3)",
        "nvidia" => "Hosted NVIDIA models",
        "huggingface" => "Hosted community models",
        "zai" => "GLM coding plan",
        "zhipuai" | "zhipu" => "Hosted GLM models",
        "moonshotai" | "moonshot" => "Hosted Moonshot models",
        "siliconflow" => "Hosted open models",
        "nebius" => "Cloud inference",
        "novita" => "Cloud inference",
        "ovhcloud" => "EU-hosted AI",
        "scaleway" => "EU cloud AI",
        "vultr" => "Cloud inference",
        "baseten" => "Model serving",
        "friendli" => "Serverless inference",
        "upstage" => "Hosted Upstage models",
        "stepfun" => "Hosted reasoning models",
        "fireworks" => "Fast inference",
        "opencode-zen" => "Free models + paid",
        "opencode-go" => "Flat-rate hosted models",
        "synthetic" => "Hosted open weights",
        "routing" => "Hosted open weights",
        "neuralwatt" => "Energy-efficient inference",
        "free" => "OpenCode Zen to OpenRouter fallback",
        "ollama" => "Local model server",
        "lmstudio" => "Local model server",
        "llamacpp" => "Local inference server",
        _ => "Configured provider",
    }
}

impl App {
    pub fn open_import_config_picker(&mut self) {
        self.import_config_picker = DialogSelectState::new("Import config", import_config_picker_items());
        self.import_config_picker.open();
    }

    /// Collect the providers the user already has credentials for, as picker
    /// items for the `/provider` switch dialog. A provider counts as
    /// configured when it has a stored credential (API key or OAuth token), a
    /// provider-specific API key in settings, a resolved env var, or (for
    /// account-based providers) at least one registered account profile.
    ///
    /// The currently active provider is always included and marked with a
    /// "CURRENT" badge so the list is never empty mid-session.
    pub(super) fn configured_provider_items(&self) -> Vec<SelectItem> {
        let registry = claurst_core::accounts::AccountRegistry::load();
        let settings = Settings::load_sync().unwrap_or_default();
        let current = self.config.selected_provider_id().to_string();
        let known = [
            "anthropic",
            "openai",
            "openai-codex",
            "github-copilot",
            "google",
            "custom-openai",
            "openrouter",
            "groq",
            "cerebras",
            "sambanova",
            "deepseek",
            "mistral",
            "xai",
            "togetherai",
            "perplexity",
            "cohere",
            "deepinfra",
            "azure",
            "minimax",
            "nvidia",
            "huggingface",
            "zai",
            "zhipuai",
            "moonshotai",
            "siliconflow",
            "nebius",
            "novita",
            "ovhcloud",
            "scaleway",
            "vultr",
            "baseten",
            "friendli",
            "upstage",
            "stepfun",
            "fireworks",
            "opencode-zen",
            "opencode-go",
            "synthetic",
            "routing",
            "neuralwatt",
            "free",
            "ollama",
            "lmstudio",
            "llamacpp",
        ];

        let mut items: Vec<SelectItem> = Vec::new();
        for id in known {
            let has_account = match id {
                "anthropic" => {
                    !registry
                        .list(claurst_core::accounts::PROVIDER_ANTHROPIC)
                        .is_empty()
                }
                "openai-codex" | "codex" => {
                    !registry
                        .list(claurst_core::accounts::PROVIDER_CODEX)
                        .is_empty()
                }
                _ => false,
            };
            // `free` is a composite that needs no key; `ollama`/`lmstudio`/
            // `llamacpp` run locally. The rest need a stored key, a provider
            // settings key, or an env var.
            let is_configured = match id {
                "free" => {
                    settings
                        .providers
                        .get("free")
                        .map(|c| c.enabled)
                        .unwrap_or(false)
                        || self.config.provider.as_deref() == Some("free")
                }
                "ollama" | "lmstudio" | "llamacpp" => {
                    settings
                        .providers
                        .get(id)
                        .map(|c| c.enabled)
                        .unwrap_or(false)
                        || self.config.provider.as_deref() == Some(id)
                }
                _ => {
                    self.auth_store.api_key_for(id).is_some()
                        || settings
                            .providers
                            .get(id)
                            .and_then(|c| c.api_key.as_ref())
                            .is_some_and(|k| !k.is_empty())
                        || has_account
                }
            };
            if !is_configured && id != current {
                continue;
            }
            let is_current = id == current;
            items.push(SelectItem {
                id: id.to_string(),
                title: provider_display_name(id),
                description: provider_description(id).to_string(),
                category: if is_current {
                    "Current".to_string()
                } else {
                    "Configured".to_string()
                },
                badge: Some(if is_current { "CURRENT" } else { "READY" }.to_string()),
            });
        }

        // Always include the active provider even if it was filtered out.
        if !items.iter().any(|i| i.id == current) {
            items.insert(
                0,
                SelectItem {
                    id: current.clone(),
                    title: provider_display_name(&current),
                    description: provider_description(&current).to_string(),
                    category: "Current".to_string(),
                    badge: Some("CURRENT".to_string()),
                },
            );
        }
        items
    }

    /// Open the `/provider` switch dialog, populating it with configured
    /// providers. If none are configured, fall back to the connect flow.
    pub(super) fn open_provider_switch_dialog(&mut self) {
        let items = self.configured_provider_items();
        if items.is_empty() {
            self.connect_dialog.open();
            self.status_message =
                Some("No providers configured. Connect one first.".to_string());
            return;
        }
        self.provider_switch_dialog = DialogSelectState::new("Switch provider", items);
        self.provider_switch_dialog.open();
    }

    /// Switch to an already-configured provider without re-running its auth
    /// flow, then open the model picker for it (matching `/connect`).
    pub(super) fn switch_provider(&mut self, provider_id: String) {
        let provider_name = provider_display_name(&provider_id);
        self.activate_provider(provider_id, provider_name, "Switched to");
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
                self.sync_cost_pricing();
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
        self.model_name = model;
        self.sync_cost_pricing();
        self.refresh_context_window_size();
        self.context_used_tokens = 0;
    }

    /// Update the active model name (also updates config + cost tracker).
    pub fn set_model(&mut self, model: String) {
        self.model_name = model.clone();
        self.config.model = Some(model.clone());
        if let Some(provider) = Self::infer_provider_from_model(&model) {
            self.config.provider = Some(provider);
        }
        self.sync_cost_pricing();
        self.refresh_context_window_size();
        // Reset used tokens when switching models (context is fresh).
        self.context_used_tokens = 0;
    }

    /// Re-apply session cost pricing to the shared tracker (issue #499):
    /// zero cost for subscription providers (ChatGPT Codex, Claude Pro/Max
    /// OAuth), catalog pricing for known models, and no invented default
    /// rate for unknown ones. Keeps the TUI footer agreeing with the
    /// headless `result` event and `/cost`.
    pub fn sync_cost_pricing(&self) {
        claurst_api::apply_session_cost_pricing(
            &self.cost_tracker,
            Some(&self.model_registry),
            self.config.selected_provider_id(),
            &self.model_name,
            self.anthropic_bearer_auth,
        );
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
        self.provider_switch_dialog = DialogSelectState::new("Switch provider", Vec::new());
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
        self.anthropic_bearer_auth = false;
        self.sync_cost_pricing();
        self.status_message = Some(status_message);
        self.clear_prompt();
    }

}
