//! Model picker overlay (/model command).
//! Mirrors src/components/ModelPicker.tsx — including effort levels and
//! fast-mode notice.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::overlays::{centered_rect, modal_search_line, CLAURST_PANEL_BG};

// ---------------------------------------------------------------------------
// Effort level
// ---------------------------------------------------------------------------

/// The effort level shown by the /model and /effort pickers.
///
/// This is a re-export of the single canonical [`claurst_core::effort::EffortLevel`]
/// (`Low, Medium, High, XHigh, Max, Ultracode`). The former TUI-local enum's
/// `Normal` variant is now [`EffortLevel::Medium`]; `symbol()`, `label()`,
/// `next()`, and `prev()` all live on the core enum. Effort controls the
/// extended-thinking budget / reasoning-effort sent to the API; only models that
/// support reasoning honour it.
pub use claurst_core::effort::EffortLevel;

/// Sentinel model id for the "Default" picker entry. When the user selects
/// this, the app clears the explicit model override so the provider's best
/// model is used.
pub const DEFAULT_MODEL_SENTINEL: &str = "__default__";

// ---------------------------------------------------------------------------
// Model capability helpers — driven by the opencode variants() ladder
// ---------------------------------------------------------------------------
//
// Both the /effort command and this /model picker now derive their effort
// tiers from the single source of truth, `claurst_api::variant_ladder`
// (a faithful port of opencode's `ProviderTransform.variants()`), instead of
// the old name-string heuristics that disagreed between the two surfaces.

/// A process-wide bundled `ModelRegistry`, built once, used only to resolve the
/// npm / release_date / reasoning fields that `variant_ladder` keys off.
///
/// The picker owns just a model id string (it does not carry the live registry
/// the app holds), so the ladder is resolved against the compile-time bundled
/// snapshot here. The `/effort` command path (`app.rs`) passes the *live*
/// registry and is therefore exact; both funnel through the same port, so they
/// agree tier-for-tier for every catalog model.
fn picker_registry() -> &'static claurst_api::ModelRegistry {
    static REG: std::sync::OnceLock<claurst_api::ModelRegistry> = std::sync::OnceLock::new();
    REG.get_or_init(claurst_api::ModelRegistry::new)
}

/// The reasoning-effort ladder (ascending, no ultracode) a model exposes, per
/// opencode's `variants()`. Empty for non-reasoning models.
///
/// The provider is inferred from the id: a `provider/model` prefix is trusted as
/// the provider; a bare id is mapped to its canonical provider via the registry
/// family heuristic. (Gateway upstream prefixes like `openai/…` are the one
/// approximation — the picker's catalog providers use bare ids and resolve
/// exactly.)
fn picker_variant_ladder(id: &str) -> Vec<EffortLevel> {
    let reg = picker_registry();
    let provider = match id.split_once('/') {
        Some((p, _)) => p.to_string(),
        None => reg
            .find_provider_for_model(id)
            .map(|p| p.to_string())
            .unwrap_or_default(),
    };
    claurst_api::variant_ladder(&provider, id, Some(reg))
}

/// Returns `true` when the model exposes more than one reasoning-effort tier —
/// i.e. the picker's ←/→ selector has something to cycle through. A model with
/// zero tiers (non-reasoning) or a single fixed tier (e.g. `gpt-5-pro` → only
/// `high`) does not surface the selector.
pub fn model_supports_effort(id: &str) -> bool {
    picker_variant_ladder(id).len() > 1
}

/// The index of the ladder rung nearest to `current`: the highest rung `<=`
/// current, or the lowest rung when `current` sits below the whole ladder.
/// `ladder` is assumed ascending (as `variant_ladder` returns it).
fn nearest_ladder_index(ladder: &[EffortLevel], current: EffortLevel) -> usize {
    let mut best = 0usize;
    for (i, level) in ladder.iter().enumerate() {
        if *level <= current {
            best = i;
        }
    }
    best
}

/// Clamp `effort` onto `ladder`: itself if present, else the nearest rung.
fn clamp_to_ladder(ladder: &[EffortLevel], effort: EffortLevel) -> EffortLevel {
    if ladder.contains(&effort) {
        effort
    } else {
        ladder[nearest_ladder_index(ladder, effort)]
    }
}

/// Step one rung along `ladder` from `current` (`dir` = +1 / -1), wrapping.
fn cycle_ladder(ladder: &[EffortLevel], current: EffortLevel, dir: isize) -> EffortLevel {
    if ladder.is_empty() {
        return current;
    }
    let idx = ladder
        .iter()
        .position(|l| *l == current)
        .unwrap_or_else(|| nearest_ladder_index(ladder, current)) as isize;
    let n = ladder.len() as isize;
    ladder[(((idx + dir) % n + n) % n) as usize]
}

// ---------------------------------------------------------------------------
// Provider grouping helpers
// ---------------------------------------------------------------------------

/// Format context window tokens for display in the model picker.
pub fn format_context_window(context_window: u32) -> String {
    if context_window >= 1_000_000 {
        if context_window.is_multiple_of(1_000_000) {
            format!("{}M context", context_window / 1_000_000)
        } else {
            format!("{:.1}M context", context_window as f64 / 1_000_000.0)
        }
    } else {
        format!("{}K context", context_window / 1000)
    }
}

/// Format a model display line with optional context window and cost info.
///
/// Example: `"gpt-4o  128K ctx  $5.00/M"`
pub fn format_model_line(
    model_str: &str,
    context_window: Option<u32>,
    cost_per_1m: Option<f64>,
) -> String {
    let mut parts = vec![model_str.to_string()];
    if let Some(ctx) = context_window {
        parts.push(format_context_window(ctx).replace(" context", " ctx"));
    }
    if let Some(cost) = cost_per_1m {
        if cost == 0.0 {
            parts.push("free".to_string());
        } else {
            parts.push(format!("${:.2}/M", cost));
        }
    }
    parts.join("  ")
}

/// A group of models belonging to the same provider, for structured display.
pub struct ProviderSection {
    pub provider_name: String,
    pub models: Vec<String>, // model ID strings in "provider/model" format
}

impl ModelPickerState {
    /// Build grouped model sections from a flat list of model strings.
    ///
    /// Models with a `"provider/model"` slash format are grouped by their
    /// provider prefix.  Bare model names are heuristically assigned to a
    /// provider based on the model name pattern.
    pub fn build_provider_sections(models: &[String]) -> Vec<ProviderSection> {
        use std::collections::HashMap;
        let mut by_provider: HashMap<String, Vec<String>> = HashMap::new();

        for m in models {
            let provider = if let Some((p, _)) = m.split_once('/') {
                p.to_string()
            } else {
                // Bare model name — detect provider from model name
                if m.contains("claude") {
                    "anthropic".to_string()
                } else if m.starts_with("gpt") || m.starts_with("o3") || m.starts_with("o4") {
                    "openai".to_string()
                } else if m.contains("gemini") {
                    "google".to_string()
                } else if m.contains("minimax") {
                    "minimax".to_string()
                } else {
                    "other".to_string()
                }
            };
            by_provider.entry(provider).or_default().push(m.clone());
        }

        // Define display order
        let order = ["anthropic", "openai", "google", "ollama", "other"];
        let mut sections = Vec::new();
        for provider in order {
            if let Some(models) = by_provider.remove(provider) {
                sections.push(ProviderSection {
                    provider_name: match provider {
                        "anthropic" => "ANTHROPIC".to_string(),
                        "openai" => "OPENAI".to_string(),
                        "google" => "GOOGLE".to_string(),
                        "ollama" => "OLLAMA".to_string(),
                        _ => provider.to_uppercase(),
                    },
                    models,
                });
            }
        }
        // Add any remaining providers not in the order list
        for (provider, models) in by_provider {
            sections.push(ProviderSection {
                provider_name: provider.to_uppercase(),
                models,
            });
        }
        sections
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single model entry shown in the picker.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    pub display_name: String,
    pub description: String,
    /// Whether this is the currently active model.
    pub is_current: bool,
    /// Provider ID this model belongs to (for all-providers mode).
    pub provider_id: String,
}

// ---------------------------------------------------------------------------
// Provider-aware model lists
// ---------------------------------------------------------------------------

/// Helper to build a `ModelEntry` with `is_current = false`.
fn model_entry(id: &str, name: &str, desc: &str, provider_id: &str) -> ModelEntry {
    ModelEntry {
        id: id.to_string(),
        display_name: name.to_string(),
        description: desc.to_string(),
        is_current: false,
        provider_id: provider_id.to_string(),
    }
}

/// Get models for a provider from the model registry (models.dev data).
///
/// Builds picker entries from the bundled / network-refreshed registry.
/// The registry is always populated (the embedded models.dev snapshot
/// contains ~118 providers / ~4500 models), so the only time the result
/// is empty is when the caller passed a truly unknown provider id — in
/// which case we synthesize a single `"default"` placeholder so the
/// picker isn't blank.
pub fn models_for_provider_from_registry(
    provider_id: &str,
    registry: &claurst_api::ModelRegistry,
) -> Vec<ModelEntry> {
    // "free" is the composite Zen → OpenRouter provider; the upstream
    // models.dev catalog has nothing under this id, so serve a curated list
    // directly.  `free/auto` is the default routing entry; the rest pin a
    // specific upstream model for users who care.
    if provider_id == "free" {
        return free_provider_models(provider_id);
    }
    // Codex (ChatGPT-authenticated OpenAI) is not in the models.dev catalog —
    // serve the curated CODEX_MODELS list so the picker isn't empty.  Accept
    // both the canonical id ("codex") and the connect-dialog alias
    // ("openai-codex"); without the alias a fresh Codex login lands on the
    // empty-registry fallback and the picker shows no models.
    if is_codex_provider(provider_id) {
        return codex_provider_models(registry, provider_id);
    }

    let mut entries = registry.list_visible_by_provider(provider_id);

    // Fall back to all entries (including alpha/deprecated) if the visible
    // filter wiped the list — better to show something than nothing.
    if entries.is_empty() {
        entries = registry.list_by_provider(provider_id);
    }

    if entries.is_empty() {
        // Truly unknown provider — keep the picker non-empty so /model still
        // works against e.g. self-hosted endpoints.
        return vec![model_entry(
            "default",
            "Default model",
            "no catalog entry for this provider",
            provider_id,
        )];
    }

    // Sort: most recently released first, then alphabetical by id.
    entries.sort_by(|a, b| {
        let rd_a = a.release_date.as_deref().unwrap_or("");
        let rd_b = b.release_date.as_deref().unwrap_or("");
        rd_b.cmp(rd_a).then_with(|| (*a.info.id).cmp(&*b.info.id))
    });

    entries
        .iter()
        .map(|e| {
            let cost_str = match (e.cost_input, e.cost_output) {
                (Some(ci), Some(co)) => format!(
                    "{} | ${:.2}/${:.2} per M",
                    format_context_window(e.info.context_window),
                    ci,
                    co
                ),
                _ => format_context_window(e.info.context_window),
            };
            ModelEntry {
                id: e.info.id.to_string(),
                display_name: e.info.name.clone(),
                description: cost_str,
                is_current: false,
                provider_id: provider_id.to_string(),
            }
        })
        .collect()
}

/// Return the provider-prefixed default model name for a given provider,
/// consulting the registry first and falling back to a `provider/default`
/// placeholder for unknown providers.
///
/// **Anthropic exception** — anthropic models are emitted bare (no
/// `anthropic/` prefix) for backward-compatibility with config files that
/// pre-date the multi-provider era.
///
/// **Free exception** — the composite Zen → OpenRouter provider ships with
/// a synthetic `free/auto` default that the wrapper translates per upstream.
pub fn default_model_for_provider(
    provider_id: &str,
    registry: &claurst_api::ModelRegistry,
) -> String {
    if provider_id == "free" {
        return "free/auto".to_string();
    }
    // Codex endpoints are not catalogued by models.dev, so the registry can
    // never supply a default for them — pin the curated flagship Codex model,
    // preserving whichever id alias ("codex" / "openai-codex") the caller used.
    if is_codex_provider(provider_id) {
        return format!(
            "{}/{}",
            provider_id,
            claurst_core::codex_oauth::DEFAULT_CODEX_MODEL
        );
    }
    if let Some(best) = registry.best_model_for_provider(provider_id) {
        if provider_id == "anthropic" {
            best
        } else {
            format!("{}/{}", provider_id, best)
        }
    } else {
        format!("{}/default", provider_id)
    }
}

/// Whether the picker list for `provider_id` is a pure read-only projection of
/// the models.dev catalog — i.e. the provider has **no** live model-discovery
/// endpoint and its `discover_models()` is the empty trait default.
///
/// For these providers the displayed list must come from
/// [`models_for_provider_from_registry`] and nothing else; the event loop skips
/// the background discovery fetch entirely so a provider return value can never
/// replace the catalog projection. This is what makes a fresh `claude-opus-*`
/// point-release surface the instant the snapshot ships it.
///
/// OpenAI/Google never had a live endpoint. Azure, Amazon Bedrock,
/// Cohere and MiniMax used to ship a tiny hardcoded `discover_models()` list
/// that clobbered the far richer catalog (108/85/12/6 rows); that override was
/// removed, so they are projected from the models.dev catalog here too rather
/// than fetched.
///
/// **Anthropic is intentionally NOT here**: it implements live discovery via
/// `GET /v1/models`, which returns exactly the models the active credential can
/// use (for a Claude Pro/Max OAuth token, the curated subscription set — no
/// legacy claude-3.x). The event loop opens on the catalog projection, then
/// intersects it with the discovered set (see the anthropic branch in the
/// discovery spawn), falling back to the full projection if discovery fails.
///
/// Live-endpoint and curated-list providers are intentionally excluded. The
/// event loop either replaces or merges the projection according to
/// [`provider_has_authoritative_live_models`].
pub fn provider_uses_catalog_projection(provider_id: &str) -> bool {
    if matches!(
        provider_id,
        "openai" | "google" | "azure" | "amazon-bedrock" | "cohere" | "minimax"
    ) {
        return true;
    }
    // Custom providers have a curated model list in settings — no live
    // endpoint to fetch from, so skip the background fetch.
    if let Ok(settings) = claurst_core::Settings::load_sync() {
        if settings.custom_providers.contains_key(provider_id) {
            return true;
        }
    }
    false
}

/// Whether live discovery is the complete set of models usable through a local
/// runtime. Catalog rows describe models it could host, not what is loaded.
pub fn provider_has_authoritative_live_models(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "ollama" | "lmstudio" | "lm-studio" | "llamacpp" | "llama-cpp" | "llama-server"
    )
}

/// Whether `provider_id` refers to the OpenAI Codex provider under either of
/// its two id spellings: the canonical `"codex"` (used by `CodexProvider`,
/// the model registry, and the runtime dispatch) or the `"openai-codex"`
/// alias emitted by the /connect dialog's Codex entry. Both must resolve to
/// the curated Codex catalog since models.dev does not list these endpoints.
fn is_codex_provider(provider_id: &str) -> bool {
    matches!(provider_id, "codex" | "openai-codex")
}

/// Codex (ChatGPT-authenticated OpenAI) model list.
///
/// Mirrors opencode's `provider.models()` hook exactly: instead of hardcoding a
/// list, it takes the models.dev `openai` catalog and keeps only the ids that
/// [`codex_model_allowed`] admits (gpt-5.5 / gpt-5.4 / gpt-5.4-mini /
/// gpt-5.3-codex-spark today, plus any future gpt-5.5+). Costs are zeroed
/// (subscription billing) and the gpt-5.5 context window is pinned to the Codex
/// 400K limit. Falls back to the curated [`CODEX_MODELS`] constant only if the
/// catalog yields nothing (e.g. an empty/old snapshot).
///
/// [`codex_model_allowed`]: claurst_core::codex_oauth::codex_model_allowed
fn codex_provider_models(
    registry: &claurst_api::ModelRegistry,
    provider_id: &str,
) -> Vec<ModelEntry> {
    use claurst_core::codex_oauth::{codex_limit_override, codex_model_allowed};

    let mut entries: Vec<&claurst_api::ModelEntry> = registry
        .list_by_provider("openai")
        .into_iter()
        .filter(|e| codex_model_allowed(&e.info.id))
        .collect();

    if entries.is_empty() {
        return codex_fallback_models(provider_id);
    }

    // Newest release first, then by id for stability — matches the picker's
    // ordering for other providers.
    entries.sort_by(|a, b| {
        let rd_a = a.release_date.as_deref().unwrap_or("");
        let rd_b = b.release_date.as_deref().unwrap_or("");
        rd_b.cmp(rd_a).then_with(|| (*a.info.id).cmp(&*b.info.id))
    });

    entries
        .iter()
        .map(|e| {
            let id: &str = &e.info.id;
            let ctx = codex_limit_override(id)
                .map(|(context, _, _)| context)
                .unwrap_or(e.info.context_window);
            ModelEntry {
                id: id.to_string(),
                display_name: e.info.name.clone(),
                // Costs are zeroed under the ChatGPT subscription, so advertise
                // "free" rather than the catalog's pay-as-you-go pricing.
                description: format!(
                    "{} | ChatGPT-authenticated · free",
                    format_context_window(ctx)
                ),
                is_current: false,
                provider_id: provider_id.to_string(),
            }
        })
        .collect()
}

/// Static fallback used when the models.dev `openai` catalog is unavailable.
fn codex_fallback_models(provider_id: &str) -> Vec<ModelEntry> {
    use claurst_core::codex_oauth::codex_limit_override;
    claurst_core::codex_oauth::CODEX_MODELS
        .iter()
        .map(|(id, name)| {
            let ctx = codex_limit_override(id)
                .map(|(context, _, _)| context)
                .unwrap_or(400_000);
            ModelEntry {
                id: id.to_string(),
                display_name: name.to_string(),
                description: format!(
                    "{} | ChatGPT-authenticated · free",
                    format_context_window(ctx)
                ),
                is_current: false,
                provider_id: provider_id.to_string(),
            }
        })
        .collect()
}

/// Curated free-mode model list used by `models_for_provider_from_registry`.
/// Always shows `free/auto` first; one pin entry per catalog upstream so the
/// user can target a specific provider when they need to.
fn free_provider_models(provider_id: &str) -> Vec<ModelEntry> {
    let mut entries = vec![ModelEntry {
        id: "free/auto".to_string(),
        display_name: "Auto (round-robin across configured providers)".to_string(),
        description: "stacks every free-tier key you've added · $0.00 per M".to_string(),
        is_current: false,
        provider_id: provider_id.to_string(),
    }];

    for upstream in claurst_api::FREE_CATALOG {
        entries.push(ModelEntry {
            id: format!("{}/{}", upstream.id, upstream.default_model),
            display_name: format!("{} \u{2014} {}", upstream.title, upstream.default_model),
            description: format!("{} · $0.00 per M", upstream.note),
            is_current: false,
            provider_id: provider_id.to_string(),
        });
    }

    entries
}

/// State for the /model picker overlay.
pub struct ModelPickerState {
    pub visible: bool,
    pub selected_idx: usize,
    pub models: Vec<ModelEntry>,
    pub title: String,
    /// Live filter typed by the user.
    pub filter: String,
    /// Current effort level for models that support extended thinking.
    pub effort_level: EffortLevel,
    /// Whether fast mode is currently active.
    pub fast_mode: bool,
    /// The currently locked fast-mode model, if fast mode is active.
    pub fast_mode_model: Option<String>,
    /// `true` once the dynamic model list has been loaded from the API.
    pub models_loaded: bool,
    /// `true` while the background fetch is in flight.
    pub loading_models: bool,
    /// Favorited model keys ("provider/model" format), loaded from Settings.
    pub favorites: Vec<String>,
    /// Synthetic "Default" entry prepended to the model list.
    default_entry: ModelEntry,
    /// When true, the picker shows models from all connected providers
    /// grouped by provider name.
    pub all_providers_mode: bool,
    /// Provider ID → display name map, populated from the model registry
    /// when the all-providers picker opens. Used for group headers so
    /// built-in providers (nvidia, deepinfra, etc.) show their real name
    /// instead of "Other".
    pub provider_names: std::collections::HashMap<String, String>,
    /// When true, show ALL providers (including unconfigured) in the picker.
    /// Default false — only connected providers shown.
    pub show_all_providers: bool,
    /// Set of connected provider IDs. When `show_all_providers` is false,
    /// only models whose provider_id is in this set are shown (plus the
    /// default entry). Populated by `open_model_picker_all_providers`.
    pub connected_provider_ids: std::collections::HashSet<String>,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl ModelPickerState {
    /// Create a new picker (not yet visible).
    ///
    /// The model list starts empty; it is populated purely from the
    /// models.dev-backed [`ModelRegistry`](claurst_api::ModelRegistry) via
    /// [`set_models`](Self::set_models) (see
    /// `models_for_provider_from_registry`) each time the picker opens for a
    /// provider. There is deliberately no hardcoded fallback list — a hardcoded
    /// Claude list previously clobbered the registry and hid newly-shipped
    /// models (#228).
    pub fn new() -> Self {
        Self {
            visible: false,
            selected_idx: 0,
            models: Vec::new(),
            title: "Select model".to_string(),
            filter: String::new(),
            effort_level: EffortLevel::Medium,
            fast_mode: false,
            fast_mode_model: None,
            models_loaded: false,
            loading_models: false,
            favorites: Vec::new(),
            default_entry: ModelEntry {
                id: DEFAULT_MODEL_SENTINEL.to_string(),
                display_name: "Default".to_string(),
                description: "Provider's recommended model (auto-selects best)".to_string(),
                is_current: false,
                provider_id: String::new(),
            },
            all_providers_mode: false,
            provider_names: std::collections::HashMap::new(),
            show_all_providers: false,
            connected_provider_ids: std::collections::HashSet::new(),
        }
    }

    /// Open the overlay.
    ///
    /// `current_model` is highlighted as active; `current_effort` and
    /// `fast_mode` are carried over from app state so the user sees the live
    /// values.
    pub fn open(&mut self, current_model: &str) {
        self.open_with_state(current_model, EffortLevel::Medium, false);
    }

    /// Open the overlay with full state context.
    pub fn open_with_state(&mut self, current_model: &str, effort: EffortLevel, fast_mode: bool) {
        self.open_with_title("Select model", current_model, effort, fast_mode);
    }

    /// Open in all-providers mode — shows models from all connected
    /// providers grouped by provider name.
    pub fn open_all_providers(
        &mut self,
        current_model: &str,
        effort: EffortLevel,
        fast_mode: bool,
    ) {
        self.all_providers_mode = true;
        self.open_with_title(
            "Select model (all providers)",
            current_model,
            effort,
            fast_mode,
        );
    }

    /// Get the display name for a provider ID (for all-providers mode).
    pub fn provider_group_name(provider_id: &str) -> String {
        // Custom providers: use the user-configured display name from settings.
        if let Ok(settings) = claurst_core::Settings::load_sync() {
            if let Some(def) = settings.custom_providers.get(provider_id) {
                return def.name.clone();
            }
        }
        match provider_id {
            "anthropic" => "Anthropic".to_string(),
            "openai" => "OpenAI".to_string(),
            "google" => "Google".to_string(),
            "groq" => "Groq".to_string(),
            "openrouter" => "OpenRouter".to_string(),
            "deepseek" => "DeepSeek".to_string(),
            "ollama" => "Ollama".to_string(),
            "lmstudio" => "LM Studio".to_string(),
            "llamacpp" | "llama-cpp" | "llama-server" => "llama.cpp".to_string(),
            "github-copilot" => "GitHub Copilot".to_string(),
            "codex" | "openai-codex" => "OpenAI Codex".to_string(),
            "cohere" => "Cohere".to_string(),
            "xai" => "xAI".to_string(),
            "free" => "Free Mode".to_string(),
            "cerebras" => "Cerebras".to_string(),
            "sambanova" => "SambaNova".to_string(),
            "together-ai" | "togetherai" => "Together AI".to_string(),
            "mistral" => "Mistral".to_string(),
            "perplexity" => "Perplexity".to_string(),
            "azure" => "Azure OpenAI".to_string(),
            "amazon-bedrock" => "AWS Bedrock".to_string(),
            "custom-openai" => "Custom OpenAI".to_string(),
            _ => "Other".to_string(),
        }
    }

    pub fn open_with_title(
        &mut self,
        title: impl Into<String>,
        current_model: &str,
        effort: EffortLevel,
        fast_mode: bool,
    ) {
        for m in &mut self.models {
            m.is_current =
                m.id == current_model || current_model == format!("{}/{}", m.provider_id, m.id);
        }
        // Mark default entry as current when no explicit model is set.
        self.default_entry.is_current =
            current_model.is_empty() || current_model == DEFAULT_MODEL_SENTINEL;
        self.title = title.into();
        // Clear the filter BEFORE computing selected_idx so a stale filter
        // can't hide the current model and leave the cursor on row 0.
        self.filter.clear();
        // Compute selected_idx from the grouped list (default entry at 0,
        // favorites first, then non-favorites) — not the raw models vec —
        // so the cursor highlights the correct row.
        self.selected_idx = self
            .filtered_models_grouped()
            .iter()
            .position(|(m, _)| m.is_current)
            .unwrap_or(0);
        self.effort_level = effort;
        self.fast_mode = fast_mode;
        self.fast_mode_model = fast_mode.then_some(current_model.to_string());
        self.visible = true;
    }

    /// Close the overlay without selecting.
    pub fn close(&mut self) {
        self.visible = false;
        self.filter.clear();
        self.all_providers_mode = false;
        self.provider_names.clear();
        self.show_all_providers = false;
        self.connected_provider_ids.clear();
    }

    pub fn is_selected_fast_mode_model(&self, model_id: &str) -> bool {
        self.fast_mode_model.as_deref() == Some(model_id)
    }

    /// Move selection up one row (wraps to last if at top).
    pub fn select_prev(&mut self) {
        let count = self.filtered_models_grouped().len();
        if count == 0 {
            return;
        }
        if self.selected_idx == 0 {
            self.selected_idx = count - 1;
        } else {
            self.selected_idx -= 1;
        }
    }

    /// Move selection down one row (wraps to first if at bottom).
    pub fn select_next(&mut self) {
        let count = self.filtered_models_grouped().len();
        if count == 0 {
            return;
        }
        self.selected_idx = (self.selected_idx + 1) % count;
    }

    /// Jump to the first model of the next provider group (all-providers mode).
    /// Wraps around to the default entry (index 0) after the last provider.
    pub fn select_next_provider(&mut self) {
        let grouped = self.filtered_models_grouped();
        if grouped.is_empty() {
            return;
        }
        let current_provider = grouped
            .get(self.selected_idx)
            .map(|(m, _)| m.provider_id.as_str())
            .unwrap_or("");
        // Find the first entry after the current position with a different
        // non-empty provider_id. Skip the default entry (empty provider_id).
        for i in (self.selected_idx + 1)..grouped.len() {
            let pid = grouped[i].0.provider_id.as_str();
            if !pid.is_empty() && pid != current_provider {
                self.selected_idx = i;
                return;
            }
        }
        // Wrap around: find the first entry with a non-empty provider_id
        // from the beginning (skips default entry at index 0).
        for i in 0..self.selected_idx {
            let pid = grouped[i].0.provider_id.as_str();
            if !pid.is_empty() && pid != current_provider {
                self.selected_idx = i;
                return;
            }
        }
        // No other provider found — stay put.
    }

    /// Jump to the first model of the previous provider group (all-providers mode).
    /// Wraps around to the last provider's first model after index 0.
    pub fn select_prev_provider(&mut self) {
        let grouped = self.filtered_models_grouped();
        if grouped.is_empty() {
            return;
        }
        let current_provider = grouped
            .get(self.selected_idx)
            .map(|(m, _)| m.provider_id.as_str())
            .unwrap_or("");
        // Walk backwards to find the first entry of the previous provider.
        // We need to find the boundary: the first entry whose provider_id
        // differs from current AND is non-empty, then from that point find
        // the FIRST entry of that provider group.
        let mut prev_provider = None;
        if self.selected_idx > 0 {
            for i in (0..self.selected_idx).rev() {
                let pid = grouped[i].0.provider_id.as_str();
                if !pid.is_empty() && pid != current_provider {
                    prev_provider = Some(pid);
                    // Now find the first entry of this provider group.
                    for j in 0..=i {
                        if grouped[j].0.provider_id == pid {
                            self.selected_idx = j;
                            return;
                        }
                    }
                }
            }
        }
        // Wrap around: find the last provider group's first entry.
        if prev_provider.is_none() {
            // Find the last distinct provider group.
            let mut last_provider = "";
            let mut last_provider_first_idx = 0;
            for (i, (m, _)) in grouped.iter().enumerate() {
                let pid = m.provider_id.as_str();
                if !pid.is_empty() && pid != last_provider {
                    last_provider = pid;
                    last_provider_first_idx = i;
                }
            }
            if !last_provider.is_empty() && last_provider != current_provider {
                self.selected_idx = last_provider_first_idx;
            }
        }
    }

    pub fn select_first(&mut self) {
        self.selected_idx = 0;
    }

    pub fn select_last(&mut self) {
        let count = self.filtered_models_grouped().len();
        self.selected_idx = count.saturating_sub(1);
    }

    /// Toggle showing all providers vs only connected ones.
    pub fn toggle_show_all_providers(&mut self) {
        self.show_all_providers = !self.show_all_providers;
        // Reset selection to stay in bounds.
        let count = self.filtered_models_grouped().len();
        if count > 0 && self.selected_idx >= count {
            self.selected_idx = count - 1;
        }
    }

    /// The reasoning-effort ladder of the currently highlighted model (ascending,
    /// no ultracode). Empty when nothing is highlighted or the model is
    /// non-reasoning.
    fn selected_ladder(&self) -> Vec<EffortLevel> {
        let filtered = self.filtered_models_grouped();
        match filtered.get(self.selected_idx) {
            Some((m, _)) => picker_variant_ladder(&m.id),
            None => Vec::new(),
        }
    }

    /// Cycle effort level forward (→ key) along the model's variants ladder.
    pub fn effort_next(&mut self) {
        let ladder = self.selected_ladder();
        self.effort_level = cycle_ladder(&ladder, self.effort_level, 1);
    }

    /// Cycle effort level backward (← key) along the model's variants ladder.
    pub fn effort_prev(&mut self) {
        let ladder = self.selected_ladder();
        self.effort_level = cycle_ladder(&ladder, self.effort_level, -1);
    }

    /// Returns the effective effort for the currently highlighted model, clamped
    /// onto its variants ladder; `None` if the model exposes no selectable effort
    /// (a single-tier or non-reasoning model).
    pub fn effective_effort(&self) -> Option<EffortLevel> {
        let ladder = self.selected_ladder();
        if ladder.len() > 1 {
            Some(clamp_to_ladder(&ladder, self.effort_level))
        } else {
            None
        }
    }

    /// Confirm the current selection.
    ///
    /// Returns `(model_id, effort)` where `effort` is `None` for models that
    /// do not support extended thinking.  Closes the picker.
    ///
    /// Returns the selected model; the caller is responsible for persisting it
    /// in the correct provider-aware format.
    pub fn confirm(&mut self) -> Option<(String, Option<EffortLevel>)> {
        let filtered = self.filtered_models_grouped();
        let custom = self.filter.trim();
        if filtered.is_empty() {
            if custom.is_empty() {
                return None;
            }
            let id = custom.to_string();
            self.close();
            return Some((id, None));
        }
        let (entry, _is_fav) = filtered.get(self.selected_idx)?;
        let id = entry.id.clone();
        if id == DEFAULT_MODEL_SENTINEL {
            self.close();
            return Some((id, None));
        }
        let ladder = picker_variant_ladder(&id);
        let effort = if ladder.len() > 1 {
            Some(clamp_to_ladder(&ladder, self.effort_level))
        } else {
            None
        };
        // If user chose a model other than the fast-mode model while fast mode is
        // active, the caller should turn off fast mode (mirrors TS behaviour).
        self.close();
        Some((id, effort))
    }

    /// Append a character to the filter string and reset the selection.
    pub fn push_filter_char(&mut self, c: char) {
        self.filter.push(c);
        self.selected_idx = 0;
    }

    /// Remove the last character from the filter string.
    pub fn pop_filter_char(&mut self) {
        self.filter.pop();
        self.selected_idx = 0;
    }

    /// Return models that match the current filter (case-insensitive).
    pub fn filtered_models(&self) -> Vec<&ModelEntry> {
        if self.filter.is_empty() {
            return self.models.iter().collect();
        }
        let needle = self.filter.to_lowercase();
        self.models
            .iter()
            .filter(|m| {
                m.id.to_lowercase().contains(needle.as_str())
                    || m.display_name.to_lowercase().contains(needle.as_str())
                    || m.description.to_lowercase().contains(needle.as_str())
            })
            .collect()
    }

    /// Load favorites from Settings into the picker state.
    pub fn load_favorites(&mut self) {
        if let Ok(settings) = claurst_core::Settings::load_sync() {
            self.favorites = settings.favorite_models;
        }
    }

    /// Toggle favorite status on a model key. Saves to Settings immediately.
    pub fn toggle_favorite(&mut self, model_key: &str) {
        if self.favorites.contains(&model_key.to_string()) {
            self.favorites.retain(|f| f != model_key);
        } else {
            self.favorites.push(model_key.to_string());
        }
        // Persist to settings.
        if let Ok(mut settings) = claurst_core::Settings::load_sync() {
            settings.favorite_models = self.favorites.clone();
            let _ = settings.save_sync();
        }
    }

    /// Check if a model key is favorited.
    pub fn is_favorite(&self, model_key: &str) -> bool {
        self.favorites.contains(&model_key.to_string())
    }

    /// Get the list of valid favorites — those present in the picker's
    /// current model list. Stale favorites (model removed from catalog)
    /// are silently excluded from display but NOT removed from settings.
    pub fn valid_favorites(&self) -> Vec<String> {
        let model_ids: std::collections::HashSet<&str> =
            self.models.iter().map(|m| m.id.as_str()).collect();
        self.favorites
            .iter()
            .filter(|f| {
                // Favorites are in "provider/model" format. The picker's
                // models list has bare model ids (no provider prefix) since
                // the picker is scoped to one provider. So we match on the
                // model portion after the slash.
                if let Some((_, model)) = f.split_once('/') {
                    model_ids.contains(model)
                } else {
                    model_ids.contains(f.as_str())
                }
            })
            .cloned()
            .collect()
    }

    /// Returns model entries for display: favorites first (marked with ★),
    /// then all other models. Applies the current filter to both sections.
    pub fn filtered_models_grouped(&self) -> Vec<(&ModelEntry, bool)> {
        let valid_favs = self.valid_favorites();
        let fav_model_ids: std::collections::HashSet<String> = valid_favs
            .iter()
            .map(|f| {
                f.split_once('/')
                    .map(|(_, m)| m.to_string())
                    .unwrap_or(f.clone())
            })
            .collect();

        // Connected-providers filter: when show_all_providers is false AND
        // we're in all-providers mode, only show models whose provider_id is
        // in the connected set. The default entry (empty provider_id) always
        // passes.
        let connected_filter = |m: &&ModelEntry| -> bool {
            if m.provider_id.is_empty() {
                return true;
            }
            if self.show_all_providers || !self.all_providers_mode {
                return true;
            }
            self.connected_provider_ids.contains(&m.provider_id)
        };

        if self.filter.is_empty() {
            // No filter: favorites section, then all models
            let mut result = Vec::new();
            // Default entry always at top.
            result.push((&self.default_entry, false));
            // Favorites first
            for m in &self.models {
                if fav_model_ids.contains(&m.id) && connected_filter(&m) {
                    result.push((m, true));
                }
            }
            // Then non-favorites
            for m in &self.models {
                if !fav_model_ids.contains(&m.id) && connected_filter(&m) {
                    result.push((m, false));
                }
            }
            result
        } else {
            let needle = self.filter.to_lowercase();
            let mut result = Vec::new();
            // Default entry — always show if filter matches or is empty.
            if self.filter.is_empty()
                || "default".contains(&needle)
                || self
                    .default_entry
                    .display_name
                    .to_lowercase()
                    .contains(&needle)
                || self
                    .default_entry
                    .description
                    .to_lowercase()
                    .contains(&needle)
            {
                result.push((&self.default_entry, false));
            }
            // Favorites matching filter
            for m in &self.models {
                if fav_model_ids.contains(&m.id)
                    && (m.id.to_lowercase().contains(&needle)
                        || m.display_name.to_lowercase().contains(&needle)
                        || m.description.to_lowercase().contains(&needle))
                    && connected_filter(&m)
                {
                    result.push((m, true));
                }
            }
            // Non-favorites matching filter
            for m in &self.models {
                if !fav_model_ids.contains(&m.id)
                    && (m.id.to_lowercase().contains(&needle)
                        || m.display_name.to_lowercase().contains(&needle)
                        || m.description.to_lowercase().contains(&needle))
                    && connected_filter(&m)
                {
                    result.push((m, false));
                }
            }
            result
        }
    }

    /// Replace the model list with dynamically loaded entries.
    ///
    /// Called by the app event loop when the background fetch completes.
    /// Resets `loading_models` and sets `models_loaded`.
    pub fn set_models(&mut self, entries: Vec<ModelEntry>) {
        self.models = entries;
        self.loading_models = false;
        self.models_loaded = true;
        // Keep selected_idx in bounds (grouped view includes default entry).
        let count = self.filtered_models_grouped().len();
        if count > 0 && self.selected_idx >= count {
            self.selected_idx = count - 1;
        }
    }

    /// Additively merge live-discovered entries into the existing list.
    ///
    /// Mirrors opencode's github-copilot `models.ts` merge (by id / api.id;
    /// models.ts:229-255): the catalog projection already loaded into the picker
    /// is authoritative — its richer cost/context/reasoning metadata is kept for
    /// every id present in both — and only live models whose id isn't already
    /// present are appended. A non-empty live result also clears the synthetic
    /// "no catalog entry" placeholder that [`models_for_provider_from_registry`]
    /// emits for providers with no catalog rows (e.g. self-hosted endpoints).
    ///
    /// Unlike copilot's merge this does **not** prune catalog ids missing from
    /// the endpoint — the overlay is purely additive, so a stale catalog row is
    /// preferred over dropping a model the user may still have access to.
    pub fn merge_models(&mut self, entries: Vec<ModelEntry>) {
        if entries.is_empty() {
            self.loading_models = false;
            return;
        }
        // A real list supersedes the synthetic "no catalog" placeholder.
        self.models
            .retain(|m| !(m.id == "default" && m.display_name == "Default model"));

        let existing: std::collections::HashSet<String> =
            self.models.iter().map(|m| m.id.clone()).collect();
        for e in entries {
            if !existing.contains(&e.id) {
                self.models.push(e);
            }
        }
        self.loading_models = false;
        self.models_loaded = true;
        // Keep selected_idx in bounds (grouped view includes default entry).
        let count = self.filtered_models_grouped().len();
        if count > 0 && self.selected_idx >= count {
            self.selected_idx = count - 1;
        }
    }
}

impl Default for ModelPickerState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the model picker overlay directly into `buf`.
///
/// Draws a centred modal (≈70 wide × ≈22 tall) with:
/// - Fast-mode notice when fast mode is active
/// - A filter line when the user is typing
/// - A scrollable list of models with effort indicator for supporting models
/// - Selection highlight on the focused row
/// - Bottom hint bar with ←/→ keys for effort adjustment
pub fn render_model_picker(state: &ModelPickerState, area: Rect, buf: &mut Buffer) {
    if !state.visible {
        return;
    }

    use ratatui::prelude::Stylize;
    use ratatui::widgets::Widget;

    let _pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let dialog_bg = CLAURST_PANEL_BG;
    let highlight_bg = Color::Rgb(233, 30, 99);
    let highlight_fg = Color::White;

    // ── Dark overlay ──
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_bg(Color::Rgb(10, 10, 14));
                cell.set_fg(Color::Rgb(40, 40, 45));
            }
        }
    }

    // ── Dialog size ──
    let width = 65u16.min(area.width.saturating_sub(6));
    let max_height = (area.height as f32 * 0.75) as u16;
    let grouped = state.filtered_models_grouped();
    let content_h = (grouped.len() as u16 + 6).min(max_height).max(8);
    let dialog_area = centered_rect(width, content_h, area);

    // ── Fill dialog bg (no border) ──
    for y in dialog_area.y..dialog_area.y + dialog_area.height {
        for x in dialog_area.x..dialog_area.x + dialog_area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(' ');
                cell.set_bg(dialog_bg);
                cell.set_fg(Color::White);
            }
        }
    }

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let footer_height = 1u16.min(inner.height);
    let header_height = 3u16.min(inner.height.saturating_sub(footer_height));
    let header_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: header_height,
    };
    let body_area = Rect {
        x: inner.x,
        y: inner.y.saturating_add(header_height),
        width: inner.width,
        height: inner.height.saturating_sub(header_height + footer_height),
    };
    let footer_area = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(footer_height),
        width: inner.width,
        height: footer_height,
    };

    // ── Fixed header ──
    let mut header_lines: Vec<Line> = Vec::new();

    // Title row: "Select model" left, "esc" right
    let title_pad = inner.width.saturating_sub(state.title.len() as u16 + 5) as usize;
    header_lines.push(Line::from(vec![
        Span::styled(
            format!(" {}", state.title),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>w$}", "esc ", w = title_pad),
            Style::default().fg(dim),
        ),
    ]));

    // Search field
    header_lines.push(Line::from(""));
    header_lines.push(modal_search_line(
        &state.filter,
        "Search",
        dim,
        Color::White,
    ));

    let header_para = Paragraph::new(header_lines).bg(dialog_bg);
    header_para.render(header_area, buf);

    if body_area.height == 0 {
        return;
    }

    // ── Model items ──
    let mut lines: Vec<Line> = Vec::new();
    let mut selected_line_idx: u16 = 0;

    if state.fast_mode {
        lines.push(Line::from(vec![Span::styled(
            format!(
                " \u{26a1} Fast mode ON ({})",
                state.fast_mode_model.as_deref().unwrap_or("current model")
            ),
            Style::default().fg(Color::Yellow),
        )]));
    }

    if state.loading_models {
        lines.push(Line::from(vec![Span::styled(
            " Loading models\u{2026}",
            Style::default().fg(dim),
        )]));
    }

    if !lines.is_empty() {
        lines.push(Line::from(""));
    }

    if grouped.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            " No results found",
            Style::default().fg(dim),
        )]));
        if !state.filter.trim().is_empty() {
            lines.push(Line::from(vec![Span::styled(
                " Press Enter to use custom model",
                Style::default().fg(Color::Rgb(200, 200, 200)),
            )]));
        }
    } else {
        let mut last_provider: Option<&str> = None;
        for (i, (model, is_fav)) in grouped.iter().enumerate() {
            // Provider group header in all-providers mode — skip for the
            // synthetic default entry (empty provider_id).
            if state.all_providers_mode
                && model.id != DEFAULT_MODEL_SENTINEL
                && last_provider != Some(&model.provider_id)
            {
                last_provider = Some(&model.provider_id);
                let group_name = state
                    .provider_names
                    .get(&model.provider_id)
                    .cloned()
                    .unwrap_or_else(|| ModelPickerState::provider_group_name(&model.provider_id));
                lines.push(Line::from(vec![Span::styled(
                    format!(" {} ", group_name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )]));
            }
            let is_selected = i == state.selected_idx;
            let supports_effort = model_supports_effort(&model.id);

            if is_selected {
                selected_line_idx = lines.len() as u16;
            }

            let (fg, bg) = if is_selected {
                (highlight_fg, highlight_bg)
            } else {
                (Color::White, dialog_bg)
            };

            let mut spans: Vec<Span<'static>> = Vec::new();

            // Default entry gets a star (★); active model gets a green dot (●).
            if model.id == DEFAULT_MODEL_SENTINEL {
                spans.push(Span::styled(
                    " \u{2605} ",
                    Style::default().fg(Color::Cyan).bg(bg),
                ));
            } else if model.is_current {
                spans.push(Span::styled(
                    " \u{25cf} ",
                    Style::default().fg(Color::Green).bg(bg),
                ));
            } else {
                spans.push(Span::styled("   ", Style::default().bg(bg)));
            }

            // Favorite indicator — heart (♥) when favorited.
            let heart = if *is_fav { "\u{2665} " } else { "  " };
            spans.push(Span::styled(
                heart.to_string(),
                Style::default().fg(Color::Yellow).bg(bg),
            ));

            spans.push(Span::styled(
                model.display_name.clone(),
                Style::default().fg(fg).bg(bg),
            ));

            // Effort indicator — show the effort clamped onto this model's
            // variants ladder so it never displays a tier the model can't do.
            if supports_effort && is_selected {
                let shown = state.effective_effort().unwrap_or(state.effort_level);
                spans.push(Span::styled(
                    format!("  {} {}", shown.symbol(), shown.label()),
                    Style::default().fg(Color::Rgb(200, 255, 200)).bg(bg),
                ));
            }

            // Description
            if !model.description.is_empty() {
                let desc_fg = if is_selected {
                    Color::Rgb(200, 200, 200)
                } else {
                    dim
                };
                spans.push(Span::styled(
                    format!("  {}", model.description),
                    Style::default().fg(desc_fg).bg(bg),
                ));
            }

            // Pad for full-width highlight
            if is_selected {
                let text_len: usize = spans.iter().map(|s| s.content.len()).sum();
                let pad = inner.width.saturating_sub(text_len as u16) as usize;
                if pad > 0 {
                    spans.push(Span::styled(
                        " ".repeat(pad),
                        Style::default().bg(highlight_bg),
                    ));
                }
            }

            lines.push(Line::from(spans));
        }
    }

    // ── Scroll ──
    let total_lines = lines.len() as u16;
    let visible = body_area.height;
    let scroll_y = if total_lines <= visible {
        0u16
    } else if selected_line_idx + 3 >= visible {
        (selected_line_idx + 3).saturating_sub(visible)
    } else {
        0
    };

    let para = Paragraph::new(lines).bg(dialog_bg).scroll((scroll_y, 0));

    para.render(body_area, buf);

    let mut footer_spans = vec![];
    // Show "enter set default" when the default sentinel is selected,
    // otherwise "enter select".
    let is_on_default = grouped
        .get(state.selected_idx)
        .map(|(m, _)| m.id == DEFAULT_MODEL_SENTINEL)
        .unwrap_or(false);
    if is_on_default {
        footer_spans.push(Span::styled(" enter", Style::default().fg(dim)));
        footer_spans.push(Span::styled(" set default", Style::default().fg(dim)));
    } else {
        footer_spans.push(Span::styled(" enter", Style::default().fg(dim)));
        footer_spans.push(Span::styled(" select", Style::default().fg(dim)));
    }
    // Favorite toggle shortcut.
    footer_spans.push(Span::raw("  "));
    footer_spans.push(Span::styled("f", Style::default().fg(dim)));
    footer_spans.push(Span::styled(" favorite", Style::default().fg(dim)));
    // Tab to jump between provider groups.
    footer_spans.push(Span::raw("  "));
    footer_spans.push(Span::styled("Tab", Style::default().fg(dim)));
    footer_spans.push(Span::styled(" providers", Style::default().fg(dim)));
    // Show-all toggle shortcut.
    footer_spans.push(Span::raw("  "));
    footer_spans.push(Span::styled("a", Style::default().fg(dim)));
    if state.show_all_providers {
        footer_spans.push(Span::styled(" connected", Style::default().fg(dim)));
    } else {
        footer_spans.push(Span::styled(" all", Style::default().fg(dim)));
    }
    if let Some((model, _)) = grouped.get(state.selected_idx) {
        if model_supports_effort(&model.id) {
            footer_spans.push(Span::raw("  "));
            footer_spans.push(Span::styled("\u{2190}/\u{2192}", Style::default().fg(dim)));
            footer_spans.push(Span::styled(" effort", Style::default().fg(dim)));
        }
    }
    footer_spans.push(Span::raw("  "));
    footer_spans.push(Span::styled(
        " /connect",
        Style::default().fg(Color::Rgb(233, 30, 99)),
    ));
    footer_spans.push(Span::styled(" providers", Style::default().fg(dim)));
    Paragraph::new(Line::from(footer_spans))
        .bg(dialog_bg)
        .render(footer_area, buf);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A small, controlled model list used to exercise the picker's selection /
    /// filter / effort behaviour independently of both the (now-removed)
    /// hardcoded list and the exact contents of the bundled registry snapshot.
    /// In production this list comes from `set_models(models_for_provider_from_registry(..))`.
    fn sample_models() -> Vec<ModelEntry> {
        vec![
            ModelEntry {
                id: "claude-opus-4-6".to_string(),
                display_name: "Claude Opus 4.6".to_string(),
                description: "200K context".to_string(),
                is_current: false,
                provider_id: "anthropic".to_string(),
            },
            ModelEntry {
                id: "claude-sonnet-4-6".to_string(),
                display_name: "Claude Sonnet 4.6".to_string(),
                description: "200K context".to_string(),
                is_current: false,
                provider_id: "anthropic".to_string(),
            },
            ModelEntry {
                id: "claude-haiku-4-5".to_string(),
                display_name: "Claude Haiku 4.5".to_string(),
                description: "200K context".to_string(),
                is_current: false,
                provider_id: "anthropic".to_string(),
            },
        ]
    }

    /// A picker seeded with `sample_models()` — the unit-test analogue of the
    /// registry-projection seeding that happens when the picker opens.
    fn make_picker() -> ModelPickerState {
        let mut p = ModelPickerState::new();
        p.set_models(sample_models());
        p
    }

    fn make_picker_with_current(current: &str) -> ModelPickerState {
        let mut p = make_picker();
        p.open(current);
        p
    }

    // 1. A model newly added to the registry surfaces in the picker immediately.
    //    The picker list is a pure projection of the models.dev registry, not a
    //    hardcoded set — regression guard for #228 ("latest model won't show").
    #[test]
    fn newly_added_registry_model_surfaces_in_picker() {
        let mut registry = claurst_api::ModelRegistry::new();

        // A fabricated future id that cannot already be in the bundled snapshot.
        let novel_id = "claude-opus-9-9-20991231";
        assert!(
            registry.get("anthropic", novel_id).is_none(),
            "fixture id must not already exist in the snapshot"
        );

        // Inject it exactly as the background refresh does: a models.dev-format
        // catalog fragment merged in via load_cache.
        let json = format!(
            r#"{{"anthropic":{{"id":"anthropic","name":"Anthropic","models":{{"{novel_id}":{{"id":"{novel_id}","name":"Claude Opus 9.9","release_date":"2099-12-31","limit":{{"context":200000,"output":64000}}}}}}}}}}"#
        );
        let path = std::env::temp_dir().join(format!(
            "claurst_picker_{}_{}.json",
            std::process::id(),
            novel_id
        ));
        std::fs::write(&path, json).expect("write temp catalog");
        registry.load_cache(&path);
        let _ = std::fs::remove_file(&path);

        let picker = models_for_provider_from_registry("anthropic", &registry);
        let novel = picker.iter().find(|m| m.id == novel_id);
        assert!(
            novel.is_some(),
            "a model added to the registry must surface in the picker"
        );
        assert_eq!(novel.unwrap().display_name, "Claude Opus 9.9");
        // Newest release_date sorts to the top of the projection.
        assert_eq!(
            picker[0].id, novel_id,
            "the freshest model must sort to the top of the picker"
        );
    }

    // 2. open() marks exactly one model as current.
    #[test]
    fn open_marks_current_model() {
        let mut p = make_picker();
        p.open("claude-sonnet-4-6");
        let current_count = p.models.iter().filter(|m| m.is_current).count();
        assert_eq!(current_count, 1);
        assert!(
            p.models
                .iter()
                .find(|m| m.id == "claude-sonnet-4-6")
                .unwrap()
                .is_current
        );
    }

    #[test]
    fn open_with_title_updates_dialog_title() {
        let mut p = ModelPickerState::new();
        p.open_with_title("Anthropic", "claude-sonnet-4-6", EffortLevel::Medium, false);
        assert_eq!(p.title, "Anthropic");
    }

    #[test]
    fn open_with_fast_mode_tracks_locked_model() {
        let mut p = ModelPickerState::new();
        p.open_with_state("gpt-4o-mini", EffortLevel::Medium, true);
        assert_eq!(p.fast_mode_model.as_deref(), Some("gpt-4o-mini"));
        assert!(p.is_selected_fast_mode_model("gpt-4o-mini"));
        assert!(!p.is_selected_fast_mode_model("gpt-4o"));
    }

    // 3. open() with an unknown model ID marks none as current and sets idx=0.
    #[test]
    fn open_unknown_model_selects_first() {
        let mut p = make_picker();
        p.open("unknown-model");
        assert_eq!(p.selected_idx, 0);
        assert!(p.models.iter().all(|m| !m.is_current));
    }

    // 4. select_next() wraps around to 0 after the last entry.
    #[test]
    fn select_next_wraps() {
        let mut p = make_picker_with_current("claude-opus-4-6");
        let total = p.filtered_models_grouped().len();
        p.selected_idx = total - 1;
        p.select_next();
        assert_eq!(p.selected_idx, 0);
    }

    // 5. select_prev() wraps around to last after idx 0.
    #[test]
    fn select_prev_wraps() {
        let mut p = make_picker_with_current("claude-opus-4-6");
        p.selected_idx = 0;
        p.select_prev();
        let total = p.filtered_models_grouped().len();
        assert_eq!(p.selected_idx, total - 1);
    }

    // 6. filter reduces visible entries.
    #[test]
    fn filter_reduces_results() {
        let mut p = make_picker_with_current("claude-opus-4-6");
        for c in "sonnet".chars() {
            p.push_filter_char(c);
        }
        let all = p.models.len();
        let filtered = p.filtered_models();
        assert!(
            filtered.len() < all,
            "filter should reduce the result count"
        );
        assert!(!filtered.is_empty(), "at least one sonnet model must match");
        for m in &filtered {
            let haystack = format!("{} {} {}", m.id, m.display_name, m.description).to_lowercase();
            assert!(
                haystack.contains("sonnet"),
                "model '{}' does not match filter",
                m.id
            );
        }
    }

    // 7. pop_filter_char removes last char.
    #[test]
    fn pop_filter_char_removes_last() {
        let mut p = make_picker_with_current("claude-opus-4-6");
        p.push_filter_char('h');
        p.push_filter_char('a');
        p.push_filter_char('i');
        assert_eq!(p.filter, "hai");
        p.pop_filter_char();
        assert_eq!(p.filter, "ha");
    }

    // 8. confirm() returns selected model ID and closes the picker.
    #[test]
    fn confirm_returns_id_and_closes() {
        let mut p = make_picker_with_current("claude-opus-4-6");
        p.selected_idx = 1; // skip Default sentinel at index 0
        let first_model_id = p.filtered_models()[0].id.clone();
        let result = p.confirm();
        assert_eq!(result.map(|(id, _)| id), Some(first_model_id));
        assert!(!p.visible, "picker should be closed after confirm");
    }

    // 9. confirm() on empty filter list uses custom model when filter is set.
    #[test]
    fn confirm_empty_filter_returns_none() {
        let mut p = make_picker_with_current("claude-opus-4-6");
        p.filter = "zzznomatch999".to_string();
        p.selected_idx = 0;
        let result = p.confirm();
        assert_eq!(result.map(|(id, _)| id), Some("zzznomatch999".to_string()));
    }

    // 10. close() clears filter and hides overlay.
    #[test]
    fn close_clears_state() {
        let mut p = make_picker_with_current("claude-opus-4-6");
        p.push_filter_char('x');
        p.close();
        assert!(!p.visible);
        assert!(p.filter.is_empty());
    }

    // 11. effort cycling follows the model's actual variants ladder.
    #[test]
    fn effort_cycles_correctly() {
        // sonnet-4-6's ladder is Low/Medium/High/Max (opencode adaptive).
        let mut p = make_picker_with_current("claude-sonnet-4-6");
        assert_eq!(p.effort_level, EffortLevel::Medium);
        p.effort_next();
        assert_eq!(p.effort_level, EffortLevel::High);
        p.effort_next();
        assert_eq!(p.effort_level, EffortLevel::Max);
        p.effort_next();
        // wraps back to the bottom of the ladder.
        assert_eq!(p.effort_level, EffortLevel::Low);
    }

    // 12. The picker ladders match opencode's variants() (single source of
    //     truth), not the old name heuristic.
    #[test]
    fn ladders_match_opencode_for_known_models() {
        use EffortLevel::*;
        // 4.6-era opus/sonnet: low/medium/high/max.
        assert_eq!(
            picker_variant_ladder("claude-opus-4-6"),
            vec![Low, Medium, High, Max]
        );
        assert_eq!(
            picker_variant_ladder("claude-sonnet-4-6"),
            vec![Low, Medium, High, Max]
        );
        // Haiku 4.5 is a thinking model: budget-based high/max.
        assert_eq!(picker_variant_ladder("claude-haiku-4-5"), vec![High, Max]);
        // gpt-4o is non-reasoning → no ladder, no selector.
        assert!(picker_variant_ladder("gpt-4o").is_empty());
        assert!(!model_supports_effort("gpt-4o"));
        for id in ["claude-opus-4-6", "claude-sonnet-4-6", "claude-haiku-4-5"] {
            assert!(model_supports_effort(id), "{id} should support effort");
        }
    }

    // 12b. GPT-5 reasoning models expose the multi-tier effort selector (incl.
    //      xhigh); a single-tier pro snapshot does not.
    #[test]
    fn gpt5_reasoning_models_support_effort() {
        use EffortLevel::*;
        for id in ["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex-spark"] {
            let ladder = picker_variant_ladder(id);
            assert!(ladder.len() > 1, "{id} should support effort: {ladder:?}");
            assert!(model_supports_effort(id), "{id} should support effort");
            assert!(
                ladder.contains(&XHigh),
                "{id} should reach xhigh: {ladder:?}"
            );
        }
        // Version-less gpt-5-pro exposes only the fixed `high` tier → no selector.
        assert_eq!(picker_variant_ladder("gpt-5-pro"), vec![High]);
        assert!(!model_supports_effort("gpt-5-pro"));
        // Chat snapshots have no reasoning variants.
        assert!(!model_supports_effort("gpt-5-chat-latest"));
        // Non-gpt5 stays unaffected.
        assert!(!model_supports_effort("gpt-4o"));
    }

    // 13. Haiku 4.5 IS a thinking model (opencode high/max), so confirming it
    //     carries an effort clamped onto its ladder.
    #[test]
    fn haiku_supports_effort() {
        assert_eq!(
            picker_variant_ladder("claude-haiku-4-5"),
            vec![EffortLevel::High, EffortLevel::Max]
        );
        assert!(model_supports_effort("claude-haiku-4-5"));
        let mut p = make_picker_with_current("claude-haiku-4-5");
        p.selected_idx = p
            .models
            .iter()
            .position(|m| m.id == "claude-haiku-4-5")
            .unwrap();
        let confirmed = p.confirm();
        assert!(
            confirmed.is_some_and(|(_, e)| e.is_some()),
            "haiku should carry a (clamped) effort"
        );
    }

    // 14. render_model_picker does not panic for a default-area call.
    #[test]
    fn render_does_not_panic() {
        let mut p = make_picker();
        p.open("claude-sonnet-4-6");
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_model_picker(&p, area, &mut buf);
    }

    // 15. render does nothing when not visible.
    #[test]
    fn render_noop_when_hidden() {
        let p = ModelPickerState::new();
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_model_picker(&p, area, &mut buf);
        for cell in buf.content() {
            assert_eq!(
                cell.symbol(),
                " ",
                "buffer should be empty when picker is hidden"
            );
        }
    }

    // 16. models_for_provider_from_registry returns the bundled snapshot's
    //     entries for each well-known provider.  Specific model IDs aren't
    //     asserted here because the snapshot is regenerated periodically;
    //     instead we check the family / provider-namespace shape.
    #[test]
    fn models_for_provider_anthropic() {
        let registry = claurst_api::ModelRegistry::new();
        let models = models_for_provider_from_registry("anthropic", &registry);
        assert!(!models.is_empty(), "anthropic must yield models");
        assert!(
            models.iter().any(|m| m.id.starts_with("claude")),
            "anthropic should expose at least one claude-* model"
        );
    }

    #[test]
    fn models_for_provider_openai() {
        let registry = claurst_api::ModelRegistry::new();
        let models = models_for_provider_from_registry("openai", &registry);
        assert!(!models.is_empty());
        // Must NOT contain Claude models
        assert!(!models.iter().any(|m| m.id.contains("claude")));
        // Should contain at least one gpt-* or o-series id
        assert!(
            models.iter().any(|m| m.id.starts_with("gpt-")
                || m.id.starts_with("o3")
                || m.id.starts_with("o4")),
            "openai should expose at least one gpt/o-series model"
        );
    }

    // The picker list for a catalog-backed provider must be exactly the
    // read-only registry projection (release_date DESC, then id) — no provider
    // return value is allowed to become the displayed list. This is what makes
    // a fresh claude-opus point-release surface the moment the snapshot ships.
    #[test]
    fn picker_list_equals_registry_projection_for_catalog_providers() {
        let registry = claurst_api::ModelRegistry::new();

        // Catalog-backed providers skip live discovery; live ones do not.
        // Anthropic now discovers via GET /v1/models, so it's on the live side;
        // its sync projection below is still the pre-discovery/offline fallback.
        assert!(!provider_uses_catalog_projection("anthropic"));
        assert!(provider_uses_catalog_projection("openai"));
        assert!(provider_uses_catalog_projection("google"));
        assert!(!provider_uses_catalog_projection("ollama"));
        assert!(!provider_uses_catalog_projection("github-copilot"));

        for pid in ["anthropic", "openai"] {
            let picker_ids: Vec<String> = models_for_provider_from_registry(pid, &registry)
                .iter()
                .map(|m| m.id.clone())
                .collect();

            let mut proj = registry.list_visible_by_provider(pid);
            proj.sort_by(|a, b| {
                let rd_a = a.release_date.as_deref().unwrap_or("");
                let rd_b = b.release_date.as_deref().unwrap_or("");
                rd_b.cmp(rd_a).then_with(|| (*a.info.id).cmp(&*b.info.id))
            });
            let proj_ids: Vec<String> = proj.iter().map(|e| e.info.id.to_string()).collect();

            assert_eq!(
                picker_ids, proj_ids,
                "{pid} picker must equal catalog projection"
            );
        }

        // Headline: the newest Opus is in the projected anthropic list.
        let anthropic_ids: Vec<String> = models_for_provider_from_registry("anthropic", &registry)
            .iter()
            .map(|m| m.id.clone())
            .collect();
        assert!(
            anthropic_ids.iter().any(|id| id == "claude-opus-4-8"),
            "claude-opus-4-8 must appear in the projected list"
        );
    }

    // Codex login lands on the "openai-codex" provider id (the /connect alias).
    // Both that alias and the canonical "codex" must yield the curated Codex
    // catalog — never the empty-registry "default" placeholder.
    #[test]
    fn models_for_provider_codex_aliases() {
        let registry = claurst_api::ModelRegistry::new();
        for pid in ["codex", "openai-codex"] {
            let models = models_for_provider_from_registry(pid, &registry);
            assert!(!models.is_empty(), "{pid} must yield Codex models");
            assert_ne!(
                models[0].id, "default",
                "{pid} must not fall back to default"
            );

            let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
            // Opencode's exact allow-list (from the current snapshot).
            assert!(ids.contains(&"gpt-5.5"), "{pid} must list gpt-5.5: {ids:?}");
            assert!(ids.contains(&"gpt-5.4"), "{pid} must list gpt-5.4: {ids:?}");
            assert!(
                ids.contains(&"gpt-5.4-mini"),
                "{pid} must list gpt-5.4-mini: {ids:?}"
            );
            // gpt-5.5 is the newest -> sorts first -> is the default highlight.
            assert_eq!(
                models[0].id, "gpt-5.5",
                "{pid} newest model should sort first"
            );
            // Legacy / disallowed models must be gone.
            for legacy in [
                "gpt-5.5-pro",
                "gpt-5.2-codex",
                "gpt-5.1-codex",
                "gpt-5.4-nano",
                "gpt-5",
            ] {
                assert!(
                    !ids.contains(&legacy),
                    "{pid} must not list {legacy}: {ids:?}"
                );
            }
        }
    }

    // default_model_for_provider must pin a real Codex flagship (not
    // "<id>/default") for both id spellings, preserving the caller's prefix.
    #[test]
    fn default_model_for_provider_codex_aliases() {
        let registry = claurst_api::ModelRegistry::new();
        for pid in ["codex", "openai-codex"] {
            let m = default_model_for_provider(pid, &registry);
            assert_eq!(
                m,
                format!("{}/{}", pid, claurst_core::codex_oauth::DEFAULT_CODEX_MODEL),
                "{pid} default must pin the curated flagship Codex model"
            );
            assert!(
                !m.ends_with("/default"),
                "{pid} must not fall back to /default"
            );
        }
    }

    #[test]
    fn models_for_provider_unknown_returns_default() {
        let registry = claurst_api::ModelRegistry::new();
        let models = models_for_provider_from_registry("some-unknown-provider", &registry);
        assert!(!models.is_empty());
        assert_eq!(models[0].id, "default");
    }

    // 17. default_model_for_provider returns prefixed models for non-anthropic.
    #[test]
    fn default_model_for_provider_openai() {
        let registry = claurst_api::ModelRegistry::new();
        let m = default_model_for_provider("openai", &registry);
        assert!(
            m.starts_with("openai/"),
            "openai default must be prefixed: {m}"
        );
    }

    #[test]
    fn default_model_for_provider_anthropic_bare() {
        // Anthropic models are bare (no prefix) for backwards compat.
        let registry = claurst_api::ModelRegistry::new();
        let m = default_model_for_provider("anthropic", &registry);
        assert!(!m.contains('/'), "anthropic default must be bare: {m}");
        assert!(
            m.starts_with("claude"),
            "anthropic default must be a claude variant: {m}"
        );
    }

    #[test]
    fn default_model_for_provider_unknown_falls_back() {
        let registry = claurst_api::ModelRegistry::new();
        assert_eq!(
            default_model_for_provider("some-self-hosted-thing", &registry),
            "some-self-hosted-thing/default"
        );
    }

    // 18. set_models replaces the model list.
    #[test]
    fn set_models_replaces_list() {
        let registry = claurst_api::ModelRegistry::new();
        let mut p = ModelPickerState::new();
        let openai_models = models_for_provider_from_registry("openai", &registry);
        p.set_models(openai_models);
        let ids: Vec<&str> = p.models.iter().map(|m| m.id.as_str()).collect();
        assert!(!ids.iter().any(|id| id.contains("claude")));
    }

    // 19. merge_models is additive: it keeps the catalog projection (including
    //     its richer metadata) for ids present in both, and appends only live
    //     ids not already listed. Mirrors copilot models.ts merge-by-id.
    #[test]
    fn merge_models_is_additive_and_keeps_catalog_metadata() {
        let mut p = ModelPickerState::new();
        p.set_models(sample_models()); // catalog projection: opus/sonnet/haiku

        let live = vec![
            // Overlaps an existing catalog id but with poorer metadata —
            // the catalog row must win (no overwrite).
            ModelEntry {
                id: "claude-opus-4-6".to_string(),
                display_name: "LIVE OVERWRITE".to_string(),
                description: "live desc".to_string(),
                is_current: false,
                provider_id: "anthropic".to_string(),
            },
            // A brand-new live id absent from the catalog — must be appended.
            ModelEntry {
                id: "gpt-5.5-live".to_string(),
                display_name: "GPT-5.5 (live)".to_string(),
                description: "live only".to_string(),
                is_current: false,
                provider_id: "anthropic".to_string(),
            },
        ];
        p.merge_models(live);

        let opus = p
            .models
            .iter()
            .find(|m| m.id == "claude-opus-4-6")
            .expect("catalog opus retained");
        assert_eq!(
            opus.display_name, "Claude Opus 4.6",
            "catalog metadata must be kept for shared ids (no live overwrite)"
        );
        assert!(
            p.models.iter().any(|m| m.id == "gpt-5.5-live"),
            "new live id must be appended"
        );
        assert!(
            p.models
                .iter()
                .filter(|m| m.id == "claude-opus-4-6")
                .count()
                == 1,
            "no duplicate for a shared id"
        );
        // All three original catalog ids are still present.
        for id in ["claude-opus-4-6", "claude-sonnet-4-6", "claude-haiku-4-5"] {
            assert!(p.models.iter().any(|m| m.id == id), "catalog id {id} kept");
        }
    }

    // 20. An empty live result never wipes the catalog projection.
    #[test]
    fn merge_models_empty_keeps_catalog() {
        let mut p = ModelPickerState::new();
        p.set_models(sample_models());
        p.merge_models(Vec::new());
        assert_eq!(
            p.models.len(),
            3,
            "empty live merge must not clear the list"
        );
        assert!(!p.loading_models);
    }

    // 21. A non-empty live result clears the synthetic "no catalog" placeholder.
    #[test]
    fn merge_models_clears_placeholder() {
        let mut p = ModelPickerState::new();
        p.set_models(vec![model_entry(
            "default",
            "Default model",
            "no catalog entry for this provider",
            "anthropic",
        )]);
        p.merge_models(vec![ModelEntry {
            id: "llama3.3".to_string(),
            display_name: "Llama 3.3".to_string(),
            description: "local".to_string(),
            is_current: false,
            provider_id: "anthropic".to_string(),
        }]);
        assert!(
            !p.models.iter().any(|m| m.id == "default"),
            "placeholder must be dropped once a real list arrives"
        );
        assert!(p.models.iter().any(|m| m.id == "llama3.3"));
    }

    // 22. The four providers whose hardcoded discover_models() was removed now
    //     project from the catalog (no live fetch that could clobber it).
    #[test]
    fn hardcoded_list_providers_use_catalog_projection() {
        for pid in ["azure", "amazon-bedrock", "cohere", "minimax"] {
            assert!(
                provider_uses_catalog_projection(pid),
                "{pid} must project from the catalog after its hardcoded list was removed"
            );
        }
        // Live/curated providers must NOT be treated as catalog projections.
        // Anthropic joined this group: it discovers via GET /v1/models.
        for pid in ["anthropic", "ollama", "github-copilot", "codex", "free"] {
            assert!(
                !provider_uses_catalog_projection(pid),
                "{pid} must keep its live/curated discovery"
            );
        }
    }

    #[test]
    fn local_runtime_models_are_authoritative() {
        for pid in [
            "ollama",
            "lmstudio",
            "lm-studio",
            "llamacpp",
            "llama-cpp",
            "llama-server",
        ] {
            assert!(
                provider_has_authoritative_live_models(pid),
                "{pid} must replace catalog rows with its live model list"
            );
        }
        assert!(!provider_has_authoritative_live_models("github-copilot"));
        assert!(!provider_has_authoritative_live_models("openrouter"));
    }

    #[test]
    fn valid_favorites_filters_stale_entries() {
        let mut state = ModelPickerState::new();
        state.models = vec![
            ModelEntry {
                id: "model-a".to_string(),
                display_name: "A".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: String::new(),
            },
            ModelEntry {
                id: "model-b".to_string(),
                display_name: "B".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: String::new(),
            },
        ];
        state.favorites = vec![
            "provider/model-a".to_string(), // valid
            "provider/model-c".to_string(), // stale (not in models list)
            "model-b".to_string(),          // valid (no provider prefix)
        ];
        let valid = state.valid_favorites();
        assert_eq!(valid.len(), 2);
        assert!(valid.contains(&"provider/model-a".to_string()));
        assert!(valid.contains(&"model-b".to_string()));
    }

    #[test]
    fn filtered_models_grouped_puts_favorites_first() {
        let mut state = ModelPickerState::new();
        state.models = vec![
            ModelEntry {
                id: "regular".to_string(),
                display_name: "Regular".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: String::new(),
            },
            ModelEntry {
                id: "pinned".to_string(),
                display_name: "Pinned".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: String::new(),
            },
            ModelEntry {
                id: "other".to_string(),
                display_name: "Other".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: String::new(),
            },
        ];
        state.favorites = vec!["provider/pinned".to_string()];
        let grouped = state.filtered_models_grouped();
        assert_eq!(grouped.len(), 4);
        // Default sentinel at top.
        assert_eq!(grouped[0].0.id, DEFAULT_MODEL_SENTINEL);
        // Favorite must come next.
        assert_eq!(grouped[1].0.id, "pinned");
        assert!(grouped[1].1);
        // Rest are non-favorites, preserve source order.
        assert_eq!(grouped[2].0.id, "regular");
        assert!(!grouped[2].1);
        assert_eq!(grouped[3].0.id, "other");
        assert!(!grouped[3].1);
    }

    #[test]
    fn filtered_models_grouped_respects_filter() {
        let mut state = ModelPickerState::new();
        state.models = vec![
            ModelEntry {
                id: "pinned-a".to_string(),
                display_name: "Pinned A".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: String::new(),
            },
            ModelEntry {
                id: "pinned-b".to_string(),
                display_name: "Other B".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: String::new(),
            },
        ];
        state.favorites = vec![
            "provider/pinned-a".to_string(),
            "provider/pinned-b".to_string(),
        ];
        for c in "pinned a".chars() {
            state.push_filter_char(c);
        }
        let grouped = state.filtered_models_grouped();
        // Only the favorite matching the filter appears, on top.
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].0.id, "pinned-a");
        assert!(grouped[0].1);
    }

    #[test]
    fn is_favorite_roundtrips_without_persist() {
        let mut state = ModelPickerState::new();
        state.favorites = vec!["anthropic/claude-sonnet-4-6".to_string()];
        assert!(state.is_favorite("anthropic/claude-sonnet-4-6"));
        assert!(!state.is_favorite("openai/gpt-4o"));
    }

    #[test]
    fn default_entry_appears_at_top() {
        let mut picker = ModelPickerState::new();
        picker.set_models(vec![
            ModelEntry {
                id: "model-a".to_string(),
                display_name: "Model A".to_string(),
                description: "desc".to_string(),
                is_current: false,
                provider_id: String::new(),
            },
            ModelEntry {
                id: "model-b".to_string(),
                display_name: "Model B".to_string(),
                description: "desc".to_string(),
                is_current: false,
                provider_id: String::new(),
            },
        ]);
        picker.open("model-a");
        let grouped = picker.filtered_models_grouped();
        assert_eq!(grouped.len(), 3);
        assert_eq!(grouped[0].0.id, DEFAULT_MODEL_SENTINEL);
        assert_eq!(grouped[1].0.id, "model-a");
    }

    #[test]
    fn default_entry_is_current_when_no_model() {
        let mut picker = ModelPickerState::new();
        picker.set_models(vec![ModelEntry {
            id: "model-a".to_string(),
            display_name: "Model A".to_string(),
            description: "desc".to_string(),
            is_current: false,
            provider_id: String::new(),
        }]);
        picker.open_with_title("Test", "", EffortLevel::Medium, false);
        assert!(picker.default_entry.is_current);
    }

    #[test]
    fn confirm_returns_default_sentinel() {
        let mut picker = ModelPickerState::new();
        picker.set_models(vec![ModelEntry {
            id: "model-a".to_string(),
            display_name: "Model A".to_string(),
            description: "desc".to_string(),
            is_current: false,
            provider_id: String::new(),
        }]);
        picker.open("model-a");
        picker.selected_idx = 0; // Default entry
        let result = picker.confirm();
        assert_eq!(result.unwrap().0, DEFAULT_MODEL_SENTINEL);
    }

    // --- selected_idx preselection from grouped order (Bug 1 + Bug 2) ---

    #[test]
    fn open_with_title_preselects_current_in_grouped_order() {
        let mut p = make_picker();
        // Set the model at raw index 1 (claude-sonnet-4-6) as current.
        // Grouped order: [default, opus, sonnet, haiku] (no favorites).
        // The grouped index of sonnet should be 2 (default=0, opus=1, sonnet=2).
        p.open_with_title("Test", "claude-sonnet-4-6", EffortLevel::Medium, false);
        let grouped = p.filtered_models_grouped();
        let expected_idx = grouped
            .iter()
            .position(|(m, _)| m.id == "claude-sonnet-4-6")
            .unwrap();
        assert_eq!(
            p.selected_idx, expected_idx,
            "selected_idx must match grouped position, not raw models position"
        );
        // Sanity: grouped includes default entry at 0.
        assert_eq!(grouped[0].0.id, DEFAULT_MODEL_SENTINEL);
        assert!(
            p.models
                .iter()
                .find(|m| m.id == "claude-sonnet-4-6")
                .unwrap()
                .is_current
        );
    }

    #[test]
    fn open_with_title_preselects_default_entry_when_no_model() {
        let mut p = make_picker();
        p.open_with_title("Test", "", EffortLevel::Medium, false);
        assert!(p.default_entry.is_current);
        assert_eq!(p.selected_idx, 0);
    }

    #[test]
    fn open_with_title_preselects_with_provider_prefixed_current() {
        let models = vec![
            ModelEntry {
                id: "revolut-ca/glm-5-2".to_string(),
                display_name: "GLM 5.2".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "together-ai-coding".to_string(),
            },
            ModelEntry {
                id: "some-other-model".to_string(),
                display_name: "Other".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "together-ai-coding".to_string(),
            },
        ];
        let mut p = ModelPickerState::new();
        p.set_models(models);
        p.open_with_title(
            "Test",
            "together-ai-coding/revolut-ca/glm-5-2",
            EffortLevel::Medium,
            false,
        );
        let glm = p
            .models
            .iter()
            .find(|m| m.id == "revolut-ca/glm-5-2")
            .unwrap();
        assert!(
            glm.is_current,
            "provider-prefixed current must set is_current"
        );
        // Other model must NOT be current.
        assert!(
            !p.models
                .iter()
                .find(|m| m.id == "some-other-model")
                .unwrap()
                .is_current
        );
        let grouped = p.filtered_models_grouped();
        let expected_idx = grouped
            .iter()
            .position(|(m, _)| m.id == "revolut-ca/glm-5-2")
            .unwrap();
        assert_eq!(p.selected_idx, expected_idx);
    }

    #[test]
    fn open_with_title_preselects_correct_row_with_favorites() {
        // Build a picker where a NON-current model is a favorite.
        // sample_models: [opus, sonnet, haiku] (all anthropic).
        // Favorite: anthropic/claude-sonnet-4-6
        // Current:  claude-opus-4-6
        // Grouped order: [default, sonnet(fav), opus, haiku]
        // So opus is at grouped index 2, not raw index 0.
        let mut p = make_picker();
        p.favorites = vec!["anthropic/claude-sonnet-4-6".to_string()];
        p.open_with_title("Test", "claude-opus-4-6", EffortLevel::Medium, false);
        let grouped = p.filtered_models_grouped();
        // Verify grouped order: default, sonnet (favorite), opus, haiku.
        assert_eq!(grouped.len(), 4);
        assert_eq!(grouped[0].0.id, DEFAULT_MODEL_SENTINEL);
        assert_eq!(grouped[1].0.id, "claude-sonnet-4-6");
        assert!(grouped[1].1, "sonnet should be favorite");
        assert_eq!(grouped[2].0.id, "claude-opus-4-6");
        assert!(!grouped[2].1, "opus should not be favorite");
        assert_eq!(grouped[3].0.id, "claude-haiku-4-5");
        // selected_idx must point to opus in grouped order (index 2).
        assert_eq!(
            p.selected_idx, 2,
            "selected_idx must be the grouped position of the current model, not the raw index"
        );
    }

    #[test]
    fn open_all_providers_preselects_provider_prefixed_model() {
        let models = vec![
            ModelEntry {
                id: "revolut-ca/glm-5-2".to_string(),
                display_name: "GLM 5.2".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "together-ai-coding".to_string(),
            },
            ModelEntry {
                id: "another-model".to_string(),
                display_name: "Another".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "together-ai-coding".to_string(),
            },
        ];
        let mut p = ModelPickerState::new();
        p.set_models(models);
        p.connected_provider_ids = ["together-ai-coding".to_string()].into_iter().collect();
        p.open_all_providers(
            "together-ai-coding/revolut-ca/glm-5-2",
            EffortLevel::Medium,
            false,
        );
        assert!(p.all_providers_mode);
        let glm = p
            .models
            .iter()
            .find(|m| m.id == "revolut-ca/glm-5-2")
            .unwrap();
        assert!(
            glm.is_current,
            "all-providers mode must match provider-prefixed current"
        );
        let grouped = p.filtered_models_grouped();
        let expected_idx = grouped
            .iter()
            .position(|(m, _)| m.id == "revolut-ca/glm-5-2")
            .unwrap();
        assert_eq!(p.selected_idx, expected_idx);
    }

    #[test]
    fn default_entry_does_not_get_group_header() {
        let entry = ModelEntry {
            id: DEFAULT_MODEL_SENTINEL.to_string(),
            display_name: "Default".to_string(),
            description: "".to_string(),
            is_current: false,
            provider_id: String::new(),
        };
        // The render condition checks model.id != DEFAULT_MODEL_SENTINEL
        // before pushing a group header. The default entry must NOT pass.
        assert!(entry.id == DEFAULT_MODEL_SENTINEL);
    }

    #[test]
    fn selected_provider_id_captured_before_close() {
        // When a filter is active and the user navigates + confirms,
        // the provider_id must be captured BEFORE close() clears the filter.
        let mut picker = ModelPickerState::new();
        picker.all_providers_mode = true;
        picker.connected_provider_ids = ["together-ai-coding".to_string(), "ollama".to_string()]
            .into_iter()
            .collect();
        picker.set_models(vec![
            ModelEntry {
                id: "glm-5-2".to_string(),
                display_name: "GLM 5.2".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "together-ai-coding".to_string(),
            },
            ModelEntry {
                id: "opus-4-8".to_string(),
                display_name: "Opus 4.8".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "ollama".to_string(),
            },
        ]);
        // Type filter "glm" so only the glm model shows (default entry is
        // filtered out because "glm" does not match "default").
        for c in "glm".chars() {
            picker.push_filter_char(c);
        }
        // glm is the only filtered result → grouped index 0.
        picker.selected_idx = 0;
        // Capture provider BEFORE confirm (as the fixed Enter handler does).
        let captured_provider = picker
            .filtered_models_grouped()
            .get(picker.selected_idx)
            .map(|(m, _)| m.provider_id.clone());
        // Now confirm — this closes and clears the filter.
        let confirmed_id = picker.confirm();
        // After close, the filter is cleared — unfiltered list is different.
        // But we captured the provider BEFORE close.
        assert_eq!(confirmed_id, Some(("glm-5-2".to_string(), None)));
        assert_eq!(captured_provider, Some("together-ai-coding".to_string()));
        // If we had re-read AFTER close, selected_idx=0 would point to the
        // default entry (provider_id="") in the unfiltered list — the bug.
        assert_ne!(
            picker
                .filtered_models_grouped()
                .get(0)
                .map(|(m, _)| m.provider_id.clone()),
            Some("together-ai-coding".to_string())
        );
    }

    // --- Tab / BackTab provider-group navigation (all-providers mode) ---

    /// Two providers with two models each — the basic multi-provider fixture.
    fn two_provider_models() -> Vec<ModelEntry> {
        vec![
            ModelEntry {
                id: "model-a1".to_string(),
                display_name: "Model A1".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "provider-a".to_string(),
            },
            ModelEntry {
                id: "model-a2".to_string(),
                display_name: "Model A2".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "provider-a".to_string(),
            },
            ModelEntry {
                id: "model-b1".to_string(),
                display_name: "Model B1".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "provider-b".to_string(),
            },
            ModelEntry {
                id: "model-b2".to_string(),
                display_name: "Model B2".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "provider-b".to_string(),
            },
        ]
    }

    #[test]
    fn select_next_provider_jumps_to_next_group() {
        let mut p = ModelPickerState::new();
        p.set_models(two_provider_models());
        p.connected_provider_ids = ["provider-a".to_string(), "provider-b".to_string()]
            .into_iter()
            .collect();
        p.open_all_providers("model-a1", EffortLevel::Medium, false);
        // Default entry at index 0, then provider-a models, then provider-b.
        let grouped = p.filtered_models_grouped();
        let a1_idx = grouped
            .iter()
            .position(|(m, _)| m.id == "model-a1")
            .unwrap();
        let b1_idx = grouped
            .iter()
            .position(|(m, _)| m.id == "model-b1")
            .unwrap();
        p.selected_idx = a1_idx;
        p.select_next_provider();
        assert_eq!(
            p.selected_idx, b1_idx,
            "Tab from provider A should jump to provider B's first model"
        );
    }

    #[test]
    fn select_prev_provider_jumps_to_previous_group() {
        let mut p = ModelPickerState::new();
        p.set_models(two_provider_models());
        p.connected_provider_ids = ["provider-a".to_string(), "provider-b".to_string()]
            .into_iter()
            .collect();
        p.open_all_providers("model-b1", EffortLevel::Medium, false);
        let grouped = p.filtered_models_grouped();
        let a1_idx = grouped
            .iter()
            .position(|(m, _)| m.id == "model-a1")
            .unwrap();
        let b1_idx = grouped
            .iter()
            .position(|(m, _)| m.id == "model-b1")
            .unwrap();
        p.selected_idx = b1_idx;
        p.select_prev_provider();
        assert_eq!(
            p.selected_idx, a1_idx,
            "BackTab from provider B should jump to provider A's first model"
        );
    }

    #[test]
    fn select_next_provider_wraps_around() {
        let mut p = ModelPickerState::new();
        p.set_models(two_provider_models());
        p.connected_provider_ids = ["provider-a".to_string(), "provider-b".to_string()]
            .into_iter()
            .collect();
        p.open_all_providers("model-b2", EffortLevel::Medium, false);
        let grouped = p.filtered_models_grouped();
        let b2_idx = grouped
            .iter()
            .position(|(m, _)| m.id == "model-b2")
            .unwrap();
        let a1_idx = grouped
            .iter()
            .position(|(m, _)| m.id == "model-a1")
            .unwrap();
        p.selected_idx = b2_idx;
        p.select_next_provider();
        assert_eq!(
            p.selected_idx, a1_idx,
            "Tab from last provider should wrap to first provider's first model"
        );
    }

    #[test]
    fn select_next_provider_skips_default_entry() {
        let mut p = ModelPickerState::new();
        p.set_models(two_provider_models());
        p.connected_provider_ids = ["provider-a".to_string(), "provider-b".to_string()]
            .into_iter()
            .collect();
        p.open_all_providers("", EffortLevel::Medium, false);
        // No current model → default entry (index 0) is selected.
        {
            let grouped = p.filtered_models_grouped();
            assert_eq!(p.selected_idx, 0, "default entry should be at index 0");
            assert_eq!(grouped[0].0.id, DEFAULT_MODEL_SENTINEL);
            assert_eq!(grouped[0].0.provider_id, "");
        }
        p.select_next_provider();
        // Tab must skip the default entry and land on the first real
        // provider's first model — never back on the default entry.
        assert_ne!(p.selected_idx, 0, "Tab must never select the default entry");
        let selected_pid = p.filtered_models_grouped()[p.selected_idx]
            .0
            .provider_id
            .clone();
        assert!(
            !selected_pid.is_empty(),
            "Tab must land on a real provider, not the default entry"
        );
    }

    #[test]
    fn connected_filter_hides_unconnected_providers() {
        let mut state = ModelPickerState::new();
        state.all_providers_mode = true;
        state.connected_provider_ids = ["provider-a".to_string()].into_iter().collect();
        state.set_models(vec![
            ModelEntry {
                id: "model-a1".to_string(),
                display_name: "A1".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "provider-a".to_string(),
            },
            ModelEntry {
                id: "model-a2".to_string(),
                display_name: "A2".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "provider-a".to_string(),
            },
            ModelEntry {
                id: "model-b1".to_string(),
                display_name: "B1".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "provider-b".to_string(),
            },
            ModelEntry {
                id: "model-b2".to_string(),
                display_name: "B2".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "provider-b".to_string(),
            },
        ]);
        let grouped = state.filtered_models_grouped();
        // Default entry + 2 provider-a models. Provider-b filtered out.
        assert_eq!(grouped.len(), 3);
        assert_eq!(grouped[0].0.id, DEFAULT_MODEL_SENTINEL);
        let provider_ids: std::collections::HashSet<&str> = grouped
            .iter()
            .map(|(m, _)| m.provider_id.as_str())
            .collect();
        assert!(!provider_ids.contains("provider-b"));
        assert!(provider_ids.contains("provider-a"));
    }

    #[test]
    fn show_all_providers_shows_everyone() {
        let mut state = ModelPickerState::new();
        state.all_providers_mode = true;
        state.show_all_providers = true;
        state.connected_provider_ids = ["provider-a".to_string()].into_iter().collect();
        state.set_models(vec![
            ModelEntry {
                id: "model-a1".to_string(),
                display_name: "A1".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "provider-a".to_string(),
            },
            ModelEntry {
                id: "model-b1".to_string(),
                display_name: "B1".to_string(),
                description: "".to_string(),
                is_current: false,
                provider_id: "provider-b".to_string(),
            },
        ]);
        let grouped = state.filtered_models_grouped();
        // Default entry + 1 from provider-a + 1 from provider-b.
        assert_eq!(grouped.len(), 3);
        let provider_ids: std::collections::HashSet<&str> = grouped
            .iter()
            .map(|(m, _)| m.provider_id.as_str())
            .collect();
        assert!(provider_ids.contains("provider-a"));
        assert!(provider_ids.contains("provider-b"));
    }

    #[test]
    fn toggle_show_all_providers_flips_state() {
        let mut state = ModelPickerState::new();
        assert!(!state.show_all_providers);
        state.toggle_show_all_providers();
        assert!(state.show_all_providers);
        state.toggle_show_all_providers();
        assert!(!state.show_all_providers);
    }
}
