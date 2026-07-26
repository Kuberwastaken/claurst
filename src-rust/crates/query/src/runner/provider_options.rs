// Provider-option assembly: reasoning-effort mapping and per-provider request
// options. Extracted from lib.rs (issue #232). Behavior-preserving move.

use crate::*;

pub(crate) fn reasoning_effort_for_level(
    effort_level: claurst_core::effort::EffortLevel,
) -> &'static str {
    use claurst_core::effort::EffortLevel;
    match effort_level {
        // `none`/`minimal` are the two OpenAI reasoning_effort tiers below `low`;
        // pass them through verbatim (the model's variants ladder only offers
        // them where the API accepts them).
        EffortLevel::None => "none",
        EffortLevel::Minimal => "minimal",
        EffortLevel::Low => "low",
        EffortLevel::Medium => "medium",
        // XHigh/Max/Ultracode collapse to "high" for the generic OpenAI-family
        // `reasoning_effort` value. Providers that accept a higher tier (e.g.
        // Codex's "xhigh") get it via the provider-specific override below;
        // defaulting to "high" keeps unknown providers safe.
        EffortLevel::High | EffortLevel::XHigh | EffortLevel::Max | EffortLevel::Ultracode => {
            "high"
        }
    }
}

pub(crate) fn google_thinking_level_for_effort(
    effort_level: Option<claurst_core::effort::EffortLevel>,
) -> &'static str {
    use claurst_core::effort::EffortLevel;
    match effort_level.unwrap_or(EffortLevel::High) {
        // Google's thinkingLevel has no "none"; floor it at "low". "minimal" is a
        // real gemini-3 thinking level, so pass Minimal through.
        EffortLevel::None => "low",
        EffortLevel::Minimal => "minimal",
        EffortLevel::Low => "low",
        EffortLevel::Medium => "medium",
        // Gemini's top thinking level is "high"; XHigh/Max/Ultracode all map onto it.
        EffortLevel::High | EffortLevel::XHigh | EffortLevel::Max | EffortLevel::Ultracode => {
            "high"
        }
    }
}

pub(crate) fn is_openai_reasoning_model(model_id: &str) -> bool {
    let model_id = model_id.to_ascii_lowercase();
    model_id.starts_with("gpt-5")
        || model_id.starts_with("o1")
        || model_id.starts_with("o3")
        || model_id.starts_with("o4")
}

pub(crate) fn is_openaiish_provider(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "openai"
            | "azure"
            | "groq"
            | "mistral"
            | "deepseek"
            | "xai"
            | "openrouter"
            | "togetherai"
            | "together-ai"
            | "perplexity"
            | "cerebras"
            | "deepinfra"
            | "venice"
            | "huggingface"
            | "nvidia"
            | "siliconflow"
            | "sambanova"
            | "moonshot"
            | "zhipu"
            | "zai"
            | "qwen"
            | "alibaba"
            | "nebius"
            | "novita"
            | "ovhcloud"
            | "scaleway"
            | "vultr"
            | "vultr-ai"
            | "baseten"
            | "friendli"
            | "upstage"
            | "stepfun"
            | "fireworks"
            | "ollama"
            | "codex"
            | "openai-codex"
            | "lmstudio"
            | "lm-studio"
            | "llamacpp"
            | "llama-cpp"
    )
}

pub(crate) fn build_provider_options(
    provider_id: &str,
    model_id: &str,
    effort_level: Option<claurst_core::effort::EffortLevel>,
    thinking_budget: Option<u32>,
) -> Value {
    let mut options = serde_json::Map::new();
    let model_id = model_id.to_ascii_lowercase();

    if provider_id == "github-copilot" {
        if model_id.contains("claude") {
            options.insert(
                "thinking_budget".to_string(),
                serde_json::json!(thinking_budget.unwrap_or(4_000)),
            );
        } else if model_id.starts_with("gpt-5") && !model_id.contains("gpt-5-pro") {
            let reasoning_effort = effort_level
                .map(reasoning_effort_for_level)
                .unwrap_or("medium");
            options.insert(
                "reasoningEffort".to_string(),
                serde_json::json!(reasoning_effort),
            );
            options.insert(
                "reasoningSummary".to_string(),
                serde_json::json!("auto"),
            );
            options.insert(
                "include".to_string(),
                serde_json::json!(["reasoning.encrypted_content"]),
            );

            if model_id.contains("gpt-5.")
                && !model_id.contains("codex")
                && !model_id.contains("-chat")
            {
                options.insert(
                    "textVerbosity".to_string(),
                    serde_json::json!("low"),
                );
            }
        }
    }

    if provider_id == "google" && model_id.contains("gemini") {
        if model_id.contains("2.5") {
            if let Some(budget) = thinking_budget {
                options.insert(
                    "thinkingConfig".to_string(),
                    serde_json::json!({
                        "includeThoughts": true,
                        "thinkingBudget": budget,
                    }),
                );
            }
        } else if model_id.contains("3.") || model_id.contains("gemini-3") {
            options.insert(
                "thinkingConfig".to_string(),
                serde_json::json!({
                    "includeThoughts": true,
                    "thinkingLevel": google_thinking_level_for_effort(effort_level),
                }),
            );
        }
    }

    if provider_id == "amazon-bedrock" {
        if model_id.contains("anthropic") || model_id.contains("claude") {
            if let Some(budget) = thinking_budget {
                options.insert(
                    "reasoningConfig".to_string(),
                    serde_json::json!({
                        "type": "enabled",
                        "budgetTokens": budget.min(31_999),
                    }),
                );
            }
        } else if let Some(level) = effort_level {
            options.insert(
                "reasoningConfig".to_string(),
                serde_json::json!({
                    "type": "enabled",
                    "maxReasoningEffort": reasoning_effort_for_level(level),
                }),
            );
        }
    }

    if is_openaiish_provider(provider_id) && is_openai_reasoning_model(&model_id) {
        let reasoning_effort = effort_level
            .map(reasoning_effort_for_level)
            .unwrap_or("medium");
        // Codex (ChatGPT) accepts the full gpt-5 effort ladder including
        // `xhigh`, so surface the top tiers (XHigh / Max / Ultracode) as "extra
        // high" there — matching opencode — without changing the value sent to
        // other OpenAI-compatible providers that may not accept it.
        let reasoning_effort = if matches!(provider_id, "codex" | "openai-codex")
            && matches!(
                effort_level,
                Some(claurst_core::effort::EffortLevel::XHigh)
                    | Some(claurst_core::effort::EffortLevel::Max)
                    | Some(claurst_core::effort::EffortLevel::Ultracode)
            ) {
            "xhigh"
        } else {
            reasoning_effort
        };
        options.insert(
            "reasoningEffort".to_string(),
            serde_json::json!(reasoning_effort),
        );

        // Match opencode's gpt-5 defaults for the Codex (ChatGPT) endpoint:
        // request an auto reasoning summary and carry encrypted reasoning state
        // across stateless turns. Scoped to Codex so other OpenAI-compatible
        // providers that ignore these fields are unaffected.
        if matches!(provider_id, "codex" | "openai-codex") {
            options.insert("reasoningSummary".to_string(), serde_json::json!("auto"));
            options.insert(
                "include".to_string(),
                serde_json::json!(["reasoning.encrypted_content"]),
            );
        }

        if model_id.starts_with("gpt-5")
            && model_id.contains("gpt-5.")
            && !model_id.contains("codex")
            && !model_id.contains("-chat")
            && provider_id != "azure"
        {
            options.insert(
                "textVerbosity".to_string(),
                serde_json::json!("low"),
            );

            // DeepSeek V4 thinking mode: map effort level to thinking/reasoning_effort params.
            // DeepSeek docs: thinking={"type":"enabled/disabled"}, reasoning_effort="high"|"max"
            // low/medium are mapped to "high" by the API; xhigh mapped to "max".
            if provider_id == "deepseek" {
                match effort_level {
                    None
                    | Some(claurst_core::effort::EffortLevel::Minimal)
                    | Some(claurst_core::effort::EffortLevel::Medium)
                    | Some(claurst_core::effort::EffortLevel::High) => {
                        options.insert(
                            "thinking".to_string(),
                            serde_json::json!({"type": "enabled"}),
                        );
                        options.insert("reasoningEffort".to_string(), serde_json::json!("high"));
                    }
                    Some(claurst_core::effort::EffortLevel::XHigh)
                    | Some(claurst_core::effort::EffortLevel::Max)
                    | Some(claurst_core::effort::EffortLevel::Ultracode) => {
                        options.insert(
                            "thinking".to_string(),
                            serde_json::json!({"type": "enabled"}),
                        );
                        options.insert("reasoningEffort".to_string(), serde_json::json!("max"));
                    }
                    // `none` and `low` both disable DeepSeek's thinking mode.
                    Some(claurst_core::effort::EffortLevel::None)
                    | Some(claurst_core::effort::EffortLevel::Low) => {
                        options.insert(
                            "thinking".to_string(),
                            serde_json::json!({"type": "disabled"}),
                        );
                    }
                }
            }
        }
    }

    if provider_id == "openrouter" {
        options.insert("usage".to_string(), serde_json::json!({ "include": true }));
        if model_id.contains("gemini-3") {
            options.insert(
                "reasoning".to_string(),
                serde_json::json!({ "effort": "high" }),
            );
        }
    }

    if provider_id == "qwen"
        && thinking_budget.is_some()
        && !model_id.contains("kimi-k2-thinking")
    {
        options.insert("enable_thinking".to_string(), serde_json::json!(true));
    }

    if (provider_id == "zhipu" || provider_id == "zai") && thinking_budget.is_some() {
        options.insert(
            "thinking".to_string(),
            serde_json::json!({
                "type": "enabled",
                "clear_thinking": false,
            }),
        );
    }

    if options.is_empty() {
        Value::Null
    } else {
        Value::Object(options)
    }
}
