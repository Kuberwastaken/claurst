//! The dialect table: what each provider is known to accept.
//!
//! Every entry is evidence-based. A keyword is listed as supported because
//! claurst has observed the provider accept it in a live request, not because the
//! spec says it should. That asymmetry is deliberate: the cost of omitting a
//! keyword the provider would have accepted is a slightly less expressive tool
//! schema, while the cost of listing one it rejects is every request failing.

use crate::dialect::{DialectSpec, DialectTransforms};

/// Annotations and validation keywords the OpenAI function-parameters subset
/// accepts. Derived from the deny-list this replaces
/// inverted against the keywords claurst and common MCP servers actually emit.
pub const OPENAI: DialectSpec = DialectSpec {
    id: "openai",
    supported_keywords: &[
        "description",
        "default",
        "examples",
        "format",
        "anyOf",
        "oneOf",
        "allOf",
        "$defs",
        "definitions",
        "$ref",
        "$schema",
        "const",
        "additionalProperties",
        "patternProperties",
        "prefixItems",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
        "pattern",
        "minItems",
        "maxItems",
        "nullable",
    ],
    // Unknown formats such as `uri` fail the strict validator.
    supported_string_formats: &[
        "date-time",
        "time",
        "date",
        "duration",
        "email",
        "hostname",
        "ipv4",
        "ipv6",
        "uuid",
    ],
    transforms: DialectTransforms {
        one_of_as_any_of: true,
        // OpenAI accepts `allOf` but does not intersect its branches the way
        // the spec requires, so a schema whose properties live only in `allOf`
        // branches ends up advertising none of them. The sanitizer this
        // replaces merged them into the parent object, so the dialect must too
        // or migrating onto it would silently empty those tools (caught by
        // diffing the two before switching).
        merge_all_of_branches: true,
        ..DEFAULT_TRANSFORMS
    },
};

/// Gemini `generateContent` accepts an OpenAPI 3.0 subset for
/// `function_declarations.parameters`, which is much narrower than OpenAI's:
/// draft keywords like `$ref`/`$defs`/`additionalProperties`/`propertyNames`
/// are rejected with HTTP 400, and it validates `required` against the
/// same object's `properties`.
pub const GEMINI: DialectSpec = DialectSpec {
    id: "gemini",
    supported_keywords: &[
        "description",
        "default",
        "format",
        "anyOf",
        "nullable",
        "minimum",
        "maximum",
        "minItems",
        "maxItems",
        "minLength",
        "maxLength",
        "pattern",
        "example",
    ],
    supported_string_formats: &[],
    transforms: DialectTransforms {
        // Code Assist rejects a root `anyOf` when `type`/`properties` sit
        // beside it ("specified other fields alongside any_of"), which is how
        // a multi-action tool expresses its branches. Nested
        // `anyOf` inside a property is fine, so only the root is widened.
        flatten_top_level_combiners: true,
        prune_dangling_required: true,
        const_as_enum: true,
        one_of_as_any_of: true,
        ..DEFAULT_TRANSFORMS
    },
};

/// Anthropic accepts full draft 2020-12 inside properties but rejects
/// combiners at the input schema's top level.
pub const ANTHROPIC: DialectSpec = DialectSpec {
    id: "anthropic",
    supported_keywords: &[
        "description",
        "default",
        "examples",
        "format",
        "anyOf",
        "oneOf",
        "allOf",
        "not",
        "if",
        "then",
        "else",
        "$defs",
        "definitions",
        "$ref",
        "$schema",
        "$comment",
        "const",
        "additionalProperties",
        "patternProperties",
        "propertyNames",
        "prefixItems",
        "contains",
        "uniqueItems",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
        "pattern",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
        "dependentRequired",
        "dependentSchemas",
        "nullable",
    ],
    supported_string_formats: &[],
    transforms: DialectTransforms {
        flatten_top_level_combiners: true,
        // Anthropic's `input_schema` must be an object schema with a
        // `properties` map: the API rejects a bare `{"type":"object"}` and a
        // non-object schema outright. The provider's own sanitizer guaranteed
        // both, so the dialect has to as well or migrating onto it would be a
        // regression (caught by diffing the two before switching).
        require_properties_on_objects: true,
        ..DEFAULT_TRANSFORMS
    },
};

/// OpenRouter forwards to whichever upstream serves the model, so it must
/// satisfy the strictest of them: Anthropic-family top-level combiner
/// rejection plus LM Studio's demand for `properties`.
pub const OPENROUTER: DialectSpec = DialectSpec {
    id: "openrouter",
    supported_keywords: OPENAI.supported_keywords,
    supported_string_formats: &[],
    transforms: DialectTransforms {
        flatten_top_level_combiners: true,
        require_properties_on_objects: true,
        ..DEFAULT_TRANSFORMS
    },
};

/// Antigravity's Claude route wraps Anthropic but rejects combiners at any
/// depth, not just at the top level, so every `anyOf`/`oneOf`/`allOf` is
/// flattened into its parent object.
pub const ANTIGRAVITY_CLAUDE: DialectSpec = DialectSpec {
    id: "antigravity-claude",
    supported_keywords: ANTHROPIC.supported_keywords,
    supported_string_formats: ANTHROPIC.supported_string_formats,
    transforms: DialectTransforms {
        flatten_all_combiners: true,
        require_properties_on_objects: true,
        prune_dangling_required: true,
        ..DEFAULT_TRANSFORMS
    },
};

/// Antigravity's Gemini->Anthropic bridge: Gemini's narrow keyword set with
/// combiner rejection at any depth and the Anthropic object-shape demand.
pub const ANTIGRAVITY_BRIDGE: DialectSpec = DialectSpec {
    id: "antigravity-bridge",
    supported_keywords: GEMINI.supported_keywords,
    supported_string_formats: GEMINI.supported_string_formats,
    transforms: DialectTransforms {
        flatten_all_combiners: true,
        require_properties_on_objects: true,
        prune_dangling_required: true,
        const_as_enum: true,
        one_of_as_any_of: true,
        ..DEFAULT_TRANSFORMS
    },
};

/// Amazon Bedrock `converse` accepts a JSON Schema subset for
/// `toolSpec.inputSchema.json`: draft keywords like `$ref`/`$defs`/
/// `patternProperties`/`if`/`then` are rejected with HTTP 400, and a `required`
/// entry naming an undeclared property fails validation.
pub const BEDROCK: DialectSpec = DialectSpec {
    id: "bedrock",
    supported_keywords: OPENAI.supported_keywords,
    supported_string_formats: &[],
    transforms: DialectTransforms {
        prune_dangling_required: true,
        one_of_as_any_of: true,
        ..DEFAULT_TRANSFORMS
    },
};

const DEFAULT_TRANSFORMS: DialectTransforms = DialectTransforms {
    flatten_top_level_combiners: false,
    flatten_all_combiners: false,
    require_properties_on_objects: false,
    prune_dangling_required: false,
    const_as_enum: false,
    one_of_as_any_of: false,
    merge_all_of_branches: false,
};

/// Every registered dialect, for conformance sweeps.
pub const ALL: &[&DialectSpec] = &[
    &OPENAI,
    &GEMINI,
    &ANTHROPIC,
    &OPENROUTER,
    &BEDROCK,
    &ANTIGRAVITY_CLAUDE,
    &ANTIGRAVITY_BRIDGE,
];

/// Look up a dialect by its stable id (used by the quirk store and CLI).
pub fn by_id(id: &str) -> Option<&'static DialectSpec> {
    ALL.iter().copied().find(|spec| spec.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_dialect_is_coherent() {
        for spec in ALL {
            spec.validate().unwrap_or_else(|err| panic!("{err}"));
        }
    }

    #[test]
    fn dialect_ids_are_unique() {
        let mut ids: Vec<&str> = ALL.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate dialect id");
    }

    #[test]
    fn lookup_round_trips() {
        for spec in ALL {
            assert_eq!(by_id(spec.id).map(|s| s.id), Some(spec.id));
        }
        assert!(by_id("nope").is_none());
    }
}
