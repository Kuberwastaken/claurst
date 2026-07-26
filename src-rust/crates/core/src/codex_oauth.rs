//! OpenAI Codex OAuth configuration and constants.
//!

/// OpenAI Codex OAuth client ID (shared with the OpenCode ecosystem).
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// OpenAI OAuth issuer base URL.
pub const CODEX_ISSUER: &str = "https://auth.openai.com";

/// OpenAI OAuth authorization endpoint
pub const CODEX_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";

/// OpenAI OAuth token endpoint
pub const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Codex Responses API endpoint (used for inference after login)
pub const CODEX_API_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Local redirect URI for OAuth callback
pub const CODEX_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// OAuth callback port
pub const CODEX_OAUTH_PORT: u16 = 1455;

/// OAuth scopes requested from OpenAI
pub const CODEX_SCOPES: &str = "openid profile email offline_access";

/// Curated Codex models — the static fallback used only when the models.dev
/// `openai` catalog is unavailable. The live list is derived by filtering that
/// catalog with [`codex_model_allowed`] (opencode's exact rule), so this list
/// must mirror what that filter yields from a current snapshot.
pub const CODEX_MODELS: &[(&str, &str)] = &[
    ("gpt-5.5", "GPT-5.5 (default)"),
    ("gpt-5.4", "GPT-5.4"),
    ("gpt-5.4-mini", "GPT-5.4 mini"),
    ("gpt-5.3-codex-spark", "GPT-5.3 Codex Spark"),
];

/// Default Codex model to use
pub const DEFAULT_CODEX_MODEL: &str = "gpt-5.5";

/// Models always offered to a ChatGPT-authenticated Codex session, regardless
/// of the version heuristic. Mirrors opencode's `ALLOWED_MODELS`.
pub const CODEX_ALLOWED_MODELS: &[&str] =
    &["gpt-5.5", "gpt-5.3-codex-spark", "gpt-5.4", "gpt-5.4-mini"];

/// Models explicitly withheld from Codex even though the version heuristic
/// would otherwise admit them. Mirrors opencode's `DISALLOWED_MODELS`.
pub const CODEX_DISALLOWED_MODELS: &[&str] = &["gpt-5.5-pro"];

/// Whether a models.dev `openai` model id is available to a ChatGPT-auth Codex
/// session. This is opencode's exact rule (see
/// `packages/opencode/src/plugin/openai/codex.ts`):
///
///   1. explicit allow-list wins,
///   2. then the explicit deny-list,
///   3. otherwise keep `gpt-<major>.<minor>` models newer than 5.4 — so future
///      releases (gpt-5.6, gpt-6.0, …) appear automatically without a code bump.
pub fn codex_model_allowed(id: &str) -> bool {
    if CODEX_ALLOWED_MODELS.contains(&id) {
        return true;
    }
    if CODEX_DISALLOWED_MODELS.contains(&id) {
        return false;
    }
    codex_model_version(id).map(|v| v > 5.4).unwrap_or(false)
}

/// Parse the leading `gpt-<major>.<minor>` version out of a model id, matching
/// opencode's `/^gpt-(\d+\.\d+)/`. Returns `None` when the id doesn't start
/// with that shape (e.g. `gpt-4o`, `o3`, non-gpt ids).
fn codex_model_version(id: &str) -> Option<f64> {
    let rest = id.strip_prefix("gpt-")?;
    let mut chars = rest.char_indices();
    let mut seen_major = false;
    let mut seen_dot = false;
    let mut seen_minor = false;
    let mut end = 0usize;
    for (i, c) in chars.by_ref() {
        if c.is_ascii_digit() {
            if seen_dot {
                seen_minor = true;
            } else {
                seen_major = true;
            }
            end = i + c.len_utf8();
        } else if c == '.' && seen_major && !seen_dot {
            seen_dot = true;
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if seen_major && seen_dot && seen_minor {
        rest[..end].parse::<f64>().ok()
    } else {
        None
    }
}

/// Context-window limit override for a Codex model, mirroring opencode: every
/// `gpt-5.5*` model is pinned to the 400K/272K/128K Codex window; all others
/// keep whatever the models.dev catalog reports. Returns
/// `(context, input, output)`.
pub fn codex_limit_override(id: &str) -> Option<(u32, u32, u32)> {
    if id.contains("gpt-5.5") {
        Some((400_000, 272_000, 128_000))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codex_constants_not_empty() {
        assert!(!CODEX_CLIENT_ID.is_empty());
        assert!(!CODEX_AUTHORIZE_URL.is_empty());
        assert!(!CODEX_TOKEN_URL.is_empty());
        assert!(!CODEX_REDIRECT_URI.is_empty());
        assert!(!CODEX_SCOPES.is_empty());
        assert!(!CODEX_MODELS.is_empty());
        assert!(!DEFAULT_CODEX_MODEL.is_empty());
    }

    #[test]
    fn test_codex_models_contains_default() {
        let default_found = CODEX_MODELS
            .iter()
            .any(|(model, _)| model == &DEFAULT_CODEX_MODEL);
        assert!(
            default_found,
            "DEFAULT_CODEX_MODEL must be in CODEX_MODELS list"
        );
    }

    #[test]
    fn test_redirect_uri_is_localhost() {
        assert!(CODEX_REDIRECT_URI.contains("localhost:1455"));
    }

    #[test]
    fn test_codex_allow_list_matches_opencode() {
        // Explicit allow-list — always kept.
        for id in ["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex-spark"] {
            assert!(codex_model_allowed(id), "{id} should be allowed");
        }
        // Explicit deny — withheld even though 5.5 > 5.4.
        assert!(!codex_model_allowed("gpt-5.5-pro"), "gpt-5.5-pro is denied");
        // Version heuristic: keep only > 5.4.
        assert!(codex_model_allowed("gpt-5.6"), "future gpt-5.6 kept by heuristic");
        assert!(codex_model_allowed("gpt-6.0"), "future gpt-6.0 kept by heuristic");
        // Legacy / non-matching ids dropped.
        for id in [
            "gpt-5.4-nano", "gpt-5.4-pro", "gpt-5.2-codex", "gpt-5.1-codex",
            "gpt-5.2", "gpt-5", "gpt-4o", "o3", "gpt-5-codex",
        ] {
            assert!(!codex_model_allowed(id), "{id} should be filtered out");
        }
    }

    #[test]
    fn test_codex_default_is_5_5_and_allowed() {
        assert_eq!(DEFAULT_CODEX_MODEL, "gpt-5.5");
        assert!(codex_model_allowed(DEFAULT_CODEX_MODEL));
        assert!(CODEX_MODELS.iter().all(|(id, _)| codex_model_allowed(id)));
    }

    #[test]
    fn test_codex_limit_override_only_for_5_5() {
        assert_eq!(codex_limit_override("gpt-5.5"), Some((400_000, 272_000, 128_000)));
        assert_eq!(codex_limit_override("gpt-5.5-codex"), Some((400_000, 272_000, 128_000)));
        assert_eq!(codex_limit_override("gpt-5.4"), None);
        assert_eq!(codex_limit_override("gpt-5.4-mini"), None);
    }
}
