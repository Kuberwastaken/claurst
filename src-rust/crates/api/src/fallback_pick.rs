// fallback_pick.rs: Rank alternative provider routes after a failure
// (ported from jcode's provider-core fallback_pick, adapted to claurst's
// provider set).
//
// Pure, side-effect free: given the credential-backed providers visible
// to this session and the route that just failed, return the alternative
// routes in best-first order so the interactive failover dialog can offer
// them.

/// One fallback candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackRoute {
    pub provider_id: String,
    /// Provider-qualified model, e.g. `"openai/gpt-5.4"` or a bare
    /// `"claude-sonnet-5"` for Anthropic.
    pub model: String,
    /// Rough ranking tier; lower is offered first.
    pub tier: u8,
}

/// Whether a provider id authenticates via an OAuth / subscription login
/// rather than a metered API key. Subscription logins are preferred
/// fallbacks: they are the most likely path to "just work" when an API
/// key is broken or unfunded.
fn provider_is_subscription(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "codex" | "openai-codex" | "github-copilot" | "anthropic-oauth" | "free"
    )
}

fn models_match(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Extra context about the failure that triggered the fallback search.
#[derive(Debug, Clone, Copy, Default)]
pub struct FallbackPickOptions<'a> {
    /// The failure was a credential/auth failure (expired OAuth session,
    /// invalid API key, failed token refresh, ...). Provider aliases that
    /// share the failed credential (e.g. `codex` / `openai-codex`) are then
    /// excluded too, since they fail identically.
    pub credential_failure: bool,
    /// Providers already marked unavailable earlier in this session.
    pub unavailable: &'a [&'a str],
}

/// Provider ids that name the same backend / credential slot.
fn same_credential_slot(a: &str, b: &str) -> bool {
    fn canonical(id: &str) -> &str {
        match id {
            "codex" | "openai-codex" => "codex",
            "lmstudio" | "lm-studio" => "lmstudio",
            "llamacpp" | "llama-cpp" | "llama-server" => "llamacpp",
            other => other,
        }
    }
    canonical(a) == canonical(b)
}

/// Rank the alternative routes for a failed `(provider, model)`.
///
/// Ordering, best first:
/// 1. Same model on another provider (rare, kept for parity).
/// 2. Other providers; subscription (OAuth/flat-rate) providers before
///    API-key providers, then in the order the caller listed them.
///
/// The failed provider is always excluded; on a credential failure its
/// aliases are excluded too.
pub fn ranked_fallback_routes(
    candidates: &[(&str, &str)], // (provider_id, provider_qualified_model)
    current_provider: &str,
    current_model: &str,
    options: FallbackPickOptions<'_>,
) -> Vec<FallbackRoute> {
    let mut out: Vec<FallbackRoute> = Vec::new();
    for &(provider_id, model) in candidates {
        if provider_id == current_provider {
            continue;
        }
        if options.credential_failure && same_credential_slot(provider_id, current_provider) {
            continue;
        }
        if options.unavailable.contains(&provider_id) {
            continue;
        }
        let same_model = models_match(model, current_model);
        let tier = if same_model {
            0
        } else if provider_is_subscription(provider_id) {
            1
        } else {
            2
        };
        out.push(FallbackRoute {
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            tier,
        });
    }
    out.sort_by_key(|route| {
        (
            route.tier,
            u8::from(!provider_is_subscription(&route.provider_id)),
        )
    });
    out
}

/// Whether `error` looks like a credential/auth failure for the active
/// route's provider (expired or revoked OAuth session, failed token
/// refresh, invalid API key). Used to widen the fallback exclusion: a
/// broken credential breaks every model behind it.
pub fn error_looks_like_credential_failure(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    let markers = [
        "token refresh failed",
        "refresh_token_invalidated",
        "refresh token was previously rejected",
        "re-authenticate",
        "session has ended",
        "authentication_error",
        "invalid_grant",
        "invalid x-api-key",
        "invalid api key",
        "incorrect api key",
        "api key not valid",
        "token expired",
        "unauthorized",
        "no refresh token",
        "credentials have been revoked",
        "please log in again",
        "run /login",
    ];
    markers.iter().any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> FallbackPickOptions<'static> {
        FallbackPickOptions::default()
    }

    #[test]
    fn excludes_current_provider() {
        let candidates = [
            ("anthropic", "claude-sonnet-5"),
            ("openai", "openai/gpt-5.4"),
        ];
        let routes = ranked_fallback_routes(&candidates, "anthropic", "claude-sonnet-5", options());
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].provider_id, "openai");
    }

    #[test]
    fn prefers_subscription_providers_over_api_key() {
        let candidates = [
            ("openai", "openai/gpt-5.4"),
            ("openai-codex", "openai-codex/gpt-5.4"),
            ("groq", "groq/llama-4"),
        ];
        let routes = ranked_fallback_routes(&candidates, "anthropic", "claude-sonnet-5", options());
        assert_eq!(routes[0].provider_id, "openai-codex");
        // Remaining routes keep the caller's order within their tier.
        assert_eq!(routes[1].provider_id, "openai");
        assert_eq!(routes[2].provider_id, "groq");
    }

    #[test]
    fn same_model_on_other_provider_ranks_first() {
        let candidates = [
            ("openrouter", "openrouter/claude-sonnet-5"),
            ("groq", "groq/llama-4"),
        ];
        let routes = ranked_fallback_routes(&candidates, "anthropic", "claude-sonnet-5", options());
        assert_eq!(routes[0].model, "openrouter/claude-sonnet-5");
    }

    #[test]
    fn skips_session_unavailable_providers() {
        let candidates = [
            ("openai", "openai/gpt-5.4"),
            ("google", "google/gemini-3-flash"),
        ];
        let opts = FallbackPickOptions {
            credential_failure: false,
            unavailable: &["openai"],
        };
        let routes = ranked_fallback_routes(&candidates, "anthropic", "claude-sonnet-5", opts);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].provider_id, "google");
    }

    #[test]
    fn credential_failure_excludes_alias_providers() {
        // A broken Codex (ChatGPT) login: the `codex` alias of the same
        // backend fails identically, so offer another provider instead.
        let candidates = [
            ("openai-codex", "openai-codex/gpt-5.4"),
            ("google", "google/gemini-3-flash"),
        ];
        let opts = FallbackPickOptions {
            credential_failure: true,
            unavailable: &[],
        };
        let routes = ranked_fallback_routes(&candidates, "codex", "codex/gpt-5.4", opts);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].provider_id, "google");
    }

    #[test]
    fn non_credential_failure_keeps_alias_providers() {
        // A quota failure on `codex` does not implicate the alias id when
        // the caller passes a different provider id for the same slot.
        let candidates = [("openai-codex", "openai-codex/gpt-5.4")];
        let routes = ranked_fallback_routes(&candidates, "anthropic", "claude-sonnet-5", options());
        assert_eq!(routes.len(), 1);
    }

    #[test]
    fn classifies_credential_failures() {
        assert!(error_looks_like_credential_failure(
            "Anthropic API error (401 Unauthorized): authentication_error invalid x-api-key"
        ));
        assert!(error_looks_like_credential_failure(
            "invalid_grant: refresh token invalid"
        ));
        assert!(error_looks_like_credential_failure(
            "Your session has ended. Please log in again."
        ));
        assert!(!error_looks_like_credential_failure(
            "429 rate limit exceeded, retry after 30s"
        ));
        assert!(!error_looks_like_credential_failure(
            "500 internal server error"
        ));
    }
}
