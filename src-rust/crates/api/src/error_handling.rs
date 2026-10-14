// error_handling.rs — Provider-aware error detection and retry utilities
// (Phase 6).
//
// Provides:
//  - `is_context_overflow`: checks a message string against 29+ known
//    context-window overflow error patterns from all major providers.
//  - `parse_error_response`: converts an HTTP status + body into the correct
//    `ProviderError` variant, including overflow detection and JSON code
//    extraction.
//
// Retry policy (classification, back-off, Retry-After) lives in `retry.rs`.

use claurst_core::provider_id::ProviderId;

use crate::provider_error::ProviderError;

// ---------------------------------------------------------------------------
// Overflow pattern table
// ---------------------------------------------------------------------------

/// 29+ context-overflow patterns that appear across all major providers.
static OVERFLOW_PATTERNS: &[&str] = &[
    "prompt is too long",
    "input is too long for requested model",
    "expected maxlength:",
    "exceeds the context window",
    "maximum context length",
    "input token count.*exceeds the maximum",
    "exceeds the maximum number of tokens allowed",
    "maximum prompt length is",
    "reduce the length of the messages",
    "maximum context length is.*tokens",
    "exceeds the limit of",
    "exceeds the available context size",
    "greater than the context length",
    "context window exceeds limit",
    "exceeded model token limit",
    "prompt too long",
    "too large for model with.*maximum context length",
    "model_context_window_exceeded",
    "context length is only.*tokens",
    "input length.*exceeds.*context length",
    "context_length_exceeded",
    "request entity too large",
    "too many tokens",
    "context.*length.*exceeded",
    "token.*limit.*exceeded",
    "prompt.*too.*long",
    "exceeds.*context.*size",
    "context.*window.*exceeded",
    "max.*tokens.*exceeded",
    "input.*too.*long",
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Returns `true` if `message` matches any known context-overflow pattern.
///
/// The comparison is case-insensitive.  Patterns are matched as substrings
/// (not full regexes) for performance — the pattern table is designed so that
/// simple substring matching is sufficient.
pub fn is_context_overflow(message: &str) -> bool {
    let lower = message.to_lowercase();
    // Rate limits ("too many tokens per minute") are transient, not overflow.
    if lower.contains("rate limit") || lower.contains("rate_limit") || lower.contains("per minute") {
        return false;
    }
    OVERFLOW_PATTERNS
        .iter()
        .any(|pattern| lower.contains(&pattern.to_lowercase()))
}

/// Convert an HTTP error response into the appropriate [`ProviderError`].
///
/// Tries JSON parsing, then falls back to the raw body.  Context overflow is
/// checked before the HTTP status code so that a 400 with an overflow message
/// is classified as [`ProviderError::ContextOverflow`] rather than
/// [`ProviderError::InvalidRequest`].
pub fn parse_error_response(status: u16, body: &str, provider: &ProviderId) -> ProviderError {
    let json: Option<serde_json::Value> = serde_json::from_str(body).ok();

    let message = if let Some(ref j) = json {
        extract_error_message(j)
    } else if body.trim_start().starts_with('<') {
        // HTML error page (Azure proxy, CDN, etc.)
        "Received HTML error page — check provider endpoint configuration".to_string()
    } else {
        body.to_string()
    };

    // Check for context overflow before all other classifications.
    if is_context_overflow(&message) || is_context_overflow(body) {
        return ProviderError::ContextOverflow {
            provider: provider.clone(),
            message,
            max_tokens: extract_token_limit(body),
        };
    }

    // Check for structured error codes returned by some providers.
    if let Some(ref j) = json {
        if let Some(code) = extract_error_code(j) {
            match code.as_str() {
                "context_length_exceeded" | "context_window_exceeded" => {
                    return ProviderError::ContextOverflow {
                        provider: provider.clone(),
                        message,
                        max_tokens: None,
                    };
                }
                "insufficient_quota" | "billing_not_active" => {
                    return ProviderError::QuotaExceeded {
                        provider: provider.clone(),
                        message,
                    };
                }
                "invalid_prompt" | "invalid_request_error" => {
                    return ProviderError::InvalidRequest {
                        provider: provider.clone(),
                        message,
                    };
                }
                "content_filter" | "content_policy_violation" => {
                    return ProviderError::ContentFiltered {
                        provider: provider.clone(),
                        message,
                    };
                }
                _ => {}
            }
        }
    }

    // Classify by HTTP status code.
    match status {
        401 | 403 => ProviderError::AuthFailed {
            provider: provider.clone(),
            message,
        },
        404 => ProviderError::ModelNotFound {
            provider: provider.clone(),
            model: "unknown".to_string(),
            suggestions: vec![],
        },
        // A 429 that names quota / billing exhaustion is not a transient
        // throttle; classify it so it is never retried.
        429 if crate::retry::is_fatal_message(&message) => ProviderError::QuotaExceeded {
            provider: provider.clone(),
            message,
        },
        429 => ProviderError::RateLimited {
            provider: provider.clone(),
            retry_after: None,
        },
        413 => ProviderError::ContextOverflow {
            provider: provider.clone(),
            message: "Request too large (413)".to_string(),
            max_tokens: None,
        },
        408 | 409 | 425 | 500..=599 => ProviderError::ServerError {
            provider: provider.clone(),
            status: Some(status),
            message,
            is_retryable: crate::retry::is_retryable_status(status),
            retry_after: None,
        },
        _ => ProviderError::Other {
            provider: provider.clone(),
            message,
            status: Some(status),
            body: Some(body.to_string()),
        },
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Walk several well-known JSON paths to find the human-readable error message.
fn extract_error_message(json: &serde_json::Value) -> String {
    // Ordered by prevalence across providers:
    //   OpenAI / Google: /error/message
    //   Anthropic:        /error/error/message
    //   Cohere / simple:  /message
    //   Some providers:   /detail
    let paths = [
        "/error/message",
        "/error/error/message",
        "/message",
        "/detail",
    ];
    for path in paths {
        if let Some(msg) = json.pointer(path).and_then(|v| v.as_str()) {
            return msg.to_string();
        }
    }
    json.to_string()
}

/// Extract a machine-readable error code from the JSON body, if present.
fn extract_error_code(json: &serde_json::Value) -> Option<String> {
    json.pointer("/error/code")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            json.pointer("/error/type")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}

/// Heuristically extract a token-limit number from error text.
///
/// Looks for an integer that is:
/// - between 1 000 and 10 000 000 (plausible token limit range), and
/// - adjacent to words like "token", "limit", "context", or "max".
fn extract_token_limit(text: &str) -> Option<u64> {
    let words: Vec<&str> = text.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        // Strip non-digit chars from both ends, then try to parse.
        let trimmed = word.trim_matches(|c: char| !c.is_ascii_digit());
        if let Ok(n) = trimmed.parse::<u64>() {
            if n > 1_000 && n < 10_000_000 {
                let start = i.saturating_sub(3);
                let context = words[start..i].join(" ").to_lowercase();
                if context.contains("token")
                    || context.contains("limit")
                    || context.contains("context")
                    || context.contains("max")
                {
                    return Some(n);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_context_overflow_basic() {
        assert!(is_context_overflow("prompt is too long for this model"));
        // Google Gemini.
        assert!(is_context_overflow(
            "The input token count (1200000) exceeds the maximum number of tokens allowed (1048576)."
        ));
        // OpenAI-compatible.
        assert!(is_context_overflow(
            "This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens."
        ));
        // A rate limit that mentions tokens is not an overflow.
        assert!(!is_context_overflow(
            "Rate limit reached: too many tokens per minute (TPM)"
        ));
        assert!(is_context_overflow("This exceeds the context window"));
        assert!(is_context_overflow("Maximum context length exceeded"));
        assert!(!is_context_overflow("something else went wrong"));
    }

    #[test]
    fn test_parse_error_response_overflow_413() {
        let pid = ProviderId::new("openai");
        let err = parse_error_response(413, "Request too large", &pid);
        assert!(matches!(err, ProviderError::ContextOverflow { .. }));
    }

    #[test]
    fn test_parse_error_response_auth() {
        let pid = ProviderId::new("anthropic");
        let err = parse_error_response(401, r#"{"error":{"message":"Invalid API key"}}"#, &pid);
        assert!(matches!(err, ProviderError::AuthFailed { .. }));
    }

    #[test]
    fn test_parse_error_response_rate_limit() {
        let pid = ProviderId::new("openai");
        let err = parse_error_response(429, "rate limited", &pid);
        assert!(matches!(err, ProviderError::RateLimited { .. }));
    }

    #[test]
    fn test_parse_error_response_quota_429_is_not_rate_limit() {
        let pid = ProviderId::new("openai");
        let err = parse_error_response(
            429,
            r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#,
            &pid,
        );
        assert!(matches!(err, ProviderError::QuotaExceeded { .. }));
    }

    #[test]
    fn test_parse_error_response_throttle_mentioning_billing_is_rate_limit() {
        let pid = ProviderId::new("google");
        let err = parse_error_response(
            429,
            r#"{"error":{"message":"Resource has been exhausted. Please check your plan and billing details."}}"#,
            &pid,
        );
        assert!(matches!(err, ProviderError::RateLimited { .. }), "{err:?}");
    }

    #[test]
    fn test_parse_error_response_transient_statuses() {
        let pid = ProviderId::new("openai");
        for status in [408, 409, 425, 500, 502, 503, 504] {
            let err = parse_error_response(status, "try later", &pid);
            assert!(
                matches!(err, ProviderError::ServerError { is_retryable: true, .. }),
                "{status} should map to a retryable ServerError, got {err:?}"
            );
        }
        let err = parse_error_response(501, "not implemented", &pid);
        assert!(matches!(err, ProviderError::ServerError { is_retryable: false, .. }));
    }
}
