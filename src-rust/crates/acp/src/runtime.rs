//! Agent runtime for one working directory.
//!
//! Built the first time a session is opened in a directory (settings,
//! AGENTS.md context and MCP servers all resolve against that directory)
//! and shared by every later session in it. Per-session state (transcript,
//! cancellation token, permission manager and queue) is layered on top via
//! `sessions::SessionState`.

use std::path::PathBuf;
use std::sync::Arc;

use claurst_core::config::{Config, Settings};
use claurst_core::CostTracker;
use claurst_query::QueryConfig;
use claurst_tools::Tool;

/// Snapshot of the agent runtime for one working directory, shared (via
/// Arc) by every session opened in that directory.
#[derive(Clone)]
pub struct AgentRuntime {
    pub config: Config,
    pub settings: Settings,
    pub api_client: Arc<claurst_api::AnthropicClient>,
    pub provider_registry: Arc<claurst_api::ProviderRegistry>,
    pub tools: Arc<Vec<Box<dyn Tool>>>,
    pub cost_tracker: Arc<CostTracker>,
    pub query_config: QueryConfig,
    pub mcp_manager: Option<Arc<claurst_mcp::McpManager>>,
    pub working_dir: PathBuf,
    /// Whether the active provider accepts image input, so prompt images
    /// are passed through instead of being replaced by a note.
    pub supports_images: bool,
}

impl AgentRuntime {
    /// Build the runtime from the settings that apply to `working_dir`
    /// (global merged with the project's `.claurst/settings.json`), env vars,
    /// and the directory's AGENTS.md context. Mirrors the headless startup
    /// path but with ACP-specific defaults (permission decisions routed back
    /// to the connected client).
    pub async fn build(working_dir: PathBuf) -> anyhow::Result<Self> {
        let settings = match Settings::load_hierarchical(&working_dir).await {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!(error = %e, cwd = %working_dir.display(), "ACP: could not load settings, using defaults");
                Settings::default()
            }
        };
        let mut config = settings.effective_config();
        // Plan mode requires interactive UI — fall back to Default so the
        // ACP permission bridge can route decisions to the client.
        if config.permission_mode == claurst_core::PermissionMode::Plan {
            config.permission_mode = claurst_core::PermissionMode::Default;
        }
        config.project_dir = Some(working_dir.clone());

        let active_provider = config.selected_provider_id().to_string();
        let (api_key, use_bearer_auth) = if active_provider == "anthropic" {
            config
                .resolve_anthropic_auth_async()
                .await
                .unwrap_or_default()
        } else {
            (String::new(), false)
        };

        let client_config = claurst_api::client::ClientConfig {
            api_key: api_key.clone(),
            api_base: config.resolve_anthropic_api_base(),
            use_bearer_auth,
            retry: claurst_api::RetryConfig::with_max_retries(config.max_retries),
            ..Default::default()
        };
        let api_client = Arc::new(claurst_api::AnthropicClient::new(client_config.clone())?);
        let provider_registry = Arc::new(claurst_api::ProviderRegistry::from_config(
            &config,
            client_config,
        ));

        let cost_tracker = CostTracker::new();

        // MCP servers from the settings for this directory — connect upfront
        // so their tools are visible to every session here. Per-session MCP
        // servers supplied via `session/new` params are not routed yet.
        let mcp_manager = build_mcp_manager(&config, &settings, &working_dir).await;

        // Build tools: built-ins + AgentTool + the tools of every MCP server
        // that passed the trust gate.
        let mut tools: Vec<Box<dyn Tool>> = claurst_tools::all_tools();
        tools.push(Box::new(claurst_query::AgentTool));
        if let Some(manager) = &mcp_manager {
            tools.extend(claurst_tools::mcp_tools(manager));
        }
        let tools = Arc::new(tools);

        let mut query_config = QueryConfig::from_config(&config);
        query_config.working_directory = Some(working_dir.display().to_string());
        query_config.provider_registry = Some(provider_registry.clone());

        // System context (git status, ...) for this directory, assembled like
        // the CLI does.
        let context = claurst_core::context::ContextBuilder::new(working_dir.clone());
        let mut system_parts: Vec<String> = config.custom_system_prompt.iter().cloned().collect();
        system_parts.push(context.build_system_context().await);
        query_config.system_prompt = Some(system_parts.join("\n\n"));
        query_config.append_system_prompt = config.append_system_prompt.clone();

        // AGENTS.md / CLAUDE.md instruction files and the skill listing, both
        // fixed for the session (they go in the cached system block).
        if !config.disable_claude_mds {
            query_config.instruction_files = claurst_core::claudemd::load_instruction_files(
                &working_dir,
                &Settings::config_dir(),
            );
        }
        let skill_registry = {
            let cwd = working_dir.clone();
            let skills_config = config.skills.clone();
            tokio::task::spawn_blocking(move || {
                claurst_core::SkillRegistry::discover(&cwd, &skills_config)
            })
            .await?
        };
        query_config.available_skills = skill_registry.listings();

        let supports_images = provider_accepts_images(&config, &provider_registry);

        Ok(Self {
            config,
            settings,
            api_client,
            provider_registry,
            tools,
            cost_tracker,
            query_config,
            mcp_manager,
            working_dir,
            supports_images,
        })
    }
}

/// Whether the provider `config` selects accepts image input.
pub(crate) fn provider_accepts_images(
    config: &Config,
    registry: &claurst_api::ProviderRegistry,
) -> bool {
    let id = claurst_core::provider_id::ProviderId::new(config.selected_provider_id());
    registry
        .get(&id)
        .is_some_and(|provider| provider.capabilities().image_input)
}

pub(crate) async fn build_mcp_manager(
    config: &Config,
    settings: &Settings,
    working_dir: &std::path::Path,
) -> Option<Arc<claurst_mcp::McpManager>> {
    if config.mcp_servers.is_empty() {
        return None;
    }
    // SECURITY (issue #123): never auto-launch project-defined MCP servers in
    // this non-interactive runtime unless they have been trusted. The settings
    // for the session's directory include the project's own
    // `.claurst/settings.json`, whose servers are project-origin and are held
    // back here until the user approves them (TUI) or trusts them globally.
    let project_root = claurst_core::mcp_trust::project_root_for(working_dir);
    let store = claurst_core::mcp_trust::McpTrustStore::load();
    let decision = claurst_core::mcp_trust::partition_mcp_servers(
        &config.mcp_servers,
        project_root.as_deref(),
        settings.trust_project_mcp_servers,
        &std::collections::HashSet::new(),
        &store,
    );
    if !decision.pending.is_empty() {
        let names: Vec<&str> = decision.pending.iter().map(|s| s.name.as_str()).collect();
        tracing::warn!(
            servers = ?names,
            "Skipping untrusted project-defined MCP server(s) in ACP runtime"
        );
    }
    if decision.allowed.is_empty() {
        return None;
    }
    Some(claurst_mcp::McpManager::connect_all(&decision.allowed).await)
}
