// protocol — wire-format protocol layer (#228).
//
// A *protocol* owns the request-building and response/stream-decoding logic for
// one on-the-wire format, independent of which vendor/endpoint speaks it. This
// mirrors opencode's `packages/llm` protocol/route split, where a single
// `anthropic-messages` / `openai-chat` / `openai-responses` / … implementation
// is shared by every provider that talks that format, and providers collapse to
// config (endpoint + auth + which protocol + model source).
//
// Status (#228): this module currently owns the **stream decoding** half of the
// OpenAI-Chat protocol — the sans-IO [`openai_chat::OpenAiChatDecoder`], factored
// out verbatim from `providers/openai_compat.rs` (the adapter that already backs
// ~35 OpenAI-compatible vendors). Request-building and the other wire formats
// (`AnthropicMessages`, `OpenAiResponses`, `Gemini`, `BedrockConverse`) are not
// yet hoisted here — see the `TODO(#228)` markers in the respective provider
// files. Decoders are kept sans-IO on purpose: they turn already-decoded text
// lines into provider-agnostic [`StreamEvent`]s with no network access, which
// makes them unit-testable without a live endpoint and lets every transport
// (wreq/reqwest, live or replayed) reuse them.

use crate::provider_types::StreamEvent;

pub mod openai_chat;

pub use openai_chat::OpenAiChatDecoder;

/// Sans-IO decoder that turns individual already-UTF-8-decoded SSE / JSON-Lines
/// **text lines** (as produced by [`crate::SseByteDecoder`]) into a stream of
/// provider-agnostic [`StreamEvent`]s.
///
/// One implementation exists per wire format (protocol). Because the decode
/// logic never touches the network, a provider's streaming loop shrinks to:
/// read a chunk → `byte_decoder.push(&chunk)` → `feed_line` each line → yield
/// the events; and the format's decoding can be tested in isolation.
pub trait LineStreamDecoder: Send {
    /// Feed one complete line (trailing `\n` already stripped; `\r` may remain).
    /// Any resulting events are appended to `out`.
    ///
    /// Returns `true` when the wire format signals the stream is finished (e.g.
    /// an OpenAI `data: [DONE]` sentinel) and the caller should stop reading.
    fn feed_line(&mut self, line: &str, out: &mut Vec<StreamEvent>) -> bool;

    /// Called once the underlying byte stream ends. Appends any trailing events
    /// (e.g. a synthesized `MessageStop`) to `out`.
    fn finish(&mut self, out: &mut Vec<StreamEvent>);
}
