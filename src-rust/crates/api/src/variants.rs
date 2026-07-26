//! Faithful port of opencode's `ProviderTransform.variants()` effort ladder.
//!
//! This module is the single source of truth for *which reasoning-effort tiers a
//! model exposes*, ported branch-for-branch from
//! `opencode/packages/opencode/src/provider/transform.ts` (`variants()`,
//! `openaiReasoningEfforts`, `openaiCompatibleReasoningEfforts`, the anthropic /
//! google helpers, the glm-5.2 special-cases, and the OpenAI release-date gates).
//!
//! opencode's `variants()` returns a map of *variant name → request params*; the
//! variant names ARE the effort tiers ("none" / "minimal" / "low" / "medium" /
//! "high" / "xhigh" / "max"). Claurst only needs the ordered set of tiers (it maps
//! each tier to its own thinking-budget / reasoning-effort in
//! [`claurst_core::effort::EffortLevel`]), so this port extracts the ordered
//! *keys* of that map — weakest to strongest — and maps them onto `EffortLevel`.
//! The param *values* (thinking budgets, `reasoningConfig`, …) are intentionally
//! dropped: they are re-derived from `EffortLevel` at request-build time.
//!
//! Faithfulness notes:
//! - opencode keys `variants()` off `model.api.npm`, `model.api.id`,
//!   `model.id`, `model.release_date` and `model.providerID`. For models.dev-
//!   derived models `model.api.id === model.id` (see `fromModelsDevModel`), and
//!   models.dev ids are already lowercase, so this port uses a single lowercased
//!   id for both. `npm` is resolved exactly as opencode does
//!   (`model.provider?.npm ?? provider.npm ?? "@ai-sdk/openai-compatible"`) by
//!   the registry-aware caller ([`crate::effort_support`]).
//! - opencode has no `ultracode` tier; the caller appends claurst's always-last
//!   `Ultracode` rung on top of whatever this module returns.
//! - The minimax-m3 adaptive variant map is `{ none, thinking }`; claurst has no
//!   dedicated "thinking" rung, so `thinking` maps to the nearest rung (`High`).
//!   See the `// NOTE:` at [`effort_key_to_level`].

use claurst_core::effort::EffortLevel;
use once_cell::sync::Lazy;
use regex::Regex;

// ---------------------------------------------------------------------------
// Release-date gates (identical strings to opencode transform.ts)
// ---------------------------------------------------------------------------

/// OpenAI rolled out the `none` reasoning_effort tier on this date (Responses
/// API). Models released before it 400 on `reasoning_effort: "none"`, so it is
/// only exposed as a variant for models new enough to accept it.
/// (`OPENAI_NONE_EFFORT_RELEASE_DATE` in transform.ts.)
pub const OPENAI_NONE_EFFORT_RELEASE_DATE: &str = "2025-11-13";

/// OpenAI rolled out the `xhigh` reasoning_effort tier on this date. Same
/// reasoning. (`OPENAI_XHIGH_EFFORT_RELEASE_DATE` in transform.ts.)
pub const OPENAI_XHIGH_EFFORT_RELEASE_DATE: &str = "2025-12-04";

// ---------------------------------------------------------------------------
// Effort key tables (mirror the const arrays in transform.ts)
// ---------------------------------------------------------------------------

const WIDELY_SUPPORTED_EFFORTS: &[&str] = &["low", "medium", "high"];
const OPENAI_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];
const OPENAI_GPT5_1_EFFORTS: &[&str] = &["none", "low", "medium", "high"];
const OPENAI_GPT5_2_PLUS_EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh"];
const OPENAI_GPT5_PRO_EFFORTS: &[&str] = &["high"];
const OPENAI_GPT5_PRO_2_PLUS_EFFORTS: &[&str] = &["medium", "high", "xhigh"];
const OPENAI_GPT5_CHAT_EFFORTS: &[&str] = &["medium"];
const OPENAI_GPT5_CODEX_XHIGH_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];
const OPENAI_GPT5_CODEX_3_PLUS_EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh"];

// ---------------------------------------------------------------------------
// Regexes (mirror the GPT5_* / anthropic / o-series patterns in transform.ts)
// ---------------------------------------------------------------------------

// Matches members of the gpt-5 family across the id formats we encounter:
//   "gpt-5", "gpt-5-nano", "gpt-5.4", "openai/gpt-5.4-codex".
static GPT5_FAMILY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:^|/)gpt-5(?:[.-]|$)").unwrap());
static GPT5_VERSION_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|/)gpt-5[.-](\d+)(?:[.-]|$)").unwrap());
static GPT5_PRO_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:^|/)gpt-5[.-]?pro(?:[.-]|$)").unwrap());
static GPT5_VERSIONED_PRO_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|/)gpt-5[.-]\d+[.-]pro(?:[.-]|$)").unwrap());

// "opus-4.7" (Anthropic/Bedrock/Vertex) and "claude-4.7-opus" (SAP inverted).
static ANTHROPIC_OPUS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)opus-(\d+)[.-](\d+)(?:[.@-]|$)|claude-(\d+)[.-](\d+)-opus(?:[.@-]|$)").unwrap()
});
static ANTHROPIC_SONNET_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)sonnet-(\d+)(?:[.@-]|$)|claude-(\d+)-sonnet(?:[.@-]|$)").unwrap());

// SAP case: `/\bo[1-9]/.test(id)` — an OpenAI o-series id (o1..o9).
static O_SERIES_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bo[1-9]").unwrap());

// ---------------------------------------------------------------------------
// gpt-5 family helpers (transform.ts:545-598)
// ---------------------------------------------------------------------------

/// `Number(GPT5_VERSION_RE.exec(apiId)?.[1]) || undefined` — the major version
/// after `gpt-5.` / `gpt-5-`, or `None` (also `None` for a captured `0`, matching
/// JS `Number(..) || undefined`).
fn gpt5_version(id: &str) -> Option<u32> {
    let caps = GPT5_VERSION_RE.captures(id)?;
    let n: u32 = caps.get(1)?.as_str().parse().ok()?;
    if n == 0 { None } else { Some(n) }
}

fn versioned_gpt5_reasoning_efforts(id: &str) -> Option<&'static [&'static str]> {
    if GPT5_VERSIONED_PRO_RE.is_match(id) {
        return Some(OPENAI_GPT5_PRO_2_PLUS_EFFORTS);
    }
    match gpt5_version(id) {
        None => None,
        Some(1) => Some(OPENAI_GPT5_1_EFFORTS),
        Some(_) => Some(OPENAI_GPT5_2_PLUS_EFFORTS),
    }
}

fn gpt5_codex_reasoning_efforts(id: &str) -> Option<&'static [&'static str]> {
    if !GPT5_FAMILY_RE.is_match(id) || !id.contains("codex") {
        return None;
    }
    let version = gpt5_version(id);
    if matches!(version, Some(v) if v >= 3) {
        return Some(OPENAI_GPT5_CODEX_3_PLUS_EFFORTS);
    }
    if id.contains("codex-max") || matches!(version, Some(v) if v >= 2) {
        return Some(OPENAI_GPT5_CODEX_XHIGH_EFFORTS);
    }
    Some(WIDELY_SUPPORTED_EFFORTS)
}

/// `gpt5ChatReasoningEfforts` — returns `Some(&[])` (an *empty but handled*
/// ladder) for a version-less `gpt-5-chat`, matching opencode's early-return.
fn gpt5_chat_reasoning_efforts(id: &str) -> Option<Vec<&'static str>> {
    if !GPT5_FAMILY_RE.is_match(id) || !id.contains("-chat") {
        return None;
    }
    Some(match gpt5_version(id) {
        None => Vec::new(),
        Some(_) => OPENAI_GPT5_CHAT_EFFORTS.to_vec(),
    })
}

/// `openaiReasoningEfforts(apiId, releaseDate)` — the reasoning_effort tiers an
/// OpenAI (or OpenAI-compatible upstream) model exposes, weakest to strongest.
fn openai_reasoning_efforts(id: &str, release_date: &str) -> Vec<&'static str> {
    if id.contains("deep-research") {
        return vec!["medium"];
    }
    if let Some(chat) = gpt5_chat_reasoning_efforts(id) {
        return chat;
    }
    if GPT5_PRO_RE.is_match(id) {
        return OPENAI_GPT5_PRO_EFFORTS.to_vec();
    }
    if let Some(codex) = gpt5_codex_reasoning_efforts(id) {
        return codex.to_vec();
    }
    if let Some(versioned) = versioned_gpt5_reasoning_efforts(id) {
        return versioned.to_vec();
    }
    let mut efforts = WIDELY_SUPPORTED_EFFORTS.to_vec();
    if GPT5_FAMILY_RE.is_match(id) {
        efforts.insert(0, "minimal");
    }
    if release_date >= OPENAI_NONE_EFFORT_RELEASE_DATE {
        efforts.insert(0, "none");
    }
    if release_date >= OPENAI_XHIGH_EFFORT_RELEASE_DATE {
        efforts.push("xhigh");
    }
    efforts
}

fn openai_compatible_reasoning_efforts(id: &str) -> Vec<&'static str> {
    if let Some(chat) = gpt5_chat_reasoning_efforts(id) {
        return chat;
    }
    if GPT5_PRO_RE.is_match(id) {
        return OPENAI_GPT5_PRO_EFFORTS.to_vec();
    }
    gpt5_codex_reasoning_efforts(id)
        .or_else(|| versioned_gpt5_reasoning_efforts(id))
        .map(|s| s.to_vec())
        .unwrap_or_else(|| OPENAI_EFFORTS.to_vec())
}

// ---------------------------------------------------------------------------
// anthropic adaptive helpers (transform.ts:600-628)
// ---------------------------------------------------------------------------

fn anthropic_opus_47_or_later(id: &str) -> bool {
    match ANTHROPIC_OPUS_RE.captures(id) {
        Some(c) => {
            let major = c.get(1).or_else(|| c.get(3)).and_then(|m| m.as_str().parse::<u32>().ok());
            let minor = c.get(2).or_else(|| c.get(4)).and_then(|m| m.as_str().parse::<u32>().ok());
            match (major, minor) {
                (Some(major), Some(minor)) => major > 4 || (major == 4 && minor >= 7),
                _ => false,
            }
        }
        None => false,
    }
}

fn anthropic_sonnet_5_or_later(id: &str) -> bool {
    match ANTHROPIC_SONNET_RE.captures(id) {
        Some(c) => c
            .get(1)
            .or_else(|| c.get(2))
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .map(|n| n >= 5)
            .unwrap_or(false),
        None => false,
    }
}

fn anthropic_adaptive_efforts(id: &str) -> Option<Vec<&'static str>> {
    if anthropic_opus_47_or_later(id) || anthropic_sonnet_5_or_later(id) || id.contains("fable-5") {
        return Some(vec!["low", "medium", "high", "xhigh", "max"]);
    }
    const V46: &[&str] = &[
        "opus-4-6", "opus-4.6", "4-6-opus", "4.6-opus", "sonnet-4-6", "sonnet-4.6", "4-6-sonnet",
        "4.6-sonnet",
    ];
    if V46.iter().any(|v| id.contains(v)) {
        return Some(vec!["low", "medium", "high", "max"]);
    }
    None
}

// ---------------------------------------------------------------------------
// google thinking helpers (transform.ts:634-671)
// ---------------------------------------------------------------------------

fn google_thinking_level_efforts(id: &str) -> Vec<&'static str> {
    if !id.contains("gemini-3") {
        return vec!["low", "high"];
    }
    if id.contains("flash-image") {
        return vec!["minimal", "high"];
    }
    if id.contains("pro-image") {
        return vec!["high"];
    }
    if id.contains("flash") {
        return vec!["minimal", "low", "medium", "high"];
    }
    vec!["low", "medium", "high"]
}

/// Effort keys of `googleThinkingVariants(model)`: the 2.5 family exposes
/// `{ high, max }`; everything else maps each thinking-level effort.
fn google_thinking_variant_keys(id: &str) -> Vec<&'static str> {
    if id.contains("2.5") {
        return vec!["high", "max"];
    }
    google_thinking_level_efforts(id)
}

// ---------------------------------------------------------------------------
// key → EffortLevel
// ---------------------------------------------------------------------------

/// Map an opencode variant key onto claurst's [`EffortLevel`].
fn effort_key_to_level(key: &str) -> Option<EffortLevel> {
    Some(match key {
        "none" => EffortLevel::None,
        "minimal" => EffortLevel::Minimal,
        "low" => EffortLevel::Low,
        "medium" => EffortLevel::Medium,
        "high" => EffortLevel::High,
        "xhigh" => EffortLevel::XHigh,
        "max" => EffortLevel::Max,
        // NOTE: opencode's minimax-m3 adaptive map is `{ none, thinking }`.
        // Claurst has no dedicated "thinking" rung; map it to the nearest one.
        "thinking" => EffortLevel::High,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// The port: variants() effort keys → ordered EffortLevels
// ---------------------------------------------------------------------------

/// The ordered opencode `variants()` effort keys (weakest→strongest) for a
/// model, given the fields opencode keys off. `id` is the models.dev id (==
/// `model.api.id`); it is lowercased here.
pub(crate) fn variant_effort_keys(
    npm: &str,
    id: &str,
    release_date: &str,
    provider_id: &str,
    reasoning: bool,
) -> Vec<&'static str> {
    if !reasoning {
        return Vec::new();
    }
    let id = id.to_ascii_lowercase();
    let id = id.as_str();

    let glm52 = ["glm-5.2", "glm-5-2", "glm-5p2"].iter().any(|n| id.contains(n));

    if id.contains("minimax-m3") && (npm == "@ai-sdk/anthropic" || npm == "@ai-sdk/openai-compatible")
    {
        return vec!["none", "thinking"];
    }

    let adaptive = anthropic_adaptive_efforts(id);

    if glm52 && npm == "@openrouter/ai-sdk-provider" {
        // OpenRouter maps xhigh to GLM-5.2's native max effort.
        return vec!["high", "xhigh"];
    }
    if glm52 && npm == "@ai-sdk/openai-compatible" {
        return vec!["high", "max"];
    }
    if glm52 && npm == "@ai-sdk/anthropic" {
        return vec!["high", "max"];
    }

    if id.contains("deepseek-chat")
        || id.contains("deepseek-reasoner")
        || id.contains("deepseek-r1")
        || id.contains("deepseek-v3")
        || id.contains("minimax")
        || (id.contains("glm") && !glm52)
        || id.contains("kimi")
        || id.contains("k2p")
        || id.contains("qwen")
        || id.contains("big-pickle")
    {
        return Vec::new();
    }

    if id.contains("grok") && id.contains("grok-3-mini") {
        return vec!["low", "high"];
    }
    if id.contains("grok") {
        return Vec::new();
    }

    match npm {
        "@openrouter/ai-sdk-provider" => {
            if id.starts_with("openai/") || id.contains("gpt") {
                openai_compatible_reasoning_efforts(id)
            } else {
                WIDELY_SUPPORTED_EFFORTS.to_vec()
            }
        }
        "ai-gateway-provider" => {
            if id.starts_with("openai/") {
                openai_reasoning_efforts(id, release_date)
            } else {
                WIDELY_SUPPORTED_EFFORTS.to_vec()
            }
        }
        "@ai-sdk/gateway" => {
            if id.contains("anthropic") {
                adaptive.unwrap_or_else(|| vec!["high", "max"])
            } else if id.contains("google") {
                if id.contains("2.5") {
                    vec!["high", "max"]
                } else {
                    vec!["low", "high"]
                }
            } else {
                openai_compatible_reasoning_efforts(id)
            }
        }
        "@ai-sdk/github-copilot" => {
            if id.contains("gemini") {
                // github copilot currently only returns thinking for gemini.
                Vec::new()
            } else if id.contains("claude") {
                WIDELY_SUPPORTED_EFFORTS.to_vec()
            } else if id.contains("5.1-codex-max") || id.contains("5.2") || id.contains("5.3") {
                let mut e = WIDELY_SUPPORTED_EFFORTS.to_vec();
                e.push("xhigh");
                e
            } else {
                let mut e = WIDELY_SUPPORTED_EFFORTS.to_vec();
                if id.contains("gpt-5") && release_date >= OPENAI_XHIGH_EFFORT_RELEASE_DATE {
                    e.push("xhigh");
                }
                e
            }
        }
        "@ai-sdk/cerebras"
        | "@ai-sdk/togetherai"
        | "@ai-sdk/xai"
        | "@ai-sdk/deepinfra"
        | "venice-ai-sdk-provider"
        | "@ai-sdk/openai-compatible" => {
            if id.contains("north-mini-code") {
                vec!["none", "high"]
            } else {
                let mut e = WIDELY_SUPPORTED_EFFORTS.to_vec();
                if id.contains("deepseek-v4") {
                    e.push("max");
                }
                e
            }
        }
        "@ai-sdk/azure" => {
            if id == "o1-mini" {
                Vec::new()
            } else {
                openai_reasoning_efforts(id, release_date)
            }
        }
        "@ai-sdk/amazon-bedrock/mantle" | "@ai-sdk/openai" => {
            openai_reasoning_efforts(id, release_date)
        }
        "@ai-sdk/anthropic" | "@ai-sdk/google-vertex/anthropic" => {
            if let Some(mut efforts) = adaptive {
                if provider_id == "github-copilot" {
                    if id.contains("opus-4.7") {
                        efforts = vec!["medium"];
                    }
                    // github-copilot currently supports low/medium/high only.
                    efforts.retain(|v| *v != "max" && *v != "xhigh");
                }
                efforts
            } else if ["opus-4-5", "opus-4.5"].iter().any(|v| id.contains(v)) {
                WIDELY_SUPPORTED_EFFORTS.to_vec()
            } else {
                vec!["high", "max"]
            }
        }
        "@ai-sdk/amazon-bedrock" => {
            if let Some(efforts) = adaptive {
                efforts
            } else if id.contains("anthropic") {
                vec!["high", "max"]
            } else {
                // Amazon Nova.
                WIDELY_SUPPORTED_EFFORTS.to_vec()
            }
        }
        "@ai-sdk/google-vertex" | "@ai-sdk/google" => google_thinking_variant_keys(id),
        "@ai-sdk/mistral" => {
            const MISTRAL_REASONING_IDS: &[&str] = &[
                "mistral-small-2603",
                "mistral-small-latest",
                "mistral-medium-3.5",
                "mistral-medium-2604",
            ];
            if !MISTRAL_REASONING_IDS.iter().any(|m| id.contains(m)) {
                Vec::new()
            } else {
                vec!["high"]
            }
        }
        "@ai-sdk/cohere" => Vec::new(),
        "@ai-sdk/groq" => vec!["none", "low", "medium", "high"],
        "@ai-sdk/perplexity" => Vec::new(),
        "@jerome-benoit/sap-ai-provider-v2" => {
            if id.contains("anthropic") {
                adaptive.unwrap_or_else(|| vec!["high", "max"])
            } else if id.contains("gemini") && id.contains("2.5") {
                google_thinking_variant_keys(id)
            } else if id.contains("gpt") || O_SERIES_RE.is_match(id) {
                openai_reasoning_efforts(id, release_date)
            } else {
                vec!["low", "medium", "high"]
            }
        }
        _ => Vec::new(),
    }
}

/// The ordered [`EffortLevel`]s a model's opencode `variants()` map exposes,
/// weakest to strongest. Empty when the model has no reasoning variants (a
/// non-reasoning model, or a provider whose `variants()` returns `{}`).
///
/// `Ultracode` is NOT appended here — that is claurst's always-last workflow
/// overlay, added by [`crate::effort_support::supported_efforts`].
pub fn variant_efforts(
    npm: &str,
    id: &str,
    release_date: &str,
    provider_id: &str,
    reasoning: bool,
) -> Vec<EffortLevel> {
    variant_effort_keys(npm, id, release_date, provider_id, reasoning)
        .into_iter()
        .filter_map(effort_key_to_level)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(
        npm: &str,
        id: &str,
        rd: &str,
        provider_id: &str,
    ) -> Vec<EffortLevel> {
        variant_efforts(npm, id, rd, provider_id, true)
    }

    #[test]
    fn anthropic_native_ladders() {
        use EffortLevel::*;
        // Opus 4.7+ and Sonnet 5+ get the full adaptive ladder incl. xhigh + max.
        assert_eq!(
            keys("@ai-sdk/anthropic", "claude-opus-4-8", "2026-06-15", "anthropic"),
            vec![Low, Medium, High, XHigh, Max]
        );
        // 4.6-era opus/sonnet: low/medium/high/max (no xhigh).
        assert_eq!(
            keys("@ai-sdk/anthropic", "claude-opus-4-6", "2026-02-05", "anthropic"),
            vec![Low, Medium, High, Max]
        );
        assert_eq!(
            keys("@ai-sdk/anthropic", "claude-sonnet-4-6", "2026-02-17", "anthropic"),
            vec![Low, Medium, High, Max]
        );
        // Opus 4.5 is the WIDELY special-case: low/medium/high only.
        assert_eq!(
            keys("@ai-sdk/anthropic", "claude-opus-4-5", "2025-11-24", "anthropic"),
            vec![Low, Medium, High]
        );
        // Non-adaptive thinking model (haiku 4.5): budget-based high/max.
        assert_eq!(
            keys("@ai-sdk/anthropic", "claude-haiku-4-5", "2025-10-15", "anthropic"),
            vec![High, Max]
        );
    }

    #[test]
    fn minimax_thinking_modes_match_model_semantics() {
        use EffortLevel::*;

        assert_eq!(
            keys("@ai-sdk/anthropic", "MiniMax-M3", "2026-06-01", "minimax"),
            vec![None, High]
        );
        assert!(
            keys(
                "@ai-sdk/anthropic",
                "MiniMax-M2.7",
                "2026-03-18",
                "minimax"
            )
            .is_empty(),
            "always-on M2.7 thinking must not expose a disable toggle"
        );
    }

    #[test]
    fn openai_modern_gpt5_has_none_and_xhigh() {
        use EffortLevel::*;
        // gpt-5.5 (version 5 >= 2): none + xhigh via the versioned path, no minimal.
        assert_eq!(
            keys("@ai-sdk/openai", "gpt-5.5", "2026-04-23", "openai"),
            vec![None, Low, Medium, High, XHigh]
        );
        assert_eq!(
            keys("@ai-sdk/openai", "gpt-5.4", "2026-03-05", "openai"),
            vec![None, Low, Medium, High, XHigh]
        );
    }

    #[test]
    fn openai_older_gpt5_has_no_none_or_xhigh() {
        use EffortLevel::*;
        // Plain "gpt-5" released 2025-08 predates both gates: minimal + widely only.
        assert_eq!(
            keys("@ai-sdk/openai", "gpt-5", "2025-08-07", "openai"),
            vec![Minimal, Low, Medium, High]
        );
    }

    #[test]
    fn openai_chat_pro_codex_special_cases() {
        use EffortLevel::*;
        // gpt-5-chat-latest: handled but empty (no effort selector).
        assert!(keys("@ai-sdk/openai", "gpt-5-chat-latest", "2025-08-07", "openai").is_empty());
        // gpt-5-pro: "high" only.
        assert_eq!(keys("@ai-sdk/openai", "gpt-5-pro", "2025-10-06", "openai"), vec![High]);
        // gpt-5-codex (version-less): WIDELY.
        assert_eq!(
            keys("@ai-sdk/openai", "gpt-5-codex", "2025-09-15", "openai"),
            vec![Low, Medium, High]
        );
    }

    #[test]
    fn non_reasoning_model_has_no_variants() {
        assert!(variant_efforts("@ai-sdk/openai", "gpt-4o", "2024-05-13", "openai", false).is_empty());
    }

    #[test]
    fn openai_release_date_gate_boundaries() {
        use EffortLevel::*;
        // `none` gate is `>= 2025-11-13`.
        assert_eq!(
            keys("@ai-sdk/openai", "gpt-5", "2025-11-12", "openai"),
            vec![Minimal, Low, Medium, High]
        );
        assert_eq!(
            keys("@ai-sdk/openai", "gpt-5", "2025-11-13", "openai"),
            vec![None, Minimal, Low, Medium, High]
        );
        // `xhigh` gate is `>= 2025-12-04`.
        assert_eq!(
            keys("@ai-sdk/openai", "gpt-5", "2025-12-03", "openai"),
            vec![None, Minimal, Low, Medium, High]
        );
        assert_eq!(
            keys("@ai-sdk/openai", "gpt-5", "2025-12-04", "openai"),
            vec![None, Minimal, Low, Medium, High, XHigh]
        );
    }

    #[test]
    fn azure_matches_openai_and_o1_mini_is_empty() {
        use EffortLevel::*;
        assert_eq!(
            keys("@ai-sdk/azure", "gpt-5", "2025-08-07", "azure"),
            vec![Minimal, Low, Medium, High]
        );
        assert!(keys("@ai-sdk/azure", "o1-mini", "2024-09-12", "azure").is_empty());
    }

    #[test]
    fn bedrock_ladders() {
        use EffortLevel::*;
        // Bedrock anthropic 4.6: adaptive low/medium/high/max.
        assert_eq!(
            keys("@ai-sdk/amazon-bedrock", "anthropic.claude-opus-4-6-v1", "2026-02-05", "amazon-bedrock"),
            vec![Low, Medium, High, Max]
        );
        // Bedrock anthropic 4.5 (non-adaptive): budget high/max (NOT the native
        // opus-4.5 WIDELY special-case, which is anthropic-SDK only).
        assert_eq!(
            keys(
                "@ai-sdk/amazon-bedrock",
                "anthropic.claude-opus-4-5-20251101-v1:0",
                "2025-11-24",
                "amazon-bedrock"
            ),
            vec![High, Max]
        );
        // Amazon Nova (non-anthropic): WIDELY.
        assert_eq!(
            keys("@ai-sdk/amazon-bedrock", "amazon.nova-pro-v1:0", "2024-12-03", "amazon-bedrock"),
            vec![Low, Medium, High]
        );
    }

    #[test]
    fn google_ladders() {
        use EffortLevel::*;
        assert_eq!(
            keys("@ai-sdk/google", "gemini-2.5-pro", "2025-03-20", "google"),
            vec![High, Max]
        );
        assert_eq!(
            keys("@ai-sdk/google", "gemini-3-flash-preview", "2025-12-17", "google"),
            vec![Minimal, Low, Medium, High]
        );
        assert_eq!(
            keys("@ai-sdk/google", "gemini-3-pro-preview", "2025-11-18", "google"),
            vec![Low, Medium, High]
        );
    }

    #[test]
    fn glm_5_2_special_cases_per_npm() {
        use EffortLevel::*;
        // OpenRouter maps xhigh to glm-5.2's native max.
        assert_eq!(
            keys("@openrouter/ai-sdk-provider", "z-ai/glm-5.2", "2026-05-01", "openrouter"),
            vec![High, XHigh]
        );
        assert_eq!(
            keys("@ai-sdk/openai-compatible", "glm-5.2", "2026-05-01", "zai"),
            vec![High, Max]
        );
        assert_eq!(
            keys("@ai-sdk/anthropic", "glm-5.2", "2026-05-01", "zai"),
            vec![High, Max]
        );
        // Non-5.2 glm is excluded entirely (no reasoning variants).
        assert!(keys("@ai-sdk/openai-compatible", "glm-5.1", "2026-03-27", "zai").is_empty());
    }

    #[test]
    fn grok_and_groq() {
        use EffortLevel::*;
        assert_eq!(keys("@ai-sdk/xai", "grok-3-mini", "2025-04-01", "xai"), vec![Low, High]);
        // Other grok models expose no effort variants.
        assert!(keys("@ai-sdk/xai", "grok-4", "2025-07-01", "xai").is_empty());
        // groq prepends `none` to widely.
        assert_eq!(
            keys("@ai-sdk/groq", "openai/gpt-oss-120b", "2025-08-05", "groq"),
            vec![None, Low, Medium, High]
        );
    }

    #[test]
    fn openrouter_openai_uses_compatible_efforts() {
        use EffortLevel::*;
        // openrouter "openai/gpt-5" (version-less) → OPENAI_EFFORTS (all six).
        assert_eq!(
            keys("@openrouter/ai-sdk-provider", "openai/gpt-5", "2025-08-07", "openrouter"),
            vec![None, Minimal, Low, Medium, High, XHigh]
        );
        // A non-openai openrouter model → widely.
        assert_eq!(
            keys("@openrouter/ai-sdk-provider", "meta-llama/llama-3.1-70b", "2024-07-23", "openrouter"),
            vec![Low, Medium, High]
        );
    }

    #[test]
    fn cohere_and_perplexity_have_no_variants() {
        assert!(keys("@ai-sdk/cohere", "command-a-03-2025", "2025-03-01", "cohere").is_empty());
        assert!(keys("@ai-sdk/perplexity", "sonar-reasoning", "2025-01-01", "perplexity").is_empty());
    }
}
