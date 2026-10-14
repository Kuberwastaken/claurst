// protocol::openai_chat — the OpenAI Chat Completions streaming wire format.
//
// `OpenAiChatDecoder` is the sans-IO decoder for the `chat/completions` SSE
// stream shared by OpenAI and the ~35 OpenAI-compatible vendors behind
// `providers/openai_compat.rs`. The logic here is a verbatim extraction of that
// adapter's former inline stream loop (#228): identical event ordering, tool-call
// block indexing, reasoning/thinking handling and finish/usage semantics — only
// now it is a reusable, unit-testable state machine instead of being welded into
// an `async_stream::stream!` block.

use std::collections::BTreeSet;

use claurst_core::types::{ContentBlock, UsageInfo};
use serde_json::{json, Value};
use tracing::debug;

use crate::protocol::dsml::{new_tool_use_id, DsmlSegment, DsmlStreamParser};
use crate::protocol::LineStreamDecoder;
use crate::providers::openai::OpenAiProvider;
use crate::provider_types::{StopReason, StreamEvent};

/// Dedicated index for the Thinking content block emitted when a provider
/// streams a `reasoning_content` field (DeepSeek V4, etc.). Chosen to avoid
/// colliding with text (index 0) or tool calls (1 + tc_index).
const THINKING_BLOCK_INDEX: usize = usize::MAX - 100;

/// First content-block index for tool calls lifted out of DeepSeek DSML text
/// envelopes (#395). Far above native `1 + tc_index` blocks and below
/// [`THINKING_BLOCK_INDEX`].
const DSML_TOOL_BLOCK_BASE: usize = usize::MAX / 2;

/// Streaming decoder for the OpenAI Chat Completions SSE format.
///
/// Construct with [`OpenAiChatDecoder::new`], feed each SSE line via
/// [`feed_line`](Self::feed_line), and after the byte stream ends call
/// [`finish`](Self::finish) to flush a trailing `MessageStop`.
pub struct OpenAiChatDecoder {
    /// Provider-specific reasoning field name (e.g. DeepSeek's
    /// `reasoning_content`), checked before the common fallbacks.
    reasoning_field: Option<String>,
    message_started: bool,
    message_id: String,
    model_name: String,
    thinking_open: bool,
    /// Content-block indices of every tool-use block opened in this stream
    /// (native and DSML), closed in index order on `finish_reason`.
    tool_blocks: BTreeSet<usize>,
    /// Extracts DeepSeek DSML tool-call envelopes from text content (#395).
    /// `None` unless enabled via [`with_dsml_tool_calls`](Self::with_dsml_tool_calls).
    dsml: Option<DsmlStreamParser>,
    /// Number of tool calls lifted out of DSML envelopes in this stream.
    dsml_tool_calls: usize,
    /// Set once a `finish_reason` or `[DONE]` arrived: the response is
    /// complete. A byte stream that ends without it was truncated (#439).
    complete: bool,
}

impl OpenAiChatDecoder {
    pub fn new(reasoning_field: Option<String>) -> Self {
        Self {
            reasoning_field,
            message_started: false,
            message_id: String::from("unknown"),
            model_name: String::new(),
            thinking_open: false,
            tool_blocks: BTreeSet::new(),
            dsml: None,
            dsml_tool_calls: 0,
            complete: false,
        }
    }

    /// Parse DeepSeek DSML tool-call envelopes out of `delta.content` text and
    /// surface them as `ToolUse` blocks instead of assistant text (#395).
    /// Independently of this flag, parsing is switched on when the first
    /// chunk's `model` names a DeepSeek model (gateway aliases, auto-routing).
    pub fn with_dsml_tool_calls(mut self, enabled: bool) -> Self {
        self.dsml = enabled.then(DsmlStreamParser::default);
        self
    }

    /// Turn DSML parser output into stream events. Tool-use blocks are
    /// registered in `tool_blocks` so they are closed on finish like native
    /// tool calls.
    fn emit_dsml_segments(&mut self, segments: Vec<DsmlSegment>, out: &mut Vec<StreamEvent>) {
        for segment in segments {
            match segment {
                DsmlSegment::Text(text) => out.push(StreamEvent::TextDelta { index: 0, text }),
                DsmlSegment::ToolCall(call) => {
                    let index = DSML_TOOL_BLOCK_BASE + self.dsml_tool_calls;
                    self.dsml_tool_calls += 1;
                    self.tool_blocks.insert(index);
                    out.push(StreamEvent::ContentBlockStart {
                        index,
                        content_block: ContentBlock::ToolUse {
                            id: new_tool_use_id(),
                            name: call.name,
                            input: json!({}),
                            thought_signature: None,
                        },
                    });
                    out.push(StreamEvent::InputJsonDelta {
                        index,
                        partial_json: Value::Object(call.input).to_string(),
                    });
                }
            }
        }
    }

    /// Flush text still held by the DSML parser (a partial opener or an
    /// unterminated envelope) so it is never lost when the message ends.
    fn flush_dsml(&mut self, out: &mut Vec<StreamEvent>) {
        let Some(parser) = self.dsml.as_mut() else {
            return;
        };
        let mut segments = Vec::new();
        parser.finish(&mut segments);
        self.emit_dsml_segments(segments, out);
    }

    /// Feed one SSE line. See [`LineStreamDecoder::feed_line`].
    pub fn feed_line(&mut self, line: &str, out: &mut Vec<StreamEvent>) -> bool {
        let line = line.trim_end_matches('\r').trim();

        if line.is_empty() || line.starts_with(':') {
            return false;
        }

        let data = match line.strip_prefix("data:") {
            Some(rest) => rest.trim(),
            None => return false,
        };

        if data == "[DONE]" {
            self.complete = true;
            self.flush_dsml(out);
            out.push(StreamEvent::MessageStop);
            return true;
        }

        let chunk_json: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                debug!("Failed to parse SSE chunk: {}: {}", e, data);
                return false;
            }
        };

        if !self.message_started {
            if let Some(id) = chunk_json.get("id").and_then(|v| v.as_str()) {
                self.message_id = id.to_string();
            }
            if let Some(m) = chunk_json.get("model").and_then(|v| v.as_str()) {
                self.model_name = m.to_string();
            }
            // Gateway aliases (e.g. opencode-zen `big-pickle`) and auto-routing
            // hide the upstream model from the request; the served model id
            // arrives here, before any content delta (#395).
            if self.dsml.is_none() && self.model_name.to_ascii_lowercase().contains("deepseek") {
                self.dsml = Some(DsmlStreamParser::default());
            }
            out.push(StreamEvent::MessageStart {
                id: self.message_id.clone(),
                model: self.model_name.clone(),
                usage: UsageInfo::default(),
            });
            out.push(StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text { text: String::new() },
            });
            self.message_started = true;
        }

        let choices = match chunk_json.get("choices").and_then(|c| c.as_array()) {
            Some(c) => c,
            None => {
                if let Some(usage_val) = chunk_json.get("usage") {
                    let usage = OpenAiProvider::parse_usage_pub(Some(usage_val));
                    out.push(StreamEvent::MessageDelta {
                        stop_reason: None,
                        usage: Some(usage),
                    });
                }
                return false;
            }
        };

        let choice = match choices.first() {
            Some(c) => c,
            None => return false,
        };

        let delta = match choice.get("delta") {
            Some(d) => d,
            None => return false,
        };

        // Reasoning / thinking extraction.
        // Check the provider-specific field first (e.g. DeepSeek's
        // "reasoning_content"), then fall back to common field names used by
        // other providers (Copilot "reasoning_text", generic "reasoning", etc.).
        // This allows reasoning traces to show for any provider that emits them
        // without needing explicit per-provider configuration.
        {
            const COMMON_REASONING_FIELDS: &[&str] = &[
                "reasoning_content", // DeepSeek
                "reasoning_text",    // GitHub Copilot
                "reasoning",         // Generic / future
            ];
            let fields_to_check: Vec<&str> = if let Some(ref f) = self.reasoning_field {
                // Provider-specific field first, then common ones.
                let mut v = vec![f.as_str()];
                for common in COMMON_REASONING_FIELDS {
                    if *common != f.as_str() {
                        v.push(common);
                    }
                }
                v
            } else {
                COMMON_REASONING_FIELDS.to_vec()
            };
            for field in &fields_to_check {
                if let Some(reasoning) = delta.get(*field).and_then(|v| v.as_str()) {
                    if !reasoning.is_empty() {
                        // Open a dedicated Thinking block on first reasoning
                        // delta so the accumulator has a partial to append into
                        // (see StreamAccumulator::on_event). Without this start
                        // event the reasoning deltas would be dropped and the
                        // completed assistant message would not carry any
                        // ContentBlock::Thinking — which is what DeepSeek V4
                        // thinking mode requires the client to echo back on
                        // subsequent turns.
                        if !self.thinking_open {
                            out.push(StreamEvent::ContentBlockStart {
                                index: THINKING_BLOCK_INDEX,
                                content_block: ContentBlock::Thinking {
                                    thinking: String::new(),
                                    signature: String::new(),
                                },
                            });
                            self.thinking_open = true;
                        }
                        out.push(StreamEvent::ReasoningDelta {
                            index: THINKING_BLOCK_INDEX,
                            reasoning: reasoning.to_string(),
                        });
                        break;
                    }
                }
            }
        }

        // Text content delta.
        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                // Close any open thinking block before visible text starts
                // streaming, so the blocks land in order in the final message:
                // [Thinking, Text, ToolUse...].
                if self.thinking_open {
                    out.push(StreamEvent::ContentBlockStop {
                        index: THINKING_BLOCK_INDEX,
                    });
                    self.thinking_open = false;
                }
                match self.dsml.as_mut() {
                    Some(parser) => {
                        let mut segments = Vec::new();
                        parser.push(content, &mut segments);
                        self.emit_dsml_segments(segments, out);
                    }
                    None => out.push(StreamEvent::TextDelta {
                        index: 0,
                        text: content.to_string(),
                    }),
                }
            }
        }

        // Tool call deltas.
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            // Close any open thinking block before tool calls start (same
            // ordering guarantee as for text above).
            if self.thinking_open {
                out.push(StreamEvent::ContentBlockStop {
                    index: THINKING_BLOCK_INDEX,
                });
                self.thinking_open = false;
            }
            for tc in tool_calls {
                let tc_index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if let Some(tc_id) = tc.get("id").and_then(|v| v.as_str()) {
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let block_index = 1 + tc_index;
                    self.tool_blocks.insert(block_index);
                    out.push(StreamEvent::ContentBlockStart {
                        index: block_index,
                        content_block: ContentBlock::ToolUse {
                            id: tc_id.to_string(),
                            name,
                            input: json!({}),
                            thought_signature: None,
                        },
                    });
                }
                if let Some(args_frag) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                {
                    if !args_frag.is_empty() {
                        out.push(StreamEvent::InputJsonDelta {
                            index: 1 + tc_index,
                            partial_json: args_frag.to_string(),
                        });
                    }
                }
            }
        }

        // finish_reason.
        if let Some(finish_reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            if !finish_reason.is_empty() && finish_reason != "null" {
                self.complete = true;
                // Flush any still-open thinking block first so it is finalized
                // into the assistant message.
                if self.thinking_open {
                    out.push(StreamEvent::ContentBlockStop {
                        index: THINKING_BLOCK_INDEX,
                    });
                    self.thinking_open = false;
                }
                self.flush_dsml(out);
                out.push(StreamEvent::ContentBlockStop { index: 0 });
                for &index in &self.tool_blocks {
                    out.push(StreamEvent::ContentBlockStop { index });
                }

                let mut stop_reason = OpenAiProvider::map_finish_reason_pub(finish_reason);
                // DSML tool calls arrive as text, so providers report a plain
                // "stop"; the turn still has to run the extracted tools.
                if self.dsml_tool_calls > 0 && stop_reason == StopReason::EndTurn {
                    stop_reason = StopReason::ToolUse;
                }
                let usage_val = chunk_json.get("usage");
                let usage = usage_val.map(|u| OpenAiProvider::parse_usage_pub(Some(u)));

                out.push(StreamEvent::MessageDelta {
                    stop_reason: Some(stop_reason),
                    usage,
                });
            }
        }

        false
    }

    /// Flush a trailing `MessageStop` when the response completed (a
    /// `finish_reason` arrived) but no explicit `[DONE]` sentinel followed.
    /// A truncated stream gets no `MessageStop`; check
    /// [`is_complete`](Self::is_complete) to report it.
    pub fn finish(&mut self, out: &mut Vec<StreamEvent>) {
        if self.complete && self.message_started {
            self.flush_dsml(out);
            out.push(StreamEvent::MessageStop);
        }
    }

    /// Whether a `finish_reason` or `[DONE]` has been seen.
    pub fn is_complete(&self) -> bool {
        self.complete
    }
}

impl LineStreamDecoder for OpenAiChatDecoder {
    fn feed_line(&mut self, line: &str, out: &mut Vec<StreamEvent>) -> bool {
        OpenAiChatDecoder::feed_line(self, line, out)
    }

    fn finish(&mut self, out: &mut Vec<StreamEvent>) {
        OpenAiChatDecoder::finish(self, out)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a slice of lines and return every event produced (excluding the
    /// stop signal, which is asserted separately where it matters).
    fn drain(decoder: &mut OpenAiChatDecoder, lines: &[&str]) -> (Vec<StreamEvent>, bool) {
        let mut out = Vec::new();
        let mut done = false;
        for l in lines {
            if decoder.feed_line(l, &mut out) {
                done = true;
                break;
            }
        }
        (out, done)
    }

    #[test]
    fn text_stream_emits_start_delta_finish() {
        let mut d = OpenAiChatDecoder::new(None);
        let (events, done) = drain(
            &mut d,
            &[
                r#"data: {"id":"chatcmpl-1","model":"gpt-x","choices":[{"delta":{"content":"Hello"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":" world"}}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ],
        );
        assert!(!done, "no [DONE] fed yet");

        // MessageStart carries the id/model; a text block opens at index 0.
        assert!(matches!(
            &events[0],
            StreamEvent::MessageStart { id, model, .. } if id == "chatcmpl-1" && model == "gpt-x"
        ));
        assert!(matches!(
            &events[1],
            StreamEvent::ContentBlockStart { index: 0, content_block: ContentBlock::Text { .. } }
        ));

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { index: 0, text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");

        // finish_reason closes block 0 and emits a MessageDelta.
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ContentBlockStop { index: 0 })));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::MessageDelta { stop_reason: Some(_), .. })));

        // finish() flushes MessageStop since content was produced.
        let mut tail = Vec::new();
        d.finish(&mut tail);
        assert!(matches!(tail.as_slice(), [StreamEvent::MessageStop]));
    }

    #[test]
    fn done_sentinel_stops_and_emits_message_stop() {
        let mut d = OpenAiChatDecoder::new(None);
        let (events, done) = drain(
            &mut d,
            &[
                r#"data: {"id":"c","model":"m","choices":[{"delta":{"content":"hi"}}]}"#,
                "data: [DONE]",
            ],
        );
        assert!(done, "[DONE] must stop the stream");
        assert!(matches!(events.last(), Some(StreamEvent::MessageStop)));
    }

    #[test]
    fn truncated_stream_is_not_complete_and_gets_no_message_stop() {
        let mut d = OpenAiChatDecoder::new(None);
        let (events, done) = drain(&mut d, &[r#"data: {"choices":[{"delta":{"content":"Hel"}}]}"#]);
        assert!(!done);
        assert!(!events.is_empty());
        let mut tail = Vec::new();
        d.finish(&mut tail);
        assert!(!d.is_complete(), "no finish_reason / [DONE] arrived");
        assert!(
            !tail.iter().any(|e| matches!(e, StreamEvent::MessageStop)),
            "a truncated stream must not be closed as if complete"
        );
    }

    #[test]
    fn tool_call_arguments_assemble_across_lines() {
        let mut d = OpenAiChatDecoder::new(None);
        let (events, _done) = drain(
            &mut d,
            &[
                r#"data: {"id":"c","model":"m","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":""}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );

        // The tool block opens at index 1 (1 + tc_index) with id + name.
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlock::ToolUse { id, name, .. }
            } if id == "call_1" && name == "get_weather"
        )));

        // The streamed argument fragments concatenate into valid JSON.
        let args: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::InputJsonDelta { index: 1, partial_json } => Some(partial_json.clone()),
                _ => None,
            })
            .collect();
        let parsed: Value = serde_json::from_str(&args).expect("assembled tool args must be valid JSON");
        assert_eq!(parsed["city"], "Paris");

        // finish closes text block 0 and the tool block 1.
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ContentBlockStop { index: 1 })));
    }

    #[test]
    fn reasoning_opens_thinking_block_then_text_closes_it() {
        let mut d = OpenAiChatDecoder::new(None);
        let (events, _done) = drain(
            &mut d,
            &[
                r#"data: {"id":"c","model":"m","choices":[{"delta":{"reasoning_content":"pondering"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":"answer"}}]}"#,
            ],
        );

        // A Thinking block opens at THINKING_BLOCK_INDEX and receives the delta.
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlock::Thinking { .. }
            } if *index == THINKING_BLOCK_INDEX
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ReasoningDelta { index, reasoning } if *index == THINKING_BLOCK_INDEX && reasoning == "pondering"
        )));
        // Visible text closes the thinking block first, preserving block order.
        let stop_pos = events
            .iter()
            .position(|e| matches!(e, StreamEvent::ContentBlockStop { index } if *index == THINKING_BLOCK_INDEX));
        let text_pos = events
            .iter()
            .position(|e| matches!(e, StreamEvent::TextDelta { index: 0, .. }));
        assert!(stop_pos.is_some() && text_pos.is_some());
        assert!(stop_pos < text_pos, "thinking block must close before text");
    }

    /// A provider-specific reasoning field (DeepSeek-style) is honoured.
    #[test]
    fn custom_reasoning_field_is_checked_first() {
        let mut d = OpenAiChatDecoder::new(Some("thinking_blob".to_string()));
        let (events, _done) = drain(
            &mut d,
            &[r#"data: {"id":"c","model":"m","choices":[{"delta":{"thinking_blob":"hmm"}}]}"#],
        );
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ReasoningDelta { reasoning, .. } if reasoning == "hmm"
        )));
    }

    /// A usage-only chunk with no `choices` yields a usage MessageDelta and does
    /// not terminate the stream.
    #[test]
    fn usage_only_chunk_yields_message_delta() {
        let mut d = OpenAiChatDecoder::new(None);
        // Prime message_started so the usage-only branch is reached the same way
        // it is in a real stream.
        let mut out = Vec::new();
        d.feed_line(
            r#"data: {"id":"c","model":"m","choices":[{"delta":{"content":"x"}}]}"#,
            &mut out,
        );
        out.clear();
        let stop = d.feed_line(
            r#"data: {"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
            &mut out,
        );
        assert!(!stop);
        assert!(matches!(
            out.as_slice(),
            [StreamEvent::MessageDelta { stop_reason: None, usage: Some(_) }]
        ));
    }

    /// SSE line carrying `content` as a delta.
    fn content_line(content: &str) -> String {
        format!(
            "data: {}",
            json!({"id": "c", "model": "test-model", "choices": [{"delta": {"content": content}}]})
        )
    }

    const DSML_ENVELOPE: &str = concat!(
        "<｜DSML｜tool_calls>\n",
        "<｜DSML｜invoke name=\"read_file\">\n",
        "<｜DSML｜parameter name=\"path\" string=\"true\">a.rs</｜DSML｜parameter>\n",
        "</｜DSML｜invoke>\n",
        "<｜DSML｜invoke name=\"bash\">\n",
        "<｜DSML｜parameter name=\"command\" string=\"true\">ls</｜DSML｜parameter>\n",
        "<｜DSML｜parameter name=\"timeout\" string=\"false\">30</｜DSML｜parameter>\n",
        "</｜DSML｜invoke>\n",
        "</｜DSML｜tool_calls>",
    );

    fn text_deltas(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { index: 0, text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// (index, id, name, assembled args) for every tool-use block opened.
    fn tool_uses(events: &[StreamEvent]) -> Vec<(usize, String, String, Value)> {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlock::ToolUse { id, name, .. },
                } => {
                    let args: String = events
                        .iter()
                        .filter_map(|e| match e {
                            StreamEvent::InputJsonDelta { index: i, partial_json } if i == index => {
                                Some(partial_json.as_str())
                            }
                            _ => None,
                        })
                        .collect();
                    let args = serde_json::from_str(&args).expect("tool args are valid JSON");
                    Some((*index, id.clone(), name.clone(), args))
                }
                _ => None,
            })
            .collect()
    }

    fn assert_dsml_tool_uses(events: &[StreamEvent]) {
        let tools = tool_uses(events);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].2, "read_file");
        assert_eq!(tools[0].3, json!({"path": "a.rs"}));
        assert_eq!(tools[1].2, "bash");
        assert_eq!(tools[1].3, json!({"command": "ls", "timeout": 30}));
        assert_ne!(tools[0].1, tools[1].1, "synthesized ids must be unique");
        assert!(tools.iter().all(|t| t.1.starts_with("call_")));
        for (index, ..) in &tools {
            assert!(events
                .iter()
                .any(|e| matches!(e, StreamEvent::ContentBlockStop { index: i } if i == index)));
        }
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::MessageDelta { stop_reason: Some(StopReason::ToolUse), .. }
        )));
        let text_stop = events
            .iter()
            .position(|e| matches!(e, StreamEvent::ContentBlockStop { index: 0 }));
        let last_text = events
            .iter()
            .rposition(|e| matches!(e, StreamEvent::TextDelta { .. }));
        assert!(last_text < text_stop, "all text precedes the text block stop");
    }

    #[test]
    fn dsml_envelope_across_many_deltas_becomes_tool_calls() {
        let full = format!("I'll check.\n{DSML_ENVELOPE}\nThen report.");
        let chars: Vec<char> = full.chars().collect();
        let mut lines: Vec<String> = chars
            .chunks(3)
            .map(|c| content_line(&c.iter().collect::<String>()))
            .collect();
        lines.push(r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#.to_string());
        lines.push("data: [DONE]".to_string());

        let mut d = OpenAiChatDecoder::new(None).with_dsml_tool_calls(true);
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (events, done) = drain(&mut d, &refs);
        assert!(done);
        assert_eq!(text_deltas(&events), "I'll check.\n\nThen report.");
        assert_dsml_tool_uses(&events);
    }

    /// Bytes split anywhere (including inside the 3-byte fullwidth bars) are
    /// reassembled by the SSE byte decoder before DSML parsing.
    #[test]
    fn dsml_envelope_survives_byte_level_splits() {
        let mut wire = String::new();
        for piece in ["ok ", DSML_ENVELOPE] {
            wire.push_str(&content_line(piece));
            wire.push('\n');
        }
        wire.push_str("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n");

        let mut bytes = crate::SseByteDecoder::new();
        let mut d = OpenAiChatDecoder::new(None).with_dsml_tool_calls(true);
        let mut events = Vec::new();
        for byte in wire.as_bytes() {
            for line in bytes.push(std::slice::from_ref(byte)) {
                d.feed_line(&line, &mut events);
            }
        }
        d.finish(&mut events);
        assert_eq!(text_deltas(&events), "ok ");
        assert_dsml_tool_uses(&events);
    }

    #[test]
    fn dsml_disabled_passes_envelope_through_as_text() {
        let line = content_line(DSML_ENVELOPE);
        let mut d = OpenAiChatDecoder::new(None);
        let (events, _) = drain(
            &mut d,
            &[line.as_str(), r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#],
        );
        assert_eq!(text_deltas(&events), DSML_ENVELOPE);
        assert!(tool_uses(&events).is_empty());
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::MessageDelta { stop_reason: Some(StopReason::EndTurn), .. }
        )));
    }

    /// Request model is an alias (`big-pickle`), so the decoder is built with
    /// DSML off; the served model in the first chunk switches it on.
    #[test]
    fn dsml_enabled_by_first_chunk_model() {
        let first = format!(
            "data: {}",
            json!({"id": "c", "model": "DeepSeek-V4-Flash", "choices": [{"delta": {"content": "ok "}}]})
        );
        let envelope = content_line(DSML_ENVELOPE);
        let mut d = OpenAiChatDecoder::new(None).with_dsml_tool_calls(false);
        let (events, _) = drain(
            &mut d,
            &[
                first.as_str(),
                envelope.as_str(),
                r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ],
        );
        assert_eq!(text_deltas(&events), "ok ");
        assert_dsml_tool_uses(&events);
    }

    #[test]
    fn dsml_stays_off_for_non_deepseek_chunk_model() {
        let first = format!(
            "data: {}",
            json!({"id": "c", "model": "big-pickle", "choices": [{"delta": {"content": DSML_ENVELOPE}}]})
        );
        let mut d = OpenAiChatDecoder::new(None).with_dsml_tool_calls(false);
        let (events, _) = drain(
            &mut d,
            &[first.as_str(), r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#],
        );
        assert_eq!(text_deltas(&events), DSML_ENVELOPE);
        assert!(tool_uses(&events).is_empty());
    }

    #[test]
    fn unterminated_dsml_is_flushed_as_text_before_message_stop() {
        let partial = "<｜DSML｜tool_calls>\n<｜DSML｜invoke name=\"bash\">";
        let first = content_line("Trying ");
        let second = content_line(partial);
        let mut d = OpenAiChatDecoder::new(None).with_dsml_tool_calls(true);
        let (events, done) = drain(&mut d, &[first.as_str(), second.as_str(), "data: [DONE]"]);
        assert!(done);
        assert_eq!(text_deltas(&events), format!("Trying {partial}"));
        assert!(tool_uses(&events).is_empty());
        assert!(matches!(events.last(), Some(StreamEvent::MessageStop)));
    }

    #[test]
    fn unterminated_dsml_is_flushed_on_finish_reason() {
        let partial = content_line("<｜DSML｜tool_calls><｜DSML｜inv");
        let mut d = OpenAiChatDecoder::new(None).with_dsml_tool_calls(true);
        let (events, _) = drain(
            &mut d,
            &[partial.as_str(), r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#],
        );
        assert_eq!(text_deltas(&events), "<｜DSML｜tool_calls><｜DSML｜inv");
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::MessageDelta { stop_reason: Some(StopReason::MaxTokens), .. }
        )));
    }
}
