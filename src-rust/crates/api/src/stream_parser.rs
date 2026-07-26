// stream_parser.rs — Trait and marker structs for parsing provider HTTP
// response bodies into unified `StreamEvent` streams.
//
// Concrete parsing logic lives in the provider-specific adapter crates and
// will be filled in during Phase 2A.

use async_trait::async_trait;
use claurst_core::provider_id::ProviderId;
use futures::Stream;
use std::pin::Pin;

use crate::provider_error::ProviderError;
use crate::provider_types::StreamEvent;

// ---------------------------------------------------------------------------
// SseByteDecoder — shared byte-buffering line decoder (#228)
// ---------------------------------------------------------------------------

/// Byte-buffering line decoder shared by every provider SSE / JSON-Lines loop.
///
/// Historically each provider decoded raw network chunks with
/// `String::from_utf8_lossy(&chunk)` *per chunk* and stitched the resulting
/// strings together with a `leftover: String`. That corrupts (or, for Google,
/// silently drops) any multibyte UTF-8 codepoint whose bytes straddle a network
/// chunk boundary — including bytes inside tool-call JSON — because the trailing
/// partial codepoint of one chunk is replaced with U+FFFD before it can be
/// joined to the continuation bytes in the next chunk.
///
/// This decoder buffers the *raw bytes* instead. On each [`push`](Self::push)
/// it appends the chunk, splits on the **last** `\n` byte, decodes only the
/// complete prefix (which — because `\n` (0x0A) can never appear inside a
/// multibyte UTF-8 sequence — always ends on a codepoint boundary) and retains
/// any trailing partial bytes (an incomplete line and/or an incomplete
/// codepoint) for the next call. Complete lines are therefore always decoded
/// from whole codepoints.
#[derive(Debug, Default)]
pub struct SseByteDecoder {
    buf: Vec<u8>,
}

impl SseByteDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Feed one raw network chunk. Returns every complete line that is now
    /// available (each without its trailing `\n`, `\r` preserved). Trailing
    /// bytes after the last `\n` — a partial line and/or a partial multibyte
    /// codepoint — are buffered until a subsequent call completes them.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);

        // Everything up to and including the last '\n' forms complete lines.
        let Some(last_nl) = self.buf.iter().rposition(|&b| b == b'\n') else {
            // No newline yet — hold the whole buffer (may end mid-codepoint).
            return Vec::new();
        };

        // Retain the bytes *after* the last '\n' (the partial tail); take the
        // complete prefix for decoding. `\n` is ASCII so the prefix always ends
        // on a UTF-8 codepoint boundary and lossy decoding cannot split a char.
        let tail = self.buf.split_off(last_nl + 1);
        let complete = std::mem::replace(&mut self.buf, tail);

        let decoded = String::from_utf8_lossy(&complete);
        let mut lines: Vec<String> = decoded.split('\n').map(str::to_string).collect();
        // The prefix ends with '\n', so `split` yields a trailing empty piece
        // that is an artifact of the split, not a real (blank) SSE line — drop
        // exactly that one. Genuine blank lines between the newlines survive.
        lines.pop();
        lines
    }

    /// Return any bytes buffered but not yet terminated by a `\n`, decoding
    /// them lossily and clearing the buffer.
    ///
    /// Providers historically discarded an unterminated trailing line at EOF
    /// (it lived in `leftover` and was dropped), so the SSE loops do **not**
    /// call this — it exists for callers that want the final partial line.
    pub fn flush(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let s = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        Some(s)
    }
}

// ---------------------------------------------------------------------------
// StreamParser
// ---------------------------------------------------------------------------

/// Parses an HTTP response body into a stream of provider-agnostic
/// `StreamEvent`s.
///
/// Each provider adapter provides its own `StreamParser` implementation that
/// knows how to decode the wire format (SSE, JSON Lines, etc.) used by that
/// provider.
#[async_trait]
pub trait StreamParser: Send + Sync {
    /// Consume a `reqwest::Response` and produce a pinned stream of
    /// `StreamEvent`s.
    ///
    /// The returned stream yields `Ok(event)` for each successfully decoded
    /// event and `Err(ProviderError)` if parsing fails mid-stream.
    async fn parse(
        &self,
        response: reqwest::Response,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    >;
}

// ---------------------------------------------------------------------------
// SseStreamParser  (marker — implementation deferred to Phase 2A)
// ---------------------------------------------------------------------------

/// Marker for SSE-based stream parsers used by Anthropic, Google Gemini, etc.
///
/// The actual parsing logic will be implemented in Phase 2A.
pub struct SseStreamParser;

impl SseStreamParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SseStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StreamParser for SseStreamParser {
    async fn parse(
        &self,
        _response: reqwest::Response,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        // Will be implemented in Phase 2A.
        Err(ProviderError::Other {
            provider: ProviderId::new("unknown"),
            message: "SseStreamParser::parse is not yet implemented".to_string(),
            status: None,
            body: None,
        })
    }
}

// ---------------------------------------------------------------------------
// JsonLinesStreamParser  (marker — implementation deferred to Phase 2A)
// ---------------------------------------------------------------------------

/// Marker for JSON Lines stream parsers used by OpenAI, Azure OpenAI, etc.
///
/// The actual parsing logic will be implemented in Phase 2A.
pub struct JsonLinesStreamParser;

impl JsonLinesStreamParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JsonLinesStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StreamParser for JsonLinesStreamParser {
    async fn parse(
        &self,
        _response: reqwest::Response,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        // Will be implemented in Phase 2A.
        Err(ProviderError::Other {
            provider: ProviderId::new("unknown"),
            message: "JsonLinesStreamParser::parse is not yet implemented".to_string(),
            status: None,
            body: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::SseByteDecoder;

    /// Feed a full SSE payload as a single chunk and collect the lines.
    #[test]
    fn single_chunk_splits_into_lines() {
        let mut d = SseByteDecoder::new();
        let lines = d.push(b"event: message_start\ndata: {\"a\":1}\n\n");
        assert_eq!(lines, vec!["event: message_start", "data: {\"a\":1}", ""]);
        // Nothing buffered — everything ended on a newline.
        assert_eq!(d.flush(), None);
    }

    /// A partial trailing line (no newline yet) is held back until completed.
    #[test]
    fn partial_line_is_buffered_until_newline() {
        let mut d = SseByteDecoder::new();
        assert!(d.push(b"data: hel").is_empty());
        assert!(d.push(b"lo wor").is_empty());
        let lines = d.push(b"ld\n");
        assert_eq!(lines, vec!["data: hello world"]);
    }

    /// A multibyte emoji split across two chunks decodes intact (the core #228
    /// UTF-8 chunk-boundary fix). Naive per-chunk `from_utf8_lossy` would emit
    /// U+FFFD replacement characters here.
    #[test]
    fn multibyte_emoji_split_across_chunks_is_intact() {
        let emoji = "🚀"; // 4 bytes: F0 9F 9A 80
        let bytes = emoji.as_bytes();
        let mut d = SseByteDecoder::new();

        // Split the emoji down the middle across two network chunks.
        let mut first = b"data: ".to_vec();
        first.extend_from_slice(&bytes[..2]);
        let mut second = bytes[2..].to_vec();
        second.extend_from_slice(b"!\n");

        assert!(d.push(&first).is_empty(), "no complete line yet");
        let lines = d.push(&second);
        assert_eq!(lines, vec![format!("data: {emoji}!")]);
        // No replacement characters leaked through.
        assert!(!lines[0].contains('\u{FFFD}'));
    }

    /// A CJK character split across chunks also survives.
    #[test]
    fn cjk_split_across_chunks_is_intact() {
        let s = "日本語"; // each char is 3 UTF-8 bytes
        let bytes = s.as_bytes();
        let mut d = SseByteDecoder::new();
        // Split in the middle of the second character.
        assert!(d.push(&bytes[..4]).is_empty());
        let mut rest = bytes[4..].to_vec();
        rest.push(b'\n');
        let lines = d.push(&rest);
        assert_eq!(lines, vec![s]);
    }

    /// A tool-call JSON fragment whose multibyte content is split across chunk
    /// boundaries reassembles into valid, parseable JSON.
    #[test]
    fn tool_call_json_fragment_split_across_chunks_assembles() {
        // A realistic streamed tool_use argument containing a multibyte char.
        let full = "data: {\"name\":\"search\",\"arguments\":{\"q\":\"café ☕\"}}\n";
        let bytes = full.as_bytes();

        // Split at every possible boundary to stress mid-codepoint splits.
        let mut assembled: Vec<String> = Vec::new();
        for i in 1..bytes.len() {
            let mut d2 = SseByteDecoder::new();
            d2.push(&bytes[..i]);
            let mut lines = d2.push(&bytes[i..]);
            assembled = std::mem::take(&mut lines);
            assert_eq!(assembled.len(), 1, "split at {i} should yield one line");
            let line = &assembled[0];
            assert!(!line.contains('\u{FFFD}'), "no U+FFFD when split at {i}");
            let data = line.strip_prefix("data: ").expect("data prefix");
            let v: serde_json::Value =
                serde_json::from_str(data).expect("valid JSON regardless of split point");
            assert_eq!(v["arguments"]["q"], "café ☕");
        }
        assert_eq!(assembled, vec!["data: {\"name\":\"search\",\"arguments\":{\"q\":\"café ☕\"}}"]);
    }

    /// Multiple complete lines that arrive together are all returned; the split
    /// artifact (empty piece after the final '\n') is dropped, but genuine blank
    /// lines between events survive so SSE frame terminators are preserved.
    #[test]
    fn multiple_frames_and_blank_lines_preserved() {
        let mut d = SseByteDecoder::new();
        let lines = d.push(b"data: a\n\ndata: b\n\n");
        assert_eq!(lines, vec!["data: a", "", "data: b", ""]);
    }

    /// `\r\n` line endings keep the `\r` (callers trim it themselves).
    #[test]
    fn carriage_returns_are_preserved_for_caller() {
        let mut d = SseByteDecoder::new();
        let lines = d.push(b"data: x\r\n");
        assert_eq!(lines, vec!["data: x\r"]);
    }

    /// Content only becomes available once the terminating newline of the line
    /// arrives, even across many tiny single-byte chunks.
    #[test]
    fn byte_at_a_time_delivery() {
        let payload = "data: hi 世界\n";
        let mut d = SseByteDecoder::new();
        let mut out: Vec<String> = Vec::new();
        for b in payload.as_bytes() {
            out.extend(d.push(&[*b]));
        }
        assert_eq!(out, vec!["data: hi 世界"]);
    }
}
