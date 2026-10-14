// cc-query: The core agentic query loop.
//
// This crate implements the main conversation loop that:
// 1. Sends messages to the Anthropic API
// 2. Processes streaming responses
// 3. Detects tool-use requests and dispatches them
// 4. Feeds tool results back to the model
// 5. Handles auto-compact when the context window fills up
// 6. Manages stop conditions (end_turn, max_turns, cancellation)

// too_many_arguments: `run_query_loop` and related orchestration entrypoints
// thread many parameters by design; splitting would obscure the control flow.
#![allow(clippy::too_many_arguments)]

pub mod agent_tool;
pub mod auto_dream;
pub mod away_summary;
pub mod command_queue;
pub mod continuation;
pub mod goal_loop;
pub mod managed_orchestrator;
pub mod compact;
pub mod context_analyzer;
pub mod coordinator;
pub mod cron_scheduler;
pub mod sanitize;
pub mod session_memory;

mod runner;
pub use runner::*;
pub use agent_tool::{AgentTool, init_team_swarm_runner};
pub use command_queue::{CommandPriority, CommandQueue, QueuedCommand, drain_command_queue};
pub use continuation::{
    ContinuationDecision, ContinuationMode, ContinuationPolicy, StopPolicy, TurnEndContext,
};
pub use cron_scheduler::start_cron_scheduler;
pub use goal_loop::{
    GoalContinuation, StopReason, check_and_continue_goal, decide_goal_continuation,
    mark_goal_complete,
};
pub use sanitize::sanitize_history;
pub use compact::{
    AutoCompactState, CompactResult, CompactTrigger, MicroCompactConfig, MessageGroup, TokenWarningState,
    auto_compact_if_needed, calculate_messages_to_keep_index, calculate_token_warning_state,
    calculate_token_warning_state_for_window, compact_conversation, context_collapse,
    context_window_for_model, estimate_context_tokens, format_compact_summary, get_compact_prompt,
    group_messages_for_compact, micro_compact_if_needed, reactive_compact,
    resolve_context_window, should_auto_compact, should_auto_compact_for_window, should_compact,
    should_context_collapse, snip_compact, auto_compact_threshold, summary_token_budget, Compaction,
    CompactionRecord, CompactionTrigger, UsageMark,
};
pub use session_memory::{
    ExtractedMemory, MemoryCategory, SessionMemoryExtractor, SessionMemoryState,
};

use claurst_api::{
    ApiMessage, ApiToolDefinition, AnthropicStreamEvent, CreateMessageRequest, StreamAccumulator,
    StreamHandler, SystemBlock, SystemPrompt,
};
use claurst_core::config::Config;
use claurst_core::cost::CostTracker;
use claurst_core::error::ClaudeError;
use claurst_core::types::{ContentBlock, Message, Role, ToolResultContent, UsageInfo};
use claurst_tools::{PermissionLevel, Tool, ToolContext, ToolResult};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Outcome of a single query-loop run.
#[derive(Debug)]
pub enum QueryOutcome {
    /// The model finished its turn (end_turn stop reason).
    EndTurn { message: Message, usage: UsageInfo },
    /// The model hit max_tokens.
    MaxTokens { partial_message: Message, usage: UsageInfo },
    /// The conversation was cancelled by the user.
    Cancelled,
    /// An unrecoverable error occurred.
    Error(ClaudeError),
    /// The configured USD budget was exceeded.
    BudgetExceeded { cost_usd: f64, limit_usd: f64 },
}

/// Configuration for a single query-loop invocation.
#[derive(Clone)]
pub struct QueryConfig {
    pub model: String,
    pub max_tokens: u32,
    pub max_turns: u32,
    /// Session-local turn-cap override (e.g. from `/turns`). When set it
    /// takes precedence over BOTH the agent definition and `max_turns`.
    /// Always a finite cap; `/turns off` maps to a high ceiling, not to
    /// "unlimited".
    pub max_turns_override: Option<u32>,
    /// Whether the max-steps graceful degradation summary turn is enabled.
    /// When `false`, exceeding `max_turns` returns cold (last assistant
    /// message) instead of running a final tool-less summary turn.
    pub degradation_enabled: bool,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    /// Session context gathered at startup (git status, IDE context).
    /// Sent in the dynamic system block, after the cached stable block.
    pub session_context: Option<String>,
    pub output_style: claurst_core::system_prompt::OutputStyle,
    pub output_style_prompt: Option<String>,
    pub working_directory: Option<String>,
    /// Skills listed to the model in the cached system block (name and
    /// description), discovered once per session. Empty: no listing.
    pub available_skills: Vec<claurst_core::system_prompt::SkillListing>,
    /// AGENTS.md / CLAUDE.md instruction files loaded at session start, sent
    /// in the cached system block. Empty: none.
    pub instruction_files: Vec<claurst_core::claudemd::InstructionFile>,
    pub thinking_budget: Option<u32>,
    pub temperature: Option<f32>,
    /// Per-item cap, in bytes, applied to each tool result when it is
    /// appended to the history: longer output keeps its head and tail with a
    /// note giving the omitted size (issue #441). History is never rewritten
    /// afterwards, so the prompt prefix stays stable. `0` disables the cap.
    /// Default: [`DEFAULT_TOOL_RESULT_MAX_BYTES`] (~16k tokens).
    pub tool_result_max_bytes: usize,
    /// Optional effort level.  When set and `thinking_budget` is `None`,
    /// the effort level's `thinking_budget_tokens()` is used as the
    /// thinking budget.  Also provides a temperature override when the
    /// level specifies one.
    pub effort_level: Option<claurst_core::effort::EffortLevel>,
    /// T1-4: Optional shared command queue.
    ///
    /// When set, the query loop drains this queue before each API call and
    /// injects any resulting messages into the conversation.  The queue is
    /// shared (Arc-backed) so the TUI input thread can push commands while the
    /// loop is waiting for a model response.
    pub command_queue: Option<CommandQueue>,
    /// Optional USD spend cap. The query loop checks accumulated cost after
    /// each turn and aborts with `QueryOutcome::BudgetExceeded` when exceeded.
    pub max_budget_usd: Option<f64>,
    /// Fallback model name. Used when the primary model returns overloaded /
    /// rate-limit errors (mirrors TS `--fallback-model`).
    pub fallback_model: Option<String>,
    /// Optional ProviderRegistry for dispatching to non-Anthropic providers.
    /// When `config.provider` is set to something other than "anthropic" and
    /// this registry contains that provider, the registry's provider is used
    /// instead of `AnthropicClient`.
    pub provider_registry: Option<std::sync::Arc<claurst_api::ProviderRegistry>>,
    /// Active agent name (e.g., "build", "plan", "explore", or None for default).
    pub agent_name: Option<String>,
    /// Resolved agent definition for the current session.
    pub agent_definition: Option<claurst_core::AgentDefinition>,
    /// Optional shared model registry for dynamic provider and model resolution.
    /// When set, the query loop uses this instead of constructing a fresh registry.
    pub model_registry: Option<std::sync::Arc<claurst_api::ModelRegistry>>,
    /// Managed agent (manager-executor) configuration.
    pub managed_agents: Option<claurst_core::ManagedAgentConfig>,
    /// Names of the tools enabled for this session (issue #233).
    ///
    /// When populated, `build_system_prompt` forwards these to
    /// `SystemPromptOptions::enabled_tools` so the "Tool use guidelines"
    /// section only emits per-tool guidance for tools that are actually
    /// loaded. `None`/empty means "unknown" and every block is emitted,
    /// which keeps existing behaviour for callers that don't set it.
    ///
    // Populated in-loop (issue #233 completion): when left `None`,
    // `run_query_loop` fills this from its live `tools: &[Box<dyn Tool>]`
    // argument before assembling the system prompt, so the top-level
    // interactive session gets progressive tool disclosure. Callers that build
    // both the tool vec and the config (e.g. sub-agents) may still set it
    // explicitly; the loop only fills an unset field.
    pub enabled_tools: Option<Vec<String>>,
    /// Current permission mode, described in the dynamic system block. When
    /// `None`, `run_query_loop` fills it from the live `ToolContext` config
    /// each turn, so a mode change (/yolo, Shift+Tab) reaches the next request.
    pub permission_mode: Option<claurst_core::config::PermissionMode>,
    /// Extra workspace roots (`/add-dir`) for `<environment_context>`. When
    /// empty, `run_query_loop` fills it from the live `ToolContext` config.
    pub additional_dirs: Vec<String>,
    /// End-of-turn continuation policy (issue #230 / MI-3).
    ///
    /// `Default` stops after one turn (normal, non-goal behaviour). Goal-driven
    /// autonomy selects `Goal`, which keeps the loop running while an active
    /// goal's guards allow, injecting the goal continuation message as the next
    /// user turn — instead of the CLI REPL re-dispatching a fresh turn.
    pub continuation: crate::continuation::ContinuationMode,
    /// Retry policy for transient model-request failures (#439): HTTP
    /// 408/409/425/429/5xx, dropped connections, stalled or truncated streams.
    pub retry: claurst_api::RetryConfig,
    /// Automatic compaction (issue #441): the pre-request threshold check and
    /// compact-and-retry on a provider context-window error. `false` (from
    /// `--no-auto-compact`) leaves the history alone and surfaces overflow
    /// errors; `/compact` still works.
    pub auto_compact: bool,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            model: claurst_core::constants::DEFAULT_MODEL.to_string(),
            max_tokens: claurst_core::constants::DEFAULT_MAX_TOKENS,
            max_turns: claurst_core::constants::MAX_TURNS_DEFAULT,
            max_turns_override: None,
            degradation_enabled: true,
            system_prompt: None,
            append_system_prompt: None,
            session_context: None,
            output_style: claurst_core::system_prompt::OutputStyle::Default,
            output_style_prompt: None,
            working_directory: None,
            available_skills: Vec::new(),
            instruction_files: Vec::new(),
            thinking_budget: None,
            temperature: None,
            tool_result_max_bytes: DEFAULT_TOOL_RESULT_MAX_BYTES,
            effort_level: None,
            command_queue: None,
            max_budget_usd: None,
            fallback_model: None,
            provider_registry: None,
            agent_name: None,
            agent_definition: None,
            model_registry: None,
            managed_agents: None,
            enabled_tools: None,
            permission_mode: None,
            additional_dirs: Vec::new(),
            continuation: crate::continuation::ContinuationMode::Default,
            retry: claurst_api::RetryConfig::default(),
            auto_compact: true,
        }
    }
}

impl QueryConfig {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            model: cfg.effective_model().to_string(),
            max_tokens: cfg.effective_max_tokens(),
            output_style: cfg.effective_output_style(),
            output_style_prompt: cfg.resolve_output_style_prompt(),
            working_directory: cfg
                .project_dir
                .as_ref()
                .map(|p| p.display().to_string()),
            managed_agents: cfg.managed_agents.clone(),
            effort_level: cfg.effective_effort_level(),
            degradation_enabled: cfg.degradation_summary_enabled_or_default(),
            retry: claurst_api::RetryConfig::with_max_retries(cfg.max_retries),
            ..Default::default()
        }
    }

    /// Build a QueryConfig using dynamic model resolution from the model registry.
    ///
    /// Prefers the best model for the configured provider (from models.dev data)
    /// over the hardcoded defaults.
    pub fn from_config_with_registry(cfg: &Config, registry: &claurst_api::ModelRegistry) -> Self {
        // We can't move the Arc here, but we need a clone for the query loop.
        // Callers typically wrap the registry in an Arc already.
        Self {
            model: claurst_api::effective_model_for_config(cfg, registry),
            max_tokens: cfg.effective_max_tokens(),
            output_style: cfg.effective_output_style(),
            output_style_prompt: cfg.resolve_output_style_prompt(),
            working_directory: cfg
                .project_dir
                .as_ref()
                .map(|p| p.display().to_string()),
            managed_agents: cfg.managed_agents.clone(),
            effort_level: cfg.effective_effort_level(),
            degradation_enabled: cfg.degradation_summary_enabled_or_default(),
            retry: claurst_api::RetryConfig::with_max_retries(cfg.max_retries),
            ..Default::default()
        }
    }
}

/// Events emitted by the query loop for the TUI to render.
#[derive(Debug, Clone)]
pub enum QueryEvent {
    /// A stream event from the API.
    Stream(AnthropicStreamEvent),
    /// A tool is about to be executed.
    ToolStart { tool_name: String, tool_id: String, input_json: String },
    /// A tool has finished executing.
    ToolEnd { tool_name: String, tool_id: String, result: String, is_error: bool },
    /// The model finished a turn.
    TurnComplete { turn: u32, stop_reason: String, usage: Option<UsageInfo> },
    /// An informational status message.
    Status(String),
    /// An error.
    Error(String),
    /// Token usage has crossed a warning threshold.
    /// `state` is Warning (≥ 80 %) or Critical (≥ 95 %).
    /// `pct_used` is the fraction of the context window consumed (0.0–1.0).
    TokenWarning { state: TokenWarningState, pct_used: f64 },
    /// A model request failed transiently and will be retried after
    /// `delay_ms` (#439). `attempt` is 1-based and at most `max_attempts`.
    /// Output streamed by the failed attempt is discarded.
    Retry { attempt: u32, max_attempts: u32, delay_ms: u64, reason: String },
    /// The loop compacted the history (issue #441). The caller records the
    /// entry in the session; `tokens_after` is the new context estimate.
    Compacted(CompactionRecord),
    /// Provider-reported subscription usage (0-100 per window), e.g. the
    /// Codex `x-codex-*` rate-limit headers.
    RateLimits { primary_used_percent: Option<f32>, secondary_used_percent: Option<f32> },
}

// ---------------------------------------------------------------------------
// Tool-result budgeting
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Query loop
// ---------------------------------------------------------------------------

/// Default per-item cap for tool results appended to the history: 64k bytes,
/// roughly 16k tokens. It sits above the largest tool-native output window
/// (Read's 50 KiB page and Bash's 50 KiB window, each plus a continuation
/// trailer) so the loop never re-cuts output a tool already bounded; cutting
/// the middle of a Read page would hide the gap behind an intact offset
/// trailer. It only catches tools without their own bound.
pub const DEFAULT_TOOL_RESULT_MAX_BYTES: usize = 64_000;

/// Maximum number of max_tokens continuation attempts before surfacing the
/// partial response.  Mirrors `MAX_OUTPUT_TOKENS_RECOVERY_LIMIT` in query.ts.
const MAX_TOKENS_RECOVERY_LIMIT: u32 = 3;

/// Message injected when the model hits its output-token limit.
/// Mirrors the TS recovery message in query.ts lines 1224-1228.
const MAX_TOKENS_RECOVERY_MSG: &str =
    "Output token limit hit. Resume directly — no apology, no recap of what \
     you were doing. Pick up mid-thought if that is where the cut happened. \
     Break remaining work into smaller pieces.";

/// Injected as the final user turn when `effective_max_turns` is reached. That
/// turn runs with tools DISABLED (graceful degradation, mirroring opencode's
/// max-steps `toolChoice:"none"` behaviour), so the model produces a plain-text
/// wrap-up instead of the loop returning cold.
const MAX_STEPS_DEGRADATION_MSG: &str =
    "You have reached the maximum number of steps for this run, so tools are now \
     disabled — do not attempt to call any tools. In plain text, briefly \
     summarize what you accomplished, what remains unfinished, and exactly where \
     you stopped, so the work can be resumed later.";

/// Content stored in the synthetic `tool_result` for a tool that was abandoned
/// mid-flight because the query loop was cancelled (issue #218). Every
/// outstanding `tool_use` still receives a matching `tool_result` carrying this
/// text so the message history stays well-formed.
const TOOL_CANCELLED_MSG: &str = "Tool execution was cancelled by the user before it completed.";

fn merge_provider_stream_usage(current: &mut UsageInfo, update: &UsageInfo) {
    if update.total_input() > 0 {
        current.input_tokens = update.input_tokens;
        current.cache_read_input_tokens = update.cache_read_input_tokens;
        current.cache_creation_input_tokens = update.cache_creation_input_tokens;
    }
    if update.output_tokens > 0 {
        current.output_tokens = update.output_tokens;
    }
}

// Spinner verbs are imported from claurst_core::spinner

/// Resolve the effective effort level for a turn.
///
/// Ultracode is a keyword-activated effort: if the most recent user message
/// contains the `ultracode` keyword (whole-word, case-insensitive), the effort
/// for this turn is raised to [`EffortLevel::Ultracode`] — the model's top
/// reasoning plus the ultracode operating procedure (injected as a system
/// addendum by the loop). Otherwise the configured `config_effort` is used
/// unchanged. Checking only the *last* user message keeps the mode scoped to the
/// turn that asked for it (a later plain turn deactivates it automatically).
///
/// [`EffortLevel::Ultracode`]: claurst_core::effort::EffortLevel::Ultracode
fn effective_effort_for_turn(
    config_effort: Option<claurst_core::effort::EffortLevel>,
    messages: &[Message],
) -> Option<claurst_core::effort::EffortLevel> {
    if let Some(last_user) = messages.iter().rev().find(|m| m.role == Role::User) {
        if claurst_core::effort::text_triggers_ultracode(&last_user.get_all_text()) {
            return Some(claurst_core::effort::EffortLevel::Ultracode);
        }
    }
    config_effort
}

/// Resolve the effective output-style persona for a turn.
///
/// Personas (`rocky` / `caveman` / `normal`) mirror the ultracode keyword: an
/// **inline** persona word in the most recent user message applies to *that one
/// turn* (transient) and then reverts, while the persona chosen via `/rocky`,
/// `/caveman`, or `/output-style` lives in `config` and **persists** until
/// changed. Inline `normal` resets to the default (no persona) for the turn.
///
/// Returns the `(output_style, output_style_prompt)` pair to assemble the
/// system prompt with for this turn. When no inline persona keyword is present,
/// the configured (persistent) pair is returned unchanged. Checking only the
/// *last* user message keeps the mode scoped to the turn that asked for it.
fn effective_output_style_for_turn(
    config: &QueryConfig,
    messages: &[Message],
) -> (
    claurst_core::system_prompt::OutputStyle,
    Option<String>,
) {
    if let Some(last_user) = messages.iter().rev().find(|m| m.role == Role::User) {
        if let Some(style_name) =
            claurst_core::keywords::inline_persona_style(&last_user.get_all_text())
        {
            // Inline `normal` (→ "default") resets the persona for this turn.
            if style_name == "default" {
                return (claurst_core::system_prompt::OutputStyle::Default, None);
            }
            // Otherwise apply the named persona's prompt for this turn only.
            let prompt = claurst_core::output_styles::find_style(
                &claurst_core::output_styles::builtin_styles(),
                style_name,
            )
            .map(|style| style.prompt.clone())
            .filter(|prompt| !prompt.trim().is_empty());
            return (claurst_core::system_prompt::OutputStyle::Default, prompt);
        }
    }
    // No inline persona keyword — keep the persistent selection.
    (config.output_style, config.output_style_prompt.clone())
}

/// Run the agentic query loop.
///
/// This sends the conversation to the API, handles tool calls in a loop, and
/// returns when the model issues an end_turn or an error/limit is hit.
///
/// `pending_messages` is an optional queue of user messages that were enqueued
/// during tool execution (e.g. by the UI or a command queue).  Each string is
/// appended as a plain user message between turns.  Callers that do not need
/// command queuing may pass `None` or an empty `Vec`.
pub async fn run_query_loop(
    client: &claurst_api::AnthropicClient,
    messages: &mut Vec<Message>,
    tools: &[Box<dyn Tool>],
    tool_ctx: &ToolContext,
    config: &QueryConfig,
    cost_tracker: Arc<CostTracker>,
    event_tx: Option<mpsc::UnboundedSender<QueryEvent>>,
    cancel_token: tokio_util::sync::CancellationToken,
    mut pending_messages: Option<&mut Vec<String>>,
) -> QueryOutcome {
    // Rebind the tool context to carry the loop's actual cancel token so the
    // parallel tool executor — and any tools or sub-agents that read
    // `ctx.cancel_token` — observe the same cancellation signal that drives this
    // loop (issue #218). Callers construct the context with a placeholder token;
    // making the loop authoritative here means a parent cancel reaches tools.
    let mut loop_ctx = tool_ctx.clone();
    loop_ctx.cancel_token = cancel_token.clone();
    let tool_ctx = &loop_ctx;

    let mut turn = 0u32;
    let mut compact_state = compact::AutoCompactState::default();
    // Prompt-token count reported for the last request, anchoring the
    // pre-request context projection. Cleared whenever history is compacted.
    let mut last_usage: Option<compact::UsageMark> = None;
    // One-shot guard: a context-window error compacts and retries the request
    // once; a second overflow on the same request surfaces the error.
    let mut overflow_recovery_used = false;
    // Tracks how many consecutive max_tokens recoveries we've attempted so
    // we don't loop forever on a model that can't finish within any budget.
    let mut max_tokens_recovery_count: u32 = 0;
    // Active model — may switch to fallback on overloaded errors.
    // Agent model override takes priority over the session model when set.
    let mut effective_model = if let Some(ref agent) = config.agent_definition {
        agent.model.clone().unwrap_or_else(|| config.model.clone())
    } else {
        config.model.clone()
    };

    // If managed-agent mode is active, override the model to the manager model.
    if let Some(ref ma_config) = config.managed_agents {
        if ma_config.enabled && !ma_config.manager_model.is_empty() {
            effective_model = ma_config.manager_model.clone();
        }
    }

    let mut used_fallback = false;
    // Retries spent on the current model request (#439). Bounded by
    // `config.retry.max_retries`; reset once a response streams to completion.
    let mut retry_attempt: u32 = 0;
    // Max-steps graceful degradation (issue #230 / MI-3): set once the final
    // tool-less summary turn has been dispatched so it can never re-trigger
    // (anti-recursion guard).
    let mut degradation_done = false;

    // Turn-cap precedence (PR #398 review): session override (set via
    // `/turns`) wins over the agent definition, which wins over config.
    // Without the explicit override field, agent definitions like the
    // built-in `plan` agent (max_turns: Some(20)) would silently ignore a
    // user-requested cap change.
    let effective_max_turns = config.max_turns_override
        .or_else(|| config.agent_definition.as_ref().and_then(|a| a.max_turns))
        .unwrap_or(config.max_turns);

    // In-loop continuation policy (issue #230 / MI-3). Consulted at the end of
    // every turn that finishes with `end_turn`. The default policy stops after
    // one turn; the goal policy keeps the loop running while an active goal's
    // guards allow. Built once per run.
    let continuation_policy = config.continuation.policy();
    // Wall-clock start of the current "continuation turn" (a span from a user /
    // continuation message to the next `end_turn`). Reset on each accepted
    // continuation so goal time/turn accounting matches the old per-dispatch
    // measurement.
    let mut goal_turn_start = std::time::Instant::now();

    // Shadow-git snapshot: capture the worktree state before any tools run so we
    // can produce a per-turn file-change patch when the turn ends.
    let shadow_snap: Option<std::sync::Arc<claurst_core::snapshot::ShadowSnapshot>> =
        if tool_ctx.config.auto_commits == Some(true) {
            claurst_core::snapshot::get_or_create(&tool_ctx.working_dir)
        } else {
            None
        };
    // Pre-capture tree hash; refreshed at the start of each turn's tool phase.
    let initial_snapshot: Option<String> = if let Some(ref s) = shadow_snap {
        s.track().await
    } else {
        None
    };

    loop {
        turn += 1;
        tool_ctx
            .current_turn
            .store(turn as usize, std::sync::atomic::Ordering::Relaxed);
        // Max-steps graceful degradation (issue #230 / MI-3). Rather than
        // returning cold when the turn cap is hit, run ONE final turn with tools
        // disabled that asks the model to summarize progress and its stopping
        // point (mirrors opencode's max-steps `toolChoice:"none"` fallback).
        // `degradation_done` is the anti-recursion guard: the summary turn is
        // dispatched exactly once, and re-exceeding the cap afterwards returns
        // cold. Applies to both goal and non-goal runs.
        let degradation_turn = if turn > effective_max_turns {
            if !config.degradation_enabled || degradation_done {
                info!(
                    turns = turn,
                    max = effective_max_turns,
                    degradation_enabled = config.degradation_enabled,
                    "Max turns reached — returning without a further summary turn"
                );
                let last_msg = messages
                    .last()
                    .cloned()
                    .unwrap_or_else(|| Message::assistant("Max turns reached."));
                return QueryOutcome::EndTurn {
                    message: last_msg,
                    usage: UsageInfo::default(),
                };
            }
            degradation_done = true;
            info!(
                turns = turn,
                max = effective_max_turns,
                "Max turns reached — running final tool-less summary turn"
            );
            if let Some(ref tx) = event_tx {
                let _ = tx.send(QueryEvent::Status(format!(
                    "Reached maximum turn limit ({}) — summarizing progress before stopping.",
                    effective_max_turns
                )));
            }
            // Inject the summary request as the next user turn. Tools are
            // disabled for this turn where `api_tools` / `provider_tools` are
            // built below.
            messages.push(Message::user(MAX_STEPS_DEGRADATION_MSG));
            true
        } else {
            false
        };

        // Continuation decision at `end_turn` (issue #230 / MI-3). Consults the
        // active continuation policy: `Continue` injects the follow-up message
        // as the next user turn and keeps looping (resetting the per-turn budget
        // so `effective_max_turns` bounds tool-rounds *within* a continuation
        // turn — the cross-turn cap is the policy's own guard, e.g. the goal
        // runaway limit); `Stop` surfaces any note and returns `EndTurn`.
        // Defined as a macro because it must `continue`/`return` the loop.
        macro_rules! continue_or_end {
            ($assistant_msg:expr, $usage:expr) => {{
                // The tool-less max-steps summary turn must never re-trigger
                // continuation (anti-recursion): return its wrap-up directly.
                if degradation_turn {
                    return QueryOutcome::EndTurn {
                        message: $assistant_msg,
                        usage: $usage,
                    };
                }
                let decision = continuation_policy.decide(&crate::continuation::TurnEndContext {
                    session_id: &tool_ctx.session_id,
                    total_tokens_used: cost_tracker.total_tokens(),
                    turn_elapsed_secs: goal_turn_start.elapsed().as_secs(),
                });
                match decision {
                    crate::continuation::ContinuationDecision::Continue { message } => {
                        if let Some(ref tx) = event_tx {
                            let _ = tx.send(QueryEvent::Status(
                                "Goal: continuing autonomously… (use /goal pause to stop)"
                                    .to_string(),
                            ));
                        }
                        messages.push(Message::user(message));
                        // Fresh per-continuation-turn budget, mirroring the old
                        // one-loop-per-goal-turn design.
                        turn = 0;
                        max_tokens_recovery_count = 0;
                        retry_attempt = 0;
                        goal_turn_start = std::time::Instant::now();
                        continue;
                    }
                    crate::continuation::ContinuationDecision::Stop { note } => {
                        if let Some(note) = note {
                            if let Some(ref tx) = event_tx {
                                let _ = tx.send(QueryEvent::Status(note));
                            }
                        }
                        return QueryOutcome::EndTurn {
                            message: $assistant_msg,
                            usage: $usage,
                        };
                    }
                }
            }};
        }

        // Transient-failure retry (#439). When the retry budget allows: report
        // the retry, wait per `config.retry` (cancellable), and restart this
        // turn. Otherwise fall through so the caller surfaces the error.
        macro_rules! retry_turn {
            ($reason:expr, $after:expr) => {{
                if retry_attempt < config.retry.max_retries {
                    retry_attempt += 1;
                    let reason: String = $reason;
                    let delay = config.retry.delay_for(retry_attempt, $after);
                    warn!(
                        attempt = retry_attempt,
                        max_retries = config.retry.max_retries,
                        delay_ms = delay.as_millis() as u64,
                        reason = %reason,
                        "Transient model-request failure; retrying"
                    );
                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(QueryEvent::Retry {
                            attempt: retry_attempt,
                            max_attempts: config.retry.max_retries,
                            delay_ms: delay.as_millis() as u64,
                            reason,
                        });
                    }
                    tokio::select! {
                        _ = cancel_token.cancelled() => return QueryOutcome::Cancelled,
                        _ = tokio::time::sleep(delay) => {}
                    }
                    if degradation_turn {
                        // The retried turn re-injects the summary request.
                        // Messages drained after the injection (pending /
                        // command queue) may follow it, so search backwards.
                        degradation_done = false;
                        if let Some(idx) = messages.iter().rposition(|m| {
                            m.role == Role::User && m.get_all_text() == MAX_STEPS_DEGRADATION_MSG
                        }) {
                            messages.remove(idx);
                        }
                    }
                    turn -= 1;
                    continue;
                }
            }};
        }

        // Check for cancellation
        if cancel_token.is_cancelled() {
            return QueryOutcome::Cancelled;
        }

        // Drain any pending user messages that were queued during the previous
        // tool-execution phase (e.g. commands entered while tools ran).
        // Mirrors the TS `messageQueueManager` drain between turns.
        if let Some(queue) = pending_messages.as_deref_mut() {
            for text in queue.drain(..) {
                debug!("Injecting pending message: {}", &text);
                messages.push(Message::user(text));
            }
        }

        // T1-4: Drain the priority command queue (if wired up) and prepend any
        // resulting messages to the conversation before the API call.
        // Mirrors the TS `messageQueueManager` priority-queue drain.
        if let Some(ref cq) = config.command_queue {
            if !cq.is_empty() {
                let injected = drain_command_queue(cq);
                if !injected.is_empty() {
                    debug!(count = injected.len(), "Injecting command-queue messages");
                    // Prepend so that higher-priority commands appear first.
                    let tail = std::mem::take(messages);
                    messages.extend(injected);
                    messages.extend(tail);
                }
            }
        }

        // Resolve where this request goes once, up front, so the compaction
        // check below summarises through the same provider the request uses.
        let route = config.provider_registry.as_deref().map(|registry| {
            resolve_provider_route(registry, config, tool_ctx, &effective_model, client)
        });

        // Pre-request compaction check point (issue #441). It runs before
        // every request, including the one that follows each tool batch, so
        // a large batch of tool results is measured before it is sent.
        //
        // The window comes from the models.dev-backed registry (correct for
        // every provider — 1M Gemini/GPT windows, 32k local models) with the
        // Claude-centric heuristic as fallback (#216). The numerator anchors
        // on the real prompt-token count of the last request and estimates
        // only what was appended since (#231).
        let context_window = compact::resolve_context_window(
            config.model_registry.as_deref(),
            route.as_ref().map_or_else(
                || tool_ctx.config.provider.as_deref().unwrap_or("anthropic"),
                |route| route.provider_id.as_str(),
            ),
            &effective_model,
        );
        let context_tokens = compact::estimate_context_tokens(messages, last_usage);

        // Emit token warning events when approaching context limits.
        // Thresholds mirror TypeScript autoCompact.ts: 80% → Warning, 95% → Critical.
        let warning_state =
            compact::calculate_token_warning_state_for_window(context_tokens, context_window);
        if warning_state != compact::TokenWarningState::Ok {
            if let Some(ref tx) = event_tx {
                let _ = tx.send(QueryEvent::TokenWarning {
                    state: warning_state,
                    pct_used: context_tokens as f64 / context_window as f64,
                });
            }
        }

        if config.auto_compact
            && compact_before_request(
                messages,
                route.as_ref(),
                client,
                &effective_model,
                context_tokens,
                context_window,
                config.max_tokens,
                &mut compact_state,
                event_tx.as_ref(),
                &cancel_token,
            )
            .await
        {
            last_usage = None;
        }

        // Request-boundary invariant pass (issue #229 / MI-2). Compaction,
        // max_tokens recovery, and the command-queue / pending-message drains
        // above can each independently leave the history with a broken
        // tool_use ↔ tool_result pairing (an orphan result, or a dangling
        // tool_use) that the provider rejects with HTTP 400. Heal it here —
        // the single choke point covering BOTH the legacy Anthropic path
        // (`api_messages` below) and the modern provider path (`provider_messages`
        // built later in the dispatch branch), since both derive from `messages`.
        // sanitize_history is idempotent, so a well-formed history is untouched.
        *messages = sanitize::sanitize_history(std::mem::take(messages));

        // Build API request
        let api_messages: Vec<ApiMessage> = messages.iter().map(ApiMessage::from).collect();
        // Max-steps degradation: the final summary turn is dispatched with NO
        // tool definitions so the model can only produce text (issue #230).
        let api_tools: Vec<ApiToolDefinition> = if degradation_turn {
            Vec::new()
        } else {
            tools
                .iter()
                .map(|t| ApiToolDefinition::from(&t.to_definition()))
                .collect()
        };

        // Effective effort for THIS turn. The configured effort is overridden to
        // Ultracode when the latest user message invokes the `ultracode` keyword,
        // so an ultracode turn gets the model's top reasoning (via the budget /
        // provider mapping below) plus the ultracode procedure addendum injected
        // into the system prompt.
        let effective_effort_level =
            effective_effort_for_turn(config.effort_level, messages.as_slice());

        // Assemble the [stable, dynamic] system blocks. Everything that varies
        // per turn (todo nudge, goal and ultracode addenda, the transient
        // output-style persona) is routed into the dynamic block through
        // `append_system_prompt` / `output_style`, so the stable block stays
        // byte-identical across turns and keeps its prompt-cache entry.
        let system = {
            // Build a (possibly patched) config for system-prompt assembly.
            // Agent prompt prefix and todo nudge are both applied here.
            let mut patched = config.clone();

            // Progressive tool disclosure (issue #233 completion): populate
            // `enabled_tools` from the live tool set this run exposes so
            // `build_system_prompt` only emits per-tool guideline blocks for
            // tools that are actually loaded. This is the boundary #233 wired
            // up; sub-agents already set it explicitly, so only fill it in when
            // the caller left it unset.
            if patched.enabled_tools.is_none() {
                patched.enabled_tools =
                    Some(tools.iter().map(|t| t.name().to_string()).collect());
            }
            if patched.permission_mode.is_none() {
                patched.permission_mode = Some(tool_ctx.config.permission_mode.clone());
            }
            if patched.additional_dirs.is_empty() {
                patched.additional_dirs = tool_ctx
                    .config
                    .additional_dirs
                    .iter()
                    .map(|dir| dir.display().to_string())
                    .collect();
            }

            // Apply agent system-prompt prefix: prepend before the main system prompt.
            if let Some(ref agent) = config.agent_definition {
                if let Some(ref agent_prompt) = agent.prompt {
                    patched.system_prompt = Some(match &config.system_prompt {
                        Some(existing) => format!("{}\n\n{}", agent_prompt, existing),
                        None => agent_prompt.clone(),
                    });
                }
            }

            // If managed-agent mode is active, append orchestration instructions.
            if let Some(ref ma_config) = config.managed_agents {
                if ma_config.enabled {
                    let ma_prompt = crate::managed_orchestrator::managed_agent_system_prompt(ma_config);
                    patched.append_system_prompt = Some(match &patched.append_system_prompt {
                        Some(existing) => format!("{}\n\n{}", existing, ma_prompt),
                        None => ma_prompt,
                    });
                }
            }

            // Apply todo nudge on turns > 2.
            if turn > 2 {
                let nudge = build_todo_nudge(&tool_ctx.session_id);
                if !nudge.is_empty() {
                    patched.append_system_prompt = Some(match patched.append_system_prompt.take() {
                        Some(existing) => format!("{}\n\n{}", existing, nudge),
                        None => nudge,
                    });
                }
            }

            // MCP server instructions (from `initialize`) for servers whose
            // tools this run exposes. Appended after the dynamic boundary:
            // servers connect and change their tools during a session.
            if let Some(section) = tool_ctx.mcp_manager.as_ref().and_then(|manager| {
                manager.instructions_prompt_section(|name| tools.iter().any(|t| t.name() == name))
            }) {
                patched.append_system_prompt = Some(match patched.append_system_prompt.take() {
                    Some(existing) => format!("{}\n\n{}", existing, section),
                    None => section,
                });
            }

            // Goal system-prompt addendum (issue #230 / MI-3). Applied fresh
            // each turn (goal state — turns used, elapsed — changes over the
            // run) whenever goal continuation mode is active and a live goal
            // exists for this session. This relocates the addendum injection
            // from the CLI into the loop so continuation turns get it too.
            // GoalStore access here is fully synchronous (no lock held across
            // an `.await`).
            if matches!(config.continuation, crate::continuation::ContinuationMode::Goal) {
                if let Some(goal) = claurst_core::GoalStore::open_default()
                    .and_then(|s| s.get_active_goal(&tool_ctx.session_id))
                {
                    let addendum = claurst_core::goal_system_prompt_addendum(&goal);
                    patched.append_system_prompt = Some(match patched.append_system_prompt.take() {
                        Some(existing) => format!("{}\n{}", existing, addendum),
                        None => addendum,
                    });
                }
            }

            // Ultracode effort. When the effective effort for this turn is
            // Ultracode (set by the `ultracode` keyword or an explicit /effort
            // ultracode), inject the ultracode operating procedure as a per-turn
            // system addendum — the same injection path the goal addendum uses.
            // The keyword also raises the effort to top reasoning (see the
            // budget / provider mapping below). Applied fresh each turn so it
            // deactivates naturally, and composes with goal mode.
            if effective_effort_level == Some(claurst_core::effort::EffortLevel::Ultracode) {
                let uc_addendum = claurst_core::effort::ultracode_system_prompt_addendum();
                patched.append_system_prompt = Some(match patched.append_system_prompt.take() {
                    Some(existing) => format!("{}\n{}", existing, uc_addendum),
                    None => uc_addendum,
                });
            }

            // Output-style persona for THIS turn. An inline `rocky` / `caveman`
            // / `normal` keyword in the latest user message overrides the
            // persisted output style transiently (used for this turn, then
            // reverts); otherwise the persisted selection stands. Mirrors the
            // ultracode keyword's transient-vs-persistent behaviour above.
            let (turn_output_style, turn_output_style_prompt) =
                effective_output_style_for_turn(config, messages.as_slice());
            patched.output_style = turn_output_style;
            patched.output_style_prompt = turn_output_style_prompt;

            build_system_prompt(&patched)
        };

        let system_for_provider = system.clone(); // used by non-Anthropic dispatch below
        let mut req_builder = CreateMessageRequest::builder(&effective_model, config.max_tokens)
            .messages(api_messages)
            .system(system)
            .tools(api_tools);

        // Resolve effective thinking budget:
        //   1. Explicit `thinking_budget` in config takes precedence.
        //   2. Fall back to the effort level's budget when no explicit budget is set.
        let effective_thinking_budget = config.thinking_budget.or_else(|| {
            effective_effort_level.and_then(|el| el.thinking_budget_tokens())
        });

        // Claude reasoning controls: adaptive thinking + `output_config.effort`
        // on current models, `budget_tokens` on older ones (one table in
        // `claurst_api::anthropic_reasoning`). Non-Claude models fall into the
        // budget branch, which is the previous behaviour.
        let anthropic_reasoning = claurst_api::anthropic_reasoning(
            &effective_model,
            effective_effort_level,
            effective_thinking_budget,
        );
        let thinking_is_adaptive = anthropic_reasoning.is_adaptive();
        if let Some(thinking) = anthropic_reasoning.thinking {
            req_builder = req_builder.thinking(thinking);
        }
        if let Some(effort) = anthropic_reasoning.effort {
            req_builder = req_builder.effort(effort);
        }

        // Apply temperature: explicit config value takes precedence, then agent override,
        // then effort-level override (not for adaptive-thinking models, which
        // reject sampling parameters alongside thinking).
        let effective_temperature = config.temperature
            .or_else(|| {
                config.agent_definition.as_ref()
                    .and_then(|a| a.temperature)
                    .map(|t| t as f32)
            })
            .or_else(|| {
                effective_effort_level
                    .filter(|_| !thinking_is_adaptive)
                    .and_then(|el| el.temperature())
            });
        if let Some(t) = effective_temperature {
            req_builder = req_builder.temperature(t);
        }

        // `request` is only ever sent through the Anthropic client below (the
        // provider dispatch path builds its own `ProviderRequest`), so the
        // Anthropic prompt-cache breakpoints are placed here unconditionally.
        let mut request = req_builder.build();
        request.apply_cache_breakpoints();

        // Create a stream handler that forwards to the event channel
        let handler: Arc<dyn StreamHandler> = if let Some(ref tx) = event_tx {
            let tx = tx.clone();
            Arc::new(ChannelStreamHandler { tx })
        } else {
            Arc::new(claurst_api::streaming::NullStreamHandler)
        };

        // Non-Anthropic provider dispatch through the route resolved at the
        // top of this iteration (see `resolve_provider_route`).
        if let Some(route) = route {
            let ProviderRoute {
                provider_id: provider_id_str,
                model_id: model_id_str,
                use_provider_dispatch,
                provider,
            } = route;
            if use_provider_dispatch {
                if let Some(provider) = provider {
                    debug!(provider = %provider_id_str, model = %model_id_str, "Dispatching to non-Anthropic provider");

                    // Notify TUI that we're calling the provider using a random spinner verb
                    if let Some(ref tx) = event_tx {
                        use claurst_core::sample_spinner_verb;
                        let seed = provider_id_str.len() ^ model_id_str.len();
                        let verb = sample_spinner_verb(seed);
                        let _ = tx.send(QueryEvent::Status(format!("✳ {}…", verb)));
                    }

                    // Build ProviderRequest from the already-assembled request data.
                    // tools comes from the api_tools we already built above.
                    // Filter unsupported modalities: replace Image/Document blocks
                    // with placeholder text when the provider doesn't support them,
                    // preventing crashes on text-only models.
                    let mut caps = provider.capabilities();
                    if let Some(model_entry) = config
                        .model_registry
                        .as_ref()
                        .and_then(|model_registry| model_registry.get(&provider_id_str, &model_id_str))
                    {
                        caps.image_input = model_entry.vision();
                        caps.tool_calling = model_entry.tool_calling;
                        caps.thinking = model_entry.reasoning;
                    }
                    // Max-steps degradation (issue #230): dispatch the final
                    // summary turn with no tools so the provider can only emit
                    // text (opencode's `toolChoice:"none"` equivalent).
                    let provider_tools: Vec<claurst_core::types::ToolDefinition> = if caps.tool_calling && !degradation_turn {
                        tools.iter().map(|t| t.to_definition()).collect()
                    } else {
                        Vec::new()
                    };
                    let provider_messages: Vec<claurst_core::types::Message> = messages
                        .iter()
                        .map(|msg| {
                            let mut msg = msg.clone();
                            if let claurst_core::types::MessageContent::Blocks(ref mut blocks) = msg.content {
                                for block in blocks.iter_mut() {
                                    match block {
                                        claurst_core::types::ContentBlock::Image { .. } if !caps.image_input => {
                                            *block = claurst_core::types::ContentBlock::Text {
                                                text: "[Image not supported by this model]".to_string(),
                                            };
                                        }
                                        claurst_core::types::ContentBlock::Document { .. } if !caps.pdf_input => {
                                            *block = claurst_core::types::ContentBlock::Text {
                                                text: "[PDF not supported by this model]".to_string(),
                                            };
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            msg
                        })
                        .collect();

                    let provider_request = claurst_api::ProviderRequest {
                        model: model_id_str.to_owned(),
                        messages: provider_messages,
                        system_prompt: Some(system_for_provider.clone()),
                        tools: provider_tools,
                        max_tokens: config.max_tokens,
                        temperature: effective_temperature.map(|t| t as f64),
                        top_p: None,
                        top_k: None,
                        stop_sequences: vec![],
                        thinking: if !caps.thinking {
                            None
                        } else if provider_id_str == "anthropic" {
                            claurst_api::anthropic_reasoning(
                                &model_id_str,
                                effective_effort_level,
                                effective_thinking_budget,
                            )
                            .thinking
                        } else {
                            effective_thinking_budget
                                .map(claurst_api::ThinkingConfig::enabled)
                        },
                        provider_options: {
                            let mut options = build_provider_options(
                                &provider_id_str,
                                &model_id_str,
                                effective_effort_level,
                                effective_thinking_budget,
                            );
                            // ChatGPT routes its prompt cache on a stable
                            // per-session key (official client: session id).
                            if matches!(provider_id_str.as_str(), "codex" | "openai-codex") {
                                if let Some(map) = options.as_object_mut() {
                                    map.insert(
                                        "prompt_cache_key".to_string(),
                                        serde_json::json!(tool_ctx.session_id),
                                    );
                                }
                            }
                            options
                        },
                    };

                    // Use create_message_stream so the TUI receives real-time
                    // text deltas instead of waiting for the full response.
                    let connect = tokio::select! {
                        _ = cancel_token.cancelled() => return QueryOutcome::Cancelled,
                        r = provider.create_message_stream(provider_request) => r,
                    };
                    let mut stream = match connect {
                        Ok(s) => s,
                        Err(e) => {
                            // Context-window error: compact once and retry,
                            // before the transient-retry policy (#441, #439).
                            if config.auto_compact
                                && !overflow_recovery_used
                                && provider_error_is_overflow(&e)
                            {
                                overflow_recovery_used = true;
                                if compact_after_overflow(
                                    messages,
                                    provider.as_ref(),
                                    &model_id_str,
                                    config.max_tokens,
                                    event_tx.as_ref(),
                                )
                                .await
                                {
                                    last_usage = None;
                                    turn -= 1;
                                    continue;
                                }
                            }
                            if let claurst_api::RetryDecision::Retry { after } =
                                claurst_api::retry::classify_provider_error(&e)
                            {
                                retry_turn!(e.to_string(), after);
                            }
                            error!(provider = %provider_id_str, error = %e, "Provider stream failed");
                            return QueryOutcome::Error(ClaudeError::Api(e.to_string()));
                        }
                    };

                    // Accumulators for building the final assistant message.
                    let mut text_chunks: Vec<String> = Vec::new();
                    // Accumulate reasoning/thinking content for providers like
                    // DeepSeek that require reasoning_content to be sent back.
                    let mut thinking_chunks: Vec<String> = Vec::new();
                    // Opaque reasoning state (Codex `encrypted_content`)
                    // that must be replayed on the next request.
                    let mut redacted_thinking_blocks: Vec<ContentBlock> = Vec::new();
                    // tool_call_blocks: index → (id, name, accumulated_json, thought_signature)
                    // thought_signature carries Gemini's opaque per-call signature
                    // through stream assembly so it survives into the persisted
                    // ToolUse block and is echoed back next turn (#311).
                    let mut tool_call_blocks: std::collections::HashMap<usize, (String, String, String, Option<String>)> =
                        std::collections::HashMap::new();
                    let mut usage = UsageInfo::default();
                    let mut stop_str = "end_turn".to_string();
                    let mut msg_id = uuid::Uuid::new_v4().to_string();

                    use futures::StreamExt as ProviderStreamExt;
                    let provider_stall_timeout = std::time::Duration::from_secs(45);
                    let provider_stall = tokio::time::sleep(provider_stall_timeout);
                    tokio::pin!(provider_stall);
                    let mut provider_stream_stalled = false;
                    // Stream integrity (#439): only `MessageStop` marks a
                    // complete response; a stream that just ends is truncated.
                    let mut saw_message_stop = false;
                    // Set when the stream yields a mid-stream `Err`. The
                    // accumulated text/tool-calls are then incomplete and MUST
                    // NOT be assembled into a "completed" turn (issue #215).
                    // The error is kept for the overflow check; the decision
                    // drives the retry policy (#439).
                    let mut provider_stream_error: Option<(
                        claurst_api::ProviderError,
                        claurst_api::RetryDecision,
                    )> = None;

                    loop {
                        tokio::select! {
                            _ = cancel_token.cancelled() => {
                                return QueryOutcome::Cancelled;
                            }
                            _ = &mut provider_stall => {
                                provider_stream_stalled = true;
                                break;
                            }
                            event = stream.next() => {
                                provider_stall.as_mut().reset(tokio::time::Instant::now() + provider_stall_timeout);
                                match event {
                                    None => break,
                                    Some(Err(e)) => {
                                        error!(provider = %provider_id_str, error = %e, "Provider stream error");
                                        let decision = claurst_api::retry::classify_provider_error(&e);
                                        provider_stream_error = Some((e, decision));
                                        break;
                                    }
                                    Some(Ok(evt)) => {
                                        // Forward to TUI via AnthropicStreamEvent mapping.
                                        if let Some(ref tx) = event_tx {
                                            if let Some(ae) = map_to_anthropic_event(&evt) {
                                                let _ = tx.send(QueryEvent::Stream(ae));
                                            }
                                        }

                                        // Accumulate response data.
                                        match &evt {
                                            claurst_api::StreamEvent::MessageStart { id, usage: u, .. } => {
                                                msg_id = id.clone();
                                                merge_provider_stream_usage(&mut usage, u);
                                            }
                                            claurst_api::StreamEvent::ContentBlockStart {
                                                index,
                                                content_block: ContentBlock::ToolUse { id, name, thought_signature, .. },
                                            } => {
                                                tool_call_blocks.insert(*index, (id.clone(), name.clone(), String::new(), thought_signature.clone()));
                                            }
                                            claurst_api::StreamEvent::ContentBlockStart {
                                                content_block: block @ ContentBlock::RedactedThinking { .. },
                                                ..
                                            } => {
                                                redacted_thinking_blocks.push(block.clone());
                                            }
                                            claurst_api::StreamEvent::TextDelta { text, .. } => {
                                                text_chunks.push(text.clone());
                                            }
                                            claurst_api::StreamEvent::ThinkingDelta { thinking, .. } => {
                                                thinking_chunks.push(thinking.clone());
                                            }
                                            claurst_api::StreamEvent::ReasoningDelta { reasoning, .. } => {
                                                thinking_chunks.push(reasoning.clone());
                                            }
                                            claurst_api::StreamEvent::InputJsonDelta { index, partial_json } => {
                                                if let Some((_, _, buf, _)) = tool_call_blocks.get_mut(index) {
                                                    buf.push_str(partial_json);
                                                }
                                            }
                                            claurst_api::StreamEvent::MessageDelta { stop_reason, usage: u } => {
                                                stop_str = match stop_reason {
                                                    Some(claurst_api::provider_types::StopReason::ToolUse) => "tool_use".to_string(),
                                                    Some(claurst_api::provider_types::StopReason::MaxTokens) => "max_tokens".to_string(),
                                                    Some(claurst_api::provider_types::StopReason::StopSequence) => "stop_sequence".to_string(),
                                                    Some(claurst_api::provider_types::StopReason::ContentFiltered) => "content_filtered".to_string(),
                                                    Some(claurst_api::provider_types::StopReason::EndTurn) => "end_turn".to_string(),
                                                    Some(claurst_api::provider_types::StopReason::Other(s)) => s.clone(),
                                                    None => "end_turn".to_string(),
                                                };
                                                if let Some(u) = u {
                                                    merge_provider_stream_usage(&mut usage, u);
                                                }
                                            }
                                            claurst_api::StreamEvent::MessageStop => {
                                                saw_message_stop = true;
                                                break;
                                            }
                                            claurst_api::StreamEvent::Error { error_type, message } => {
                                                error!(provider = %provider_id_str, error_type, message, "Provider in-band stream error");
                                                provider_stream_error = Some((
                                                    claurst_api::ProviderError::StreamError {
                                                        provider: provider.id().clone(),
                                                        message: format!("{error_type}: {message}"),
                                                        partial_response: None,
                                                    },
                                                    claurst_api::retry::classify_stream_error(error_type, message),
                                                ));
                                                break;
                                            }
                                            claurst_api::StreamEvent::RateLimits { primary_used_percent, secondary_used_percent } => {
                                                if let Some(ref tx) = event_tx {
                                                    let _ = tx.send(QueryEvent::RateLimits {
                                                        primary_used_percent: *primary_used_percent,
                                                        secondary_used_percent: *secondary_used_percent,
                                                    });
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // A stalled stream is a transient failure: retry the turn
                    // while the budget allows, never accept the partial output.
                    if provider_stream_stalled {
                        let reason = format!(
                            "no data for {}s",
                            provider_stall_timeout.as_secs()
                        );
                        retry_turn!(reason.clone(), None);
                        return QueryOutcome::Error(ClaudeError::Api(format!(
                            "Provider '{}' stream stalled (model '{}'): {}",
                            provider_id_str, model_id_str, reason
                        )));
                    }

                    // A mid-stream error means the accumulated text and
                    // tool-call JSON are incomplete/untrustworthy. Do NOT fall
                    // through to assemble and execute tools from a truncated
                    // stream (issue #215 — an Edit/Write could otherwise run
                    // with empty `{}` args). A context-window error compacts
                    // once and retries; other failures follow the shared
                    // retry policy, else surface a QueryOutcome::Error.
                    if let Some((err, decision)) = provider_stream_error {
                        if config.auto_compact
                            && !overflow_recovery_used
                            && provider_error_is_overflow(&err)
                        {
                            overflow_recovery_used = true;
                            if compact_after_overflow(
                                messages,
                                provider.as_ref(),
                                &model_id_str,
                                config.max_tokens,
                                event_tx.as_ref(),
                            )
                            .await
                            {
                                last_usage = None;
                                turn -= 1;
                                continue;
                            }
                        }
                        if let claurst_api::RetryDecision::Retry { after } = decision {
                            retry_turn!(err.to_string(), after);
                        }
                        error!(
                            provider = %provider_id_str,
                            model = %model_id_str,
                            error = %err,
                            "Provider stream error — not retryable or retries exhausted"
                        );
                        return QueryOutcome::Error(ClaudeError::Api(format!(
                            "Provider '{}' stream error (model '{}'): {}",
                            provider_id_str, model_id_str, err
                        )));
                    }
                    // Ended without `MessageStop`: truncated, never a finished turn.
                    if !saw_message_stop {
                        let reason = "stream ended before completion".to_string();
                        retry_turn!(reason.clone(), None);
                        return QueryOutcome::Error(ClaudeError::Api(format!(
                            "Provider '{}' {} (model '{}'); retries exhausted",
                            provider_id_str, reason, model_id_str
                        )));
                    }
                    retry_attempt = 0;

                    // Build the content blocks from accumulated stream data.
                    let mut content_blocks: Vec<ContentBlock> = Vec::new();

                    // Thinking / reasoning block — must come first so that
                    // inject_reasoning_for_tool_turns can find it later.
                    let combined_thinking = thinking_chunks.join("");
                    if !combined_thinking.is_empty() {
                        content_blocks.push(ContentBlock::Thinking {
                            thinking: combined_thinking.clone(),
                            signature: String::new(),
                        });
                    }
                    content_blocks.append(&mut redacted_thinking_blocks);

                    let combined_text = text_chunks.join("");
                    if !combined_text.is_empty() {
                        content_blocks.push(ContentBlock::Text { text: combined_text.clone() });
                    }

                    // Reconstruct tool-use blocks (sorted by index for determinism).
                    let mut tc_indices: Vec<usize> = tool_call_blocks.keys().cloned().collect();
                    tc_indices.sort();
                    // Tool calls whose accumulated JSON arguments failed to
                    // parse. We still emit a tool_use block (so the assistant
                    // message stays well-formed and every tool_use has a
                    // matching tool_result), but we must NOT execute the tool
                    // with empty/garbage input — instead we surface a tool
                    // error to the model so it can retry (issue #215).
                    let mut malformed_tool_calls: std::collections::HashMap<String, String> =
                        std::collections::HashMap::new();
                    for idx in tc_indices {
                        if let Some((id, name, json_str, thought_signature)) = tool_call_blocks.remove(&idx) {
                            let input = match claurst_api::tool_json::parse_tool_input(&json_str) {
                                Ok(v) => v,
                                Err(e) => {
                                    warn!(
                                        provider = %provider_id_str,
                                        tool = %name,
                                        tool_id = %id,
                                        error = %e,
                                        "Tool-call arguments failed to parse (truncated/malformed JSON); surfacing a tool error instead of executing with empty args"
                                    );
                                    malformed_tool_calls.insert(id.clone(), e.to_string());
                                    // Placeholder input — this call is never executed.
                                    serde_json::json!({})
                                }
                            };
                            content_blocks.push(ContentBlock::ToolUse { id, name, input, thought_signature });
                        }
                    }

                    let mut assistant_msg = Message {
                        role: claurst_core::types::Role::Assistant,
                        content: claurst_core::types::MessageContent::Blocks(content_blocks.clone()),
                        uuid: Some(msg_id),
                        cost: None,
                        snapshot_patch: None,
                    };

                    cost_tracker.add_usage(
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_creation_input_tokens,
                        usage.cache_read_input_tokens,
                    );
                    last_usage = Some(compact::UsageMark {
                        prompt_tokens: usage.total_input(),
                        message_count: messages.len(),
                    });
                    overflow_recovery_used = false;

                    messages.push(assistant_msg.clone());

                    // Handle tool-use turn: execute tools and loop.
                    let tool_use_blocks: Vec<_> = content_blocks.iter().filter_map(|b| {
                        if let ContentBlock::ToolUse { id, name, input, .. } = b {
                            Some((id.clone(), name.clone(), input.clone()))
                        } else {
                            None
                        }
                    }).collect();

                    // Execute tools if any tool_use blocks were returned.
                    // Note: we check the blocks themselves rather than relying
                    // solely on stop_str == "tool_use" because many OpenAI-
                    // compatible providers (Ollama, LM Studio, etc.) return
                    // finish_reason "stop" even when tool calls are present.
                    if !tool_use_blocks.is_empty() {
                        let mut tool_results = Vec::new();
                        for (tool_id, tool_name, tool_input) in tool_use_blocks {
                            // Notify TUI that a tool is starting (matches Anthropic path).
                            if let Some(ref tx) = event_tx {
                                let _ = tx.send(QueryEvent::ToolStart {
                                    tool_name: tool_name.clone(),
                                    tool_id: tool_id.clone(),
                                    input_json: tool_input.to_string(),
                                });
                            }
                            let result = if let Some(err) = malformed_tool_calls.get(&tool_id) {
                                // Never execute a tool whose arguments could not
                                // be parsed — return an error the model can see
                                // and recover from (issues #215, #439).
                                invalid_tool_input_result(&tool_name, err)
                            } else {
                                execute_tool(&tool_name, &tool_input, tools, tool_ctx).await
                            };
                            if let Some(ref tx) = event_tx {
                                let _ = tx.send(QueryEvent::ToolEnd {
                                    tool_name: tool_name.clone(),
                                    tool_id: tool_id.clone(),
                                    result: result.content.clone(),
                                    is_error: result.is_error,
                                });
                            }
                            let is_error = result.is_error;
                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: tool_id,
                                content: tool_result_for_history(
                                    result.content,
                                    result.images,
                                    config.tool_result_max_bytes,
                                ),
                                is_error: Some(is_error),
                            });
                        }
                        messages.push(Message {
                            role: claurst_core::types::Role::User,
                            content: claurst_core::types::MessageContent::Blocks(tool_results),
                            uuid: None,
                            cost: None,
                            snapshot_patch: None,
                        });
                        continue; // loop for next turn
                    }

                    // End turn — notify TUI and return.
                    // Issue #149 follow-up: providers occasionally end the
                    // turn after a tool round without emitting any text or
                    // tool calls, which left the user staring at a blank
                    // screen ("agent randomly stops"). Surface a placeholder
                    // so the user always sees *some* assistant output and
                    // knows the turn really ended.
                    if combined_text.is_empty() && combined_thinking.is_empty() {
                        let placeholder = format!(
                            "(no response — model ended the turn with stop_reason \"{}\")",
                            stop_str
                        );
                        if let Some(ref tx) = event_tx {
                            let _ = tx.send(QueryEvent::Stream(
                                AnthropicStreamEvent::ContentBlockDelta {
                                    index: 0,
                                    delta: claurst_api::streaming::ContentDelta::TextDelta { text: placeholder.clone() },
                                },
                            ));
                        }
                        if let claurst_core::types::MessageContent::Blocks(ref mut blocks) =
                            assistant_msg.content
                        {
                            blocks.push(ContentBlock::Text { text: placeholder.clone() });
                        }
                        if let Some(last) = messages.last_mut() {
                            *last = assistant_msg.clone();
                        }
                    }

                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(QueryEvent::TurnComplete {
                            stop_reason: stop_str.clone(),
                            turn,
                            usage: Some(usage.clone()),
                        });
                    }

                    // Attach snapshot patch covering all file changes this query.
                    if let (Some(ref snap), Some(ref hash)) = (&shadow_snap, &initial_snapshot) {
                        let patch = snap.patch(hash).await;
                        if !patch.files.is_empty() {
                            assistant_msg.snapshot_patch = Some(patch);
                        }
                    }

                    continue_or_end!(assistant_msg, usage);
                } else if provider_id_str != "anthropic" {
                    // Non-Anthropic provider detected but no API key / credentials
                    // available.  Return a clear error instead of silently falling
                    // through to the Anthropic client.
                    let hint = match provider_id_str.as_str() {
                        "google" => "Set GOOGLE_API_KEY or run `claurst auth login --provider google`.",
                        "openai" => "Set OPENAI_API_KEY or run `claurst auth login --provider openai`.",
                        "groq" => "Set GROQ_API_KEY.",
                        "mistral" => "Set MISTRAL_API_KEY.",
                        "deepseek" => "Set DEEPSEEK_API_KEY.",
                        "xai" => "Set XAI_API_KEY.",
                        "github-copilot" => "Reconnect GitHub Copilot via /connect, or set GITHUB_TOKEN.",
                        "cohere" => "Set COHERE_API_KEY.",
                        _ => "Set the appropriate API key environment variable or use `claurst auth login`.",
                    };
                    error!(
                        provider = %provider_id_str,
                        model = %model_id_str,
                        "No credentials found for provider"
                    );
                    return QueryOutcome::Error(
                        ClaudeError::Api(format!(
                            "No API key for provider '{}' (model '{}'). {}",
                            provider_id_str, model_id_str, hint
                        ))
                    );
                }
                // Anthropic with no auth_store key: fall through to the raw
                // client path below (which has its own deferred key validation
                // with detailed model-specific hints).
            }
        }

        // Send to API
        debug!(turn, model = %effective_model, "Sending API request");
        let connect = tokio::select! {
            _ = cancel_token.cancelled() => return QueryOutcome::Cancelled,
            r = client.create_message_stream_once(request, handler) => r,
        };
        let mut stream_rx = match connect {
            Ok(rx) => rx,
            Err(e) => {
                // Context-window error: compact once and retry, before the
                // transient-retry policy (#441, #439).
                if config.auto_compact
                    && !overflow_recovery_used
                    && claude_error_is_overflow(&e)
                {
                    overflow_recovery_used = true;
                    let summariser = anthropic_summariser(client);
                    if compact_after_overflow(
                        messages,
                        &summariser,
                        &effective_model,
                        config.max_tokens,
                        event_tx.as_ref(),
                    )
                    .await
                    {
                        last_usage = None;
                        turn -= 1;
                        continue;
                    }
                }
                if let claurst_api::RetryDecision::Retry { after } =
                    claurst_api::retry::classify_claude_error(&e)
                {
                    retry_turn!(e.to_string(), after);
                }
                // Retries exhausted on an overloaded / rate-limited model:
                // attempt one switch to the fallback model.
                let unavailable = matches!(
                    e,
                    ClaudeError::RateLimit | ClaudeError::ApiStatus { status: 429 | 529, .. }
                ) || e.to_string().to_lowercase().contains("overloaded");
                if !used_fallback && unavailable {
                    if let Some(ref fb) = config.fallback_model {
                        warn!(
                            primary = %effective_model,
                            fallback = %fb,
                            "Primary model unavailable — switching to fallback"
                        );
                        if let Some(ref tx) = event_tx {
                            let _ = tx.send(QueryEvent::Status(format!(
                                "Model unavailable — switching to fallback ({})",
                                fb
                            )));
                        }
                        effective_model = fb.clone();
                        used_fallback = true;
                        retry_attempt = 0;
                        turn -= 1; // don't count this attempt against max_turns
                        continue;
                    }
                }
                error!(error = %e, "API request failed");
                return QueryOutcome::Error(e);
            }
        };

        // Accumulate the streamed response.
        // A stall timeout auto-retries the request if no data arrives for 45s
        // (some providers are slow; we don't want to give up too early).
        const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
        let mut accumulator = StreamAccumulator::new();
        // Stream integrity (#439): an in-band `error` event or a stream that
        // ends without `message_stop` is a failed attempt, never `end_turn`.
        let mut stream_error: Option<(String, String)> = None;
        let mut saw_message_stop = false;
        let stall_deadline = tokio::time::sleep(STALL_TIMEOUT);
        tokio::pin!(stall_deadline);
        // Set when the stream reports a context-window error event.
        let mut stream_overflow = false;

        let stream_stalled = loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    return QueryOutcome::Cancelled;
                }
                _ = &mut stall_deadline => {
                    // No data for 45s — stall detected
                    break true;
                }
                event = stream_rx.recv() => {
                    // Reset stall timer on every received event.
                    stall_deadline.as_mut().reset(tokio::time::Instant::now() + STALL_TIMEOUT);
                    match event {
                        Some(evt) => {
                            accumulator.on_event(&evt);
                            match &evt {
                                AnthropicStreamEvent::Error { error_type, message } => {
                                    if error_type == "overloaded_error" {
                                        warn!(model = %effective_model, "API overloaded");
                                    }
                                    if claurst_api::is_context_overflow(message) {
                                        stream_overflow = true;
                                    }
                                    error!(error_type, message, "Stream error");
                                    stream_error = Some((error_type.clone(), message.clone()));
                                    break false;
                                }
                                AnthropicStreamEvent::MessageStop => {
                                    saw_message_stop = true;
                                    break false;
                                }
                                _ => {}
                            }
                        }
                        None => break false, // Stream ended
                    }
                }
            }
        };

        // A stalled stream is a transient failure: retry the turn while the
        // budget allows, never accept the partial output as a finished turn.
        if stream_stalled {
            let reason = format!("no data for {}s", STALL_TIMEOUT.as_secs());
            retry_turn!(reason.clone(), None);
            return QueryOutcome::Error(ClaudeError::Api(format!(
                "Stream stalled (model '{}'): {}",
                effective_model, reason
            )));
        }
        // A context-window error mid-stream: compact once and retry (before
        // the transient-retry policy); if that is not possible, surface it
        // rather than a partial turn.
        if stream_overflow {
            if config.auto_compact && !overflow_recovery_used {
                overflow_recovery_used = true;
                let summariser = anthropic_summariser(client);
                if compact_after_overflow(
                    messages,
                    &summariser,
                    &effective_model,
                    config.max_tokens,
                    event_tx.as_ref(),
                )
                .await
                {
                    last_usage = None;
                    turn -= 1;
                    continue;
                }
            }
            return QueryOutcome::Error(ClaudeError::ContextWindowExceeded);
        }
        if let Some((error_type, message)) = stream_error {
            let decision = claurst_api::retry::classify_stream_error(&error_type, &message);
            let reason = format!("{error_type}: {message}");
            if let claurst_api::RetryDecision::Retry { after } = decision {
                retry_turn!(reason.clone(), after);
            }
            return QueryOutcome::Error(ClaudeError::Api(format!(
                "Stream error (model '{}'): {}",
                effective_model, reason
            )));
        }
        if !saw_message_stop {
            let reason = "stream ended before message_stop".to_string();
            retry_turn!(reason.clone(), None);
            return QueryOutcome::Error(ClaudeError::Api(format!(
                "{} (model '{}'); retries exhausted",
                reason, effective_model
            )));
        }
        retry_attempt = 0;

        let invalid_tool_inputs = accumulator.take_invalid_tool_inputs();
        let (mut assistant_msg, usage, stop_reason) = accumulator.finish();

        // Track costs
        cost_tracker.add_usage(
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        );
        last_usage = Some(compact::UsageMark {
            prompt_tokens: usage.total_input(),
            message_count: messages.len(),
        });
        overflow_recovery_used = false;

        // Budget guard: abort the loop if the configured USD cap is exceeded.
        if let Some(limit) = config.max_budget_usd {
            let spent = cost_tracker.total_cost_usd();
            if spent >= limit {
                if let Some(ref tx) = event_tx {
                    let _ = tx.send(QueryEvent::Status(format!(
                        "Budget limit ${:.4} exceeded (spent ${:.4}) — stopping.",
                        limit, spent
                    )));
                }
                return QueryOutcome::BudgetExceeded {
                    cost_usd: spent,
                    limit_usd: limit,
                };
            }
        }

        // Append assistant message to conversation
        messages.push(assistant_msg.clone());

        // If the provider returned an unknown stop reason but the assistant
        // message contains tool_use blocks, treat it as tool_use so we don't
        // silently end the turn (issue #149: agent stops after tool call for
        // providers that emit non-standard finish reasons).
        let raw_stop = stop_reason.as_deref().unwrap_or("end_turn");
        let stop = match raw_stop {
            "end_turn" | "tool_use" | "max_tokens" | "stop_sequence" | "content_filtered" => raw_stop,
            _ if !assistant_msg.get_tool_use_blocks().is_empty() => {
                warn!(stop_reason = raw_stop, "Unknown stop reason with tool_use blocks present; treating as tool_use");
                "tool_use"
            }
            _ => raw_stop,
        };

        // T1-3: Fire PostModelTurn hooks after the model samples a response.
        // Hooks can inject blocking errors or veto continuation entirely.
        {
            let hook_result = fire_post_sampling_hooks(&assistant_msg, &tool_ctx.config);
            if !hook_result.blocking_errors.is_empty() {
                if hook_result.prevent_continuation {
                    // Hard veto: push the errors into the conversation and abort.
                    for err_msg in hook_result.blocking_errors {
                        messages.push(err_msg);
                    }
                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(QueryEvent::Status(
                            "PostModelTurn hook vetoed continuation.".to_string(),
                        ));
                    }
                    let last = messages
                        .last()
                        .cloned()
                        .unwrap_or_else(|| Message::assistant("Hook blocked continuation."));
                    return QueryOutcome::EndTurn {
                        message: last,
                        usage,
                    };
                }
                // Soft errors: inject them so the model can react next turn.
                for err_msg in hook_result.blocking_errors {
                    debug!("PostModelTurn hook injecting error message");
                    messages.push(err_msg);
                }
            }
        }

        if let Some(ref tx) = event_tx {
            let _ = tx.send(QueryEvent::TurnComplete {
                turn,
                stop_reason: stop.to_string(),
                usage: Some(usage.clone()),
            });
        }

        // Helper closure for firing the Stop hook.
        macro_rules! fire_stop_hook {
            ($msg:expr) => {{
                let stop_ctx = claurst_core::hooks::HookContext {
                    event: "Stop".to_string(),
                    tool_name: None,
                    tool_input: None,
                    tool_output: Some($msg.get_all_text()),
                    is_error: None,
                    session_id: Some(tool_ctx.session_id.clone()),
                };
                claurst_core::hooks::run_hooks(
                    &tool_ctx.config.hooks,
                    claurst_core::config::HookEvent::Stop,
                    &stop_ctx,
                    &tool_ctx.working_dir,
                )
                .await;
            }};
        }

        match stop {
            "end_turn" => {
                fire_stop_hook!(assistant_msg);

                // T1-3: Fire Stop hooks in background (fire-and-forget).
                // `stop_hooks_with_full_behavior` spawns blocking tasks internally
                // and returns immediately with an empty Vec.
                let _bg = stop_hooks_with_full_behavior(
                    &assistant_msg,
                    &tool_ctx.config,
                    tool_ctx.working_dir.clone(),
                );

                // Asynchronously extract and persist session memories if warranted.
                // Runs in a detached Tokio task so it doesn't block the query loop.
                if session_memory::SessionMemoryExtractor::should_extract(messages) {
                    let model_clone = config.model.clone();
                    let messages_clone = messages.clone();
                    let working_dir_clone = tool_ctx.working_dir.clone();

                    // Build a fresh client using the same API key.  This avoids
                    // requiring an Arc in the existing run_query_loop signature.
                    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
                        if !api_key.is_empty() {
                            if let Ok(sm_client) = claurst_api::AnthropicClient::new(
                                claurst_api::client::ClientConfig {
                                    api_key,
                                    ..Default::default()
                                },
                            ) {
                                let sm_client = std::sync::Arc::new(sm_client);
                                tokio::spawn(async move {
                                    let extractor =
                                        session_memory::SessionMemoryExtractor::new(&model_clone);
                                    match extractor
                                        .extract(&messages_clone, &working_dir_clone, &sm_client)
                                        .await
                                    {
                                        Ok(memories) if !memories.is_empty() => {
                                            let target = working_dir_clone
                                                .join(".claurst")
                                                .join("AGENTS.md");
                                            if let Err(e) =
                                                session_memory::SessionMemoryExtractor::persist(
                                                    &memories, &target,
                                                )
                                                .await
                                            {
                                                tracing::warn!(
                                                    error = %e,
                                                    "Failed to persist session memories"
                                                );
                                            }
                                        }
                                        Ok(_) => {} // no memories extracted
                                        Err(e) => {
                                            tracing::debug!(
                                                error = %e,
                                                "Session memory extraction failed (non-fatal)"
                                            );
                                        }
                                    }
                                });
                            }
                        }
                    }
                }

                // Trigger AutoDream consolidation check (non-blocking, best-effort).
                // maybe_trigger() checks gates + acquires lock. If it returns
                // Some(task), we spawn a background subagent via AgentTool so
                // the spawn doesn't call run_query_loop recursively from within
                // its own future (which would make the future !Send).
                {
                    let claurst_home = claurst_core::config::Settings::config_dir();
                    let memory_dir = Some(claurst_home.join("memory"));
                    let conversations_dir = Some(claurst_home.join("conversations"));
                    if let (Some(mem), Some(conv)) = (memory_dir, conversations_dir) {
                        let dreamer = crate::auto_dream::AutoDream::new(mem, conv);
                        if let Ok(Some(task)) = dreamer.maybe_trigger().await {
                            // Run the consolidation subagent in a background Tokio
                            // task. We use the AgentTool execute path (via
                            // poll_background_agent / BACKGROUND_AGENTS) to avoid
                            // re-entering run_query_loop from within the same
                            // future graph.
                            let agent_input = serde_json::json!({
                                "description": "memory consolidation",
                                "prompt": task.prompt,
                                "max_turns": 20,
                                "system_prompt": "You are performing automatic memory consolidation. Complete the task and return a brief summary.",
                                "run_in_background": true,
                                "isolation": null
                            });
                            let ctx_for_dream = tool_ctx.clone();
                            tokio::spawn(async move {
                                let agent = crate::agent_tool::AgentTool;
                                let _result = claurst_tools::Tool::execute(
                                    &agent,
                                    agent_input,
                                    &ctx_for_dream,
                                )
                                .await;
                                crate::auto_dream::AutoDream::finish_consolidation(&task).await;
                            });
                        }
                    }
                }

                // Attach snapshot patch covering all file changes this query.
                if let (Some(ref snap), Some(ref hash)) = (&shadow_snap, &initial_snapshot) {
                    let patch = snap.patch(hash).await;
                    if !patch.files.is_empty() {
                        assistant_msg.snapshot_patch = Some(patch);
                    }
                }

                continue_or_end!(assistant_msg, usage);
            }
            "max_tokens" => {
                // Mirror the TS recovery loop: inject a continuation nudge and
                // retry up to MAX_TOKENS_RECOVERY_LIMIT times before surfacing
                // the partial response as QueryOutcome::MaxTokens.
                if max_tokens_recovery_count < MAX_TOKENS_RECOVERY_LIMIT {
                    max_tokens_recovery_count += 1;
                    warn!(
                        attempt = max_tokens_recovery_count,
                        limit = MAX_TOKENS_RECOVERY_LIMIT,
                        "max_tokens hit — injecting continuation message (attempt {}/{})",
                        max_tokens_recovery_count,
                        MAX_TOKENS_RECOVERY_LIMIT,
                    );
                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(QueryEvent::Status(format!(
                            "Output token limit hit — continuing (attempt {}/{})",
                            max_tokens_recovery_count, MAX_TOKENS_RECOVERY_LIMIT
                        )));
                    }
                    // The partial assistant message must be in the history so
                    // the continuation makes sense to the model.
                    messages.push(Message::user(MAX_TOKENS_RECOVERY_MSG));
                    continue;
                }
                // Recovery exhausted — surface the partial response.
                warn!(
                    "max_tokens recovery exhausted after {} attempts",
                    MAX_TOKENS_RECOVERY_LIMIT
                );
                return QueryOutcome::MaxTokens {
                    partial_message: assistant_msg,
                    usage,
                };
            }
            "tool_use" => {
                // A completed tool-use turn counts as a successful recovery
                // boundary; reset the max_tokens retry counter.
                max_tokens_recovery_count = 0;
                // Extract tool calls and execute them
                let tool_blocks = assistant_msg.get_tool_use_blocks();
                if tool_blocks.is_empty() {
                    // Shouldn't happen but treat as end_turn
                    return QueryOutcome::EndTurn {
                        message: assistant_msg,
                        usage,
                    };
                }

                // ---------------------------------------------------------------------------
                // Streaming tool executor: parallel non-agent tool dispatch.
                //
                // Phase 1: Run PreToolUse hooks sequentially (they can block/deny execution
                //          and may display interactive permission dialogs).
                // Phase 2: Dispatch all non-blocked tool executions concurrently via
                //          futures::future::join_all, preserving original order.
                // Phase 3: Fire PostToolUse hooks + emit events, then collect results.
                //
                // This mirrors the TypeScript StreamingToolExecutor pattern.
                // ---------------------------------------------------------------------------

                // Intermediate record produced during Phase 1.
                struct PreparedTool {
                    id: String,
                    name: String,
                    input: Value,
                    /// None means the pre-hook blocked execution; the String is the error reason.
                    blocked_result: Option<ToolResult>,
                }

                // Phase 1: sequential pre-hook pass.
                let mut prepared: Vec<PreparedTool> = Vec::with_capacity(tool_blocks.len());
                for block in tool_blocks {
                    if let ContentBlock::ToolUse { id, name, input, .. } = block {
                        // Clone from the references returned by get_tool_use_blocks()
                        let id = id.clone();
                        let name = name.clone();
                        let input = input.clone();

                        if let Some(ref tx) = event_tx {
                            let _ = tx.send(QueryEvent::ToolStart {
                                tool_name: name.clone(),
                                tool_id: id.clone(),
                                input_json: input.to_string(),
                            });
                        }

                        // Invalid argument JSON (#439): answer with a tool
                        // error, never run the tool or its hooks with `{}`.
                        if let Some(err) = invalid_tool_inputs.get(&id) {
                            let blocked_result = Some(invalid_tool_input_result(&name, err));
                            prepared.push(PreparedTool { id, name, input, blocked_result });
                            continue;
                        }

                        let hooks = &tool_ctx.config.hooks;
                        let hook_ctx = claurst_core::hooks::HookContext {
                            event: "PreToolUse".to_string(),
                            tool_name: Some(name.clone()),
                            tool_input: Some(input.clone()),
                            tool_output: None,
                            is_error: None,
                            session_id: Some(tool_ctx.session_id.clone()),
                        };
                        let pre_outcome = claurst_core::hooks::run_hooks(
                            hooks,
                            claurst_core::config::HookEvent::PreToolUse,
                            &hook_ctx,
                            &tool_ctx.working_dir,
                        )
                        .await;

                        let plugin_pre_outcome =
                            claurst_plugins::run_global_pre_tool_hook(&name, &input);

                        let blocked_result =
                            if let claurst_core::hooks::HookOutcome::Blocked(reason) = pre_outcome {
                                warn!(tool = %name, reason = %reason, "PreToolUse hook blocked execution");
                                Some(claurst_tools::ToolResult::error(format!(
                                    "Blocked by hook: {}",
                                    reason
                                )))
                            } else if let claurst_plugins::HookOutcome::Deny(reason) = plugin_pre_outcome {
                                warn!(tool = %name, reason = %reason, "Plugin PreToolUse hook blocked execution");
                                Some(claurst_tools::ToolResult::error(format!(
                                    "Blocked by plugin hook: {}",
                                    reason
                                )))
                            } else {
                                None
                            };

                        prepared.push(PreparedTool {
                            id,
                            name,
                            input,
                            blocked_result,
                        });
                    }
                }

                // Phase 2: build execution futures for non-blocked tools and join them.
                // Blocked tools yield a ready future with the pre-computed error result.
                // Non-blocked tools execute concurrently via join_all.
                // Each async block owns its cloned name/input so there are no lifetime issues.
                let exec_futures: Vec<_> = prepared
                    .iter()
                    .map(|p| {
                        if p.blocked_result.is_some() {
                            let r = p.blocked_result.clone().unwrap();
                            futures::future::Either::Left(async move { r })
                        } else {
                            let name = p.name.clone();
                            let input = p.input.clone();
                            futures::future::Either::Right(async move {
                                execute_tool(&name, &input, tools, tool_ctx).await
                            })
                        }
                    })
                    .collect();

                // Run all tool futures concurrently, but race the batch against the
                // loop's cancel token (issue #218): on cancellation the in-flight
                // tools are abandoned promptly instead of blocking until the
                // slowest one finishes, and a cancelled ToolResult is synthesized
                // for EVERY tool so each tool_use still gets a matching tool_result
                // and the message history stays well-formed.
                let (exec_results, batch_cancelled) =
                    run_tool_batch(exec_futures, &tool_ctx.cancel_token).await;

                // Phase 3: post-hooks, event emission, and result block assembly.
                // When the batch was cancelled we skip the awaiting PostToolUse
                // hooks (they run external commands and would defeat the point of
                // returning promptly) but still emit ToolEnd + build every result
                // block so the conversation and TUI stay consistent.
                let mut result_blocks: Vec<ContentBlock> =
                    Vec::with_capacity(prepared.len());
                for (p, result) in prepared.iter().zip(exec_results) {
                    if !batch_cancelled {
                        let hooks = &tool_ctx.config.hooks;
                        let post_ctx = claurst_core::hooks::HookContext {
                            event: "PostToolUse".to_string(),
                            tool_name: Some(p.name.clone()),
                            tool_input: Some(p.input.clone()),
                            tool_output: Some(result.content.clone()),
                            is_error: Some(result.is_error),
                            session_id: Some(tool_ctx.session_id.clone()),
                        };
                        claurst_core::hooks::run_hooks(
                            hooks,
                            claurst_core::config::HookEvent::PostToolUse,
                            &post_ctx,
                            &tool_ctx.working_dir,
                        )
                        .await;

                        claurst_plugins::run_global_post_tool_hook(
                            &p.name,
                            &p.input,
                            &result.content,
                            result.is_error,
                        );
                    }

                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(QueryEvent::ToolEnd {
                            tool_name: p.name.clone(),
                            tool_id: p.id.clone(),
                            result: result.content.clone(),
                            is_error: result.is_error,
                        });
                    }

                    let is_error = result.is_error;
                    result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: p.id.clone(),
                        content: tool_result_for_history(
                            result.content,
                            result.images,
                            config.tool_result_max_bytes,
                        ),
                        is_error: if is_error { Some(true) } else { None },
                    });
                }

                // Append tool results as a user message so the history remains
                // valid (every tool_use is answered) even on cancellation.
                messages.push(Message::user_blocks(result_blocks));

                // If the batch was abandoned due to cancellation, stop the loop
                // now rather than sending the (cancelled) results back to the model.
                if batch_cancelled {
                    return QueryOutcome::Cancelled;
                }

                // Continue the loop to send results back to the model
                continue;
            }
            "stop_sequence" => {
                fire_stop_hook!(assistant_msg);
                let _bg = stop_hooks_with_full_behavior(
                    &assistant_msg,
                    &tool_ctx.config,
                    tool_ctx.working_dir.clone(),
                );
                if let (Some(ref snap), Some(ref hash)) = (&shadow_snap, &initial_snapshot) {
                    let patch = snap.patch(hash).await;
                    if !patch.files.is_empty() {
                        assistant_msg.snapshot_patch = Some(patch);
                    }
                }
                continue_or_end!(assistant_msg, usage);
            }
            other => {
                warn!(stop_reason = other, "Unknown stop reason, treating as end_turn");
                fire_stop_hook!(assistant_msg);
                let _bg = stop_hooks_with_full_behavior(
                    &assistant_msg,
                    &tool_ctx.config,
                    tool_ctx.working_dir.clone(),
                );
                if let (Some(ref snap), Some(ref hash)) = (&shadow_snap, &initial_snapshot) {
                    let patch = snap.patch(hash).await;
                    if !patch.files.is_empty() {
                        assistant_msg.snapshot_patch = Some(patch);
                    }
                }
                continue_or_end!(assistant_msg, usage);
            }
        }
    }
}

/// Stream handler that forwards events to an unbounded channel.
struct ChannelStreamHandler {
    tx: mpsc::UnboundedSender<QueryEvent>,
}

impl StreamHandler for ChannelStreamHandler {
    fn on_event(&self, event: &AnthropicStreamEvent) {
        let _ = self.tx.send(QueryEvent::Stream(event.clone()));
    }
}

// ---------------------------------------------------------------------------
// Provider stream event mapping
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_api::SystemPrompt;

    #[test]
    fn final_stream_usage_supplies_prompt_tokens_to_turn_usage() {
        let mut turn_usage = UsageInfo::default();
        let final_usage = UsageInfo {
            input_tokens: 1_200,
            output_tokens: 80,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 300,
        };

        merge_provider_stream_usage(&mut turn_usage, &final_usage);

        assert_eq!(turn_usage.input_tokens, 1_200);
        assert_eq!(turn_usage.cache_read_input_tokens, 300);
        assert_eq!(turn_usage.output_tokens, 80);
        let context_counter_increment = turn_usage.input_tokens
            + turn_usage.output_tokens
            + turn_usage.cache_creation_input_tokens
            + turn_usage.cache_read_input_tokens;
        assert_eq!(context_counter_increment, 1_580);
    }

    #[test]
    fn output_only_final_usage_preserves_start_input_tokens() {
        let mut turn_usage = UsageInfo {
            input_tokens: 900,
            output_tokens: 0,
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 0,
        };
        let final_usage = UsageInfo {
            output_tokens: 75,
            ..Default::default()
        };

        merge_provider_stream_usage(&mut turn_usage, &final_usage);

        assert_eq!(turn_usage.input_tokens, 900);
        assert_eq!(turn_usage.cache_creation_input_tokens, 100);
        assert_eq!(turn_usage.output_tokens, 75);
    }

    #[test]
    fn from_config_carries_effort_level_through() {
        let cfg = claurst_core::config::Config {
            effort: Some("high".to_string()),
            ..Default::default()
        };
        let query_config = QueryConfig::from_config(&cfg);
        assert_eq!(query_config.effort_level, Some(claurst_core::effort::EffortLevel::High));
    }

    #[test]
    fn from_config_unset_effort_is_none() {
        let cfg = claurst_core::config::Config::default();
        let query_config = QueryConfig::from_config(&cfg);
        assert_eq!(query_config.effort_level, None);
    }

    fn make_config(sys: Option<&str>, append: Option<&str>) -> QueryConfig {
        QueryConfig {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 4096,
            max_turns: 10,
            max_turns_override: None,
            degradation_enabled: true,
            system_prompt: sys.map(String::from),
            append_system_prompt: append.map(String::from),
            session_context: None,
            output_style: claurst_core::system_prompt::OutputStyle::Default,
            output_style_prompt: None,
            working_directory: None,
            available_skills: Vec::new(),
            instruction_files: Vec::new(),
            thinking_budget: None,
            temperature: None,
            tool_result_max_bytes: DEFAULT_TOOL_RESULT_MAX_BYTES,
            effort_level: None,
            command_queue: None,
            max_budget_usd: None,
            fallback_model: None,
            provider_registry: None,
            agent_name: None,
            agent_definition: None,
            model_registry: None,
            managed_agents: None,
            enabled_tools: None,
            permission_mode: None,
            additional_dirs: Vec::new(),
            continuation: crate::continuation::ContinuationMode::Default,
            retry: claurst_api::RetryConfig::default(),
            auto_compact: true,
        }
    }

    // ---- tool-argument parsing tests (issues #215, #439) ---------------------------------

    #[test]
    fn test_parse_tool_args_valid_object() {
        // A complete JSON object parses to the same value.
        let v = claurst_api::tool_json::parse_tool_input("{\"a\":1}").expect("valid JSON should parse");
        assert_eq!(v, serde_json::json!({ "a": 1 }));

        let v = claurst_api::tool_json::parse_tool_input("{\"path\": \"/tmp/x\", \"content\": \"hi\"}")
            .expect("valid JSON should parse");
        assert_eq!(v["path"], "/tmp/x");
        assert_eq!(v["content"], "hi");
    }

    #[test]
    fn test_parse_tool_args_empty_is_empty_object() {
        // No-argument tool calls arrive as an empty (or whitespace-only)
        // buffer and must map to `{}` so the happy path still works.
        assert_eq!(claurst_api::tool_json::parse_tool_input("").unwrap(), serde_json::json!({}));
        assert_eq!(claurst_api::tool_json::parse_tool_input("   ").unwrap(), serde_json::json!({}));
        assert_eq!(claurst_api::tool_json::parse_tool_input("\n\t ").unwrap(), serde_json::json!({}));
    }

    #[test]
    fn test_parse_tool_args_truncated_is_error_not_empty_object() {
        // The core of issue #215: a truncated/malformed stream must surface
        // an error, NOT silently become `{}` (which would run Edit/Write with
        // empty arguments).
        assert!(
            claurst_api::tool_json::parse_tool_input("{\"a\":").is_err(),
            "truncated JSON must be an error"
        );
        assert!(
            claurst_api::tool_json::parse_tool_input("{\"path\": \"/etc/passwd").is_err(),
            "truncated string value must be an error"
        );
        assert!(
            claurst_api::tool_json::parse_tool_input("{not json}").is_err(),
            "invalid JSON must be an error"
        );

        // Regression guard: the failing cases must never resolve to `{}`.
        for bad in ["{\"a\":", "{\"path\": \"/etc/passwd", "{not json}"] {
            let resolved = claurst_api::tool_json::parse_tool_input(bad).unwrap_or(serde_json::json!({}));
            // The OLD buggy behavior turned these into `{}`; assert we now
            // *detect* the error rather than relying on that fallback.
            assert!(
                claurst_api::tool_json::parse_tool_input(bad).is_err(),
                "expected error for {:?}, but got {}",
                bad,
                resolved
            );
        }
    }

    // ---- build_system_prompt tests ------------------------------------------

    /// Unwrap the `[stable, dynamic]` system blocks and assert that the
    /// internal boundary marker reaches neither of them.
    fn prompt_halves(prompt: SystemPrompt) -> (String, String) {
        let SystemPrompt::Blocks(blocks) = prompt else {
            panic!("expected [stable, dynamic] system blocks");
        };
        assert_eq!(blocks.len(), 2, "expected exactly two system blocks");
        for block in &blocks {
            assert!(
                !block.text.contains(claurst_core::system_prompt::SYSTEM_PROMPT_DYNAMIC_BOUNDARY),
                "the boundary marker must never be sent to the model"
            );
        }
        (blocks[0].text.clone(), blocks[1].text.clone())
    }

    #[test]
    fn test_system_prompt_default_when_empty() {
        // The default prompt (no custom system prompt set) should include the
        // Claurst attribution and standard sections in the stable block.
        let cfg = make_config(None, None);
        let (stable, _dynamic) = prompt_halves(build_system_prompt(&cfg));
        assert!(
            stable.contains("Claurst") || stable.contains("Claude agent"),
            "Default prompt should contain attribution: {}",
            stable
        );
    }

    #[test]
    fn test_system_prompt_with_custom() {
        // A custom system prompt is injected into the stable block as
        // <custom_instructions>; the default sections are still present.
        let cfg = make_config(Some("You are a code reviewer."), None);
        let (stable, _dynamic) = prompt_halves(build_system_prompt(&cfg));
        assert!(
            stable.contains("You are a code reviewer."),
            "Custom prompt text should appear in the stable block"
        );
        assert!(
            stable.contains("Claurst") || stable.contains("Claude agent"),
            "Default attribution should still be present"
        );
    }

    #[test]
    fn test_system_prompt_with_append() {
        // Appended text lands in the dynamic block.
        let cfg = make_config(Some("Base prompt."), Some("Additional context."));
        let (stable, dynamic) = prompt_halves(build_system_prompt(&cfg));
        assert!(stable.contains("Base prompt."));
        assert!(!stable.contains("Additional context."));
        assert!(dynamic.contains("Additional context."));
    }

    #[test]
    fn test_system_prompt_append_only() {
        // When only append is set, default sections are present plus the
        // appended text in the dynamic block.
        let cfg = make_config(None, Some("Appended text."));
        let (stable, dynamic) = prompt_halves(build_system_prompt(&cfg));
        assert!(!stable.contains("Appended text."));
        assert!(
            dynamic.contains("Appended text."),
            "Appended text must appear in the dynamic block"
        );
    }

    #[test]
    fn test_system_prompt_with_custom_output_style_prompt() {
        let mut cfg = make_config(None, None);
        cfg.output_style_prompt = Some("Answer like a pirate.".to_string());
        let (stable, dynamic) = prompt_halves(build_system_prompt(&cfg));
        assert!(!stable.contains("Answer like a pirate."));
        assert!(dynamic.contains("Answer like a pirate."));
    }

    #[test]
    fn permission_mode_change_only_touches_the_dynamic_block() {
        use claurst_core::config::PermissionMode;
        let mut cfg = make_config(Some("Project rules."), None);
        cfg.enabled_tools = Some(vec!["Bash".to_string(), "Edit".to_string()]);
        cfg.permission_mode = Some(PermissionMode::Default);
        let (stable_default, dynamic_default) = prompt_halves(build_system_prompt(&cfg));
        cfg.permission_mode = Some(PermissionMode::Plan);
        let (stable_plan, dynamic_plan) = prompt_halves(build_system_prompt(&cfg));

        assert_eq!(stable_default, stable_plan, "the cached block must stay byte-identical");
        assert!(dynamic_default.contains("Current permission mode: `default`"));
        assert!(dynamic_plan.contains("Current permission mode: `plan`"));
        assert!(!dynamic_plan.contains("`default`"));
    }

    // ---- QueryConfig tests --------------------------------------------------

    #[test]
    fn test_query_config_clone() {
        let cfg = make_config(Some("test"), Some("append"));
        let cloned = cfg.clone();
        assert_eq!(cloned.model, "claude-sonnet-4-6");
        assert_eq!(cloned.max_tokens, 4096);
        assert_eq!(cloned.system_prompt, Some("test".to_string()));
    }

    // ---- QueryOutcome variant tests -----------------------------------------

    #[test]
    fn test_query_outcome_debug() {
        // Ensure the enum variants can be created and debug-formatted
        let outcome = QueryOutcome::Cancelled;
        let s = format!("{:?}", outcome);
        assert!(s.contains("Cancelled"));

        let err_outcome = QueryOutcome::Error(claurst_core::error::ClaudeError::RateLimit);
        let s2 = format!("{:?}", err_outcome);
        assert!(s2.contains("Error"));
    }

    #[test]
    fn test_build_provider_options_for_google_gemini_3() {
        let options = build_provider_options(
            "google",
            "gemini-3-flash-preview",
            Some(claurst_core::effort::EffortLevel::High),
            None,
        );
        assert_eq!(
            options["thinkingConfig"]["thinkingLevel"],
            serde_json::json!("high")
        );
        assert_eq!(
            options["thinkingConfig"]["includeThoughts"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn test_build_provider_options_for_openrouter_gpt5() {
        let options = build_provider_options(
            "openrouter",
            "gpt-5.4",
            Some(claurst_core::effort::EffortLevel::Medium),
            None,
        );
        assert_eq!(options["reasoningEffort"], serde_json::json!("medium"));
        assert_eq!(options["textVerbosity"], serde_json::json!("low"));
        assert_eq!(options["usage"]["include"], serde_json::json!(true));
    }

    #[test]
    fn test_build_provider_options_codex_effort_ladder() {
        // Codex maps the lower tiers like any OpenAI reasoning model...
        for (level, expected) in [
            (claurst_core::effort::EffortLevel::Low, "low"),
            (claurst_core::effort::EffortLevel::Medium, "medium"),
            (claurst_core::effort::EffortLevel::High, "high"),
        ] {
            let options = build_provider_options("openai-codex", "gpt-5.5", Some(level), None);
            assert_eq!(options["reasoningEffort"], serde_json::json!(expected));
        }
        // ...but the top "Max" tier becomes "xhigh" (extra high) on Codex.
        let options = build_provider_options(
            "openai-codex",
            "gpt-5.5",
            Some(claurst_core::effort::EffortLevel::Max),
            None,
        );
        assert_eq!(options["reasoningEffort"], serde_json::json!("xhigh"));
        assert_eq!(options["reasoningSummary"], serde_json::json!("auto"));

        // Other OpenAI-compatible providers keep "high" for Max (no xhigh).
        let other = build_provider_options(
            "openrouter",
            "gpt-5.4",
            Some(claurst_core::effort::EffortLevel::Max),
            None,
        );
        assert_eq!(other["reasoningEffort"], serde_json::json!("high"));
    }

    #[test]
    fn test_build_provider_options_for_bedrock_anthropic() {
        let options = build_provider_options(
            "amazon-bedrock",
            "anthropic.claude-sonnet-4-6-v1",
            Some(claurst_core::effort::EffortLevel::High),
            Some(10_000),
        );
        assert_eq!(
            options["reasoningConfig"]["budgetTokens"],
            serde_json::json!(10_000)
        );
    }

    #[test]
    fn test_alibaba_is_openaiish_provider() {
        // "alibaba" is an alias for "qwen" (Alibaba's DashScope backend);
        // both must be treated as OpenAI-compatible providers.
        assert!(is_openaiish_provider("alibaba"));
        assert!(is_openaiish_provider("qwen"));
    }

    // ---- apply_compact_result / #213 data-loss guard ------------------------

    fn sample_conversation() -> Vec<Message> {
        vec![
            Message::user("initial user request"),
            Message::assistant("assistant reply with important context"),
            Message::user("follow-up question"),
            Message::assistant("second assistant reply"),
        ]
    }

    fn texts(messages: &[Message]) -> Vec<String> {
        messages.iter().map(|m| m.get_all_text()).collect()
    }

    #[test]
    fn failed_compaction_preserves_messages() {
        // Regression test for #213: a failed compaction must NOT wipe the
        // conversation. Previously the reactive path drained `messages` with
        // std::mem::take and never restored them on error.
        let mut messages = sample_conversation();
        let before = texts(&messages);

        // Simulate a failed reactive_compact / context_collapse (API error,
        // Cancelled, empty summary all map to Err here).
        let outcome: Result<compact::CompactResult, ClaudeError> =
            Err(ClaudeError::Cancelled);
        let result = apply_compact_result(&mut messages, outcome);

        assert!(result.is_err(), "helper must surface the compaction error");
        assert_eq!(
            messages.len(),
            before.len(),
            "messages must not be emptied on failed compaction"
        );
        assert_eq!(
            texts(&messages),
            before,
            "message contents must be identical after failed compaction"
        );
    }

    #[test]
    fn failed_compaction_with_generic_error_preserves_messages() {
        // The helper is generic over the error type; any Err leaves messages
        // untouched.
        let mut messages = sample_conversation();
        let before = texts(&messages);

        let outcome: Result<compact::CompactResult, &str> = Err("empty summary");
        let result = apply_compact_result(&mut messages, outcome);

        assert_eq!(result, Err("empty summary"));
        assert_eq!(texts(&messages), before);
    }

    #[test]
    fn successful_compaction_replaces_messages() {
        // On success the compacted result replaces the live messages and the
        // freed-token count is returned.
        let mut messages = sample_conversation();
        let compacted = vec![
            Message::user("[summary of earlier conversation]"),
            Message::user("follow-up question"),
        ];
        let expected = texts(&compacted);

        let outcome: Result<compact::CompactResult, ClaudeError> = Ok(compact::CompactResult {
            messages: compacted,
            summary: "[summary of earlier conversation]".to_string(),
            tokens_freed: 4_096,
        });
        let result = apply_compact_result(&mut messages, outcome);

        assert_eq!(
            result.unwrap(),
            4_096,
            "tokens_freed must be surfaced on success"
        );
        assert_eq!(
            texts(&messages),
            expected,
            "messages must be replaced with the compacted result on success"
        );
    }

    // ---- Central permission backstop (issue #210) ---------------------------
    //
    // These tests pin the `execute_tool` backstop contract:
    //  (a) a non-self-gating tool at a gated level is DENIED (never executes)
    //      when the handler denies;
    //  (b) a self-gating tool is NOT gated centrally (no double-prompt) — its
    //      execute() runs even though the handler would deny;
    //  (c) a ReadOnly / None tool is never gated centrally.

    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    /// Permission handler that denies everything (returns `Ask`, which in a
    /// non-interactive context surfaces as a hard denial).
    struct DenyAllHandler;
    impl claurst_core::permissions::PermissionHandler for DenyAllHandler {
        fn check_permission(
            &self,
            _request: &claurst_core::permissions::PermissionRequest,
        ) -> claurst_core::permissions::PermissionDecision {
            claurst_core::permissions::PermissionDecision::Ask {
                reason: "denied by test handler".to_string(),
            }
        }
        fn request_permission(
            &self,
            request: &claurst_core::permissions::PermissionRequest,
        ) -> claurst_core::permissions::PermissionDecision {
            self.check_permission(request)
        }
    }

    /// A configurable mock tool that records whether its `execute()` ran.
    struct MockTool {
        name: &'static str,
        level: PermissionLevel,
        self_gates: bool,
        ran: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str { self.name }
        fn description(&self) -> &str { "mock tool for backstop tests" }
        fn permission_level(&self) -> PermissionLevel { self.level }
        fn self_gates(&self) -> bool { self.self_gates }
        fn input_schema(&self) -> Value { serde_json::json!({"type": "object"}) }
        async fn execute(&self, _input: Value, _ctx: &ToolContext) -> ToolResult {
            self.ran.store(true, AtomicOrdering::SeqCst);
            ToolResult::success("mock ran")
        }
    }

    fn deny_all_context() -> ToolContext {
        ToolContext {
            working_dir: std::path::PathBuf::from("/workspace"),
            permission_mode: claurst_core::config::PermissionMode::Default,
            permission_handler: Arc::new(DenyAllHandler),
            cost_tracker: claurst_core::cost::CostTracker::new(),
            session_id: "backstop-test".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                claurst_core::file_history::FileHistory::new(),
            )),
            current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config: claurst_core::config::Config::default(),
            managed_agent_config: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// (a) A tool that does NOT self-gate and requires a gated level (Execute)
    /// is blocked by the central backstop when the handler denies — and its
    /// `execute()` never runs.
    #[tokio::test]
    async fn backstop_denies_non_self_gating_gated_tool() {
        let ran = Arc::new(AtomicBool::new(false));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool {
            name: "MockExec",
            level: PermissionLevel::Execute,
            self_gates: false,
            ran: ran.clone(),
        })];
        let ctx = deny_all_context();

        let result = execute_tool("MockExec", &serde_json::json!({}), &tools, &ctx).await;

        assert!(result.is_error, "central backstop must block a denied tool");
        assert!(
            !ran.load(AtomicOrdering::SeqCst),
            "execute() must NOT run when the backstop denies"
        );
    }

    /// (b) A self-gating tool is NOT gated by the central backstop (no double
    /// prompt): even with a deny-all handler, its `execute()` still runs
    /// because the central gate is skipped for self-gaters.
    #[tokio::test]
    async fn backstop_skips_self_gating_tool() {
        let ran = Arc::new(AtomicBool::new(false));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool {
            name: "MockSelfGated",
            level: PermissionLevel::Execute,
            self_gates: true,
            ran: ran.clone(),
        })];
        let ctx = deny_all_context();

        let result = execute_tool("MockSelfGated", &serde_json::json!({}), &tools, &ctx).await;

        assert!(
            !result.is_error,
            "self-gating tool must not be blocked by the central backstop"
        );
        assert_eq!(result.content, "mock ran");
        assert!(
            ran.load(AtomicOrdering::SeqCst),
            "self-gating tool's execute() must run (central gate skipped)"
        );
    }

    /// (c) ReadOnly and None tools are never gated centrally, so they run even
    /// under a deny-all handler.
    #[tokio::test]
    async fn backstop_skips_read_only_and_none_tools() {
        for level in [PermissionLevel::ReadOnly, PermissionLevel::None] {
            let ran = Arc::new(AtomicBool::new(false));
            let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool {
                name: "MockSafe",
                level,
                self_gates: false,
                ran: ran.clone(),
            })];
            let ctx = deny_all_context();

            let result = execute_tool("MockSafe", &serde_json::json!({}), &tools, &ctx).await;

            assert!(
                !result.is_error,
                "{:?} tool must not be gated centrally",
                level
            );
            assert!(
                ran.load(AtomicOrdering::SeqCst),
                "{:?} tool's execute() must run",
                level
            );
        }
    }

    #[test]
    fn backstop_permission_level_gating_matrix() {
        assert!(!permission_level_is_gated(PermissionLevel::None));
        assert!(!permission_level_is_gated(PermissionLevel::ReadOnly));
        assert!(permission_level_is_gated(PermissionLevel::Write));
        assert!(permission_level_is_gated(PermissionLevel::Execute));
        assert!(permission_level_is_gated(PermissionLevel::Dangerous));
        assert!(permission_level_is_gated(PermissionLevel::Forbidden));
    }

    // ---- Issue #218: cancellation plumbing ---------------------------------

    /// (a) The parallel tool executor (`run_tool_batch`, the exact code the query
    /// loop runs) must abandon a long-running tool the moment the cancel token
    /// fires: with a tool future that never completes and a pre-cancelled token,
    /// the batch returns promptly instead of blocking, reports cancellation, and
    /// still yields one cancelled `ToolResult` per tool so every `tool_use` can
    /// be answered and the message history stays valid.
    #[tokio::test]
    async fn executor_abandons_in_flight_tools_on_cancel() {
        use std::future::Future;
        use std::pin::Pin;

        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel(); // pre-cancelled

        // Two tool futures: one that never completes (a long-running tool) and
        // one that would succeed. Boxed so they share a concrete type.
        let never: Pin<Box<dyn Future<Output = ToolResult> + Send>> =
            Box::pin(std::future::pending());
        let quick: Pin<Box<dyn Future<Output = ToolResult> + Send>> =
            Box::pin(async { ToolResult::success("done") });

        // If the executor blocked on the never-completing tool this would time
        // out; it must return promptly instead.
        let (results, cancelled) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_tool_batch(vec![never, quick], &cancel),
        )
        .await
        .expect("executor must return promptly, not block on the pending tool");

        assert!(cancelled, "batch must report that it was cancelled");
        assert_eq!(
            results.len(),
            2,
            "every tool_use must still receive a tool_result"
        );
        assert!(
            results.iter().all(|r| r.is_error),
            "cancelled tool results are errors"
        );
        assert!(
            results[0].content.contains("cancelled"),
            "cancelled result should say so, got: {}",
            results[0].content
        );
    }

    /// The happy path is unchanged: with a live (never-cancelled) token the batch
    /// runs the futures to completion and returns their real results in order.
    #[tokio::test]
    async fn executor_runs_to_completion_without_cancel() {
        let cancel = tokio_util::sync::CancellationToken::new();
        // `std::future::ready` gives both futures the same concrete type so they
        // share a Vec (mirroring the Either-unified futures the real loop builds).
        let f1 = std::future::ready(ToolResult::success("a"));
        let f2 = std::future::ready(ToolResult::error("b"));

        let (results, cancelled) = run_tool_batch(vec![f1, f2], &cancel).await;

        assert!(!cancelled, "no cancellation should have occurred");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content, "a");
        assert!(!results[0].is_error);
        assert_eq!(results[1].content, "b");
        assert!(results[1].is_error);
    }

    /// (b) A sub-agent receives a CHILD of the parent's cancel token — exactly
    /// how `AgentTool` derives it from `ctx.cancel_token` — so cancelling the
    /// parent query propagates into the sub-agent. `ToolContext` now exposes the
    /// token, and cancelling it must flip the child.
    #[test]
    fn subagent_child_token_propagates_parent_cancel() {
        let ctx = deny_all_context();
        // AgentTool spawns each sub-agent with a token derived exactly this way.
        let child = ctx.cancel_token.child_token();

        assert!(!child.is_cancelled(), "child starts live");
        ctx.cancel_token.cancel();
        assert!(
            child.is_cancelled(),
            "cancelling the parent's token must cancel the sub-agent's child token"
        );
    }

    // ---- Issue #230 (MI-3): in-loop continuation + max-steps degradation -----

    use std::sync::Mutex as StdMutex;

    /// A provider double that records, per request, whether the tool set was
    /// empty (i.e. tools were disabled — the max-steps degradation turn) and
    /// replays a scripted response. Drives `run_query_loop` end-to-end.
    struct RecordingProvider {
        id: claurst_core::provider_id::ProviderId,
        /// One entry per request: `true` when its tool set was empty.
        tools_empty_per_request: Arc<StdMutex<Vec<bool>>>,
        /// One entry per request: the system prompt it carried.
        systems_per_request: Arc<StdMutex<Vec<Option<SystemPrompt>>>>,
        /// When true, always end the turn with text (ignores tools). Otherwise
        /// emit a `tool_use` while tools are present and end the turn once
        /// they're gone (so the degradation turn ends the loop).
        always_end_turn: bool,
    }

    #[async_trait::async_trait]
    impl claurst_api::LlmProvider for RecordingProvider {
        fn id(&self) -> &claurst_core::provider_id::ProviderId {
            &self.id
        }
        fn name(&self) -> &str {
            "recording-mock"
        }

        async fn create_message(
            &self,
            _request: claurst_api::ProviderRequest,
        ) -> Result<claurst_api::ProviderResponse, claurst_api::ProviderError> {
            unimplemented!("these tests only use create_message_stream")
        }

        async fn create_message_stream(
            &self,
            request: claurst_api::ProviderRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<claurst_api::StreamEvent, claurst_api::ProviderError>,
                        > + Send,
                >,
            >,
            claurst_api::ProviderError,
        > {
            use claurst_api::provider_types::StopReason;
            use claurst_api::StreamEvent;

            let tools_empty = request.tools.is_empty();
            self.tools_empty_per_request
                .lock()
                .unwrap()
                .push(tools_empty);
            self.systems_per_request
                .lock()
                .unwrap()
                .push(request.system_prompt.clone());

            let msg_id = uuid::Uuid::new_v4().to_string();
            let emit_tool_use = !self.always_end_turn && !tools_empty;

            let events: Vec<Result<StreamEvent, claurst_api::ProviderError>> = if emit_tool_use {
                let tool_id = uuid::Uuid::new_v4().to_string();
                vec![
                    Ok(StreamEvent::MessageStart {
                        id: msg_id,
                        model: "mock-model".to_string(),
                        usage: UsageInfo::default(),
                    }),
                    Ok(StreamEvent::ContentBlockStart {
                        index: 0,
                        content_block: ContentBlock::ToolUse {
                            id: tool_id,
                            name: "noop_tool".to_string(),
                            input: serde_json::json!({}),
                            thought_signature: None,
                        },
                    }),
                    Ok(StreamEvent::InputJsonDelta {
                        index: 0,
                        partial_json: "{}".to_string(),
                    }),
                    Ok(StreamEvent::MessageDelta {
                        stop_reason: Some(StopReason::ToolUse),
                        usage: Some(UsageInfo::default()),
                    }),
                    Ok(StreamEvent::MessageStop),
                ]
            } else {
                vec![
                    Ok(StreamEvent::MessageStart {
                        id: msg_id,
                        model: "mock-model".to_string(),
                        usage: UsageInfo::default(),
                    }),
                    Ok(StreamEvent::TextDelta {
                        index: 0,
                        text: "Progress summary.".to_string(),
                    }),
                    Ok(StreamEvent::MessageDelta {
                        stop_reason: Some(StopReason::EndTurn),
                        usage: Some(UsageInfo::default()),
                    }),
                    Ok(StreamEvent::MessageStop),
                ]
            };

            Ok(Box::pin(futures::stream::iter(events)))
        }

        async fn health_check(
            &self,
        ) -> Result<claurst_api::ProviderStatus, claurst_api::ProviderError> {
            Ok(claurst_api::ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> claurst_api::ProviderCapabilities {
            claurst_api::ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: claurst_api::SystemPromptStyle::TopLevel,
            }
        }
    }

    fn noop_tools() -> Vec<Box<dyn Tool>> {
        vec![Box::new(MockTool {
            name: "noop_tool",
            level: PermissionLevel::ReadOnly,
            self_gates: false,
            ran: Arc::new(AtomicBool::new(false)),
        })]
    }

    /// Drive `run_query_loop` against the recording provider. Returns the
    /// outcome, the per-request "tools were empty" record, and the final
    /// message history.
    async fn drive_loop_with_mock(
        always_end_turn: bool,
        max_turns: u32,
        tools: Vec<Box<dyn Tool>>,
        continuation: crate::continuation::ContinuationMode,
    ) -> (QueryOutcome, Vec<bool>, Vec<Message>) {
        drive_loop_with_options(
            always_end_turn,
            max_turns,
            tools,
            continuation,
            true,
            None,
            None,
        )
        .await
    }

    /// Same as [`drive_loop_with_mock`], plus explicit control over the
    /// `degradation_enabled` flag on `QueryConfig`, the session-level
    /// `max_turns_override`, and the agent definition's `max_turns` (the
    /// latter two feed `effective_max_turns`).
    async fn drive_loop_with_options(
        always_end_turn: bool,
        max_turns: u32,
        tools: Vec<Box<dyn Tool>>,
        continuation: crate::continuation::ContinuationMode,
        degradation_enabled: bool,
        max_turns_override: Option<u32>,
        agent_max_turns: Option<u32>,
    ) -> (QueryOutcome, Vec<bool>, Vec<Message>) {
        let recorded = Arc::new(StdMutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider {
            id: claurst_core::provider_id::ProviderId::new("mockprov"),
            tools_empty_per_request: recorded.clone(),
            systems_per_request: Arc::new(StdMutex::new(Vec::new())),
            always_end_turn,
        });
        let mut registry = claurst_api::ProviderRegistry::new();
        registry.register(provider);
        let registry = Arc::new(registry);

        let client = claurst_api::AnthropicClient::new(claurst_api::client::ClientConfig {
            api_key: "test-key".to_string(),
            ..Default::default()
        })
        .expect("build test client");

        let mut ctx = deny_all_context();
        ctx.session_id = "loop-test".to_string();
        ctx.config.provider = Some("mockprov".to_string());

        let mut config = make_config(None, None);
        config.model = "mock-model".to_string();
        config.max_turns = max_turns;
        config.degradation_enabled = degradation_enabled;
        config.max_turns_override = max_turns_override;
        if let Some(agent_turns) = agent_max_turns {
            config.agent_definition = Some(claurst_core::AgentDefinition {
                max_turns: Some(agent_turns),
                ..Default::default()
            });
        }
        config.provider_registry = Some(registry);
        config.continuation = continuation;

        let cost = claurst_core::cost::CostTracker::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut messages = vec![Message::user("start")];

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_query_loop(
                &client,
                &mut messages,
                &tools,
                &ctx,
                &config,
                cost,
                None,
                cancel,
                None,
            ),
        )
        .await
        .expect("loop must not hang");

        let recorded = recorded.lock().unwrap().clone();
        (outcome, recorded, messages)
    }

    /// Issue #440: across a scripted multi-turn run the stable system block is
    /// byte-identical on every request and never carries the boundary marker,
    /// while per-session and per-turn content stays in the dynamic block.
    #[tokio::test]
    async fn stable_system_block_is_byte_identical_across_turns() {
        let systems = Arc::new(StdMutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider {
            id: claurst_core::provider_id::ProviderId::new("mockprov"),
            tools_empty_per_request: Arc::new(StdMutex::new(Vec::new())),
            systems_per_request: systems.clone(),
            always_end_turn: false,
        });
        let mut registry = claurst_api::ProviderRegistry::new();
        registry.register(provider);

        let client = claurst_api::AnthropicClient::new(claurst_api::client::ClientConfig {
            api_key: "test-key".to_string(),
            ..Default::default()
        })
        .expect("build test client");

        let mut ctx = deny_all_context();
        ctx.session_id = "cache-stability-test".to_string();
        ctx.config.provider = Some("mockprov".to_string());

        let mut config = make_config(Some("Project rules."), Some("Appended."));
        config.model = "mock-model".to_string();
        config.max_turns = 1;
        config.session_context = Some("# Git Status\n## main".to_string());
        config.output_style = claurst_core::system_prompt::OutputStyle::Concise;
        config.provider_registry = Some(Arc::new(registry));

        let mut messages = vec![Message::user("start")];
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_query_loop(
                &client,
                &mut messages,
                &noop_tools(),
                &ctx,
                &config,
                claurst_core::cost::CostTracker::new(),
                None,
                tokio_util::sync::CancellationToken::new(),
                None,
            ),
        )
        .await
        .expect("loop must not hang");
        assert!(matches!(outcome, QueryOutcome::EndTurn { .. }));

        // Turn 1 uses a tool, turn 2 is the tool-less summary turn.
        let systems = systems.lock().unwrap().clone();
        assert_eq!(systems.len(), 2, "expected a two-turn run");
        let halves: Vec<(String, String)> = systems
            .into_iter()
            .map(|system| prompt_halves(system.expect("system prompt sent")))
            .collect();
        let (stable, dynamic) = &halves[0];
        assert_eq!(stable, &halves[1].0, "stable block changed between turns");
        assert!(stable.contains("Project rules."));
        for volatile in ["# Git Status", "maximally concise", "Appended."] {
            assert!(!stable.contains(volatile), "{volatile:?} leaked into the stable block");
            assert!(dynamic.contains(volatile), "{volatile:?} missing from the dynamic block");
        }
    }

    /// (a) A non-goal turn that ends with `end_turn` stops after exactly one
    /// turn — the default `StopPolicy` never continues the loop.
    #[tokio::test]
    async fn non_goal_turn_stops_after_one_turn() {
        let (outcome, recorded, _msgs) = drive_loop_with_mock(
            true,
            5,
            noop_tools(),
            crate::continuation::ContinuationMode::Default,
        )
        .await;

        assert!(
            matches!(outcome, QueryOutcome::EndTurn { .. }),
            "a completed turn must yield EndTurn"
        );
        assert_eq!(
            recorded.len(),
            1,
            "a non-goal end_turn must stop after exactly one request/turn, got {:?}",
            recorded
        );
    }

    /// (c) Hitting `effective_max_turns` runs ONE final turn with tools disabled
    /// (graceful degradation) rather than returning cold: the last request has
    /// an empty tool set and the loop then ends.
    #[tokio::test]
    async fn max_steps_runs_tool_less_summary_turn_then_ends() {
        // max_turns = 2: turns 1 & 2 are tool_use turns, turn 3 exceeds the cap
        // and triggers the tool-less summary turn.
        let (outcome, recorded, msgs) = drive_loop_with_mock(
            false,
            2,
            noop_tools(),
            crate::continuation::ContinuationMode::Default,
        )
        .await;

        assert!(
            matches!(outcome, QueryOutcome::EndTurn { .. }),
            "the loop must end after the degradation summary turn"
        );
        assert_eq!(
            recorded.len(),
            3,
            "expected 2 tool turns + 1 degradation turn, got {:?}",
            recorded
        );
        assert!(
            *recorded.last().unwrap(),
            "the final (summary) turn must be dispatched with tools DISABLED: {:?}",
            recorded
        );
        assert!(
            recorded[..recorded.len() - 1].iter().all(|&empty| !empty),
            "only the degradation turn disables tools: {:?}",
            recorded
        );
        assert!(
            msgs.iter()
                .any(|m| m.get_all_text().contains("maximum number of steps")),
            "the tool-less summary prompt must be injected into the history"
        );
    }

    /// (c-1) Turn-cap precedence (PR #398 review): the session override set
    /// via `/turns` must win over the agent definition's cap, which in turn
    /// wins over the plain config cap. Without the override field, a
    /// `max_turns: Some(20)` agent definition would silently ignore the
    /// user's `/turns 1`.
    #[tokio::test]
    async fn turns_override_wins_over_agent_and_config_caps() {
        // Without an override, the agent definition wins over config:
        // agent cap 3 > config cap 5 → the loop stops at 3 tool turns + 1
        // degradation summary turn (4 requests, the last tool-less).
        let (_outcome, recorded, _msgs) = drive_loop_with_options(
            false,
            5,
            noop_tools(),
            crate::continuation::ContinuationMode::Default,
            true,
            None,
            Some(3),
        )
        .await;
        assert_eq!(
            recorded.len(),
            4,
            "agent cap 3 must beat config cap 5 (3 tool turns + degradation), got {:?}",
            recorded
        );

        // With the override, it beats the agent definition: override 2 wins
        // over agent cap 3 → 2 tool turns + 1 degradation turn (3 requests).
        let (_outcome, recorded, _msgs) = drive_loop_with_options(
            false,
            5,
            noop_tools(),
            crate::continuation::ContinuationMode::Default,
            true,
            Some(2),
            Some(3),
        )
        .await;
        assert_eq!(
            recorded.len(),
            3,
            "session override 2 must beat agent cap 3 (2 tool turns + degradation), got {:?}",
            recorded
        );

        // The override can also RAISE the cap above the agent definition
        // (what `/turns off` relies on): override 4 over agent cap 1 → 4 tool
        // turns + 1 degradation turn (5 requests), more than the agent cap.
        let (_outcome, recorded, _msgs) = drive_loop_with_options(
            false,
            1,
            noop_tools(),
            crate::continuation::ContinuationMode::Default,
            true,
            Some(4),
            Some(1),
        )
        .await;
        assert_eq!(
            recorded.len(),
            5,
            "session override 4 must lift agent cap 1 (4 tool turns + degradation), got {:?}",
            recorded
        );
    }

    /// (c-2) With `degradation_summary_enabled: false`, hitting the turn cap
    /// returns cold immediately: NO extra tool-less summary request is made
    /// and the summary prompt is never injected into the history.
    #[tokio::test]
    async fn max_steps_with_degradation_disabled_returns_cold() {
        // max_turns = 2: turns 1 & 2 are tool_use turns, turn 3 exceeds the cap.
        // With the flag off the loop must stop after exactly 2 requests.
        let (outcome, recorded, msgs) = drive_loop_with_options(
            false,
            2,
            noop_tools(),
            crate::continuation::ContinuationMode::Default,
            false,
            None,
            None,
        )
        .await;

        assert!(
            matches!(outcome, QueryOutcome::EndTurn { .. }),
            "the disabled-flag loop must still end with EndTurn (cold return)"
        );
        assert_eq!(
            recorded.len(),
            2,
            "no extra summary request must be dispatched when the flag is off, got {:?}",
            recorded
        );
        assert!(
            recorded.iter().all(|&empty| !empty),
            "tools must stay enabled on every dispatched turn: {:?}",
            recorded
        );
        assert!(
            !msgs
                .iter()
                .any(|m| m.get_all_text().contains("maximum number of steps")),
            "the summary prompt must NOT be injected when the flag is off"
        );
        // The cold return must carry the last assistant message verbatim.
        let last_assistant = msgs
            .iter()
            .rev()
            .find(|m| m.role == claurst_core::types::Role::Assistant)
            .cloned();
        if let QueryOutcome::EndTurn { message, .. } = &outcome {
            let returned_text = message.get_all_text();
            let expected_text = last_assistant
                .as_ref()
                .map(|m| m.get_all_text())
                .unwrap_or_default();
            assert_eq!(
                returned_text, expected_text,
                "cold return must carry the last assistant message"
            );
        }
    }

    /// (b) The goal continuation guards, exercised against an in-memory store:
    /// an active goal within its guards continues (recording the turn), while
    /// the soft-budget and runaway guards each stop with the same paused
    /// outcome as before.
    #[test]
    fn goal_policy_continues_while_active_and_stops_on_guards() {
        use crate::goal_loop::{decide_goal_continuation, GoalContinuation, StopReason};

        let store =
            claurst_core::GoalStore::open(std::path::Path::new(":memory:")).expect("open store");

        // Active goal, guards allow → continue with the goal continuation message.
        store.set_goal("live", "ship the feature", None).unwrap();
        match decide_goal_continuation(&store, "live", 0, 1) {
            GoalContinuation::Continue { message } => {
                assert!(
                    message.contains("Goal continuation"),
                    "unexpected continuation message: {}",
                    message
                );
            }
            _ => panic!("an active goal within its guards must continue"),
        }
        // The turn was recorded in the store.
        assert_eq!(store.get_goal("live").unwrap().turns_used, 1);

        // Soft token budget tripped → budget-limited (paused) outcome.
        store.set_goal("budget", "big task", Some(100)).unwrap();
        match decide_goal_continuation(&store, "budget", 500, 1) {
            GoalContinuation::Stop {
                reason: StopReason::BudgetLimited,
            } => {}
            _ => panic!("an over-budget goal must stop budget-limited"),
        }
        assert_eq!(
            store.get_goal("budget").unwrap().status,
            claurst_core::GoalStatus::BudgetLimited,
            "over-budget goal must be persisted as budget-limited"
        );

        // Runaway guard tripped → paused outcome (same as the cross-turn design).
        store.set_goal("runaway", "endless", None).unwrap();
        for _ in 0..claurst_core::MAX_GOAL_TURNS {
            store.record_turn("runaway", 0).unwrap();
        }
        match decide_goal_continuation(&store, "runaway", 0, 1) {
            GoalContinuation::Stop {
                reason: StopReason::RunawayGuard { turns_used },
            } => {
                assert_eq!(turns_used, claurst_core::MAX_GOAL_TURNS);
            }
            _ => panic!("a runaway goal must pause"),
        }
        assert_eq!(
            store.get_goal("runaway").unwrap().status,
            claurst_core::GoalStatus::Paused,
            "runaway goal must be persisted as paused"
        );
    }

    // ---- ultracode activation (effort) ----------------------------------

    #[test]
    fn ultracode_keyword_raises_effort_to_ultracode() {
        use claurst_core::effort::EffortLevel;
        let msgs = vec![Message::user("please ultracode this refactor")];
        // Even with no configured effort, the keyword forces Ultracode.
        assert_eq!(
            effective_effort_for_turn(None, &msgs),
            Some(EffortLevel::Ultracode)
        );
        // ...and it overrides a lower configured effort for the turn.
        assert_eq!(
            effective_effort_for_turn(Some(EffortLevel::Low), &msgs),
            Some(EffortLevel::Ultracode)
        );
    }

    #[test]
    fn no_keyword_keeps_configured_effort() {
        use claurst_core::effort::EffortLevel;
        let msgs = vec![Message::user("please refactor this module")];
        assert_eq!(effective_effort_for_turn(None, &msgs), None);
        assert_eq!(
            effective_effort_for_turn(Some(EffortLevel::High), &msgs),
            Some(EffortLevel::High)
        );
    }

    #[test]
    fn ultracode_effort_checks_only_the_last_user_message() {
        // Keyword in an earlier turn does not keep ultracode active on a later
        // plain turn.
        let msgs = vec![
            Message::user("ultracode kick things off"),
            Message::assistant("working on it"),
            Message::user("now just tidy up the docs"),
        ];
        assert_eq!(effective_effort_for_turn(None, &msgs), None);
    }

    #[test]
    fn ultracode_addendum_flows_into_built_system_prompt() {
        use claurst_core::effort::EffortLevel;
        // Mirrors the loop wiring: when the effective effort is Ultracode the
        // procedure addendum is threaded through `append_system_prompt` into the
        // assembled system prompt.
        let msgs = vec![Message::user("ultracode audit the query loop")];
        assert_eq!(
            effective_effort_for_turn(None, &msgs),
            Some(EffortLevel::Ultracode)
        );
        let addendum = claurst_core::effort::ultracode_system_prompt_addendum();
        let opts = claurst_core::system_prompt::SystemPromptOptions {
            append_system_prompt: Some(addendum),
            skip_env_info: true,
            ..Default::default()
        };
        let prompt = claurst_core::system_prompt::build_system_prompt(&opts);
        assert!(prompt.contains("Ultracode Mode"));
        assert!(prompt.contains("TeamCreate"));

        // Absent path: no keyword -> configured effort stays, no ultracode text.
        assert_eq!(
            effective_effort_for_turn(None, &[Message::user("hi there")]),
            None
        );
        let plain = claurst_core::system_prompt::build_system_prompt(
            &claurst_core::system_prompt::SystemPromptOptions {
                skip_env_info: true,
                ..Default::default()
            },
        );
        assert!(!plain.contains("Ultracode Mode"));
    }

    // ---- persona output-style (transient vs persistent) ------------------

    #[test]
    fn inline_persona_keyword_applies_transiently_for_the_turn() {
        // No persisted persona; an inline `rocky` selects the rocky prompt for
        // this turn only.
        let cfg = QueryConfig::default();
        let msgs = vec![Message::user("please rocky explain this borrow error")];
        let (_style, prompt) = effective_output_style_for_turn(&cfg, &msgs);
        let prompt = prompt.expect("inline rocky should resolve a persona prompt");
        assert!(prompt.contains("Project Hail Mary"));

        // Caveman likewise.
        let msgs = vec![Message::user("caveman summarize the diff")];
        let (_s, prompt) = effective_output_style_for_turn(&cfg, &msgs);
        assert!(prompt.unwrap().contains("UNCHANGED"));
    }

    #[test]
    fn persona_only_checks_the_last_user_message() {
        // A persona keyword in an earlier turn does not linger onto a later
        // plain turn (transient, like ultracode).
        let cfg = QueryConfig::default();
        let msgs = vec![
            Message::user("rocky kick things off"),
            Message::assistant("good good good"),
            Message::user("now just tidy the docs"),
        ];
        let (_style, prompt) = effective_output_style_for_turn(&cfg, &msgs);
        assert!(prompt.is_none(), "persona should not persist to a plain turn");
    }

    #[test]
    fn persisted_persona_stands_without_an_inline_keyword() {
        // A persona chosen via /rocky or /output-style lives in the config and
        // persists across plain turns.
        let cfg = QueryConfig {
            output_style_prompt: Some("PERSISTED PERSONA".to_string()),
            ..QueryConfig::default()
        };
        let msgs = vec![Message::user("just a plain request here please")];
        let (_style, prompt) = effective_output_style_for_turn(&cfg, &msgs);
        assert_eq!(prompt.as_deref(), Some("PERSISTED PERSONA"));
    }

    #[test]
    fn inline_normal_resets_a_persisted_persona_for_the_turn() {
        // With a persona persisted, an inline `normal` clears it for this turn.
        let cfg = QueryConfig {
            output_style_prompt: Some("PERSISTED PERSONA".to_string()),
            ..QueryConfig::default()
        };
        let msgs = vec![Message::user("back to normal for this one please")];
        let (style, prompt) = effective_output_style_for_turn(&cfg, &msgs);
        assert!(prompt.is_none(), "inline normal should reset the persona");
        assert_eq!(style, claurst_core::system_prompt::OutputStyle::Default);
    }

    #[test]
    fn inline_persona_overrides_a_different_persisted_persona() {
        // Persisted caveman, but this turn asks for rocky inline → rocky wins
        // transiently.
        let cfg = QueryConfig {
            output_style_prompt: Some(
                claurst_core::output_styles::OutputStyleDef::builtin_caveman().prompt,
            ),
            ..QueryConfig::default()
        };
        let msgs = vec![Message::user("rocky, review this function")];
        let (_style, prompt) = effective_output_style_for_turn(&cfg, &msgs);
        assert!(prompt.unwrap().contains("Project Hail Mary"));
    }

    // ---- Issue #439: unified retry policy, driven through the real loop -----

    /// One scripted provider response: a request-level error, or the stream
    /// items to yield for that request.
    enum ScriptStep {
        Fail(claurst_api::ProviderError),
        Stream(Vec<Result<claurst_api::StreamEvent, claurst_api::ProviderError>>),
    }

    /// A provider double that replays `ScriptStep`s in order (one per
    /// request) and then answers with a plain `end_turn` text response.
    struct ScriptedProvider {
        id: claurst_core::provider_id::ProviderId,
        steps: StdMutex<std::collections::VecDeque<ScriptStep>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    fn text_turn(text: &str) -> Vec<Result<claurst_api::StreamEvent, claurst_api::ProviderError>> {
        use claurst_api::provider_types::StopReason;
        use claurst_api::StreamEvent;
        vec![
            Ok(StreamEvent::MessageStart {
                id: uuid::Uuid::new_v4().to_string(),
                model: "mock-model".to_string(),
                usage: UsageInfo::default(),
            }),
            Ok(StreamEvent::TextDelta { index: 0, text: text.to_string() }),
            Ok(StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                usage: Some(UsageInfo::default()),
            }),
            Ok(StreamEvent::MessageStop),
        ]
    }

    #[async_trait::async_trait]
    impl claurst_api::LlmProvider for ScriptedProvider {
        fn id(&self) -> &claurst_core::provider_id::ProviderId {
            &self.id
        }
        fn name(&self) -> &str {
            "scripted-mock"
        }

        async fn create_message(
            &self,
            _request: claurst_api::ProviderRequest,
        ) -> Result<claurst_api::ProviderResponse, claurst_api::ProviderError> {
            unimplemented!("these tests only use create_message_stream")
        }

        async fn create_message_stream(
            &self,
            _request: claurst_api::ProviderRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<claurst_api::StreamEvent, claurst_api::ProviderError>,
                        > + Send,
                >,
            >,
            claurst_api::ProviderError,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let step = self.steps.lock().unwrap().pop_front();
            let events = match step {
                Some(ScriptStep::Fail(e)) => return Err(e),
                Some(ScriptStep::Stream(events)) => events,
                None => text_turn("final answer"),
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }

        async fn health_check(
            &self,
        ) -> Result<claurst_api::ProviderStatus, claurst_api::ProviderError> {
            Ok(claurst_api::ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> claurst_api::ProviderCapabilities {
            claurst_api::ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: claurst_api::SystemPromptStyle::TopLevel,
            }
        }
    }

    struct ScriptedRun {
        outcome: QueryOutcome,
        calls: usize,
        events: Vec<QueryEvent>,
        messages: Vec<Message>,
    }

    /// Drive `run_query_loop` against a `ScriptedProvider` with a fast retry
    /// policy (1 ms base back-off) and collect every emitted event.
    async fn drive_scripted(
        steps: Vec<ScriptStep>,
        max_retries: u32,
        tools: Vec<Box<dyn Tool>>,
    ) -> ScriptedRun {
        drive_scripted_with(steps, max_retries, tools, None, Vec::new(), None).await
    }

    /// [`drive_scripted`] with an explicit turn cap and queued user messages
    /// (drained by the loop at the top of each turn).
    async fn drive_scripted_with(
        steps: Vec<ScriptStep>,
        max_retries: u32,
        tools: Vec<Box<dyn Tool>>,
        max_turns: Option<u32>,
        mut pending: Vec<String>,
        history: Option<Vec<Message>>,
    ) -> ScriptedRun {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = Arc::new(ScriptedProvider {
            id: claurst_core::provider_id::ProviderId::new("scriptprov"),
            steps: StdMutex::new(steps.into()),
            calls: calls.clone(),
        });
        let mut registry = claurst_api::ProviderRegistry::new();
        registry.register(provider);

        let client = claurst_api::AnthropicClient::new(claurst_api::client::ClientConfig {
            api_key: "test-key".to_string(),
            ..Default::default()
        })
        .expect("build test client");

        let mut ctx = deny_all_context();
        ctx.session_id = "retry-test".to_string();
        ctx.config.provider = Some("scriptprov".to_string());

        let mut config = make_config(None, None);
        config.model = "mock-model".to_string();
        config.provider_registry = Some(Arc::new(registry));
        config.retry = claurst_api::RetryConfig {
            max_retries,
            initial_delay: std::time::Duration::from_millis(1),
            ..claurst_api::RetryConfig::default()
        };
        if let Some(max_turns) = max_turns {
            config.max_turns = max_turns;
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut messages = history.unwrap_or_else(|| vec![Message::user("start")]);
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_query_loop(
                &client,
                &mut messages,
                &tools,
                &ctx,
                &config,
                claurst_core::cost::CostTracker::new(),
                Some(tx),
                tokio_util::sync::CancellationToken::new(),
                Some(&mut pending),
            ),
        )
        .await
        .expect("loop must not hang");

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        ScriptedRun {
            outcome,
            calls: calls.load(std::sync::atomic::Ordering::SeqCst),
            events,
            messages,
        }
    }

    fn pid_scripted() -> claurst_core::provider_id::ProviderId {
        claurst_core::provider_id::ProviderId::new("scriptprov")
    }

    fn server_error(status: u16) -> claurst_api::ProviderError {
        claurst_api::parse_error_response(status, "upstream unavailable", &pid_scripted())
    }

    #[tokio::test]
    async fn retry_503_then_success_completes_the_turn() {
        let run = drive_scripted(vec![ScriptStep::Fail(server_error(503))], 5, noop_tools()).await;
        assert!(
            matches!(run.outcome, QueryOutcome::EndTurn { .. }),
            "a 503 followed by success must complete the turn"
        );
        assert_eq!(run.calls, 2, "one failed attempt plus one successful retry");
        assert!(
            run.events.iter().any(|e| matches!(
                e,
                QueryEvent::Retry { attempt: 1, max_attempts: 5, reason, .. }
                    if reason.contains("503")
            )),
            "the retry must be reported: {:?}",
            run.events
        );
        assert!(
            run.messages.iter().any(|m| m.get_all_text().contains("final answer")),
            "the retried response must land in the history"
        );
    }

    #[tokio::test]
    async fn retry_401_is_never_retried() {
        let auth = claurst_api::parse_error_response(
            401,
            r#"{"error":{"message":"invalid api key"}}"#,
            &pid_scripted(),
        );
        let run = drive_scripted(vec![ScriptStep::Fail(auth)], 5, noop_tools()).await;
        assert!(matches!(run.outcome, QueryOutcome::Error(_)), "401 must surface an error");
        assert_eq!(run.calls, 1, "401 must not be retried");
    }

    #[tokio::test]
    async fn retry_429_honours_server_retry_after() {
        let rate = claurst_api::ProviderError::RateLimited {
            provider: pid_scripted(),
            retry_after: Some(std::time::Duration::from_millis(20)),
        };
        let started = std::time::Instant::now();
        let run = drive_scripted(vec![ScriptStep::Fail(rate)], 5, noop_tools()).await;
        assert!(matches!(run.outcome, QueryOutcome::EndTurn { .. }));
        assert_eq!(run.calls, 2);
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(20),
            "the loop must wait at least the server-requested Retry-After"
        );
        assert!(
            run.events
                .iter()
                .any(|e| matches!(e, QueryEvent::Retry { delay_ms: 20, .. })),
            "the Retry event carries the server-requested delay: {:?}",
            run.events
        );
    }

    #[tokio::test]
    async fn retry_budget_exhaustion_surfaces_an_error() {
        let steps = (0..3).map(|_| ScriptStep::Fail(server_error(502))).collect();
        let run = drive_scripted(steps, 2, noop_tools()).await;
        assert!(
            matches!(run.outcome, QueryOutcome::Error(_)),
            "exhausted retries must end the turn with an error"
        );
        assert_eq!(run.calls, 3, "initial attempt + 2 retries");
    }

    #[tokio::test]
    async fn retry_mid_stream_error_then_success() {
        let mut cut = text_turn("partial");
        cut.truncate(2);
        cut.push(Err(claurst_api::ProviderError::StreamError {
            provider: pid_scripted(),
            message: "Stream read error: connection reset".to_string(),
            partial_response: None,
        }));
        let run = drive_scripted(vec![ScriptStep::Stream(cut)], 5, noop_tools()).await;
        assert!(matches!(run.outcome, QueryOutcome::EndTurn { .. }));
        assert_eq!(run.calls, 2);
        assert!(
            !run.messages.iter().any(|m| m.get_all_text().contains("partial")),
            "output from the failed attempt must not be kept"
        );
    }

    #[tokio::test]
    async fn retry_stream_cut_without_message_stop_then_success() {
        let mut cut = text_turn("half an ans");
        cut.truncate(2); // MessageStart + one delta, then the stream just ends
        let run = drive_scripted(vec![ScriptStep::Stream(cut)], 5, noop_tools()).await;
        assert!(matches!(run.outcome, QueryOutcome::EndTurn { .. }));
        assert_eq!(run.calls, 2, "a truncated stream must be retried");
        assert!(
            !run.messages.iter().any(|m| m.get_all_text().contains("half an ans")),
            "the truncated attempt must not be kept as a finished turn"
        );
    }

    #[tokio::test]
    async fn truncated_stream_with_no_retries_is_an_error_not_end_turn() {
        let mut cut = text_turn("half");
        cut.truncate(2);
        let run = drive_scripted(vec![ScriptStep::Stream(cut)], 0, noop_tools()).await;
        assert!(
            matches!(run.outcome, QueryOutcome::Error(_)),
            "a truncated stream must end the turn with a visible error"
        );
    }

    /// A tool-use turn whose argument JSON is `args`, then `end_turn`.
    fn tool_call_turn(args: &str) -> Vec<Result<claurst_api::StreamEvent, claurst_api::ProviderError>> {
        use claurst_api::provider_types::StopReason;
        use claurst_api::StreamEvent;
        vec![
            Ok(StreamEvent::MessageStart {
                id: uuid::Uuid::new_v4().to_string(),
                model: "mock-model".to_string(),
                usage: UsageInfo::default(),
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlock::ToolUse {
                    id: "call-1".to_string(),
                    name: "noop_tool".to_string(),
                    input: serde_json::json!({}),
                    thought_signature: None,
                },
            }),
            Ok(StreamEvent::InputJsonDelta { index: 1, partial_json: args.to_string() }),
            Ok(StreamEvent::ContentBlockStop { index: 1 }),
            Ok(StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::ToolUse),
                usage: Some(UsageInfo::default()),
            }),
            Ok(StreamEvent::MessageStop),
        ]
    }

    fn recording_tool() -> (Vec<Box<dyn Tool>>, Arc<AtomicBool>) {
        let ran = Arc::new(AtomicBool::new(false));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool {
            name: "noop_tool",
            level: PermissionLevel::ReadOnly,
            self_gates: false,
            ran: ran.clone(),
        })];
        (tools, ran)
    }

    fn tool_result_for(messages: &[Message], id: &str) -> Option<(String, bool)> {
        messages.iter().find_map(|m| match &m.content {
            claurst_core::types::MessageContent::Blocks(blocks) => blocks.iter().find_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, content, is_error } if tool_use_id == id => {
                    let text = match content {
                        ToolResultContent::Text(t) => t.clone(),
                        other => format!("{other:?}"),
                    };
                    Some((text, is_error.unwrap_or(false)))
                }
                _ => None,
            }),
            _ => None,
        })
    }

    #[tokio::test]
    async fn malformed_tool_json_is_repaired_and_the_tool_runs() {
        let (tools, ran) = recording_tool();
        let run = drive_scripted(
            vec![ScriptStep::Stream(tool_call_turn("{\"note\": \"a\nb\",}"))],
            0,
            tools,
        )
        .await;
        assert!(matches!(run.outcome, QueryOutcome::EndTurn { .. }));
        assert!(ran.load(AtomicOrdering::SeqCst), "repaired arguments must run the tool");
        let (_, is_error) = tool_result_for(&run.messages, "call-1").expect("tool result");
        assert!(!is_error);
    }

    #[tokio::test]
    async fn unrepairable_tool_json_is_rejected_without_running_the_tool() {
        let (tools, ran) = recording_tool();
        let run = drive_scripted(
            vec![ScriptStep::Stream(tool_call_turn("{\"path\": \"/etc/pas"))],
            0,
            tools,
        )
        .await;
        assert!(matches!(run.outcome, QueryOutcome::EndTurn { .. }));
        assert!(!ran.load(AtomicOrdering::SeqCst), "the tool must not run with `{{}}`");
        let (text, is_error) = tool_result_for(&run.messages, "call-1").expect("tool result");
        assert!(is_error);
        assert!(
            text.contains("invalid JSON input for tool noop_tool"),
            "the model must see why: {text}"
        );
    }

    #[tokio::test]
    async fn retried_degradation_turn_injects_the_summary_prompt_once() {
        // max_turns = 0 makes the first turn the tool-less summary turn. A
        // queued message drained after the injection sits after the prompt;
        // the retry must still remove it before re-injecting.
        let run = drive_scripted_with(
            vec![ScriptStep::Fail(server_error(503))],
            5,
            noop_tools(),
            Some(0),
            vec!["typed during the attempt".to_string()],
            None,
        )
        .await;
        assert!(matches!(run.outcome, QueryOutcome::EndTurn { .. }));
        assert_eq!(run.calls, 2);
        let prompts = run
            .messages
            .iter()
            .filter(|m| m.get_all_text() == MAX_STEPS_DEGRADATION_MSG)
            .count();
        assert_eq!(prompts, 1, "summary prompt must appear exactly once");
    }

    // ---- Issue #441: compaction check points --------------------------------

    /// Scripted outcome for one main (non-summary) request.
    #[derive(Clone, Copy)]
    enum MainStep {
        /// Fail with a provider context-window error.
        Overflow,
        /// End the turn with text.
        Reply,
    }

    /// A provider double for compaction tests. Summarisation requests (the
    /// summariser system prompt, no tools) get a canned summary; main requests
    /// replay `script`. Every request is logged as `"summary"`, or as
    /// `"main:compacted"` / `"main:plain"` depending on whether the history it
    /// carried starts with a compaction summary.
    struct CompactionProvider {
        id: claurst_core::provider_id::ProviderId,
        script: StdMutex<std::collections::VecDeque<MainStep>>,
        log: Arc<StdMutex<Vec<String>>>,
    }

    fn scripted_text(
        text: &str,
    ) -> Vec<Result<claurst_api::StreamEvent, claurst_api::ProviderError>> {
        use claurst_api::provider_types::StopReason;
        use claurst_api::StreamEvent;
        vec![
            Ok(StreamEvent::MessageStart {
                id: uuid::Uuid::new_v4().to_string(),
                model: "mock-model".to_string(),
                usage: UsageInfo::default(),
            }),
            Ok(StreamEvent::TextDelta { index: 0, text: text.to_string() }),
            Ok(StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                usage: Some(UsageInfo::default()),
            }),
            Ok(StreamEvent::MessageStop),
        ]
    }

    #[async_trait::async_trait]
    impl claurst_api::LlmProvider for CompactionProvider {
        fn id(&self) -> &claurst_core::provider_id::ProviderId {
            &self.id
        }
        fn name(&self) -> &str {
            "compaction-mock"
        }

        async fn create_message(
            &self,
            _request: claurst_api::ProviderRequest,
        ) -> Result<claurst_api::ProviderResponse, claurst_api::ProviderError> {
            unimplemented!("compaction only uses create_message_stream")
        }

        async fn create_message_stream(
            &self,
            request: claurst_api::ProviderRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<claurst_api::StreamEvent, claurst_api::ProviderError>,
                        > + Send,
                >,
            >,
            claurst_api::ProviderError,
        > {
            let is_summary = matches!(
                &request.system_prompt,
                Some(claurst_api::SystemPrompt::Text(text)) if text.contains("conversation summaries")
            );
            if is_summary {
                self.log.lock().unwrap().push("summary".to_string());
                return Ok(Box::pin(futures::stream::iter(scripted_text(
                    "<summary>Earlier work, condensed.</summary>",
                ))));
            }

            let compacted = request
                .messages
                .first()
                .is_some_and(|m| m.get_all_text().contains("<compact-summary>"));
            self.log.lock().unwrap().push(
                if compacted { "main:compacted" } else { "main:plain" }.to_string(),
            );
            let step = self.script.lock().unwrap().pop_front().unwrap_or(MainStep::Reply);
            match step {
                MainStep::Overflow => Err(claurst_api::ProviderError::ContextOverflow {
                    provider: self.id.clone(),
                    message: "prompt is too long".to_string(),
                    max_tokens: None,
                }),
                MainStep::Reply => Ok(Box::pin(futures::stream::iter(scripted_text("done")))),
            }
        }

        async fn health_check(
            &self,
        ) -> Result<claurst_api::ProviderStatus, claurst_api::ProviderError> {
            Ok(claurst_api::ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> claurst_api::ProviderCapabilities {
            claurst_api::ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: claurst_api::SystemPromptStyle::TopLevel,
            }
        }
    }

    /// `count` alternating user/assistant messages of `size` chars each,
    /// starting and ending with a user message (`count` must be odd).
    fn long_history(count: usize, size: usize) -> Vec<Message> {
        (0..count)
            .map(|i| {
                let text = format!("turn {} {}", i, "x".repeat(size));
                if i % 2 == 0 { Message::user(text) } else { Message::assistant(text) }
            })
            .collect()
    }

    /// Drive `run_query_loop` against [`CompactionProvider`] registered under
    /// the non-Anthropic id `mockprov`. The mock model has no registry entry,
    /// so the window falls back to the 100k heuristic and the threshold is
    /// min(90k, 100k - min(32k, 25k)) = 75k tokens.
    async fn drive_compaction_loop(
        history: Vec<Message>,
        script: Vec<MainStep>,
        auto_compact: bool,
    ) -> (QueryOutcome, Vec<String>, Vec<Message>) {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let provider = Arc::new(CompactionProvider {
            id: claurst_core::provider_id::ProviderId::new("mockprov"),
            script: StdMutex::new(script.into()),
            log: log.clone(),
        });
        let mut registry = claurst_api::ProviderRegistry::new();
        registry.register(provider);

        let client = claurst_api::AnthropicClient::new(claurst_api::client::ClientConfig {
            api_key: "test-key".to_string(),
            ..Default::default()
        })
        .expect("build test client");

        let mut ctx = deny_all_context();
        ctx.session_id = "compaction-test".to_string();
        ctx.config.provider = Some("mockprov".to_string());

        let mut config = make_config(None, None);
        config.model = "mock-model".to_string();
        config.provider_registry = Some(Arc::new(registry));
        config.auto_compact = auto_compact;

        let mut messages = history;
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_query_loop(
                &client,
                &mut messages,
                &noop_tools(),
                &ctx,
                &config,
                claurst_core::cost::CostTracker::new(),
                None,
                tokio_util::sync::CancellationToken::new(),
                None,
            ),
        )
        .await
        .expect("loop must not hang");

        let log = log.lock().unwrap().clone();
        (outcome, log, messages)
    }

    /// Over the threshold, the history is compacted through the active
    /// (non-Anthropic) provider before the request is sent.
    #[tokio::test]
    async fn compaction_triggers_at_threshold_before_request() {
        // 11 × 30k chars ≈ 110k estimated tokens > 75k threshold.
        let (outcome, log, msgs) =
            drive_compaction_loop(long_history(11, 30_000), vec![MainStep::Reply], true).await;

        assert!(matches!(outcome, QueryOutcome::EndTurn { .. }));
        assert_eq!(log, vec!["summary", "main:compacted"]);
        assert!(msgs[0].get_all_text().contains("Earlier work, condensed."));
        assert!(msgs.len() < 11, "history must shrink, got {} messages", msgs.len());
    }

    #[tokio::test]
    async fn no_compaction_below_threshold() {
        let (_outcome, log, _msgs) =
            drive_compaction_loop(long_history(3, 1_000), vec![MainStep::Reply], true).await;
        assert_eq!(log, vec!["main:plain"]);
    }

    /// `--no-auto-compact` (auto_compact = false) leaves an over-threshold
    /// history alone.
    #[tokio::test]
    async fn auto_compact_disabled_skips_threshold_compaction() {
        let (_outcome, log, msgs) =
            drive_compaction_loop(long_history(11, 30_000), vec![MainStep::Reply], false).await;
        assert_eq!(log, vec!["main:plain"]);
        assert_eq!(msgs.len(), 12, "history untouched plus the reply");
    }

    /// A context-window error compacts once and retries the request.
    #[tokio::test]
    async fn overflow_error_compacts_and_retries() {
        // 7 × 20k chars ≈ 47k estimated tokens: under the threshold, so only
        // the provider's overflow error triggers compaction.
        let (outcome, log, msgs) = drive_compaction_loop(
            long_history(7, 20_000),
            vec![MainStep::Overflow, MainStep::Reply],
            true,
        )
        .await;

        assert!(matches!(outcome, QueryOutcome::EndTurn { .. }), "{:?}", outcome);
        assert_eq!(log, vec!["main:plain", "summary", "main:compacted"]);
        assert!(msgs[0].get_all_text().contains("<compact-summary>"));
    }

    /// A second overflow on the retried request surfaces the error.
    #[tokio::test]
    async fn repeated_overflow_surfaces_the_error() {
        let (outcome, log, _msgs) = drive_compaction_loop(
            long_history(7, 20_000),
            vec![MainStep::Overflow, MainStep::Overflow],
            true,
        )
        .await;

        assert!(matches!(outcome, QueryOutcome::Error(_)), "{:?}", outcome);
        assert_eq!(log, vec!["main:plain", "summary", "main:compacted"]);
    }

    #[tokio::test]
    async fn overflow_with_auto_compact_disabled_surfaces_the_error() {
        let (outcome, log, _msgs) = drive_compaction_loop(
            long_history(7, 20_000),
            vec![MainStep::Overflow],
            false,
        )
        .await;

        assert!(matches!(outcome, QueryOutcome::Error(_)), "{:?}", outcome);
        assert_eq!(log, vec!["main:plain"]);
    }

    // ---- Issue #441: per-item tool-result cap -------------------------------

    /// A read-only tool named like the one `RecordingProvider` calls, whose
    /// output is far over the per-item cap.
    struct BigOutputTool;

    #[async_trait::async_trait]
    impl Tool for BigOutputTool {
        fn name(&self) -> &str { "noop_tool" }
        fn description(&self) -> &str { "emits oversized output" }
        fn permission_level(&self) -> PermissionLevel { PermissionLevel::ReadOnly }
        fn input_schema(&self) -> Value { serde_json::json!({"type": "object"}) }
        async fn execute(&self, _input: Value, _ctx: &ToolContext) -> ToolResult {
            ToolResult::success(format!("FIRST LINE\n{}\nLAST LINE", "y".repeat(200_000)))
        }
    }

    /// Each tool result is capped once, when appended: head and tail kept,
    /// the omitted size noted.
    #[tokio::test]
    async fn tool_results_are_capped_head_and_tail_on_insert() {
        let (_outcome, _recorded, msgs) = drive_loop_with_mock(
            false,
            1,
            vec![Box::new(BigOutputTool)],
            crate::continuation::ContinuationMode::Default,
        )
        .await;

        let result = msgs
            .iter()
            .flat_map(|m| match &m.content {
                claurst_core::types::MessageContent::Blocks(blocks) => blocks.as_slice(),
                _ => &[],
            })
            .find_map(|b| match b {
                ContentBlock::ToolResult { content: ToolResultContent::Text(t), .. } => Some(t),
                _ => None,
            })
            .expect("tool result in history");
        assert!(result.starts_with("FIRST LINE"));
        assert!(result.ends_with("LAST LINE"));
        assert!(result.contains("bytes omitted from the middle"));
        assert!(result.len() <= DEFAULT_TOOL_RESULT_MAX_BYTES + 100, "{}", result.len());
    }

    /// #439 x #441: a context-window error compacts once and retries; it is
    /// not treated as a transient failure (no `Retry` event, no back-off).
    #[tokio::test]
    async fn overflow_compacts_once_then_retries_without_a_retry_event() {
        let overflow = claurst_api::ProviderError::ContextOverflow {
            provider: pid_scripted(),
            message: "prompt is too long".to_string(),
            max_tokens: None,
        };
        // Request 1 overflows; request 2 is the summariser; request 3 succeeds.
        let run = drive_scripted_with(
            vec![
                ScriptStep::Fail(overflow),
                ScriptStep::Stream(text_turn("<summary>Earlier work, condensed.</summary>")),
            ],
            5,
            noop_tools(),
            None,
            Vec::new(),
            Some(long_history(7, 20_000)),
        )
        .await;
        assert_eq!(run.calls, 3, "overflow, summary, retried request");
        assert!(matches!(run.outcome, QueryOutcome::EndTurn { .. }), "{:?}", run.events);
        assert!(
            !run.events.iter().any(|e| matches!(e, QueryEvent::Retry { .. })),
            "an overflow must not be reported as a transient retry: {:?}",
            run.events
        );
        assert_eq!(
            run.events.iter().filter(|e| matches!(e, QueryEvent::Compacted(_))).count(),
            1,
            "exactly one compaction: {:?}",
            run.events
        );
    }
}
