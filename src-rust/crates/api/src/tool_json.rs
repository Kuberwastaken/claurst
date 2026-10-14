// tool_json.rs — repair-then-reject parsing of streamed tool-call arguments (#439).
//
// A tool call's arguments arrive as concatenated partial-JSON deltas. They are
// parsed strictly first; if that fails, one bounded repair pass fixes the
// mistakes models actually make (raw control characters and invalid escapes
// inside strings — pi's `repairJson` — plus trailing commas and a few missing
// closing braces/brackets). If the result still does not parse to an object,
// the call is rejected and the caller returns a tool error to the model
// instead of running the tool with `{}`.
//
// The repair never invents data: a buffer that ends inside a string or right
// after a `:` is rejected, so a truncated path or command is never "completed".

use serde_json::Value;

/// Most closing braces/brackets the repair pass will append.
const MAX_APPENDED_CLOSERS: usize = 4;

/// Why a tool call's arguments were rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ToolInputError(String);

/// Parse a tool call's accumulated argument buffer.
///
/// An empty (or whitespace-only) buffer is a no-argument call and maps to
/// `{}`. Anything else must be (or be repairable into) a JSON object.
pub fn parse_tool_input(raw: &str) -> Result<Value, ToolInputError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let strict_err = match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => return require_object(value),
        Err(e) => e,
    };
    match repair_json(trimmed) {
        Some(repaired) if repaired != trimmed => match serde_json::from_str::<Value>(&repaired) {
            Ok(value) => require_object(value),
            Err(_) => Err(ToolInputError(strict_err.to_string())),
        },
        _ => Err(ToolInputError(strict_err.to_string())),
    }
}

fn require_object(value: Value) -> Result<Value, ToolInputError> {
    if value.is_object() {
        Ok(value)
    } else {
        Err(ToolInputError(format!(
            "expected a JSON object, got {}",
            match value {
                Value::Null => "null",
                Value::Bool(_) => "a boolean",
                Value::Number(_) => "a number",
                Value::String(_) => "a string",
                Value::Array(_) => "an array",
                Value::Object(_) => "an object",
            }
        )))
    }
}

/// One linear repair pass. Returns `None` when the input cannot be repaired
/// safely (truncated inside a string or after a key, mismatched brackets, or
/// too many missing closers).
fn repair_json(input: &str) -> Option<String> {
    let mut out = String::with_capacity(input.len() + MAX_APPENDED_CLOSERS);
    let mut closers: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if in_string {
            match c {
                '"' => {
                    in_string = false;
                    out.push(c);
                }
                '\\' => match chars.peek().copied() {
                    Some(next @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't')) => {
                        out.push('\\');
                        out.push(next);
                        chars.next();
                    }
                    Some('u') => {
                        let hex: String = chars.clone().skip(1).take(4).collect();
                        if hex.len() == 4 && hex.chars().all(|h| h.is_ascii_hexdigit()) {
                            out.push_str("\\u");
                            out.push_str(&hex);
                            for _ in 0..5 {
                                chars.next();
                            }
                        } else {
                            out.push_str("\\\\");
                        }
                    }
                    // Invalid escape or a trailing backslash: keep it literally.
                    _ => out.push_str("\\\\"),
                },
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0c}' => out.push_str("\\f"),
                c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
                c => out.push(c),
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' => {
                closers.push('}');
                out.push(c);
            }
            '[' => {
                closers.push(']');
                out.push(c);
            }
            '}' | ']' => {
                strip_trailing_comma(&mut out);
                if closers.pop() != Some(c) {
                    return None;
                }
                out.push(c);
            }
            c => out.push(c),
        }
    }

    if in_string {
        return None;
    }
    if !closers.is_empty() {
        if closers.len() > MAX_APPENDED_CLOSERS || out.trim_end().ends_with(':') {
            return None;
        }
        strip_trailing_comma(&mut out);
        while let Some(closer) = closers.pop() {
            out.push(closer);
        }
    }
    Some(out)
}

/// Drop a structural comma (outside strings) that directly precedes a closer.
fn strip_trailing_comma(out: &mut String) {
    let kept = out.trim_end().len();
    if out[..kept].ends_with(',') {
        out.truncate(kept - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_and_empty_inputs() {
        assert_eq!(parse_tool_input(r#"{"a":1}"#).unwrap(), json!({"a": 1}));
        assert_eq!(parse_tool_input("").unwrap(), json!({}));
        assert_eq!(parse_tool_input("  \n").unwrap(), json!({}));
    }

    #[test]
    fn repairs_trailing_commas() {
        assert_eq!(
            parse_tool_input(r#"{"a": [1, 2,], "b": "x",}"#).unwrap(),
            json!({"a": [1, 2], "b": "x"})
        );
    }

    #[test]
    fn repairs_raw_newlines_and_bad_escapes_in_strings() {
        let raw = "{\"content\": \"line1\nline2\tend\", \"path\": \"C:\\dir\"}";
        let v = parse_tool_input(raw).unwrap();
        assert_eq!(v["content"], "line1\nline2\tend");
        assert_eq!(v["path"], "C:\\dir");
    }

    #[test]
    fn repairs_missing_closing_braces() {
        assert_eq!(
            parse_tool_input(r#"{"cmd": "ls", "opts": {"all": true"#).unwrap(),
            json!({"cmd": "ls", "opts": {"all": true}})
        );
    }

    #[test]
    fn rejects_unrepairable_input() {
        // Truncated inside a string: never complete a path or command.
        assert!(parse_tool_input(r#"{"path": "/etc/passwd"#).is_err());
        // Truncated after a key.
        assert!(parse_tool_input(r#"{"a":"#).is_err());
        // Not JSON at all.
        assert!(parse_tool_input("{not json}").is_err());
        // Mismatched brackets.
        assert!(parse_tool_input(r#"{"a": [1}"#).is_err());
        // Valid JSON that is not an object.
        assert!(parse_tool_input("[1, 2]").is_err());
        assert!(parse_tool_input("\"str\"").is_err());
    }
}
