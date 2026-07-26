// model_registry.rs — Lossless model registry sourced from models.dev.
//
// **Architecture** (mirrors opencode):
//   1. A bundled snapshot of `https://models.dev/api.json` is embedded at
//      compile time from `crates/api/assets/models-snapshot.json`.  This is
//      the authoritative catalog for ~118 providers / ~4500 models.
//   2. On startup the registry is hydrated from the embedded snapshot.
//   3. Optionally, `load_cache()` overlays a fresher copy from disk
//      (refreshed by `refresh_from_models_dev()` once it has run).
//   4. `refresh_from_models_dev()` fetches the latest catalog from
//      `https://models.dev/api.json` (overridable via `MODELS_DEV_URL` /
//      `CLAURST_MODELS_URL`) and writes it back to the on-disk cache.
//
// **No more hardcoded per-provider model lists.**  All metadata —
// modalities, pricing, release date, capability flags, npm SDK package —
// lives in the bundled JSON and is updated by re-running
// `script/sync-models.{ps1,sh}`.
//
// All network and parse failures are non-fatal: the bundled snapshot is
// always available as a fallback.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use claurst_core::provider_id::{ModelId, ProviderId};

use crate::provider::ModelInfo;

// ---------------------------------------------------------------------------
// Embedded snapshot
// ---------------------------------------------------------------------------

/// The bundled models.dev snapshot, baked into the binary at compile time.
///
/// Refresh it locally with `bun run script/sync-models.ts` (TS) or
/// `pwsh script/sync-models.ps1` (PS).  CI also refreshes weekly.
const BUNDLED_SNAPSHOT: &[u8] = include_bytes!("../assets/models-snapshot.json");

// ---------------------------------------------------------------------------
// Capability enums
// ---------------------------------------------------------------------------

/// Input or output media type for a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    Text,
    Audio,
    Image,
    Video,
    Pdf,
}

/// Model lifecycle status as reported by models.dev.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ModelStatus {
    #[default]
    Active,
    Beta,
    Alpha,
    Deprecated,
}


impl ModelStatus {
    /// Whether to surface this model in default UI listings.
    ///
    /// Alpha/deprecated models are hidden unless
    /// `CLAURST_ENABLE_EXPERIMENTAL_MODELS=1`.
    pub fn is_listed_by_default(self) -> bool {
        matches!(self, ModelStatus::Active | ModelStatus::Beta)
    }
}

/// How a model emits reasoning content during streaming.
///
/// `Plain` means reasoning is delivered alongside normal content.  The
/// `Field` variant indicates reasoning arrives in a specific JSON field
/// (`reasoning_content` or `reasoning_details`) and must be hoisted into a
/// thinking block by the streaming adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InterleavedReasoning {
    Plain(bool),
    Field { field: String },
}

/// Per-model override of the provider-level NPM SDK package or API URL.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

/// One alternative dispatch mode for a model (e.g. "fast", "priority").
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExperimentalMode {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostBreakdown>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub provider_body: HashMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub provider_headers: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// CostBreakdown
// ---------------------------------------------------------------------------

/// The condition that activates a models.dev pricing tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostTierCondition {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// One conditional pricing tier. All prices are USD per 1M tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostTier {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_audio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_audio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<f64>,
    pub tier: CostTierCondition,
}

/// Full pricing breakdown for one model.  All values are USD per 1M tokens.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CostBreakdown {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_audio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_audio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<f64>,
    /// Generic conditional tiers from models.dev. Unlike the legacy field
    /// below, each entry retains its actual activation threshold.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<CostTier>,
    /// Pricing tier when the prompt exceeds 200K tokens (currently used by
    /// Claude on certain providers).  Not recursive — this is the only depth
    /// models.dev supports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_over_200k: Option<Box<CostBreakdown>>,
}

// ---------------------------------------------------------------------------
// ProviderEntry
// ---------------------------------------------------------------------------

/// Provider-level metadata captured from models.dev.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub id: ProviderId,
    pub name: String,
    /// Environment variable names that may supply this provider's credentials.
    #[serde(default)]
    pub env: Vec<String>,
    /// Default base URL for the provider's API (may be `None` for providers
    /// that require user-supplied URLs, e.g. self-hosted deployments).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// AI-SDK npm package for this provider (informational).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    /// Documentation URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

// ---------------------------------------------------------------------------
// ModelEntry
// ---------------------------------------------------------------------------

/// Lossless representation of one model.  Mirrors models.dev schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Identifier, provider link, name, context window, output limit.
    pub info: ModelInfo,

    // ---- Identity & lifecycle ---------------------------------------------
    /// Model family (`"claude"`, `"gpt"`, `"gemini"`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Lifecycle status; influences default visibility in pickers.
    #[serde(default)]
    pub status: ModelStatus,
    /// First public availability (ISO 8601 date string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    /// Last meaningful update on models.dev (ISO 8601 date string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<String>,
    /// Knowledge cutoff (free-form, e.g. `"2024-09"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge: Option<String>,
    /// Whether the model weights are publicly available.
    #[serde(default)]
    pub open_weights: bool,

    // ---- Capability flags -------------------------------------------------
    /// Supports tool / function calling.
    #[serde(default)]
    pub tool_calling: bool,
    /// Supports extended thinking / reasoning.
    #[serde(default)]
    pub reasoning: bool,
    /// Supports structured (JSON-schema) output.
    #[serde(default)]
    pub structured_output: bool,
    /// Honours the `temperature` parameter.
    #[serde(default = "default_true")]
    pub temperature: bool,
    /// Accepts file attachments (PDF, images, etc.).
    #[serde(default)]
    pub attachment: bool,
    /// How reasoning content is delivered when streaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interleaved: Option<InterleavedReasoning>,

    // ---- Modalities -------------------------------------------------------
    /// Input modalities (text, image, audio, video, pdf).
    #[serde(default)]
    pub modalities_input: Vec<Modality>,
    /// Output modalities.
    #[serde(default)]
    pub modalities_output: Vec<Modality>,

    // ---- Pricing (top-level mirrors are kept for backward compat) ---------
    /// USD per 1M input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_input: Option<f64>,
    /// USD per 1M output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_output: Option<f64>,
    /// USD per 1M cache-read tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_cache_read: Option<f64>,
    /// USD per 1M cache-write tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_cache_write: Option<f64>,
    /// Full cost breakdown including audio and reasoning premiums.  Use this
    /// instead of the top-level `cost_*` fields when you need every tier.
    #[serde(default)]
    pub cost: CostBreakdown,

    // ---- SDK overrides ----------------------------------------------------
    /// Override for the provider-level npm SDK / API URL on a per-model basis
    /// (e.g. when a "minimax-m2.7" hosted on opencode-go uses the Anthropic
    /// SDK rather than the openai-compatible one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_override: Option<ProviderOverride>,
    /// Per-mode dispatch alternatives (rarely populated).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub experimental_modes: HashMap<String, ExperimentalMode>,
    /// Default request-body options for this model, camelCase-keyed. Populated
    /// for entries synthesized from an [`ExperimentalMode`] (opencode
    /// `provider.body`, see [`expand_experimental_modes`]); empty for base
    /// catalog models.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub options: HashMap<String, serde_json::Value>,
    /// Default request headers for this model. Populated for synthesized
    /// experimental-mode entries (opencode `provider.headers`); empty for base
    /// catalog models.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

fn default_true() -> bool { true }

impl ModelEntry {
    /// Whether this model accepts image input.  Derived from `modalities_input`.
    ///
    /// Kept as a method (rather than a field) so the registry only stores one
    /// source of truth.
    pub fn vision(&self) -> bool {
        self.modalities_input.contains(&Modality::Image)
    }

    /// Whether this model accepts audio input.
    pub fn audio_input(&self) -> bool {
        self.modalities_input.contains(&Modality::Audio)
    }

    /// Whether this model accepts PDF input.
    pub fn pdf_input(&self) -> bool {
        self.modalities_input.contains(&Modality::Pdf)
    }

    /// Whether this model accepts video input.
    pub fn video_input(&self) -> bool {
        self.modalities_input.contains(&Modality::Video)
    }
}

// ---------------------------------------------------------------------------
// models.dev raw schema (for parsing)
// ---------------------------------------------------------------------------

/// Raw shape of `models.dev/api.json`.  We deserialize into this and then
/// transform into the internal `ProviderEntry` / `ModelEntry` records.
#[allow(dead_code)]
mod md {
    use super::*;

    pub type ApiJson = HashMap<String, Provider>;

    #[derive(Debug, Deserialize)]
    pub struct Provider {
        pub id: String,
        pub name: String,
        #[serde(default)]
        pub env: Vec<String>,
        #[serde(default)]
        pub api: Option<String>,
        #[serde(default)]
        pub npm: Option<String>,
        #[serde(default)]
        pub doc: Option<String>,
        #[serde(default)]
        pub models: HashMap<String, Model>,
    }

    #[derive(Debug, Deserialize)]
    pub struct Model {
        pub id: String,
        pub name: String,
        #[serde(default)]
        pub family: Option<String>,
        #[serde(default)]
        pub status: Option<ModelStatus>,
        #[serde(default)]
        pub release_date: Option<String>,
        #[serde(default)]
        pub last_updated: Option<String>,
        #[serde(default)]
        pub knowledge: Option<String>,
        #[serde(default)]
        pub open_weights: bool,
        #[serde(default)]
        pub attachment: bool,
        #[serde(default = "default_true")]
        pub temperature: bool,
        #[serde(default)]
        pub tool_call: bool,
        #[serde(default)]
        pub reasoning: bool,
        #[serde(default)]
        pub structured_output: bool,
        #[serde(default)]
        pub interleaved: Option<InterleavedReasoning>,
        #[serde(default)]
        pub modalities: Option<Modalities>,
        #[serde(default)]
        pub cost: Option<MdCost>,
        #[serde(default)]
        pub limit: Option<MdLimit>,
        #[serde(default)]
        pub provider: Option<ProviderOverride>,
        #[serde(default)]
        pub experimental: Option<MdExperimental>,
    }

    #[derive(Debug, Deserialize)]
    pub struct Modalities {
        #[serde(default)]
        pub input: Vec<Modality>,
        #[serde(default)]
        pub output: Vec<Modality>,
    }

    #[derive(Debug, Deserialize, Clone)]
    pub struct MdCost {
        #[serde(default)]
        pub input: Option<f64>,
        #[serde(default)]
        pub output: Option<f64>,
        #[serde(default)]
        pub cache_read: Option<f64>,
        #[serde(default)]
        pub cache_write: Option<f64>,
        #[serde(default)]
        pub input_audio: Option<f64>,
        #[serde(default)]
        pub output_audio: Option<f64>,
        #[serde(default)]
        pub reasoning: Option<f64>,
        #[serde(default)]
        pub tiers: Vec<CostTier>,
        #[serde(default)]
        pub context_over_200k: Option<Box<MdCost>>,
    }

    impl From<MdCost> for CostBreakdown {
        fn from(c: MdCost) -> Self {
            CostBreakdown {
                input: c.input,
                output: c.output,
                cache_read: c.cache_read,
                cache_write: c.cache_write,
                input_audio: c.input_audio,
                output_audio: c.output_audio,
                reasoning: c.reasoning,
                tiers: c.tiers,
                context_over_200k: c.context_over_200k.map(|c| Box::new((*c).into())),
            }
        }
    }

    #[derive(Debug, Deserialize)]
    pub struct MdLimit {
        #[serde(default)]
        pub context: Option<u64>,
        #[serde(default)]
        pub input: Option<u64>,
        #[serde(default)]
        pub output: Option<u64>,
    }

    #[derive(Debug, Deserialize)]
    pub struct MdExperimental {
        #[serde(default)]
        pub modes: HashMap<String, MdExperimentalMode>,
    }

    #[derive(Debug, Deserialize)]
    pub struct MdExperimentalMode {
        #[serde(default)]
        pub cost: Option<MdCost>,
        #[serde(default)]
        pub provider: Option<MdExperimentalProvider>,
    }

    #[derive(Debug, Deserialize)]
    pub struct MdExperimentalProvider {
        #[serde(default)]
        pub body: HashMap<String, serde_json::Value>,
        #[serde(default)]
        pub headers: HashMap<String, String>,
    }

    fn default_true() -> bool { true }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Result of parsing a models.dev `api.json` payload.
#[derive(Debug, Default)]
struct ParsedSnapshot {
    providers: HashMap<String, ProviderEntry>,
    models: HashMap<String, ModelEntry>,
}

fn parse_snapshot_bytes(bytes: &[u8]) -> Option<ParsedSnapshot> {
    let api: md::ApiJson = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, "Failed to parse models.dev snapshot");
            return None;
        }
    };
    Some(transform_api(api))
}

fn parse_snapshot_str(s: &str) -> Option<ParsedSnapshot> {
    parse_snapshot_bytes(s.as_bytes())
}

// models.dev uses "opencode" as the top-level key, but our provider config and
// free-model detection expect "opencode-zen". Remap at parse time so a snapshot
// refresh never silently breaks free-model detection again.
fn remap_provider_id(id: &str) -> &str {
    match id {
        "opencode" => "opencode-zen",
        other => other,
    }
}

fn normalize_provider_api(provider_id: &str, api: Option<String>) -> Option<String> {
    if matches!(
        provider_id,
        "minimax" | "minimax-cn" | "minimax-coding-plan" | "minimax-cn-coding-plan"
    ) {
        return api.map(|url| {
            if let Some(base) = url.strip_suffix("/v1") {
                base.to_owned()
            } else {
                url
            }
        });
    }

    api
}

fn transform_api(api: md::ApiJson) -> ParsedSnapshot {
    let mut out = ParsedSnapshot::default();

    for (raw_provider_id, p) in api.into_iter() {
        let provider_id = remap_provider_id(&raw_provider_id).to_string();
        let pid = ProviderId::new(provider_id.clone());

        out.providers.insert(provider_id.clone(), ProviderEntry {
            id: pid.clone(),
            name: p.name,
            env: p.env,
            api: normalize_provider_api(&provider_id, p.api),
            npm: p.npm,
            doc: p.doc,
        });

        for (model_id, m) in p.models.into_iter() {
            let mid = ModelId::new(model_id.clone());
            let key = format!("{}/{}", provider_id, model_id);

            // Resolve context window / output limit (default to 4K if absent —
            // matches old behaviour).
            let (ctx_window, max_output) = match m.limit {
                Some(l) => (
                    l.context.unwrap_or(4_096) as u32,
                    l.output.unwrap_or(4_096) as u32,
                ),
                None => (4_096, 4_096),
            };

            // Cost: keep top-level mirrors so existing callers
            // (`entry.cost_input.unwrap_or(0.0)`) keep working.
            let cost: CostBreakdown = m.cost.clone().map(Into::into).unwrap_or_default();

            // Modalities: default to text-only when omitted.
            let (mod_in, mod_out) = match m.modalities {
                Some(m) => (m.input, m.output),
                None => (vec![Modality::Text], vec![Modality::Text]),
            };

            // Experimental modes -> internal map.
            let experimental_modes = m
                .experimental
                .map(|e| {
                    e.modes
                        .into_iter()
                        .map(|(k, v)| {
                            let mode = ExperimentalMode {
                                cost: v.cost.map(Into::into),
                                provider_body: v
                                    .provider
                                    .as_ref()
                                    .map(|p| p.body.clone())
                                    .unwrap_or_default(),
                                provider_headers: v
                                    .provider
                                    .as_ref()
                                    .map(|p| p.headers.clone())
                                    .unwrap_or_default(),
                            };
                            (k, mode)
                        })
                        .collect()
                })
                .unwrap_or_default();

            let entry = ModelEntry {
                info: ModelInfo {
                    id: mid,
                    provider_id: pid.clone(),
                    name: m.name,
                    context_window: ctx_window,
                    max_output_tokens: max_output,
                    release_date: m.release_date.clone(),
                    status: m.status.map(|s| format!("{s:?}").to_lowercase()),
                },
                family: m.family,
                status: m.status.unwrap_or_default(),
                release_date: m.release_date,
                last_updated: m.last_updated,
                knowledge: m.knowledge,
                open_weights: m.open_weights,
                tool_calling: m.tool_call,
                reasoning: m.reasoning,
                structured_output: m.structured_output,
                temperature: m.temperature,
                attachment: m.attachment,
                interleaved: m.interleaved,
                modalities_input: mod_in,
                modalities_output: mod_out,
                cost_input: cost.input,
                cost_output: cost.output,
                cost_cache_read: cost.cache_read,
                cost_cache_write: cost.cache_write,
                cost,
                provider_override: m.provider,
                experimental_modes,
                options: HashMap::new(),
                headers: HashMap::new(),
            };

            out.models.insert(key, entry);
        }
    }

    // Expand each model's experimental.modes into extra listed models, mirroring
    // opencode's `fromModelsDevProvider` (provider.ts:1247-1264).
    expand_experimental_modes(&mut out.models);

    out
}

/// Expand each model's `experimental.modes` into additional listed models,
/// mirroring opencode's `fromModelsDevProvider` (provider.ts:1247-1264).
///
/// For every model with a non-empty `experimental_modes` map, a sibling entry
/// is synthesized keyed `"<provider>/<model>-<mode>"`, named `"<name> <Mode>"`
/// (only the first letter of the mode is upper-cased, matching
/// `mode[0].toUpperCase()`), with:
///   * `cost` = the mode's `cost` deep-merged over the base cost (else base),
///   * `options` = the mode's `provider.body` with snake_case→camelCase keys,
///   * `headers` = the mode's `provider.headers` (else the base's).
///
/// The synthesized entries are what makes the extra modes appear in the picker.
fn expand_experimental_modes(models: &mut HashMap<String, ModelEntry>) {
    let mut synthesized: Vec<(String, ModelEntry)> = Vec::new();
    for entry in models.values() {
        if entry.experimental_modes.is_empty() {
            continue;
        }
        let base_id: &str = &entry.info.id;
        let provider_id: &str = &entry.info.provider_id;
        for (mode, opts) in &entry.experimental_modes {
            let mode_id = format!("{base_id}-{mode}");
            let key = format!("{provider_id}/{mode_id}");
            let mut synth = entry.clone();
            synth.info.id = ModelId::new(mode_id);
            synth.info.name = format!("{} {}", entry.info.name, capitalize_first(mode));

            // cost: mode cost deep-merged over the base cost (opencode:
            // `opts.cost ? mergeDeep(base.cost, cost(opts.cost)) : base.cost`).
            if let Some(mode_cost) = &opts.cost {
                synth.cost = merge_cost(&entry.cost, mode_cost);
                synth.cost_input = synth.cost.input;
                synth.cost_output = synth.cost.output;
                synth.cost_cache_read = synth.cost.cache_read;
                synth.cost_cache_write = synth.cost.cache_write;
            }

            // options: provider.body with snake_case → camelCase keys
            // (opencode provider.ts:1256). When absent, keep the base's.
            if !opts.provider_body.is_empty() {
                synth.options = opts
                    .provider_body
                    .iter()
                    .map(|(k, v)| (snake_to_camel(k), v.clone()))
                    .collect();
            }

            // headers: mode headers override the base's (`opts.provider?.headers
            // ?? base.headers`).
            if !opts.provider_headers.is_empty() {
                synth.headers = opts.provider_headers.clone();
            }

            // A synthesized entry must not itself be re-expanded.
            synth.experimental_modes = HashMap::new();
            synthesized.push((key, synth));
        }
    }
    for (key, entry) in synthesized {
        models.insert(key, entry);
    }
}

/// Deep-merge cost `over` onto `base` (opencode `mergeDeep`): a `Some` value in
/// `over` wins, a `None` keeps the `base` value; recurses into the 200K tier.
fn merge_cost(base: &CostBreakdown, over: &CostBreakdown) -> CostBreakdown {
    CostBreakdown {
        input: over.input.or(base.input),
        output: over.output.or(base.output),
        cache_read: over.cache_read.or(base.cache_read),
        cache_write: over.cache_write.or(base.cache_write),
        input_audio: over.input_audio.or(base.input_audio),
        output_audio: over.output_audio.or(base.output_audio),
        reasoning: over.reasoning.or(base.reasoning),
        tiers: if over.tiers.is_empty() {
            base.tiers.clone()
        } else {
            over.tiers.clone()
        },
        context_over_200k: match (&over.context_over_200k, &base.context_over_200k) {
            (Some(o), Some(b)) => Some(Box::new(merge_cost(b, o))),
            (Some(o), None) => Some(o.clone()),
            (None, b) => b.clone(),
        },
    }
}

/// Convert snake_case keys to camelCase, matching opencode's
/// `k.replace(/_([a-z])/g, (_, c) => c.toUpperCase())` (provider.ts:1256): only
/// an underscore immediately followed by an ASCII lowercase letter is collapsed
/// (upper-casing that letter); every other underscore is preserved verbatim.
fn snake_to_camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut chars = key.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            if let Some(&next) = chars.peek() {
                if next.is_ascii_lowercase() {
                    out.push(next.to_ascii_uppercase());
                    chars.next();
                    continue;
                }
            }
            out.push('_');
        } else {
            out.push(c);
        }
    }
    out
}

/// Upper-case only the first character of `s` (mirrors opencode's
/// `${mode[0].toUpperCase()}${mode.slice(1)}`).
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// ModelRegistry
// ---------------------------------------------------------------------------

/// In-memory registry of every known provider and model.
///
/// Hydrated on construction from the embedded models.dev snapshot, then
/// optionally overlaid from disk cache and refreshed from the network.
pub struct ModelRegistry {
    /// Keyed by `"provider_id/model_id"`.
    entries: HashMap<String, ModelEntry>,
    /// Keyed by provider id.
    providers: HashMap<String, ProviderEntry>,
    /// Optional path for on-disk persistence.
    cache_path: Option<PathBuf>,
    /// Minimum age before a network refresh is attempted again (mtime-based).
    refresh_interval: Duration,
    /// User-supplied metadata overrides, keyed by `"provider/model"`. Re-applied
    /// after every catalog merge so they stay authoritative across cache reloads
    /// and network refreshes (issue #309).
    overrides: HashMap<String, claurst_core::config::ModelOverride>,
}

impl ModelRegistry {
    /// Create a new registry pre-populated with the bundled snapshot.
    pub fn new() -> Self {
        let mut registry = Self {
            entries: HashMap::new(),
            providers: HashMap::new(),
            cache_path: None,
            refresh_interval: Duration::from_secs(5 * 60),
            overrides: HashMap::new(),
        };
        registry.load_bundled_snapshot();
        registry
    }

    /// Configure a cache file path for persistence between sessions.
    pub fn with_cache_path(mut self, path: PathBuf) -> Self {
        self.cache_path = Some(path);
        self
    }

    fn load_bundled_snapshot(&mut self) {
        if let Some(parsed) = parse_snapshot_bytes(BUNDLED_SNAPSHOT) {
            self.entries = parsed.models;
            self.providers = parsed.providers;
            tracing::debug!(
                providers = self.providers.len(),
                models = self.entries.len(),
                "Loaded bundled models.dev snapshot"
            );
        } else {
            tracing::warn!("Embedded models snapshot failed to parse; registry empty");
        }
    }

    // -----------------------------------------------------------------------
    // Queries — models
    // -----------------------------------------------------------------------

    /// Get an entry by `"provider_id/model_id"` key.
    pub fn get(&self, provider_id: &str, model_id: &str) -> Option<&ModelEntry> {
        let key = format!("{}/{}", provider_id, model_id);
        self.entries.get(&key)
    }

    /// Resolve a model string into `(ProviderId, ModelId)`.
    ///
    /// Accepts either `"provider/model"` or a bare model name (which defaults
    /// to the Anthropic provider for backward-compat).
    pub fn resolve(s: &str) -> (ProviderId, ModelId) {
        if let Some((provider, model)) = s.split_once('/') {
            (ProviderId::new(provider), ModelId::new(model))
        } else {
            (ProviderId::new(ProviderId::ANTHROPIC), ModelId::new(s))
        }
    }

    /// Look up a bare model name across all registry entries and return the
    /// provider that owns it.  Returns `None` if the model is not found or
    /// if the model string already contains a `"provider/"` prefix.
    pub fn find_provider_for_model(&self, model_name: &str) -> Option<ProviderId> {
        if model_name.contains('/') {
            return None;
        }

        // Family-based heuristic FIRST: well-known model name prefixes always
        // map to their canonical provider.  Prevents gateway/proxy entries in
        // the registry from hijacking well-known models like claude-* or gpt-*.
        let canonical: Option<&'static str> = if model_name.starts_with("claude") {
            Some(ProviderId::ANTHROPIC)
        } else if model_name.starts_with("gpt-")
            || model_name.starts_with("o1")
            || model_name.starts_with("o3")
            || model_name.starts_with("o4")
        {
            Some(ProviderId::OPENAI)
        } else if model_name.starts_with("gemini") || model_name.starts_with("gemma") {
            Some(ProviderId::GOOGLE)
        } else if model_name.starts_with("deepseek") {
            Some(ProviderId::DEEPSEEK)
        } else if model_name.starts_with("mistral")
            || model_name.starts_with("codestral")
            || model_name.starts_with("pixtral")
        {
            Some(ProviderId::MISTRAL)
        } else if model_name.starts_with("grok") {
            Some(ProviderId::XAI)
        } else if model_name.starts_with("command-r") || model_name.starts_with("command-a") {
            Some(ProviderId::COHERE)
        } else if model_name.starts_with("sonar") {
            Some(ProviderId::PERPLEXITY)
        } else if model_name.starts_with("glm-") {
            Some(ProviderId::ZAI)
        } else {
            None
        };
        if let Some(pid) = canonical {
            return Some(ProviderId::new(pid));
        }

        // Exact match
        for entry in self.entries.values() {
            if &*entry.info.id == model_name {
                return Some(entry.info.provider_id.clone());
            }
        }

        // Prefix match (handles version suffixes)
        for entry in self.entries.values() {
            if (*entry.info.id).starts_with(model_name)
                || model_name.starts_with(&*entry.info.id)
            {
                return Some(entry.info.provider_id.clone());
            }
        }

        None
    }

    /// List all models for a given provider.
    ///
    /// The provider id is canonicalized to its models.dev snapshot key first
    /// (see [`canonical_snapshot_key`]), so a runtime id that differs from the
    /// catalog key (e.g. `"qwen"` → `"alibaba"`) still resolves instead of
    /// silently returning nothing.
    pub fn list_by_provider(&self, provider_id: &str) -> Vec<&ModelEntry> {
        let key = canonical_snapshot_key(provider_id);
        self.entries
            .values()
            .filter(|e| &*e.info.provider_id == key)
            .collect()
    }

    /// List models for a provider, filtered to those that should appear in
    /// default UI listings (active/beta only — no alpha or deprecated).
    ///
    /// Set the `CLAURST_ENABLE_EXPERIMENTAL_MODELS=1` env var to also include
    /// alpha/deprecated entries.
    pub fn list_visible_by_provider(&self, provider_id: &str) -> Vec<&ModelEntry> {
        let show_all = std::env::var("CLAURST_ENABLE_EXPERIMENTAL_MODELS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        self.list_by_provider(provider_id)
            .into_iter()
            .filter(|e| show_all || e.status.is_listed_by_default())
            .collect()
    }

    /// Pick the best default model for a provider.
    ///
    /// **Selection rule** (opencode-style — the priority list governs the
    /// default, *not* `release_date`):
    ///   1. Prefer non-alpha, non-deprecated models.
    ///   2. Rank by a small, provider-agnostic [`PREFERRED_FLAGSHIPS`] priority
    ///      list (local runtimes use their capability list instead). This is
    ///      version-agnostic on purpose: `"claude-opus"` always beats
    ///      `"claude-sonnet"`/`"claude-haiku"`, so a new Opus point-release
    ///      surfaces with no code change.
    ///   3. Tie-break by [`cmp_ids_newest_first`] — a version-aware id compare,
    ///      so `opus-4-8` > `opus-4-7` > `opus-4-10`-vs-`4-9` (numeric), and a
    ///      brand-new high-version id with *no* `release_date` still wins.
    ///
    /// `release_date` deliberately does **not** drive this (that would regress
    /// Anthropic→Sonnet and Google→flash-lite); it is used for *listing* only.
    pub fn best_model_for_provider(&self, provider_id: &str) -> Option<String> {
        let provider_id = canonical_snapshot_key(provider_id);
        let mut models = self.list_visible_by_provider(provider_id);
        if models.is_empty() {
            // Fallback: include alpha/deprecated rather than return nothing
            models = self.list_by_provider(provider_id);
        }
        if models.is_empty() {
            return None;
        }

        let priority_patterns = flagship_patterns_for(provider_id);

        models.sort_by(|a, b| {
            let id_a: &str = &a.info.id;
            let id_b: &str = &b.info.id;

            // 1. Priority-list index (lower = preferred flagship).
            let prio_a = priority_patterns
                .iter()
                .position(|pat| id_a.contains(pat))
                .unwrap_or(usize::MAX);
            let prio_b = priority_patterns
                .iter()
                .position(|pat| id_b.contains(pat))
                .unwrap_or(usize::MAX);

            prio_a
                .cmp(&prio_b)
                // 2. Version-aware id compare (newest first). This also makes a
                //    rolling "-latest"/clean alias win over a date-pinned
                //    sibling, without a separate (and footgun-prone) "latest"
                //    bonus that mis-fires on ids like "gpt-5.3-chat-latest".
                .then_with(|| cmp_ids_newest_first(id_a, id_b))
        });

        models.first().map(|e| e.info.id.to_string())
    }

    /// Pick the best "small" (fast/cheap) model for a provider.
    pub fn best_small_model_for_provider(&self, provider_id: &str) -> Option<String> {
        let provider_id = canonical_snapshot_key(provider_id);
        let mut models = self.list_visible_by_provider(provider_id);
        if models.is_empty() {
            models = self.list_by_provider(provider_id);
        }
        if models.is_empty() {
            return None;
        }

        let small_priority = small_patterns_for(provider_id);

        models.sort_by(|a, b| {
            let id_a: &str = &a.info.id;
            let id_b: &str = &b.info.id;

            let prio_a = small_priority
                .iter()
                .position(|pat| id_a.contains(pat))
                .unwrap_or(usize::MAX);
            let prio_b = small_priority
                .iter()
                .position(|pat| id_b.contains(pat))
                .unwrap_or(usize::MAX);

            prio_a
                .cmp(&prio_b)
                .then_with(|| {
                    let rd_a = a.release_date.as_deref().unwrap_or("");
                    let rd_b = b.release_date.as_deref().unwrap_or("");
                    rd_b.cmp(rd_a)
                })
                .then_with(|| id_b.cmp(id_a))
        });

        models.first().map(|e| e.info.id.to_string())
    }

    /// List every entry in the registry.
    pub fn list_all(&self) -> Vec<&ModelEntry> {
        self.entries.values().collect()
    }

    /// Number of models in the registry.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if the registry has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    // -----------------------------------------------------------------------
    // Queries — providers
    // -----------------------------------------------------------------------

    /// Get provider metadata by id.
    pub fn provider(&self, provider_id: &str) -> Option<&ProviderEntry> {
        self.providers.get(provider_id)
    }

    /// List all known providers (sorted by id for stable output).
    pub fn list_providers(&self) -> Vec<&ProviderEntry> {
        let mut v: Vec<&ProviderEntry> = self.providers.values().collect();
        // ProviderId sorts by its `&str` deref; a sort_by_key would need an
        // owned key, so keep the explicit comparator.
        #[allow(clippy::unnecessary_sort_by)]
        v.sort_by(|a, b| (*a.id).cmp(&*b.id));
        v
    }

    /// Number of providers in the registry.
    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    // -----------------------------------------------------------------------
    // Network refresh
    // -----------------------------------------------------------------------

    /// Resolve the models.dev source URL, honoring env-var overrides.
    fn source_url() -> String {
        std::env::var("CLAURST_MODELS_URL")
            .or_else(|_| std::env::var("MODELS_DEV_URL"))
            .unwrap_or_else(|_| "https://models.dev/api.json".to_string())
    }

    /// Whether the configured cache file is newer than the refresh interval.
    fn cache_is_fresh(&self) -> bool {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return false,
        };
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        let mtime = match meta.modified() {
            Ok(t) => t,
            Err(_) => return false,
        };
        match mtime.elapsed() {
            Ok(age) => age < self.refresh_interval,
            Err(_) => true, // future mtime → treat as fresh
        }
    }

    /// Attempt to refresh the registry from the models.dev public API.
    ///
    /// Returns `Ok(true)` if new data was fetched, `Ok(false)` if the cache
    /// was still fresh.  Honors `CLAURST_DISABLE_MODELS_FETCH`.  All network
    /// or parse failures are silenced — the bundled snapshot is always
    /// sufficient.
    pub async fn refresh_from_models_dev(&mut self) -> anyhow::Result<bool> {
        if std::env::var("CLAURST_DISABLE_MODELS_FETCH").is_ok() {
            return Ok(false);
        }
        if self.cache_is_fresh() {
            return Ok(false);
        }

        let url = Self::source_url();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        let resp = client.get(&url).send().await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let text = r.text().await?;
                if let Some(parsed) = parse_snapshot_str(&text) {
                    self.merge_entries(parsed.models);
                    self.providers.extend(parsed.providers);
                    // Re-assert user overrides over the freshly fetched catalog.
                    self.reapply_overrides();
                    if let Some(ref path) = self.cache_path.clone() {
                        // Write the raw response so future loads can re-parse
                        // it identically.  Best-effort; ignore I/O errors.
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::write(path, &text);
                    }
                    return Ok(true);
                }
                Ok(false)
            }
            // Fail silently — bundled snapshot is sufficient.
            _ => Ok(false),
        }
    }

    // -----------------------------------------------------------------------
    // Cache persistence
    // -----------------------------------------------------------------------

    /// Load a previously saved cache file, merging entries into the registry.
    ///
    /// The cache file may be either:
    ///   1. The raw models.dev `api.json` response (providers at the top level), or
    ///   2. Our own serialized `HashMap<String, ModelEntry>` format (legacy).
    ///
    /// Both formats are tried in order so the background fetch can simply
    /// save the raw models.dev response to disk and this method will ingest
    /// it.
    pub fn load_cache(&mut self, path: &PathBuf) {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(_) => return,
        };

        // Try models.dev raw format first.
        if let Some(parsed) = parse_snapshot_str(&data) {
            // Only overwrite if the cache yielded at least one model — guards
            // against an accidentally-empty response wiping the bundled
            // snapshot.
            if !parsed.models.is_empty() {
                self.merge_entries(parsed.models);
                self.providers.extend(parsed.providers);
                // Re-assert user overrides over the freshly merged catalog.
                self.reapply_overrides();
                return;
            }
        }

        // Legacy: our own serialized HashMap<String, ModelEntry> format.
        if let Ok(entries) = serde_json::from_str::<HashMap<String, ModelEntry>>(&data) {
            self.merge_entries(entries);
            self.reapply_overrides();
        }
    }

    /// Merge incoming catalog entries into the registry, preserving existing
    /// non-null metadata when the incoming record omits it.
    ///
    /// A thin refresh (e.g. a partial models.dev response, or a custom mirror)
    /// must never blank out fields the bundled snapshot already had: an
    /// incoming `None` for `release_date`/`last_updated`/`knowledge`/`family`
    /// keeps the existing `Some`, and an incoming default `Active` status does
    /// not overwrite an existing non-default lifecycle status. Everything the
    /// refresh *does* specify wins.
    fn merge_entries(&mut self, incoming: HashMap<String, ModelEntry>) {
        for (key, mut entry) in incoming {
            if let Some(existing) = self.entries.get(&key) {
                entry.release_date = entry.release_date.or_else(|| existing.release_date.clone());
                entry.last_updated = entry.last_updated.or_else(|| existing.last_updated.clone());
                entry.knowledge = entry.knowledge.or_else(|| existing.knowledge.clone());
                entry.family = entry.family.or_else(|| existing.family.clone());
                if entry.status == ModelStatus::Active && existing.status != ModelStatus::Active {
                    entry.status = existing.status;
                }
                // Keep the richer ModelInfo metadata too (mirrors the fields).
                if entry.info.release_date.is_none() {
                    entry.info.release_date = existing.info.release_date.clone();
                }
                if entry.info.status.is_none() {
                    entry.info.status = existing.info.status.clone();
                }
            }
            self.entries.insert(key, entry);
        }
    }

    /// Register user-supplied [`ModelOverride`]s and layer them onto the catalog.
    ///
    /// Precedence is **user override > models.dev > built-in defaults**: each
    /// `Some` field replaces the catalog value for the keyed `"provider/model"`,
    /// leaving unset fields untouched. A key with no catalog entry — a
    /// self-hosted alias, or an id models.dev does not know — is materialised
    /// into a synthetic entry so context-window-dependent logic (the model
    /// picker, token warnings, auto-compact thresholds) sizes it from the
    /// override instead of mismatching it to an unrelated catalog model.
    ///
    /// The overrides are retained and automatically re-applied after every
    /// later cache reload ([`Self::load_cache`]) and network refresh
    /// ([`Self::refresh_from_models_dev`]) so a fresh catalog merge can never
    /// silently clobber them.
    ///
    /// Keys without a `'/'` separator are skipped: the registry is keyed by
    /// `"provider/model"` and a bare id cannot be placed unambiguously.
    ///
    /// [`ModelOverride`]: claurst_core::config::ModelOverride
    pub fn apply_model_overrides(
        &mut self,
        overrides: &HashMap<String, claurst_core::config::ModelOverride>,
    ) {
        for (key, ov) in overrides {
            self.overrides.insert(key.clone(), ov.clone());
        }
        apply_overrides_to_entries(&mut self.entries, &self.overrides);
    }

    /// Re-apply the retained overrides after a catalog merge, so a cache reload
    /// or refresh never wipes out user-corrected metadata. No-op when no
    /// overrides are registered.
    fn reapply_overrides(&mut self) {
        if self.overrides.is_empty() {
            return;
        }
        apply_overrides_to_entries(&mut self.entries, &self.overrides);
    }
}

/// Layer `overrides` onto `entries`: patch existing catalog rows in place and
/// synthesize a minimal entry for any `"provider/model"` key the catalog lacks.
/// Malformed keys (no `'/'`, or an empty half) and empty overrides are skipped.
fn apply_overrides_to_entries(
    entries: &mut HashMap<String, ModelEntry>,
    overrides: &HashMap<String, claurst_core::config::ModelOverride>,
) {
    for (key, ov) in overrides {
        if ov.is_empty() {
            continue;
        }
        let Some((provider, model)) = key.split_once('/') else {
            tracing::warn!(key = %key, "model override key must be \"provider/model\"; skipping");
            continue;
        };
        if provider.is_empty() || model.is_empty() {
            tracing::warn!(key = %key, "model override key must be \"provider/model\"; skipping");
            continue;
        }

        let registry_key = format!("{}/{}", provider, model);
        match entries.get_mut(&registry_key) {
            Some(entry) => {
                // Patch: each Some field wins over the catalog value.
                if let Some(cw) = ov.context_window {
                    entry.info.context_window = cw;
                }
                if let Some(mo) = ov.max_output_tokens {
                    entry.info.max_output_tokens = mo;
                }
                if let Some(name) = &ov.name {
                    entry.info.name = name.clone();
                }
                if let Some(rd) = &ov.release_date {
                    entry.release_date = Some(rd.clone());
                    entry.info.release_date = Some(rd.clone());
                }
                if let Some(st) = &ov.status {
                    entry.status = model_status_from_str(st);
                    entry.info.status = Some(st.clone());
                }
            }
            None => {
                // Materialise a synthetic entry for an id the catalog lacks.
                let entry = ModelEntry {
                    info: ModelInfo {
                        id: ModelId::new(model),
                        provider_id: ProviderId::new(provider),
                        name: ov.name.clone().unwrap_or_else(|| model.to_string()),
                        context_window: ov.context_window.unwrap_or(0),
                        max_output_tokens: ov.max_output_tokens.unwrap_or(0),
                        release_date: ov.release_date.clone(),
                        status: ov.status.clone(),
                    },
                    family: None,
                    status: ov
                        .status
                        .as_deref()
                        .map(model_status_from_str)
                        .unwrap_or_default(),
                    release_date: ov.release_date.clone(),
                    last_updated: None,
                    knowledge: None,
                    open_weights: false,
                    tool_calling: true,
                    reasoning: false,
                    structured_output: false,
                    temperature: true,
                    attachment: false,
                    interleaved: None,
                    modalities_input: vec![Modality::Text],
                    modalities_output: vec![Modality::Text],
                    cost_input: None,
                    cost_output: None,
                    cost_cache_read: None,
                    cost_cache_write: None,
                    cost: CostBreakdown::default(),
                    provider_override: None,
                    experimental_modes: HashMap::new(),
                    options: HashMap::new(),
                    headers: HashMap::new(),
                };
                entries.insert(registry_key, entry);
            }
        }
    }
}

/// Parse a models.dev lifecycle status string into [`ModelStatus`], defaulting
/// to `Active` for anything unrecognised.
fn model_status_from_str(s: &str) -> ModelStatus {
    match s.to_ascii_lowercase().as_str() {
        "beta" => ModelStatus::Beta,
        "alpha" => ModelStatus::Alpha,
        "deprecated" => ModelStatus::Deprecated,
        _ => ModelStatus::Active,
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Provider-specific flagship / small model patterns
// ---------------------------------------------------------------------------

/// Map a runtime provider id to the key models.dev uses in the catalog.
///
/// Some providers run under an id that differs from their snapshot key — e.g.
/// Alibaba's DashScope is `"qwen"` at runtime but lives under `"alibaba"` in
/// models.dev, and the Codex endpoint is `"openai-codex"` in the connect dialog
/// but `"codex"` everywhere else. Apply this at every catalog read so a lookup
/// never silently misses and falls through to an unrelated provider's default
/// (the bug that used to resolve `"qwen"` to a `claude-*` model).
pub(crate) fn canonical_snapshot_key(provider_id: &str) -> &str {
    match provider_id {
        "qwen" => "alibaba",
        "openai-codex" => "codex",
        other => other,
    }
}

/// Whether a provider id refers to a local runtime whose models are ranked by
/// capability (coder-first) rather than by the cloud-flagship priority list.
fn is_local_runtime(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "ollama" | "lmstudio" | "lm-studio" | "llamacpp" | "llama-cpp"
    )
}

/// Small, provider-agnostic priority list of preferred flagships (opencode
/// style). Replaces the old per-provider, version-pinned regex tables.
///
/// Entries are deliberately **family/tier level, not version-pinned** so a new
/// point-release surfaces with zero code changes — `"claude-opus"` keeps Opus
/// the Anthropic default through 4.6 → 4.7 → 4.8 → …, and the version-aware id
/// tie-break ([`cmp_ids_newest_first`]) picks the newest within the family. The
/// generic `"-pro"`/`"-max"` tier markers at the end catch flagship tiers for
/// providers without an explicit family entry (e.g. Google's `gemini-*-pro`).
const PREFERRED_FLAGSHIPS: &[&str] = &[
    "claude-opus",       // Anthropic flagship
    "gpt-5",             // OpenAI flagship family
    "grok-4",            // xAI
    "command-a",         // Cohere
    "glm-5",             // Z.ai
    "deepseek-v4",       // DeepSeek
    "deepseek-reasoner",
    "mistral-large",
    "mistral-medium",
    "sonar-pro",         // Perplexity
    "claude-sonnet",     // Anthropic mid-tier (below opus, above haiku)
    "-pro",              // generic flagship tier (Gemini pro, gpt-5-pro, …)
    "-max",              // generic flagship tier (qwen3.6-max, …)
];

/// Capability priority for local runtimes (coder-first), kept from the old
/// per-provider table.
const LOCAL_CAPABILITY_PATTERNS: &[&str] = &[
    "qwen3-coder",
    "qwen2.5-coder",
    "deepseek",
    "llama3.3",
    "llama3.1",
    "qwen2.5",
];

/// Priority list for [`ModelRegistry::best_model_for_provider`]: the local
/// capability list for local runtimes, otherwise the shared flagship list.
fn flagship_patterns_for(provider_id: &str) -> &'static [&'static str] {
    if is_local_runtime(provider_id) {
        LOCAL_CAPABILITY_PATTERNS
    } else {
        PREFERRED_FLAGSHIPS
    }
}

/// One run of a tokenized model id: a number, or a text segment.
#[derive(Debug, PartialEq, Eq)]
enum IdToken {
    Num(u64),
    Text(String),
}

/// Split a model id into alternating numeric / text runs so versions compare
/// numerically (`opus-4-10` > `opus-4-9`) instead of lexically.
fn tokenize_id(id: &str) -> Vec<IdToken> {
    let mut out = Vec::new();
    let mut chars = id.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut n: u64 = 0;
            while let Some(&d) = chars.peek() {
                match d.to_digit(10) {
                    Some(dig) => {
                        n = n.saturating_mul(10).saturating_add(dig as u64);
                        chars.next();
                    }
                    None => break,
                }
            }
            out.push(IdToken::Num(n));
        } else {
            let mut s = String::new();
            while let Some(&t) = chars.peek() {
                if t.is_ascii_digit() {
                    break;
                }
                s.push(t);
                chars.next();
            }
            out.push(IdToken::Text(s));
        }
    }
    out
}

/// Compare two model ids "newest first" for use as a sort tie-break.
///
/// Compares run-by-run: numbers numerically (higher = newer), text segments
/// lexically (greater = newer, so a `-latest` alias beats a date-pinned
/// sibling), and a number outranks text at the same position. When all shared
/// runs are equal the shorter id (the cleaner rolling alias, e.g.
/// `claude-opus-4-5` over `claude-opus-4-5-20251101`) is treated as newer.
fn cmp_ids_newest_first(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ta = tokenize_id(a);
    let tb = tokenize_id(b);
    for (x, y) in ta.iter().zip(tb.iter()) {
        // `Greater` == x is newer.
        let newer = match (x, y) {
            (IdToken::Num(na), IdToken::Num(nb)) => {
                // A run of >= 6 digits is a date stamp (YYYYMM / YYYYMMDD), not
                // a version component: a date-pinned alias must NOT outrank the
                // clean point-release, so `claude-opus-4-8` beats the dated
                // `claude-opus-4-20250514` (which is really 4.0). Version-like
                // numbers therefore rank newer than date-like ones; within the
                // same class, compare numerically (so 4-10 > 4-9, newer date
                // wins among date-pinned siblings).
                let date_a = *na >= 100_000;
                let date_b = *nb >= 100_000;
                match (date_a, date_b) {
                    (false, true) => Ordering::Greater,
                    (true, false) => Ordering::Less,
                    _ => na.cmp(nb),
                }
            }
            (IdToken::Text(sa), IdToken::Text(sb)) => sa.cmp(sb),
            (IdToken::Num(_), IdToken::Text(_)) => Ordering::Greater,
            (IdToken::Text(_), IdToken::Num(_)) => Ordering::Less,
        };
        if newer != Ordering::Equal {
            return newer.reverse(); // newest (Greater) sorts first (Less)
        }
    }
    // Shorter id (fewer runs) is the cleaner alias → sorts first.
    ta.len().cmp(&tb.len())
}

/// Substring patterns marking a model as the lightweight/cheap default.
fn small_patterns_for(provider_id: &str) -> &'static [&'static str] {
    match provider_id {
        "anthropic" | "amazon-bedrock" | "github-copilot" | "azure" => &[
            "claude-haiku-4",
            "claude-haiku-3-5",
            "claude-haiku",
        ],
        "openai" => &["gpt-5-mini", "gpt-4o-mini", "o4-mini", "o3-mini"],
        "google" => &["gemini-2.5-flash-lite", "gemini-2.5-flash", "gemini-2.0-flash"],
        "deepseek" => &["deepseek-v4-flash", "deepseek-chat"],
        "mistral" => &["mistral-small", "mistral-nemo"],
        "xai" => &["grok-3-mini", "grok-2-mini"],
        "cohere" => &["command-r7b", "command-r"],
        "groq" => &["llama-3.1-8b", "gemma2-9b"],
        "openrouter" => &[
            "anthropic/claude-haiku",
            "openai/gpt-4o-mini",
            "google/gemini-2.5-flash",
        ],
        "zai" => &["glm-5-turbo", "glm-4.7"],
        _ => &["mini", "haiku", "flash", "lite", "small", "nano"],
    }
}

// ---------------------------------------------------------------------------
// Dynamic model resolution helper
// ---------------------------------------------------------------------------

/// Resolve the effective model for a [`Config`], using the model registry to
/// dynamically pick the best available model for the active provider.
///
/// **Resolution order**:
///  1. If the user explicitly set `config.model`, use it verbatim.
///  2. Consult the model registry for the configured provider's best model.
///  3. Fall back to the hardcoded table in [`Config::effective_model()`].
pub fn effective_model_for_config(
    config: &claurst_core::config::Config,
    registry: &ModelRegistry,
) -> String {
    if config.model.is_some() {
        return config.effective_model().to_string();
    }

    if let Some(provider_id) = config.provider.as_deref() {
        if let Some(best) = registry.best_model_for_provider(provider_id) {
            return best;
        }
    }

    config.effective_model().to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_snapshot_loads() {
        let reg = ModelRegistry::new();
        // Empty would mean the embed or parser broke.
        assert!(!reg.is_empty(), "bundled snapshot must populate registry");
        assert!(reg.provider_count() > 0, "providers must be populated");
    }

    #[test]
    fn well_known_providers_present() {
        let reg = ModelRegistry::new();
        for pid in ["anthropic", "openai", "google", "openrouter", "groq"] {
            assert!(
                reg.provider(pid).is_some(),
                "expected provider {pid} in bundled snapshot"
            );
            assert!(
                !reg.list_by_provider(pid).is_empty(),
                "expected at least one model for provider {pid}"
            );
        }
    }

    #[test]
    fn anthropic_has_claude_models() {
        let reg = ModelRegistry::new();
        let models = reg.list_by_provider("anthropic");
        let has_claude = models.iter().any(|m| (*m.info.id).starts_with("claude"));
        assert!(has_claude, "anthropic should have at least one claude model");
    }

    #[test]
    fn best_model_for_anthropic_is_claude() {
        let reg = ModelRegistry::new();
        let best = reg.best_model_for_provider("anthropic");
        assert!(best.is_some(), "anthropic must have a default model");
        assert!(
            best.unwrap().contains("claude"),
            "anthropic default must be a claude variant"
        );
    }

    #[test]
    fn modalities_drive_vision() {
        let reg = ModelRegistry::new();
        if let Some(opus) = reg.list_by_provider("anthropic")
            .iter()
            .find(|m| (*m.info.id).contains("opus"))
        {
            // Opus models are multimodal — image input expected.
            assert!(
                opus.modalities_input.contains(&Modality::Image),
                "opus should accept image input"
            );
            assert!(opus.vision(), "opus.vision() must mirror image modality");
        }
    }

    #[test]
    fn find_provider_for_model_canonical() {
        let reg = ModelRegistry::new();
        assert_eq!(
            reg.find_provider_for_model("claude-sonnet-4-6"),
            Some(ProviderId::new("anthropic"))
        );
        assert_eq!(
            reg.find_provider_for_model("gpt-4o"),
            Some(ProviderId::new("openai"))
        );
        assert_eq!(
            reg.find_provider_for_model("gemini-2.5-pro"),
            Some(ProviderId::new("google"))
        );
    }

    #[test]
    fn provider_metadata_populated() {
        let reg = ModelRegistry::new();
        let anthropic = reg.provider("anthropic").expect("anthropic provider");
        assert_eq!(anthropic.name, "Anthropic");
        assert!(anthropic.env.iter().any(|e| e == "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn opencode_remapped_to_opencode_zen() {
        // models.dev uses "opencode" as the top-level key; we must normalize it
        // to "opencode-zen" so free-model detection works correctly.
        let json = r#"{"opencode":{"id":"opencode","name":"OpenCode Zen","env":[],"models":{"qwen3.6-plus-free":{"id":"qwen3.6-plus-free","name":"Qwen 3.6 Plus Free","cost":{"input":0,"output":0},"limit":{"context":8192,"output":4096}}}}}"#;
        let parsed = parse_snapshot_str(json).expect("parse must succeed");
        assert!(
            parsed.providers.contains_key("opencode-zen"),
            "provider must be stored as opencode-zen"
        );
        assert!(
            !parsed.providers.contains_key("opencode"),
            "raw opencode key must not appear in registry"
        );
        assert!(
            parsed.models.contains_key("opencode-zen/qwen3.6-plus-free"),
            "model key must use opencode-zen prefix"
        );
    }

    #[test]
    fn minimax_anthropic_api_root_excludes_the_messages_prefix() {
        let upstream_api = ["https://api.minimax.io/anthropic", "v1"].join("/");
        let json = serde_json::json!({
            "minimax": {
                "id": "minimax",
                "name": "MiniMax",
                "api": upstream_api,
                "models": {}
            }
        })
        .to_string();
        let parsed = parse_snapshot_str(&json).expect("parse must succeed");
        let provider = parsed.providers.get("minimax").expect("minimax provider");

        assert_eq!(
            provider.api.as_deref(),
            Some("https://api.minimax.io/anthropic")
        );
    }

    // ---- experimental.modes expansion (opencode provider.ts:1247-1264) ----

    #[test]
    fn experimental_modes_expand_into_listed_models() {
        // A model with an experimental.modes map must yield an extra sibling
        // entry keyed "<provider>/<model>-<mode>", with cost deep-merged, the
        // provider.body camelCased into `options`, and provider.headers carried.
        let json = r#"{"myprov":{"id":"myprov","name":"My Provider","env":[],"models":{"base-model":{"id":"base-model","name":"Base Model","cost":{"input":1.0,"output":2.0},"limit":{"context":100000,"output":8000},"experimental":{"modes":{"fast":{"cost":{"output":5.0},"provider":{"body":{"max_output_tokens":1024,"reasoning_effort":"low"},"headers":{"x-mode":"fast"}}}}}}}}}"#;
        let parsed = parse_snapshot_str(json).expect("parse must succeed");

        // Base entry still present.
        assert!(
            parsed.models.contains_key("myprov/base-model"),
            "base model must remain"
        );

        // Synthesized mode entry present under the "-fast" key.
        let synth = parsed
            .models
            .get("myprov/base-model-fast")
            .expect("experimental mode must be expanded into a listed model");
        assert_eq!(&*synth.info.id, "base-model-fast");
        assert_eq!(synth.info.name, "Base Model Fast");

        // Cost is the mode's cost deep-merged over the base: output overridden,
        // input retained from the base.
        assert_eq!(synth.cost_input, Some(1.0));
        assert_eq!(synth.cost_output, Some(5.0));

        // provider.body keys are snake_case → camelCase converted.
        assert_eq!(
            synth.options.get("maxOutputTokens").and_then(|v| v.as_u64()),
            Some(1024)
        );
        assert_eq!(
            synth.options.get("reasoningEffort").and_then(|v| v.as_str()),
            Some("low")
        );

        // provider.headers carried through verbatim.
        assert_eq!(
            synth.headers.get("x-mode").map(String::as_str),
            Some("fast")
        );

        // The synthesized entry must not carry modes to re-expand.
        assert!(synth.experimental_modes.is_empty());
    }

    #[test]
    fn snake_to_camel_matches_opencode() {
        assert_eq!(snake_to_camel("max_output_tokens"), "maxOutputTokens");
        assert_eq!(snake_to_camel("reasoning_effort"), "reasoningEffort");
        // Trailing / doubled underscores and non-alpha follows are preserved.
        assert_eq!(snake_to_camel("already"), "already");
        assert_eq!(snake_to_camel("trailing_"), "trailing_");
        assert_eq!(snake_to_camel("a_1b"), "a_1b");
    }

    #[test]
    fn hardcoded_list_providers_surface_full_catalog() {
        // cohere/azure/amazon-bedrock/minimax previously overwrote the catalog
        // via a tiny hardcoded discover_models() (2/4/3/1 models). The catalog
        // itself carries the full models.dev list for each; the provider fix
        // stops the overwrite so these surface in the picker.
        let reg = ModelRegistry::new();
        assert!(
            reg.list_by_provider("cohere").len() >= 10,
            "cohere catalog must surface (~12), not the old hardcoded 2"
        );
        assert!(
            reg.list_by_provider("azure").len() >= 50,
            "azure catalog must surface (~108), not the old hardcoded 4"
        );
        assert!(
            reg.list_by_provider("amazon-bedrock").len() >= 50,
            "bedrock catalog must surface (~85), not the old hardcoded 3"
        );
        assert!(
            reg.list_by_provider("minimax").len() >= 5,
            "minimax catalog must surface (~6), not the old hardcoded 1"
        );
    }

    #[test]
    fn minimax_targets_preserve_current_metadata() {
        let reg = ModelRegistry::new();

        for (provider_id, expected_api) in [
            ("minimax", "https://api.minimax.io/anthropic"),
            ("minimax-cn", "https://api.minimaxi.com/anthropic"),
            ("minimax-coding-plan", "https://api.minimax.io/anthropic"),
            (
                "minimax-cn-coding-plan",
                "https://api.minimaxi.com/anthropic",
            ),
        ] {
            assert_eq!(
                reg.provider(provider_id).and_then(|provider| provider.api.as_deref()),
                Some(expected_api),
                "{provider_id} must expose the Anthropic base URL without /v1"
            );
        }

        let m3 = reg
            .get("minimax", "MiniMax-M3")
            .expect("MiniMax-M3 must be bundled");
        assert_eq!(m3.info.context_window, 1_000_000);
        assert!(m3.reasoning);
        assert!(m3.tool_calling);
        assert_eq!(
            m3.modalities_input,
            vec![Modality::Text, Modality::Image, Modality::Video]
        );
        assert_eq!(m3.cost_input, Some(0.3));
        assert_eq!(m3.cost_output, Some(1.2));
        assert_eq!(m3.cost_cache_read, Some(0.06));

        let long_context_tier = m3
            .cost
            .tiers
            .iter()
            .find(|tier| tier.tier.kind == "context" && tier.tier.size == Some(512_000))
            .expect("MiniMax-M3 must retain its 512k pricing tier");
        assert_eq!(long_context_tier.input, Some(0.6));
        assert_eq!(long_context_tier.output, Some(2.4));
        assert_eq!(long_context_tier.cache_read, Some(0.12));

        let m27 = reg
            .get("minimax", "MiniMax-M2.7")
            .expect("MiniMax-M2.7 must remain bundled");
        assert_eq!(m27.info.context_window, 204_800);
        assert!(m27.reasoning);
        assert_eq!(m27.modalities_input, vec![Modality::Text]);
        assert_eq!(m27.cost_input, Some(0.3));
        assert_eq!(m27.cost_output, Some(1.2));
        assert_eq!(m27.cost_cache_read, Some(0.06));
        assert_eq!(m27.cost_cache_write, Some(0.375));

        assert_eq!(
            reg.best_model_for_provider("minimax").as_deref(),
            Some("MiniMax-M3")
        );
    }

    // ---- opencode-style model surfacing (issue #180) ----------------------

    /// Merge a small JSON snapshot fragment into an existing registry, reusing
    /// the real parse + field-merge path so tests exercise production code.
    fn merge_json(reg: &mut ModelRegistry, json: &str) {
        let parsed = parse_snapshot_str(json).expect("fragment must parse");
        reg.merge_entries(parsed.models);
    }

    #[test]
    fn version_aware_id_compare() {
        use std::cmp::Ordering;
        // Newer point-release sorts first.
        assert_eq!(cmp_ids_newest_first("claude-opus-4-8", "claude-opus-4-7"), Ordering::Less);
        // Numeric, not lexical: 4-10 is newer than 4-9.
        assert_eq!(cmp_ids_newest_first("claude-opus-4-10", "claude-opus-4-9"), Ordering::Less);
        // A clean alias beats its date-pinned sibling.
        assert_eq!(
            cmp_ids_newest_first("claude-opus-4-5", "claude-opus-4-5-20251101"),
            Ordering::Less
        );
        // A date suffix must not masquerade as a huge minor version.
        assert_eq!(
            cmp_ids_newest_first("claude-opus-4-8", "claude-opus-4-20250514"),
            Ordering::Less
        );
    }

    #[test]
    fn opus_4_8_surfaces_as_anthropic_default_and_in_list() {
        // Headline bug: the newest Opus in the catalog must be the default and
        // must appear in the listing — driven purely by the snapshot.
        let reg = ModelRegistry::new();
        assert_eq!(
            reg.best_model_for_provider("anthropic").as_deref(),
            Some("claude-opus-4-8"),
            "anthropic default must be the newest catalog Opus"
        );
        assert!(
            reg.list_by_provider("anthropic")
                .iter()
                .any(|e| &*e.info.id == "claude-opus-4-8"),
            "claude-opus-4-8 must be listed"
        );
    }

    #[test]
    fn newer_id_wins_without_a_release_date() {
        // A fake high-version Opus with NO release_date must still win — the
        // priority list + version-aware id compare governs the default, not date.
        let mut reg = ModelRegistry::new();
        merge_json(
            &mut reg,
            r#"{"anthropic":{"id":"anthropic","name":"Anthropic","env":[],"models":{"claude-opus-9-9":{"id":"claude-opus-9-9","name":"Claude Opus 9.9","limit":{"context":200000,"output":32000}}}}}"#,
        );
        assert!(
            reg.get("anthropic", "claude-opus-9-9").unwrap().release_date.is_none(),
            "fixture must have no release_date"
        );
        assert_eq!(
            reg.best_model_for_provider("anthropic").as_deref(),
            Some("claude-opus-9-9"),
        );
    }

    #[test]
    fn anthropic_default_stays_opus_even_if_sonnet_is_newer() {
        // Inject a far-future Sonnet; the default must remain an Opus because the
        // priority list ranks opus above sonnet regardless of release_date.
        let mut reg = ModelRegistry::new();
        merge_json(
            &mut reg,
            r#"{"anthropic":{"id":"anthropic","name":"Anthropic","env":[],"models":{"claude-sonnet-9-9":{"id":"claude-sonnet-9-9","name":"Claude Sonnet 9.9","release_date":"2099-01-01","limit":{"context":200000,"output":32000}}}}}"#,
        );
        let best = reg.best_model_for_provider("anthropic").expect("a default");
        assert!(best.contains("opus"), "expected an opus default, got {best}");
    }

    #[test]
    fn qwen_canonicalizes_to_alibaba_never_claude() {
        let reg = ModelRegistry::new();
        // Catalog reads under the runtime id "qwen" hit the "alibaba" snapshot key.
        assert!(
            !reg.list_by_provider("qwen").is_empty(),
            "qwen must resolve to the alibaba catalog"
        );
        let best = reg.best_model_for_provider("qwen").expect("a qwen default");
        assert!(
            best.contains("qwen") && !best.contains("claude"),
            "qwen default must be a qwen id, got {best}"
        );

        // The single resolver agrees when only the provider (no model) is set.
        let cfg = claurst_core::config::Config {
            provider: Some("qwen".to_string()),
            model: None,
            ..Default::default()
        };
        let resolved = effective_model_for_config(&cfg, &reg);
        assert!(
            !resolved.contains("claude"),
            "qwen must never resolve to a claude model, got {resolved}"
        );
    }

    #[test]
    fn refresh_field_merge_keeps_bundled_release_date() {
        // A refresh that drops release_date must not erase the bundled date,
        // while still applying the fields it does specify (e.g. the name).
        let mut reg = ModelRegistry::new();
        let before = reg
            .get("anthropic", "claude-opus-4-8")
            .and_then(|e| e.release_date.clone())
            .expect("bundled opus-4-8 has a date");
        merge_json(
            &mut reg,
            r#"{"anthropic":{"id":"anthropic","name":"Anthropic","env":[],"models":{"claude-opus-4-8":{"id":"claude-opus-4-8","name":"Claude Opus 4.8 REFRESHED","limit":{"context":200000,"output":32000}}}}}"#,
        );
        let after = reg.get("anthropic", "claude-opus-4-8").unwrap();
        assert_eq!(
            after.release_date.as_deref(),
            Some(before.as_str()),
            "an incoming None must not clobber the existing release_date"
        );
        assert_eq!(after.info.name, "Claude Opus 4.8 REFRESHED", "present fields still win");
    }

    /// Guardrail: the model-surfacing contract must not silently regress.
    ///   1. No provider may reintroduce `fn list_models` (the trait method is
    ///      `discover_models`, defaulting to empty for catalog providers).
    ///   2. Catalog-backed providers (anthropic/openai/google) must not hardcode
    ///      claude flagship ids — their list is the catalog projection. Scoped to
    ///      those files so deliberate copilot/bedrock/codex/free seeds elsewhere
    ///      are not false-flagged.
    #[test]
    fn guard_no_list_models_or_hardcoded_catalog_claude() {
        use std::fs;
        use std::path::Path;
        let providers = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/providers");

        for entry in fs::read_dir(&providers).expect("read providers dir") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = fs::read_to_string(&path).expect("read source");
            assert!(
                !src.contains("fn list_models"),
                "{path:?} still defines fn list_models — rename to discover_models"
            );
        }

        for name in ["anthropic.rs", "openai.rs", "google.rs"] {
            let src = fs::read_to_string(providers.join(name)).expect("read catalog provider");
            for fam in ["claude-opus", "claude-sonnet", "claude-haiku"] {
                let needle = format!("ModelId::new(\"{fam}");
                assert!(
                    !src.contains(&needle),
                    "{name} hardcodes a {fam} model id — catalog providers must project from the registry"
                );
            }
        }
    }

    // ---- model overrides (issue #309) --------------------------------------

    use claurst_core::config::ModelOverride;

    fn overrides(pairs: Vec<(&str, ModelOverride)>) -> HashMap<String, ModelOverride> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn override_patches_existing_catalog_entry() {
        let mut reg = ModelRegistry::new();
        // Pick a real catalog model and prove the override wins over models.dev.
        let model = reg
            .list_by_provider("anthropic")
            .iter()
            .find(|m| (*m.info.id).contains("opus"))
            .map(|m| m.info.id.to_string())
            .expect("anthropic must have an opus model in the bundled snapshot");
        let key = format!("anthropic/{model}");

        let original = reg.get("anthropic", &model).unwrap();
        assert_ne!(original.info.context_window, 12_345, "test value must differ");

        reg.apply_model_overrides(&overrides(vec![(
            key.as_str(),
            ModelOverride {
                context_window: Some(12_345),
                max_output_tokens: Some(6_789),
                name: Some("My Patched Opus".to_string()),
                ..Default::default()
            },
        )]));

        let patched = reg.get("anthropic", &model).unwrap();
        assert_eq!(patched.info.context_window, 12_345);
        assert_eq!(patched.info.max_output_tokens, 6_789);
        assert_eq!(patched.info.name, "My Patched Opus");
    }

    #[test]
    fn override_partial_leaves_other_fields_intact() {
        let mut reg = ModelRegistry::new();
        let model = reg
            .list_by_provider("anthropic")
            .iter()
            .find(|m| (*m.info.id).contains("opus"))
            .map(|m| m.info.id.to_string())
            .expect("anthropic opus model present");
        let original_max = reg.get("anthropic", &model).unwrap().info.max_output_tokens;
        let original_name = reg.get("anthropic", &model).unwrap().info.name.clone();

        // Only context_window is set; max_output_tokens and name must survive.
        reg.apply_model_overrides(&overrides(vec![(
            format!("anthropic/{model}").as_str(),
            ModelOverride { context_window: Some(500_000), ..Default::default() },
        )]));

        let patched = reg.get("anthropic", &model).unwrap();
        assert_eq!(patched.info.context_window, 500_000);
        assert_eq!(patched.info.max_output_tokens, original_max);
        assert_eq!(patched.info.name, original_name);
    }

    #[test]
    fn override_synthesizes_entry_for_unknown_alias() {
        let mut reg = ModelRegistry::new();
        // A self-hosted alias models.dev never heard of.
        assert!(reg.get("custom-openai", "my-local-llm").is_none());

        reg.apply_model_overrides(&overrides(vec![(
            "custom-openai/my-local-llm",
            ModelOverride {
                context_window: Some(32_768),
                max_output_tokens: Some(4_096),
                name: Some("My Local LLM".to_string()),
                status: Some("beta".to_string()),
                ..Default::default()
            },
        )]));

        let entry = reg
            .get("custom-openai", "my-local-llm")
            .expect("synthetic entry must be inserted for an unknown alias");
        assert_eq!(entry.info.context_window, 32_768);
        assert_eq!(entry.info.max_output_tokens, 4_096);
        assert_eq!(entry.info.name, "My Local LLM");
        assert_eq!(entry.status, ModelStatus::Beta);
        // And it surfaces in the provider listing the picker reads.
        assert!(reg
            .list_by_provider("custom-openai")
            .iter()
            .any(|m| &*m.info.id == "my-local-llm"));
    }

    #[test]
    fn override_empty_or_malformed_keys_are_ignored() {
        let mut reg = ModelRegistry::new();
        let before = reg.len();
        reg.apply_model_overrides(&overrides(vec![
            // Empty override — nothing to apply.
            ("custom-openai/noop", ModelOverride::default()),
            // No provider/model separator — cannot be placed.
            ("bare-id", ModelOverride { context_window: Some(1_000), ..Default::default() }),
            // Empty provider / model halves.
            ("/model", ModelOverride { context_window: Some(1_000), ..Default::default() }),
            ("provider/", ModelOverride { context_window: Some(1_000), ..Default::default() }),
        ]));
        assert_eq!(reg.len(), before, "malformed/empty overrides must not add entries");
        assert!(reg.get("custom-openai", "noop").is_none());
    }
}
