// retry.rs — the single retry policy for model requests (#439).
//
// Every provider request goes through the same decision:
//  - classify the failure (`classify_provider_error`, `classify_claude_error`,
//    `classify_stream_error`) into `RetryDecision::Retry` or `RetryDecision::Fail`;
//  - pick the wait (`RetryConfig::delay_for`): the server's `retry-after-ms` /
//    `Retry-After` (seconds or HTTP-date) when present, otherwise exponential
//    back-off with jitter; every wait is capped at `MAX_RETRY_DELAY`.
//
// Retryable: HTTP 408/409/425/429/500/502/503/504 (plus Anthropic's 529 and the
// Cloudflare 520-524 range), connection and timeout failures, and streams that
// end before their completion marker. Never retried: 400/401/403/404/422, auth,
// quota/billing exhaustion, and invalid requests. The message classifier ports
// pi's transient / never-retry lists (`packages/ai/src/utils/retry.ts`).
//
// The query loop owns the retry loop for the main turn (so it can emit
// `QueryEvent::Retry` and cancel the wait); `AnthropicClient` applies the same
// policy internally for its auxiliary callers (compaction, summaries).

use std::time::{Duration, SystemTime};

use claurst_core::error::ClaudeError;
use reqwest::header::HeaderMap;

use crate::provider_error::ProviderError;

/// Upper bound on any single wait, whether computed or server-requested.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

/// Default number of retries after the first attempt.
pub const DEFAULT_MAX_RETRIES: u32 = 5;

// ---------------------------------------------------------------------------
// RetryConfig
// ---------------------------------------------------------------------------

/// Exponential back-off configuration for provider retries.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryConfig {
    /// Retries allowed after the first attempt (0 disables retrying).
    pub max_retries: u32,
    /// Base delay before the first retry.
    pub initial_delay: Duration,
    /// Upper bound on any wait; never above [`MAX_RETRY_DELAY`].
    pub max_delay: Duration,
    /// Multiplicative factor applied per attempt.
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            initial_delay: Duration::from_secs(1),
            max_delay: MAX_RETRY_DELAY,
            backoff_multiplier: 2.0,
        }
    }
}

impl RetryConfig {
    /// Default policy with a configured retry budget (`None` keeps the default).
    pub fn with_max_retries(max_retries: Option<u32>) -> Self {
        Self {
            max_retries: max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
            ..Self::default()
        }
    }

    fn cap(&self) -> Duration {
        self.max_delay.min(MAX_RETRY_DELAY)
    }

    /// Computed back-off for the 1-indexed retry `attempt`: exponential growth
    /// from `initial_delay`, minus up to 25 % jitter, capped.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(32) as i32;
        let base = self.initial_delay.as_secs_f64() * self.backoff_multiplier.powi(exponent);
        let jittered = base * (1.0 - 0.25 * time_jitter_f64());
        Duration::from_secs_f64(jittered.min(self.cap().as_secs_f64()).max(0.0))
    }

    /// Wait before the 1-indexed retry `attempt`: the server's hint when it gave
    /// one, otherwise [`backoff`](Self::backoff). Always capped.
    pub fn delay_for(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        match retry_after {
            Some(hint) => hint.min(self.cap()),
            None => self.backoff(attempt),
        }
    }
}

/// A value in `[0, 1)` derived from the clock's sub-second nanos, used as
/// retry jitter without a `rand` dependency.
fn time_jitter_f64() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos % 1000) / 1000.0
}

// ---------------------------------------------------------------------------
// Retry-After
// ---------------------------------------------------------------------------

/// Server-requested wait from a response's headers: `retry-after-ms`
/// (milliseconds), then `Retry-After` as seconds or an HTTP-date.
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    parse_retry_after(get("retry-after-ms"), get("retry-after"), SystemTime::now())
}

/// Header-value form of [`retry_after`], with an explicit clock for tests.
pub fn parse_retry_after(
    retry_after_ms: Option<&str>,
    retry_after: Option<&str>,
    now: SystemTime,
) -> Option<Duration> {
    if let Some(ms) = retry_after_ms.and_then(|v| v.trim().parse::<f64>().ok()) {
        if ms.is_finite() && ms >= 0.0 {
            return Some(Duration::from_secs_f64(ms / 1000.0));
        }
    }
    let value = retry_after?.trim();
    if let Ok(secs) = value.parse::<f64>() {
        return (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let target = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(u64::try_from(date.timestamp()).ok()?))?;
    Some(target.duration_since(now).unwrap_or(Duration::ZERO))
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// What to do with a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Transient: try again, after `after` when the server asked for a wait.
    Retry { after: Option<Duration> },
    /// Deterministic: surface the error now.
    Fail,
}

impl RetryDecision {
    pub fn is_retry(self) -> bool {
        matches!(self, RetryDecision::Retry { .. })
    }
}

/// HTTP statuses that indicate a transient failure.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 520..=524 | 529)
}

/// Quota / billing / credential failures. These are never retried even when
/// they arrive as a 429 or inside an otherwise transient-looking message.
const FATAL_PATTERNS: &[&str] = &[
    "insufficient_quota",
    "quota exceeded",
    "quota_exceeded",
    "out of budget",
    "billing_not_active",
    "billing hard limit",
    "credit balance",
    "usage limit",
    "usagelimiterror",
    "subscription_sharing_usage_limit_exceeded",
    "invalid api key",
    "invalid_api_key",
    "invalid x-api-key",
    "authentication_error",
    "permission_error",
    "builder error",
    "relative url without a base",
];

/// Transient provider / transport failures (ported from pi's classifier).
const TRANSIENT_PATTERNS: &[&str] = &[
    "overloaded",
    "server_busy",
    "servers are currently busy",
    "high demand",
    "at capacity",
    "rate limit",
    "rate_limit",
    "too many requests",
    "service unavailable",
    "service_unavailable",
    "temporarily unavailable",
    "bad gateway",
    "gateway timeout",
    "internal server error",
    "server error",
    "provider returned error",
    "upstream",
    "network error",
    "connection",
    "error sending request",
    "http request failed",
    "broken pipe",
    "reset by peer",
    "unexpected eof",
    "socket hang up",
    "dns error",
    "eai_again",
    "timed out",
    "timeout",
    "stream read error",
    "stream stalled",
    "stream ended",
    "stream_incomplete",
    "ended without",
    "you can retry",
    "try your request again",
    "please retry",
    "resourceexhausted",
    "resource_exhausted",
];

/// Whether `message` names a quota, billing, or credential failure.
pub fn is_fatal_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    FATAL_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Free-text classification: transient unless the message names a fatal cause.
pub fn classify_message(message: &str) -> RetryDecision {
    let lower = message.to_lowercase();
    if !is_fatal_message(message) && TRANSIENT_PATTERNS.iter().any(|p| lower.contains(p)) {
        RetryDecision::Retry { after: None }
    } else {
        RetryDecision::Fail
    }
}

fn classify_status(status: u16, message: &str, after: Option<Duration>) -> RetryDecision {
    if is_retryable_status(status) && !is_fatal_message(message) {
        RetryDecision::Retry { after }
    } else {
        RetryDecision::Fail
    }
}

/// Classify a provider-adapter error.
pub fn classify_provider_error(err: &ProviderError) -> RetryDecision {
    match err {
        ProviderError::RateLimited { retry_after, .. } => RetryDecision::Retry { after: *retry_after },
        ProviderError::ServerError { status, message, is_retryable, retry_after, .. } => {
            if *is_retryable && !is_fatal_message(message) {
                RetryDecision::Retry { after: *retry_after }
            } else if let Some(status) = status {
                classify_status(*status, message, *retry_after)
            } else {
                RetryDecision::Fail
            }
        }
        ProviderError::StreamError { message, .. } => {
            if is_fatal_message(message) {
                RetryDecision::Fail
            } else {
                RetryDecision::Retry { after: None }
            }
        }
        ProviderError::Other { status: Some(status), message, .. } => {
            classify_status(*status, message, None)
        }
        ProviderError::Other { status: None, message, .. } => classify_message(message),
        ProviderError::ContextOverflow { .. }
        | ProviderError::AuthFailed { .. }
        | ProviderError::QuotaExceeded { .. }
        | ProviderError::ModelNotFound { .. }
        | ProviderError::InvalidRequest { .. }
        | ProviderError::ContentFiltered { .. } => RetryDecision::Fail,
    }
}

/// Classify an error from the legacy `AnthropicClient` path.
pub fn classify_claude_error(err: &ClaudeError) -> RetryDecision {
    match err {
        ClaudeError::ApiStatus { status, message, retry_after } => {
            classify_status(*status, message, *retry_after)
        }
        ClaudeError::RateLimit => RetryDecision::Retry { after: None },
        ClaudeError::Api(message) | ClaudeError::Other(message) => classify_message(message),
        ClaudeError::Http(e) => classify_message(&e.to_string()),
        ClaudeError::Io(e) => classify_message(&e.to_string()),
        _ => RetryDecision::Fail,
    }
}

/// Classify an in-band stream error (`event: error` / `StreamEvent::Error`).
pub fn classify_stream_error(error_type: &str, message: &str) -> RetryDecision {
    match error_type {
        "invalid_request_error" | "authentication_error" | "permission_error"
        | "not_found_error" | "billing_error" | "request_too_large" => RetryDecision::Fail,
        "overloaded_error" | "api_error" | "rate_limit_error" | "timeout_error"
        | "stream_error" | "stream_incomplete" => {
            if is_fatal_message(message) {
                RetryDecision::Fail
            } else {
                RetryDecision::Retry { after: None }
            }
        }
        _ => classify_message(&format!("{error_type}: {message}")),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::provider_id::ProviderId;

    fn pid() -> ProviderId {
        ProviderId::new("test")
    }

    #[test]
    fn retry_after_seconds_and_ms() {
        let now = SystemTime::now();
        assert_eq!(parse_retry_after(None, Some("7"), now), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_retry_after(Some("1500"), Some("7"), now),
            Some(Duration::from_millis(1500)),
            "retry-after-ms wins over Retry-After"
        );
        assert_eq!(parse_retry_after(None, Some("0.5"), now), Some(Duration::from_millis(500)));
        assert_eq!(parse_retry_after(None, Some("soon"), now), None);
        assert_eq!(parse_retry_after(None, None, now), None);
    }

    #[test]
    fn retry_after_http_date() {
        // Wed, 21 Oct 2015 07:28:00 GMT == 1445412480
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_445_412_480 - 12);
        assert_eq!(
            parse_retry_after(None, Some("Wed, 21 Oct 2015 07:28:00 GMT"), now),
            Some(Duration::from_secs(12))
        );
        // A date in the past means "retry now".
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(1_445_412_480 + 30);
        assert_eq!(
            parse_retry_after(None, Some("Wed, 21 Oct 2015 07:28:00 GMT"), later),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn retry_after_reads_header_map() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", reqwest::header::HeaderValue::from_static("3"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(3)));
    }

    #[test]
    fn delays_grow_and_are_capped() {
        let cfg = RetryConfig::default();
        assert!(cfg.backoff(1) <= Duration::from_secs(1));
        assert!(cfg.backoff(1) >= Duration::from_millis(750));
        assert!(cfg.backoff(3) >= Duration::from_secs(3));
        assert!(cfg.backoff(30) <= MAX_RETRY_DELAY);
        assert_eq!(
            cfg.delay_for(1, Some(Duration::from_secs(600))),
            MAX_RETRY_DELAY,
            "a server-requested wait is capped at 60 s"
        );
        assert_eq!(cfg.delay_for(1, Some(Duration::from_secs(2))), Duration::from_secs(2));
    }

    #[test]
    fn status_classification() {
        for s in [408, 409, 425, 429, 500, 502, 503, 504, 529] {
            assert!(is_retryable_status(s), "{s} should be retryable");
        }
        for s in [400, 401, 403, 404, 413, 422] {
            assert!(!is_retryable_status(s), "{s} must never be retried");
        }
    }

    #[test]
    fn provider_error_classification() {
        let rate = ProviderError::RateLimited {
            provider: pid(),
            retry_after: Some(Duration::from_secs(4)),
        };
        assert_eq!(
            classify_provider_error(&rate),
            RetryDecision::Retry { after: Some(Duration::from_secs(4)) }
        );
        let auth = ProviderError::AuthFailed { provider: pid(), message: "bad key".into() };
        assert_eq!(classify_provider_error(&auth), RetryDecision::Fail);
        let quota = ProviderError::QuotaExceeded { provider: pid(), message: "x".into() };
        assert_eq!(classify_provider_error(&quota), RetryDecision::Fail);
        let conn = ProviderError::Other {
            provider: pid(),
            message: "HTTP request failed: error sending request for url".into(),
            status: None,
            body: None,
        };
        assert!(classify_provider_error(&conn).is_retry());
        let unprocessable = ProviderError::Other {
            provider: pid(),
            message: "bad schema".into(),
            status: Some(422),
            body: None,
        };
        assert_eq!(classify_provider_error(&unprocessable), RetryDecision::Fail);
        let stream_quota = ProviderError::StreamError {
            provider: pid(),
            message: "insufficient_quota".into(),
            partial_response: None,
        };
        assert_eq!(classify_provider_error(&stream_quota), RetryDecision::Fail);
    }

    #[test]
    fn claude_error_classification() {
        let overloaded = ClaudeError::ApiStatus {
            status: 529,
            message: "Overloaded".into(),
            retry_after: None,
        };
        assert!(classify_claude_error(&overloaded).is_retry());
        let unauthorized = ClaudeError::ApiStatus {
            status: 401,
            message: "invalid x-api-key".into(),
            retry_after: None,
        };
        assert_eq!(classify_claude_error(&unauthorized), RetryDecision::Fail);
        assert_eq!(classify_claude_error(&ClaudeError::Auth("no".into())), RetryDecision::Fail);
        let billing_429 = ClaudeError::ApiStatus {
            status: 429,
            message: "Your credit balance is too low".into(),
            retry_after: None,
        };
        assert_eq!(classify_claude_error(&billing_429), RetryDecision::Fail);
        assert!(classify_claude_error(&ClaudeError::Api("connection error: reset".into())).is_retry());
        // Transient per-minute 429s often link to a billing page; they must
        // still be retried (Groq / OpenAI free tier / Gemini wording).
        for msg in [
            "Rate limit reached for model in organization on tokens per minute (TPM). Upgrade to Dev Tier at https://console.groq.com/settings/billing",
            "Rate limit reached for requests. Visit https://platform.openai.com/account/billing to add a payment method",
            "Resource has been exhausted. Please check your plan and billing details.",
        ] {
            let throttled = ClaudeError::ApiStatus { status: 429, message: msg.into(), retry_after: None };
            assert!(classify_claude_error(&throttled).is_retry(), "{msg}");
        }
    }

    #[test]
    fn stream_error_classification() {
        assert!(classify_stream_error("overloaded_error", "Overloaded").is_retry());
        assert!(classify_stream_error("stream_incomplete", "ended early").is_retry());
        assert_eq!(classify_stream_error("invalid_request_error", "bad"), RetryDecision::Fail);
        assert_eq!(classify_stream_error("weird", "nothing known"), RetryDecision::Fail);
        assert!(classify_stream_error("weird", "upstream connect error").is_retry());
    }

    // ---- HTTP-level: a real provider against a one-shot local server -------

    /// Serve exactly one raw HTTP response on a loopback port and return the
    /// base URL. The request (headers + `content-length` body) is drained
    /// first so the client never sees a reset.
    pub(crate) async fn serve_once(response: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
        });
        format!("http://{addr}")
    }

    pub(crate) fn tiny_request() -> crate::ProviderRequest {
        crate::ProviderRequest {
            model: "m".to_string(),
            messages: vec![claurst_core::types::Message::user("hi")],
            system_prompt: None,
            tools: vec![],
            max_tokens: 16,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            thinking: None,
            provider_options: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn http_429_retry_after_date_reaches_the_policy() {
        use crate::provider::LlmProvider;
        let when = chrono::Utc::now() + chrono::Duration::seconds(30);
        let body = r#"{"error":{"message":"slow down"}}"#;
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            when.format("%a, %d %b %Y %H:%M:%S GMT"),
            body.len(),
            body
        );
        let base = serve_once(response).await;
        let provider = crate::OpenAiCompatProvider::new("t", "T", base);
        let err = match provider.create_message_stream(tiny_request()).await {
            Err(e) => e,
            Ok(_) => panic!("a 429 must be an error"),
        };
        match classify_provider_error(&err) {
            RetryDecision::Retry { after: Some(wait) } => {
                assert!(
                    wait >= Duration::from_secs(25) && wait <= Duration::from_secs(31),
                    "Retry-After HTTP-date should yield ~30 s, got {wait:?}"
                );
            }
            other => panic!("expected a retry with a server wait, got {other:?} for {err}"),
        }
    }

    #[tokio::test]
    async fn http_401_is_not_retryable() {
        use crate::provider::LlmProvider;
        let body = r#"{"error":{"message":"Incorrect API key provided"}}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let base = serve_once(response).await;
        let provider = crate::OpenAiCompatProvider::new("t", "T", base);
        let err = match provider.create_message_stream(tiny_request()).await {
            Err(e) => e,
            Ok(_) => panic!("a 401 must be an error"),
        };
        assert_eq!(classify_provider_error(&err), RetryDecision::Fail, "{err}");
    }

    #[tokio::test]
    async fn http_stream_cut_before_finish_reason_is_a_retryable_error() {
        use crate::provider::LlmProvider;
        use futures::StreamExt;
        let body = "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let base = serve_once(response).await;
        let provider = crate::OpenAiCompatProvider::new("t", "T", base);
        let mut stream = provider
            .create_message_stream(tiny_request())
            .await
            .expect("200 response opens a stream");
        let mut saw_stop = false;
        let mut last_err = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(crate::StreamEvent::MessageStop) => saw_stop = true,
                Ok(_) => {}
                Err(e) => last_err = Some(e),
            }
        }
        assert!(!saw_stop, "a truncated stream must not report MessageStop");
        let err = last_err.expect("a truncated stream must end with an error");
        assert!(classify_provider_error(&err).is_retry(), "{err}");
    }
}
