// protocol::dsml — DeepSeek DSML tool-call envelopes carried in plain text (#395).
//
// DeepSeek V4-family models served over OpenAI-compatible endpoints (DeepSeek
// direct, SiliconFlow, Chutes, OpenCode Zen, …) frequently emit tool calls as a
// tagged envelope inside ordinary `delta.content` text instead of the standard
// `tool_calls` delta:
//
//   <｜DSML｜tool_calls>
//   <｜DSML｜invoke name="read_file">
//   <｜DSML｜parameter name="path" string="true">src/main.rs</｜DSML｜parameter>
//   <｜DSML｜parameter name="limit" string="false">40</｜DSML｜parameter>
//   </｜DSML｜invoke>
//   </｜DSML｜tool_calls>
//
// Grammar accepted here:
//   * markup envelopes `<M tool_calls>` / `<M function_calls>` where the marker
//     `M` is `｜DSML｜` (fullwidth bars, U+FF5C) or `|DSML|` (ASCII bars), with
//     one or more `<M invoke name="…">` children, each holding zero or more
//     `<M parameter name="…" string="true|false">value</M parameter>` elements.
//     `string="true"` keeps the value verbatim as a JSON string;
//     `string="false"` decodes it as JSON (with a small repair pass for trailing
//     commas and raw control characters inside strings, then a raw-string
//     fallback);
//   * JSON-body envelopes `<|tool_call|>…</|tool_call|>` and `<|DSML|>…</|DSML|>`
//     whose body is `{"name": …, "arguments": {…} | "<json>"}` or an array of
//     such objects.
//
// Not handled: the DeepSeek V3/V3.1 special-token format
// (`<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>…`), which serving stacks
// normally translate into `tool_calls` before it reaches the client.
//
// The parser is streaming-safe: text is forwarded as soon as it cannot be the
// start of an envelope opener, a possible opener prefix at the end of the
// buffer is held back so partial tags never reach the transcript, and a
// complete envelope is replaced by the tool calls it encodes. Anything that
// does not parse (malformed envelope, or an envelope still open when the
// stream ends) is surfaced verbatim as text with a warning, so a bad envelope
// can never stall the turn or silently drop output.

use claurst_core::types::ContentBlock;
use serde_json::{Map, Value};
use tracing::warn;

/// How the body of an envelope encodes its tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grammar {
    /// Nested `invoke` / `parameter` tags using this marker between `<` and the
    /// tag name (e.g. `｜DSML｜`).
    Markup(&'static str),
    /// A JSON object (or array of objects) with `name` and `arguments`.
    Json,
}

#[derive(Debug, PartialEq, Eq)]
struct Envelope {
    open: &'static str,
    close: &'static str,
    grammar: Grammar,
}

const FULLWIDTH_MARKER: &str = "｜DSML｜";
const ASCII_MARKER: &str = "|DSML|";

const ENVELOPES: &[Envelope] = &[
    Envelope {
        open: "<｜DSML｜tool_calls>",
        close: "</｜DSML｜tool_calls>",
        grammar: Grammar::Markup(FULLWIDTH_MARKER),
    },
    Envelope {
        open: "<｜DSML｜function_calls>",
        close: "</｜DSML｜function_calls>",
        grammar: Grammar::Markup(FULLWIDTH_MARKER),
    },
    Envelope {
        open: "<|DSML|tool_calls>",
        close: "</|DSML|tool_calls>",
        grammar: Grammar::Markup(ASCII_MARKER),
    },
    Envelope {
        open: "<|DSML|function_calls>",
        close: "</|DSML|function_calls>",
        grammar: Grammar::Markup(ASCII_MARKER),
    },
    Envelope {
        open: "<|tool_call|>",
        close: "</|tool_call|>",
        grammar: Grammar::Json,
    },
    Envelope {
        open: "<|DSML|>",
        close: "</|DSML|>",
        grammar: Grammar::Json,
    },
];

/// Longest opener in [`ENVELOPES`], in bytes; bounds the held-back suffix.
const MAX_OPEN_LEN: usize = "<｜DSML｜function_calls>".len();

/// One tool call decoded from an envelope.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DsmlToolCall {
    pub name: String,
    pub input: Map<String, Value>,
}

/// A piece of parser output, in stream order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DsmlSegment {
    Text(String),
    ToolCall(DsmlToolCall),
}

/// Incremental extractor for DSML envelopes in streamed text.
#[derive(Debug, Default)]
pub(crate) struct DsmlStreamParser {
    /// Text not yet emitted. While an envelope is open it starts with the
    /// envelope's opener.
    buf: String,
    /// The envelope whose opener begins `buf`, if any.
    open: Option<&'static Envelope>,
    /// Byte offset in `buf` before which no closer / nested opener can start,
    /// so each push only rescans the newly arrived tail of a long envelope.
    scanned: usize,
}

impl DsmlStreamParser {
    /// Feed the next text fragment; completed segments are appended to `out`.
    pub(crate) fn push(&mut self, text: &str, out: &mut Vec<DsmlSegment>) {
        self.buf.push_str(text);
        loop {
            let Some(envelope) = self.open else {
                if let Some((pos, envelope)) = find_opener(&self.buf) {
                    self.emit_prefix(pos, out);
                    self.open = Some(envelope);
                    self.scanned = envelope.open.len();
                    continue;
                }
                let held = partial_opener_len(&self.buf);
                self.emit_prefix(self.buf.len() - held, out);
                return;
            };

            let body_start = envelope.open.len();
            let from = self.scanned.max(body_start);
            let close = self.buf[from..].find(envelope.close).map(|i| from + i);
            let reopen = self.buf[from..].find(envelope.open).map(|i| from + i);
            match (close, reopen) {
                (Some(close), reopen) if reopen.is_none_or(|r| close < r) => {
                    let rest = self.buf.split_off(close + envelope.close.len());
                    let raw = std::mem::replace(&mut self.buf, rest);
                    self.open = None;
                    self.scanned = 0;
                    match parse_body(envelope.grammar, &raw[body_start..close]) {
                        Some(calls) => out.extend(calls.into_iter().map(DsmlSegment::ToolCall)),
                        None => {
                            warn!(
                                envelope = envelope.open,
                                "malformed DeepSeek DSML tool-call envelope; emitting it as plain text"
                            );
                            out.push(DsmlSegment::Text(raw));
                        }
                    }
                }
                (_, Some(reopen)) => {
                    // A second opener before any closer: the first envelope was
                    // never terminated. Surface it as text and restart at the
                    // new opener.
                    warn!(
                        envelope = envelope.open,
                        "unterminated DeepSeek DSML tool-call envelope followed by a new one; emitting it as plain text"
                    );
                    self.emit_prefix(reopen, out);
                    self.scanned = body_start;
                }
                _ => {
                    let mut scanned = self
                        .buf
                        .len()
                        .saturating_sub(envelope.close.len().max(envelope.open.len()));
                    while !self.buf.is_char_boundary(scanned) {
                        scanned -= 1;
                    }
                    self.scanned = scanned;
                    return;
                }
            }
        }
    }

    /// Flush everything still buffered at end of stream. An envelope that
    /// never closed is emitted verbatim as text.
    pub(crate) fn finish(&mut self, out: &mut Vec<DsmlSegment>) {
        if let Some(envelope) = self.open.take() {
            warn!(
                envelope = envelope.open,
                "DeepSeek DSML tool-call envelope still open at end of stream; emitting it as plain text"
            );
        }
        self.scanned = 0;
        let len = self.buf.len();
        self.emit_prefix(len, out);
    }

    /// Move `buf[..end]` to `out` as text (if non-empty).
    fn emit_prefix(&mut self, end: usize, out: &mut Vec<DsmlSegment>) {
        if end == 0 {
            return;
        }
        let rest = self.buf.split_off(end);
        out.push(DsmlSegment::Text(std::mem::replace(&mut self.buf, rest)));
    }
}

/// Fresh, unique id for a tool call synthesized from a DSML envelope.
pub(crate) fn new_tool_use_id() -> String {
    format!("call_{}", uuid::Uuid::new_v4().simple())
}

/// Lift DSML envelopes out of the text blocks of a complete (non-streamed)
/// response. Extracted tool calls are appended as `ToolUse` blocks after the
/// existing blocks. Returns `true` when at least one tool call was extracted.
pub(crate) fn extract_tool_calls_from_blocks(blocks: &mut Vec<ContentBlock>) -> bool {
    let mut rebuilt = Vec::with_capacity(blocks.len());
    let mut tool_uses = Vec::new();
    for block in blocks.drain(..) {
        let ContentBlock::Text { text } = block else {
            rebuilt.push(block);
            continue;
        };
        let mut segments = Vec::new();
        let mut parser = DsmlStreamParser::default();
        parser.push(&text, &mut segments);
        parser.finish(&mut segments);
        let mut kept = String::new();
        for segment in segments {
            match segment {
                DsmlSegment::Text(t) => kept.push_str(&t),
                DsmlSegment::ToolCall(call) => tool_uses.push(ContentBlock::ToolUse {
                    id: new_tool_use_id(),
                    name: call.name,
                    input: Value::Object(call.input),
                    thought_signature: None,
                }),
            }
        }
        if !kept.is_empty() {
            rebuilt.push(ContentBlock::Text { text: kept });
        }
    }
    let extracted = !tool_uses.is_empty();
    rebuilt.extend(tool_uses);
    *blocks = rebuilt;
    extracted
}

/// Earliest complete opener in `text`.
fn find_opener(text: &str) -> Option<(usize, &'static Envelope)> {
    ENVELOPES
        .iter()
        .filter_map(|envelope| text.find(envelope.open).map(|pos| (pos, envelope)))
        .min_by_key(|(pos, _)| *pos)
}

/// Length of the longest suffix of `text` that is a proper prefix of some
/// opener. That suffix must be held back until more text arrives.
fn partial_opener_len(text: &str) -> usize {
    let mut start = text.len().saturating_sub(MAX_OPEN_LEN);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..]
        .char_indices()
        .map(|(i, _)| start + i)
        .find(|&i| {
            let tail = &text[i..];
            ENVELOPES.iter().any(|envelope| envelope.open.starts_with(tail))
        })
        .map_or(0, |i| text.len() - i)
}

fn parse_body(grammar: Grammar, body: &str) -> Option<Vec<DsmlToolCall>> {
    let calls = match grammar {
        Grammar::Markup(marker) => parse_markup_body(marker, body)?,
        Grammar::Json => match decode_json(body)? {
            Value::Array(items) => items
                .into_iter()
                .map(tool_call_from_json)
                .collect::<Option<Vec<_>>>()?,
            item => vec![tool_call_from_json(item)?],
        },
    };
    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

fn parse_markup_body(marker: &str, body: &str) -> Option<Vec<DsmlToolCall>> {
    let invoke_open = format!("<{marker}invoke");
    let invoke_close = format!("</{marker}invoke>");
    let param_open = format!("<{marker}parameter");
    let param_close = format!("</{marker}parameter>");

    let mut calls = Vec::new();
    let mut rest = body.trim_start();
    while !rest.is_empty() {
        let (attrs, after_tag) = split_tag(rest, &invoke_open)?;
        let name = attr(&attrs, "name")?;
        if name.is_empty() {
            return None;
        }
        let close = after_tag.find(invoke_close.as_str())?;
        let input = parse_parameters(&after_tag[..close], &param_open, &param_close)?;
        calls.push(DsmlToolCall {
            name: name.to_string(),
            input,
        });
        rest = after_tag[close + invoke_close.len()..].trim_start();
    }
    Some(calls)
}

fn parse_parameters(body: &str, open: &str, close: &str) -> Option<Map<String, Value>> {
    let mut input = Map::new();
    let mut rest = body.trim_start();
    while !rest.is_empty() {
        let (attrs, after_tag) = split_tag(rest, open)?;
        let name = attr(&attrs, "name")?;
        let end = after_tag.find(close)?;
        let raw = &after_tag[..end];
        let value = match attr(&attrs, "string") {
            Some(s) if s.eq_ignore_ascii_case("true") => Value::String(raw.to_string()),
            _ => decode_json(raw).unwrap_or_else(|| Value::String(raw.to_string())),
        };
        input.insert(name.to_string(), value);
        rest = after_tag[end + close.len()..].trim_start();
    }
    Some(input)
}

/// Split `<tag attrs…>rest` into its attributes and the text after `>`.
/// `text` must start with `tag_prefix` (e.g. `<｜DSML｜invoke`) followed by
/// whitespace or `>`.
fn split_tag<'a>(text: &'a str, tag_prefix: &str) -> Option<(Vec<(&'a str, &'a str)>, &'a str)> {
    let after = text.strip_prefix(tag_prefix)?;
    if !after.starts_with(|c: char| c == '>' || c.is_whitespace()) {
        return None;
    }
    let end = after.find('>')?;
    Some((parse_attrs(&after[..end])?, &after[end + 1..]))
}

/// Parse `key="value"` pairs separated by whitespace.
fn parse_attrs(text: &str) -> Option<Vec<(&str, &str)>> {
    let mut attrs = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let eq = rest.find('=')?;
        let key = rest[..eq].trim_end();
        if key.is_empty() || key.contains(char::is_whitespace) {
            return None;
        }
        let quoted = rest[eq + 1..].trim_start().strip_prefix('"')?;
        let end = quoted.find('"')?;
        attrs.push((key, &quoted[..end]));
        rest = quoted[end + 1..].trim_start();
    }
    Some(attrs)
}

fn attr<'a>(attrs: &[(&str, &'a str)], key: &str) -> Option<&'a str> {
    attrs.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// Build a tool call from `{"name": …, "arguments": {…} | "<json>"}`.
fn tool_call_from_json(value: Value) -> Option<DsmlToolCall> {
    let Value::Object(mut obj) = value else {
        return None;
    };
    let name = match obj.remove("name")? {
        Value::String(name) if !name.is_empty() => name,
        _ => return None,
    };
    let arguments = obj
        .remove("arguments")
        .or_else(|| obj.remove("parameters"))
        .unwrap_or_else(|| Value::Object(Map::new()));
    let input = match arguments {
        Value::Object(map) => map,
        Value::String(encoded) => match decode_json(&encoded)? {
            Value::Object(map) => map,
            _ => return None,
        },
        _ => return None,
    };
    Some(DsmlToolCall { name, input })
}

/// Decode model-written JSON: strict first, then after [`repair_json`].
fn decode_json(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    serde_json::from_str(trimmed)
        .ok()
        .or_else(|| serde_json::from_str(&repair_json(trimmed)).ok())
}

/// Best-effort repair of common model JSON mistakes: trailing commas before
/// `}` / `]`, and raw newlines, carriage returns or tabs inside strings.
fn repair_json(text: &str) -> String {
    let mut repaired = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        if in_string {
            match c {
                _ if escaped => {
                    escaped = false;
                    repaired.push(c);
                }
                '\\' => {
                    escaped = true;
                    repaired.push(c);
                }
                '"' => {
                    in_string = false;
                    repaired.push(c);
                }
                '\n' => repaired.push_str("\\n"),
                '\r' => repaired.push_str("\\r"),
                '\t' => repaired.push_str("\\t"),
                _ => repaired.push(c),
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                repaired.push(c);
            }
            ',' if text[i + 1..]
                .trim_start()
                .starts_with(['}', ']']) => {}
            _ => repaired.push(c),
        }
    }
    repaired
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FULL: &str = concat!(
        "<｜DSML｜tool_calls>\n",
        "<｜DSML｜invoke name=\"read_file\">\n",
        "<｜DSML｜parameter name=\"path\" string=\"true\">src/main.rs</｜DSML｜parameter>\n",
        "<｜DSML｜parameter name=\"limit\" string=\"false\">40</｜DSML｜parameter>\n",
        "</｜DSML｜invoke>\n",
        "</｜DSML｜tool_calls>",
    );

    fn run(chunks: &[&str]) -> Vec<DsmlSegment> {
        let mut parser = DsmlStreamParser::default();
        let mut out = Vec::new();
        for chunk in chunks {
            parser.push(chunk, &mut out);
        }
        parser.finish(&mut out);
        out
    }

    fn text_of(segments: &[DsmlSegment]) -> String {
        segments
            .iter()
            .filter_map(|s| match s {
                DsmlSegment::Text(t) => Some(t.as_str()),
                DsmlSegment::ToolCall(_) => None,
            })
            .collect()
    }

    fn calls_of(segments: &[DsmlSegment]) -> Vec<&DsmlToolCall> {
        segments
            .iter()
            .filter_map(|s| match s {
                DsmlSegment::ToolCall(c) => Some(c),
                DsmlSegment::Text(_) => None,
            })
            .collect()
    }

    /// Split `text` into chunks of `n` chars (never inside a char).
    fn char_chunks(text: &str, n: usize) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        chars.chunks(n).map(|c| c.iter().collect()).collect()
    }

    #[test]
    fn full_envelope_becomes_one_tool_call() {
        let out = run(&[FULL]);
        assert_eq!(text_of(&out), "");
        let calls = calls_of(&out);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(
            Value::Object(calls[0].input.clone()),
            json!({"path": "src/main.rs", "limit": 40})
        );
    }

    #[test]
    fn two_invokes_in_one_envelope() {
        let body = concat!(
            "<｜DSML｜tool_calls>",
            "<｜DSML｜invoke name=\"a\"><｜DSML｜parameter name=\"x\" string=\"true\">1</｜DSML｜parameter></｜DSML｜invoke>",
            "\n",
            "<｜DSML｜invoke name=\"b\"><｜DSML｜parameter name=\"y\" string=\"false\">{\"k\": [1, 2]}</｜DSML｜parameter></｜DSML｜invoke>",
            "</｜DSML｜tool_calls>",
        );
        let out = run(&[body]);
        let calls = calls_of(&out);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[0].input["x"], json!("1"));
        assert_eq!(calls[1].name, "b");
        assert_eq!(calls[1].input["y"], json!({"k": [1, 2]}));
    }

    #[test]
    fn invoke_without_parameters_has_empty_input() {
        let out = run(&["<｜DSML｜tool_calls><｜DSML｜invoke name=\"ls\"></｜DSML｜invoke></｜DSML｜tool_calls>"]);
        let calls = calls_of(&out);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].input.is_empty());
    }

    #[test]
    fn text_around_envelope_is_preserved_in_order() {
        let input = format!("Let me look.\n{FULL}\nDone looking.");
        let out = run(&[input.as_str()]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], DsmlSegment::Text("Let me look.\n".to_string()));
        assert!(matches!(&out[1], DsmlSegment::ToolCall(c) if c.name == "read_file"));
        assert_eq!(out[2], DsmlSegment::Text("\nDone looking.".to_string()));
    }

    #[test]
    fn chunked_delivery_every_split_point_matches_single_shot() {
        let input = format!("Before {FULL} after");
        let expected_text = "Before  after";
        for n in 1..=7 {
            let chunks = char_chunks(&input, n);
            let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
            let out = run(&refs);
            assert_eq!(text_of(&out), expected_text, "chunk size {n}");
            let calls = calls_of(&out);
            assert_eq!(calls.len(), 1, "chunk size {n}");
            assert_eq!(calls[0].input["path"], json!("src/main.rs"));
        }
    }

    #[test]
    fn partial_opener_is_never_emitted_as_text() {
        let mut parser = DsmlStreamParser::default();
        let mut out = Vec::new();
        parser.push("Running it <", &mut out);
        parser.push("｜DS", &mut out);
        assert_eq!(text_of(&out), "Running it ");
        parser.push("ML｜tool", &mut out);
        assert_eq!(text_of(&out), "Running it ", "no partial tag may leak");
    }

    #[test]
    fn plain_text_with_opener_prefix_passes_through() {
        let out = run(&["if a <", "| b then <｜DS", "X done"]);
        assert_eq!(text_of(&out), "if a <| b then <｜DSX done");
        assert!(calls_of(&out).is_empty());
    }

    #[test]
    fn ascii_marker_variant() {
        let out = run(&[concat!(
            "<|DSML|tool_calls>\n",
            "<|DSML|invoke name=\"bash\">\n",
            "<|DSML|parameter name=\"command\" string=\"true\">ls -la</|DSML|parameter>\n",
            "</|DSML|invoke>\n",
            "</|DSML|tool_calls>",
        )]);
        let calls = calls_of(&out);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].input["command"], json!("ls -la"));
    }

    #[test]
    fn function_calls_envelope_name() {
        let out = run(&["<｜DSML｜function_calls><｜DSML｜invoke name=\"x\"></｜DSML｜invoke></｜DSML｜function_calls>"]);
        assert_eq!(calls_of(&out).len(), 1);
    }

    #[test]
    fn ascii_tool_call_json_variant() {
        let out = run(&[
            "ok <|tool_call|>\n{\"name\": \"bash\", \"arguments\": {\"command\": \"pwd\"}}\n</|tool_call|>",
        ]);
        assert_eq!(text_of(&out), "ok ");
        let calls = calls_of(&out);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].input["command"], json!("pwd"));
    }

    #[test]
    fn json_variant_with_string_encoded_arguments_and_array() {
        let out = run(&[concat!(
            "<|DSML|>[",
            "{\"name\": \"a\", \"arguments\": \"{\\\"x\\\": 1}\"},",
            "{\"name\": \"b\"}",
            "]</|DSML|>",
        )]);
        let calls = calls_of(&out);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].input["x"], json!(1));
        assert!(calls[1].input.is_empty());
    }

    #[test]
    fn unterminated_envelope_is_flushed_as_text_at_finish() {
        let partial = "Working <｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"bash\">";
        let mut parser = DsmlStreamParser::default();
        let mut out = Vec::new();
        parser.push(partial, &mut out);
        assert_eq!(text_of(&out), "Working ", "envelope stays buffered mid-stream");
        parser.finish(&mut out);
        assert_eq!(text_of(&out), partial);
        assert!(calls_of(&out).is_empty());
    }

    #[test]
    fn unterminated_envelope_followed_by_complete_one() {
        let first = "<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"rm\">\n";
        let out = run(&[first, FULL]);
        assert_eq!(text_of(&out), first);
        let calls = calls_of(&out);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
    }

    #[test]
    fn malformed_envelope_is_emitted_as_text() {
        // Wrong attribute name (`tool_name`) — not the DSML grammar.
        let bad = concat!(
            "<｜DSML｜tool_calls>",
            "<｜DSML｜invoke tool_name=\"bash\"></｜DSML｜invoke>",
            "</｜DSML｜tool_calls>",
        );
        let out = run(&["a ", bad, " b"]);
        assert_eq!(text_of(&out), format!("a {bad} b"));
        assert!(calls_of(&out).is_empty());
    }

    #[test]
    fn empty_envelope_is_emitted_as_text() {
        let bad = "<｜DSML｜tool_calls>\n</｜DSML｜tool_calls>";
        let out = run(&[bad]);
        assert_eq!(text_of(&out), bad);
    }

    #[test]
    fn malformed_json_parameter_is_repaired() {
        let out = run(&[concat!(
            "<｜DSML｜tool_calls><｜DSML｜invoke name=\"q\">",
            "<｜DSML｜parameter name=\"items\" string=\"false\">[{\"q\": \"line one\nline two\",},]</｜DSML｜parameter>",
            "</｜DSML｜invoke></｜DSML｜tool_calls>",
        )]);
        let calls = calls_of(&out);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["items"], json!([{"q": "line one\nline two"}]));
    }

    #[test]
    fn unrepairable_json_parameter_falls_back_to_raw_string() {
        let out = run(&[concat!(
            "<｜DSML｜tool_calls><｜DSML｜invoke name=\"q\">",
            "<｜DSML｜parameter name=\"n\" string=\"false\">not json {</｜DSML｜parameter>",
            "</｜DSML｜invoke></｜DSML｜tool_calls>",
        )]);
        let calls = calls_of(&out);
        assert_eq!(calls[0].input["n"], json!("not json {"));
    }

    #[test]
    fn string_parameter_keeps_whitespace_and_markup_verbatim() {
        let content = "  fn main() {\n    println!(\"<hi>\");\n}\n";
        let input = format!(
            "<｜DSML｜tool_calls><｜DSML｜invoke name=\"write\"><｜DSML｜parameter name=\"content\" string=\"true\">{content}</｜DSML｜parameter></｜DSML｜invoke></｜DSML｜tool_calls>"
        );
        let out = run(&[input.as_str()]);
        assert_eq!(calls_of(&out)[0].input["content"], json!(content));
    }

    #[test]
    fn repair_json_leaves_commas_inside_strings() {
        assert_eq!(repair_json(r#"{"a": "x,}", }"#), r#"{"a": "x,}" }"#);
    }

    #[test]
    fn blocks_helper_lifts_tool_calls_and_keeps_text() {
        let mut blocks = vec![ContentBlock::Text {
            text: format!("Checking.{FULL}"),
        }];
        assert!(extract_tool_calls_from_blocks(&mut blocks));
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "Checking."));
        assert!(matches!(
            &blocks[1],
            ContentBlock::ToolUse { id, name, input, .. }
                if id.starts_with("call_") && name == "read_file" && input["limit"] == json!(40)
        ));
    }

    #[test]
    fn blocks_helper_is_noop_without_envelopes() {
        let mut blocks = vec![ContentBlock::Text {
            text: "plain <answer>".to_string(),
        }];
        assert!(!extract_tool_calls_from_blocks(&mut blocks));
        assert!(matches!(&blocks[..], [ContentBlock::Text { text }] if text == "plain <answer>"));
    }
}
