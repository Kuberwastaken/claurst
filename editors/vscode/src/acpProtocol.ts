/**
 * Pure message parsing for the newline-delimited JSON-RPC 2.0 wire format
 * used by the Agent Client Protocol, matching
 * src-rust/crates/acp/src/connection.rs. No child_process / IO here — kept
 * separate so the routing logic is unit-testable without spawning anything.
 */

export type ParsedMessage =
  | { kind: 'response'; id: number; result?: unknown; error?: { code: number; message: string; data?: unknown } }
  | { kind: 'request'; id: number; method: string; params: unknown }
  | { kind: 'notification'; method: string; params: unknown };

/** Parses one line of the wire protocol. Returns `null` for a blank line or
 * a line that isn't a well-formed JSON-RPC message. */
export function parseLine(line: string): ParsedMessage | null {
  const trimmed = line.trim();
  if (trimmed.length === 0) {
    return null;
  }

  let msg: any;
  try {
    msg = JSON.parse(trimmed);
  } catch {
    return null;
  }
  if (typeof msg !== 'object' || msg === null) {
    return null;
  }

  const hasId = msg.id !== undefined && msg.id !== null;
  const hasResult = 'result' in msg;
  const hasError = 'error' in msg;
  const hasMethod = typeof msg.method === 'string';

  if (hasId && (hasResult || hasError) && !hasMethod) {
    return { kind: 'response', id: msg.id, result: msg.result, error: msg.error };
  }
  if (hasId && hasMethod) {
    return { kind: 'request', id: msg.id, method: msg.method, params: msg.params };
  }
  if (hasMethod) {
    return { kind: 'notification', method: msg.method, params: msg.params };
  }
  return null;
}

/** Extracts plain text from an ACP `ContentBlock` (only the `text` variant
 * is meaningful for the chat transcript; other variants render as empty). */
export function extractText(content: any): string {
  if (!content) {
    return '';
  }
  if (content.type === 'text') {
    return content.text ?? '';
  }
  return '';
}
