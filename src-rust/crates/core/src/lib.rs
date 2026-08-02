// cc-core: Core types, error handling, configuration, settings, and constants
// for Claurst.
//
// All sub-modules are defined inline below.

// too_many_arguments: several config-import and permission-resolution helpers
// legitimately thread many parameters; grouping them into structs is a larger
// refactor out of scope for this cleanup.
#![allow(clippy::too_many_arguments)]
// should_implement_trait: intentional inherent `from_str` constructors that
// return domain-specific types, not the std `FromStr` trait.
#![allow(clippy::should_implement_trait)]

// Branded provider / model identifier newtypes.
pub mod provider_id;
pub use provider_id::{ModelId, ProviderId};

// Session transcript persistence (JSONL, matches TS sessionStorage.ts schema).
pub mod session_storage;

// SQLite-backed session storage (faster alternative to JSONL).
pub mod sqlite_storage;
pub use sqlite_storage::{SessionSummary, SqliteSessionStore};

// Attachment pipeline — assembles per-turn context attachments (T1-6).
pub mod attachments;

// Git utilities (T4-3).
pub mod git_utils;

// Credential storage for provider API keys and OAuth tokens.
pub mod auth_store;
pub use auth_store::{AuthStore, StoredCredential};

// GitHub Device Code Flow (RFC 8628) for OAuth device authorization.
pub mod device_code;

// Utility modules ported from src/utils/
pub mod auto_mode;
pub mod crypto_utils;
pub mod format_utils;
pub mod spinner;
pub mod status_notices;
pub mod token_budget;
pub mod truncate;
pub use spinner::{
    sample_completion_verb, sample_spinner_verb, SPINNER_VERBS, TURN_COMPLETION_VERBS,
};

// Remote session sync and cloud session API (T3-1, T3-2).
pub mod cloud_session;
pub mod remote_session;

// AGENTS.md hierarchical memory loading (T4-1).
pub mod claudemd;

// Message manipulation utilities (T4-2).
pub mod message_utils;

// Per-session file modification history (T4-6).
pub mod file_history;

// Snapshot/undo system — tracks file changes per session for /undo support.
pub mod snapshot;

// Per-session durable objectives (/goal feature).
pub mod goal;
pub use goal::{
    goal_continuation_message, goal_kickoff_message, goal_system_prompt_addendum, goals_enabled,
    Goal, GoalError, GoalStatus, GoalStore, MAX_GOAL_TURNS, MAX_OBJECTIVE_CHARS,
};

// Feature flag management via GrowthBook.
pub mod feature_flags;

// MCP resource prompt template rendering with variable substitution.
pub mod mcp_templates;

// IDE environment detection (VS Code, Cursor, JetBrains, …).
pub mod ide;
pub use ide::{detect_ide, IdeKind};

// Background update checker — compares running version against GitHub releases.
pub mod update_check;
pub use update_check::{check_for_updates, UpdateInfo};

// Self-contained HTML export of a session, used by the `/share` slash command.
pub mod share_export;

// Re-export commonly used types at the crate root
pub use config::{
    builtin_managed_agent_presets, default_agents, strip_jsonc_comments, substitute_env_vars,
    AgentDefinition, BangCommandsConfig, BudgetSplitPolicy, CommandTemplate, Config,
    CustomModelDef, CustomProviderDef, FormatterConfig, ManagedAgentConfig, ManagedAgentPreset,
    McpServerConfig, McpServerOrigin, ModelVariant, OutputFormat, PermissionMode, ProviderConfig,
    Settings, SkillsConfig, Theme,
};
pub use error::{ClaudeError, Result};
pub use import_config::{
    build_import_preview, execute_import, summarize_import_result, ClaudeMdPreview,
    ImportExecutionResult, ImportPaths, ImportPreview, ImportSelection, PreviewAction,
    PreviewField, SettingsPreview,
};
pub use types::{
    CitationsConfig, ContentBlock, DocumentSource, ImageSource, Message, MessageContent,
    MessageCost, Role, ToolDefinition, ToolResultContent, UsageInfo,
};

// Skill discovery: filesystem and git URL skill loading.
pub mod skill_discovery;
pub use cost::CostTracker;
pub use feature_flags::FeatureFlagManager;
pub use history::ConversationSession;
pub use paths::claurst_home;
pub use permissions::{
    format_permission_reason, AutoPermissionHandler, InteractivePermissionHandler,
    ManagedAutoPermissionHandler, ManagedInteractivePermissionHandler, PermissionAction,
    PermissionDecision, PermissionHandler, PermissionLevel, PermissionManager, PermissionRequest,
    PermissionRule, PermissionScope, SerializedPermissionRule,
};
pub use skill_discovery::{discover_skills, parse_skill_file, DiscoveredSkill};

// ---------------------------------------------------------------------------
// error module
// ---------------------------------------------------------------------------
pub mod error {
    use thiserror::Error;

    /// The unified error type for Claurst.
    #[derive(Error, Debug)]
    pub enum ClaudeError {
        #[error("API error: {0}")]
        Api(String),

        #[error("API error {status}: {message}")]
        ApiStatus { status: u16, message: String },

        #[error("Authentication error: {0}")]
        Auth(String),

        #[error("Permission denied: {0}")]
        PermissionDenied(String),

        #[error("Tool error: {0}")]
        Tool(String),

        #[error("IO error: {0}")]
        Io(#[from] std::io::Error),

        #[error("JSON error: {0}")]
        Json(#[from] serde_json::Error),

        #[error("HTTP error: {0}")]
        Http(#[from] reqwest::Error),

        #[error("Rate limit exceeded")]
        RateLimit,

        #[error("Context window exceeded")]
        ContextWindowExceeded,

        #[error("Max tokens reached")]
        MaxTokensReached,

        #[error("Cancelled")]
        Cancelled,

        #[error("Configuration error: {0}")]
        Config(String),

        #[error("MCP error: {0}")]
        Mcp(String),

        #[error("{0}")]
        Other(String),
    }

    /// Convenience alias used throughout the project.
    pub type Result<T> = std::result::Result<T, ClaudeError>;

    impl ClaudeError {
        /// Return `true` when the caller should retry the request.
        pub fn is_retryable(&self) -> bool {
            matches!(
                self,
                ClaudeError::RateLimit
                    | ClaudeError::ApiStatus { status: 429, .. }
                    | ClaudeError::ApiStatus { status: 529, .. }
            )
        }

        /// Return `true` for errors that mean the conversation cannot continue
        /// without intervention (e.g. compaction or context-window reset).
        pub fn is_context_limit(&self) -> bool {
            matches!(
                self,
                ClaudeError::ContextWindowExceeded | ClaudeError::MaxTokensReached
            )
        }
    }
}

// ---------------------------------------------------------------------------
// types module
// ---------------------------------------------------------------------------
pub mod types {
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    // ---- Roles -----------------------------------------------------------

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "lowercase")]
    pub enum Role {
        User,
        Assistant,
    }

    // ---- Content blocks --------------------------------------------------

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ContentBlock {
        Text {
            text: String,
        },
        Image {
            source: ImageSource,
        },
        ToolUse {
            id: String,
            name: String,
            input: Value,
            /// Opaque, provider-supplied metadata that must be echoed back
            /// verbatim on subsequent turns for the tool call to be accepted.
            /// Currently carries Google Gemini's `thoughtSignature` for thinking
            /// models (issue #311); `None` for every other provider. Persisted
            /// with the session so it survives save/load.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            thought_signature: Option<String>,
        },
        ToolResult {
            tool_use_id: String,
            content: ToolResultContent,
            #[serde(skip_serializing_if = "Option::is_none")]
            is_error: Option<bool>,
        },
        Thinking {
            thinking: String,
            signature: String,
        },
        RedactedThinking {
            data: String,
        },
        Document {
            source: DocumentSource,
            #[serde(skip_serializing_if = "Option::is_none")]
            title: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            context: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            citations: Option<CitationsConfig>,
        },
        /// A `!`-prefixed shell command invoked by the user, with its captured output.
        /// Rendered as a faint gray block with a `!command` header.
        UserLocalCommandOutput {
            command: String,
            output: String,
        },
        /// A skill/slash-command invocation entered by the user.
        /// Rendered as `▸ name args` with cyan styling.
        UserCommand {
            name: String,
            args: String,
        },
        /// A memory key/value written by the user (e.g. via `/memory`).
        /// Rendered as `# key: value` in cyan with a `Got it.` footer.
        UserMemoryInput {
            key: String,
            value: String,
        },
        /// A system-level API error, rendered as a red-bordered block.
        /// Shows first 5 lines with `[expand]` hint when truncated, and an
        /// optional `Retrying in Ns...` countdown line when `retry_secs` is set.
        SystemAPIError {
            message: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            retry_secs: Option<u32>,
        },
        /// A collapsed summary of multiple read/search tool calls.
        /// Rendered as `▸ Read N files (+ M more)` on a single line.
        CollapsedReadSearch {
            tool_name: String,
            paths: Vec<String>,
            n_hidden: usize,
        },
        /// A sub-task assignment in an agentic workflow.
        /// Rendered as a cyan-bordered box with Task ID, subject, and description.
        TaskAssignment {
            id: String,
            subject: String,
            description: String,
        },
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(untagged)]
    pub enum ToolResultContent {
        Text(String),
        Blocks(Vec<ContentBlock>),
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ImageSource {
        #[serde(rename = "type")]
        pub source_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub media_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub data: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub url: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DocumentSource {
        #[serde(rename = "type")]
        pub source_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub media_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub data: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub url: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct CitationsConfig {
        pub enabled: bool,
    }

    // ---- Messages --------------------------------------------------------

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Message {
        pub role: Role,
        pub content: MessageContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub uuid: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub cost: Option<MessageCost>,
        /// Files changed during this assistant turn, captured by the shadow snapshot.
        /// Populated by the query loop on `finish-step`; absent on user messages.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub snapshot_patch: Option<crate::snapshot::Patch>,
        /// Marks this user message as an auto-poke continuation (not typed by
        /// the user). Used only by the TUI for distinct rendering. Not sent to
        /// the API — providers see it as a normal `Role::User` message.
        #[serde(default, skip)]
        pub is_auto_poke: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(untagged)]
    pub enum MessageContent {
        Text(String),
        Blocks(Vec<ContentBlock>),
    }

    impl Message {
        /// Create a simple user text message.
        pub fn user(content: impl Into<String>) -> Self {
            Self {
                role: Role::User,
                content: MessageContent::Text(content.into()),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                is_auto_poke: false,
            }
        }

        /// Create a user message composed of multiple content blocks.
        pub fn user_blocks(blocks: Vec<ContentBlock>) -> Self {
            Self {
                role: Role::User,
                content: MessageContent::Blocks(blocks),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                is_auto_poke: false,
            }
        }

        /// Create a simple assistant text message.
        pub fn assistant(content: impl Into<String>) -> Self {
            Self {
                role: Role::Assistant,
                content: MessageContent::Text(content.into()),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                is_auto_poke: false,
            }
        }

        /// Create an assistant message composed of multiple content blocks.
        pub fn assistant_blocks(blocks: Vec<ContentBlock>) -> Self {
            Self {
                role: Role::Assistant,
                content: MessageContent::Blocks(blocks),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                is_auto_poke: false,
            }
        }

        /// Create an auto-poke continuation user message. Rendered distinctly
        /// in the TUI but sent to the provider as a normal user message.
        pub fn auto_poke(content: impl Into<String>) -> Self {
            Self {
                role: Role::User,
                content: MessageContent::Text(content.into()),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                is_auto_poke: true,
            }
        }

        /// Extract the first text content from this message.
        pub fn get_text(&self) -> Option<&str> {
            match &self.content {
                MessageContent::Text(t) => Some(t.as_str()),
                MessageContent::Blocks(blocks) => blocks.iter().find_map(|b| {
                    if let ContentBlock::Text { text } = b {
                        Some(text.as_str())
                    } else {
                        None
                    }
                }),
            }
        }

        /// Collect all text content blocks into one concatenated string.
        pub fn get_all_text(&self) -> String {
            match &self.content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| {
                        if let ContentBlock::Text { text } = b {
                            Some(text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            }
        }

        /// Return references to all `ToolUse` blocks in this message.
        pub fn get_tool_use_blocks(&self) -> Vec<&ContentBlock> {
            match &self.content {
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
                    .collect(),
                _ => vec![],
            }
        }

        /// Return references to all `ToolResult` blocks in this message.
        pub fn get_tool_result_blocks(&self) -> Vec<&ContentBlock> {
            match &self.content {
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
                    .collect(),
                _ => vec![],
            }
        }

        /// Return references to all `Thinking` blocks in this message.
        pub fn get_thinking_blocks(&self) -> Vec<&ContentBlock> {
            match &self.content {
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Thinking { .. }))
                    .collect(),
                _ => vec![],
            }
        }

        /// Returns all content blocks (wrapping a single text into a vec).
        pub fn content_blocks(&self) -> Vec<ContentBlock> {
            match &self.content {
                MessageContent::Text(t) => vec![ContentBlock::Text { text: t.clone() }],
                MessageContent::Blocks(b) => b.clone(),
            }
        }

        /// Check whether this message has any tool use blocks.
        pub fn has_tool_use(&self) -> bool {
            !self.get_tool_use_blocks().is_empty()
        }

        /// Create a user message representing a `!`-prefixed local shell command with output.
        pub fn user_local_command_output(
            command: impl Into<String>,
            output: impl Into<String>,
        ) -> Self {
            Self {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::UserLocalCommandOutput {
                    command: command.into(),
                    output: output.into(),
                }]),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                is_auto_poke: false,
            }
        }

        /// Create a user message representing a skill/slash-command invocation.
        pub fn user_command(name: impl Into<String>, args: impl Into<String>) -> Self {
            Self {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::UserCommand {
                    name: name.into(),
                    args: args.into(),
                }]),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                is_auto_poke: false,
            }
        }

        /// Create a user message representing a memory key/value entry.
        pub fn user_memory_input(key: impl Into<String>, value: impl Into<String>) -> Self {
            Self {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::UserMemoryInput {
                    key: key.into(),
                    value: value.into(),
                }]),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                is_auto_poke: false,
            }
        }

        /// Create a system message representing an API error (red-bordered block).
        pub fn system_api_error(message: impl Into<String>, retry_secs: Option<u32>) -> Self {
            Self {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::SystemAPIError {
                    message: message.into(),
                    retry_secs,
                }]),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                is_auto_poke: false,
            }
        }

        /// Create a system message representing a collapsed read/search summary.
        pub fn collapsed_read_search(
            tool_name: impl Into<String>,
            paths: Vec<String>,
            n_hidden: usize,
        ) -> Self {
            Self {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::CollapsedReadSearch {
                    tool_name: tool_name.into(),
                    paths,
                    n_hidden,
                }]),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                is_auto_poke: false,
            }
        }

        /// Create a system message representing a sub-task assignment.
        pub fn task_assignment(
            id: impl Into<String>,
            subject: impl Into<String>,
            description: impl Into<String>,
        ) -> Self {
            Self {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::TaskAssignment {
                    id: id.into(),
                    subject: subject.into(),
                    description: description.into(),
                }]),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                is_auto_poke: false,
            }
        }
    }

    // ---- Cost / usage ----------------------------------------------------

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    pub struct MessageCost {
        pub input_tokens: u64,
        pub output_tokens: u64,
        pub cache_creation_input_tokens: u64,
        pub cache_read_input_tokens: u64,
        pub cost_usd: f64,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ToolDefinition {
        pub name: String,
        pub description: String,
        pub input_schema: Value,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    pub struct UsageInfo {
        pub input_tokens: u64,
        pub output_tokens: u64,
        #[serde(default)]
        pub cache_creation_input_tokens: u64,
        #[serde(default)]
        pub cache_read_input_tokens: u64,
    }

    impl UsageInfo {
        pub fn total_input(&self) -> u64 {
            self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
        }

        pub fn total(&self) -> u64 {
            self.total_input() + self.output_tokens
        }
    }
}

// ---------------------------------------------------------------------------
// config module
// ---------------------------------------------------------------------------
pub mod config {
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    // ---- Hook configuration ----------------------------------------------

    /// Events that can trigger hooks.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
    #[serde(rename_all = "PascalCase")]
    pub enum HookEvent {
        /// Fires before a tool is executed.
        PreToolUse,
        /// Fires after a tool has returned its result.
        PostToolUse,
        /// Fires when the model finishes its turn (stop).
        Stop,
        /// Fires after the model samples a response, before tool execution.
        /// Corresponds to `hooks.PostModelTurn` in settings.json.
        PostModelTurn,
        /// Fires when the user submits a prompt.
        UserPromptSubmit,
        /// General-purpose notification event.
        Notification,
    }

    /// A single hook entry: a shell command to run on a specific event.
    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    pub struct HookEntry {
        /// Shell command to execute. Receives event JSON on stdin.
        pub command: String,
        /// Optional tool name filter — only run for this tool (PreToolUse/PostToolUse).
        #[serde(skip_serializing_if = "Option::is_none")]
        pub tool_filter: Option<String>,
        /// If true, a non-zero exit code blocks the operation.
        #[serde(default)]
        pub blocking: bool,
    }

    // ---- AgentDefinition -------------------------------------------------

    fn default_agent_access() -> String {
        "full".to_string()
    }

    fn default_true() -> bool {
        true
    }

    fn default_file_autocomplete_limit() -> usize {
        15
    }

    fn default_file_injection_max_size() -> usize {
        100 // 100 KB
    }

    /// Default total request timeout (seconds) when the user has not configured
    /// one. Generous so slow local models (CPU inference, large MoE) that can
    /// take several minutes to first token are not cut off prematurely.
    pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 600;

    /// Definition of a named agent with per-agent model, permissions,
    /// temperature, and system prompt.
    pub fn api_key_env_vars_for_provider(provider_id: &str) -> &'static [&'static str] {
        match provider_id {
            "anthropic" => &["ANTHROPIC_API_KEY"],
            "openai" => &["OPENAI_API_KEY"],
            "google" | "google-vertex" => &["GOOGLE_API_KEY", "GOOGLE_GENERATIVE_AI_API_KEY"],
            "github-copilot" => &["GITHUB_TOKEN"],
            "groq" => &["GROQ_API_KEY"],
            "cerebras" => &["CEREBRAS_API_KEY"],
            "sambanova" => &["SAMBANOVA_API_KEY"],
            "deepseek" => &["DEEPSEEK_API_KEY"],
            "mistral" => &["MISTRAL_API_KEY"],
            "openrouter" => &["OPENROUTER_API_KEY"],
            "togetherai" | "together-ai" => &["TOGETHER_API_KEY"],
            "perplexity" => &["PERPLEXITY_API_KEY"],
            "cohere" => &["COHERE_API_KEY"],
            "xai" => &["XAI_API_KEY"],
            "deepinfra" => &["DEEPINFRA_API_KEY"],
            "azure" => &["AZURE_API_KEY"],
            "gitlab" => &["GITLAB_TOKEN"],
            "huggingface" => &["HF_TOKEN"],
            "nvidia" => &["NVIDIA_API_KEY"],
            "alibaba" | "qwen" => &["DASHSCOPE_API_KEY"],
            "venice" => &["VENICE_API_KEY"],
            "moonshot" | "moonshotai" => &["MOONSHOT_API_KEY"],
            "zhipu" | "zhipuai" => &["ZHIPU_API_KEY"],
            "zai" => &["ZAI_API_KEY"],
            "siliconflow" => &["SILICONFLOW_API_KEY"],
            "nebius" => &["NEBIUS_API_KEY"],
            "novita" => &["NOVITA_API_KEY"],
            "minimax" => &["MINIMAX_API_KEY"],
            "ovhcloud" => &["OVHCLOUD_API_KEY"],
            "scaleway" => &["SCALEWAY_API_KEY"],
            "vultr" | "vultr-ai" => &["VULTR_API_KEY"],
            "baseten" => &["BASETEN_API_KEY"],
            "friendli" => &["FRIENDLI_TOKEN"],
            "upstage" => &["UPSTAGE_API_KEY"],
            "stepfun" => &["STEPFUN_API_KEY"],
            "fireworks" => &["FIREWORKS_API_KEY"],
            "cloudflare" | "cloudflare-ai-gateway" | "cloudflare-workers-ai" => {
                &["CLOUDFLARE_API_TOKEN"]
            }
            "vercel" => &["AI_GATEWAY_API_KEY"],
            "helicone" => &["HELICONE_API_KEY"],
            "sap" | "sap-ai-core" => &["AICORE_SERVICE_KEY"],
            _ => &[],
        }
    }

    pub fn primary_api_key_env_var_for_provider(provider_id: &str) -> Option<&'static str> {
        api_key_env_vars_for_provider(provider_id).first().copied()
    }

    pub fn api_base_env_var_for_provider(provider_id: &str) -> Option<&'static str> {
        match provider_id {
            "anthropic" => Some("ANTHROPIC_BASE_URL"),
            "openai" => Some("OPENAI_BASE_URL"),
            "minimax" => Some("MINIMAX_BASE_URL"),
            "ollama" => Some("OLLAMA_HOST"),
            "lmstudio" | "lm-studio" => Some("LM_STUDIO_HOST"),
            "llamacpp" | "llama-cpp" | "llama-server" => Some("LLAMA_CPP_HOST"),
            _ => None,
        }
    }

    pub fn default_api_base_for_provider(provider_id: &str) -> Option<&'static str> {
        match provider_id {
            "anthropic" => Some(crate::constants::ANTHROPIC_API_BASE),
            "openai" => Some("https://api.openai.com"),
            "minimax" => Some(crate::constants::MINIMAX_ANTHROPIC_API_BASE),
            "ollama" => Some("http://localhost:11434"),
            "lmstudio" | "lm-studio" => Some("http://localhost:1234"),
            "llamacpp" | "llama-cpp" | "llama-server" => Some("http://localhost:8080"),
            _ => None,
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AgentDefinition {
        /// Display name / description
        pub description: Option<String>,
        /// Model override for this agent (e.g., "anthropic/claude-haiku-4-5")
        pub model: Option<String>,
        /// Temperature override
        pub temperature: Option<f64>,
        /// System prompt prefix (prepended before the main system prompt)
        pub prompt: Option<String>,
        /// Permission restriction: "full", "read-only", "search-only"
        #[serde(default = "default_agent_access")]
        pub access: String,
        /// Whether to show in @agent autocomplete
        #[serde(default = "default_true")]
        pub visible: bool,
        /// Max agentic turns for this agent (overrides global)
        pub max_turns: Option<u32>,
        /// ANSI color for display: "cyan", "magenta", "green", etc.
        pub color: Option<String>,
    }

    impl Default for AgentDefinition {
        fn default() -> Self {
            Self {
                description: None,
                model: None,
                temperature: None,
                prompt: None,
                access: default_agent_access(),
                visible: true,
                max_turns: None,
                color: None,
            }
        }
    }

    // ---- ManagedAgentConfig ----------------------------------------------

    /// Budget allocation strategy between manager and executor agents.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    #[derive(Default)]
    pub enum BudgetSplitPolicy {
        /// Shared pool — no split (default).
        #[default]
        SharedPool,
        /// Manager gets manager_pct% of total budget.
        Percentage { manager_pct: u8 },
        /// Hard USD caps per role.
        FixedCaps { manager_usd: f64, executor_usd: f64 },
    }

    /// Configuration for manager-executor agent architecture.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ManagedAgentConfig {
        pub enabled: bool,
        /// "provider/model" string, e.g. "anthropic/claude-opus-4-6"
        pub manager_model: String,
        /// "provider/model" string, e.g. "anthropic/claude-sonnet-4-6"
        pub executor_model: String,
        #[serde(default = "default_executor_max_turns")]
        pub executor_max_turns: u32,
        #[serde(default = "default_max_concurrent_executors")]
        pub max_concurrent_executors: u32,
        #[serde(default)]
        pub budget_split: BudgetSplitPolicy,
        #[serde(default)]
        pub total_budget_usd: Option<f64>,
        #[serde(default)]
        pub preset_name: Option<String>,
        #[serde(default)]
        pub executor_isolation: bool,
    }

    fn default_executor_max_turns() -> u32 {
        10
    }
    fn default_max_concurrent_executors() -> u32 {
        4
    }

    /// A named preset for common manager-executor configurations.
    pub struct ManagedAgentPreset {
        pub name: &'static str,
        pub label: &'static str,
        pub description: &'static str,
        pub manager_model: &'static str,
        pub executor_model: &'static str,
        pub executor_max_turns: u32,
        pub max_concurrent_executors: u32,
    }

    pub fn builtin_managed_agent_presets() -> Vec<ManagedAgentPreset> {
        vec![
            ManagedAgentPreset {
                name: "anthropic-tiered",
                label: "Anthropic Tiered",
                description: "Opus 4.6 manages, Sonnet 4.6 executes (best quality)",
                manager_model: "anthropic/claude-opus-4-6",
                executor_model: "anthropic/claude-sonnet-4-6",
                executor_max_turns: 10,
                max_concurrent_executors: 4,
            },
            ManagedAgentPreset {
                name: "anthropic-budget",
                label: "Anthropic Budget",
                description: "Sonnet 4.6 manages, Haiku 4.5 executes (cost-optimized)",
                manager_model: "anthropic/claude-sonnet-4-6",
                executor_model: "anthropic/claude-haiku-4-5-20251001",
                executor_max_turns: 10,
                max_concurrent_executors: 6,
            },
            ManagedAgentPreset {
                name: "google-tiered",
                label: "Google Tiered",
                description: "Gemini 2.5 Pro manages, Flash executes",
                manager_model: "google/gemini-2.5-pro",
                executor_model: "google/gemini-2.5-flash",
                executor_max_turns: 10,
                max_concurrent_executors: 4,
            },
            ManagedAgentPreset {
                name: "cross-opus-flash",
                label: "Cross: Opus + Flash",
                description: "Anthropic Opus manages, Google Flash executes (cheapest executors)",
                manager_model: "anthropic/claude-opus-4-6",
                executor_model: "google/gemini-2.5-flash",
                executor_max_turns: 10,
                max_concurrent_executors: 6,
            },
            ManagedAgentPreset {
                name: "openai-tiered",
                label: "OpenAI Tiered",
                description: "o3 manages, gpt-4o executes",
                manager_model: "openai/o3",
                executor_model: "openai/gpt-4o",
                executor_max_turns: 10,
                max_concurrent_executors: 4,
            },
            ManagedAgentPreset {
                name: "cross-openai-anthropic",
                label: "Cross: OpenAI + Anthropic",
                description: "o3 manages, Sonnet 4.6 executes",
                manager_model: "openai/o3",
                executor_model: "anthropic/claude-sonnet-4-6",
                executor_max_turns: 10,
                max_concurrent_executors: 4,
            },
        ]
    }

    // ---- ProviderConfig --------------------------------------------------

    /// Per-provider configuration: API keys, base URLs, and options.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ProviderConfig {
        /// API key (overrides environment variable)
        pub api_key: Option<String>,
        /// Override the default base URL for this provider
        pub api_base: Option<String>,
        /// Whether this provider is enabled (default: true)
        #[serde(default = "default_true")]
        pub enabled: bool,
        /// Model ID whitelist (empty = allow all)
        #[serde(default)]
        pub models_whitelist: Vec<String>,
        /// Model ID blacklist
        #[serde(default)]
        pub models_blacklist: Vec<String>,
        /// Provider-specific options (passed through to provider implementation)
        #[serde(default)]
        pub options: HashMap<String, serde_json::Value>,
        /// Total request timeout in seconds for this provider's HTTP client.
        /// Overrides the global [`Config::request_timeout_secs`] when set.
        /// Useful for slow local models (CPU inference, large MoE) that can take
        /// several minutes to first token. `None` falls back to the global value.
        #[serde(
            default,
            rename = "requestTimeoutSecs",
            alias = "request_timeout_secs",
            skip_serializing_if = "Option::is_none"
        )]
        pub request_timeout_secs: Option<u64>,
    }

    impl Default for ProviderConfig {
        fn default() -> Self {
            Self {
                api_key: None,
                api_base: None,
                enabled: true,
                models_whitelist: Vec::new(),
                models_blacklist: Vec::new(),
                options: HashMap::new(),
                request_timeout_secs: None,
            }
        }
    }
    /// the model picker, token warnings, and auto-compact thresholds size it
    /// correctly instead of mismatching it to an unrelated catalog model.
    ///
    /// Field names accept both camelCase (`contextWindow`) and snake_case
    /// (`context_window`) so the override reads naturally whether it lives at the
    /// top level of `settings.json` or under the nested `config` block.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct ModelOverride {
        /// Total context window size in tokens.
        #[serde(
            default,
            rename = "contextWindow",
            alias = "context_window",
            skip_serializing_if = "Option::is_none"
        )]
        pub context_window: Option<u32>,
        /// Maximum tokens the model can emit in a single response.
        #[serde(
            default,
            rename = "maxOutputTokens",
            alias = "max_output_tokens",
            skip_serializing_if = "Option::is_none"
        )]
        pub max_output_tokens: Option<u32>,
        /// Human-readable display name shown in the model picker.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub name: Option<String>,
        /// First public availability (ISO 8601 date), drives date-DESC listing.
        #[serde(
            default,
            rename = "releaseDate",
            alias = "release_date",
            skip_serializing_if = "Option::is_none"
        )]
        pub release_date: Option<String>,
        /// Lifecycle status string (`"active"`, `"beta"`, …).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub status: Option<String>,
    }

    impl ModelOverride {
        /// Whether this override carries no data (every field is `None`).
        pub fn is_empty(&self) -> bool {
            self.context_window.is_none()
                && self.max_output_tokens.is_none()
                && self.name.is_none()
                && self.release_date.is_none()
                && self.status.is_none()
        }
    }

    // ---- ModelVariant ----------------------------------------------------

    /// A model variant that overrides specific fields from its parent model
    /// definition (e.g. a "max" variant that changes only `reasoning_effort`).
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct ModelVariant {
        /// Override for reasoning effort level.
        #[serde(
            default,
            rename = "reasoningEffort",
            alias = "reasoning_effort",
            skip_serializing_if = "Option::is_none"
        )]
        pub reasoning_effort: Option<String>,
    }

    // ---- CustomModelDef -------------------------------------------------

    /// A model definition within a custom OpenAI-compatible provider.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct CustomModelDef {
        /// Total context window size in tokens.
        #[serde(
            default,
            rename = "contextWindow",
            alias = "context_window",
            skip_serializing_if = "Option::is_none"
        )]
        pub context_window: Option<u32>,
        /// Maximum tokens the model can emit in a single response.
        #[serde(
            default,
            rename = "maxOutputTokens",
            alias = "max_output_tokens",
            skip_serializing_if = "Option::is_none"
        )]
        pub max_output_tokens: Option<u32>,
        /// Human-readable display name shown in the model picker.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub name: Option<String>,
        /// Reasoning effort level (e.g. "high", "max", "none").
        #[serde(
            default,
            rename = "reasoningEffort",
            alias = "reasoning_effort",
            skip_serializing_if = "Option::is_none"
        )]
        pub reasoning_effort: Option<String>,
        /// Named variants that override specific fields from this model definition.
        #[serde(default)]
        pub variants: HashMap<String, ModelVariant>,
    }

    // ---- CustomProviderDef ----------------------------------------------

    /// A user-defined OpenAI-compatible provider, stored in
    /// [`Settings::custom_providers`] keyed by its unique id.
    ///
    /// Unlike [`ProviderConfig`] (which tweaks built-in providers), this is a
    /// fully self-contained provider definition with its own base URL, API key,
    /// custom headers, and model catalog. The id (map key) becomes the provider
    /// id used in `provider/model` routing.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct CustomProviderDef {
        /// Human-readable display name shown in the provider picker.
        #[serde(default)]
        pub name: String,
        /// OpenAI-compatible base URL (claurst appends `/chat/completions`).
        #[serde(rename = "apiBase", alias = "api_base")]
        pub api_base: String,
        /// API key. Supports `{env:VAR_NAME}` substitution at load time.
        /// `None` means no key — provider is registered but health_check
        /// returns `Unavailable`.
        #[serde(
            rename = "apiKey",
            alias = "api_key",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        pub api_key: Option<String>,
        /// Custom HTTP headers sent on every request to this provider.
        #[serde(default)]
        pub headers: HashMap<String, String>,
        /// Model catalog local to this provider, keyed by model id.
        #[serde(default)]
        pub models: HashMap<String, CustomModelDef>,
        /// Per-provider request timeout override in seconds.
        #[serde(
            default,
            rename = "requestTimeoutSecs",
            alias = "request_timeout_secs",
            skip_serializing_if = "Option::is_none"
        )]
        pub request_timeout_secs: Option<u64>,
        /// Whether streaming is enabled for this provider. Default: true
        /// (streaming). Set to false for providers that don't support SSE.
        #[serde(default, rename = "streaming", skip_serializing_if = "Option::is_none")]
        pub streaming: Option<bool>,
        /// Whether to send stream_options.include_usage when streaming.
        /// Default: true. Set to false for providers that don't support it.
        #[serde(
            default,
            rename = "includeUsageInStream",
            alias = "include_usage_in_stream",
            skip_serializing_if = "Option::is_none"
        )]
        pub include_usage_in_stream: Option<bool>,
        /// JSON field name for reasoning/thinking content (e.g.
        /// "reasoning_content" for DeepSeek). None means no reasoning field.
        #[serde(
            default,
            rename = "reasoningField",
            alias = "reasoning_field",
            skip_serializing_if = "Option::is_none"
        )]
        pub reasoning_field: Option<String>,
    }

    impl CustomProviderDef {
        /// Resolve the API key, substituting `{env:VAR_NAME}` patterns with
        /// the corresponding environment variable value. Returns `None` if
        /// the key is absent or the env var is unset.
        pub fn resolve_api_key(&self) -> Option<String> {
            self.api_key.as_ref().and_then(|raw| {
                if let Some(var) = raw.strip_prefix("{env:").and_then(|s| s.strip_suffix('}')) {
                    std::env::var(var).ok().filter(|v| !v.is_empty())
                } else if raw.is_empty() {
                    None
                } else {
                    Some(raw.clone())
                }
            })
        }
    }

    // ---- Config ----------------------------------------------------------

    /// Top-level configuration values, merged from CLI args + settings file + env.
    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    pub struct Config {
        pub api_key: Option<String>,
        pub model: Option<String>,
        pub max_tokens: Option<u32>,
        pub permission_mode: PermissionMode,
        pub theme: Theme,
        #[serde(default)]
        pub output_style: Option<String>,
        pub auto_compact: bool,
        pub compact_threshold: f32,
        pub verbose: bool,
        pub output_format: OutputFormat,
        pub mcp_servers: Vec<McpServerConfig>,
        #[serde(default)]
        pub lsp_servers: Vec<crate::lsp::LspServerConfig>,
        pub allowed_tools: Vec<String>,
        pub disallowed_tools: Vec<String>,
        pub env: HashMap<String, String>,
        pub enable_all_mcp_servers: bool,
        pub custom_system_prompt: Option<String>,
        pub append_system_prompt: Option<String>,
        pub disable_claude_mds: bool,
        pub project_dir: Option<PathBuf>,
        #[serde(default)]
        pub workspace_paths: Vec<PathBuf>,
        /// Additional directories granted access via --add-dir.
        #[serde(default)]
        pub additional_dirs: Vec<PathBuf>,
        /// Event hooks: map of event → list of hook commands.
        #[serde(default)]
        pub hooks: HashMap<HookEvent, Vec<HookEntry>>,
        /// Active provider ID (default: "anthropic")
        #[serde(default)]
        pub provider: Option<String>,
        /// Per-provider configurations
        #[serde(default)]
        pub provider_configs: HashMap<String, ProviderConfig>,
        /// User-supplied model metadata overrides, keyed by `"provider/model"`.
        /// Take precedence over the models.dev catalog (copied from Settings on
        /// load; see [`ModelOverride`]).
        #[serde(default, rename = "modelOverrides", alias = "model_overrides")]
        pub model_overrides: HashMap<String, ModelOverride>,
        /// Formatter configurations (copied from Settings on load).
        #[serde(default)]
        pub formatter: HashMap<String, FormatterConfig>,
        /// User-defined command templates (copied from Settings on load).
        #[serde(default)]
        pub commands: HashMap<String, CommandTemplate>,
        /// Named agent definitions (copied from Settings on load).
        #[serde(default)]
        pub agents: HashMap<String, AgentDefinition>,
        /// Skill-discovery configuration (copied from Settings on load).
        #[serde(default)]
        pub skills: SkillsConfig,
        /// Managed agent (manager-executor) configuration.
        #[serde(default)]
        pub managed_agents: Option<ManagedAgentConfig>,
        /// Shadow-git auto-commit snapshot system.  `Some(true)` = enabled.  `None` or `Some(false)` = disabled (default).
        /// Set via `--auto-commits` flag or `"autoCommits": true` in settings.json.
        #[serde(
            default,
            rename = "autoCommits",
            skip_serializing_if = "Option::is_none"
        )]
        pub auto_commits: Option<bool>,
        /// Enable cursor blinking in the chat prompt. Defaults to false (disabled).
        #[serde(
            default,
            rename = "cursorBlinkEnabled",
            skip_serializing_if = "is_false"
        )]
        pub cursor_blink_enabled: bool,
        /// Maximum number of file suggestions shown in autocomplete. Defaults to 15.
        #[serde(
            default = "default_file_autocomplete_limit",
            rename = "fileAutocompleteLimit"
        )]
        pub file_autocomplete_limit: usize,
        /// Whether to show hidden files in file autocomplete. Defaults to false.
        #[serde(default, rename = "fileAutocompleteShowHiddenFiles")]
        pub file_autocomplete_show_hidden_files: bool,
        /// Whether @ file references are automatically injected into message context. Defaults to true.
        /// When true: @file auto-injects file contents into your message before sending.
        /// When false: @ is just autocomplete and reference (no auto-injection).
        /// Note: This only affects user messages. @include in CLAUDE.md/AGENTS.md always injects with no size limits.
        #[serde(default = "default_true", rename = "fileInjectionEnabled")]
        pub file_injection_enabled: bool,
        /// Maximum file size to auto-inject (in KB). Defaults to 100. Set to 0 for no limit.
        /// When a file exceeds this limit, users get a warning and can choose to override or cancel.
        /// Note: @include in CLAUDE.md/AGENTS.md always injects regardless of this limit.
        #[serde(
            default = "default_file_injection_max_size",
            rename = "fileInjectionMaxSize"
        )]
        pub file_injection_max_size: usize,
        /// Total request timeout in seconds applied to provider HTTP clients.
        /// Slow local models (CPU inference, large MoE) can take several minutes
        /// to first token; raise this to avoid premature cut-off. `None` (or 0)
        /// uses [`DEFAULT_REQUEST_TIMEOUT_SECS`]. Per-provider overrides live on
        /// [`ProviderConfig::request_timeout_secs`].
        #[serde(
            default,
            rename = "requestTimeoutSecs",
            alias = "request_timeout_secs",
            skip_serializing_if = "Option::is_none"
        )]
        pub request_timeout_secs: Option<u64>,
        /// Whether app-level mouse capture is enabled. `None` (default) or
        /// `Some(true)` means claurst captures the mouse for scroll / right-click
        /// context menu / middle-click paste / drag text-selection. Set
        /// `"mouseCapture": false` to release the mouse to the terminal so native
        /// click-drag selection and copy/paste work without lag (issue #104).
        /// Keyboard scrolling (PageUp/PageDown, etc.) is unaffected either way.
        #[serde(
            default,
            rename = "mouseCapture",
            skip_serializing_if = "Option::is_none"
        )]
        pub mouse_capture: Option<bool>,
        /// Whether the max-steps graceful degradation summary turn is enabled.
        /// When `true` (default), exceeding `max_turns` runs one final
        /// tool-less turn asking the model to summarize progress. When `false`,
        /// the loop returns cold (last assistant message) immediately.
        #[serde(default = "default_true", rename = "degradationSummaryEnabled")]
        pub degradation_summary_enabled: bool,
        /// When true, the TUI log shows only the tool type (e.g. "Running
        /// command") without the command content or summary.
        #[serde(default, rename = "hideToolContent")]
        pub hide_tool_content: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
    #[serde(rename_all = "camelCase")]
    pub enum PermissionMode {
        #[default]
        Default,
        AcceptEdits,
        BypassPermissions,
        Plan,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    pub enum Theme {
        #[default]
        Default,
        Dark,
        Light,
        Custom(String),
        Deuteranopia,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    #[serde(rename_all = "lowercase")]
    pub enum OutputFormat {
        #[default]
        Text,
        Json,
        StreamJson,
    }

    /// Where an MCP server definition came from.
    ///
    /// This is a *runtime* classification used to gate auto-launching of
    /// servers that can run arbitrary commands. It is deliberately NEVER
    /// (de)serialized from the settings file (see `#[serde(skip)]` on
    /// `McpServerConfig::origin`): a repository's `.claurst/settings.json`
    /// must not be able to forge `User` to bypass the trust gate. The origin
    /// is always assigned in code at load time.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    pub enum McpServerOrigin {
        /// Defined in the user's global `~/.claurst/settings.json`, supplied
        /// on the command line (`--mcp-config`), or contributed by an
        /// explicitly-enabled plugin. Considered trusted: auto-connects.
        #[default]
        User,
        /// Defined in a repository's project-level `.claurst/settings.json`.
        /// Untrusted until the user approves it, because opening a cloned repo
        /// would otherwise spawn an attacker-controlled process (RCE).
        Project,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct McpServerConfig {
        pub name: String,
        pub command: Option<String>,
        #[serde(default)]
        pub args: Vec<String>,
        #[serde(default)]
        pub env: HashMap<String, String>,
        pub url: Option<String>,
        #[serde(rename = "type", default = "default_mcp_type")]
        pub server_type: String,
        /// Origin of this definition. Never read from JSON (always `User` on
        /// deserialize); set to `Project` in `find_project_settings` for
        /// servers loaded from a repo. See [`McpServerOrigin`].
        #[serde(skip)]
        pub origin: McpServerOrigin,
    }

    fn default_mcp_type() -> String {
        "stdio".to_string()
    }

    // ---- SkillsConfig ----------------------------------------------------

    /// Configuration for the skill-discovery system.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct SkillsConfig {
        /// Additional directories to search for skill `.md` files.
        #[serde(default)]
        pub paths: Vec<String>,
        /// Git repository URLs to fetch skills from (cloned once, then cached).
        #[serde(default)]
        pub urls: Vec<String>,
    }

    // ---- BangCommandsConfig ---------------------------------------------

    /// Configuration for the `!` batch command feature.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct BangCommandsConfig {
        /// Whether the feature is enabled. Default: false (opt-in).
        #[serde(default)]
        pub enabled: bool,
        /// Whether to add `!` commands to a separate shell-command history.
        #[serde(default, rename = "addToHistory")]
        pub add_to_history: bool,
        /// Whether to display command + output in the chat transcript.
        #[serde(default = "default_true", rename = "showInTranscript")]
        pub show_in_transcript: bool,
    }

    impl Default for BangCommandsConfig {
        fn default() -> Self {
            Self {
                enabled: false,
                add_to_history: false,
                show_in_transcript: default_true(),
            }
        }
    }

    // ---- Settings --------------------------------------------------------

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    pub struct Settings {
        #[serde(default)]
        pub config: Config,
        pub version: Option<u32>,
        #[serde(default)]
        pub projects: HashMap<String, ProjectSettings>,
        #[serde(default, rename = "remoteControlAtStartup")]
        pub remote_control_at_startup: bool,
        /// Global opt-in: trust and auto-launch project-defined MCP servers
        /// (those declared in a repository's `.claurst/settings.json`) without
        /// prompting. Defaults to `false`. Leaving it off means project servers
        /// must be approved per-project before they can spawn a process.
        /// Prefer per-project approval over flipping this on globally.
        #[serde(default, rename = "trustProjectMcpServers")]
        pub trust_project_mcp_servers: bool,
        /// Persisted permission rules saved by the user across sessions.
        #[serde(default, rename = "permissionRules")]
        pub permission_rules: Vec<crate::permissions::SerializedPermissionRule>,
        /// Names of plugins that have been explicitly enabled by the user.
        #[serde(default, rename = "enabledPlugins")]
        pub enabled_plugins: std::collections::HashSet<String>,
        /// Names of plugins that have been explicitly disabled by the user.
        #[serde(default, rename = "disabledPlugins")]
        pub disabled_plugins: std::collections::HashSet<String>,
        /// Whether the user has completed the first-launch onboarding flow.
        /// Mirrors TS `hasAcknowledgedSafetyNotice` / `hasCompletedOnboarding`.
        #[serde(default, rename = "hasCompletedOnboarding")]
        pub has_completed_onboarding: bool,
        /// Whether the user has accepted the Bypass Permissions warning.
        /// Mirrors TS `skipDangerousModePermissionPrompt`: once accepted the
        /// startup warning dialog is never shown again.
        #[serde(default, rename = "skipDangerousModePermissionPrompt")]
        pub skip_dangerous_mode_permission_prompt: bool,
        /// Bash command prefixes (first word) the user chose to always allow
        /// from the permission dialog's "Allow commands matching <prefix>*"
        /// option. Loaded into the prefix allowlist at startup.
        #[serde(default, rename = "allowedBashPrefixes")]
        pub allowed_bash_prefixes: Vec<String>,
        /// App version at last launch — used to detect upgrades and show release notes.
        #[serde(default, rename = "lastSeenVersion")]
        pub last_seen_version: Option<String>,
        /// Active provider ID at the settings level (e.g. "anthropic", "openai").
        #[serde(default)]
        pub provider: Option<String>,
        /// Per-provider configurations stored in settings.json.
        #[serde(default)]
        pub providers: HashMap<String, ProviderConfig>,
        /// User-defined OpenAI-compatible providers, keyed by unique id.
        /// Each entry is a self-contained provider with its own base URL,
        /// API key, headers, and model catalog. The id becomes the provider
        /// id used in `provider/model` routing.
        #[serde(default, rename = "customProviders", alias = "custom_providers")]
        pub custom_providers: HashMap<String, CustomProviderDef>,
        /// User-favorited model keys ("provider/model" format). Shown in a
        /// dedicated section at the top of the /model picker.
        #[serde(default, rename = "favoriteModels", alias = "favorite_models")]
        pub favorite_models: Vec<String>,
        /// User-supplied model metadata overrides stored in settings.json,
        /// keyed by `"provider/model"`. Merged into
        /// [`Config::model_overrides`] by [`Settings::effective_config`] and
        /// take precedence over the models.dev catalog.
        #[serde(default, rename = "modelOverrides", alias = "model_overrides")]
        pub model_overrides: HashMap<String, ModelOverride>,
        /// User-defined slash command templates.
        #[serde(default)]
        pub commands: HashMap<String, CommandTemplate>,
        /// Formatter configurations keyed by a user-defined name.
        #[serde(default)]
        pub formatter: HashMap<String, FormatterConfig>,
        /// Named agent definitions (overrides built-in defaults).
        #[serde(default)]
        pub agents: HashMap<String, AgentDefinition>,
        /// Skill-discovery configuration (extra paths and git URLs).
        #[serde(default)]
        pub skills: SkillsConfig,
        /// Managed agent (manager-executor) configuration.
        #[serde(default)]
        pub managed_agents: Option<ManagedAgentConfig>,
        /// When true, releasing a drag selection automatically copies it to
        /// the system clipboard. Defaults to `false` — users opt in by
        /// setting `"autoCopyOnHighlight": true` in
        /// `~/.claurst/settings.json`.
        #[serde(default, rename = "autoCopyOnHighlight")]
        pub auto_copy_on_highlight: bool,
        /// Whether to show current working directory in footer. Defaults to true.
        #[serde(default = "default_true", rename = "showCwd")]
        pub show_cwd: bool,
        /// Whether to show git branch in footer. Defaults to true.
        #[serde(default = "default_true", rename = "showGitBranch")]
        pub show_git_branch: bool,
        /// Whether to enable desktop notifications. Defaults to true.
        #[serde(default = "default_true", rename = "notifications")]
        pub notifications: bool,
        /// Whether to show turn duration in output. Defaults to false.
        #[serde(default, rename = "showTurnDuration")]
        pub show_turn_duration: bool,
        /// Whether to reduce motion in UI. Defaults to false.
        #[serde(default, rename = "reduceMotion")]
        pub reduce_motion: bool,
        /// Whether to show terminal progress bars. Defaults to true.
        #[serde(default = "default_true", rename = "terminalProgressBar")]
        pub terminal_progress_bar: bool,
        /// Whether to enable auto-compact. Defaults to true.
        #[serde(default = "default_true", rename = "autoCompact")]
        pub auto_compact: bool,
        /// Maximum number of file suggestions shown in autocomplete. Defaults to 15.
        #[serde(
            default = "default_file_autocomplete_limit",
            rename = "fileAutocompleteLimit"
        )]
        pub file_autocomplete_limit: usize,
        /// Whether to show hidden files in file autocomplete. Defaults to false.
        #[serde(default, rename = "fileAutocompleteShowHiddenFiles")]
        pub file_autocomplete_show_hidden_files: bool,
        /// Whether @ file references are automatically injected into message context. Defaults to true.
        /// When true: @file auto-injects file contents into your message before sending.
        /// When false: @ is just autocomplete and reference (no auto-injection).
        /// Note: This only affects user messages. @include in CLAUDE.md/AGENTS.md always injects with no size limits.
        #[serde(default = "default_true", rename = "fileInjectionEnabled")]
        pub file_injection_enabled: bool,
        /// Maximum file size to auto-inject (in KB). Defaults to 100. Set to 0 for no limit.
        /// When a file exceeds this limit, users get a warning and can choose to override or cancel.
        /// Note: @include in CLAUDE.md/AGENTS.md always injects regardless of this limit.
        #[serde(
            default = "default_file_injection_max_size",
            rename = "fileInjectionMaxSize"
        )]
        pub file_injection_max_size: usize,
        /// Whether auto-poke is enabled. When true, the model is automatically
        /// continued when it stops with incomplete todos. Default: true.
        #[serde(default = "default_true", rename = "autoPokeEnabled")]
        pub auto_poke_enabled: bool,
        /// Configuration for the `!` batch command feature.
        #[serde(default, rename = "bangCommands")]
        pub bang_commands: BangCommandsConfig,
        /// When true, the TUI log shows only the tool type (e.g. "Running
        /// command") without the command content or summary. Defaults to false.
        #[serde(default, rename = "hideToolContent")]
        pub hide_tool_content: bool,
    }

    /// A user-defined slash command template.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct CommandTemplate {
        /// The template string; `$ARGUMENTS` gets replaced with user input.
        pub template: String,
        /// Optional description shown in /help.
        pub description: Option<String>,
        /// Optional agent to use (e.g. "plan").
        pub agent: Option<String>,
        /// Optional model override (e.g. "anthropic/claude-haiku-4-5").
        pub model: Option<String>,
    }

    /// Configuration for a file formatter tool.
    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    pub struct FormatterConfig {
        /// Command to run, e.g. `["prettier", "--write"]`.
        pub command: Vec<String>,
        /// File extensions this formatter handles, e.g. `[".ts", ".tsx", ".js"]`.
        pub extensions: Vec<String>,
        /// Whether this formatter is disabled.
        #[serde(default)]
        pub disabled: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    pub struct ProjectSettings {
        #[serde(default)]
        pub allowed_tools: Vec<String>,
        #[serde(default)]
        pub mcp_servers: Vec<McpServerConfig>,
        pub custom_system_prompt: Option<String>,
    }

    /// Return the three built-in named agent definitions.
    /// User-defined agents in `settings.json` can override these by name.
    pub fn default_agents() -> HashMap<String, AgentDefinition> {
        let mut m = HashMap::new();
        m.insert("build".to_string(), AgentDefinition {
            description: Some("Full-access agent for implementing features and fixing bugs".to_string()),
            model: None,
            temperature: None,
            prompt: Some("You are the build agent. You have full access to read, write, and execute. Focus on implementing the requested changes completely and correctly.".to_string()),
            access: "full".to_string(),
            visible: true,
            max_turns: None,
            color: Some("cyan".to_string()),
        });
        m.insert("plan".to_string(), AgentDefinition {
            description: Some("Read-only agent for analyzing code and planning changes".to_string()),
            model: None,
            temperature: None,
            prompt: Some("You are the plan agent. You can read files and analyze code but cannot write files or execute commands. Focus on understanding the codebase and describing what changes should be made.".to_string()),
            access: "read-only".to_string(),
            visible: true,
            max_turns: Some(20),
            color: Some("yellow".to_string()),
        });
        m.insert("explore".to_string(), AgentDefinition {
            description: Some("Fast search-only agent for code exploration".to_string()),
            model: None,
            temperature: None,
            prompt: Some("You are the explore agent. You can search and read files. Focus on quickly finding relevant code and answering questions about the codebase.".to_string()),
            access: "search-only".to_string(),
            visible: true,
            max_turns: Some(15),
            color: Some("green".to_string()),
        });
        m
    }

    fn is_false(b: &bool) -> bool {
        !b
    }

    impl Config {
        /// Whether app-level mouse capture should be enabled. Defaults to `true`
        /// (capture on) when unset, preserving historical behaviour; users opt out
        /// via `"mouseCapture": false` to restore native terminal text selection
        /// and copy/paste (issue #104).
        pub fn mouse_capture_enabled(&self) -> bool {
            self.mouse_capture.unwrap_or(true)
        }

        pub fn selected_provider_id(&self) -> &str {
            self.provider
                .as_deref()
                .or_else(|| {
                    self.model
                        .as_deref()
                        .and_then(|model| model.split_once('/').map(|(provider, _)| provider))
                })
                .unwrap_or("anthropic")
        }

        /// Resolve the effective model, falling back to a provider-appropriate default.
        ///
        /// When a non-Anthropic provider is active and no model is explicitly set,
        /// returns that provider's canonical default model instead of `DEFAULT_MODEL`
        /// (which is Claude-specific).
        pub fn effective_model(&self) -> &str {
            if let Some(ref m) = self.model {
                return m;
            }
            match self.provider.as_deref() {
                Some("openai") => "gpt-4o",
                Some("google") => "gemini-2.5-flash",
                Some("groq") => "llama-3.3-70b-versatile",
                Some("cerebras") => "llama-3.3-70b",
                Some("deepseek") => "deepseek-v4-pro",
                Some("mistral") => "mistral-large-latest",
                Some("xai") => "grok-2",
                Some("openrouter") => "anthropic/claude-sonnet-4",
                Some("togetherai") | Some("together-ai") => {
                    "meta-llama/Llama-3.3-70B-Instruct-Turbo"
                }
                Some("perplexity") => "sonar-pro",
                Some("cohere") => "command-r-plus",
                // DashScope runs as "qwen" at runtime but is "alibaba" in the
                // models.dev catalog; terminal fallback keeps a qwen id so an
                // unconfigured Qwen provider never resolves to a claude-* model.
                Some("qwen") | Some("alibaba") => "qwen3-max",
                Some("deepinfra") => "meta-llama/Llama-3.3-70B-Instruct",
                Some("github-copilot") => "gpt-4o",
                Some("ollama") => "llama3.2",
                Some("lmstudio") => "default",
                Some("llamacpp") => "default",
                Some("custom-openai") => "default",
                Some("azure") => "gpt-4o",
                Some("amazon-bedrock") => "anthropic.claude-sonnet-4-6-v1",
                Some("venice") => "llama-3.3-70b",
                _ => crate::constants::DEFAULT_MODEL, // Anthropic default
            }
        }

        /// Resolve the effective max-tokens.
        pub fn effective_max_tokens(&self) -> u32 {
            self.max_tokens
                .unwrap_or(crate::constants::DEFAULT_MAX_TOKENS)
        }

        /// Resolve the effective compact threshold (0.0 - 1.0).
        pub fn effective_compact_threshold(&self) -> f32 {
            if self.compact_threshold > 0.0 {
                self.compact_threshold
            } else {
                crate::constants::DEFAULT_COMPACT_THRESHOLD
            }
        }

        /// Resolve the effective output style for system-prompt assembly.
        pub fn effective_output_style(&self) -> crate::system_prompt::OutputStyle {
            self.output_style
                .as_deref()
                .map(crate::system_prompt::OutputStyle::from_str)
                .unwrap_or_default()
        }

        /// Resolve the prompt text for the selected output style, including
        /// user-defined styles loaded from `~/.claurst/output-styles/`.
        pub fn resolve_output_style_prompt(&self) -> Option<String> {
            let style_name = self.output_style.as_deref().unwrap_or("default");
            let styles = crate::output_styles::all_styles(&Settings::config_dir());
            crate::output_styles::find_style(&styles, style_name)
                .map(|style| style.prompt.clone())
                .filter(|prompt| !prompt.trim().is_empty())
        }

        pub fn resolve_provider_api_key(&self, provider_id: &str) -> Option<String> {
            let provider_cfg = self.provider_configs.get(provider_id);
            if provider_cfg.is_some_and(|provider| !provider.enabled) {
                return None;
            }

            let top_level_key = if provider_id == self.selected_provider_id() {
                self.api_key.clone()
            } else {
                None
            };

            top_level_key
                .filter(|key| !key.is_empty())
                .or_else(|| {
                    provider_cfg
                        .and_then(|provider| provider.api_key.clone())
                        .filter(|key| !key.is_empty())
                })
                .or_else(|| {
                    api_key_env_vars_for_provider(provider_id)
                        .iter()
                        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
                })
                .or_else(|| crate::AuthStore::load().api_key_for(provider_id))
                // Support {env:VAR_NAME} patterns in the resolved value
                .map(|key| substitute_env_vars(&key))
        }

        pub fn resolve_anthropic_api_key(&self) -> Option<String> {
            self.api_key
                .clone()
                .filter(|key| !key.is_empty())
                .or_else(|| {
                    self.provider_configs
                        .get("anthropic")
                        .and_then(|provider| provider.api_key.clone())
                        .filter(|key| !key.is_empty())
                })
                .or_else(|| {
                    api_key_env_vars_for_provider("anthropic")
                        .iter()
                        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
                })
                // Support {env:VAR_NAME} patterns in the resolved value
                .map(|key| substitute_env_vars(&key))
        }

        /// Resolve the API key for the active provider.
        pub fn resolve_api_key(&self) -> Option<String> {
            self.resolve_provider_api_key(self.selected_provider_id())
        }

        /// Async variant: also checks `~/.claurst/oauth_tokens.json`.
        /// Returns `(credential, use_bearer_auth)`.
        /// - For Console OAuth flow: credential is the stored API key, bearer=false.
        /// - For Claude.ai OAuth flow: credential is the access token, bearer=true.
        ///
        /// Silently attempts token refresh when the access token is expired.
        pub async fn resolve_auth_async(&self) -> Option<(String, bool)> {
            if self.selected_provider_id() != "anthropic" {
                return self.resolve_api_key().map(|key| (key, false));
            }

            self.resolve_anthropic_auth_async().await
        }

        pub async fn resolve_anthropic_auth_async(&self) -> Option<(String, bool)> {
            if let Some(key) = self.resolve_anthropic_api_key() {
                return Some((key, false));
            }

            let tokens = crate::oauth::OAuthTokens::load().await?;

            // If expired and we have a refresh token, attempt silent refresh.
            // Clone the refresh token up-front so we don't borrow `tokens` during the async call.
            let refresh_token_owned = tokens.refresh_token.clone();
            let tokens = if tokens.is_expired() {
                if let Some(rt) = refresh_token_owned {
                    // Inline the refresh HTTP call (cc_core can't depend on cc_cli::oauth_flow).
                    let body = serde_json::json!({
                        "grant_type": "refresh_token",
                        "refresh_token": rt,
                        "client_id": crate::oauth::CLIENT_ID,
                        "scope": crate::oauth::ALL_SCOPES.join(" "),
                    });
                    let refreshed = 'refresh: {
                        let Ok(client) = reqwest::Client::builder()
                            .timeout(std::time::Duration::from_secs(30))
                            .build()
                        else {
                            break 'refresh None;
                        };
                        let Ok(resp) = client
                            .post(crate::oauth::TOKEN_URL)
                            .header("content-type", "application/json")
                            .json(&body)
                            .send()
                            .await
                        else {
                            break 'refresh None;
                        };
                        if !resp.status().is_success() {
                            break 'refresh None;
                        }
                        let Ok(data) = resp.json::<serde_json::Value>().await else {
                            break 'refresh None;
                        };
                        let new_at = data["access_token"].as_str().unwrap_or("").to_string();
                        if new_at.is_empty() {
                            break 'refresh None;
                        }
                        let new_rt = data["refresh_token"].as_str().map(String::from);
                        let exp_in = data["expires_in"].as_u64().unwrap_or(3600);
                        let exp_ms = chrono::Utc::now().timestamp_millis() + (exp_in as i64 * 1000);
                        let scopes: Vec<String> = data["scope"]
                            .as_str()
                            .unwrap_or("")
                            .split_whitespace()
                            .map(String::from)
                            .collect();
                        let mut r = tokens.clone();
                        r.access_token = new_at;
                        if let Some(nrt) = new_rt {
                            r.refresh_token = Some(nrt);
                        }
                        r.expires_at_ms = Some(exp_ms);
                        r.scopes = scopes;
                        let _ = r.save().await;
                        Some(r)
                    };
                    refreshed.unwrap_or(tokens)
                } else {
                    tokens // expired, no refresh token → can't fix
                }
            } else {
                tokens
            };

            tokens
                .effective_credential()
                .map(|cred| (cred.to_string(), tokens.uses_bearer_auth()))
        }

        pub fn resolve_provider_api_base(&self, provider_id: &str) -> Option<String> {
            let provider_cfg = self.provider_configs.get(provider_id);
            if provider_cfg.is_some_and(|provider| !provider.enabled) {
                return None;
            }

            provider_cfg
                .and_then(|provider| provider.api_base.clone())
                .filter(|base| !base.is_empty())
                .or_else(|| {
                    api_base_env_var_for_provider(provider_id)
                        .and_then(|name| std::env::var(name).ok())
                        .filter(|base| !base.is_empty())
                })
                .or_else(|| default_api_base_for_provider(provider_id).map(str::to_owned))
                // Support {env:VAR_NAME} patterns in the resolved base URL
                .map(|base| substitute_env_vars(&base))
        }

        pub fn resolve_anthropic_api_base(&self) -> String {
            self.resolve_provider_api_base("anthropic")
                .unwrap_or_else(|| crate::constants::ANTHROPIC_API_BASE.to_string())
        }

        /// Resolve the API base URL for the active provider.
        pub fn resolve_api_base(&self) -> String {
            self.resolve_provider_api_base(self.selected_provider_id())
                .unwrap_or_else(|| self.resolve_anthropic_api_base())
        }

        /// Resolve the total request timeout (in seconds) for `provider_id`.
        ///
        /// Precedence: per-provider [`ProviderConfig::request_timeout_secs`] >
        /// global [`Config::request_timeout_secs`] > [`DEFAULT_REQUEST_TIMEOUT_SECS`].
        /// Zero values are treated as unset.
        pub fn resolve_request_timeout_secs(&self, provider_id: &str) -> u64 {
            self.provider_configs
                .get(provider_id)
                .and_then(|provider| provider.request_timeout_secs)
                .filter(|&secs| secs > 0)
                .or_else(|| self.request_timeout_secs.filter(|&secs| secs > 0))
                .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS)
        }

        /// Resolve the request timeout for the active provider.
        pub fn resolve_request_timeout_secs_active(&self) -> u64 {
            self.resolve_request_timeout_secs(self.selected_provider_id())
        }
    }

    impl Settings {
        /// The canonical per-user claurst home directory — the single source of
        /// truth for where claurst keeps everything (settings, sessions,
        /// accounts, skills, …). Every subdirectory (`config_dir().join("sessions")`,
        /// `.join("accounts")`, …) lives under this one root.
        ///
        /// Resolution precedence (see issue #207 — XDG Base Directory support,
        /// kept fully back-compatible so existing installs are untouched):
        ///
        /// 1. **`$CLAURST_HOME`** — if set and non-empty, used verbatim.
        /// 2. **Legacy `~/.claurst`** — if that directory already exists, it is
        ///    reused so existing users need no migration.
        /// 3. **XDG** — `$XDG_CONFIG_HOME/claurst` when `$XDG_CONFIG_HOME` is set
        ///    (and absolute, per the spec), otherwise `~/.config/claurst`. Fresh
        ///    installs land here.
        pub fn config_dir() -> PathBuf {
            // 1. Explicit override wins, used verbatim.
            if let Some(explicit) = std::env::var_os("CLAURST_HOME") {
                if !explicit.is_empty() {
                    return PathBuf::from(explicit);
                }
            }

            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

            // 2. Back-compat: an existing legacy `~/.claurst` is used as-is.
            let legacy = home.join(".claurst");
            if legacy.is_dir() {
                return legacy;
            }

            // 3. XDG config location for fresh installs.
            if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
                let xdg = PathBuf::from(xdg);
                // Per the XDG spec a relative $XDG_CONFIG_HOME must be ignored.
                if xdg.is_absolute() {
                    return xdg.join("claurst");
                }
            }
            home.join(".config").join("claurst")
        }

        /// Full path to the global settings JSON file.
        pub fn global_settings_path() -> PathBuf {
            Self::config_dir().join("settings.json")
        }

        fn parse_file(content: &str, path: &Path) -> anyhow::Result<Self> {
            serde_json::from_str(content).map_err(|error| {
                anyhow::anyhow!(
                    "Failed to parse settings file {}: {}. The file was not modified; fix the JSON and restart Claurst.",
                    path.display(),
                    error
                )
            })
        }

        async fn load_from_path(path: &Path) -> anyhow::Result<Self> {
            if path.exists() {
                let content = tokio::fs::read_to_string(path).await?;
                Self::parse_file(&content, path)
            } else {
                Ok(Self::default())
            }
        }

        fn load_from_path_sync(path: &Path) -> anyhow::Result<Self> {
            if path.exists() {
                let content = std::fs::read_to_string(path)?;
                Self::parse_file(&content, path)
            } else {
                Ok(Self::default())
            }
        }

        async fn save_to_path(&self, path: &Path) -> anyhow::Result<()> {
            if path.exists() {
                let content = tokio::fs::read_to_string(path).await?;
                Self::parse_file(&content, path).map_err(|error| {
                    anyhow::anyhow!(
                        "Refusing to overwrite malformed settings file {}: {}",
                        path.display(),
                        error
                    )
                })?;
            }
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let content = serde_json::to_string_pretty(self)?;
            tokio::fs::write(path, content).await?;
            Ok(())
        }

        fn save_to_path_sync(&self, path: &Path) -> anyhow::Result<()> {
            if path.exists() {
                let content = std::fs::read_to_string(path)?;
                Self::parse_file(&content, path).map_err(|error| {
                    anyhow::anyhow!(
                        "Refusing to overwrite malformed settings file {}: {}",
                        path.display(),
                        error
                    )
                })?;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let content = serde_json::to_string_pretty(self)?;
            // Atomic write: serialize to a temp file, then rename into place.
            // On Unix, rename() is atomic — readers never see a partial file.
            let tmp_path = path.with_extension("json.tmp");
            std::fs::write(&tmp_path, &content)?;
            std::fs::rename(&tmp_path, path)?;
            Ok(())
        }

        /// Load settings from disk, returning defaults when the file is missing.
        ///
        /// A malformed file is returned as an error and is never replaced with
        /// defaults.
        pub async fn load() -> anyhow::Result<Self> {
            let path = Self::global_settings_path();
            Self::load_from_path(&path).await
        }

        /// Persist settings to disk without overwriting a malformed file.
        pub async fn save(&self) -> anyhow::Result<()> {
            let path = Self::global_settings_path();
            self.save_to_path(&path).await
        }

        /// Synchronous variant used by pre-session commands.
        pub fn load_sync() -> anyhow::Result<Self> {
            let path = Self::global_settings_path();
            Self::load_from_path_sync(&path)
        }

        /// Synchronous variant used by pre-session commands. Refuses to
        /// overwrite a malformed file.
        pub fn save_sync(&self) -> anyhow::Result<()> {
            let path = Self::global_settings_path();
            self.save_to_path_sync(&path)
        }

        /// Return the effective `Config`, merging top-level provider settings
        /// into the embedded `config` field.
        ///
        /// - `settings.provider` wins over `settings.config.provider` (if set).
        /// - `settings.providers` entries are merged into `config.provider_configs`,
        ///   with the embedded config values taking precedence for keys already present.
        pub fn effective_config(&self) -> Config {
            let mut config = self.config.clone();
            // Top-level `provider` key overrides config.provider when set.
            if self.provider.is_some() && config.provider.is_none() {
                config.provider = self.provider.clone();
            }
            // Merge top-level `providers` map into config.provider_configs.
            for (id, pc) in &self.providers {
                config
                    .provider_configs
                    .entry(id.clone())
                    .or_insert_with(|| pc.clone());
            }
            // Merge custom providers' API keys and base URLs into
            // provider_configs so resolve_provider_api_key() can find them.
            for (id, cp) in &self.custom_providers {
                config
                    .provider_configs
                    .entry(id.clone())
                    .or_insert_with(|| ProviderConfig {
                        api_key: cp.api_key.clone(),
                        api_base: Some(cp.api_base.clone()),
                        enabled: true,
                        ..Default::default()
                    });
            }
            // Merge top-level `modelOverrides` into config.model_overrides
            // (nested `config` block wins for keys present in both).
            for (id, ov) in &self.model_overrides {
                config
                    .model_overrides
                    .entry(id.clone())
                    .or_insert_with(|| ov.clone());
            }
            // Copy top-level formatters and commands into config.
            for (k, v) in &self.formatter {
                config
                    .formatter
                    .entry(k.clone())
                    .or_insert_with(|| v.clone());
            }
            for (k, v) in &self.commands {
                config
                    .commands
                    .entry(k.clone())
                    .or_insert_with(|| v.clone());
            }
            // Copy top-level agent definitions into config.
            for (k, v) in &self.agents {
                config.agents.entry(k.clone()).or_insert_with(|| v.clone());
            }
            // Copy skills config into effective config (paths and urls merged).
            for p in &self.skills.paths {
                if !config.skills.paths.contains(p) {
                    config.skills.paths.push(p.clone());
                }
            }
            for u in &self.skills.urls {
                if !config.skills.urls.contains(u) {
                    config.skills.urls.push(u.clone());
                }
            }
            // Copy file autocomplete and injection settings from the top-level Settings
            // fields, but only when they were explicitly set (differ from their defaults).
            // If they're at defaults, the nested "config" section value (already in `config`
            // via the clone above) takes precedence.
            if self.file_autocomplete_limit != default_file_autocomplete_limit() {
                config.file_autocomplete_limit = self.file_autocomplete_limit;
            }
            if self.file_autocomplete_show_hidden_files {
                config.file_autocomplete_show_hidden_files = true;
            }
            if self.file_injection_enabled != default_true() {
                config.file_injection_enabled = self.file_injection_enabled;
            }
            if self.file_injection_max_size != default_file_injection_max_size() {
                config.file_injection_max_size = self.file_injection_max_size;
            }
            if self.hide_tool_content {
                config.hide_tool_content = true;
            }
            config
        }

        /// Load settings from all config levels and merge them.
        /// Priority: project > global.
        pub async fn load_hierarchical(cwd: &std::path::Path) -> anyhow::Result<Self> {
            // 1. Load global settings.
            let mut merged = Self::load().await?;
            // 2. Find and merge project settings (project wins).
            if let Some(project_settings) = Self::find_project_settings(cwd).await {
                merged = Self::merge(merged, project_settings);
            }
            Ok(merged)
        }

        /// Walk up from `cwd` looking for `.claurst/settings.json` or
        /// `.claurst/settings.jsonc`.
        async fn find_project_settings(cwd: &std::path::Path) -> Option<Self> {
            let global_path = Self::global_settings_path();
            let mut dir = cwd;
            loop {
                // Try .json first, then .jsonc.
                for name in &["settings.json", "settings.jsonc"] {
                    let candidate = dir.join(".claurst").join(name);
                    if candidate.exists() && candidate != global_path {
                        if let Ok(content) = tokio::fs::read_to_string(&candidate).await {
                            let stripped = strip_jsonc_comments(&content);
                            if let Ok(mut s) = serde_json::from_str::<Self>(&stripped) {
                                // SECURITY: tag every server defined by this
                                // repository as project-origin so it gets gated
                                // behind explicit approval before launching.
                                // `origin` is `#[serde(skip)]`, so the file can
                                // never set it itself — we always assign here.
                                for server in &mut s.config.mcp_servers {
                                    server.origin = McpServerOrigin::Project;
                                }
                                for ps in s.projects.values_mut() {
                                    for server in &mut ps.mcp_servers {
                                        server.origin = McpServerOrigin::Project;
                                    }
                                }
                                return Some(s);
                            }
                        }
                        // Found a file but couldn't parse — stop here, don't go up.
                        return None;
                    }
                }
                match dir.parent() {
                    Some(parent) => dir = parent,
                    None => break,
                }
            }
            None
        }

        /// Merge two settings with `override_settings` taking priority.
        /// Simple strategy: override wins for all scalar fields; Vecs are
        /// concatenated (deduped); HashMaps are merged (override wins on collision).
        fn merge(base: Self, over: Self) -> Self {
            // Helper to merge two HashMaps (over wins on key collision).
            fn merge_map<K: std::hash::Hash + Eq + Clone, V: Clone>(
                mut base: HashMap<K, V>,
                over: HashMap<K, V>,
            ) -> HashMap<K, V> {
                for (k, v) in over {
                    base.insert(k, v);
                }
                base
            }
            // Merge the embedded Config structs.
            let merged_config = Config {
                api_key: over.config.api_key.or(base.config.api_key),
                model: over.config.model.or(base.config.model),
                max_tokens: over.config.max_tokens.or(base.config.max_tokens),
                permission_mode: over.config.permission_mode,
                theme: over.config.theme,
                output_style: over.config.output_style.or(base.config.output_style),
                auto_compact: over.config.auto_compact || base.config.auto_compact,
                compact_threshold: if over.config.compact_threshold != 0.0 {
                    over.config.compact_threshold
                } else {
                    base.config.compact_threshold
                },
                verbose: over.config.verbose || base.config.verbose,
                output_format: over.config.output_format,
                mcp_servers: {
                    let mut v = base.config.mcp_servers;
                    v.extend(over.config.mcp_servers);
                    v
                },
                lsp_servers: {
                    let mut v = base.config.lsp_servers;
                    v.extend(over.config.lsp_servers);
                    v
                },
                allowed_tools: {
                    let mut v = base.config.allowed_tools;
                    v.extend(over.config.allowed_tools);
                    v.dedup();
                    v
                },
                disallowed_tools: {
                    let mut v = base.config.disallowed_tools;
                    v.extend(over.config.disallowed_tools);
                    v.dedup();
                    v
                },
                env: merge_map(base.config.env, over.config.env),
                enable_all_mcp_servers: over.config.enable_all_mcp_servers
                    || base.config.enable_all_mcp_servers,
                custom_system_prompt: over
                    .config
                    .custom_system_prompt
                    .or(base.config.custom_system_prompt),
                append_system_prompt: over
                    .config
                    .append_system_prompt
                    .or(base.config.append_system_prompt),
                disable_claude_mds: over.config.disable_claude_mds
                    || base.config.disable_claude_mds,
                project_dir: over.config.project_dir.or(base.config.project_dir),
                workspace_paths: {
                    let mut v = base.config.workspace_paths;
                    v.extend(over.config.workspace_paths);
                    v
                },
                additional_dirs: {
                    let mut v = base.config.additional_dirs;
                    v.extend(over.config.additional_dirs);
                    v
                },
                hooks: merge_map(base.config.hooks, over.config.hooks),
                provider: over.config.provider.or(base.config.provider),
                provider_configs: merge_map(
                    base.config.provider_configs,
                    over.config.provider_configs,
                ),
                model_overrides: merge_map(
                    base.config.model_overrides,
                    over.config.model_overrides,
                ),
                formatter: merge_map(base.config.formatter, over.config.formatter),
                commands: merge_map(base.config.commands, over.config.commands),
                agents: merge_map(base.config.agents, over.config.agents),
                skills: {
                    let mut paths = base.config.skills.paths;
                    for p in over.config.skills.paths {
                        if !paths.contains(&p) {
                            paths.push(p);
                        }
                    }
                    let mut urls = base.config.skills.urls;
                    for u in over.config.skills.urls {
                        if !urls.contains(&u) {
                            urls.push(u);
                        }
                    }
                    SkillsConfig { paths, urls }
                },
                managed_agents: over.config.managed_agents.or(base.config.managed_agents),
                auto_commits: over.config.auto_commits.or(base.config.auto_commits),
                mouse_capture: over.config.mouse_capture.or(base.config.mouse_capture),
                cursor_blink_enabled: over.config.cursor_blink_enabled
                    || base.config.cursor_blink_enabled,
                file_autocomplete_limit: if over.config.file_autocomplete_limit != 0 {
                    over.config.file_autocomplete_limit
                } else {
                    base.config.file_autocomplete_limit
                },
                file_autocomplete_show_hidden_files: over
                    .config
                    .file_autocomplete_show_hidden_files
                    || base.config.file_autocomplete_show_hidden_files,
                file_injection_enabled: over.config.file_injection_enabled
                    || base.config.file_injection_enabled,
                file_injection_max_size: if over.config.file_injection_max_size != 0 {
                    over.config.file_injection_max_size
                } else {
                    base.config.file_injection_max_size
                },
                request_timeout_secs: over
                    .config
                    .request_timeout_secs
                    .or(base.config.request_timeout_secs),
                degradation_summary_enabled: over.config.degradation_summary_enabled
                    && base.config.degradation_summary_enabled,
                hide_tool_content: over.config.hide_tool_content || base.config.hide_tool_content,
            };
            Self {
                config: merged_config,
                version: over.version.or(base.version),
                projects: merge_map(base.projects, over.projects),
                remote_control_at_startup: over.remote_control_at_startup
                    || base.remote_control_at_startup,
                // SECURITY: only the user's global settings may grant blanket
                // trust to project MCP servers. A project's own settings file
                // (`over`) must NOT be able to flip this on — otherwise a
                // malicious repo could set `trustProjectMcpServers: true` to
                // bypass the approval gate entirely.
                trust_project_mcp_servers: base.trust_project_mcp_servers,
                // SECURITY: same reasoning — these silence approval prompts,
                // so only the user's global settings may set them. A project
                // settings file must not be able to pre-accept bypass mode or
                // pre-approve bash command prefixes.
                skip_dangerous_mode_permission_prompt: base.skip_dangerous_mode_permission_prompt,
                allowed_bash_prefixes: base.allowed_bash_prefixes,
                permission_rules: {
                    let mut v = base.permission_rules;
                    v.extend(over.permission_rules);
                    v
                },
                enabled_plugins: {
                    let mut s = base.enabled_plugins;
                    s.extend(over.enabled_plugins);
                    s
                },
                disabled_plugins: {
                    let mut s = base.disabled_plugins;
                    s.extend(over.disabled_plugins);
                    s
                },
                has_completed_onboarding: over.has_completed_onboarding
                    || base.has_completed_onboarding,
                last_seen_version: over.last_seen_version.or(base.last_seen_version),
                provider: over.provider.or(base.provider),
                providers: merge_map(base.providers, over.providers),
                custom_providers: merge_map(base.custom_providers, over.custom_providers),
                favorite_models: {
                    let mut v = base.favorite_models;
                    for f in over.favorite_models {
                        if !v.contains(&f) {
                            v.push(f);
                        }
                    }
                    v
                },
                model_overrides: merge_map(base.model_overrides, over.model_overrides),
                commands: merge_map(base.commands, over.commands),
                formatter: merge_map(base.formatter, over.formatter),
                agents: merge_map(base.agents, over.agents),
                skills: {
                    let mut paths = base.skills.paths;
                    for p in over.skills.paths {
                        if !paths.contains(&p) {
                            paths.push(p);
                        }
                    }
                    let mut urls = base.skills.urls;
                    for u in over.skills.urls {
                        if !urls.contains(&u) {
                            urls.push(u);
                        }
                    }
                    SkillsConfig { paths, urls }
                },
                managed_agents: over.managed_agents.or(base.managed_agents),
                auto_copy_on_highlight: over.auto_copy_on_highlight || base.auto_copy_on_highlight,
                notifications: over.notifications || base.notifications,
                show_turn_duration: over.show_turn_duration || base.show_turn_duration,
                reduce_motion: over.reduce_motion || base.reduce_motion,
                terminal_progress_bar: over.terminal_progress_bar || base.terminal_progress_bar,
                show_cwd: over.show_cwd || base.show_cwd,
                show_git_branch: over.show_git_branch || base.show_git_branch,
                auto_compact: over.auto_compact || base.auto_compact,
                file_autocomplete_limit: if over.file_autocomplete_limit != 0 {
                    over.file_autocomplete_limit
                } else {
                    base.file_autocomplete_limit
                },
                file_autocomplete_show_hidden_files: over.file_autocomplete_show_hidden_files
                    || base.file_autocomplete_show_hidden_files,
                file_injection_enabled: over.file_injection_enabled || base.file_injection_enabled,
                file_injection_max_size: if over.file_injection_max_size != 0 {
                    over.file_injection_max_size
                } else {
                    base.file_injection_max_size
                },
                auto_poke_enabled: over.auto_poke_enabled || base.auto_poke_enabled,
                bang_commands: BangCommandsConfig {
                    enabled: over.bang_commands.enabled || base.bang_commands.enabled,
                    add_to_history: over.bang_commands.add_to_history
                        || base.bang_commands.add_to_history,
                    show_in_transcript: over.bang_commands.show_in_transcript
                        && base.bang_commands.show_in_transcript,
                },
                hide_tool_content: over.hide_tool_content || base.hide_tool_content,
            }
        }
    }

    /// Strip `//` line-comments and `/* */` block-comments from a JSON string
    /// (JSONC format), preserving newlines for error-message line numbers.
    pub fn strip_jsonc_comments(input: &str) -> String {
        let mut result = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        let mut in_string = false;
        let mut prev_char = '\0';

        while let Some(ch) = chars.next() {
            if in_string {
                if ch == '"' && prev_char != '\\' {
                    in_string = false;
                }
                result.push(ch);
                prev_char = ch;
                continue;
            }
            if ch == '"' {
                in_string = true;
                result.push(ch);
                prev_char = ch;
                continue;
            }
            if ch == '/' {
                match chars.peek() {
                    Some('/') => {
                        // Line comment — skip to end of line.
                        for c in chars.by_ref() {
                            if c == '\n' {
                                result.push('\n');
                                break;
                            }
                        }
                    }
                    Some('*') => {
                        // Block comment — skip until `*/`.
                        chars.next();
                        let mut prev = '\0';
                        for c in chars.by_ref() {
                            if prev == '*' && c == '/' {
                                break;
                            }
                            if c == '\n' {
                                result.push('\n');
                            }
                            prev = c;
                        }
                    }
                    _ => result.push(ch),
                }
                prev_char = '\0';
                continue;
            }
            result.push(ch);
            prev_char = ch;
        }
        result
    }

    /// Replace `{env:VARNAME}` patterns in a string with environment variable
    /// values.  Missing variables are replaced with an empty string.
    pub fn substitute_env_vars(s: &str) -> String {
        let mut result = s.to_string();
        loop {
            match result.find("{env:") {
                None => break,
                Some(start) => match result[start..].find('}') {
                    None => break,
                    Some(rel_end) => {
                        let var_name = result[start + 5..start + rel_end].to_string();
                        let value = std::env::var(&var_name).unwrap_or_default();
                        result.replace_range(start..start + rel_end + 1, &value);
                    }
                },
            }
        }
        result
    }

    #[cfg(test)]
    mod settings_io_tests {
        use super::*;

        const MALFORMED_SETTINGS: &str = r#"{"config":{"model":"test-model",}}"#;

        #[test]
        fn sync_load_reports_malformed_settings_without_modifying_them() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("settings.json");
            std::fs::write(&path, MALFORMED_SETTINGS).unwrap();

            let error = Settings::load_from_path_sync(&path).unwrap_err();

            assert!(error.to_string().contains("Failed to parse settings file"));
            assert!(error.to_string().contains(&path.display().to_string()));
            assert!(error.to_string().contains("The file was not modified"));
            assert_eq!(std::fs::read_to_string(path).unwrap(), MALFORMED_SETTINGS);
        }

        #[test]
        fn sync_save_refuses_to_overwrite_malformed_settings() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("settings.json");
            std::fs::write(&path, MALFORMED_SETTINGS).unwrap();
            let mut replacement = Settings::default();
            replacement.config.model = Some("replacement".to_string());

            let error = replacement.save_to_path_sync(&path).unwrap_err();

            assert!(error
                .to_string()
                .contains("Refusing to overwrite malformed settings file"));
            assert_eq!(std::fs::read_to_string(path).unwrap(), MALFORMED_SETTINGS);
        }

        #[tokio::test]
        async fn async_load_and_save_preserve_malformed_settings() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("settings.json");
            tokio::fs::write(&path, MALFORMED_SETTINGS).await.unwrap();

            let load_error = Settings::load_from_path(&path).await.unwrap_err();
            assert!(load_error
                .to_string()
                .contains("Failed to parse settings file"));

            let save_error = Settings::default().save_to_path(&path).await.unwrap_err();
            assert!(save_error
                .to_string()
                .contains("Refusing to overwrite malformed settings file"));
            assert_eq!(
                tokio::fs::read_to_string(path).await.unwrap(),
                MALFORMED_SETTINGS
            );
        }
    }

    #[cfg(test)]
    mod request_timeout_tests {
        use super::*;

        #[test]
        fn defaults_to_600_when_unset() {
            let config = Config::default();
            assert_eq!(config.request_timeout_secs, None);
            assert_eq!(
                config.resolve_request_timeout_secs("openai"),
                DEFAULT_REQUEST_TIMEOUT_SECS
            );
            assert_eq!(DEFAULT_REQUEST_TIMEOUT_SECS, 600);
        }

        #[test]
        fn global_request_timeout_serde_roundtrips_with_camelcase_key() {
            let config = Config {
                request_timeout_secs: Some(1800),
                ..Default::default()
            };
            // Serialises with the documented camelCase key.
            let json = serde_json::to_string(&config).expect("serialise");
            assert!(
                json.contains("\"requestTimeoutSecs\":1800"),
                "expected camelCase key in: {json}"
            );
            // Round-trips back and threads through the resolver.
            let parsed: Config = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(parsed.request_timeout_secs, Some(1800));
            assert_eq!(parsed.resolve_request_timeout_secs("ollama"), 1800);
        }

        #[test]
        fn snake_case_alias_also_parses() {
            // Patch a fully-serialised config to use the snake_case alias and
            // confirm it still deserialises (back-compat with snake_case keys).
            let mut value = serde_json::to_value(Config::default()).expect("to_value");
            let obj = value.as_object_mut().unwrap();
            obj.remove("requestTimeoutSecs");
            obj.insert("request_timeout_secs".to_string(), serde_json::json!(900));
            let parsed: Config = serde_json::from_value(value).expect("alias should parse");
            assert_eq!(parsed.request_timeout_secs, Some(900));
        }

        #[test]
        fn per_provider_override_wins_over_global() {
            let mut config = Config {
                request_timeout_secs: Some(1200),
                ..Default::default()
            };
            let provider = ProviderConfig {
                request_timeout_secs: Some(3600),
                ..Default::default()
            };
            config
                .provider_configs
                .insert("ollama".to_string(), provider);
            // Per-provider override applies to ollama.
            assert_eq!(config.resolve_request_timeout_secs("ollama"), 3600);
            // Other providers fall back to the global value.
            assert_eq!(config.resolve_request_timeout_secs("openai"), 1200);
        }

        #[test]
        fn effective_config_merges_top_level_provider_timeout() {
            let mut settings = Settings::default();
            settings.config.request_timeout_secs = Some(1200);
            let provider = ProviderConfig {
                request_timeout_secs: Some(3600),
                ..Default::default()
            };
            settings.providers.insert("ollama".to_string(), provider);
            let config = settings.effective_config();
            assert_eq!(config.resolve_request_timeout_secs("ollama"), 3600);
            assert_eq!(config.resolve_request_timeout_secs("openai"), 1200);
        }

        #[test]
        fn effective_config_merges_top_level_model_overrides() {
            let mut settings = Settings::default();
            // Nested `config` block wins for a key present in both.
            settings.config.model_overrides.insert(
                "custom-openai/a".to_string(),
                ModelOverride {
                    context_window: Some(111),
                    ..Default::default()
                },
            );
            settings.model_overrides.insert(
                "custom-openai/a".to_string(),
                ModelOverride {
                    context_window: Some(999),
                    ..Default::default()
                },
            );
            // Top-level-only key is folded in.
            settings.model_overrides.insert(
                "custom-openai/b".to_string(),
                ModelOverride {
                    context_window: Some(222),
                    ..Default::default()
                },
            );
            let config = settings.effective_config();
            assert_eq!(
                config.model_overrides["custom-openai/a"].context_window,
                Some(111)
            );
            assert_eq!(
                config.model_overrides["custom-openai/b"].context_window,
                Some(222)
            );
        }

        #[test]
        fn model_override_accepts_camel_and_snake_case() {
            // Top-level camelCase key `modelOverrides`, camelCase fields.
            let camel = r#"{
                "modelOverrides": {
                    "custom-openai/x": { "contextWindow": 32768, "maxOutputTokens": 4096, "name": "X" }
                }
            }"#;
            let s: Settings = serde_json::from_str(camel).unwrap();
            let ov = &s.model_overrides["custom-openai/x"];
            assert_eq!(ov.context_window, Some(32768));
            assert_eq!(ov.max_output_tokens, Some(4096));
            assert_eq!(ov.name.as_deref(), Some("X"));

            // snake_case top-level alias `model_overrides` and snake_case fields.
            let snake = r#"{
                "model_overrides": {
                    "ollama/y": { "context_window": 262144, "status": "beta" }
                }
            }"#;
            let s: Settings = serde_json::from_str(snake).unwrap();
            let ov = &s.model_overrides["ollama/y"];
            assert_eq!(ov.context_window, Some(262144));
            assert_eq!(ov.status.as_deref(), Some("beta"));
        }

        #[test]
        fn favorite_models_deserialize() {
            let json = r#"{"favoriteModels": ["anthropic/claude-sonnet-4-6", "openai/gpt-4o"]}"#;
            let settings: Settings = serde_json::from_str(json).unwrap();
            assert_eq!(settings.favorite_models.len(), 2);
            assert_eq!(settings.favorite_models[0], "anthropic/claude-sonnet-4-6");
            assert_eq!(settings.favorite_models[1], "openai/gpt-4o");
        }

        #[test]
        fn favorite_models_snake_alias() {
            let json = r#"{"favorite_models": ["anthropic/claude-opus-4-6"]}"#;
            let settings: Settings = serde_json::from_str(json).unwrap();
            assert_eq!(settings.favorite_models.len(), 1);
            assert_eq!(settings.favorite_models[0], "anthropic/claude-opus-4-6");
        }

        #[test]
        fn favorite_models_default_empty() {
            let json = r#"{}"#;
            let settings: Settings = serde_json::from_str(json).unwrap();
            assert!(settings.favorite_models.is_empty());
        }

        #[test]
        fn zero_is_treated_as_unset() {
            let config = Config {
                request_timeout_secs: Some(0),
                ..Default::default()
            };
            assert_eq!(
                config.resolve_request_timeout_secs("openai"),
                DEFAULT_REQUEST_TIMEOUT_SECS
            );
        }

        #[test]
        fn bang_commands_default_disabled() {
            let json = r#"{}"#;
            let settings: Settings = serde_json::from_str(json).unwrap();
            assert!(!settings.bang_commands.enabled);
            assert!(!settings.bang_commands.add_to_history);
            assert!(settings.bang_commands.show_in_transcript);
        }

        #[test]
        fn bang_commands_deserialize() {
            let json = r#"{"bangCommands": {"enabled": true, "addToHistory": true, "showInTranscript": false}}"#;
            let settings: Settings = serde_json::from_str(json).unwrap();
            assert!(settings.bang_commands.enabled);
            assert!(settings.bang_commands.add_to_history);
            assert!(!settings.bang_commands.show_in_transcript);
        }
    }
}

// ---------------------------------------------------------------------------
// constants module
// ---------------------------------------------------------------------------
pub mod constants {
    pub const APP_NAME: &str = "claude";
    pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

    // Models
    pub const DEFAULT_MODEL: &str = "claude-opus-4-6";
    pub const SONNET_MODEL: &str = "claude-sonnet-4-6";
    pub const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
    pub const OPUS_MODEL: &str = "claude-opus-4-6";

    // Token limits
    pub const DEFAULT_MAX_TOKENS: u32 = 32_000;
    pub const MAX_TOKENS_HARD_LIMIT: u32 = 65_536;
    pub const DEFAULT_COMPACT_THRESHOLD: f32 = 0.9;
    pub const MAX_TURNS_DEFAULT: u32 = 10;
    pub const MAX_TOOL_ERRORS: u32 = 3;

    // API endpoints & headers
    pub const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
    pub const MINIMAX_ANTHROPIC_API_BASE: &str = "https://api.minimax.io/anthropic";
    pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";
    pub const ANTHROPIC_BETA_HEADER: &str =
        "interleaved-thinking-2025-05-14,token-efficient-tools-2025-02-19,files-api-2025-04-14,\
         effort-2025-11-24";

    // File system
    pub const CLAUDE_MD_FILENAME: &str = "AGENTS.md";
    pub const SETTINGS_FILENAME: &str = "settings.json";
    pub const HISTORY_FILENAME: &str = "conversations";
    pub const CONFIG_DIR_NAME: &str = ".claurst";

    // Tool names
    pub const TOOL_NAME_BASH: &str = "Bash";
    pub const TOOL_NAME_FILE_EDIT: &str = "Edit";
    pub const TOOL_NAME_FILE_READ: &str = "Read";
    pub const TOOL_NAME_FILE_WRITE: &str = "Write";
    pub const TOOL_NAME_GLOB: &str = "Glob";
    pub const TOOL_NAME_GREP: &str = "Grep";
    pub const TOOL_NAME_AGENT: &str = "Agent";
    pub const TOOL_NAME_WEB_FETCH: &str = "WebFetch";
    pub const TOOL_NAME_WEB_SEARCH: &str = "WebSearch";
    pub const TOOL_NAME_TODO_WRITE: &str = "TodoWrite";
    pub const TOOL_NAME_TASK_CREATE: &str = "TaskCreate";
    pub const TOOL_NAME_TASK_GET: &str = "TaskGet";
    pub const TOOL_NAME_TASK_UPDATE: &str = "TaskUpdate";
    pub const TOOL_NAME_TASK_LIST: &str = "TaskList";
    pub const TOOL_NAME_TASK_STOP: &str = "TaskStop";
    pub const TOOL_NAME_TASK_OUTPUT: &str = "TaskOutput";
    pub const TOOL_NAME_ENTER_PLAN_MODE: &str = "EnterPlanMode";
    pub const TOOL_NAME_EXIT_PLAN_MODE: &str = "ExitPlanMode";
    pub const TOOL_NAME_ASK_USER: &str = "AskUserQuestion";
    pub const TOOL_NAME_MCP: &str = "mcp";
    pub const TOOL_NAME_NOTEBOOK_EDIT: &str = "NotebookEdit";
    pub const TOOL_NAME_BATCH_EDIT: &str = "BatchEdit";
    pub const TOOL_NAME_APPLY_PATCH: &str = "ApplyPatch";

    // Session ID prefixes
    pub const SESSION_ID_PREFIX_BASH: &str = "b";
    pub const SESSION_ID_PREFIX_AGENT: &str = "a";
    pub const SESSION_ID_PREFIX_TEAMMATE: &str = "t";

    // Retry budget
    pub const MAX_OUTPUT_TOKENS_RETRIES: u32 = 3;
    pub const MAX_COMPACT_RETRIES: u32 = 3;

    // Stop sequences
    pub const STOP_SEQUENCE_END_OF_TURN: &str = "\n\nHuman:";
}

// ---------------------------------------------------------------------------
// context module
// ---------------------------------------------------------------------------
pub mod context {
    use std::path::PathBuf;
    use tokio::process::Command;

    /// Builds the system-level and user-level context that gets prepended to
    /// every conversation with the model.
    pub struct ContextBuilder {
        cwd: PathBuf,
        disable_claude_mds: bool,
    }

    impl ContextBuilder {
        pub fn new(cwd: PathBuf) -> Self {
            Self {
                cwd,
                disable_claude_mds: false,
            }
        }

        pub fn disable_claude_mds(mut self, val: bool) -> Self {
            self.disable_claude_mds = val;
            self
        }

        /// System context (git status, platform, IDE, etc.)
        pub async fn build_system_context(&self) -> String {
            let mut parts = vec![];

            // Platform information
            parts.push(format!("Platform: {}", std::env::consts::OS));
            parts.push(format!("Working directory: {}", self.cwd.display()));

            if let Some(git_context) = self.get_git_context().await {
                parts.push(git_context);
            }

            // IDE context — injected when an IDE extension is connected.
            // Mirrors TS getContextAttachments() → IdeContext attachment.
            if let Some(ide_ctx) = crate::attachments::get_ide_context() {
                parts.push(format!("# IDE Context\n{}", ide_ctx));
            }

            parts.join("\n\n")
        }

        /// User context (date, AGENTS.md memories, etc.)
        pub async fn build_user_context(&self) -> String {
            let mut parts = vec![];

            let date = chrono::Local::now().format("%A, %B %d, %Y").to_string();
            parts.push(format!("Today's date is {}.", date));

            if !self.disable_claude_mds {
                if let Some(claude_md) = self.find_and_read_claude_md().await {
                    parts.push(claude_md);
                }
            }

            parts.join("\n\n")
        }

        /// Gather short git status + recent log.
        async fn get_git_context(&self) -> Option<String> {
            let output = Command::new("git")
                .args(["status", "--short", "--branch"])
                .current_dir(&self.cwd)
                .output()
                .await
                .ok()?;

            if !output.status.success() {
                return None;
            }

            let status = String::from_utf8_lossy(&output.stdout).to_string();

            let log_output = Command::new("git")
                .args(["log", "--oneline", "-5"])
                .current_dir(&self.cwd)
                .output()
                .await
                .ok()?;

            let log = String::from_utf8_lossy(&log_output.stdout).to_string();

            let mut result = format!("# Git Status\n{}", status.trim());
            if !log.trim().is_empty() {
                result.push_str(&format!("\n\n# Recent Commits\n{}", log.trim()));
            }

            Some(result)
        }

        /// Walk up from cwd looking for AGENTS.md files and the global one.
        async fn find_and_read_claude_md(&self) -> Option<String> {
            let mut claude_mds = vec![];

            // Global <claurst home>/AGENTS.md
            {
                let global_claude_md = crate::config::Settings::config_dir()
                    .join(crate::constants::CLAUDE_MD_FILENAME);
                if global_claude_md.exists() {
                    if let Ok(content) = tokio::fs::read_to_string(&global_claude_md).await {
                        claude_mds.push(format!(
                            "# Memory (from {})\n{}",
                            global_claude_md.display(),
                            content
                        ));
                    }
                }
            }

            // Walk from cwd up to filesystem root, collecting AGENTS.md
            let mut dir = Some(self.cwd.as_path());
            let mut project_mds: Vec<String> = vec![];
            while let Some(d) = dir {
                let candidate = d.join(crate::constants::CLAUDE_MD_FILENAME);
                if candidate.exists() {
                    if let Ok(content) = tokio::fs::read_to_string(&candidate).await {
                        project_mds.push(format!(
                            "# Project Memory (from {})\n{}",
                            candidate.display(),
                            content
                        ));
                    }
                }
                dir = d.parent();
            }
            // Reverse so outermost directory comes first
            project_mds.reverse();
            claude_mds.extend(project_mds);

            if claude_mds.is_empty() {
                None
            } else {
                Some(claude_mds.join("\n\n"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// permissions module
// ---------------------------------------------------------------------------
pub mod permissions {
    use serde::{Deserialize, Serialize};
    use std::sync::{Arc, Mutex};

    // -----------------------------------------------------------------------
    // Danger level assigned to each tool type
    // -----------------------------------------------------------------------

    /// How dangerous a tool operation is — used as the default decision when
    /// no explicit rule matches.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum PermissionLevel {
        /// Read-only operations (Glob, Grep, Read, WebSearch, etc.).
        Read,
        /// File write/edit operations (Write, Edit).
        Write,
        /// Shell command execution (Bash).
        Execute,
        /// Outbound network access (WebFetch).
        Network,
    }

    impl PermissionLevel {
        /// Derive the permission level from a well-known tool name.
        pub fn for_tool(tool_name: &str) -> Self {
            match tool_name {
                "Bash" | "bash" => Self::Execute,
                "Write" | "Edit" | "NotebookEdit" => Self::Write,
                "WebFetch" => Self::Network,
                _ => Self::Read,
            }
        }
    }

    // -----------------------------------------------------------------------
    // Rule action & scope
    // -----------------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub enum PermissionAction {
        Allow,
        Deny,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub enum PermissionScope {
        /// Only lasts for the current process session.
        Session,
        /// Saved to settings.json and survives restarts.
        Persistent,
    }

    // -----------------------------------------------------------------------
    // Rule definition
    // -----------------------------------------------------------------------

    /// A single permission rule.
    ///
    /// Matches requests where:
    ///   - `tool_name` is `None` (applies to every tool) OR equals the
    ///     request tool name.
    ///   - `path_pattern` is `None` OR the glob pattern matches the
    ///     request path.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PermissionRule {
        /// `None` means "applies to all tools".
        pub tool_name: Option<String>,
        /// Optional glob pattern for file / command paths.
        pub path_pattern: Option<String>,
        pub action: PermissionAction,
        pub scope: PermissionScope,
    }

    impl PermissionRule {
        /// Returns `true` when this rule matches the given tool name and
        /// optional path argument.
        pub fn matches(&self, tool_name: &str, path: Option<&str>) -> bool {
            // Tool name check
            if let Some(ref rule_tool) = self.tool_name {
                if rule_tool != tool_name {
                    return false;
                }
            }
            // Path pattern check — only when a pattern is specified
            if let Some(ref pattern) = self.path_pattern {
                let Some(p) = path else {
                    // Rule requires a path but none was provided → no match
                    return false;
                };
                let pat = match glob::Pattern::new(pattern) {
                    Ok(pat) => pat,
                    Err(_) => return false,
                };
                if !pat.matches(p) {
                    return false;
                }
            }
            true
        }
    }

    // -----------------------------------------------------------------------
    // Serialised rule (stored in settings.json)
    // -----------------------------------------------------------------------

    /// Serde-friendly representation of a `PermissionRule` saved to disk.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct SerializedPermissionRule {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub tool_name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub path_pattern: Option<String>,
        pub action: PermissionAction,
    }

    impl From<&PermissionRule> for SerializedPermissionRule {
        fn from(r: &PermissionRule) -> Self {
            Self {
                tool_name: r.tool_name.clone(),
                path_pattern: r.path_pattern.clone(),
                action: r.action.clone(),
            }
        }
    }

    impl From<&SerializedPermissionRule> for PermissionRule {
        fn from(s: &SerializedPermissionRule) -> Self {
            Self {
                tool_name: s.tool_name.clone(),
                path_pattern: s.path_pattern.clone(),
                action: s.action.clone(),
                scope: PermissionScope::Persistent,
            }
        }
    }

    // -----------------------------------------------------------------------
    // Decision type
    // -----------------------------------------------------------------------

    /// The outcome of evaluating a permission request.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub enum PermissionDecision {
        /// Unconditionally allow.
        Allow,
        /// Allow and remember permanently.
        AllowPermanently,
        /// Deny.
        Deny,
        /// Deny and remember permanently.
        DenyPermanently,
        /// Ask the user (show dialog) with an explanation of why.
        Ask { reason: String },
    }

    // -----------------------------------------------------------------------
    // Format a human-readable explanation for the dialog
    // -----------------------------------------------------------------------

    /// Build the explanation paragraph shown in the permission dialog.
    ///
    /// Mirrors the TS `createPermissionRequestMessage` / `permissionExplainer`
    /// output style.
    pub fn format_permission_reason(
        tool_name: &str,
        description: &str,
        path: Option<&str>,
        level: PermissionLevel,
    ) -> String {
        match level {
            PermissionLevel::Execute => description.to_string(),
            PermissionLevel::Write => {
                let target = path.unwrap_or(description);
                let extra = if target.contains("/etc/") || target.contains("\\etc\\") {
                    "\nModifying system files could affect network resolution \
                     and system configuration."
                } else if target.starts_with("~/.") || target.contains("/.") {
                    "\nThis is a hidden/configuration file."
                } else {
                    "\nThis will write to the filesystem."
                };
                format!("{} wants to write to `{}`{}", tool_name, target, extra)
            }
            PermissionLevel::Network => {
                let url = path.unwrap_or(description);
                format!(
                    "WebFetch wants to fetch: `{}`\nThis will make an outbound HTTP request.",
                    url
                )
            }
            PermissionLevel::Read => {
                let target = path.unwrap_or(description);
                format!("{} wants to read: `{}`", tool_name, target)
            }
        }
    }

    // -----------------------------------------------------------------------
    // PermissionManager
    // -----------------------------------------------------------------------

    /// Returns true when `path` falls under the active workspace roots.
    fn is_path_within_allowed_roots(
        path: &str,
        working_dir: Option<&std::path::Path>,
        allowed_roots: &[std::path::PathBuf],
    ) -> bool {
        let canonical_path =
            std::fs::canonicalize(path).unwrap_or_else(|_| std::path::PathBuf::from(path));

        let mut roots: Vec<std::path::PathBuf> = Vec::new();
        if let Some(root) = working_dir {
            roots.push(std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()));
        }
        roots.extend(
            allowed_roots
                .iter()
                .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| root.clone())),
        );

        roots.iter().any(|root| canonical_path.starts_with(root))
    }

    /// Pending permission request waiting for resolution (e.g. from a bridge
    /// remote peer or the interactive TUI dialog).
    pub struct PendingPermission {
        pub tool_use_id: String,
        pub created_at: std::time::Instant,
        pub resolve_tx: tokio::sync::oneshot::Sender<PermissionDecision>,
    }

    /// Central permission manager: holds mode, session rules, persistent
    /// rules, and any in-flight pending decisions.
    pub struct PermissionManager {
        pub mode: crate::config::PermissionMode,
        /// Rules added during this session only.
        pub session_rules: Vec<PermissionRule>,
        /// Rules loaded from / saved to settings.json.
        pub persistent_rules: Vec<PermissionRule>,
        /// Pending interactive decisions keyed by tool_use_id.
        pending: Vec<PendingPermission>,
    }

    impl PermissionManager {
        /// Construct from a mode and the current settings (which may contain
        /// previously-persisted rules).
        pub fn new(
            mode: crate::config::PermissionMode,
            settings: &crate::config::Settings,
        ) -> Self {
            let persistent_rules = settings
                .permission_rules
                .iter()
                .map(PermissionRule::from)
                .collect();
            Self {
                mode,
                session_rules: Vec::new(),
                persistent_rules,
                pending: Vec::new(),
            }
        }

        // ----------------------------------------------------------------
        // Evaluation (ported from TS hasPermissionsToUseTool)
        // ----------------------------------------------------------------

        /// Evaluate whether `tool_name` should be allowed to run.
        ///
        /// Evaluation order (faithful to TS behaviour):
        /// 1. BypassPermissions → always Allow.
        /// 2. Check deny rules (persistent first, then session) → if any
        ///    matched, Deny.
        /// 3. Check allow rules (persistent first, then session) → if any
        ///    matched, Allow.
        /// 4. AcceptEdits → Allow (auto-accept file edits).
        /// 5. Plan mode → Allow reads; deny everything else.
        /// 6. Default → derive from tool danger level.
        pub fn evaluate(
            &self,
            tool_name: &str,
            description: &str,
            path: Option<&str>,
            working_dir: Option<&std::path::Path>,
            allowed_roots: &[std::path::PathBuf],
        ) -> PermissionDecision {
            use crate::config::PermissionMode;

            // Step 1 — bypass everything
            if self.mode == PermissionMode::BypassPermissions {
                return PermissionDecision::Allow;
            }

            // Steps 2–3 — evaluate explicit rules (deny has priority over
            // allow; persistent rules evaluated before session rules within
            // each polarity, matching TS rule-source ordering)
            let all_rules = self
                .persistent_rules
                .iter()
                .chain(self.session_rules.iter());

            let mut deny_matched = false;
            let mut allow_matched = false;

            for rule in all_rules {
                if rule.matches(tool_name, path) {
                    match rule.action {
                        PermissionAction::Deny => {
                            deny_matched = true;
                        }
                        PermissionAction::Allow => {
                            allow_matched = true;
                        }
                    }
                }
            }

            if deny_matched {
                return PermissionDecision::Deny;
            }

            if allow_matched {
                return PermissionDecision::Allow;
            }

            let level = match PermissionLevel::for_tool(tool_name) {
                PermissionLevel::Read
                    if !matches!(
                        tool_name,
                        "Read"
                            | "Glob"
                            | "Grep"
                            | "ListMcpResources"
                            | "ReadMcpResource"
                            | "LSP"
                            | "Skill"
                    ) =>
                {
                    PermissionLevel::Execute
                }
                other => other,
            };
            let read_in_workspace = path.is_some_and(|target| {
                is_path_within_allowed_roots(target, working_dir, allowed_roots)
            });
            let should_ask_read = match tool_name {
                "ListMcpResources" | "ReadMcpResource" => true,
                _ if matches!(level, PermissionLevel::Read) && path.is_some() => !read_in_workspace,
                _ => false,
            };

            // Step 4 — AcceptEdits: only auto-allow Edit; everything else keeps normal checks.
            if self.mode == PermissionMode::AcceptEdits && tool_name == "Edit" {
                return PermissionDecision::Allow;
            }

            // Step 5 — Plan mode: reads only
            if self.mode == PermissionMode::Plan {
                return match level {
                    PermissionLevel::Read => PermissionDecision::Allow,
                    _ => PermissionDecision::Deny,
                };
            }

            // Step 6 — Default / remaining AcceptEdits behavior.
            match level {
                PermissionLevel::Read if !should_ask_read => PermissionDecision::Allow,
                PermissionLevel::Read
                | PermissionLevel::Write
                | PermissionLevel::Execute
                | PermissionLevel::Network => {
                    let reason = format_permission_reason(tool_name, description, path, level);
                    PermissionDecision::Ask { reason }
                }
            }
        }

        // ----------------------------------------------------------------
        // Rule management
        // ----------------------------------------------------------------

        /// Add an arbitrary rule to this manager.
        pub fn add_rule(&mut self, rule: PermissionRule) {
            match rule.scope {
                PermissionScope::Session => self.session_rules.push(rule),
                PermissionScope::Persistent => self.persistent_rules.push(rule),
            }
        }

        /// Allow `tool_name` for the rest of this session.
        pub fn add_session_allow(&mut self, tool_name: &str) {
            self.session_rules.push(PermissionRule {
                tool_name: Some(tool_name.to_string()),
                path_pattern: None,
                action: PermissionAction::Allow,
                scope: PermissionScope::Session,
            });
        }

        /// Allow `tool_name` on `path` (glob) for the rest of this session.
        pub fn add_session_allow_path(&mut self, tool_name: &str, path: &str) {
            self.session_rules.push(PermissionRule {
                tool_name: Some(tool_name.to_string()),
                path_pattern: Some(path.to_string()),
                action: PermissionAction::Allow,
                scope: PermissionScope::Session,
            });
        }

        /// Allow `tool_name` persistently and save to settings.
        pub fn add_persistent_allow(
            &mut self,
            tool_name: &str,
            settings: &mut crate::config::Settings,
        ) -> crate::error::Result<()> {
            let rule = PermissionRule {
                tool_name: Some(tool_name.to_string()),
                path_pattern: None,
                action: PermissionAction::Allow,
                scope: PermissionScope::Persistent,
            };
            let serialized = SerializedPermissionRule::from(&rule);
            settings.permission_rules.push(serialized);
            settings
                .save_sync()
                .map_err(|e| crate::error::ClaudeError::Config(e.to_string()))?;
            self.persistent_rules.push(rule);
            Ok(())
        }

        /// Allow `tool_name` persistently on `path` and save settings.
        pub fn add_persistent_allow_path(
            &mut self,
            tool_name: &str,
            path: &str,
            settings: &mut crate::config::Settings,
        ) -> crate::error::Result<()> {
            let rule = PermissionRule {
                tool_name: Some(tool_name.to_string()),
                path_pattern: Some(path.to_string()),
                action: PermissionAction::Allow,
                scope: PermissionScope::Persistent,
            };
            let serialized = SerializedPermissionRule::from(&rule);
            settings.permission_rules.push(serialized);
            settings
                .save_sync()
                .map_err(|e| crate::error::ClaudeError::Config(e.to_string()))?;
            self.persistent_rules.push(rule);
            Ok(())
        }

        /// Remove a persistent rule by index and save settings.
        pub fn remove_rule(
            &mut self,
            idx: usize,
            settings: &mut crate::config::Settings,
        ) -> crate::error::Result<()> {
            if idx >= settings.permission_rules.len() {
                return Err(crate::error::ClaudeError::Config(format!(
                    "Rule index {} out of bounds",
                    idx
                )));
            }
            settings.permission_rules.remove(idx);
            settings
                .save_sync()
                .map_err(|e| crate::error::ClaudeError::Config(e.to_string()))?;
            // Rebuild persistent_rules from the updated settings
            self.persistent_rules = settings
                .permission_rules
                .iter()
                .map(PermissionRule::from)
                .collect();
            Ok(())
        }

        // ----------------------------------------------------------------
        // Bridge / async pending permissions
        // ----------------------------------------------------------------

        /// Register a pending permission and return a receiver.  The caller
        /// awaits the receiver and gets a `PermissionDecision` when the user
        /// (or a bridge peer) resolves the request.
        pub fn register_pending(
            &mut self,
            id: String,
        ) -> tokio::sync::oneshot::Receiver<PermissionDecision> {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.pending.push(PendingPermission {
                tool_use_id: id,
                created_at: std::time::Instant::now(),
                resolve_tx: tx,
            });
            rx
        }

        /// Resolve a pending permission by `tool_use_id`, delivering
        /// `decision` to the waiting receiver.  No-op if the ID is unknown.
        pub fn resolve_pending(&mut self, id: &str, decision: PermissionDecision) {
            if let Some(pos) = self.pending.iter().position(|p| p.tool_use_id == id) {
                let pending = self.pending.remove(pos);
                let _ = pending.resolve_tx.send(decision);
            }
        }
    }

    // -----------------------------------------------------------------------
    // PermissionRequest (passed to handlers & TUI)
    // -----------------------------------------------------------------------

    #[derive(Debug, Clone)]
    pub struct PermissionRequest {
        pub tool_name: String,
        pub description: String,
        pub details: Option<String>,
        pub is_read_only: bool,
        /// Canonical or resolved target path when the permission decision is path-sensitive.
        pub path: Option<String>,
        /// Current workspace root used for path-boundary checks.
        pub working_dir: Option<std::path::PathBuf>,
        /// Additional workspace roots considered in-bounds for file access.
        pub allowed_roots: Vec<std::path::PathBuf>,
        /// Context-aware description showing user WHY the tool needs permission.
        /// E.g. "bash: execute `ls -la /home`", "write file: /path/to/.bashrc", "fetch: https://example.com"
        pub context_description: Option<String>,
    }

    // -----------------------------------------------------------------------
    // PermissionHandler trait + handlers
    // -----------------------------------------------------------------------

    /// Trait implemented by anything that can decide whether to allow a tool.
    pub trait PermissionHandler: Send + Sync {
        fn check_permission(&self, request: &PermissionRequest) -> PermissionDecision;
        fn request_permission(&self, request: &PermissionRequest) -> PermissionDecision;
    }

    /// Handler for non-interactive / headless modes.
    ///
    /// Uses simple mode-based rules.  For rule-based evaluation backed by a
    /// `PermissionManager`, use `ManagedAutoPermissionHandler` instead.
    pub struct AutoPermissionHandler {
        pub mode: crate::config::PermissionMode,
    }

    impl PermissionHandler for AutoPermissionHandler {
        fn check_permission(&self, request: &PermissionRequest) -> PermissionDecision {
            use crate::config::PermissionMode;
            match self.mode {
                PermissionMode::BypassPermissions => PermissionDecision::Allow,
                PermissionMode::AcceptEdits => {
                    if request.tool_name == "Edit" || request.is_read_only {
                        PermissionDecision::Allow
                    } else {
                        PermissionDecision::Deny
                    }
                }
                PermissionMode::Plan => {
                    if request.is_read_only {
                        PermissionDecision::Allow
                    } else {
                        PermissionDecision::Deny
                    }
                }
                PermissionMode::Default => {
                    if request.is_read_only {
                        PermissionDecision::Allow
                    } else {
                        PermissionDecision::Deny
                    }
                }
            }
        }

        fn request_permission(&self, request: &PermissionRequest) -> PermissionDecision {
            self.check_permission(request)
        }
    }

    /// Permission handler for interactive (TUI) mode.
    ///
    /// Uses simple mode-based rules.  For rule-based evaluation backed by a
    /// `PermissionManager`, use `ManagedInteractivePermissionHandler`.
    pub struct InteractivePermissionHandler {
        pub mode: crate::config::PermissionMode,
    }

    impl PermissionHandler for InteractivePermissionHandler {
        fn check_permission(&self, request: &PermissionRequest) -> PermissionDecision {
            use crate::config::PermissionMode;
            match self.mode {
                PermissionMode::Plan => {
                    if request.is_read_only {
                        PermissionDecision::Allow
                    } else {
                        PermissionDecision::Deny
                    }
                }
                // In Default / AcceptEdits / BypassPermissions the user is
                // watching the TUI so we allow all.
                _ => PermissionDecision::Allow,
            }
        }

        fn request_permission(&self, request: &PermissionRequest) -> PermissionDecision {
            self.check_permission(request)
        }
    }

    // ---- Manager-backed handlers -----------------------------------------

    /// Non-interactive handler backed by a shared `PermissionManager`.
    ///
    /// Delegates to `PermissionManager::evaluate`; converts `Ask` decisions
    /// into `Deny` (no interactive prompt available in headless mode).
    pub struct ManagedAutoPermissionHandler {
        pub manager: Arc<Mutex<PermissionManager>>,
    }

    impl ManagedAutoPermissionHandler {
        pub fn new(manager: Arc<Mutex<PermissionManager>>) -> Self {
            Self { manager }
        }
    }

    impl PermissionHandler for ManagedAutoPermissionHandler {
        fn check_permission(&self, request: &PermissionRequest) -> PermissionDecision {
            if let Ok(m) = self.manager.lock() {
                let decision = m.evaluate(
                    &request.tool_name,
                    &request.description,
                    request.path.as_deref(),
                    request.working_dir.as_deref(),
                    &request.allowed_roots,
                );
                return match decision {
                    PermissionDecision::Ask { .. } => PermissionDecision::Deny,
                    other => other,
                };
            }
            PermissionDecision::Deny
        }

        fn request_permission(&self, request: &PermissionRequest) -> PermissionDecision {
            self.check_permission(request)
        }
    }

    /// Interactive (TUI) handler backed by a shared `PermissionManager`.
    ///
    /// Delegates to `PermissionManager::evaluate`; passes `Ask` decisions
    /// through so the TUI dialog can display them.
    pub struct ManagedInteractivePermissionHandler {
        pub manager: Arc<Mutex<PermissionManager>>,
    }

    impl ManagedInteractivePermissionHandler {
        pub fn new(manager: Arc<Mutex<PermissionManager>>) -> Self {
            Self { manager }
        }
    }

    impl PermissionHandler for ManagedInteractivePermissionHandler {
        fn check_permission(&self, request: &PermissionRequest) -> PermissionDecision {
            if let Ok(m) = self.manager.lock() {
                return m.evaluate(
                    &request.tool_name,
                    &request.description,
                    request.path.as_deref(),
                    request.working_dir.as_deref(),
                    &request.allowed_roots,
                );
            }
            // If the lock is poisoned fall back to allow (user is watching)
            PermissionDecision::Allow
        }

        fn request_permission(&self, request: &PermissionRequest) -> PermissionDecision {
            self.check_permission(request)
        }
    }

    // Convenience constructor aliases used by the spec
    impl InteractivePermissionHandler {
        /// Build a manager-backed interactive handler.
        pub fn with_manager(
            manager: Arc<Mutex<PermissionManager>>,
        ) -> ManagedInteractivePermissionHandler {
            ManagedInteractivePermissionHandler::new(manager)
        }
    }

    impl AutoPermissionHandler {
        /// Build a manager-backed auto handler.
        pub fn with_manager(
            manager: Arc<Mutex<PermissionManager>>,
        ) -> ManagedAutoPermissionHandler {
            ManagedAutoPermissionHandler::new(manager)
        }
    }

    // -----------------------------------------------------------------------
    // Unit tests
    // -----------------------------------------------------------------------

    #[cfg(test)]
    mod perm_tests {
        use super::*;
        use crate::config::{PermissionMode, Settings};

        fn mgr(mode: PermissionMode) -> PermissionManager {
            PermissionManager::new(mode, &Settings::default())
        }

        #[test]
        fn bypass_always_allows() {
            let m = mgr(PermissionMode::BypassPermissions);
            assert_eq!(
                m.evaluate("Bash", "rm -rf /", None, None, &[]),
                PermissionDecision::Allow
            );
        }

        #[test]
        fn default_read_allows_workspace_paths() {
            let m = mgr(PermissionMode::Default);
            let cwd = std::path::Path::new("/workspace");
            assert_eq!(
                m.evaluate(
                    "Read",
                    "read file",
                    Some("/workspace/src/lib.rs"),
                    Some(cwd),
                    &[],
                ),
                PermissionDecision::Allow
            );
        }

        #[test]
        fn default_read_asks_outside_workspace() {
            let m = mgr(PermissionMode::Default);
            let cwd = std::path::Path::new("/workspace");
            match m.evaluate(
                "Read",
                "read file",
                Some("/tmp/outside.txt"),
                Some(cwd),
                &[],
            ) {
                PermissionDecision::Ask { .. } => {}
                other => panic!("Expected Ask, got {:?}", other),
            }
        }

        #[test]
        fn default_read_allows_additional_workspace_roots() {
            let m = mgr(PermissionMode::Default);
            let cwd = std::path::Path::new("/workspace");
            let extra = vec![std::path::PathBuf::from("/external")];
            assert_eq!(
                m.evaluate(
                    "Read",
                    "read file",
                    Some("/external/notes.txt"),
                    Some(cwd),
                    &extra,
                ),
                PermissionDecision::Allow
            );
        }

        #[test]
        fn default_bash_asks() {
            let m = mgr(PermissionMode::Default);
            match m.evaluate("Bash", "echo hello", None, None, &[]) {
                PermissionDecision::Ask { .. } => {}
                other => panic!("Expected Ask, got {:?}", other),
            }
        }

        #[test]
        fn session_allow_overrides_default() {
            let mut m = mgr(PermissionMode::Default);
            m.add_session_allow("Bash");
            assert_eq!(
                m.evaluate("Bash", "echo hi", None, None, &[]),
                PermissionDecision::Allow
            );
        }

        #[test]
        fn deny_beats_allow() {
            let mut m = mgr(PermissionMode::Default);
            m.add_session_allow("Bash");
            m.add_rule(PermissionRule {
                tool_name: Some("Bash".to_string()),
                path_pattern: None,
                action: PermissionAction::Deny,
                scope: PermissionScope::Session,
            });
            assert_eq!(
                m.evaluate("Bash", "echo hi", None, None, &[]),
                PermissionDecision::Deny
            );
        }

        #[test]
        fn plan_denies_writes() {
            let m = mgr(PermissionMode::Plan);
            assert_eq!(
                m.evaluate("Write", "write file", Some("/tmp/foo"), None, &[]),
                PermissionDecision::Deny
            );
        }

        #[test]
        fn plan_allows_reads() {
            let m = mgr(PermissionMode::Plan);
            assert_eq!(
                m.evaluate("Read", "read file", Some("/tmp/foo"), None, &[]),
                PermissionDecision::Allow
            );
        }

        #[test]
        fn accept_edits_only_allows_edit() {
            let m = mgr(PermissionMode::AcceptEdits);
            assert_eq!(
                m.evaluate(
                    "Edit",
                    "edit file",
                    Some("/workspace/src/lib.rs"),
                    None,
                    &[]
                ),
                PermissionDecision::Allow
            );
            match m.evaluate("Bash", "rm -rf /tmp", None, None, &[]) {
                PermissionDecision::Ask { .. } => {}
                other => panic!("Expected Ask, got {:?}", other),
            }
        }

        #[test]
        fn glob_path_allow_matches() {
            let mut m = mgr(PermissionMode::Default);
            m.add_rule(PermissionRule {
                tool_name: Some("Write".to_string()),
                path_pattern: Some("/tmp/**".to_string()),
                action: PermissionAction::Allow,
                scope: PermissionScope::Session,
            });
            assert_eq!(
                m.evaluate("Write", "write", Some("/tmp/foo/bar.txt"), None, &[]),
                PermissionDecision::Allow
            );
        }

        #[test]
        fn glob_path_no_match_asks() {
            let mut m = mgr(PermissionMode::Default);
            m.add_rule(PermissionRule {
                tool_name: Some("Write".to_string()),
                path_pattern: Some("/tmp/**".to_string()),
                action: PermissionAction::Allow,
                scope: PermissionScope::Session,
            });
            match m.evaluate("Write", "write", Some("/etc/hosts"), None, &[]) {
                PermissionDecision::Ask { .. } => {}
                other => panic!("Expected Ask, got {:?}", other),
            }
        }

        #[test]
        fn format_reason_bash() {
            let s = format_permission_reason(
                "Bash",
                "This will execute a shell command.",
                None,
                PermissionLevel::Execute,
            );
            assert_eq!(s, "This will execute a shell command.");
        }

        #[test]
        fn format_reason_powershell() {
            let s = format_permission_reason(
                "PowerShell",
                "[High risk] This may modify system-wide security policy.",
                None,
                PermissionLevel::Execute,
            );
            assert_eq!(
                s,
                "[High risk] This may modify system-wide security policy."
            );
        }

        #[test]
        fn format_reason_write_etc() {
            let s = format_permission_reason(
                "Write",
                "write",
                Some("/etc/hosts"),
                PermissionLevel::Write,
            );
            assert!(s.contains("/etc/hosts"));
            assert!(s.contains("system files"));
        }

        #[test]
        fn format_reason_webfetch() {
            let s = format_permission_reason(
                "WebFetch",
                "fetch",
                Some("https://example.com"),
                PermissionLevel::Network,
            );
            assert!(s.contains("https://example.com"));
            assert!(s.contains("HTTP request"));
        }
    }
}

// ---------------------------------------------------------------------------
// history module
// ---------------------------------------------------------------------------
pub mod history {
    use crate::types::Message;
    use serde::{Deserialize, Serialize};

    /// A checkpoint snapshot of conversation messages at a specific point in time.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SessionCheckpoint {
        /// The message index this checkpoint was taken at (exclusive upper bound).
        pub message_idx: usize,
        /// Optional human-readable label.
        pub label: Option<String>,
        /// When this checkpoint was created.
        pub created_at: chrono::DateTime<chrono::Utc>,
        /// Snapshot of all messages up to (and including) `message_idx - 1`.
        pub snapshot: Vec<Message>,
    }

    /// A single persisted conversation session.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ConversationSession {
        pub id: String,
        pub created_at: chrono::DateTime<chrono::Utc>,
        pub updated_at: chrono::DateTime<chrono::Utc>,
        pub messages: Vec<Message>,
        pub model: String,
        pub title: Option<String>,
        pub working_dir: Option<String>,
        /// Tags for filtering / searching sessions.
        #[serde(default)]
        pub tags: Vec<String>,
        /// ID of the session this was branched from, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub branch_from: Option<String>,
        /// Message index in the parent session at which this branch was created.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub branch_at_message: Option<usize>,
        /// Remote bridge URL if this session is mirrored to a remote endpoint.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub remote_session_url: Option<String>,
        /// Accumulated USD cost for this session.
        #[serde(default)]
        pub total_cost: f64,
        /// Accumulated token count for this session.
        #[serde(default)]
        pub total_tokens: u64,
        /// Saved checkpoints (rewind points) within this session.
        #[serde(default)]
        pub checkpoints: Vec<SessionCheckpoint>,
        /// ID of the parent session this was forked from (via /fork).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub parent_session_id: Option<String>,
        /// Message index in the parent session at which this fork was created.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub fork_point_message_index: Option<usize>,
    }

    impl ConversationSession {
        pub fn new(model: String) -> Self {
            let now = chrono::Utc::now();
            Self {
                id: uuid::Uuid::new_v4().to_string(),
                created_at: now,
                updated_at: now,
                messages: vec![],
                model,
                title: None,
                working_dir: None,
                tags: vec![],
                branch_from: None,
                branch_at_message: None,
                remote_session_url: None,
                total_cost: 0.0,
                total_tokens: 0,
                checkpoints: vec![],
                parent_session_id: None,
                fork_point_message_index: None,
            }
        }

        pub fn add_message(&mut self, message: Message) {
            self.messages.push(message);
            self.updated_at = chrono::Utc::now();
        }

        pub fn message_count(&self) -> usize {
            self.messages.len()
        }

        pub fn last_user_message(&self) -> Option<&Message> {
            self.messages
                .iter()
                .rev()
                .find(|m| m.role == crate::types::Role::User)
        }
    }

    // -------------------------------------------------------------------------
    // Checkpoint helpers (synchronous, operate on a mutable session in-memory)
    // -------------------------------------------------------------------------

    /// Create a checkpoint at the current end of the session's message list.
    /// The checkpoint captures all messages currently in the session.
    pub fn create_checkpoint(session: &mut ConversationSession, label: Option<&str>) {
        let idx = session.messages.len();
        let checkpoint = SessionCheckpoint {
            message_idx: idx,
            label: label.map(|s| s.to_string()),
            created_at: chrono::Utc::now(),
            snapshot: session.messages.clone(),
        };
        session.checkpoints.push(checkpoint);
        session.updated_at = chrono::Utc::now();
    }

    /// Restore the session's messages to those saved in checkpoint `idx`.
    ///
    /// Returns the messages that were replaced (i.e. the messages discarded by
    /// the rewind).  The session's `messages` field is replaced with the
    /// checkpoint snapshot; `updated_at` is refreshed.
    ///
    /// # Panics
    /// Panics if `idx` is out of bounds (i.e. >= `session.checkpoints.len()`).
    pub fn restore_checkpoint(session: &mut ConversationSession, idx: usize) -> Vec<Message> {
        let snapshot = session.checkpoints[idx].snapshot.clone();
        let replaced = std::mem::replace(&mut session.messages, snapshot);
        session.updated_at = chrono::Utc::now();
        replaced
    }

    // -------------------------------------------------------------------------
    // Persistent storage helpers
    // -------------------------------------------------------------------------

    /// The on-disk directory for conversation sessions.
    fn sessions_dir() -> std::path::PathBuf {
        crate::config::Settings::config_dir().join("sessions")
    }

    /// Save a session to `~/.claurst/sessions/<id>.json`.
    pub async fn save_session(session: &ConversationSession) -> anyhow::Result<()> {
        let dir = sessions_dir();
        tokio::fs::create_dir_all(&dir).await?;
        crate::accounts::set_user_only_dir_perms(&dir);
        let path = dir.join(format!("{}.json", session.id));
        let content = serde_json::to_string_pretty(session)?;
        tokio::fs::write(&path, content).await?;
        // Session transcripts can contain secrets pulled into context; keep
        // them owner-only (issue #212).
        crate::accounts::set_user_only_perms(&path);
        Ok(())
    }

    /// Load a specific session by ID.
    pub async fn load_session(id: &str) -> anyhow::Result<ConversationSession> {
        let path = sessions_dir().join(format!("{}.json", id));
        let content = tokio::fs::read_to_string(&path).await?;
        Ok(serde_json::from_str(&content)?)
    }

    /// List all sessions, sorted by most-recently-updated first.
    pub async fn list_sessions() -> Vec<ConversationSession> {
        let dir = sessions_dir();
        if !dir.exists() {
            return vec![];
        }

        let mut sessions = vec![];
        if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json") {
                    if let Ok(content) = tokio::fs::read_to_string(&path).await {
                        if let Ok(session) = serde_json::from_str::<ConversationSession>(&content) {
                            sessions.push(session);
                        }
                    }
                }
            }
        }

        sessions.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
        sessions
    }

    /// Delete a session by ID.
    pub async fn delete_session(id: &str) -> anyhow::Result<()> {
        let path = sessions_dir().join(format!("{}.json", id));
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }
        Ok(())
    }

    /// Rename (set the title of) a session.
    pub async fn rename_session(id: &str, new_title: &str) -> anyhow::Result<()> {
        let mut session = load_session(id).await?;
        session.title = Some(new_title.to_string());
        session.updated_at = chrono::Utc::now();
        save_session(&session).await
    }

    /// Add a tag to a session (idempotent — duplicate tags are ignored).
    pub async fn tag_session(id: &str, tag: &str) -> anyhow::Result<()> {
        let mut session = load_session(id).await?;
        let tag_str = tag.to_string();
        if !session.tags.contains(&tag_str) {
            session.tags.push(tag_str);
            session.updated_at = chrono::Utc::now();
            save_session(&session).await?;
        }
        Ok(())
    }

    /// Remove a tag from a session (no-op if tag is not present).
    pub async fn untag_session(id: &str, tag: &str) -> anyhow::Result<()> {
        let mut session = load_session(id).await?;
        let before_len = session.tags.len();
        session.tags.retain(|t| t != tag);
        if session.tags.len() != before_len {
            session.updated_at = chrono::Utc::now();
            save_session(&session).await?;
        }
        Ok(())
    }

    /// Create a new session that is a branch of `source_id` at message index
    /// `at_message_idx`.  The new session starts with messages
    /// `[0, at_message_idx)` copied from the source.
    pub async fn branch_session(
        source_id: &str,
        at_message_idx: usize,
        new_title: Option<&str>,
    ) -> anyhow::Result<ConversationSession> {
        let source = load_session(source_id).await?;
        let clamped_idx = at_message_idx.min(source.messages.len());
        let now = chrono::Utc::now();
        let branched = ConversationSession {
            id: uuid::Uuid::new_v4().to_string(),
            created_at: now,
            updated_at: now,
            messages: source.messages[..clamped_idx].to_vec(),
            model: source.model.clone(),
            title: new_title
                .map(|t| t.to_string())
                .or_else(|| source.title.as_ref().map(|t| format!("{} (branch)", t))),
            working_dir: source.working_dir.clone(),
            tags: source.tags.clone(),
            branch_from: Some(source_id.to_string()),
            branch_at_message: Some(clamped_idx),
            remote_session_url: None,
            total_cost: 0.0,
            total_tokens: 0,
            checkpoints: vec![],
            parent_session_id: None,
            fork_point_message_index: None,
        };
        save_session(&branched).await?;
        Ok(branched)
    }

    /// Search sessions whose title or tags contain `query` (case-insensitive
    /// substring match).  Results are sorted by `updated_at` descending.
    pub async fn search_sessions(query: &str) -> Vec<ConversationSession> {
        let lower_query = query.to_lowercase();
        let all = list_sessions().await;
        all.into_iter()
            .filter(|s| {
                // Check title
                if let Some(ref title) = s.title {
                    if title.to_lowercase().contains(&lower_query) {
                        return true;
                    }
                }
                // Check tags
                if s.tags
                    .iter()
                    .any(|t| t.to_lowercase().contains(&lower_query))
                {
                    return true;
                }
                false
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// cost module
// ---------------------------------------------------------------------------
pub mod cost {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// Free upstream provider IDs used in the free provider system.
    ///
    /// These overlap with providers that appear in `api_key_env_vars_for_provider`.
    /// When adding a provider to one, check whether it also belongs in the other.
    const FREE_UPSTREAM_IDS: &[&str] = &[
        "groq",
        "cerebras",
        "google",
        "mistral",
        "sambanova",
        "nvidia",
        "cohere",
        "openrouter",
        "opencode-zen",
        "zai",
        "zhipuai",
    ];

    /// Check if a model name is an upstream-prefixed free model (e.g., "groq/llama-3.3-70b-versatile").
    fn is_free_upstream_model(model: &str) -> bool {
        for upstream_id in FREE_UPSTREAM_IDS {
            if model.starts_with(&format!("{}/", upstream_id)) {
                return true;
            }
        }
        false
    }

    /// Per-model pricing tiers (USD per million tokens).
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct ModelPricing {
        pub input_per_mtk: f64,
        pub output_per_mtk: f64,
        pub cache_creation_per_mtk: f64,
        pub cache_read_per_mtk: f64,
    }

    impl ModelPricing {
        /// Pricing for Claude Opus 4 family.
        pub const OPUS: Self = Self {
            input_per_mtk: 15.0,
            output_per_mtk: 75.0,
            cache_creation_per_mtk: 18.75,
            cache_read_per_mtk: 1.5,
        };

        /// Pricing for Claude Sonnet 4 family.
        pub const SONNET: Self = Self {
            input_per_mtk: 3.0,
            output_per_mtk: 15.0,
            cache_creation_per_mtk: 3.75,
            cache_read_per_mtk: 0.3,
        };

        /// Pricing for Claude Haiku family.
        pub const HAIKU: Self = Self {
            input_per_mtk: 0.80,
            output_per_mtk: 4.0,
            cache_creation_per_mtk: 1.0,
            cache_read_per_mtk: 0.08,
        };

        /// Free model pricing (no cost).
        pub const FREE: Self = Self {
            input_per_mtk: 0.0,
            output_per_mtk: 0.0,
            cache_creation_per_mtk: 0.0,
            cache_read_per_mtk: 0.0,
        };

        /// Default pricing is Opus (most capable, highest cost).
        pub fn default_pricing() -> Self {
            Self::OPUS
        }

        /// Pick pricing based on model name substring matching.
        pub fn for_model(model: &str) -> Self {
            // Check for free models first (those with "-free" suffix, "free/" prefix, or upstream-prefixed free model)
            if model.ends_with("-free")
                || model.starts_with("free/")
                || is_free_upstream_model(model)
            {
                Self::FREE
            } else if model.contains("opus") {
                Self::OPUS
            } else if model.contains("haiku") {
                Self::HAIKU
            } else {
                // Default to Sonnet pricing for unknown models
                Self::SONNET
            }
        }
    }

    impl Default for ModelPricing {
        fn default() -> Self {
            Self::OPUS
        }
    }

    /// Thread-safe, lock-free cost tracker that accumulates token usage.
    #[derive(Debug, Default)]
    pub struct CostTracker {
        input_tokens: AtomicU64,
        output_tokens: AtomicU64,
        cache_creation_tokens: AtomicU64,
        cache_read_tokens: AtomicU64,
        pricing: parking_lot::RwLock<ModelPricing>,
    }

    // We need a default for RwLock<ModelPricing> -- use Opus as default.
    impl CostTracker {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                pricing: parking_lot::RwLock::new(ModelPricing::OPUS),
                ..Default::default()
            })
        }

        pub fn with_model(model: &str) -> Arc<Self> {
            Arc::new(Self {
                pricing: parking_lot::RwLock::new(ModelPricing::for_model(model)),
                ..Default::default()
            })
        }

        pub fn set_model(&self, model: &str) {
            *self.pricing.write() = ModelPricing::for_model(model);
        }

        pub fn add_usage(&self, input: u64, output: u64, cache_creation: u64, cache_read: u64) {
            self.input_tokens.fetch_add(input, Ordering::Relaxed);
            self.output_tokens.fetch_add(output, Ordering::Relaxed);
            self.cache_creation_tokens
                .fetch_add(cache_creation, Ordering::Relaxed);
            self.cache_read_tokens
                .fetch_add(cache_read, Ordering::Relaxed);
        }

        pub fn total_cost_usd(&self) -> f64 {
            let pricing = *self.pricing.read();
            let input = self.input_tokens.load(Ordering::Relaxed) as f64;
            let output = self.output_tokens.load(Ordering::Relaxed) as f64;
            let cache_creation = self.cache_creation_tokens.load(Ordering::Relaxed) as f64;
            let cache_read = self.cache_read_tokens.load(Ordering::Relaxed) as f64;

            (input * pricing.input_per_mtk
                + output * pricing.output_per_mtk
                + cache_creation * pricing.cache_creation_per_mtk
                + cache_read * pricing.cache_read_per_mtk)
                / 1_000_000.0
        }

        pub fn total_tokens(&self) -> u64 {
            self.input_tokens.load(Ordering::Relaxed)
                + self.output_tokens.load(Ordering::Relaxed)
                + self.cache_creation_tokens.load(Ordering::Relaxed)
                + self.cache_read_tokens.load(Ordering::Relaxed)
        }

        pub fn input_tokens(&self) -> u64 {
            self.input_tokens.load(Ordering::Relaxed)
        }

        pub fn output_tokens(&self) -> u64 {
            self.output_tokens.load(Ordering::Relaxed)
        }

        pub fn cache_creation_tokens(&self) -> u64 {
            self.cache_creation_tokens.load(Ordering::Relaxed)
        }

        pub fn cache_read_tokens(&self) -> u64 {
            self.cache_read_tokens.load(Ordering::Relaxed)
        }

        /// Produce a human-readable summary string, e.g. for display in the TUI.
        pub fn summary(&self) -> String {
            let cost = self.total_cost_usd();
            let total = self.total_tokens();
            if cost == 0.0 {
                format!("{} tokens ($0.00)", total)
            } else if cost < 0.01 {
                format!("{} tokens (<$0.01)", total)
            } else {
                format!("{} tokens (${:.2})", total, cost)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// hooks module
// ---------------------------------------------------------------------------
pub mod hooks {
    use crate::config::{HookEntry, HookEvent};
    use serde_json::Value;
    use std::collections::HashMap;
    use std::path::Path;
    use tracing::{debug, warn};

    /// Context passed to hook commands via stdin as JSON.
    #[derive(Debug, serde::Serialize)]
    pub struct HookContext {
        pub event: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub tool_name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub tool_input: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub tool_output: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub session_id: Option<String>,
    }

    /// Result of running a hook.
    #[derive(Debug)]
    pub enum HookOutcome {
        /// Hook ran and allowed execution to continue.
        Allowed,
        /// Hook ran and blocked execution (blocking hook with non-zero exit).
        Blocked(String),
        /// Hook produced modified output (stdout of the hook command).
        Modified(String),
    }

    /// Run all hooks registered for the given event. Returns the first blocking
    /// result if any hook blocks, otherwise `Allowed`.
    pub async fn run_hooks(
        hooks: &HashMap<HookEvent, Vec<HookEntry>>,
        event: HookEvent,
        ctx: &HookContext,
        working_dir: &Path,
    ) -> HookOutcome {
        let Some(entries) = hooks.get(&event) else {
            return HookOutcome::Allowed;
        };

        let ctx_json = match serde_json::to_string(ctx) {
            Ok(j) => j,
            Err(e) => {
                warn!("Failed to serialize hook context: {}", e);
                return HookOutcome::Allowed;
            }
        };

        for entry in entries {
            // Apply tool filter if set
            if let Some(ref filter) = entry.tool_filter {
                if let Some(ref tool) = ctx.tool_name {
                    if !filter.is_empty() && filter != tool && filter != "*" {
                        continue;
                    }
                }
            }

            debug!(command = %entry.command, event = ?event, "Running hook");

            let result = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
                .args(if cfg!(windows) {
                    ["/C", &entry.command]
                } else {
                    ["-c", &entry.command]
                })
                .current_dir(working_dir)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn();

            let mut child = match result {
                Ok(c) => c,
                Err(e) => {
                    warn!(command = %entry.command, error = %e, "Failed to spawn hook");
                    continue;
                }
            };

            // Write context JSON to stdin
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(ctx_json.as_bytes()).await;
            }

            let output = match child.wait_with_output().await {
                Ok(o) => o,
                Err(e) => {
                    warn!(command = %entry.command, error = %e, "Hook wait failed");
                    continue;
                }
            };

            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let exit_ok = output.status.success();

            if !exit_ok && entry.blocking {
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                let reason = if !stderr.is_empty() { stderr } else { stdout };
                return HookOutcome::Blocked(format!(
                    "Hook '{}' blocked execution: {}",
                    entry.command,
                    reason.trim()
                ));
            }

            if !stdout.trim().is_empty() {
                return HookOutcome::Modified(stdout.trim().to_string());
            }
        }

        HookOutcome::Allowed
    }
}

// ---------------------------------------------------------------------------
// oauth module
// ---------------------------------------------------------------------------

/// OAuth 2.0 PKCE authentication support.
///
/// Supports two login paths mirroring the TypeScript implementation:
/// - **Console** (`org:create_api_key` scope): exchanges access token for an API key.
/// - **Claude.ai** (`user:inference` scope): uses the access token as a Bearer credential.
pub mod oauth {
    use serde::{Deserialize, Serialize};

    // ---- Production OAuth endpoints & constants ----

    // Claude Code client ID, used in stealth-impersonation mode (see
    // `claurst_core::oauth_config` for the matching request-time headers and
    // system-prompt prefix wired into `claurst_api::AnthropicClient`).
    pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
    pub const CONSOLE_AUTHORIZE_URL: &str = "https://platform.claude.com/oauth/authorize";
    pub const CLAUDE_AI_AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
    pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
    pub const API_KEY_URL: &str = "https://api.anthropic.com/api/oauth/claude_cli/create_api_key";
    pub const MANUAL_REDIRECT_URL: &str = "https://platform.claude.com/oauth/code/callback";
    pub const CLAUDEAI_SUCCESS_URL: &str =
        "https://platform.claude.com/oauth/code/success?app=claude-code";
    pub const CONSOLE_SUCCESS_URL: &str = "https://platform.claude.com/buy_credits\
        ?returnUrl=/oauth/code/success%3Fapp%3Dclaude-code";

    /// All scopes requested during login (union of Console + Claude.ai scopes).
    pub const ALL_SCOPES: &[&str] = &[
        "org:create_api_key",
        "user:profile",
        "user:inference",
        "user:sessions:claude_code",
        "user:mcp_servers",
        "user:file_upload",
    ];

    /// Scope that identifies a Claude.ai subscription token (uses Bearer auth).
    pub const CLAUDE_AI_INFERENCE_SCOPE: &str = "user:inference";

    // ---- Stored token struct ----

    /// Persisted OAuth tokens (saved to `~/.claurst/oauth_tokens.json`).
    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    pub struct OAuthTokens {
        pub access_token: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub refresh_token: Option<String>,
        /// Unix timestamp in milliseconds when the access token expires.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub expires_at_ms: Option<i64>,
        pub scopes: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub account_uuid: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub email: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub organization_uuid: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub subscription_type: Option<String>,
        /// API key created for Console-flow users (exchanged from access token).
        #[serde(skip_serializing_if = "Option::is_none")]
        pub api_key: Option<String>,
    }

    impl OAuthTokens {
        /// Returns true if the token requires Bearer-style authorization
        /// (i.e. Claude.ai subscription with `user:inference` scope).
        pub fn uses_bearer_auth(&self) -> bool {
            self.scopes.iter().any(|s| s == CLAUDE_AI_INFERENCE_SCOPE)
        }

        /// The credential to present to the Anthropic API:
        /// - Console flow: the stored `api_key` (sk-ant-…)
        /// - Claude.ai flow: the `access_token` itself (Bearer)
        pub fn effective_credential(&self) -> Option<&str> {
            if self.uses_bearer_auth() {
                if self.access_token.is_empty() {
                    None
                } else {
                    Some(&self.access_token)
                }
            } else {
                self.api_key.as_deref()
            }
        }

        /// True if the access token has passed (or is within 5 minutes of) its expiry.
        pub fn is_expired(&self) -> bool {
            if let Some(exp) = self.expires_at_ms {
                let buffer_ms: i64 = 5 * 60 * 1000;
                let now_ms = chrono::Utc::now().timestamp_millis();
                (now_ms + buffer_ms) >= exp
            } else {
                false
            }
        }

        /// Legacy token file path — kept for backward-compat reads when no
        /// account registry exists yet. New writes go to per-account dirs.
        pub fn token_file_path() -> std::path::PathBuf {
            crate::config::Settings::config_dir().join("oauth_tokens.json")
        }

        /// Save tokens for a specific account profile under
        /// `~/.claurst/accounts/anthropic/<profile_id>/oauth_tokens.json`.
        pub async fn save_for_profile(&self, profile_id: &str) -> anyhow::Result<()> {
            let path = crate::accounts::anthropic_token_path(profile_id);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
                crate::accounts::set_user_only_dir_perms(parent);
            }
            tokio::fs::write(&path, serde_json::to_string_pretty(self)?).await?;
            // These are live OAuth access + refresh tokens — never leave them
            // world/group readable (issue #212).
            crate::accounts::set_user_only_perms(&path);
            Ok(())
        }

        /// Load tokens for a specific account profile, or `None` if missing.
        pub async fn load_for_profile(profile_id: &str) -> Option<Self> {
            let path = crate::accounts::anthropic_token_path(profile_id);
            let content = tokio::fs::read_to_string(&path).await.ok()?;
            serde_json::from_str(&content).ok()
        }

        /// Save these tokens, register/refresh a profile in the account
        /// registry, and mark it active. Returns the profile id used.
        ///
        /// If `label` is None, derives the id from email/account_uuid.
        pub async fn save_and_register(&self, label: Option<&str>) -> anyhow::Result<String> {
            use crate::accounts::{
                ensure_unique_profile_id, slugify_profile_id, AccountProfile, AccountRegistry,
                PROVIDER_ANTHROPIC,
            };

            let mut registry = AccountRegistry::load();

            // Identity-aware id resolution: if a profile with the same email
            // or account_uuid already exists, reuse it instead of stacking
            // duplicates.
            let existing_id = registry
                .list(PROVIDER_ANTHROPIC)
                .into_iter()
                .find(|p| {
                    (self.email.is_some() && p.email == self.email)
                        || (self.account_uuid.is_some() && p.account_id == self.account_uuid)
                })
                .map(|p| p.id);

            let id = if let Some(id) = existing_id {
                id
            } else if let Some(label) = label {
                ensure_unique_profile_id(&registry, PROVIDER_ANTHROPIC, label)
            } else {
                let base = self
                    .email
                    .as_deref()
                    .map(|e| e.split('@').next().unwrap_or(e).to_string())
                    .or_else(|| self.account_uuid.clone())
                    .unwrap_or_else(|| "account".to_string());
                ensure_unique_profile_id(&registry, PROVIDER_ANTHROPIC, &base)
            };

            self.save_for_profile(&id).await?;

            let profile = AccountProfile {
                id: id.clone(),
                label: label.map(slugify_profile_id),
                email: self.email.clone(),
                account_id: self.account_uuid.clone(),
                organization_uuid: self.organization_uuid.clone(),
                subscription_tier: self.subscription_type.clone(),
                added_at: None,
                last_selected_at: None,
            };
            registry.upsert(PROVIDER_ANTHROPIC, profile, true)?;
            Ok(id)
        }

        /// Save (active profile, or new profile if registry empty) — back-compat
        /// shim for callers that don't think in terms of profiles.
        pub async fn save(&self) -> anyhow::Result<()> {
            let registry = crate::accounts::AccountRegistry::load();
            if let Some(active) = registry.active(crate::accounts::PROVIDER_ANTHROPIC) {
                self.save_for_profile(active).await
            } else {
                // No registry yet — register as a new profile.
                self.save_and_register(None).await.map(|_| ())
            }
        }

        /// Load tokens for the active anthropic profile. Falls back to the
        /// legacy `~/.claurst/oauth_tokens.json` (auto-migrating it into a
        /// "default" profile on first read) if no registry exists.
        pub async fn load() -> Option<Self> {
            let mut registry = crate::accounts::AccountRegistry::load();

            if let Some(active) = registry.active(crate::accounts::PROVIDER_ANTHROPIC) {
                if let Some(t) = Self::load_for_profile(active).await {
                    return Some(t);
                }
            }

            // Fallback: legacy single-file storage. Migrate on the spot.
            let legacy = Self::token_file_path();
            if legacy.exists() {
                let content = tokio::fs::read_to_string(&legacy).await.ok()?;
                let tokens: Self = serde_json::from_str(&content).ok()?;
                // Best-effort migration: register under a derived id.
                if let Ok(id) = tokens.save_and_register(None).await {
                    let _ = tokio::fs::remove_file(&legacy).await;
                    // refresh active pointer
                    let _ = registry.switch_to(crate::accounts::PROVIDER_ANTHROPIC, &id);
                }
                return Some(tokens);
            }
            None
        }

        /// Clear credentials for the active profile (or all credentials if
        /// `purge_all` is true) and drop the profile from the registry.
        pub async fn clear() -> anyhow::Result<()> {
            let mut registry = crate::accounts::AccountRegistry::load();
            if let Some(active) = registry
                .active(crate::accounts::PROVIDER_ANTHROPIC)
                .map(String::from)
            {
                registry.remove(crate::accounts::PROVIDER_ANTHROPIC, &active)?;
            }
            // Also remove any legacy file.
            let legacy = Self::token_file_path();
            if legacy.exists() {
                tokio::fs::remove_file(&legacy).await?;
            }
            Ok(())
        }
    }

    // ---- PKCE helpers ----

    /// Generate a 32-byte random code verifier, base64url-encoded (no padding).
    pub fn generate_code_verifier() -> String {
        use base64::Engine;
        let mut bytes = [0u8; 32];
        let u1 = uuid::Uuid::new_v4();
        let u2 = uuid::Uuid::new_v4();
        bytes[..16].copy_from_slice(u1.as_bytes());
        bytes[16..].copy_from_slice(u2.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Derive the PKCE code challenge from a verifier: BASE64URL(SHA256(verifier)).
    pub fn generate_code_challenge(verifier: &str) -> String {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(verifier.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
    }

    /// Generate a random OAuth state parameter for CSRF protection.
    pub fn generate_state() -> String {
        use base64::Engine;
        let mut bytes = [0u8; 32];
        let u1 = uuid::Uuid::new_v4();
        let u2 = uuid::Uuid::new_v4();
        bytes[..16].copy_from_slice(u1.as_bytes());
        bytes[16..].copy_from_slice(u2.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    // ---- URL builder ----

    /// Build an OAuth authorization URL with all required PKCE parameters.
    pub fn build_auth_url(
        authorize_base: &str,
        code_challenge: &str,
        state: &str,
        callback_port: u16,
        is_manual: bool,
    ) -> String {
        let mut u = url::Url::parse(authorize_base).expect("valid OAuth authorize base URL");
        {
            let mut q = u.query_pairs_mut();
            q.append_pair("code", "true"); // tells the login page to show Claude Max upsell
            q.append_pair("client_id", CLIENT_ID);
            q.append_pair("response_type", "code");
            let redirect = if is_manual {
                MANUAL_REDIRECT_URL.to_string()
            } else {
                format!("http://localhost:{}/callback", callback_port)
            };
            q.append_pair("redirect_uri", &redirect);
            q.append_pair("scope", &ALL_SCOPES.join(" "));
            q.append_pair("code_challenge", code_challenge);
            q.append_pair("code_challenge_method", "S256");
            q.append_pair("state", state);
        }
        u.to_string()
    }

    /// Active OAuth account `(account_uuid, has_premium)` from
    /// `/api/oauth/profile`. `has_premium` (Claude Max or extra-usage) gates the
    /// `context-1m` / `mid-conversation-system` betas. Falls back to the token's
    /// stored `account_uuid` if the profile call fails; `None` if no token.
    pub async fn current_anthropic_account_meta() -> Option<(String, bool)> {
        let tokens = OAuthTokens::load().await?;
        let token = tokens.access_token.clone();
        let stored_uuid = tokens.account_uuid.clone();

        let fetched = async {
            let cfg = crate::oauth_config::get_oauth_config();
            let url = format!("{}/api/oauth/profile", cfg.base_api_url);
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .ok()?;
            let resp = client
                .get(&url)
                .header("Authorization", format!("Bearer {token}"))
                .header("anthropic-beta", "oauth-2025-04-20")
                .header("content-type", "application/json")
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let v: serde_json::Value = resp.json().await.ok()?;
            let uuid = v["account"]["uuid"].as_str()?.to_string();
            let has_max = v["account"]["has_claude_max"].as_bool().unwrap_or(false);
            let has_extra = v["organization"]["has_extra_usage_enabled"]
                .as_bool()
                .unwrap_or(false);
            Some((uuid, has_max || has_extra))
        }
        .await;

        fetched.or_else(|| stored_uuid.map(|u| (u, false)))
    }
}

// Re-export OAuthTokens at crate root for convenience
pub use oauth::OAuthTokens;

// ---------------------------------------------------------------------------
// New modules: keybindings, voice, analytics, lsp, team_memory_sync,
//              system_prompt, memdir, oauth_config
// ---------------------------------------------------------------------------
pub mod accounts;
pub mod analytics;
pub mod bash_classifier;
pub mod codex_oauth;
pub mod context_collapse;
pub mod effort;
pub mod feature_gates;
pub mod import_config;
pub mod keybindings;
pub mod keywords;
pub mod lsp;
pub mod mcp_trust;
pub mod memdir;
pub mod migrations;
pub mod oauth_config;
pub mod output_styles;
pub mod paths;
pub mod prompt_history;
pub mod ps_classifier;
pub mod remote_settings;
pub mod session_tracing;
pub mod settings_sync;
pub mod system_prompt;
pub mod team_memory_sync;
pub mod tips;
pub mod voice;

// ---------------------------------------------------------------------------
// tasks module — background task registry
// ---------------------------------------------------------------------------
pub mod tasks {
    use chrono::{DateTime, Utc};
    use dashmap::DashMap;
    use once_cell::sync::Lazy;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    /// Current status of a background task.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub enum TaskStatus {
        Running,
        Completed,
        Failed(String),
        Cancelled,
    }

    impl std::fmt::Display for TaskStatus {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                TaskStatus::Running => write!(f, "running"),
                TaskStatus::Completed => write!(f, "completed"),
                TaskStatus::Failed(reason) => write!(f, "failed: {}", reason),
                TaskStatus::Cancelled => write!(f, "cancelled"),
            }
        }
    }

    /// A single background task tracked by the registry.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct BackgroundTask {
        /// Unique identifier for the task.
        pub id: String,
        /// Human-readable name / description.
        pub name: String,
        /// Current execution status.
        pub status: TaskStatus,
        /// When the task was registered.
        pub started_at: DateTime<Utc>,
        /// When the task finished (completed, failed, or cancelled).
        pub completed_at: Option<DateTime<Utc>>,
        /// Lines of output produced by the task.
        pub output: Vec<String>,
        /// OS process ID, if applicable.
        pub pid: Option<u32>,
        /// Cancellation token for the task's in-process work loop. Signalling it
        /// stops the running loop (e.g. a background sub-agent). Not persisted —
        /// it holds no meaningful state across (de)serialization.
        #[serde(skip)]
        pub cancel_token: Option<CancellationToken>,
    }

    impl BackgroundTask {
        /// Create a new running task with the given name.
        pub fn new(name: impl Into<String>) -> Self {
            Self {
                id: Uuid::new_v4().to_string(),
                name: name.into(),
                status: TaskStatus::Running,
                started_at: Utc::now(),
                completed_at: None,
                output: Vec::new(),
                pid: None,
                cancel_token: None,
            }
        }

        /// Return `true` if the task is still running.
        pub fn is_running(&self) -> bool {
            matches!(self.status, TaskStatus::Running)
        }
    }

    /// Thread-safe registry of background tasks.
    pub struct TaskRegistry {
        tasks: Arc<DashMap<String, BackgroundTask>>,
    }

    impl TaskRegistry {
        /// Create a new empty registry.
        pub fn new() -> Self {
            Self {
                tasks: Arc::new(DashMap::new()),
            }
        }

        /// Register a new task.  Returns the assigned task ID.
        pub fn register(&self, task: BackgroundTask) -> String {
            let id = task.id.clone();
            self.tasks.insert(id.clone(), task);
            id
        }

        /// Update the status of a task.  No-op if the ID is unknown.
        pub fn update_status(&self, id: &str, status: TaskStatus) {
            if let Some(mut entry) = self.tasks.get_mut(id) {
                let is_terminal = !matches!(status, TaskStatus::Running);
                entry.status = status;
                if is_terminal && entry.completed_at.is_none() {
                    entry.completed_at = Some(Utc::now());
                }
            }
        }

        /// Append a line of output to an existing task.  No-op if unknown.
        pub fn append_output(&self, id: &str, line: &str) {
            if let Some(mut entry) = self.tasks.get_mut(id) {
                entry.output.push(line.to_string());
            }
        }

        /// Look up a task by ID.
        pub fn get(&self, id: &str) -> Option<BackgroundTask> {
            self.tasks.get(id).map(|e| e.clone())
        }

        /// Return a snapshot of all tasks, ordered by `started_at` ascending.
        pub fn list(&self) -> Vec<BackgroundTask> {
            let mut tasks: Vec<BackgroundTask> =
                self.tasks.iter().map(|e| e.value().clone()).collect();
            tasks.sort_by_key(|t| t.started_at);
            tasks
        }

        /// Mark a task as `Completed`.  No-op if unknown or already terminal.
        pub fn complete(&self, id: &str) {
            self.update_status(id, TaskStatus::Completed);
        }

        /// Attach a cancellation token to a task so it can later be signalled by
        /// [`TaskRegistry::cancel`].  No-op if the ID is unknown.
        pub fn set_cancel_token(&self, id: &str, token: CancellationToken) {
            if let Some(mut entry) = self.tasks.get_mut(id) {
                entry.cancel_token = Some(token);
            }
        }

        /// Mark a task as `Cancelled` and signal its cancellation token (if any)
        /// so the running work loop actually stops.  No-op if unknown or already
        /// terminal.
        pub fn cancel(&self, id: &str) {
            // Clone the token out from under the shard guard, then signal it once
            // the guard has been dropped — never hold a DashMap lock across other
            // registry operations (or any `.await`).
            let token = self.tasks.get(id).and_then(|e| e.cancel_token.clone());
            if let Some(token) = token {
                token.cancel();
            }
            self.update_status(id, TaskStatus::Cancelled);
        }

        /// Set the OS process ID for a task.  No-op if unknown.
        pub fn set_pid(&self, id: &str, pid: u32) {
            if let Some(mut entry) = self.tasks.get_mut(id) {
                entry.pid = Some(pid);
            }
        }
    }

    impl Default for TaskRegistry {
        fn default() -> Self {
            Self::new()
        }
    }

    /// The process-global task registry singleton.
    static GLOBAL_REGISTRY: Lazy<TaskRegistry> = Lazy::new(TaskRegistry::new);

    /// Return a reference to the process-global `TaskRegistry`.
    pub fn global_registry() -> &'static TaskRegistry {
        &GLOBAL_REGISTRY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_user() {
        let msg = Message::user("hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.get_text(), Some("hello"));
    }

    #[test]
    fn test_message_assistant_blocks() {
        let msg = Message::assistant_blocks(vec![
            ContentBlock::Thinking {
                thinking: "let me think".into(),
                signature: "sig".into(),
            },
            ContentBlock::Text {
                text: "response".into(),
            },
        ]);
        assert_eq!(msg.get_text(), Some("response"));
        assert_eq!(msg.get_thinking_blocks().len(), 1);
    }

    #[test]
    fn test_hooks_config_default() {
        let cfg = crate::config::Config::default();
        assert!(cfg.hooks.is_empty());
    }

    /// The bypass-permissions acceptance and always-allow bash prefixes must
    /// round-trip through settings.json so they survive restarts.
    #[test]
    fn settings_persist_bypass_acceptance_and_bash_prefixes() {
        let mut settings = crate::config::Settings::default();
        assert!(!settings.skip_dangerous_mode_permission_prompt);
        assert!(settings.allowed_bash_prefixes.is_empty());

        settings.skip_dangerous_mode_permission_prompt = true;
        settings.allowed_bash_prefixes.push("git".to_string());

        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains("\"skipDangerousModePermissionPrompt\":true"));
        assert!(json.contains("\"allowedBashPrefixes\":[\"git\"]"));

        let restored: crate::config::Settings = serde_json::from_str(&json).unwrap();
        assert!(restored.skip_dangerous_mode_permission_prompt);
        assert_eq!(restored.allowed_bash_prefixes, vec!["git".to_string()]);
    }

    /// Security (issue #123): MCP servers declared in a repository's
    /// `.claurst/settings.json` must be tagged `Project` origin after a
    /// hierarchical load, while the `origin` field is never honored from the
    /// file itself (a repo cannot forge `User`).
    #[tokio::test]
    async fn project_mcp_servers_are_tagged_project_origin() {
        use crate::config::{McpServerConfig, McpServerOrigin, Settings};
        let dir = tempfile::tempdir().unwrap();
        let claurst = dir.path().join(".claurst");
        std::fs::create_dir_all(&claurst).unwrap();

        // Build a full, valid project settings file containing one MCP server.
        // The server is deliberately created with `origin: User` (the value an
        // attacker would want) — but `origin` is `#[serde(skip)]`, so it is
        // neither written to nor read from disk, and the loader re-tags it.
        let mut project = Settings::default();
        project.config.mcp_servers.push(McpServerConfig {
            name: "evil".to_string(),
            command: Some("/bin/sh".to_string()),
            args: vec!["-c".to_string(), "id".to_string()],
            env: std::collections::HashMap::new(),
            url: None,
            server_type: "stdio".to_string(),
            origin: McpServerOrigin::User,
        });
        let json = serde_json::to_string_pretty(&project).unwrap();
        assert!(
            !json.contains("origin"),
            "origin must never be serialized to the settings file"
        );
        std::fs::write(claurst.join("settings.json"), json).unwrap();

        let merged = Settings::load_hierarchical(dir.path()).await.unwrap();
        let server = merged
            .config
            .mcp_servers
            .iter()
            .find(|s| s.name == "evil")
            .expect("project server should be present after hierarchical load");
        assert_eq!(
            server.origin,
            McpServerOrigin::Project,
            "project-defined server must be tagged Project origin and cannot forge User"
        );
    }

    #[test]
    fn test_cost_tracker() {
        let tracker = CostTracker::new();
        tracker.add_usage(1000, 500, 200, 100);
        assert_eq!(tracker.input_tokens(), 1000);
        assert_eq!(tracker.output_tokens(), 500);
        assert!(tracker.total_cost_usd() > 0.0);
    }

    #[test]
    fn test_error_retryable() {
        assert!(ClaudeError::RateLimit.is_retryable());
        assert!(ClaudeError::ApiStatus {
            status: 429,
            message: "rate limited".into()
        }
        .is_retryable());
        assert!(!ClaudeError::Auth("bad key".into()).is_retryable());
    }

    // ---- Config tests -------------------------------------------------------

    #[test]
    fn test_config_mouse_capture_defaults_on() {
        // Unset (None) must read as enabled to preserve historical behaviour.
        let cfg = crate::config::Config::default();
        assert_eq!(cfg.mouse_capture, None);
        assert!(cfg.mouse_capture_enabled());
    }

    #[test]
    fn test_config_mouse_capture_explicit_off() {
        let mut cfg = crate::config::Config {
            mouse_capture: Some(false),
            ..Default::default()
        };
        assert!(!cfg.mouse_capture_enabled());
        cfg.mouse_capture = Some(true);
        assert!(cfg.mouse_capture_enabled());
    }

    #[test]
    fn test_config_mouse_capture_serde_roundtrip() {
        // Unset round-trips as None and is omitted from the serialized JSON
        // (skip_serializing_if), so existing settings files stay unchanged.
        let cfg = crate::config::Config::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("mouseCapture"));
        let back: crate::config::Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mouse_capture, None);
        assert!(back.mouse_capture_enabled());

        // Explicit off serializes the key and round-trips as disabled.
        let cfg = crate::config::Config {
            mouse_capture: Some(false),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"mouseCapture\":false"));
        let back: crate::config::Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mouse_capture, Some(false));
        assert!(!back.mouse_capture_enabled());
    }

    #[test]
    fn test_config_effective_model_default() {
        let cfg = crate::config::Config::default();
        assert_eq!(cfg.effective_model(), crate::constants::DEFAULT_MODEL);
    }

    #[test]
    fn test_config_effective_model_override() {
        let cfg = crate::config::Config {
            model: Some("claude-haiku-4-5-20251001".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.effective_model(), "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_config_effective_max_tokens_default() {
        let cfg = crate::config::Config::default();
        assert_eq!(
            cfg.effective_max_tokens(),
            crate::constants::DEFAULT_MAX_TOKENS
        );
    }

    #[test]
    fn test_config_effective_max_tokens_override() {
        let cfg = crate::config::Config {
            max_tokens: Some(8192),
            ..Default::default()
        };
        assert_eq!(cfg.effective_max_tokens(), 8192);
    }

    #[test]
    fn test_config_resolve_api_key_from_config() {
        // When config.api_key is set, it should be returned regardless of env var
        // (Config key takes priority — resolve_api_key returns it first)
        let orig = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");

        let cfg = crate::config::Config {
            api_key: Some("sk-ant-config-key".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_api_key(), Some("sk-ant-config-key".to_string()));

        if let Some(k) = orig {
            std::env::set_var("ANTHROPIC_API_KEY", k);
        }
    }

    #[test]
    fn test_config_resolve_api_key_none() {
        // Temporarily ensure no env var override
        let orig = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");

        let cfg = crate::config::Config::default();
        assert!(cfg.resolve_api_key().is_none());

        // Restore
        if let Some(k) = orig {
            std::env::set_var("ANTHROPIC_API_KEY", k);
        }
    }

    #[test]
    fn test_config_resolve_api_key_from_env() {
        let orig = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-env-key");

        let cfg = crate::config::Config::default();
        assert_eq!(cfg.resolve_api_key(), Some("sk-ant-env-key".to_string()));

        // Restore
        std::env::remove_var("ANTHROPIC_API_KEY");
        if let Some(k) = orig {
            std::env::set_var("ANTHROPIC_API_KEY", k);
        }
    }

    // ---- OAuth token tests --------------------------------------------------

    #[test]
    fn test_oauth_tokens_not_expired_no_expiry() {
        let tokens = crate::oauth::OAuthTokens {
            access_token: "at".to_string(),
            expires_at_ms: None,
            ..Default::default()
        };
        assert!(
            !tokens.is_expired(),
            "Token with no expiry should not be considered expired"
        );
    }

    #[test]
    fn test_oauth_tokens_expired_past() {
        let tokens = crate::oauth::OAuthTokens {
            access_token: "at".to_string(),
            // Expired 1 hour ago
            expires_at_ms: Some(chrono::Utc::now().timestamp_millis() - 3_600_000),
            ..Default::default()
        };
        assert!(tokens.is_expired());
    }

    #[test]
    fn test_oauth_tokens_not_expired_future() {
        let tokens = crate::oauth::OAuthTokens {
            access_token: "at".to_string(),
            // Expires in 1 hour
            expires_at_ms: Some(chrono::Utc::now().timestamp_millis() + 3_600_000),
            ..Default::default()
        };
        assert!(!tokens.is_expired());
    }

    #[test]
    fn test_oauth_tokens_expired_within_buffer() {
        let tokens = crate::oauth::OAuthTokens {
            access_token: "at".to_string(),
            // Expires in 3 minutes — within the 5-minute buffer, so treated as expired
            expires_at_ms: Some(chrono::Utc::now().timestamp_millis() + 3 * 60 * 1000),
            ..Default::default()
        };
        assert!(
            tokens.is_expired(),
            "Token within 5-min buffer should be considered expired"
        );
    }

    #[test]
    fn test_oauth_uses_bearer_auth_with_inference_scope() {
        let tokens = crate::oauth::OAuthTokens {
            scopes: vec![crate::oauth::CLAUDE_AI_INFERENCE_SCOPE.to_string()],
            ..Default::default()
        };
        assert!(tokens.uses_bearer_auth());
    }

    #[test]
    fn test_oauth_uses_bearer_auth_without_inference_scope() {
        let tokens = crate::oauth::OAuthTokens {
            scopes: vec!["org:create_api_key".to_string()],
            ..Default::default()
        };
        assert!(!tokens.uses_bearer_auth());
    }

    #[test]
    fn test_oauth_effective_credential_bearer() {
        let tokens = crate::oauth::OAuthTokens {
            access_token: "bearer_token_xyz".to_string(),
            scopes: vec![crate::oauth::CLAUDE_AI_INFERENCE_SCOPE.to_string()],
            api_key: Some("sk-ant-ignored".to_string()),
            ..Default::default()
        };
        assert_eq!(tokens.effective_credential(), Some("bearer_token_xyz"));
    }

    #[test]
    fn test_oauth_effective_credential_api_key() {
        let tokens = crate::oauth::OAuthTokens {
            access_token: "at".to_string(),
            scopes: vec!["org:create_api_key".to_string()],
            api_key: Some("sk-ant-real-key".to_string()),
            ..Default::default()
        };
        assert_eq!(tokens.effective_credential(), Some("sk-ant-real-key"));
    }

    #[test]
    fn test_oauth_effective_credential_bearer_empty_access_token() {
        let tokens = crate::oauth::OAuthTokens {
            access_token: String::new(),
            scopes: vec![crate::oauth::CLAUDE_AI_INFERENCE_SCOPE.to_string()],
            ..Default::default()
        };
        assert_eq!(tokens.effective_credential(), None);
    }

    #[test]
    fn test_oauth_effective_credential_no_api_key() {
        let tokens = crate::oauth::OAuthTokens {
            access_token: "at".to_string(),
            scopes: vec!["org:create_api_key".to_string()],
            api_key: None,
            ..Default::default()
        };
        assert_eq!(tokens.effective_credential(), None);
    }

    // ---- PKCE tests ---------------------------------------------------------

    #[test]
    fn test_pkce_code_verifier_length() {
        let verifier = crate::oauth::generate_code_verifier();
        // 32 bytes base64url-encoded (no padding) = ceil(32 * 4/3) = 43 chars
        assert_eq!(
            verifier.len(),
            43,
            "Code verifier should be 43 base64url chars (32 bytes)"
        );
        // Must only contain URL-safe base64 chars
        assert!(verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn test_pkce_code_challenge_format() {
        let verifier = crate::oauth::generate_code_verifier();
        let challenge = crate::oauth::generate_code_challenge(&verifier);
        // SHA256 = 32 bytes → 43 base64url chars
        assert_eq!(
            challenge.len(),
            43,
            "Code challenge should be 43 base64url chars (SHA256 = 32 bytes)"
        );
        assert!(challenge
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn test_pkce_challenge_deterministic() {
        // Same verifier must produce same challenge
        let verifier = "test_verifier_fixed_input";
        let c1 = crate::oauth::generate_code_challenge(verifier);
        let c2 = crate::oauth::generate_code_challenge(verifier);
        assert_eq!(c1, c2);
    }

    #[test]
    fn test_pkce_verifier_unique() {
        let v1 = crate::oauth::generate_code_verifier();
        let v2 = crate::oauth::generate_code_verifier();
        assert_ne!(v1, v2, "Code verifiers should be unique");
    }

    #[test]
    fn test_pkce_state_length_and_format() {
        let state = crate::oauth::generate_state();
        assert_eq!(state.len(), 43);
        assert!(state
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    // ---- Auth URL building tests --------------------------------------------

    #[test]
    fn test_build_auth_url_automatic_has_localhost_redirect() {
        let challenge = "test_challenge";
        let state = "test_state";
        let port: u16 = 12345;
        let url = crate::oauth::build_auth_url(
            crate::oauth::CONSOLE_AUTHORIZE_URL,
            challenge,
            state,
            port,
            false, // automatic
        );
        assert!(url.contains("redirect_uri="), "URL must have redirect_uri");
        assert!(
            url.contains("localhost%3A12345") || url.contains("localhost:12345"),
            "Automatic URL should use localhost callback"
        );
        assert!(url.contains("code_challenge=test_challenge"));
        assert!(url.contains("state=test_state"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("client_id={}", crate::oauth::CLIENT_ID)));
    }

    #[test]
    fn test_build_auth_url_manual_has_manual_redirect() {
        let url = crate::oauth::build_auth_url(
            crate::oauth::CLAUDE_AI_AUTHORIZE_URL,
            "challenge",
            "state",
            9999,
            true, // manual
        );
        assert!(url.contains("redirect_uri="), "URL must have redirect_uri");
        // Manual redirect should NOT be localhost
        assert!(
            !url.contains("localhost"),
            "Manual URL should not use localhost callback"
        );
    }

    // ---- Permission handler tests -------------------------------------------

    fn make_req(tool_name: &str, is_read_only: bool) -> crate::permissions::PermissionRequest {
        crate::permissions::PermissionRequest {
            tool_name: tool_name.to_string(),
            description: format!("{} operation", tool_name),
            details: None,
            is_read_only,
            path: None,
            working_dir: None,
            allowed_roots: Vec::new(),
            context_description: None,
        }
    }

    #[test]
    fn test_auto_handler_bypass_allows_all() {
        let handler = crate::permissions::AutoPermissionHandler {
            mode: crate::config::PermissionMode::BypassPermissions,
        };
        assert_eq!(
            handler.check_permission(&make_req("FileWrite", false)),
            crate::permissions::PermissionDecision::Allow
        );
    }

    #[test]
    fn test_auto_handler_default_allows_reads() {
        let handler = crate::permissions::AutoPermissionHandler {
            mode: crate::config::PermissionMode::Default,
        };
        assert_eq!(
            handler.check_permission(&make_req("FileRead", true)),
            crate::permissions::PermissionDecision::Allow
        );
    }

    #[test]
    fn test_auto_handler_default_denies_writes() {
        let handler = crate::permissions::AutoPermissionHandler {
            mode: crate::config::PermissionMode::Default,
        };
        assert_eq!(
            handler.check_permission(&make_req("FileWrite", false)),
            crate::permissions::PermissionDecision::Deny
        );
    }

    #[test]
    fn test_auto_handler_accept_edits_only_allows_edit() {
        let handler = crate::permissions::AutoPermissionHandler {
            mode: crate::config::PermissionMode::AcceptEdits,
        };
        assert_eq!(
            handler.check_permission(&make_req("Edit", false)),
            crate::permissions::PermissionDecision::Allow
        );
        assert_eq!(
            handler.check_permission(&make_req("FileWrite", false)),
            crate::permissions::PermissionDecision::Deny
        );
    }

    #[test]
    fn test_interactive_handler_default_allows_writes() {
        // Legacy InteractivePermissionHandler still allows everything outside Plan.
        let handler = crate::permissions::InteractivePermissionHandler {
            mode: crate::config::PermissionMode::Default,
        };
        assert_eq!(
            handler.check_permission(&make_req("FileWrite", false)),
            crate::permissions::PermissionDecision::Allow
        );
    }

    #[test]
    fn test_managed_interactive_default_asks_for_write() {
        let manager = std::sync::Arc::new(std::sync::Mutex::new(
            crate::permissions::PermissionManager::new(
                crate::config::PermissionMode::Default,
                &crate::config::Settings::default(),
            ),
        ));
        let handler = crate::permissions::InteractivePermissionHandler::with_manager(manager);
        match handler.check_permission(&make_req("FileWrite", false)) {
            crate::permissions::PermissionDecision::Ask { .. } => {}
            other => panic!("Expected Ask, got {:?}", other),
        }
    }

    #[test]
    fn test_managed_interactive_default_allows_workspace_read() {
        let manager = std::sync::Arc::new(std::sync::Mutex::new(
            crate::permissions::PermissionManager::new(
                crate::config::PermissionMode::Default,
                &crate::config::Settings::default(),
            ),
        ));
        let handler = crate::permissions::InteractivePermissionHandler::with_manager(manager);
        let mut req = make_req("Read", true);
        req.path = Some("/workspace/src/lib.rs".to_string());
        req.working_dir = Some(std::path::PathBuf::from("/workspace"));
        assert_eq!(
            handler.check_permission(&req),
            crate::permissions::PermissionDecision::Allow
        );
    }

    // ---- Message content tests ----------------------------------------------

    #[test]
    fn test_message_get_all_text_multiple_blocks() {
        let msg = Message::assistant_blocks(vec![
            ContentBlock::Text {
                text: "First ".into(),
            },
            ContentBlock::Text {
                text: "Second".into(),
            },
        ]);
        assert_eq!(msg.get_all_text(), "First Second");
    }

    #[test]
    fn test_message_get_text_returns_first_text_block() {
        let msg = Message::assistant_blocks(vec![
            ContentBlock::Thinking {
                thinking: "reasoning".into(),
                signature: "sig".into(),
            },
            ContentBlock::Text {
                text: "answer".into(),
            },
        ]);
        assert_eq!(msg.get_text(), Some("answer"));
    }

    #[test]
    fn test_message_has_tool_use_false() {
        let msg = Message::user("just text");
        assert!(!msg.has_tool_use());
    }

    #[test]
    fn test_cost_tracker_cumulative() {
        let tracker = CostTracker::new();
        tracker.add_usage(1000, 500, 100, 50);
        tracker.add_usage(200, 100, 0, 0);
        assert_eq!(tracker.input_tokens(), 1200);
        assert_eq!(tracker.output_tokens(), 600);
    }

    #[test]
    fn test_cost_tracker_initial_zero() {
        let tracker = CostTracker::new();
        assert_eq!(tracker.input_tokens(), 0);
        assert_eq!(tracker.output_tokens(), 0);
        assert_eq!(tracker.total_cost_usd(), 0.0);
    }

    #[test]
    fn test_cost_tracker_free_model() {
        let tracker = CostTracker::with_model("deepseek-v4-flash-free");
        tracker.add_usage(1000, 500, 200, 100);
        // Free models should have zero cost even with token usage
        assert_eq!(tracker.total_cost_usd(), 0.0);
    }

    #[test]
    fn test_model_pricing_free_variants() {
        // Test that models ending with -free use FREE pricing
        assert_eq!(
            cost::ModelPricing::for_model("deepseek-v4-flash-free"),
            cost::ModelPricing::FREE
        );
        assert_eq!(
            cost::ModelPricing::for_model("zen/minimax-m2.5-free"),
            cost::ModelPricing::FREE
        );

        // Test that models starting with free/ use FREE pricing
        assert_eq!(
            cost::ModelPricing::for_model("free/auto"),
            cost::ModelPricing::FREE
        );
        assert_eq!(
            cost::ModelPricing::for_model("free/some-model"),
            cost::ModelPricing::FREE
        );

        // Test that upstream-prefixed free models use FREE pricing
        assert_eq!(
            cost::ModelPricing::for_model("groq/llama-3.3-70b-versatile"),
            cost::ModelPricing::FREE
        );
        assert_eq!(
            cost::ModelPricing::for_model("cerebras/qwen-3-235b-a22b-instruct-2507"),
            cost::ModelPricing::FREE
        );
        assert_eq!(
            cost::ModelPricing::for_model("google/gemini-2.5-flash"),
            cost::ModelPricing::FREE
        );
        assert_eq!(
            cost::ModelPricing::for_model("mistral/mistral-large-latest"),
            cost::ModelPricing::FREE
        );
        assert_eq!(
            cost::ModelPricing::for_model("sambanova/Meta-Llama-3.3-70B-Instruct"),
            cost::ModelPricing::FREE
        );
        assert_eq!(
            cost::ModelPricing::for_model("nvidia/meta/llama-3.3-70b-instruct"),
            cost::ModelPricing::FREE
        );
        assert_eq!(
            cost::ModelPricing::for_model("cohere/command-r-plus"),
            cost::ModelPricing::FREE
        );
        assert_eq!(
            cost::ModelPricing::for_model("openrouter/free"),
            cost::ModelPricing::FREE
        );
        assert_eq!(
            cost::ModelPricing::for_model("opencode-zen/minimax-m2.5-free"),
            cost::ModelPricing::FREE
        );
        assert_eq!(
            cost::ModelPricing::for_model("zai/glm-4.6"),
            cost::ModelPricing::FREE
        );
        assert_eq!(
            cost::ModelPricing::for_model("zhipuai/glm-4.5"),
            cost::ModelPricing::FREE
        );

        // Test that other models use their appropriate pricing
        assert_eq!(
            cost::ModelPricing::for_model("claude-opus"),
            cost::ModelPricing::OPUS
        );
        assert_eq!(
            cost::ModelPricing::for_model("claude-haiku"),
            cost::ModelPricing::HAIKU
        );
        assert_eq!(
            cost::ModelPricing::for_model("claude-sonnet"),
            cost::ModelPricing::SONNET
        );
    }

    #[test]
    fn managed_agent_config_serde_round_trip() {
        let cfg = ManagedAgentConfig {
            enabled: true,
            manager_model: "anthropic/claude-opus-4-6".to_string(),
            executor_model: "anthropic/claude-sonnet-4-6".to_string(),
            executor_max_turns: 10,
            max_concurrent_executors: 4,
            budget_split: BudgetSplitPolicy::Percentage { manager_pct: 30 },
            total_budget_usd: Some(5.0),
            preset_name: Some("anthropic-tiered".to_string()),
            executor_isolation: false,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let decoded: ManagedAgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.manager_model, "anthropic/claude-opus-4-6");
        assert_eq!(decoded.executor_max_turns, 10);
    }

    #[test]
    fn budget_split_policy_defaults_to_shared_pool() {
        let json = r#"{"enabled":true,"manager_model":"a/b","executor_model":"a/c"}"#;
        let cfg: ManagedAgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.budget_split, BudgetSplitPolicy::SharedPool);
        assert_eq!(cfg.executor_max_turns, 10);
    }

    #[test]
    fn builtin_presets_all_have_valid_model_format() {
        for preset in builtin_managed_agent_presets() {
            assert!(
                preset.manager_model.contains('/'),
                "Preset {} manager_model must be provider/model",
                preset.name
            );
            assert!(
                preset.executor_model.contains('/'),
                "Preset {} executor_model must be provider/model",
                preset.name
            );
        }
    }

    // ---- Background task cancellation (issue #219) --------------------------

    /// Cancelling a task must signal the cancellation token attached to it, not
    /// merely relabel its status. Without this, a "cancelled" background agent
    /// keeps running and editing files.
    #[test]
    fn registry_cancel_signals_attached_token() {
        use tokio_util::sync::CancellationToken;

        let registry = tasks::TaskRegistry::new();
        let id = registry.register(tasks::BackgroundTask::new("cancellable task"));

        let token = CancellationToken::new();
        registry.set_cancel_token(&id, token.clone());
        assert!(!token.is_cancelled());

        registry.cancel(&id);

        assert!(
            token.is_cancelled(),
            "cancel() must signal the attached cancellation token"
        );
        assert_eq!(
            registry.get(&id).unwrap().status,
            tasks::TaskStatus::Cancelled
        );
    }

    /// A running work loop that holds the registered token (as the background
    /// sub-agent's `run_query_loop` does) must actually stop when the task is
    /// cancelled through the registry.
    #[tokio::test]
    async fn spawned_loop_observes_registry_cancellation() {
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        let registry = tasks::TaskRegistry::new();
        let mut task = tasks::BackgroundTask::new("bg loop");
        let id = task.id.clone();
        let token = CancellationToken::new();
        // Attach at registration, exactly as the background spawn does.
        task.cancel_token = Some(token.clone());
        registry.register(task);

        // Stand-in for run_query_loop: keep "working" until the shared token is
        // signalled, mirroring the real loop's between-turn cancellation check.
        let loop_token = token.clone();
        let handle = tokio::spawn(async move {
            loop {
                if loop_token.is_cancelled() {
                    return "cancelled";
                }
                tokio::select! {
                    _ = loop_token.cancelled() => return "cancelled",
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            }
        });

        // Let the loop start spinning, then cancel via the registry.
        tokio::time::sleep(Duration::from_millis(10)).await;
        registry.cancel(&id);

        let reason = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("loop must stop promptly after cancellation")
            .expect("loop task must not panic");

        assert_eq!(reason, "cancelled");
        assert!(token.is_cancelled());
        assert_eq!(
            registry.get(&id).unwrap().status,
            tasks::TaskStatus::Cancelled
        );
    }

    #[test]
    fn custom_provider_def_deserialize_camel_case() {
        let json = r#"{
            "name": "Test Gateway",
            "apiBase": "https://gateway.example.com/v1",
            "apiKey": "secret-key",
            "headers": { "X-Custom-Header": "value" },
            "models": {
                "model-1": {
                    "name": "Model One",
                    "contextWindow": 128000,
                    "maxOutputTokens": 8192,
                    "reasoningEffort": "high"
                }
            },
            "requestTimeoutSecs": 300
        }"#;
        let def: CustomProviderDef = serde_json::from_str(json).unwrap();
        assert_eq!(def.name, "Test Gateway");
        assert_eq!(def.api_base, "https://gateway.example.com/v1");
        assert_eq!(def.api_key.as_deref(), Some("secret-key"));
        assert_eq!(def.headers.get("X-Custom-Header").unwrap(), "value");
        assert_eq!(def.models.len(), 1);
        let model = def.models.get("model-1").unwrap();
        assert_eq!(model.context_window, Some(128000));
        assert_eq!(model.max_output_tokens, Some(8192));
        assert_eq!(model.name.as_deref(), Some("Model One"));
        assert_eq!(model.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(def.request_timeout_secs, Some(300));
    }

    #[test]
    fn custom_provider_def_deserialize_snake_case() {
        let json = r#"{
            "name": "Test",
            "api_base": "https://example.com",
            "api_key": "key",
            "models": {}
        }"#;
        let def: CustomProviderDef = serde_json::from_str(json).unwrap();
        assert_eq!(def.api_base, "https://example.com");
        assert_eq!(def.api_key.as_deref(), Some("key"));
    }

    #[test]
    fn custom_provider_def_resolve_api_key_env_substitution() {
        std::env::set_var("TEST_CUSTOM_PROVIDER_KEY", "env-key-value");
        let def = CustomProviderDef {
            api_key: Some("{env:TEST_CUSTOM_PROVIDER_KEY}".to_string()),
            ..Default::default()
        };
        assert_eq!(def.resolve_api_key().as_deref(), Some("env-key-value"));
        std::env::remove_var("TEST_CUSTOM_PROVIDER_KEY");
    }

    #[test]
    fn custom_provider_def_resolve_api_key_empty_env() {
        let def = CustomProviderDef {
            api_key: Some("{env:NONEXISTENT_VAR_12345}".to_string()),
            ..Default::default()
        };
        assert!(def.resolve_api_key().is_none());
    }

    #[test]
    fn custom_provider_def_resolve_api_key_none() {
        let def = CustomProviderDef {
            api_key: None,
            ..Default::default()
        };
        assert!(def.resolve_api_key().is_none());
    }

    #[test]
    fn custom_provider_def_resolve_api_key_plain() {
        let def = CustomProviderDef {
            api_key: Some("plain-key".to_string()),
            ..Default::default()
        };
        assert_eq!(def.resolve_api_key().as_deref(), Some("plain-key"));
    }

    #[test]
    fn custom_provider_def_with_streaming_settings() {
        let json = r#"{
            "name": "Test",
            "apiBase": "https://example.com/v1",
            "streaming": false,
            "includeUsageInStream": false,
            "reasoningField": "reasoning_content"
        }"#;
        let def: CustomProviderDef = serde_json::from_str(json).unwrap();
        assert_eq!(def.streaming, Some(false));
        assert_eq!(def.include_usage_in_stream, Some(false));
        assert_eq!(def.reasoning_field.as_deref(), Some("reasoning_content"));
    }

    #[test]
    fn custom_model_def_with_variants() {
        let json = r#"{
            "contextWindow": 230000,
            "maxOutputTokens": 32072,
            "variants": {
                "max": { "reasoningEffort": "max" },
                "none": { "reasoningEffort": "none" }
            }
        }"#;
        let model: CustomModelDef = serde_json::from_str(json).unwrap();
        assert_eq!(model.context_window, Some(230000));
        assert_eq!(model.variants.len(), 2);
        assert_eq!(
            model
                .variants
                .get("max")
                .unwrap()
                .reasoning_effort
                .as_deref(),
            Some("max")
        );
    }

    #[test]
    fn settings_deserialize_with_custom_providers() {
        let json = r#"{
            "customProviders": {
                "my-gateway": {
                    "name": "My Gateway",
                    "apiBase": "https://gw.example.com/v1",
                    "models": {
                        "model-a": { "name": "Model A" }
                    }
                }
            }
        }"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.custom_providers.len(), 1);
        let provider = settings.custom_providers.get("my-gateway").unwrap();
        assert_eq!(provider.name, "My Gateway");
        assert_eq!(provider.models.len(), 1);
    }

    #[test]
    fn auto_poke_enabled_default_true() {
        let json = r#"{}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert!(settings.auto_poke_enabled);
    }
}
