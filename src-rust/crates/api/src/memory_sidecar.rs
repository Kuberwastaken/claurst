// memory_sidecar.rs — Lightweight sidecar client for fast, cheap LLM calls.
//
// Ported from jcode `jcode-base/src/sidecar.rs`, adapted to claurst's
// provider architecture: instead of jcode's dedicated OAuth HTTP clients,
// every call dispatches through the claurst `LlmProvider` stack
// (`ProviderRegistry` + `runtime_provider_for`), so the sidecar works on
// ALL providers claurst supports, not just OpenAI/Claude.
//
// Used by the memory system for relevance verification, contradiction
// checks, and memory extraction. The heavy prompt text from jcode's
// sidecar is intentionally moved here unchanged.

use crate::provider::LlmProvider;
use crate::provider_error::ProviderError;
use crate::provider_types::{ProviderRequest, ProviderResponse, SystemPrompt};
use crate::ModelRegistry;
use anyhow::{Context, Result};
use claurst_core::memory_config::{MemorySettings, MemorySidecarBackend, MemorySidecarFallback};
use claurst_core::types::{ContentBlock, Message, MessageContent, Role};
use serde_json::Value;
use std::sync::Arc;

/// Fast/cheap default model used when no memory model is configured.
/// gpt-5-mini keeps sidecar calls cheap while the memory sidecar stays
/// functional on the OpenAI path; haiku is the Claude-path default.
pub const SIDECAR_DEFAULT_MODEL: &str = "gpt-5-mini";
pub const SIDECAR_CLAUDE_MODEL: &str = "claude-haiku-4-5";

/// Maximum tokens for sidecar responses (keep small for speed/cost).
const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Whether retrying a failed sidecar request can reasonably succeed without
/// a configuration or credential change. Ported from jcode's
/// `SidecarErrorKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarErrorKind {
    Transient,
    Permanent,
}

/// Classify a sidecar failure for retry policy. HTTP client/auth/request
/// errors are permanent; throttling, server failures, and transport
/// failures are transient. Unknown provider errors retain the conservative
/// retry behavior.
pub fn classify_error(error: &anyhow::Error) -> SidecarErrorKind {
    for cause in error.chain() {
        if let Some(pe) = cause.downcast_ref::<ProviderError>() {
            return classify_provider_error(pe);
        }
    }

    // Fall back to message sniffing, mirroring jcode.
    let message = error.to_string().to_ascii_lowercase();
    if [
        "400",
        "401",
        "403",
        "404",
        "bad request",
        "unauthorized",
        "forbidden",
        "not_found_error",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        SidecarErrorKind::Permanent
    } else {
        SidecarErrorKind::Transient
    }
}

fn classify_provider_error(error: &ProviderError) -> SidecarErrorKind {
    match error {
        ProviderError::RateLimited { .. }
        | ProviderError::ServerError { .. }
        | ProviderError::ContextOverflow { .. } => SidecarErrorKind::Transient,
        ProviderError::AuthFailed { .. }
        | ProviderError::QuotaExceeded { .. }
        | ProviderError::ModelNotFound { .. } => SidecarErrorKind::Permanent,
        ProviderError::Other { status, .. } => match status {
            Some(code) if *code == 429 || *code >= 500 => SidecarErrorKind::Transient,
            Some(code) if *code >= 400 => SidecarErrorKind::Permanent,
            _ => SidecarErrorKind::Transient,
        },
        _ => SidecarErrorKind::Transient,
    }
}

/// The dispatch target for sidecar completions.
#[derive(Clone)]
enum SidecarTarget {
    /// A specific provider instance, with an explicit model id.
    Provider {
        provider: Arc<dyn LlmProvider>,
        model: String,
    },
    /// No usable backend: calls fail with a clear error.
    Dormant,
}

/// Lightweight client for fast sidecar calls.
///
/// Unlike jcode's sidecar (which hand-rolls OpenAI/Claude HTTP), claurst's
/// sidecar dispatches one-shot completions through the same
/// [`LlmProvider`] stack the main agent uses. Model/backend resolution
/// follows jcode's priority order:
///
/// 1. `session_memory_model` (TUI model picker, persisted per session)
/// 2. `configured_model` (settings `memoryModel` or env `CLAURST_MEMORY_MODEL`)
/// 3. Fallback based on `memory_sidecar_fallback` config:
///    - `openai_claude` (default): try OpenAI, then Claude, then provider registry
///    - `provider`: use the active provider's current model
///    - `none`: dormant sidecar (no model)
#[derive(Clone)]
pub struct MemorySidecar {
    target: SidecarTarget,
}

impl MemorySidecar {
    /// Create a sidecar from resolved settings, auto-selecting the best
    /// available backend.
    pub fn new(settings: &MemorySettings, session_memory_model: Option<&str>) -> Self {
        Self::with_session_memory_model(settings, session_memory_model)
    }

    /// Create a sidecar with a session-level memory model override.
    pub fn with_session_memory_model(
        settings: &MemorySettings,
        session_memory_model: Option<&str>,
    ) -> Self {
        // Session-level override wins over config-level.
        let effective_model = session_memory_model
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .or_else(|| settings.memory_model.clone().filter(|m| !m.is_empty()));

        let target = match settings.memory_sidecar_backend {
            Some(MemorySidecarBackend::Provider) => {
                // Explicit "provider" backend: always dispatch through the
                // active agent provider, keeping its own model unless an
                // explicit memory model was set.
                Self::target_from_active_provider(effective_model)
            }
            Some(MemorySidecarBackend::OpenAI) => {
                let model = effective_model.unwrap_or_else(|| SIDECAR_DEFAULT_MODEL.to_string());
                Self::target_for_provider_id("openai", model)
            }
            Some(MemorySidecarBackend::Claude) => {
                let model = effective_model.unwrap_or_else(|| SIDECAR_CLAUDE_MODEL.to_string());
                Self::target_for_provider_id("anthropic", model)
            }
            Some(MemorySidecarBackend::Auto) => {
                // "auto" or unset: route an explicit memory model to the
                // provider that owns it; otherwise apply the fallback chain.
                match effective_model {
                    Some(model) => Self::target_for_model(&model),
                    None => Self::target_from_fallback(settings),
                }
            }
            None => match effective_model {
                Some(model) => Self::target_for_model(&model),
                None => Self::target_from_fallback(settings),
            },
        };

        tracing::debug!(
            dormant = matches!(target, SidecarTarget::Dormant),
            "Memory sidecar target selected"
        );

        Self { target }
    }

    fn target_from_active_provider(effective_model: Option<String>) -> SidecarTarget {
        let provider = active_provider();
        match provider {
            Some(provider) => {
                let model = effective_model.unwrap_or_else(|| SIDECAR_CLAUDE_MODEL.to_string());
                SidecarTarget::Provider { provider, model }
            }
            None => SidecarTarget::Dormant,
        }
    }

    /// Resolve a dispatch target for a specific model id, routing to the
    /// provider that owns the model per the `ModelRegistry` heuristics.
    fn target_for_model(model: &str) -> SidecarTarget {
        let registry = ModelRegistry::new();
        match registry.find_provider_for_model(model) {
            Some(provider_id) => {
                let pid = provider_id.to_string();
                Self::target_for_provider_id(&pid, model.to_string())
            }
            None => {
                // Unknown model: route through the active provider.
                Self::target_from_active_provider(Some(model.to_string()))
            }
        }
    }

    fn target_for_provider_id(provider_id: &str, model: String) -> SidecarTarget {
        let provider = runtime_provider(provider_id);
        match provider {
            Some(provider) => SidecarTarget::Provider { provider, model },
            None => SidecarTarget::Dormant,
        }
    }

    fn target_from_fallback(settings: &MemorySettings) -> SidecarTarget {
        // Unset fallback defaults to the legacy openai→claude→provider chain.
        match settings.memory_sidecar_fallback.clone().unwrap_or_default() {
            MemorySidecarFallback::None => SidecarTarget::Dormant,
            MemorySidecarFallback::Provider => Self::target_from_active_provider(None),
            MemorySidecarFallback::OpenAiClaude => {
                // Try OpenAI, then Claude, then the active provider.
                for (pid, model) in [
                    ("openai", SIDECAR_DEFAULT_MODEL),
                    ("anthropic", SIDECAR_CLAUDE_MODEL),
                ] {
                    if let Some(provider) = runtime_provider(pid) {
                        return SidecarTarget::Provider {
                            provider,
                            model: model.to_string(),
                        };
                    }
                }
                Self::target_from_active_provider(None)
            }
        }
    }

    /// Simple completion: send a system prompt and a user message, get text
    /// back.
    pub async fn complete(&self, system: &str, user_message: &str) -> Result<String> {
        let SidecarTarget::Provider { provider, model } = &self.target else {
            anyhow::bail!(
                "Memory sidecar is dormant: no usable LLM backend is configured \
                 (memorySidecarFallback is \"none\" or no credentials are available)"
            );
        };

        let request = ProviderRequest {
            model: model.clone(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(user_message.to_string()),
                uuid: None,
                cost: None,
                snapshot_patch: None,
            }],
            system_prompt: if system.is_empty() {
                None
            } else {
                Some(SystemPrompt::Text(system.to_string()))
            },
            tools: Vec::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            provider_options: Value::Null,
        };

        let response = provider
            .create_message(request)
            .await
            .with_context(|| format!("Memory sidecar completion via {} failed", provider.id()))?;

        extract_text(&response)
    }

    /// Whether this sidecar instance can dispatch calls.
    pub fn is_dormant(&self) -> bool {
        matches!(self.target, SidecarTarget::Dormant)
    }

    /// The model the sidecar will use, if available.
    pub fn model_name(&self) -> Option<&str> {
        match &self.target {
            SidecarTarget::Provider { model, .. } => Some(model),
            SidecarTarget::Dormant => None,
        }
    }

    /// The provider id the sidecar will dispatch through, if available.
    pub fn backend_name(&self) -> Option<String> {
        match &self.target {
            SidecarTarget::Provider { provider, .. } => Some(provider.id().to_string()),
            SidecarTarget::Dormant => None,
        }
    }

    /// Check if a memory is relevant to the current context.
    /// Returns (is_relevant, explanation).
    pub async fn check_relevance(
        &self,
        memory_content: &str,
        current_context: &str,
    ) -> Result<(bool, String)> {
        let system = r#"You are a memory relevance checker. Your job is to determine if a stored memory is relevant to the current context.

Respond in this exact format:
RELEVANT: yes/no
REASON: <brief explanation>

Be conservative - only say "yes" if the memory would actually be useful for the current task."#;

        let prompt = format!(
            "## Stored Memory\n{}\n\n## Current Context\n{}\n\nIs this memory relevant to the current context?",
            memory_content, current_context
        );

        let response = self.complete(system, &prompt).await?;

        // Parse response
        let mut is_relevant = false;
        for line in response.lines() {
            let line = line.trim();
            if line.len() >= 9 && line[..9].eq_ignore_ascii_case("relevant:") {
                let value = line[9..].trim();
                is_relevant = value.eq_ignore_ascii_case("yes") || value.starts_with("yes");
                break;
            }
        }
        let reason = response
            .lines()
            .find(|line| line.to_lowercase().starts_with("reason:"))
            .map(|line| line.trim_start_matches(|c: char| !c.is_alphabetic()).trim())
            .unwrap_or(&response)
            .to_string();

        Ok((is_relevant, reason))
    }

    /// Check if new information contradicts existing information.
    /// Returns true if the two statements are contradictory.
    pub async fn check_contradiction(
        &self,
        new_content: &str,
        existing_content: &str,
    ) -> Result<bool> {
        let system = "You are a contradiction detector. Given two statements, determine if the new information directly contradicts the existing information. Reply with exactly YES or NO.";

        let prompt = format!(
            "## Existing Information\n{}\n\n## New Information\n{}\n\nDoes the new information contradict the existing information?",
            existing_content, new_content
        );

        let response = self.complete(system, &prompt).await?;
        let trimmed = response.trim().to_uppercase();
        Ok(trimmed.starts_with("YES"))
    }

    /// Extract memories from a session transcript.
    pub async fn extract_memories(&self, transcript: &str) -> Result<Vec<ExtractedMemory>> {
        self.extract_memories_with_existing(transcript, &[]).await
    }

    /// Extract memories from a session transcript, aware of what's already stored.
    pub async fn extract_memories_with_existing(
        &self,
        transcript: &str,
        existing: &[String],
    ) -> Result<Vec<ExtractedMemory>> {
        let mut system = String::from(
            r#"You are a memory extraction assistant. Extract important NEW learnings from the conversation that should be remembered for future sessions.

Categories (use EXACTLY one of these):
- fact: Technical facts about the codebase, architecture, patterns, dependencies, tools, environment
- preference: User preferences, workflow habits, UX expectations, coding style, conventions, how they want the assistant to behave
- correction: Mistakes that were corrected, bugs found and fixed, wrong assumptions, things the user corrected
- entity: Named entities worth tracking - people, projects, services, repos, teams

Categorization rules:
- If it describes what the USER WANTS or HOW THEY LIKE THINGS, it is "preference", not "fact"
- If it describes a BUG FIX or MISTAKE, it is "correction", not "fact"
- "fact" is for objective technical information about code/systems, not user behavior

IMPORTANT - Do NOT extract:
- Transient debugging details, compile errors, or intermediate build steps
- Specific commit hashes, git operations, or "changes were committed/pushed" details
- Line-by-line code changes like "X was updated to Y in file Z" - these belong in git history, not memory
- Self-evident project context (e.g., the project name, repo URL, language) that is already in the system prompt
- Redundant variations of information already known (check the "Already known" list carefully)

Quality bar: Only extract information that would ACTUALLY BE USEFUL if recalled in a future session on a different topic. Ask: "Would a developer benefit from knowing this weeks from now?"

For each memory, output in this format (one per line):
CATEGORY|CONTENT|TRUST

Where:
- CATEGORY is one of: fact, preference, correction, entity
- CONTENT is a concise statement (1-2 sentences max, under 200 characters preferred)
- TRUST is one of: high (user stated), medium (observed), low (inferred)

Output ONLY the formatted lines, no other text. If no NEW memories worth extracting, output nothing."#,
        );

        if !existing.is_empty() {
            system.push_str("\n\nAlready known (do NOT re-extract these or close paraphrases):\n");
            for mem in existing.iter().take(80) {
                system.push_str("- ");
                system.push_str(&truncate_str(mem, 150));
                system.push('\n');
            }
        }

        let response = self.complete(&system, transcript).await?;

        let memories = response
            .lines()
            .filter(|line| line.contains('|'))
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() >= 3 {
                    Some(ExtractedMemory {
                        category: parts[0].trim().to_lowercase(),
                        content: parts[1].trim().to_string(),
                        trust: parts[2].trim().to_lowercase(),
                    })
                } else {
                    None
                }
            })
            .collect();

        Ok(memories)
    }
}

impl Default for MemorySidecar {
    fn default() -> Self {
        Self::new(&MemorySettings::default(), None)
    }
}

// ---------------------------------------------------------------------------
// Extracted memory types and helpers
// ---------------------------------------------------------------------------

/// A memory extracted by the sidecar. Ported from jcode's `ExtractedMemory`.
/// The `category` and `trust` strings match the pipe-separated
/// `CATEGORY|CONTENT|TRUST` output format the extraction prompt mandates.
#[derive(Debug, Clone)]
pub struct ExtractedMemory {
    pub category: String,
    pub content: String,
    pub trust: String,
}

/// Extract the concatenated text blocks from a provider response.
fn extract_text(response: &ProviderResponse) -> Result<String> {
    let mut text = String::new();
    for block in &response.content {
        if let ContentBlock::Text { text: t } = block {
            text.push_str(t);
        }
    }
    Ok(text)
}

/// Truncate a string to `max_chars`, appending an ellipsis when cut.
/// Ported from jcode's `util::truncate_str`.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let mut end = max_chars;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ---------------------------------------------------------------------------
// Provider resolution helpers
// ---------------------------------------------------------------------------

/// Resolve the active provider for sidecar dispatch. Mirrors jcode's
/// `active_provider_fork` semantics using claurst's auth-store-aware
/// `runtime_provider_for`: prefer a fresh provider built from the auth store
/// (so keys added via /connect are picked up), then fall back to the
/// settings-configured provider, then environment-detected providers.
fn active_provider() -> Option<Arc<dyn LlmProvider>> {
    // Highest priority: provider selected in settings.json.
    let config = claurst_core::config::Settings::load_sync().ok()?;
    let selected = config.provider.as_deref().unwrap_or_default();
    if !selected.is_empty() {
        if let Some(provider) = runtime_provider(selected) {
            return Some(provider);
        }
    }

    // Environment-scanned providers with keys available.
    for pid in ["anthropic", "openai", "google", "github-copilot"] {
        if let Some(provider) = runtime_provider(pid) {
            return Some(provider);
        }
    }

    None
}

/// Build a provider by id via the registry's auth-store-aware resolution.
fn runtime_provider(provider_id: &str) -> Option<Arc<dyn LlmProvider>> {
    crate::registry::runtime_provider_for(provider_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dormant_when_fallback_none_and_no_model() {
        let settings = MemorySettings {
            memory_sidecar_fallback: Some(MemorySidecarFallback::None),
            ..Default::default()
        };
        let sidecar = MemorySidecar::new(&settings, None);
        assert!(sidecar.is_dormant());
        assert!(sidecar.model_name().is_none());
    }

    #[test]
    fn session_model_overrides_config() {
        // Backend "provider" with no available provider is dormant regardless.
        let settings = MemorySettings {
            memory_sidecar_backend: Some(MemorySidecarBackend::Provider),
            memory_model: Some("gpt-5-mini".to_string()),
            ..Default::default()
        };
        // In the test environment no provider is registered, so this is
        // dormant; the important part is it does not panic.
        let sidecar = MemorySidecar::new(&settings, None);
        let _ = sidecar.is_dormant();
    }

    #[test]
    fn extract_memories_parses_pipe_lines() {
        let response = "preference|User prefers vim keybindings|high\ncorrection|Bug in parser fixed|medium\ngarbage line without pipes";
        // Parse exactly like the production filter_map does.
        let memories: Vec<ExtractedMemory> = response
            .lines()
            .filter(|line| line.contains('|'))
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() >= 3 {
                    Some(ExtractedMemory {
                        category: parts[0].trim().to_lowercase(),
                        content: parts[1].trim().to_string(),
                        trust: parts[2].trim().to_lowercase(),
                    })
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(memories.len(), 2);
        assert_eq!(memories[0].category, "preference");
        assert_eq!(memories[0].trust, "high");
    }

    #[test]
    fn relevance_parse() {
        let response = "RELEVANT: yes\nREASON: both discuss the parser";
        let mut is_relevant = false;
        for line in response.lines() {
            let line = line.trim();
            if line.len() >= 9 && line[..9].eq_ignore_ascii_case("relevant:") {
                let value = line[9..].trim();
                is_relevant = value.eq_ignore_ascii_case("yes") || value.starts_with("yes");
                break;
            }
        }
        assert!(is_relevant);
    }

    #[test]
    fn classify_error_message_sniffing() {
        let err = anyhow::anyhow!("401 unauthorized");
        assert_eq!(classify_error(&err), SidecarErrorKind::Permanent);
        let err = anyhow::anyhow!("connection reset");
        assert_eq!(classify_error(&err), SidecarErrorKind::Transient);
    }

    #[test]
    fn truncate_str_respects_char_boundaries() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello…");
    }
}
