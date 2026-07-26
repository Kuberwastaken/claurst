//! Model-adaptive effort ladders — the single, registry-aware surface.
//!
//! Both effort-selection surfaces in the app funnel through here:
//!   * the `/effort` command (`supported_efforts`, via `app.rs`), and
//!   * the `/model` picker's inline ←/→ effort selector (`variant_ladder`, via
//!     `model_picker.rs`).
//!
//! The per-model reasoning tiers are computed by [`crate::variants`], a faithful
//! branch-for-branch port of opencode's `ProviderTransform.variants()`. This
//! module resolves the fields opencode keys off — the provider's npm SDK
//! package, the model's catalog id, its `release_date`, its provider id, and its
//! `reasoning` capability — from the [`ModelRegistry`] and hands them to the
//! port. `Ultracode`, claurst's always-last workflow overlay (not an opencode
//! tier), is appended by [`supported_efforts`].

use crate::model_registry::canonical_snapshot_key;
use crate::ModelRegistry;
use claurst_core::effort::EffortLevel;

/// opencode's ultimate fallback npm when neither the model nor its provider
/// declares one (`... ?? "@ai-sdk/openai-compatible"`).
const DEFAULT_NPM: &str = "@ai-sdk/openai-compatible";

/// The reasoning-effort tiers a model exposes, ascending (weakest→strongest),
/// exactly as opencode's `variants()` would — with **no** `Ultracode` appended
/// and **no** claurst base augmentation.
///
/// This is the raw ladder the /model picker cycles through: empty for a
/// non-reasoning model (no effort selector), otherwise the model's real tiers.
///
/// `registry` supplies the authoritative npm / release_date / reasoning fields.
/// When it is absent (or the model is unknown), the fields are inferred:
/// `reasoning` from a model-name heuristic, `npm` from the provider entry (or
/// the opencode default), and `release_date` as empty (which disables the
/// date-gated OpenAI tiers).
pub fn variant_ladder(
    provider: &str,
    model: &str,
    registry: Option<&ModelRegistry>,
) -> Vec<EffortLevel> {
    let facts = ModelFacts::resolve(provider, model, registry);
    crate::variants::variant_efforts(
        &facts.npm,
        &facts.id,
        &facts.release_date,
        &facts.provider_id,
        facts.reasoning,
    )
}

/// The effort levels selectable for `provider`/`model`, ascending, with
/// [`EffortLevel::Ultracode`] always last.
///
/// - **Models with reasoning variants** get exactly opencode's `variants()`
///   tiers for that model (e.g. an Opus 4.7+ gets `Low, Medium, High, XHigh,
///   Max`; a plain older `gpt-5` gets `Minimal, Low, Medium, High`; `gpt-5-pro`
///   gets just `High`).
/// - **Non-reasoning models** (opencode `variants()` == `{}`) still get claurst's
///   temperature-differentiated base ladder `Low, Medium, High`, because claurst
///   also uses the effort level to drive temperature / prompt shaping.
/// - `Ultracode` is appended in every case — it is a workflow overlay on top of
///   whatever top reasoning the model can do.
pub fn supported_efforts(
    provider: &str,
    model: &str,
    registry: Option<&ModelRegistry>,
) -> Vec<EffortLevel> {
    let ladder = variant_ladder(provider, model, registry);
    let mut levels = if ladder.is_empty() {
        // No native reasoning tiers — offer the base temperature ladder.
        vec![EffortLevel::Low, EffortLevel::Medium, EffortLevel::High]
    } else {
        ladder
    };
    // Ultracode is claurst-only and always the top rung.
    levels.push(EffortLevel::Ultracode);
    levels
}

/// Whether `provider`/`model` is a reasoning/thinking model.
///
/// Prefers the registry `reasoning` flag when the model is known; otherwise uses
/// a model-name heuristic.
pub fn model_is_reasoning(provider: &str, model: &str, registry: Option<&ModelRegistry>) -> bool {
    ModelFacts::resolve(provider, model, registry).reasoning
}

// ---------------------------------------------------------------------------
// Field resolution — the (npm, id, release_date, provider_id, reasoning) tuple
// opencode's variants() keys off.
// ---------------------------------------------------------------------------

struct ModelFacts {
    /// `model.provider?.npm ?? provider.npm ?? "@ai-sdk/openai-compatible"`.
    npm: String,
    /// `model.id` (== `model.api.id` for models.dev models).
    id: String,
    /// `model.release_date ?? ""`.
    release_date: String,
    /// `model.providerID`.
    provider_id: String,
    /// `model.capabilities.reasoning`.
    reasoning: bool,
}

impl ModelFacts {
    fn resolve(provider: &str, model: &str, registry: Option<&ModelRegistry>) -> Self {
        let bare = bare_model(model);

        // The provider whose catalog entry we actually matched. Usually the
        // connected provider, but the fallback below may resolve a model to a
        // different provider that truly owns it in the catalog.
        let mut matched_provider = provider.to_string();

        // Try the id as given, then its bare (prefix-stripped) form, under the
        // connected provider.
        let mut entry =
            registry.and_then(|r| r.get(provider, model).or_else(|| r.get(provider, bare)));

        // Fallback: some providers surface a model whose canonical catalog entry
        // lives under a DIFFERENT provider key. The clearest case is the
        // `openai-codex` (ChatGPT-subscription) endpoint, which serves OpenAI's
        // `gpt-5.5` — but the catalog lists that model only under `openai`, and
        // there is no `codex`/`openai-codex` provider at all. A direct
        // `get("openai-codex", "gpt-5.5")` therefore misses, so we resolve the
        // model's canonical entry by its bare id. Without this the model's
        // `release_date` was lost, the date-gated `none`/`xhigh` reasoning tiers
        // silently vanished, and the ladder collapsed to Low/Medium/High.
        if entry.is_none() {
            if let Some(r) = registry {
                if let Some(canon) = r.find_provider_for_model(bare) {
                    let canon_str = canon.to_string();
                    if let Some(e) = r.get(&canon_str, bare) {
                        matched_provider = canon_str;
                        entry = Some(e);
                    }
                }
            }
        }

        // The provider-level npm (from the matched provider entry), used when the
        // model has no per-model override — resolves even for a model not in the
        // catalog, as long as the provider is known.
        let provider_npm = registry
            .and_then(|r| r.provider(canonical_snapshot_key(&matched_provider)))
            .and_then(|p| p.npm.clone());

        if let Some(e) = entry {
            let npm = e
                .provider_override
                .as_ref()
                .and_then(|o| o.npm.clone())
                .or(provider_npm)
                .unwrap_or_else(|| DEFAULT_NPM.to_string());
            Self {
                npm,
                id: e.info.id.to_string(),
                release_date: e.release_date.clone().unwrap_or_default(),
                provider_id: e.info.provider_id.to_string(),
                reasoning: e.reasoning,
            }
        } else {
            Self {
                npm: provider_npm.unwrap_or_else(|| DEFAULT_NPM.to_string()),
                id: model.to_string(),
                release_date: String::new(),
                provider_id: provider.to_string(),
                reasoning: reasoning_heuristic(bare),
            }
        }
    }
}

/// Name-based reasoning heuristic used when the registry has no entry for a
/// model (self-hosted / freshly-discovered endpoints).
fn reasoning_heuristic(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    // Anthropic extended-thinking families.
    let anthropic_thinking = m.starts_with("claude-3-7")
        || m.starts_with("claude-opus-4")
        || m.starts_with("claude-sonnet-4")
        || m.starts_with("claude-haiku-4");
    // OpenAI reasoning family.
    let openai_reasoning = (m.starts_with("gpt-5") && !m.contains("-chat"))
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4");
    // Gemini thinking models (2.5 and 3.x).
    let gemini_thinking =
        m.contains("gemini") && (m.contains("2.5") || m.contains("gemini-3") || m.contains("-3-"));
    anthropic_thinking || openai_reasoning || gemini_thinking
}

/// Strip a leading `provider/` (or nested `namespace/`) prefix, returning the
/// final path segment used for name-based heuristics.
fn bare_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_ultracode_last(levels: &[EffortLevel]) {
        assert_eq!(
            levels.last(),
            Some(&EffortLevel::Ultracode),
            "ultracode must always be last: {levels:?}"
        );
        assert_eq!(
            levels.iter().filter(|l| l.is_ultracode()).count(),
            1,
            "ultracode must appear exactly once"
        );
    }

    // The registry-backed path (bundled snapshot) must match opencode's
    // variants() for representative models.
    #[test]
    fn registry_ladders_match_opencode() {
        use EffortLevel::*;
        let reg = ModelRegistry::new();

        // Opus 4.8 (adaptive, xhigh+max) + ultracode.
        assert_eq!(
            supported_efforts("anthropic", "claude-opus-4-8", Some(&reg)),
            vec![Low, Medium, High, XHigh, Max, Ultracode]
        );
        // Sonnet 4.6: low/medium/high/max.
        assert_eq!(
            supported_efforts("anthropic", "claude-sonnet-4-6", Some(&reg)),
            vec![Low, Medium, High, Max, Ultracode]
        );
        // Modern gpt-5.5: none + xhigh.
        assert_eq!(
            supported_efforts("openai", "gpt-5.5", Some(&reg)),
            vec![None, Low, Medium, High, XHigh, Ultracode]
        );
        // gemini-2.5-pro: high/max.
        assert_eq!(
            supported_efforts("google", "gemini-2.5-pro", Some(&reg)),
            vec![High, Max, Ultracode]
        );
    }

    // Regression: a model connected under a provider ALIAS that isn't a catalog
    // key (the `openai-codex` ChatGPT endpoint serves OpenAI's `gpt-5.5`) must
    // still resolve the model's real catalog facts — otherwise its release_date
    // is lost and the date-gated `none`/`xhigh` tiers collapse to Low/Medium/High.
    #[test]
    fn codex_provider_alias_resolves_full_openai_ladder() {
        use EffortLevel::*;
        let reg = ModelRegistry::new();
        // openai-codex is not a catalog provider; gpt-5.5 lives under openai.
        assert_eq!(
            supported_efforts("openai-codex", "gpt-5.5", Some(&reg)),
            vec![None, Low, Medium, High, XHigh, Ultracode],
            "codex-connected gpt-5.5 must expose the same tiers as native openai"
        );
        // Prefixed form (openai-codex/gpt-5.5) must resolve identically.
        assert_eq!(
            supported_efforts("openai-codex", "openai-codex/gpt-5.5", Some(&reg)),
            vec![None, Low, Medium, High, XHigh, Ultracode]
        );
        // The picker path (provider inferred from a prefixed id) too.
        assert_eq!(
            variant_ladder("openai-codex", "gpt-5.5", Some(&reg)),
            vec![None, Low, Medium, High, XHigh]
        );
    }

    #[test]
    fn non_reasoning_model_gets_base_ladder() {
        let reg = ModelRegistry::new();
        // gpt-4o has no reasoning variants → base low/medium/high + ultracode.
        let levels = supported_efforts("openai", "gpt-4o", Some(&reg));
        assert_eq!(
            levels,
            vec![
                EffortLevel::Low,
                EffortLevel::Medium,
                EffortLevel::High,
                EffortLevel::Ultracode,
            ]
        );
        assert!(!levels.contains(&EffortLevel::XHigh));
        assert_ultracode_last(&levels);
        // The raw ladder (picker) is empty → no inline effort selector.
        assert!(variant_ladder("openai", "gpt-4o", Some(&reg)).is_empty());
    }

    #[test]
    fn variant_ladder_omits_ultracode() {
        let reg = ModelRegistry::new();
        let raw = variant_ladder("anthropic", "claude-opus-4-8", Some(&reg));
        assert!(!raw.contains(&EffortLevel::Ultracode));
        assert_eq!(
            raw,
            vec![
                EffortLevel::Low,
                EffortLevel::Medium,
                EffortLevel::High,
                EffortLevel::XHigh,
                EffortLevel::Max
            ]
        );
    }

    #[test]
    fn ultracode_is_always_last_across_model_shapes() {
        let reg = ModelRegistry::new();
        for (p, m) in [
            ("anthropic", "claude-opus-4-8"),
            ("anthropic", "claude-haiku-4-5"),
            ("openai", "gpt-5.5"),
            ("openai", "gpt-4o"),
            ("google", "gemini-2.5-pro"),
            ("some-self-hosted", "mystery-model-1"),
        ] {
            assert_ultracode_last(&supported_efforts(p, m, Some(&reg)));
        }
    }

    #[test]
    fn no_registry_falls_back_to_name_heuristics() {
        // Without a registry we can't know npm/release_date, but the name
        // heuristic still classifies reasoning vs not, and the provider default
        // npm path yields a sensible ladder.
        assert!(model_is_reasoning("anthropic", "claude-opus-4-8", None));
        assert!(!model_is_reasoning("openai", "gpt-4o", None));
        // A plain unknown model gets the base ladder + ultracode.
        let levels = supported_efforts("some-self-hosted", "mystery-model-1", None);
        assert_eq!(levels.last(), Some(&EffortLevel::Ultracode));
    }

    #[test]
    fn registry_reasoning_flag_drives_ladder() {
        // A model whose *name* looks non-reasoning but whose registry entry says
        // reasoning=true still gets a non-empty raw ladder.
        let mut registry = ModelRegistry::new();
        let json = r#"{"acme":{"id":"acme","name":"Acme","npm":"@ai-sdk/openai-compatible","models":{"reasoner-x":{"id":"reasoner-x","name":"Reasoner X","reasoning":true,"limit":{"context":200000,"output":64000}},"plain-y":{"id":"plain-y","name":"Plain Y","reasoning":false,"limit":{"context":128000,"output":32000}}}}}"#;
        let path = std::env::temp_dir()
            .join(format!("claurst_effort_support_{}.json", std::process::id()));
        std::fs::write(&path, json).expect("write temp catalog");
        registry.load_cache(&path);
        let _ = std::fs::remove_file(&path);

        // reasoning=true → openai-compatible OPENAI_EFFORTS.
        let reasoner = variant_ladder("acme", "reasoner-x", Some(&registry));
        assert!(!reasoner.is_empty(), "reasoning=true must yield a ladder: {reasoner:?}");
        // reasoning=false → empty raw ladder, base ladder from supported_efforts.
        assert!(variant_ladder("acme", "plain-y", Some(&registry)).is_empty());
        let plain = supported_efforts("acme", "plain-y", Some(&registry));
        assert_eq!(
            plain,
            vec![
                EffortLevel::Low,
                EffortLevel::Medium,
                EffortLevel::High,
                EffortLevel::Ultracode,
            ]
        );
    }
}
