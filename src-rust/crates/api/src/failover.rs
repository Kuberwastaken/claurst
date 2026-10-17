// failover.rs: Interactive cross-provider failover prompt (ported from jcode).
//
// When a model request fails with an auth error, a quota/rate limit, or a
// 5xx after retries are exhausted, the query loop attaches a structured
// failover prompt to the surfaced error message. The TUI parses the JSON
// prefix and offers a one-keypress switch to a ranked alternative route.

use serde::{Deserialize, Serialize};

const PROVIDER_FAILOVER_PROMPT_PREFIX: &str = "[claurst-provider-failover]";

/// The interactive failover offer attached to a failover-class error.
///
/// Serialized as a single JSON line prefixed with
/// `[claurst-provider-failover]` on the first line of the error message.
/// Alternative routes are ordered best-first; the TUI lets the user pick
/// one with a digit key or arrows + Enter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderFailoverPrompt {
    pub from_provider: String,
    pub from_label: String,
    pub from_model: String,
    /// Alternative routes, best first. Each entry is
    /// `"provider_id/model_id"` (same form the model picker and config
    /// use), or a bare `"provider_id"` for provider-level switches.
    #[serde(default)]
    pub routes: Vec<String>,
    pub reason: String,
    /// Whether the failing provider should be marked unavailable for the
    /// rest of the session once the user switches away.
    #[serde(default)]
    pub mark_unavailable: bool,
}

impl ProviderFailoverPrompt {
    pub fn to_error_message(&self) -> String {
        let payload = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        format!(
            "{PROVIDER_FAILOVER_PROMPT_PREFIX}{payload}\n{} ({}) is unavailable: {}. \
             Alternative routes are available: switch from the failover dialog.",
            self.from_label, self.from_model, self.reason,
        )
    }
}

/// Parse a failover prompt from the first line of an error message.
pub fn parse_failover_prompt_message(message: &str) -> Option<ProviderFailoverPrompt> {
    let line = message.lines().next()?.trim();
    let json = line.strip_prefix(PROVIDER_FAILOVER_PROMPT_PREFIX)?;
    serde_json::from_str(json).ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailoverDecision {
    None,
    /// Offer alternatives; the failure is request-specific (e.g. context
    /// overflow on this model) so the provider itself stays usable.
    RetryNextProvider,
    /// Offer alternatives and mark the provider unavailable for the session.
    RetryAndMarkUnavailable,
}

impl FailoverDecision {
    pub fn should_failover(self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn should_mark_provider_unavailable(self) -> bool {
        matches!(self, Self::RetryAndMarkUnavailable)
    }
}

/// Whether `code` appears in `haystack` as a standalone number, not as a
/// substring of a longer digit run (e.g. "429" in "model 4291").
fn contains_independent_status_code(haystack: &str, code: &str) -> bool {
    let haystack_bytes = haystack.as_bytes();
    let code_len = code.len();

    haystack.match_indices(code).any(|(start, _)| {
        let before_ok = start == 0 || !haystack_bytes[start - 1].is_ascii_digit();
        let end = start + code_len;
        let after_ok = end == haystack_bytes.len() || !haystack_bytes[end].is_ascii_digit();
        before_ok && after_ok
    })
}

/// Classify an error message into a failover decision.
pub fn classify_failover_error_message(message: &str) -> FailoverDecision {
    let lower = message.to_ascii_lowercase();

    let request_size_or_context = [
        "context length",
        "context_length",
        "context window",
        "maximum context",
        "prompt is too long",
        "input is too long",
        "too many tokens",
        "token limit",
        "token_limit",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || contains_independent_status_code(&lower, "413");
    if request_size_or_context {
        return FailoverDecision::RetryNextProvider;
    }

    let rate_or_quota = [
        "rate limit",
        "rate-limited",
        "too many requests",
        "quota",
        "credit balance",
        "credits have run out",
        "insufficient credit",
        "billing",
        "payment required",
        "usage tier",
        "overloaded",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || contains_independent_status_code(&lower, "429")
        || contains_independent_status_code(&lower, "402");
    if rate_or_quota {
        return FailoverDecision::RetryAndMarkUnavailable;
    }

    let auth_or_access = [
        "access denied",
        "provider unavailable",
        "provider not available",
        "provider is unavailable",
        "provider currently unavailable",
        "provider not configured",
        "credentials are not configured",
        "no credentials",
        "token exchange failed",
        "refresh token was previously rejected",
        "authentication failed",
        "authentication_error",
        "unauthorized",
        "forbidden",
        "no api key",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || contains_independent_status_code(&lower, "401")
        || contains_independent_status_code(&lower, "403");
    if auth_or_access {
        return FailoverDecision::RetryAndMarkUnavailable;
    }

    FailoverDecision::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failover_prompt_roundtrips_from_error_message() {
        let prompt = ProviderFailoverPrompt {
            from_provider: "anthropic".to_string(),
            from_label: "Anthropic".to_string(),
            from_model: "claude-sonnet-5".to_string(),
            routes: vec![
                "openai/gpt-5.4".to_string(),
                "google/gemini-3-flash".to_string(),
            ],
            reason: "rate limit".to_string(),
            mark_unavailable: true,
        };

        let parsed = parse_failover_prompt_message(&prompt.to_error_message()).expect("prompt");
        assert_eq!(parsed, prompt);
    }

    #[test]
    fn parse_rejects_missing_prefix() {
        assert!(parse_failover_prompt_message("Error: 429 too many requests").is_none());
    }

    #[test]
    fn parse_rejects_invalid_json() {
        assert!(parse_failover_prompt_message("[claurst-provider-failover]not json").is_none());
    }

    #[test]
    fn classifier_marks_rate_limits_unavailable() {
        assert_eq!(
            classify_failover_error_message("429 Too Many Requests"),
            FailoverDecision::RetryAndMarkUnavailable
        );
        assert_eq!(
            classify_failover_error_message("Anthropic API error: overloaded_error"),
            FailoverDecision::RetryAndMarkUnavailable
        );
    }

    #[test]
    fn classifier_marks_auth_errors_unavailable() {
        for message in [
            "401 Unauthorized: invalid x-api-key",
            "403 Forbidden",
            "refresh token was previously rejected by the provider",
            "authentication_error: bad key",
            "No API key for provider 'openai' (model 'gpt-5').",
        ] {
            assert_eq!(
                classify_failover_error_message(message),
                FailoverDecision::RetryAndMarkUnavailable,
                "auth-class failure must mark unavailable: {message}"
            );
        }
    }

    #[test]
    fn classifier_retries_context_errors_without_marking_unavailable() {
        let decision = classify_failover_error_message("context length exceeded");
        assert_eq!(decision, FailoverDecision::RetryNextProvider);
        assert!(decision.should_failover());
        assert!(!decision.should_mark_provider_unavailable());
    }

    #[test]
    fn classifier_marks_quota_unavailable() {
        assert_eq!(
            classify_failover_error_message("402 Payment Required: insufficient credit"),
            FailoverDecision::RetryAndMarkUnavailable
        );
    }

    #[test]
    fn classifier_ignores_embedded_status_digits() {
        assert_eq!(
            classify_failover_error_message("model version 4130 failed"),
            FailoverDecision::None
        );
    }

    #[test]
    fn classifier_ignores_transient_errors() {
        for message in [
            "connection reset by peer",
            "500 internal server error",
            "request timed out",
        ] {
            assert_eq!(
                classify_failover_error_message(message),
                FailoverDecision::None,
                "transient failure must not trigger failover: {message}"
            );
        }
    }
}
