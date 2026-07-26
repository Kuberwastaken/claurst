// protocol::openai_chat — the OpenAI Chat Completions streaming wire format.
//
// `OpenAiChatDecoder` is the sans-IO decoder for the `chat/completions` SSE
// stream shared by OpenAI and the ~35 OpenAI-compatible vendors behind
// `providers/openai_compat.rs`. The logic here is a verbatim extraction of that
// adapter's former inline stream loop (#228): identical event ordering, tool-call
// block indexing, reasoning/thinking handling and finish/usage semantics — only
// now it is a reusable, unit-testable state machine instead of being welded into
// an `async_stream::stream!` block.

use std::collections::HashMap;

use claurst_core::types::{ContentBlock, UsageInfo};
use serde_json::{json, Value};
use tracing::debug;

use crate::protocol::LineStreamDecoder;
use crate::providers::openai::OpenAiProvider;
use crate::provider_types::StreamEvent;

/// Dedicated index for the Thinking content block emitted when a provider
/// streams a `reasoning_content` field (DeepSeek V4, etc.). Chosen to avoid
/// colliding with text (index 0) or tool calls (1 + tc_index).
const THINKING_BLOCK_INDEX: usize = usize::MAX - 100;

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
    /// Keyed by content-block index → (tool_call_id, name, accumulated_args).
    tool_call_buffers: HashMap<usize, (String, String, String)>,
}

impl OpenAiChatDecoder {
    pub fn new(reasoning_field: Option<String>) -> Self {
        Self {
            reasoning_field,
            message_started: false,
            message_id: String::from("unknown"),
            model_name: String::new(),
            thinking_open: false,
            tool_call_buffers: HashMap::new(),
        }
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
                out.push(StreamEvent::TextDelta {
                    index: 0,
                    text: content.to_string(),
                });
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
                    self.tool_call_buffers.insert(
                        block_index,
                        (tc_id.to_string(), name.clone(), String::new()),
                    );
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
                        let block_index = 1 + tc_index;
                        if let Some((_, _, buf)) = self.tool_call_buffers.get_mut(&block_index) {
                            buf.push_str(args_frag);
                        }
                        out.push(StreamEvent::InputJsonDelta {
                            index: block_index,
                            partial_json: args_frag.to_string(),
                        });
                    }
                }
            }
        }

        // finish_reason.
        if let Some(finish_reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            if !finish_reason.is_empty() && finish_reason != "null" {
                // Flush any still-open thinking block first so it is finalized
                // into the assistant message.
                if self.thinking_open {
                    out.push(StreamEvent::ContentBlockStop {
                        index: THINKING_BLOCK_INDEX,
                    });
                    self.thinking_open = false;
                }
                out.push(StreamEvent::ContentBlockStop { index: 0 });
                let mut tc_indices: Vec<usize> = self.tool_call_buffers.keys().cloned().collect();
                tc_indices.sort();
                for idx in tc_indices {
                    out.push(StreamEvent::ContentBlockStop { index: idx });
                }

                let stop_reason = OpenAiProvider::map_finish_reason_pub(finish_reason);
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

    /// Flush a trailing `MessageStop` if the stream produced any content but
    /// ended without an explicit `[DONE]` sentinel.
    pub fn finish(&mut self, out: &mut Vec<StreamEvent>) {
        if self.message_started {
            out.push(StreamEvent::MessageStop);
        }
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
}
