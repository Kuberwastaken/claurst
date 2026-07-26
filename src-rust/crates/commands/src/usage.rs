// Usage-reporting commands: `/usage` and `/extra-usage`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct UsageCommand;
pub struct ExtraUsageCommand;

// ---- /usage --------------------------------------------------------------

#[async_trait]
impl SlashCommand for UsageCommand {
    fn name(&self) -> &str { "usage" }
    fn description(&self) -> &str { "Show API usage, quotas, and rate limit status" }
    fn help(&self) -> &str {
        "Usage: /usage\n\n\
         Shows current session API usage and account quota information.\n\
         For detailed per-call breakdown, use /extra-usage.\n\
         For cost details, use /cost."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let input = ctx.cost_tracker.input_tokens();
        let output = ctx.cost_tracker.output_tokens();
        let cache_creation = ctx.cost_tracker.cache_creation_tokens();
        let cache_read = ctx.cost_tracker.cache_read_tokens();
        let total = ctx.cost_tracker.total_tokens();
        let cost = ctx.cost_tracker.total_cost_usd();

        // Some providers (notably local OpenAI-compatible servers that don't
        // report `prompt_tokens_details.cached_tokens`) never surface cache
        // usage. Show "n/a" rather than a permanent 0, which reads as if
        // caching were broken. See docs/local-models.md.
        let cache_creation_disp = display_cache_count(cache_creation, cache_read);
        let cache_read_disp = display_cache_count(cache_read, cache_creation);

        // Try to get account tier from OAuth tokens
        let account_info = match claurst_core::oauth::OAuthTokens::load().await {
            Some(tokens) => {
                let sub = tokens.subscription_type.as_deref().unwrap_or("unknown");
                format!("Plan: {}", sub)
            }
            None => {
                if ctx.config.resolve_api_key().is_some() {
                    "Plan: API key (Console billing)".to_string()
                } else {
                    "Plan: not authenticated — run /login".to_string()
                }
            }
        };

        CommandResult::Message(format!(
            "API Usage — Current Session\n\
             ────────────────────────────\n\
             {account_info}\n\
             Model:          {model}\n\n\
             Tokens used this session:\n\
               Input:        {input:>10}\n\
               Output:       {output:>10}\n\
               Cache write:  {cache_creation:>10}\n\
               Cache read:   {cache_read:>10}\n\
               Total:        {total:>10}\n\n\
             Estimated cost: ${cost:.4}\n\n\
             Use /extra-usage for per-call breakdown.\n\
             Use /rate-limit-options to see your plan limits.",
            account_info = account_info,
            model = ctx.config.effective_model(),
            input = input,
            output = output,
            cache_creation = cache_creation_disp,
            cache_read = cache_read_disp,
            total = total,
            cost = cost,
        ))
    }
}

// ---- /extra-usage --------------------------------------------------------

#[async_trait]
impl SlashCommand for ExtraUsageCommand {
    fn name(&self) -> &str { "extra-usage" }
    fn description(&self) -> &str { "Show detailed usage statistics: calls, cache, tools" }
    fn help(&self) -> &str {
        "Usage: /extra-usage\n\n\
         Displays extended usage statistics beyond /cost:\n\
         - API call count\n\
         - Cache hit/miss ratio\n\
         - Token breakdown by type\n\
         - Effective cost per call"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let input = ctx.cost_tracker.input_tokens();
        let output = ctx.cost_tracker.output_tokens();
        let cache_creation = ctx.cost_tracker.cache_creation_tokens();
        let cache_read = ctx.cost_tracker.cache_read_tokens();
        let total = ctx.cost_tracker.total_tokens();
        let cost = ctx.cost_tracker.total_cost_usd();

        // Estimate API calls from messages (each assistant message ~ 1 API call)
        let api_calls = ctx.messages.iter()
            .filter(|m| m.role == claurst_core::types::Role::Assistant)
            .count();
        let api_calls = api_calls.max(1); // at least 1 if we have any data

        // Cache efficiency
        let cache_total = cache_creation + cache_read;
        let cache_hit_pct = if cache_total > 0 {
            (cache_read as f64 / cache_total as f64) * 100.0
        } else {
            0.0
        };

        let cost_per_call = if api_calls > 0 {
            cost / api_calls as f64
        } else {
            0.0
        };

        CommandResult::Message(format!(
            "Detailed Usage Statistics\n\
             ─────────────────────────\n\
             API calls:           {api_calls}\n\
             Avg cost/call:       ${cost_per_call:.4}\n\n\
             Token Breakdown:\n\
               Input tokens:      {input:>10}\n\
               Output tokens:     {output:>10}\n\
               Cache creation:    {cache_creation:>10}\n\
               Cache read:        {cache_read:>10}\n\
               Total tokens:      {total:>10}\n\n\
             Cache Performance:\n\
               Cache hit rate:    {cache_hit_disp}\n\
               Cache efficiency:  {cache_eff}\n\n\
             Cost:\n\
               Total cost:        ${cost:.4}\n\
               Cost/1k tokens:    ${cost_per_k:.4}",
            api_calls = api_calls,
            cost_per_call = cost_per_call,
            input = input,
            output = output,
            cache_creation = display_cache_count(cache_creation, cache_read),
            cache_read = display_cache_count(cache_read, cache_creation),
            total = total,
            cache_hit_disp = if cache_total > 0 {
                format!("{:.1}%", cache_hit_pct)
            } else {
                "n/a".to_string()
            },
            cache_eff = if cache_hit_pct > 70.0 {
                "Excellent"
            } else if cache_hit_pct > 40.0 {
                "Good"
            } else if cache_total > 0 {
                "Low — prompts may not be stable enough to cache"
            } else {
                "No cache activity"
            },
            cost = cost,
            cost_per_k = if total > 0 { cost / (total as f64 / 1000.0) } else { 0.0 },
        ))
    }
}

/// Render a cache token count for display. When neither the read nor the write
/// counter recorded any activity this session, the provider almost certainly
/// does not report cache usage at all (e.g. a local OpenAI-compatible server
/// without prompt caching), so show `n/a` instead of a flat `0` that reads as
/// if caching were broken.
fn display_cache_count(value: u64, other: u64) -> String {
    if value == 0 && other == 0 {
        "n/a".to_string()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::display_cache_count;

    #[test]
    fn cache_count_zero_both_is_na() {
        assert_eq!(display_cache_count(0, 0), "n/a");
    }

    #[test]
    fn cache_count_nonzero_shows_number() {
        assert_eq!(display_cache_count(1234, 0), "1234");
    }

    #[test]
    fn cache_count_zero_read_but_nonzero_write_shows_zero() {
        // Anthropic can report cache_creation on the first turn with no reads
        // yet; that is real activity, so the zero read count is honest, not n/a.
        assert_eq!(display_cache_count(0, 500), "0");
    }
}
