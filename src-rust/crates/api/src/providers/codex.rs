// providers/codex.rs — OpenAI Codex provider (OAuth-authenticated).
//
// Codex uses OpenAI's Responses API at:
//   https://chatgpt.com/backend-api/codex/responses
//
// Auth: Bearer token obtained via the Codex OAuth flow stored in
//   ~/.claurst/codex_tokens.json (`CodexTokens` struct).
//
// Token refresh: when the access token (JWT `exp`, else the stored
//   `expires_at`) expires within `REFRESH_WINDOW_SECS` we POST to the OpenAI
//   token endpoint with `grant_type=refresh_token` before making the request,
//   matching the official client's 5-minute proactive refresh window.
//
// Model list: the live ChatGPT catalog (`GET  .../codex/models?client_version=X`,
// pinned to `CODEX_CATALOG_CLIENT_VERSION`), filtered to the entries the
// official picker lists, falling back to the `CODEX_MODELS` constant from
// `claurst-core` when the fetch fails.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_stream::stream;
use async_trait::async_trait;
use claurst_core::codex_oauth::{
    CODEX_API_ENDPOINT, CODEX_MODELS, CODEX_TOKEN_URL, DEFAULT_CODEX_MODEL,
};
use claurst_core::accounts::jwt_identity;
use claurst_core::constants::TOOL_NAME_APPLY_PATCH;
use claurst_core::oauth_config::{get_codex_tokens, save_codex_tokens, CodexTokens};
use claurst_core::provider_id::{ModelId, ProviderId};
use claurst_core::types::UsageInfo;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::error_handling::parse_error_response;
use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, RateLimitUsage,
    StopReason, StreamEvent, SystemPromptStyle,
};

// Re-use Copilot's message translation helpers via the public Copilot type.
use crate::providers::copilot::CopilotProvider;

// ---------------------------------------------------------------------------
// apply_patch as a freeform tool
// ---------------------------------------------------------------------------

/// The name Codex models call the patch tool by.
const FREEFORM_APPLY_PATCH: &str = "apply_patch";

/// The grammar the official client attaches to its freeform `apply_patch`
/// tool, from openai/codex `codex-rs/core/assets/tools/apply_patch.lark`
/// (Copyright 2025 OpenAI, Apache License 2.0).
const APPLY_PATCH_LARK_GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF

%import common.LF
"#;

/// Whether `model` takes apply_patch as a freeform `custom` tool with the
/// Lark grammar rather than a JSON function tool. The official client reads
/// this from the catalog's `apply_patch_tool_type`, which is `freeform` for
/// every model it lists (all gpt-5 and later, `models-manager/models.json`);
/// those are the models trained on the envelope.
fn uses_freeform_apply_patch(model: &str) -> bool {
    let model = model.rsplit('/').next().unwrap_or(model);
    model.starts_with("gpt-5") || model.starts_with("gpt-6") || model.contains("codex")
}

/// Rewrite replayed ApplyPatch calls into the `custom_tool_call` /
/// `custom_tool_call_output` items a freeform tool's calls are, so the
/// history matches the tool the request declares.
fn replay_apply_patch_as_custom(input: &mut [Value]) {
    let mut call_ids: Vec<String> = Vec::new();
    for item in input.iter_mut() {
        if item["type"] != "function_call" || item["name"] != TOOL_NAME_APPLY_PATCH {
            continue;
        }
        let patch = item["arguments"]
            .as_str()
            .and_then(|args| serde_json::from_str::<Value>(args).ok())
            .and_then(|args| args["patch"].as_str().map(str::to_owned));
        let (Some(patch), Some(call_id)) = (patch, item["call_id"].as_str().map(str::to_owned)) else {
            continue;
        };
        *item = json!({
            "type": "custom_tool_call",
            "call_id": call_id,
            "name": FREEFORM_APPLY_PATCH,
            "input": patch,
        });
        call_ids.push(call_id);
    }
    for item in input.iter_mut() {
        if item["type"] == "function_call_output"
            && item["call_id"].as_str().is_some_and(|id| call_ids.iter().any(|c| c == id))
        {
            item["type"] = json!("custom_tool_call_output");
        }
    }
}

/// The tool a `custom_tool_call` names, in claurst's tool names.
fn custom_tool_name(name: &str) -> String {
    if name == FREEFORM_APPLY_PATCH { TOOL_NAME_APPLY_PATCH } else { name }.to_string()
}

// ---------------------------------------------------------------------------
// CodexProvider
// ---------------------------------------------------------------------------

/// Refresh the access token this many seconds before it expires (the official
/// client's `CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES` = 5).
const REFRESH_WINDOW_SECS: u64 = 5 * 60;

/// Client version the live model-catalog query advertises.
///
/// The ChatGPT backend (`codex-api/src/endpoint/models.rs`) returns only the
/// models whose `minimal_client_version` is at or below this value, so the
/// pinned constant must be the highest codex release whose request shape
/// claurst fully implements (`0.124.0`, the last one before
/// `use_responses_lite` entries started being listed). Never send claurst's
/// own version — `0.1.8` hides every entry — and never a current codex
/// version, which lists lite models whose request body claurst does not
/// build. Bump this when claurst implements the lite request shape.
pub const CODEX_CATALOG_CLIENT_VERSION: &str = "0.124.0";

// ---------------------------------------------------------------------------
// Live model catalog (codex-rs models-manager)
// ---------------------------------------------------------------------------

/// One entry of the Codex `/models` catalog response.
///
/// Field set intentionally minimal: only what claurst's registry and gating
/// need, with `serde` defaults so new server fields never break parsing
/// (mirrors codex-rs `openai_models::Model`).
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct CodexCatalogModel {
    /// Model slug, e.g. `gpt-5.5`.
    #[serde(default)]
    pub slug: String,
    /// Display name, e.g. `GPT-5.5 (default)`; falls back to the slug.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Total context window in tokens.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Catalog ordering priority (higher = preferred default in the
    /// official client's picker).
    #[serde(default)]
    pub priority: Option<u64>,
    /// Whether the model is callable over the Responses API.
    #[serde(default)]
    pub supported_in_api: bool,
    /// Picker visibility (`"list"` = shown in the official picker).
    #[serde(default)]
    pub visibility: Option<String>,
    /// Whether the model only accepts the "lite" Responses request shape,
    /// which claurst does not emit yet (see
    /// [`CODEX_CATALOG_CLIENT_VERSION`]).
    #[serde(default)]
    pub use_responses_lite: bool,
    /// Reasoning efforts the model accepts, ascending (official
    /// `supported_reasoning_levels`); empty for non-reasoning models.
    #[serde(default)]
    pub supported_reasoning_levels: Vec<CodexReasoningLevel>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct CodexReasoningLevel {
    #[serde(default)]
    pub effort: String,
}

impl CodexCatalogModel {
    /// Entries the official picker lists: callable, listed, and not
    /// restricted to the lite request shape.
    pub fn is_listed(&self) -> bool {
        self.supported_in_api
            && self.visibility.as_deref() == Some("list")
            && !self.use_responses_lite
    }
}

/// Parse a canned/live `/models` response, keeping only the entries the
/// official picker would list ([`CodexCatalogModel::is_listed`]).
pub fn parse_codex_catalog(json_val: &Value) -> Vec<CodexCatalogModel> {
    let Some(models) = json_val.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|model| serde_json::from_value::<CodexCatalogModel>(model.clone()).ok())
        .filter(CodexCatalogModel::is_listed)
        .collect()
}

/// The last successfully fetched live catalog, shared process-wide so a
/// mid-conversation model switch (or the `include` gating below) consults the
/// same registry snapshot under `codex`. Hydrated by
/// [`CodexProvider::fetch_catalog`]; until then the static `CODEX_MODELS`
/// fallback stands in.
static LIVE_CATALOG: RwLock<Option<Box<[CodexCatalogModel]>>> = RwLock::new(None);

/// Process-wide one-shot for the lazy catalog hydration: a failed attempt is
/// cached too, so requests never stall behind a repeatedly failing endpoint
/// and no request path retries it.
static CATALOG_HYDRATION: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// Publish a parsed catalog to the process-wide registry.
fn set_live_catalog(models: Vec<CodexCatalogModel>) {
    let mut guard = LIVE_CATALOG.write().unwrap_or_else(PoisonError::into_inner);
    *guard = Some(models.into_boxed_slice());
}

/// Look `model` up in the live catalog (accepting a `provider/` prefix the
/// way `uses_freeform_apply_patch` does). `None` while no live catalog has
/// been fetched — callers fall back to the model-id heuristics.
pub fn codex_catalog_lookup(model: &str) -> Option<CodexCatalogModel> {
    let slug = model.rsplit('/').next().unwrap_or(model);
    let guard = LIVE_CATALOG.read().unwrap_or_else(PoisonError::into_inner);
    guard
        .as_deref()
        .and_then(|catalog| catalog.iter().find(|m| m.slug == slug))
        .cloned()
}

/// Base URL for the catalog endpoint, derived from the Responses URL so the
/// two can never drift (`https://chatgpt.com/backend-api/codex/models`).
fn codex_models_url() -> String {
    let base = CODEX_API_ENDPOINT
        .trim_end_matches("responses")
        .trim_end_matches('/');
    format!("{base}/models?client_version={CODEX_CATALOG_CLIENT_VERSION}")
}

/// One fetch attempt of the live catalog. Failure is swallowed by the caller;
/// the static builtin list remains the fallback.
async fn fetch_catalog_raw(
    provider_id: &ProviderId,
    http_client: &reqwest::Client,
    token: &str,
    account_id: Option<&str>,
) -> Result<Vec<CodexCatalogModel>, ProviderError> {
    let builder = http_client.get(codex_models_url()).header("Accept", "application/json");
    let builder = CodexProvider::codex_headers(builder, token, account_id, &json!({}));
    let resp = builder.send().await.map_err(|e| ProviderError::Other {
        provider: provider_id.clone(),
        message: format!("Codex model catalog request failed: {}", e),
        status: None,
        body: None,
    })?;

    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| ProviderError::Other {
        provider: provider_id.clone(),
        message: format!("Failed to read Codex model catalog response: {}", e),
        status: Some(status),
        body: None,
    })?;

    if !(200..300).contains(&(status as usize)) {
        return Err(ProviderError::Other {
            provider: provider_id.clone(),
            message: format!("Codex model catalog request failed (HTTP {})", status),
            status: Some(status),
            body: Some(text),
        });
    }

    let json_val: Value = serde_json::from_str(&text).map_err(|e| ProviderError::Other {
        provider: provider_id.clone(),
        message: format!("Failed to parse Codex model catalog JSON: {}", e),
        status: Some(status),
        body: Some(text.clone()),
    })?;

    let models = parse_codex_catalog(&json_val);
    if models.is_empty() {
        return Err(ProviderError::Other {
            provider: provider_id.clone(),
            message: "Codex model catalog listed no usable models".to_string(),
            status: Some(status),
            body: Some(text),
        });
    }
    Ok(models)
}

/// Process-wide refresh guard (the official client's `AuthManager.refresh_lock`).
/// Refresh tokens are single-use and the query path builds a fresh
/// `CodexProvider` per request, so the lock cannot live on the instance.
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// OAuth error codes meaning the refresh token is permanently unusable
/// (official `classify_refresh_token_failure`).
fn is_permanent_refresh_failure(status: u16, body: &str) -> bool {
    let code = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        v.pointer("/error/code")
            .or_else(|| v.get("error"))
            .or_else(|| v.get("code"))
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
    });
    status == 401
        || matches!(
            code.as_deref(),
            Some(
                "invalid_grant"
                    | "refresh_token_expired"
                    | "refresh_token_reused"
                    | "refresh_token_invalidated"
            )
        )
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct CodexProvider {
    id: ProviderId,
    http_client: reqwest::Client,
    /// Mutable token cache: updated in-place when a refresh succeeds.
    tokens: Arc<Mutex<CodexTokens>>,
}

impl CodexProvider {
    pub fn new(mut tokens: CodexTokens) -> Self {
        // Token files written before the claim name was fixed carry no
        // account id; derive it so `ChatGPT-Account-Id` is still sent.
        if tokens.account_id.is_none() {
            tokens.account_id = jwt_identity(&tokens.access_token).account_id;
        }
        let http_client = reqwest::Client::builder()
            .timeout(crate::request_timeout())
            .build()
            .expect("failed to build reqwest client");

        Self {
            id: ProviderId::new(ProviderId::CODEX),
            http_client,
            tokens: Arc::new(Mutex::new(tokens)),
        }
    }

    /// Construct from stored tokens; returns `None` if no tokens are saved.
    pub fn from_stored() -> Option<Self> {
        let tokens = get_codex_tokens()?;
        if tokens.access_token.is_empty() {
            return None;
        }
        Some(Self::new(tokens))
    }

    // -----------------------------------------------------------------------
    // Token management
    // -----------------------------------------------------------------------

    /// True when the access token expires within `REFRESH_WINDOW_SECS` of
    /// `now`. Tokens with no known expiry are treated as valid.
    fn is_expired(tokens: &CodexTokens, now: u64) -> bool {
        tokens
            .access_token_expiry()
            .is_some_and(|expires_at| now.saturating_add(REFRESH_WINDOW_SECS) >= expires_at)
    }

    /// Snapshot `(access_token, expired, refresh_token)` under the token lock.
    fn token_state(&self) -> (String, bool, Option<String>) {
        let guard = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        (
            guard.access_token.clone(),
            Self::is_expired(&guard, unix_now()),
            guard.refresh_token.clone(),
        )
    }

    /// Return the current access token, refreshing first if it is expired.
    async fn access_token(&self) -> Result<String, ProviderError> {
        let (token, needs_refresh, _) = self.token_state();
        if !needs_refresh {
            return Ok(token);
        }

        // Under the process-wide lock, reload from disk: another provider
        // instance (subagent, cron, a previous request) may already have
        // refreshed and spent the single-use refresh token. Adopt its result
        // instead of refreshing again (official `manager.rs` guarded reload).
        let _refresh_guard = REFRESH_LOCK.lock().await;
        if let Some(stored) = get_codex_tokens() {
            if stored.access_token != token
                && !stored.access_token.is_empty()
                && !Self::is_expired(&stored, unix_now())
            {
                let fresh = stored.access_token.clone();
                let mut guard = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
                *guard = stored;
                if guard.account_id.is_none() {
                    guard.account_id = jwt_identity(&guard.access_token).account_id;
                }
                return Ok(fresh);
            }
        }
        let (token, needs_refresh, refresh_token) = self.token_state();
        if !needs_refresh {
            return Ok(token);
        }

        let Some(refresh) = refresh_token else {
            // No refresh token — return what we have and hope for the best.
            warn!("Codex access token is expired and no refresh token is available");
            return Ok(token);
        };

        debug!("Codex access token expired — refreshing");
        self.refresh_token(&refresh).await
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<String, ProviderError> {
        let body = json!({
            "grant_type": "refresh_token",
            "client_id": claurst_core::codex_oauth::CODEX_CLIENT_ID,
            "refresh_token": refresh_token,
        });

        let resp = self
            .http_client
            .post(CODEX_TOKEN_URL)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("Token refresh request failed: {}", e),
                status: None,
                body: None,
            })?;

        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to read token refresh response: {}", e),
            status: Some(status),
            body: None,
        })?;

        if !(200..300).contains(&(status as usize)) {
            if is_permanent_refresh_failure(status, &text) {
                return Err(ProviderError::AuthFailed {
                    provider: self.id.clone(),
                    message: "Codex refresh token was rejected (expired, revoked, or already \
                              used); run /connect to sign in again"
                        .to_string(),
                });
            }
            return Err(ProviderError::Other {
                provider: self.id.clone(),
                message: format!("Token refresh failed (HTTP {})", status),
                status: Some(status),
                body: Some(text),
            });
        }

        let json_val: Value = serde_json::from_str(&text).map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to parse token refresh response: {}", e),
            status: Some(status),
            body: Some(text.clone()),
        })?;

        let new_access = json_val
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if new_access.is_empty() {
            return Err(ProviderError::Other {
                provider: self.id.clone(),
                message: "Token refresh response missing access_token".to_string(),
                status: Some(status),
                body: Some(text),
            });
        }

        let new_refresh = json_val
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let new_expires_at = claurst_core::oauth_config::codex_expires_at(
            json_val.get("expires_in").and_then(|v| v.as_u64()),
            &new_access,
            unix_now(),
        );

        // Persist and cache the refreshed tokens.
        let mut updated = {
            let guard = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
            guard.clone()
        };
        updated.access_token = new_access.clone();
        if let Some(r) = new_refresh {
            updated.refresh_token = Some(r);
        }
        updated.expires_at = new_expires_at;
        if let Some(account_id) = json_val
            .get("id_token")
            .and_then(|v| v.as_str())
            .and_then(|id_token| jwt_identity(id_token).account_id)
            .or_else(|| jwt_identity(&new_access).account_id)
        {
            updated.account_id = Some(account_id);
        }

        if let Err(e) = save_codex_tokens(&updated) {
            warn!("Failed to persist refreshed Codex tokens: {}", e);
        }

        {
            let mut guard = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
            *guard = updated;
        }

        Ok(new_access)
    }

    // -----------------------------------------------------------------------
    // Request helpers
    // -----------------------------------------------------------------------

    /// Auth and routing headers for a Codex request. `body` supplies the
    /// `session-id` header: ChatGPT derives prompt-cache affinity from it, and
    /// the official client sets it to the same value as `prompt_cache_key`.
    fn codex_headers(
        builder: reqwest::RequestBuilder,
        token: &str,
        account_id: Option<&str>,
        body: &Value,
    ) -> reqwest::RequestBuilder {
        let mut builder = builder
            .bearer_auth(token)
            .header("User-Agent", concat!("claurst/", env!("CARGO_PKG_VERSION")));

        if let Some(id) = account_id {
            builder = builder.header("ChatGPT-Account-Id", id);
        }
        if let Some(session_id) = body.get("prompt_cache_key").and_then(Value::as_str) {
            builder = builder.header("session-id", session_id);
        }
        builder
    }

    /// Parse the `x-codex-{primary,secondary}-used-percent` headers (official
    /// `codex-api/src/rate_limits.rs`). `None` when neither is present.
    fn parse_rate_limits(headers: &reqwest::header::HeaderMap) -> Option<RateLimitUsage> {
        let used = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<f32>().ok())
                .filter(|pct| pct.is_finite())
        };
        let primary_used_percent = used("x-codex-primary-used-percent");
        let secondary_used_percent = used("x-codex-secondary-used-percent");
        if primary_used_percent.is_none() && secondary_used_percent.is_none() {
            return None;
        }
        Some(RateLimitUsage {
            primary_used_percent,
            secondary_used_percent,
        })
    }

    fn account_id(&self) -> Option<String> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner).account_id.clone()
    }

    /// Fetch the live model catalog with fresh credentials (used by
    /// `discover_models`, which must not hit a stale one-shot guard).
    async fn fetch_catalog(&self) -> Result<Vec<CodexCatalogModel>, ProviderError> {
        let token = self.access_token().await?;
        let account_id = self.account_id();
        let models = fetch_catalog_raw(&self.id, &self.http_client, &token, account_id.as_deref()).await?;
        set_live_catalog(models.clone());
        Ok(models)
    }

    /// Hydrate the shared catalog once per process, before the request body is
    /// built, so the `include: reasoning.encrypted_content` gate and any later
    /// model switch consult real capabilities instead of a model-id heuristic.
    /// Best-effort: failures leave the static fallback in place.
    async fn ensure_catalog(&self, token: &str, account_id: Option<&str>) {
        let _ = CATALOG_HYDRATION
            .get_or_try_init(|| async {
                if let Ok(models) =
                    fetch_catalog_raw(&self.id, &self.http_client, token, account_id).await
                {
                    set_live_catalog(models);
                }
                // Failure is cached too: one attempt per process, never a
                // retry storm against the catalog endpoint.
                Ok::<(), ProviderError>(())
            })
            .await;
    }

    /// Build the Responses-API request body for Codex.
    fn build_responses_body(request: &ProviderRequest) -> Value {
        // Re-use the same message translation that the Copilot provider uses.
        // It emits the system prompt as a leading `role: system` input item;
        // the official client sends base instructions exactly once, as an
        // `input[0]` developer message with no top-level `instructions`.
        let mut input = CopilotProvider::to_responses_input_pub(request);
        if let Some(first) = input.first_mut() {
            if first.get("role").and_then(Value::as_str) == Some("system") {
                first["role"] = json!("developer");
            }
        }
        // The official client only replays reasoning items that carry
        // `encrypted_content`; a summary-only item cannot be resumed by a
        // store:false backend, so drop those.
        input.retain(|item| {
            item.get("type").and_then(Value::as_str) != Some("reasoning")
                || item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_some_and(|data| !data.is_empty())
        });

        let freeform_patch = uses_freeform_apply_patch(&request.model);
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                if freeform_patch && tool.name == TOOL_NAME_APPLY_PATCH {
                    return json!({
                        "type": "custom",
                        "name": FREEFORM_APPLY_PATCH,
                        "description": "The `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.",
                        "format": {
                            "type": "grammar",
                            "syntax": "lark",
                            "definition": APPLY_PATCH_LARK_GRAMMAR,
                        },
                    });
                }
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                    "strict": false,
                })
            })
            .collect();
        if freeform_patch {
            replay_apply_patch_as_custom(&mut input);
        }

        let mut body = json!({
            "model": request.model,
            "input": input,
            "store": false,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }

        // Apply reasoning effort / summary / verbosity exactly like the Copilot
        // Responses path. The query layer already populates `reasoningEffort`
        // (default "medium"), `reasoningSummary`, and `include` for gpt-5 Codex
        // models — without this they were silently dropped and every request ran
        // at the server default. Mirrors opencode's gpt-5 reasoning defaults.
        CopilotProvider::apply_responses_provider_options_pub(&mut body, &request.provider_options);

        // The query layer's `include` option rides a gpt-5*/o* model-id
        // heuristic, which fires for every gpt-5 slug — including ones the
        // live catalog knows have no reasoning levels. A model that produces
        // no encrypted reasoning state must not request it (the endpoint
        // rejects the include). Catalog-unknown models keep the heuristic
        // fallback since nothing else discriminates.
        if codex_catalog_lookup(&request.model)
            .is_some_and(|entry| entry.supported_reasoning_levels.is_empty())
        {
            if let Some(object) = body.as_object_mut() {
                if let Some(Value::Array(items)) = object.get_mut("include") {
                    items.retain(|item| item.as_str() != Some("reasoning.encrypted_content"));
                    if items.is_empty() {
                        object.remove("include");
                    }
                }
            }
        }

        body
    }

    fn extract_stream_error_message(json_val: &Value) -> String {
        json_val
            .pointer("/error/message")
            .and_then(|value| value.as_str())
            .or_else(|| {
                json_val
                    .pointer("/response/error/message")
                    .and_then(|value| value.as_str())
            })
            .or_else(|| {
                json_val
                    .pointer("/message")
                    .and_then(|value| value.as_str())
            })
            .map(|value| value.to_string())
            .unwrap_or_else(|| json_val.to_string())
    }

    // -----------------------------------------------------------------------
    // HTTP call
    // -----------------------------------------------------------------------

    async fn send_responses_request(
        &self,
        request: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let mut token = self.access_token().await?;
        let account_id = self.account_id();

        // Best-effort one-shot: real capabilities (item 5's include gate) need
        // the live catalog before the body is built.
        self.ensure_catalog(&token, account_id.as_deref()).await;

        let body = Self::build_responses_body(request);

        let mut retried_auth = false;
        loop {
            let builder = self
                .http_client
                .post(CODEX_API_ENDPOINT)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json");
            let builder = Self::codex_headers(builder, &token, account_id.as_deref(), &body);

            let resp = builder
                .json(&body)
                .send()
                .await
                .map_err(|e| ProviderError::Other {
                    provider: self.id.clone(),
                    message: format!("HTTP request failed: {}", e),
                    status: None,
                    body: None,
                })?;

            let status = resp.status().as_u16();
            // A 401 means the bearer token was rejected even though the
            // 5-minute proactive refresh window said it was fine (server-side
            // revocation, expired JWT, adoption race). Refresh once and retry,
            // like the official client does before surfacing the error.
            if status == 401 && !retried_auth {
                retried_auth = true;
                if let Ok(refreshed) = self.refresh_after_auth_failure(&token).await {
                    debug!("Codex request hit a 401 — token refreshed, retrying once");
                    token = refreshed;
                    continue;
                }
            }

            // The non-streaming path must parse the x-codex-* usage headers
            // too — the streaming path gets them via `sse_events`.
            let rate_limits = Self::parse_rate_limits(resp.headers());

            let text = resp.text().await.map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("Failed to read response body: {}", e),
                status: Some(status),
                body: None,
            })?;

            if !(200..300).contains(&(status as usize)) {
                return Err(parse_error_response(status, &text, &self.id));
            }

            let json_val: Value = serde_json::from_str(&text).map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("Failed to parse response JSON: {}", e),
                status: Some(status),
                body: Some(text.clone()),
            })?;

            let mut response = Self::parse_responses_response(&self.id, &json_val)?;
            response.rate_limits = rate_limits;
            return Ok(response);
        }
    }

    async fn send_responses_streaming_request(
        &self,
        request: &ProviderRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let mut token = self.access_token().await?;
        let account_id = self.account_id();

        // Same one-shot as the non-streaming path, before body building.
        self.ensure_catalog(&token, account_id.as_deref()).await;

        let mut body = Self::build_responses_body(request);
        body["stream"] = json!(true);

        let mut retried_auth = false;
        loop {
            let builder = self
                .http_client
                .post(CODEX_API_ENDPOINT)
                .header("Content-Type", "application/json")
                .header("Accept", "text/event-stream");
            let builder = Self::codex_headers(builder, &token, account_id.as_deref(), &body);

            let resp = builder
                .json(&body)
                .send()
                .await
                .map_err(|e| ProviderError::Other {
                    provider: self.id.clone(),
                    message: format!("HTTP request failed: {}", e),
                    status: None,
                    body: None,
                })?;

            let status = resp.status().as_u16();
            if !(200..300).contains(&(status as usize)) {
                // Same refresh-once-and-retry contract as the non-streaming
                // request (only a pre-stream rejection can be replayed; a
                // mid-stream failure is terminal).
                if status == 401 && !retried_auth {
                    retried_auth = true;
                    if let Ok(refreshed) = self.refresh_after_auth_failure(&token).await {
                        debug!("Codex stream hit a 401 — token refreshed, retrying once");
                        token = refreshed;
                        continue;
                    }
                }
                let retry_after = crate::retry::retry_after(resp.headers());
                let text = resp.text().await.map_err(|e| ProviderError::Other {
                    provider: self.id.clone(),
                    message: format!("Failed to read response body: {}", e),
                    status: Some(status),
                    body: None,
                })?;
                return Err(
                    parse_error_response(status, &text, &self.id).with_retry_after(retry_after)
                );
            }

            return Ok(resp);
        }
    }

    /// Recover from a 401 by obtaining a usable token (official client's
    /// `reload-or-refresh` before surfacing the error):
    ///
    ///   1. adopt fresher tokens saved to disk by another provider instance —
    ///      the single-use refresh token may already have been spent there;
    ///   2. else force a refresh, bypassing the 5-minute proactive window that
    ///      produced the stale-bearer 401.
    ///
    /// Returns `Err` (propagating the refresh failure's own taxonomy) when no
    /// usable token can be produced; the caller then surfaces the original
    /// 401.
    async fn refresh_after_auth_failure(&self, stale_token: &str) -> Result<String, ProviderError> {
        // Refresh tokens are single-use: serialize against other instances.
        let _refresh_guard = REFRESH_LOCK.lock().await;
        if let Some(stored) = get_codex_tokens() {
            if stored.access_token != stale_token
                && !stored.access_token.is_empty()
                && !Self::is_expired(&stored, unix_now())
            {
                let fresh = stored.access_token.clone();
                let mut guard = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
                *guard = stored;
                if guard.account_id.is_none() {
                    guard.account_id = jwt_identity(&guard.access_token).account_id;
                }
                return Ok(fresh);
            }
        }

        let refresh = self
            .tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .refresh_token
            .clone();
        let Some(refresh) = refresh else {
            return Err(ProviderError::AuthFailed {
                provider: self.id.clone(),
                message: "Codex access token was rejected and no refresh token is available — \
                          run /connect to sign in again"
                    .to_string(),
            });
        };
        self.refresh_token(&refresh).await
    }

    // -----------------------------------------------------------------------
    // Response parsing  (mirrors CopilotProvider::parse_responses_response)
    // -----------------------------------------------------------------------

    fn parse_responses_response(
        provider_id: &ProviderId,
        json_val: &Value,
    ) -> Result<ProviderResponse, ProviderError> {
        use claurst_core::types::ContentBlock;

        let id = json_val
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let model = json_val
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_CODEX_MODEL)
            .to_string();

        let output = json_val
            .get("output")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ProviderError::Other {
                provider: provider_id.clone(),
                message: "No output in Codex Responses API response".to_string(),
                status: None,
                body: Some(json_val.to_string()),
            })?;

        let mut content: Vec<ContentBlock> = Vec::new();
        let mut has_tool_call = false;

        for item in output {
            match item.get("type").and_then(|v| v.as_str()) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(|v| v.as_array()) {
                        for part in parts {
                            match part.get("type").and_then(|v| v.as_str()) {
                                Some("output_text") | Some("text") => {
                                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                        if !text.is_empty() {
                                            content.push(ContentBlock::Text {
                                                text: text.to_string(),
                                            });
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Some("reasoning") => {
                    if let Some(summaries) = item.get("summary").and_then(|v| v.as_array()) {
                        let reasoning: String = summaries
                            .iter()
                            .filter_map(|s| s.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("");
                        if !reasoning.is_empty() {
                            content.push(ContentBlock::Thinking {
                                thinking: reasoning,
                                signature: String::new(),
                            });
                        }
                    }
                    if let Some(data) = item
                        .get("encrypted_content")
                        .and_then(|v| v.as_str())
                        .filter(|data| !data.is_empty())
                    {
                        content.push(ContentBlock::RedactedThinking {
                            data: data.to_string(),
                        });
                    }
                }
                Some("function_call") => {
                    has_tool_call = true;
                    let id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = item
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}");
                    let input = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
                    content.push(ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        thought_signature: None,
                    });
                }
                Some("custom_tool_call") => {
                    has_tool_call = true;
                    content.push(ContentBlock::ToolUse {
                        id: item["call_id"].as_str().or(item["id"].as_str()).unwrap_or("").to_string(),
                        name: custom_tool_name(item["name"].as_str().unwrap_or("")),
                        input: json!({ "patch": item["input"].as_str().unwrap_or("") }),
                        thought_signature: None,
                    });
                }
                _ => {}
            }
        }

        let stop_reason = if has_tool_call {
            StopReason::ToolUse
        } else {
            match json_val
                .get("incomplete_details")
                .and_then(|v| v.get("reason"))
                .and_then(|v| v.as_str())
            {
                Some("max_output_tokens") => StopReason::MaxTokens,
                Some("content_filter") => StopReason::ContentFiltered,
                Some(other) if !other.is_empty() => StopReason::Other(other.to_string()),
                _ => StopReason::EndTurn,
            }
        };

        let usage = {
            let u = json_val.get("usage");
            UsageInfo {
                input_tokens: u
                    .and_then(|v| v.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                output_tokens: u
                    .and_then(|v| v.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }
        };

        Ok(ProviderResponse {
            id,
            content,
            stop_reason,
            usage,
            model,
            rate_limits: None,
        })
    }

    /// Decode a Codex Responses SSE byte stream into `StreamEvent`s.
    /// `rate_limits` (parsed from the response headers) is emitted first.
    fn sse_events<S, B, E>(
        mut byte_stream: S,
        provider_id: ProviderId,
        rate_limits: Option<StreamEvent>,
    ) -> impl Stream<Item = Result<StreamEvent, ProviderError>> + Send
    where
        S: Stream<Item = Result<B, E>> + Unpin + Send,
        B: AsRef<[u8]> + Send,
        E: std::fmt::Display + Send,
    {
        stream! {
            if let Some(event) = rate_limits {
                yield Ok(event);
            }
            let mut leftover = String::new();
            let mut current_event = String::new();
            let mut current_data: Vec<String> = Vec::new();
            let mut message_started = false;
            let mut message_id = String::from("unknown");
            let mut model_name = String::from(DEFAULT_CODEX_MODEL);
            let mut saw_tool_call = false;
            let mut open_blocks: std::collections::HashSet<usize> = std::collections::HashSet::new();
            // Freeform tool input streamed so far, by output index.
            let mut custom_inputs: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
            // Reasoning items whose summary deltas were streamed into an open
            // block, by output index. Each output_index is one reasoning item,
            // so its Thinking block must be started and stopped as a unit and
            // the item's encrypted state attached to that same index when the
            // item completes — keeping the (summary, encrypted_content) pair
            // and the ordering relative to function calls.
            let mut reasoning_items: std::collections::HashSet<usize> = std::collections::HashSet::new();

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk = match chunk_result {
                    Ok(chunk) => chunk,
                    Err(e) => {
                        yield Err(ProviderError::StreamError {
                            provider: provider_id.clone(),
                            message: format!("Stream read error: {}", e),
                            partial_response: None,
                        });
                        return;
                    }
                };

                let text = String::from_utf8_lossy(chunk.as_ref());
                let combined = if leftover.is_empty() {
                    text.to_string()
                } else {
                    let mut s = std::mem::take(&mut leftover);
                    s.push_str(&text);
                    s
                };

                let mut lines: Vec<&str> = combined.split('\n').collect();
                if !combined.ends_with('\n') {
                    leftover = lines.pop().unwrap_or("").to_string();
                }

                for raw_line in lines {
                    let line = raw_line.trim_end_matches('\r');

                    if line.is_empty() {
                        if current_data.is_empty() {
                            current_event.clear();
                            continue;
                        }

                        let data = current_data.join("\n");
                        current_data.clear();
                        let trimmed = data.trim();
                        if trimmed.is_empty() || trimmed == "[DONE]" {
                            current_event.clear();
                            continue;
                        }

                        let json_val: Value = match serde_json::from_str(trimmed) {
                            Ok(value) => value,
                            Err(e) => {
                                yield Err(ProviderError::StreamError {
                                    provider: provider_id.clone(),
                                    message: format!("Failed to parse Codex stream JSON: {}", e),
                                    partial_response: Some(trimmed.to_string()),
                                });
                                return;
                            }
                        };

                        let event_name = if current_event.is_empty() {
                            json_val
                                .get("type")
                                .and_then(|value| value.as_str())
                                .unwrap_or("")
                                .to_string()
                        } else {
                            current_event.clone()
                        };

                        match event_name.as_str() {
                            "response.created" | "response.in_progress" => {
                                if let Some(response) = json_val.get("response") {
                                    if let Some(id) = response.get("id").and_then(|value| value.as_str()) {
                                        message_id = id.to_string();
                                    }
                                    if let Some(model) = response.get("model").and_then(|value| value.as_str()) {
                                        model_name = model.to_string();
                                    }
                                    if !message_started {
                                        yield Ok(StreamEvent::MessageStart {
                                            id: message_id.clone(),
                                            model: model_name.clone(),
                                            usage: UsageInfo::default(),
                                        });
                                        message_started = true;
                                    }
                                }
                            }
                            "response.output_item.added" => {
                                let output_index = json_val
                                    .get("output_index")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0) as usize;
                                if let Some(item) = json_val.get("item") {
                                    match item.get("type").and_then(|value| value.as_str()) {
                                        Some("message") => {
                                            if let Some(id) = item.get("id").and_then(|value| value.as_str()) {
                                                message_id = id.to_string();
                                            }
                                            if !message_started {
                                                yield Ok(StreamEvent::MessageStart {
                                                    id: message_id.clone(),
                                                    model: model_name.clone(),
                                                    usage: UsageInfo::default(),
                                                });
                                                message_started = true;
                                            }
                                        }
                                        Some("custom_tool_call") => {
                                            saw_tool_call = true;
                                            custom_inputs.insert(output_index, String::new());
                                            if open_blocks.insert(output_index) {
                                                yield Ok(StreamEvent::ContentBlockStart {
                                                    index: output_index,
                                                    content_block: claurst_core::types::ContentBlock::ToolUse {
                                                        id: item["call_id"].as_str().or(item["id"].as_str()).unwrap_or("").to_string(),
                                                        name: custom_tool_name(item["name"].as_str().unwrap_or("")),
                                                        input: json!({}),
                                                        thought_signature: None,
                                                    },
                                                });
                                            }
                                        }
                                        Some("function_call") => {
                                            saw_tool_call = true;
                                            let call_id = item
                                                .get("call_id")
                                                .or_else(|| item.get("id"))
                                                .and_then(|value| value.as_str())
                                                .unwrap_or("")
                                                .to_string();
                                            let name = item
                                                .get("name")
                                                .and_then(|value| value.as_str())
                                                .unwrap_or("")
                                                .to_string();
                                            if open_blocks.insert(output_index) {
                                                yield Ok(StreamEvent::ContentBlockStart {
                                                    index: output_index,
                                                    content_block: claurst_core::types::ContentBlock::ToolUse {
                                                        id: call_id,
                                                        name,
                                                        input: json!({}),
                                                        thought_signature: None,
                                                    },
                                                });
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            "response.content_part.added" => {
                                let output_index = json_val
                                    .get("output_index")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0) as usize;
                                if let Some(part) = json_val.get("part") {
                                    if matches!(part.get("type").and_then(|value| value.as_str()), Some("output_text") | Some("text")) {
                                        if !message_started {
                                            yield Ok(StreamEvent::MessageStart {
                                                id: message_id.clone(),
                                                model: model_name.clone(),
                                                usage: UsageInfo::default(),
                                            });
                                            message_started = true;
                                        }
                                        if open_blocks.insert(output_index) {
                                            yield Ok(StreamEvent::ContentBlockStart {
                                                index: output_index,
                                                content_block: claurst_core::types::ContentBlock::Text {
                                                    text: String::new(),
                                                },
                                            });
                                        }
                                    }
                                }
                            }
                            "response.output_text.delta" => {
                                let output_index = json_val
                                    .get("output_index")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0) as usize;
                                let delta = json_val
                                    .get("delta")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("");
                                if !message_started {
                                    yield Ok(StreamEvent::MessageStart {
                                        id: message_id.clone(),
                                        model: model_name.clone(),
                                        usage: UsageInfo::default(),
                                    });
                                    message_started = true;
                                }
                                if open_blocks.insert(output_index) {
                                    yield Ok(StreamEvent::ContentBlockStart {
                                        index: output_index,
                                        content_block: claurst_core::types::ContentBlock::Text {
                                            text: String::new(),
                                        },
                                    });
                                }
                                if !delta.is_empty() {
                                    yield Ok(StreamEvent::TextDelta {
                                        index: output_index,
                                        text: delta.to_string(),
                                    });
                                }
                            }
                            "response.function_call_arguments.delta" => {
                                let output_index = json_val
                                    .get("output_index")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0) as usize;
                                let delta = json_val
                                    .get("delta")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("");
                                if !delta.is_empty() {
                                    yield Ok(StreamEvent::InputJsonDelta {
                                        index: output_index,
                                        partial_json: delta.to_string(),
                                    });
                                }
                            }
                            "response.custom_tool_call_input.delta" => {
                                let output_index = json_val
                                    .get("output_index")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0) as usize;
                                if let (Some(buf), Some(delta)) =
                                    (custom_inputs.get_mut(&output_index), json_val["delta"].as_str())
                                {
                                    buf.push_str(delta);
                                }
                            }
                            "response.reasoning_summary_part.added" => {
                                // Separate consecutive summary parts like the
                                // official client's transcript does.
                                let summary_index = json_val
                                    .get("summary_index")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0);
                                if summary_index > 0 {
                                    let output_index = json_val
                                        .get("output_index")
                                        .and_then(|value| value.as_u64())
                                        .unwrap_or(0) as usize;
                                    if reasoning_items.insert(output_index) {
                                        yield Ok(StreamEvent::ContentBlockStart {
                                            index: output_index,
                                            content_block: claurst_core::types::ContentBlock::Thinking {
                                                thinking: String::new(),
                                                signature: String::new(),
                                            },
                                        });
                                    }
                                    yield Ok(StreamEvent::ThinkingDelta {
                                        index: output_index,
                                        thinking: "\n\n".to_string(),
                                    });
                                }
                            }
                            "response.reasoning_summary_text.delta" => {
                                let output_index = json_val
                                    .get("output_index")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0) as usize;
                                let delta = json_val
                                    .get("delta")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("");
                                if !message_started {
                                    yield Ok(StreamEvent::MessageStart {
                                        id: message_id.clone(),
                                        model: model_name.clone(),
                                        usage: UsageInfo::default(),
                                    });
                                    message_started = true;
                                }
                                // Each reasoning item's summary streams into
                                // its own block: open it on the first delta.
                                if reasoning_items.insert(output_index) {
                                    yield Ok(StreamEvent::ContentBlockStart {
                                        index: output_index,
                                        content_block: claurst_core::types::ContentBlock::Thinking {
                                            thinking: String::new(),
                                            signature: String::new(),
                                        },
                                    });
                                }
                                if !delta.is_empty() {
                                    yield Ok(StreamEvent::ThinkingDelta {
                                        index: output_index,
                                        thinking: delta.to_string(),
                                    });
                                }
                            }
                            "response.output_item.done" => {
                                let output_index = json_val
                                    .get("output_index")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0) as usize;
                                // A freeform call's input is raw text, not JSON:
                                // hand it over whole as the tool's `patch`.
                                if let Some(streamed) = custom_inputs.remove(&output_index) {
                                    let raw = json_val["item"]["input"].as_str().unwrap_or(&streamed);
                                    if open_blocks.contains(&output_index) {
                                        yield Ok(StreamEvent::InputJsonDelta {
                                            index: output_index,
                                            partial_json: json!({ "patch": raw }).to_string(),
                                        });
                                    }
                                }
                                if open_blocks.remove(&output_index) {
                                    yield Ok(StreamEvent::ContentBlockStop { index: output_index });
                                }
                                // A reasoning item's summary block closes first,
                                // keeping the (summary, encrypted_content) pair
                                // adjacent on the same output index.
                                if json_val.pointer("/item/type").and_then(Value::as_str) == Some("reasoning")
                                    && reasoning_items.remove(&output_index)
                                {
                                    yield Ok(StreamEvent::ContentBlockStop { index: output_index });
                                }
                                // Carry the reasoning item's encrypted state so
                                // the next (stateless, store:false) request can
                                // replay it and the model keeps its chain.
                                if let Some(data) = json_val
                                    .get("item")
                                    .filter(|item| item.get("type").and_then(|value| value.as_str()) == Some("reasoning"))
                                    .and_then(|item| item.get("encrypted_content"))
                                    .and_then(|value| value.as_str())
                                    .filter(|data| !data.is_empty())
                                {
                                    yield Ok(StreamEvent::ContentBlockStart {
                                        index: output_index,
                                        content_block: claurst_core::types::ContentBlock::RedactedThinking {
                                            data: data.to_string(),
                                        },
                                    });
                                    yield Ok(StreamEvent::ContentBlockStop { index: output_index });
                                }
                            }
                            "response.completed" => {
                                if let Some(response) = json_val.get("response") {
                                    if let Some(id) = response.get("id").and_then(|value| value.as_str()) {
                                        message_id = id.to_string();
                                    }
                                    if let Some(model) = response.get("model").and_then(|value| value.as_str()) {
                                        model_name = model.to_string();
                                    }

                                    if !message_started {
                                        yield Ok(StreamEvent::MessageStart {
                                            id: message_id.clone(),
                                            model: model_name.clone(),
                                            usage: UsageInfo::default(),
                                        });
                                        // No reassignment: this branch is the
                                        // stream-completion handler — execution
                                        // never re-enters the guard below.
                                    }

                                    // Reasoning blocks whose item never saw
                                    // `output_item.done` (defensive: a
                                    // truncated done event) still need a
                                    // closing stop event.
                                    let mut reasoning_left: Vec<usize> =
                                        reasoning_items.drain().collect();
                                    reasoning_left.sort_unstable();
                                    for index in reasoning_left {
                                        yield Ok(StreamEvent::ContentBlockStop { index });
                                    }

                                    let mut remaining: Vec<usize> = open_blocks.drain().collect();
                                    remaining.sort_unstable();
                                    for index in remaining {
                                        yield Ok(StreamEvent::ContentBlockStop { index });
                                    }

                                    let usage_json = response.get("usage");
                                    let usage = UsageInfo {
                                        input_tokens: usage_json
                                            .and_then(|value| value.get("input_tokens"))
                                            .and_then(|value| value.as_u64())
                                            .unwrap_or(0),
                                        output_tokens: usage_json
                                            .and_then(|value| value.get("output_tokens"))
                                            .and_then(|value| value.as_u64())
                                            .unwrap_or(0),
                                        cache_creation_input_tokens: 0,
                                        cache_read_input_tokens: usage_json
                                            .and_then(|value| value.get("input_tokens_details"))
                                            .and_then(|value| value.get("cached_tokens"))
                                            .and_then(|value| value.as_u64())
                                            .unwrap_or(0),
                                    };

                                    let stop_reason = if saw_tool_call {
                                        StopReason::ToolUse
                                    } else {
                                        match response
                                            .get("incomplete_details")
                                            .and_then(|value| value.get("reason"))
                                            .and_then(|value| value.as_str())
                                        {
                                            Some("max_output_tokens") => StopReason::MaxTokens,
                                            Some("content_filter") => StopReason::ContentFiltered,
                                            Some(other) if !other.is_empty() => StopReason::Other(other.to_string()),
                                            _ => StopReason::EndTurn,
                                        }
                                    };

                                    yield Ok(StreamEvent::MessageDelta {
                                        stop_reason: Some(stop_reason),
                                        usage: Some(usage),
                                    });
                                    yield Ok(StreamEvent::MessageStop);
                                    return;
                                }
                            }
                            "response.failed" | "response.error" | "error" => {
                                yield Err(ProviderError::StreamError {
                                    provider: provider_id.clone(),
                                    message: Self::extract_stream_error_message(&json_val),
                                    partial_response: Some(trimmed.to_string()),
                                });
                                return;
                            }
                            _ => {}
                        }

                        current_event.clear();
                        continue;
                    }

                    if let Some(rest) = line.strip_prefix("event:") {
                        current_event = rest.trim().to_string();
                        continue;
                    }

                    if let Some(rest) = line.strip_prefix("data:") {
                        current_data.push(rest.trim_start().to_string());
                    }
                }
            }

            yield Err(ProviderError::StreamError {
                provider: provider_id,
                message: "Codex stream ended before response.completed".to_string(),
                partial_response: None,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// LlmProvider impl
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmProvider for CodexProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "OpenAI Codex"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        self.send_responses_request(&request).await
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let resp = self.send_responses_streaming_request(&request).await?;
        let rate_limits = Self::parse_rate_limits(resp.headers()).map(StreamEvent::from);
        Ok(Box::pin(Self::sse_events(
            resp.bytes_stream(),
            self.id.clone(),
            rate_limits,
        )))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        use claurst_core::codex_oauth::codex_limit_override;

        // Live catalog first (issue #462 item 1): the ChatGPT backend filters
        // by the pinned client version, and `context_window` /
        // `supported_reasoning_levels` / `priority` come straight from the
        // response. Fall back to the static builtin list when the fetch
        // fails or yields nothing usable — the picker must never go empty.
        let live = match self.fetch_catalog().await {
            Ok(models) => Some(models),
            Err(err) => {
                debug!(error = %err, "Codex live model catalog unavailable — using the builtin list");
                None
            }
        };

        if let Some(models) = live {
            return Ok(models
                .iter()
                .map(|entry| {
                    let context_window = entry.context_window.map(|c| c as u32).unwrap_or(400_000);
                    let max_output_tokens = codex_limit_override(&entry.slug)
                        .map(|(_, _, out)| out)
                        .unwrap_or(128_000);
                    ModelInfo {
                        id: ModelId::new(entry.slug.clone()),
                        provider_id: self.id.clone(),
                        name: entry
                            .display_name
                            .clone()
                            .unwrap_or_else(|| entry.slug.clone()),
                        context_window,
                        max_output_tokens,
                        ..Default::default()
                    }
                })
                .collect());
        }

        let models = CODEX_MODELS
            .iter()
            .map(|(id, name)| {
                let (context_window, max_output_tokens) = codex_limit_override(id)
                    .map(|(ctx, _, out)| (ctx, out))
                    .unwrap_or((400_000, 128_000));
                ModelInfo {
                    id: ModelId::new(*id),
                    provider_id: self.id.clone(),
                    name: name.to_string(),
                    context_window,
                    max_output_tokens,
                    ..Default::default()
                }
            })
            .collect();
        Ok(models)
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        // Validate that a non-expired token exists without making a network call.
        let guard = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.access_token.is_empty() {
            return Ok(ProviderStatus::Unavailable {
                reason: "no Codex access token — run /connect to authenticate".to_string(),
            });
        }
        if Self::is_expired(&guard, unix_now()) && guard.refresh_token.is_none() {
            return Ok(ProviderStatus::Unavailable {
                reason: "Codex access token expired and no refresh token — re-run /connect"
                    .to_string(),
            });
        }
        Ok(ProviderStatus::Healthy)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            thinking: false,
            image_input: false,
            pdf_input: false,
            audio_input: false,
            video_input: false,
            caching: false,
            structured_output: false,
            system_prompt_style: SystemPromptStyle::SystemMessage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn sse_line(event: &str, payload: &Value) -> String {
        format!("event: {event}\ndata: {payload}\n\n")
    }

    /// Two reasoning items interleaved with a function call must stream as
    /// well-formed per-item blocks — (summary, encrypted_content) pair per
    /// `output_index`, in wire order relative to the function call — instead
    /// of one joined Thinking blob followed by detached redacted blocks.
    #[tokio::test]
    async fn sse_reasoning_items_stay_paired_per_output_index() {
        use claurst_core::types::ContentBlock;

        let events = [
            sse_line("response.created", &json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5"}})),
            sse_line("response.reasoning_summary_text.delta", &json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"first item"})),
            sse_line("response.output_item.added", &json!({"type":"response.output_item.added","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"Bash"}})),
            sse_line("response.function_call_arguments.delta", &json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":r#"{"command":"ls"}"#})),
            sse_line("response.output_item.done", &json!({"type":"response.output_item.done","output_index":0,"item":{"id":"rs_0","type":"reasoning","encrypted_content":"ENC-1"}})),
            sse_line("response.output_item.done", &json!({"type":"response.output_item.done","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"Bash","arguments":r#"{"command":"ls"}"#}})),
            sse_line("response.reasoning_summary_text.delta", &json!({"type":"response.reasoning_summary_text.delta","output_index":2,"delta":"second item"})),
            sse_line("response.output_item.done", &json!({"type":"response.output_item.done","output_index":2,"item":{"id":"rs_2","type":"reasoning","encrypted_content":"ENC-2"}})),
            sse_line("response.completed", &json!({"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.5","usage":{"input_tokens":10,"output_tokens":5}}})),
        ]
        .join("");
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> = vec![Ok(events.into_bytes())];
        let events: Vec<StreamEvent> = CodexProvider::sse_events(
            futures::stream::iter(chunks),
            ProviderId::new(ProviderId::CODEX),
            None,
        )
        .map(|event| event.expect("stream has no errors"))
        .collect()
        .await;

        // The per-item pairing: for each reasoning item, Thinking start →
        // ThinkingDelta → Thinking stop → encrypted RedactedThinking, all at
        // the item's output index.
        let position = |probe: &dyn Fn(&StreamEvent) -> bool| -> Option<usize> {
            events.iter().position(probe)
        };
        for (index, delta_text, encrypted) in [
            (0usize, "first item", "ENC-1"),
            (2usize, "second item", "ENC-2"),
        ] {
            let probe = |event: &StreamEvent| matches!(event,
                StreamEvent::ContentBlockStart { index: i, content_block: ContentBlock::Thinking { .. } } if *i == index);
            let start = position(&probe).unwrap_or_else(|| panic!("item {index}: Thinking block must start"));
            let delta = events[start..]
                .iter()
                .position(|event| matches!(event, StreamEvent::ThinkingDelta { index: i, thinking } if *i == index && thinking == delta_text))
                .map(|p| start + p)
                .expect("item deltas after the block start");
            let stop = events[delta..]
                .iter()
                .position(|event| matches!(event, StreamEvent::ContentBlockStop { index: i } if *i == index))
                .map(|p| delta + p)
                .expect("item summary block must stop before the encrypted state");
            let redacted_start = events[stop..]
                .iter()
                .position(|event| matches!(event,
                    StreamEvent::ContentBlockStart { index: i, content_block: ContentBlock::RedactedThinking { data } }
                    if *i == index && data == encrypted))
                .map(|p| stop + p)
                .expect("item's encrypted state must pair with its summary");
            assert!(matches!(
                &events[redacted_start + 1],
                StreamEvent::ContentBlockStop { index: i } if *i == index,
            ));
        }

        // Wire order relative to the function call: item 0's encrypted state
        // precedes the tool call; item 2's summary block opens after it.
        let tool_start = events
            .iter()
            .position(|event| matches!(event,
                StreamEvent::ContentBlockStart { index: 1, content_block: ContentBlock::ToolUse { .. } }))
            .expect("function call block must start");
        let enc1 = events.iter().position(|event| matches!(event,
            StreamEvent::ContentBlockStart { content_block: ContentBlock::RedactedThinking { data }, .. } if data == "ENC-1"))
            .expect("item 0 encrypted state present");
        assert!(enc1 < tool_start, "item 0's pair must complete before the tool call");
        let thinking2 = events.iter().position(|event| matches!(event,
            StreamEvent::ContentBlockStart { index: 2, content_block: ContentBlock::Thinking { .. } }))
            .expect("item 2 Thinking start present");
        assert!(thinking2 > tool_start, "item 2 must stream after the tool call");
    }

    fn jwt(payload: Value) -> String {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string());
        format!("h.{b64}.s")
    }

    fn request(system: Option<&str>) -> ProviderRequest {
        ProviderRequest {
            model: "gpt-5.5".to_string(),
            messages: vec![claurst_core::types::Message::user("hi")],
            system_prompt: system
                .map(|text| crate::provider_types::SystemPrompt::Text(text.to_string())),
            tools: Vec::new(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            provider_options: json!({}),
        }
    }

    #[test]
    fn body_sends_system_prompt_once_as_developer_message() {
        let body = CodexProvider::build_responses_body(&request(Some("SYSTEM PROMPT")));
        assert!(body.get("instructions").is_none());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[0]["content"][0]["text"], "SYSTEM PROMPT");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(body.to_string().matches("SYSTEM PROMPT").count(), 1);

        let body = CodexProvider::build_responses_body(&request(None));
        assert_eq!(body["input"][0]["role"], "user");
    }

    #[test]
    fn prompt_cache_key_option_reaches_body() {
        let mut req = request(None);
        req.provider_options = json!({ "prompt_cache_key": "session-abc" });
        let body = CodexProvider::build_responses_body(&req);
        assert_eq!(body["prompt_cache_key"], "session-abc");
    }

    #[tokio::test]
    async fn sse_fixture_yields_reasoning_summary_encrypted_state_and_tool_call() {
        use claurst_core::types::ContentBlock;
        let fixture = include_str!("fixtures/codex_reasoning_tool_call.sse");
        // Odd-sized chunks exercise the partial-line carry-over.
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> = fixture
            .as_bytes()
            .chunks(37)
            .map(|chunk| Ok(chunk.to_vec()))
            .collect();
        let rate_limits = Some(StreamEvent::RateLimits {
            primary_used_percent: Some(10.0),
            secondary_used_percent: None,
        });
        let events: Vec<StreamEvent> = CodexProvider::sse_events(
            futures::stream::iter(chunks),
            ProviderId::new(ProviderId::CODEX),
            rate_limits,
        )
        .map(|event| event.expect("fixture stream has no errors"))
        .collect()
        .await;

        assert!(matches!(events.first(), Some(StreamEvent::RateLimits { .. })));
        assert!(matches!(events.last(), Some(StreamEvent::MessageStop)));

        let thinking: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ThinkingDelta { thinking, .. } => Some(thinking.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "**Inspecting the repo**\n\nListing files first.");

        let encrypted: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ContentBlockStart {
                    content_block: ContentBlock::RedactedThinking { data },
                    ..
                } => Some(data.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(encrypted, ["gAAAAABo-ENCRYPTED-REASONING-STATE=="]);

        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlock::ToolUse { id, name, .. },
            } if id == "call_42" && name == "Bash"
        )));
        let args: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::InputJsonDelta { partial_json, .. } => Some(partial_json.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(args, r#"{"command":"ls"}"#);

        let Some(StreamEvent::MessageDelta {
            stop_reason: Some(StopReason::ToolUse),
            usage: Some(usage),
        }) = events.iter().rev().nth(1)
        else {
            panic!("expected a tool-use MessageDelta before MessageStop");
        };
        assert_eq!(usage.input_tokens, 1200);
        assert_eq!(usage.cache_read_input_tokens, 1024);
    }

    #[test]
    fn body_replays_encrypted_reasoning_and_drops_summary_only_items() {
        use claurst_core::types::{ContentBlock, Message, ToolResultContent};
        let mut req = request(None);
        req.messages = vec![
            Message::user("list files"),
            Message::assistant_blocks(vec![
                ContentBlock::Thinking {
                    thinking: "Listing files first.".into(),
                    signature: String::new(),
                },
                ContentBlock::RedactedThinking {
                    data: "ENC-1".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_42".into(),
                    name: "Bash".into(),
                    input: json!({ "command": "ls" }),
                    thought_signature: None,
                },
            ]),
            Message::user_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_42".into(),
                content: ToolResultContent::Text("a.rs".into()),
                is_error: None,
            }]),
            Message::assistant_blocks(vec![
                ContentBlock::Thinking {
                    thinking: "summary without state".into(),
                    signature: String::new(),
                },
                ContentBlock::Text {
                    text: "done".into(),
                },
            ]),
        ];
        let body = CodexProvider::build_responses_body(&req);
        let input = body["input"].as_array().unwrap();
        let types: Vec<&str> = input
            .iter()
            .map(|item| item["type"].as_str().or(item["role"].as_str()).unwrap_or(""))
            .collect();
        assert_eq!(types, ["user", "reasoning", "function_call", "function_call_output", "assistant"]);
        assert_eq!(input[1]["encrypted_content"], "ENC-1");
        assert_eq!(input[1]["summary"][0]["text"], "Listing files first.");
    }

    #[test]
    fn codex_models_url_pins_the_catalog_client_version() {
        let url = codex_models_url();
        assert_eq!(
            url,
            format!(
                "https://chatgpt.com/backend-api/codex/models?client_version={CODEX_CATALOG_CLIENT_VERSION}"
            )
        );
        assert_eq!(CODEX_CATALOG_CLIENT_VERSION, "0.124.0");
    }

    /// The live catalog (`GET .../codex/models?client_version=X`, canned
    /// response): entries the official picker lists (supported_in_api,
    /// visibility == "list", not `use_responses_lite`) survive, and the
    /// registry-relevant fields (context window, reasoning levels, priority)
    /// come straight from the response.
    #[test]
    fn catalog_filter_and_field_mapping_match_the_canned_response() {
        use claurst_core::codex_oauth::codex_limit_override;

        let json_val: Value =
            serde_json::from_str(include_str!("fixtures/codex_catalog.json")).unwrap();
        let models = parse_codex_catalog(&json_val);

        let slugs: Vec<&str> = models.iter().map(|m| m.slug.as_str()).collect();
        assert_eq!(
            slugs,
            ["gpt-5.5", "gpt-5.3-codex-spark"],
            "lite / non-listed / non-API models must be filtered out"
        );

        let gpt55 = &models[0];
        assert_eq!(gpt55.display_name.as_deref(), Some("GPT-5.5 (default)"));
        // Context window and the gpt-5.5 output override come from the
        // catalog / limit table, not the (missing) models.dev row.
        assert_eq!(gpt55.context_window, Some(400_000));
        assert_eq!(
            codex_limit_override("gpt-5.5").map(|(_, _, out)| out),
            Some(128_000)
        );
        assert_eq!(gpt55.priority, Some(3));
        let efforts: Vec<&str> =
            gpt55.supported_reasoning_levels.iter().map(|l| l.effort.as_str()).collect();
        assert_eq!(efforts, ["minimal", "low", "medium", "high", "xhigh"]);
    }

    /// Serializes tests that swap the process-wide `LIVE_CATALOG` registry:
    /// the registry is shared state and the suite may run multi-threaded.
    static CATALOG_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Snapshot/restore helper for `CATALOG_TEST_LOCK` holders: swaps the
    /// process-wide catalog for `models`, restoring the previous contents
    /// when the returned guard is dropped.
    fn with_live_catalog(models: Vec<CodexCatalogModel>) -> impl Drop {
        struct Restore(Option<Vec<CodexCatalogModel>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                let restore = self.0.take();
                set_live_catalog(restore.unwrap_or_default());
            }
        }
        let saved = LIVE_CATALOG
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_deref()
            .map(|catalog| catalog.to_vec());
        set_live_catalog(models);
        Restore(saved)
    }

    #[test]
    fn catalog_lookup_accepts_prefixed_ids_and_misses_unknown_models() {
        let _guard = CATALOG_TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let _catalog = with_live_catalog(parse_codex_catalog(
            &serde_json::from_str(include_str!("fixtures/codex_catalog.json")).unwrap(),
        ));
        assert!(codex_catalog_lookup("gpt-5.5").is_some());
        assert!(codex_catalog_lookup("codex/gpt-5.5").is_some());
        assert!(codex_catalog_lookup("gpt-6-lens").is_none());
    }

    /// The `include: reasoning.encrypted_content` option the query layer adds
    /// on a gpt-5* id-heuristic must not reach the wire for models the live
    /// catalog lists with no reasoning levels; models the catalog does not
    /// list keep the heuristic fallback.
    #[test]
    fn include_gate_reads_catalog_reasoning_levels() {
        let _guard = CATALOG_TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);

        let options = |model: &str| -> ProviderRequest {
            let mut req = request(None);
            req.model = model.to_string();
            req.provider_options = json!({
                "reasoningEffort": "medium",
                "reasoningSummary": "auto",
                "include": ["reasoning.encrypted_content"],
            });
            req
        };

        let _catalog = with_live_catalog(vec![
            CodexCatalogModel {
                slug: "gpt-5-no-effort".to_string(),
                visibility: Some("list".to_string()),
                supported_in_api: true,
                ..CodexCatalogModel::default()
            }, // The reasoning-bearing listed entry keeps its include gate.
            CodexCatalogModel {
                slug: "gpt-5-with-effort".to_string(),
                visibility: Some("list".to_string()),
                supported_in_api: true,
                supported_reasoning_levels: vec![CodexReasoningLevel {
                    effort: "high".to_string(),
                }],
                ..CodexCatalogModel::default()
            },
        ]);

        let body = CodexProvider::build_responses_body(&options("gpt-5-no-effort"));
        assert_eq!(body["model"], "gpt-5-no-effort");
        assert!(body.get("include").is_none(), "non-reasoning model must not request encrypted reasoning: {body}");
        // Reasoning options themselves are untouched.
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["reasoning"]["effort"], "medium");

        let body = CodexProvider::build_responses_body(&options("gpt-5-with-effort"));
        assert_eq!(
            body["include"],
            json!(["reasoning.encrypted_content"]),
            "reasoning-capable catalog entries keep the include"
        );

        // Not in the catalog: heuristic fallback keeps the include.
        let body = CodexProvider::build_responses_body(&options("gpt-99-unknown"));
        assert_eq!(
            body["include"],
            json!(["reasoning.encrypted_content"]),
            "catalog-unknown models fall back to the id heuristic"
        );

        // A `provider/`-prefixed id resolves through the catalog too.
        let body = CodexProvider::build_responses_body(&options("openai/gpt-5-no-effort"));
        assert!(body.get("include").is_none());
    }

    fn tool(name: &str) -> claurst_core::types::ToolDefinition {
        claurst_core::types::ToolDefinition {
            name: name.to_string(),
            description: format!("{name} tool"),
            input_schema: json!({ "type": "object" }),
        }
    }

    /// gpt-5 and later get apply_patch as the freeform tool the official
    /// client sends; other tools, and other models, stay JSON functions.
    #[test]
    fn apply_patch_is_a_freeform_tool_for_gpt5_models() {
        let mut req = request(None);
        req.tools = vec![tool("Bash"), tool(TOOL_NAME_APPLY_PATCH)];
        let body = CodexProvider::build_responses_body(&req);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "Bash");
        assert_eq!(tools[1]["type"], "custom");
        assert_eq!(tools[1]["name"], "apply_patch");
        assert_eq!(tools[1]["format"]["type"], "grammar");
        assert_eq!(tools[1]["format"]["syntax"], "lark");
        assert!(tools[1]["format"]["definition"]
            .as_str()
            .unwrap()
            .starts_with("start: begin_patch hunk+ end_patch"));
        assert!(tools[1].get("parameters").is_none());

        req.model = "gpt-4.1".to_string();
        let body = CodexProvider::build_responses_body(&req);
        assert_eq!(body["tools"][1]["type"], "function");
        assert_eq!(body["tools"][1]["name"], TOOL_NAME_APPLY_PATCH);

        assert!(uses_freeform_apply_patch("gpt-5.3-codex-spark"));
        assert!(uses_freeform_apply_patch("openai/gpt-6.1-sol"));
        assert!(!uses_freeform_apply_patch("o3"));
    }

    /// An earlier ApplyPatch call is replayed as the custom tool call (and
    /// output) the declared freeform tool produces.
    #[test]
    fn apply_patch_history_replays_as_custom_tool_items() {
        use claurst_core::types::{ContentBlock, Message, ToolResultContent};
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch";
        let mut req = request(None);
        req.tools = vec![tool(TOOL_NAME_APPLY_PATCH)];
        req.messages = vec![
            Message::user("add a file"),
            Message::assistant_blocks(vec![ContentBlock::ToolUse {
                id: "call_7".into(),
                name: TOOL_NAME_APPLY_PATCH.into(),
                input: json!({ "patch": patch }),
                thought_signature: None,
            }]),
            Message::user_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_7".into(),
                content: ToolResultContent::Text("Success. Updated the following files:\nA a.txt\n".into()),
                is_error: None,
            }]),
        ];
        let body = CodexProvider::build_responses_body(&req);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[1]["type"], "custom_tool_call");
        assert_eq!(input[1]["call_id"], "call_7");
        assert_eq!(input[1]["name"], "apply_patch");
        assert_eq!(input[1]["input"], patch);
        assert_eq!(input[2]["type"], "custom_tool_call_output");
        assert_eq!(input[2]["call_id"], "call_7");
    }

    /// A streamed `custom_tool_call` becomes an ApplyPatch tool use whose
    /// input is `{"patch": <raw text>}`.
    #[tokio::test]
    async fn sse_custom_tool_call_becomes_apply_patch_tool_use() {
        use claurst_core::types::ContentBlock;
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch";
        let events_text = [
            json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_9","name":"apply_patch","input":""}}),
            json!({"type":"response.custom_tool_call_input.delta","output_index":0,"item_id":"ctc_1","delta":&patch[..20]}),
            json!({"type":"response.custom_tool_call_input.delta","output_index":0,"item_id":"ctc_1","delta":&patch[20..]}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_9","name":"apply_patch","input":patch}}),
            json!({"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.5","usage":{"input_tokens":10,"output_tokens":5}}}),
        ]
        .iter()
        .map(|event| format!("event: {}\ndata: {}\n\n", event["type"].as_str().unwrap(), event))
        .collect::<String>();
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> = vec![Ok(events_text.into_bytes())];
        let events: Vec<StreamEvent> = CodexProvider::sse_events(
            futures::stream::iter(chunks),
            ProviderId::new(ProviderId::CODEX),
            None,
        )
        .map(|event| event.expect("stream has no errors"))
        .collect()
        .await;

        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::ToolUse { id, name, .. },
            } if id == "call_9" && name == TOOL_NAME_APPLY_PATCH
        )));
        let args: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::InputJsonDelta { partial_json, .. } => Some(partial_json.as_str()),
                _ => None,
            })
            .collect();
        let args: Value = serde_json::from_str(&args).unwrap();
        assert_eq!(args, json!({ "patch": patch }));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::MessageDelta { stop_reason: Some(StopReason::ToolUse), .. }
        )));

        let response = json!({
            "id": "resp_1",
            "model": "gpt-5.5",
            "output": [{"type":"custom_tool_call","call_id":"call_9","name":"apply_patch","input":patch}],
        });
        let parsed =
            CodexProvider::parse_responses_response(&ProviderId::new(ProviderId::CODEX), &response).unwrap();
        assert!(matches!(parsed.stop_reason, StopReason::ToolUse));
        assert!(matches!(
            &parsed.content[0],
            ContentBlock::ToolUse { name, input, .. }
                if name == TOOL_NAME_APPLY_PATCH && input == &json!({ "patch": patch })
        ));
    }

    #[test]
    fn parses_codex_rate_limit_headers() {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        headers.insert("x-codex-primary-used-percent", HeaderValue::from_static("12.5"));
        headers.insert("x-codex-primary-window-minutes", HeaderValue::from_static("300"));
        headers.insert("x-codex-secondary-used-percent", HeaderValue::from_static("71"));
        let usage = CodexProvider::parse_rate_limits(&headers).expect("both headers present");
        assert_eq!(usage.primary_used_percent, Some(12.5));
        assert_eq!(usage.secondary_used_percent, Some(71.0));
        // The usage converts to the stream event the streaming path emits.
        assert!(matches!(
            StreamEvent::from(usage.clone()),
            StreamEvent::RateLimits {
                primary_used_percent,
                secondary_used_percent,
            }
            if primary_used_percent == Some(12.5) && secondary_used_percent == Some(71.0)
        ));

        let mut partial = HeaderMap::new();
        partial.insert("x-codex-secondary-used-percent", HeaderValue::from_static("garbage"));
        assert!(CodexProvider::parse_rate_limits(&partial).is_none());
        assert!(CodexProvider::parse_rate_limits(&HeaderMap::new()).is_none());
    }

    #[test]
    fn new_derives_account_id_from_access_token_claim() {
        let provider = CodexProvider::new(CodexTokens {
            access_token: jwt(json!({
                "https://api.openai.com/auth": { "chatgpt_account_id": "ws_123" }
            })),
            ..Default::default()
        });
        assert_eq!(provider.account_id().as_deref(), Some("ws_123"));
    }

    #[tokio::test]
    async fn access_token_adopts_fresh_tokens_refreshed_by_another_instance() {
        let home = std::env::temp_dir().join(format!("claurst-codex-{}", uuid::Uuid::new_v4()));
        let prev_home = std::env::var_os("CLAURST_HOME");
        std::env::set_var("CLAURST_HOME", &home);

        let stale = CodexProvider::new(CodexTokens {
            access_token: jwt(json!({ "exp": 1 })),
            refresh_token: Some("spent-refresh-token".into()),
            ..Default::default()
        });
        let fresh_access = jwt(json!({ "exp": unix_now() + 3_600 }));
        let saved = claurst_core::oauth_config::save_codex_tokens_and_register(
            &CodexTokens {
                access_token: fresh_access.clone(),
                refresh_token: Some("new-refresh-token".into()),
                ..Default::default()
            },
            None,
        );
        // No network: the expired in-memory token must be replaced by the
        // one on disk without POSTing the spent refresh token.
        let token = stale.access_token().await;

        match prev_home {
            Some(v) => std::env::set_var("CLAURST_HOME", v),
            None => std::env::remove_var("CLAURST_HOME"),
        }
        let _ = std::fs::remove_dir_all(&home);

        saved.unwrap();
        assert_eq!(token.unwrap(), fresh_access);
        assert_eq!(
            stale.tokens.lock().unwrap().refresh_token.as_deref(),
            Some("new-refresh-token")
        );
    }

    #[test]
    fn classifies_permanent_refresh_failures() {
        assert!(is_permanent_refresh_failure(400, r#"{"error":"invalid_grant"}"#));
        assert!(is_permanent_refresh_failure(
            400,
            r#"{"error":{"code":"refresh_token_reused","message":"x"}}"#
        ));
        assert!(is_permanent_refresh_failure(401, ""));
        assert!(!is_permanent_refresh_failure(500, r#"{"error":"server_error"}"#));
        assert!(!is_permanent_refresh_failure(400, "not json"));
    }

    #[test]
    fn is_expired_uses_jwt_exp_with_five_minute_window() {
        let tokens = CodexTokens {
            access_token: jwt(json!({ "exp": 10_000 })),
            ..Default::default()
        };
        assert!(!CodexProvider::is_expired(&tokens, 10_000 - REFRESH_WINDOW_SECS - 1));
        assert!(CodexProvider::is_expired(&tokens, 10_000 - REFRESH_WINDOW_SECS));
        assert!(CodexProvider::is_expired(&tokens, 20_000));
    }

    #[test]
    fn is_expired_falls_back_to_stored_expiry_then_valid() {
        let stored = CodexTokens {
            access_token: "opaque".into(),
            expires_at: Some(1_000),
            ..Default::default()
        };
        assert!(CodexProvider::is_expired(&stored, 900));
        let unknown = CodexTokens {
            access_token: "opaque".into(),
            ..Default::default()
        };
        assert!(!CodexProvider::is_expired(&unknown, u64::MAX - 1));
    }
}
