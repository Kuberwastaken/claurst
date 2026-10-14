// Compaction check points for run_query_loop (issue #441): the pre-request
// threshold check and compact-and-retry after a context-window error.

use crate::*;

/// Wrap the loop's Anthropic client as an [`claurst_api::LlmProvider`] so the
/// raw-client branch summarises through the same provider-agnostic path as
/// every other provider. Built only when a compaction is about to run.
pub(crate) fn anthropic_summariser(
    client: &claurst_api::AnthropicClient,
) -> claurst_api::AnthropicProvider {
    claurst_api::AnthropicProvider::new(Arc::new(client.clone()))
}

/// The provider and model compaction summarises through for this request:
/// the routed provider, or the raw Anthropic client wrapped as a provider.
/// `None` when the routed provider has no credentials (the request itself
/// fails with a clear error in that case).
fn compaction_endpoint(
    route: Option<&ProviderRoute>,
    client: &claurst_api::AnthropicClient,
    effective_model: &str,
) -> Option<(Arc<dyn claurst_api::LlmProvider>, String)> {
    match route {
        Some(route) if route.use_provider_dispatch => route
            .provider
            .clone()
            .map(|provider| (provider, route.model_id.clone())),
        _ => Some((Arc::new(anthropic_summariser(client)), effective_model.to_string())),
    }
}

/// `true` when `err` reports that the request exceeded the context window.
/// Rate limits never count, even when their message mentions tokens.
pub(crate) fn provider_error_is_overflow(err: &claurst_api::ProviderError) -> bool {
    match err {
        claurst_api::ProviderError::ContextOverflow { .. } => true,
        claurst_api::ProviderError::RateLimited { .. } => false,
        _ => claurst_api::is_context_overflow(&err.to_string()),
    }
}

/// `true` when `err` reports that the request exceeded the context window.
pub(crate) fn claude_error_is_overflow(err: &ClaudeError) -> bool {
    match err {
        ClaudeError::ContextWindowExceeded => true,
        ClaudeError::RateLimit => false,
        _ if claurst_api::retry::classify_claude_error(err).is_retry() => false,
        _ => claurst_api::is_context_overflow(&err.to_string()),
    }
}

/// Pre-request check point: compact when the projected context crosses the
/// threshold. Returns `true` when the history was replaced.
///
/// With the `reactive_compact` feature gate the 90 % / 97 % reactive
/// compact / context-collapse ladder runs instead of the proactive path.
pub(crate) async fn compact_before_request(
    messages: &mut Vec<Message>,
    route: Option<&ProviderRoute>,
    client: &claurst_api::AnthropicClient,
    effective_model: &str,
    context_tokens: u64,
    context_window: u64,
    max_output_tokens: u32,
    state: &mut compact::AutoCompactState,
    event_tx: Option<&mpsc::UnboundedSender<QueryEvent>>,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> bool {
    let send_status = |text: &str| {
        if let Some(tx) = event_tx {
            let _ = tx.send(QueryEvent::Status(text.to_string()));
        }
    };

    if claurst_core::feature_gates::is_feature_enabled("reactive_compact") {
        let collapse = compact::should_context_collapse(context_tokens, context_window);
        if !collapse && !compact::should_compact(context_tokens, context_window) {
            return false;
        }
        let Some((provider, model)) = compaction_endpoint(route, client, effective_model) else {
            return false;
        };
        // Pass a clone so the live conversation survives a failed
        // compaction; `*messages` is only overwritten on success (#213).
        let outcome = if collapse {
            send_status("Compacting context... (emergency collapse)");
            compact::context_collapse(messages.clone(), provider.as_ref(), &model).await
        } else {
            send_status("Compacting context...");
            compact::reactive_compact(
                messages.clone(),
                provider.as_ref(),
                &model,
                cancel_token.clone(),
                &[],
            )
            .await
        };
        return match apply_compact_result(messages, outcome) {
            Ok(tokens_freed) => {
                info!(tokens_freed, collapse, "Reactive compact complete");
                true
            }
            Err(e) => {
                warn!(error = %e, "Reactive compact failed; conversation preserved");
                false
            }
        };
    }

    if !compact::should_auto_compact_for_window(
        context_tokens,
        context_window,
        u64::from(max_output_tokens),
        state,
    ) {
        return false;
    }
    let Some((provider, model)) = compaction_endpoint(route, client, effective_model) else {
        return false;
    };
    send_status("Compacting context...");
    match compact::auto_compact_if_needed(
        provider.as_ref(),
        &model,
        messages,
        context_tokens,
        context_window,
        max_output_tokens,
        state,
    )
    .await
    {
        Some(compaction) => {
            apply_compaction(messages, compaction, event_tx);
            true
        }
        None => false,
    }
}

/// Overflow recovery: the provider rejected the request for exceeding the
/// context window, so compact once before the caller retries. Returns `true`
/// when the history was compacted.
pub(crate) async fn compact_after_overflow(
    messages: &mut Vec<Message>,
    provider: &dyn claurst_api::LlmProvider,
    model: &str,
    max_output_tokens: u32,
    event_tx: Option<&mpsc::UnboundedSender<QueryEvent>>,
) -> bool {
    if let Some(tx) = event_tx {
        let _ = tx.send(QueryEvent::Status(
            "Context window exceeded — compacting and retrying…".to_string(),
        ));
    }
    match compact::compact_conversation(
        provider,
        model,
        messages,
        compact::CompactionTrigger::Overflow,
        compact::summary_token_budget(max_output_tokens),
        None,
    )
    .await
    {
        Ok(Some(compaction)) => {
            info!(
                before = messages.len(),
                after = compaction.messages.len(),
                "Compacted after a context-window error; retrying the request"
            );
            apply_compaction(messages, compaction, event_tx);
            true
        }
        Ok(None) => {
            warn!("Context-window error but nothing to compact; surfacing the error");
            false
        }
        Err(e) => {
            warn!(error = %e, "Compaction after a context-window error failed");
            false
        }
    }
}

/// Replace the history with a compaction's result and report it: a status
/// line with the token counts and a [`QueryEvent::Compacted`] entry for the
/// caller to record in the session.
fn apply_compaction(
    messages: &mut Vec<Message>,
    compaction: compact::Compaction,
    event_tx: Option<&mpsc::UnboundedSender<QueryEvent>>,
) {
    *messages = compaction.messages;
    if let Some(tx) = event_tx {
        let record = compaction.record;
        let verb = if record.summarised {
            "Context compacted"
        } else {
            "Context trimmed without summary"
        };
        let _ = tx.send(QueryEvent::Status(format!(
            "{}: ~{} → ~{} tokens.",
            verb, record.tokens_before, record.tokens_after
        )));
        let _ = tx.send(QueryEvent::Compacted(record));
    }
}
