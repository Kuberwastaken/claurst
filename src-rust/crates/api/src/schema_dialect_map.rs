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

/// Dialect for `provider_id`, or `None` when no entry covers it. Unmapped
/// providers keep receiving their schemas untouched.
pub fn spec_for(provider_id: &str) -> Option<&'static DialectSpec> {
    match provider_id {
        "google" | "gemini" | "antigravity" => Some(match provider_id {
            "antigravity" => &registry::ANTIGRAVITY_BRIDGE,
            _ => &registry::GEMINI,
        }),
        "bedrock" => Some(&registry::BEDROCK),
        "anthropic" => Some(&registry::ANTHROPIC),
        "openai" => Some(&registry::OPENAI),
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
