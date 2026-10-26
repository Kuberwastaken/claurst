// schema_dialect_map.rs: mapping provider ids to their JSON Schema dialect.
//
// The claurst-schema-dialect crate owns the dialect table and the recursion.
// This module only answers "which dialect does this provider speak", so the
// mapping lives next to the provider ids it keys on and every provider pulls
// from the same source of truth.

use claurst_core::types::ToolDefinition;
use claurst_schema_dialect::dialect::DialectSpec;
use claurst_schema_dialect::registry;
use claurst_schema_dialect::{normalize, recover_from_error, RecoveryAction};

// Shared wire syntax does not imply shared backend validation. Keep learned
// rejections isolated even when providers begin with the same accepted subset.
const AZURE: DialectSpec = DialectSpec {
    id: "azure",
    ..registry::OPENAI
};
const COPILOT: DialectSpec = DialectSpec {
    id: "github-copilot",
    ..registry::OPENAI
};
const COHERE: DialectSpec = DialectSpec {
    id: "cohere",
    ..registry::OPENAI
};

/// Dialect for `provider_id`, or `None` when no entry covers it. Unmapped
/// providers keep receiving their schemas untouched.
///
/// azure / copilot / cohere speak the OpenAI function-calling subset: their
/// request builders emit tools via the OpenAI wire format, so both the
/// prevention filter and rejection recovery must apply the OpenAI dialect to
/// them. Without this, a 400 naming a rejected keyword on those routes kills
/// the turn where the same error on the openai route would self-heal.
pub fn spec_for(provider_id: &str) -> Option<&'static DialectSpec> {
    match provider_id {
        "google" | "gemini" | "google-vertex" | "antigravity" => Some(match provider_id {
            "antigravity" => &registry::ANTIGRAVITY_BRIDGE,
            _ => &registry::GEMINI,
        }),
        "bedrock" | "amazon-bedrock" => Some(&registry::BEDROCK),
        "anthropic" => Some(&registry::ANTHROPIC),
        "openai" => Some(&registry::OPENAI),
        "azure" => Some(&AZURE),
        "copilot" | "github-copilot" => Some(&COPILOT),
        "cohere" => Some(&COHERE),
        "openrouter" => Some(&registry::OPENROUTER),
        _ => None,
    }
}

/// Filter every tool's input schema through `provider_id`'s dialect, applying
/// anything already learned from live rejections. Cheap no-op when no dialect
/// covers the provider.
pub fn filter_tools(provider_id: &str, tools: &[ToolDefinition]) -> Vec<ToolDefinition> {
    let Some(spec) = spec_for(provider_id) else {
        return tools.to_vec();
    };
    tools
        .iter()
        .map(|td| ToolDefinition {
            name: td.name.clone(),
            description: td.description.clone(),
            input_schema: normalize(&td.input_schema, spec),
        })
        .collect()
}

/// Classify a provider error against `provider_id`'s dialect, learning any
/// named construct. Returns the description to log when a retry is warranted.
pub fn recover_description(provider_id: &str, message: &str) -> Option<String> {
    let spec = spec_for(provider_id)?;
    match recover_from_error(message, spec) {
        RecoveryAction::RetryWithoutConstruct { description } => Some(description),
        // Not schema related, or retrying cannot help: handle the error
        // through the normal paths either way.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(schema: serde_json::Value) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "t".to_string(),
            description: "d".to_string(),
            input_schema: schema,
        }]
    }

    #[test]
    fn gemini_dialect_strips_rejected_keywords() {
        let filtered = filter_tools(
            "google",
            &tool(json!({
                "type": "object",
                "properties": { "data": { "type": "object", "propertyNames": { "type": "string" } } },
                "required": ["data", "missing"]
            })),
        );
        let schema = &filtered[0].input_schema;
        assert!(schema["properties"]["data"].get("propertyNames").is_none());
        assert_eq!(schema["required"], json!(["data"]));
    }

    #[test]
    fn unmapped_providers_keep_their_schemas() {
        let tools = tool(json!({ "$schema": "https://example.com", "type": "object" }));
        let filtered = filter_tools("someunknown", &tools);
        assert!(filtered[0].input_schema.get("$schema").is_some());
    }

    #[test]
    fn openai_wire_providers_share_the_openai_dialect() {
        // azure / copilot / cohere emit tools through the OpenAI wire format,
        // so both prevention and rejection recovery must use the OpenAI
        // dialect for them.
        let tools = tool(json!({
            "type": "object",
            "properties": { "data": { "type": "object", "propertyNames": { "type": "string" } } },
        }));
        for provider in ["openai", "azure", "copilot", "github-copilot", "cohere"] {
            let filtered = filter_tools(provider, &tools);
            assert!(
                filtered[0].input_schema["properties"]["data"]
                    .get("propertyNames")
                    .is_none(),
                "prevention must strip for {provider}"
            );
            let recovered = recover_description(
                provider,
                "[openai] Invalid request: 'uniqueItems' is not permitted.",
            );
            assert!(
                recovered.is_some(),
                "a rejection naming a keyword must be actionable for {provider}"
            );
        }
    }

    #[test]
    fn learned_rejections_do_not_cross_backend_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        claurst_schema_dialect::quirks::use_test_path(dir.path().join("quirks.json"));
        let tools = tool(json!({"type":"object", "additionalProperties":false}));
        for provider in ["openai", "azure", "github-copilot", "cohere"] {
            assert!(filter_tools(provider, &tools)[0]
                .input_schema
                .get("additionalProperties")
                .is_some());
        }
        assert!(
            recover_description("github-copilot", "'additionalProperties' is not permitted")
                .is_some()
        );
        assert!(filter_tools("copilot", &tools)[0]
            .input_schema
            .get("additionalProperties")
            .is_none());
        for provider in ["openai", "azure", "cohere"] {
            assert!(
                filter_tools(provider, &tools)[0]
                    .input_schema
                    .get("additionalProperties")
                    .is_some(),
                "Copilot must not alter {provider}"
            );
        }
        claurst_schema_dialect::quirks::reset_cache_for_tests();
        assert!(filter_tools("github-copilot", &tools)[0]
            .input_schema
            .get("additionalProperties")
            .is_none());
        assert!(filter_tools("openai", &tools)[0]
            .input_schema
            .get("additionalProperties")
            .is_some());
        assert_eq!(
            spec_for("amazon-bedrock").unwrap().id,
            spec_for("bedrock").unwrap().id
        );
        assert_eq!(
            spec_for("google-vertex").unwrap().id,
            spec_for("google").unwrap().id
        );
    }

    #[test]
    fn bedrock_uses_its_dialect() {
        let filtered = filter_tools(
            "bedrock",
            &tool(json!({
                "type": "object",
                "properties": { "id": { "type": "string", "format": "uuid" } },
                "if": { "properties": { "a": { "type": "string" } } },
                "then": { "properties": { "b": { "type": "string" } } }
            })),
        );
        let schema = &filtered[0].input_schema;
        assert!(schema.get("if").is_none());
        assert!(schema.get("then").is_none());
        // An empty `supported_string_formats` list means "any format", so
        // Bedrock keeps `format` values it has not rejected live.
        assert!(schema["properties"]["id"].get("format").is_some());
    }

    #[test]
    fn openai_keeps_supported_formats() {
        let filtered = filter_tools(
            "openai",
            &tool(json!({
                "type": "object",
                "properties": { "when": { "type": "string", "format": "date-time" } }
            })),
        );
        assert_eq!(
            filtered[0].input_schema["properties"]["when"]["format"],
            "date-time"
        );
    }
}
