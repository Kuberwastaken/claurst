// SyntheticOutputTool: lets a headless run return its final answer as JSON
// matching a caller-supplied schema (`--json-schema`, #473).
//
// Tool name: "StructuredOutput" (matches TypeScript SYNTHETIC_OUTPUT_TOOL_NAME)
//
// Input:  an object matching the schema given to `SyntheticOutputTool::new`
// Output: "Structured output provided successfully"; the headless runner
//         captures the accepted input as the result's `structured_output`.
//
// The tool is registered only when `--json-schema` is given; it is not part
// of `all_tools()`. Input is checked against the schema's top level (object,
// `required` keys, property `type`s) so the model can correct a bad call.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde_json::Value;

#[derive(Clone)]
pub struct SyntheticOutputTool {
    schema: Value,
}

impl SyntheticOutputTool {
    pub const NAME: &'static str = "StructuredOutput";

    /// `schema` must be a JSON Schema object whose top-level `type` is
    /// `"object"` (tool inputs are always objects).
    pub fn new(schema: Value) -> Result<Self, String> {
        match schema.get("type").and_then(Value::as_str) {
            Some("object") => Ok(Self { schema }),
            Some(other) => Err(format!(
                "the schema's top-level \"type\" must be \"object\", not {:?}",
                other
            )),
            None => Err("the schema must be a JSON object with \"type\": \"object\"".to_string()),
        }
    }

    /// Check `input` against the schema: `Ok(())` when it matches, otherwise
    /// the violations, one message each.
    pub fn validate(&self, input: &Value) -> Result<(), Vec<String>> {
        let problems = self.violations(input);
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    /// Top-level schema violations in `input`, one message each.
    fn violations(&self, input: &Value) -> Vec<String> {
        let Some(object) = input.as_object() else {
            return vec!["the input must be a JSON object".to_string()];
        };
        let mut problems = Vec::new();
        if let Some(required) = self.schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    problems.push(format!("missing required property \"{}\"", key));
                }
            }
        }
        if let Some(properties) = self.schema.get("properties").and_then(Value::as_object) {
            for (key, value) in object {
                let Some(expected) = properties.get(key).and_then(|p| p.get("type")) else {
                    continue;
                };
                let allowed: Vec<&str> = match expected {
                    Value::String(ty) => vec![ty.as_str()],
                    Value::Array(types) => types.iter().filter_map(Value::as_str).collect(),
                    _ => continue,
                };
                if !allowed.iter().any(|ty| value_has_type(value, ty)) {
                    problems.push(format!(
                        "property \"{}\" must be of type {}",
                        key,
                        allowed.join(" or ")
                    ));
                }
            }
        }
        problems
    }
}

fn value_has_type(value: &Value, ty: &str) -> bool {
    match ty {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        // Unknown type keywords are not checked.
        _ => true,
    }
}

#[async_trait]
impl Tool for SyntheticOutputTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Return your final answer as structured JSON matching this tool's input \
         schema. Call this tool exactly once, when the task is done. If the call \
         is rejected, fix the input and call it again."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        match self.validate(&input) {
            Ok(()) => ToolResult::success("Structured output provided successfully"),
            Err(problems) => ToolResult::error(format!(
                "StructuredOutput input does not match the schema: {}.",
                problems.join("; ")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool() -> SyntheticOutputTool {
        SyntheticOutputTool::new(json!({
            "type": "object",
            "properties": {
                "answer": {"type": "string"},
                "score": {"type": ["integer", "null"]}
            },
            "required": ["answer"]
        }))
        .unwrap()
    }

    #[test]
    fn only_object_schemas_are_accepted() {
        assert!(SyntheticOutputTool::new(json!({"type": "array"})).is_err());
        assert!(SyntheticOutputTool::new(json!([1, 2])).is_err());
        assert!(SyntheticOutputTool::new(json!({"properties": {}})).is_err());
        assert!(SyntheticOutputTool::new(json!({"type": "object"})).is_ok());
    }

    #[test]
    fn input_schema_is_the_given_schema() {
        assert_eq!(tool().input_schema()["required"], json!(["answer"]));
    }

    #[test]
    fn violations_cover_required_keys_and_types() {
        let tool = tool();
        assert!(tool.violations(&json!({"answer": "42", "score": 3})).is_empty());
        assert!(tool.violations(&json!({"answer": "42", "score": null})).is_empty());
        assert_eq!(
            tool.violations(&json!({"score": 3})),
            ["missing required property \"answer\""]
        );
        assert_eq!(
            tool.violations(&json!({"answer": 42})),
            ["property \"answer\" must be of type string"]
        );
        assert_eq!(tool.violations(&json!("text")), ["the input must be a JSON object"]);
    }
}
