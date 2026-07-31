import * as cp from 'child_process';
import * as readline from 'readline';

/** Minimal newline-delimited JSON-RPC 2.0 client for the Agent Client Protocol,
 * matching the wire format implemented in src-rust/crates/acp/src/connection.rs:
 * one UTF-8 line per message, no Content-Length framing. */

export interface JsonRpcError {
  code: number;
  message: string;
  data?: unknown;
}

export type PermissionOption = {
  optionId: string;
  name: string;
  kind: string;
};

export type ToolCallUpdate = {
  toolCallId?: string;
  title?: string;
  status?: string;
  kind?: string;
};

export interface AcpClientEvents {
  onTextChunk?: (text: string, isThought: boolean) => void;
  onToolCall?: (update: ToolCallUpdate) => void;
  onToolCallUpdate?: (update: ToolCallUpdate) => void;
  /** Must resolve to one of the option ids offered in `options`. */
  onRequestPermission?: (toolCall: ToolCallUpdate, options: PermissionOption[]) => Promise<string>;
  onStderr?: (line: string) => void;
  onExit?: (code: number | null) => void;
}

/** Speaks ACP to a `claurst acp` child process over stdio. */
export class AcpClient {
  private child: cp.ChildProcessWithoutNullStreams;
  private rl: readline.Interface;
  private nextId = 1;
  private pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void }>();
  private sessionId: string | undefined;

  constructor(executablePath: string, cwd: string, private events: AcpClientEvents) {
    this.child = cp.spawn(executablePath, ['acp'], { cwd, stdio: ['pipe', 'pipe', 'pipe'] });
    this.rl = readline.createInterface({ input: this.child.stdout });
    this.rl.on('line', (line) => this.handleLine(line));
    this.child.stderr.on('data', (data: Buffer) => {
      const text = data.toString('utf8');
      for (const line of text.split('\n')) {
        if (line.trim().length > 0) {
          this.events.onStderr?.(line);
        }
      }
    });
    this.child.on('exit', (code) => {
      for (const { reject } of this.pending.values()) {
        reject(new Error('claurst acp process exited'));
      }
      this.pending.clear();
      this.events.onExit?.(code);
    });
  }

  private handleLine(line: string): void {
    const trimmed = line.trim();
    if (trimmed.length === 0) {
      return;
    }
    let msg: any;
    try {
      msg = JSON.parse(trimmed);
    } catch {
      this.events.onStderr?.(`[claurst-vscode] malformed line from agent: ${trimmed}`);
      return;
    }

    const hasId = msg.id !== undefined && msg.id !== null;
    const hasResult = 'result' in msg;
    const hasError = 'error' in msg;
    const hasMethod = typeof msg.method === 'string';

    if (hasId && (hasResult || hasError) && !hasMethod) {
      const pending = this.pending.get(msg.id);
      if (!pending) {
        return;
      }
      this.pending.delete(msg.id);
      if (hasError) {
        pending.reject(Object.assign(new Error(msg.error?.message ?? 'ACP error'), { data: msg.error }));
      } else {
        pending.resolve(msg.result);
      }
      return;
    }

    if (hasId && hasMethod) {
      // Agent → client request. Only session/request_permission is expected in v1.
      this.handleIncomingRequest(msg.id, msg.method, msg.params).catch((e) => {
        this.events.onStderr?.(`[claurst-vscode] failed to handle ${msg.method}: ${e}`);
      });
      return;
    }

    if (hasMethod) {
      this.handleNotification(msg.method, msg.params);
    }
  }

  private async handleIncomingRequest(id: number, method: string, params: any): Promise<void> {
    if (method === 'session/request_permission') {
      const toolCall: ToolCallUpdate = {
        toolCallId: params?.toolCall?.toolCallId,
        title: params?.toolCall?.title,
        status: params?.toolCall?.status,
        kind: params?.toolCall?.kind,
      };
      const options: PermissionOption[] = (params?.options ?? []).map((o: any) => ({
        optionId: o.optionId,
        name: o.name,
        kind: o.kind,
      }));
      const chosen = (await this.events.onRequestPermission?.(toolCall, options)) ?? options[0]?.optionId;
      this.writeMessage({
        jsonrpc: '2.0',
        id,
        result: { outcome: { outcome: 'selected', optionId: chosen } },
      });
      return;
    }

    // Unknown incoming request — respond with method-not-found so the agent
    // doesn't hang waiting for a reply.
    this.writeMessage({
      jsonrpc: '2.0',
      id,
      error: { code: -32601, message: `client does not implement '${method}'` },
    });
  }

  private handleNotification(method: string, params: any): void {
    if (method === 'session/update') {
      const update = params?.update;
      if (!update) {
        return;
      }
      switch (update.sessionUpdate) {
        case 'agent_message_chunk':
          this.events.onTextChunk?.(extractText(update.content), false);
          break;
        case 'agent_thought_chunk':
          this.events.onTextChunk?.(extractText(update.content), true);
          break;
        case 'tool_call':
          this.events.onToolCall?.({
            toolCallId: update.toolCallId,
            title: update.title,
            status: update.status,
            kind: update.kind,
          });
          break;
        case 'tool_call_update':
          this.events.onToolCallUpdate?.({
            toolCallId: update.toolCallId,
            title: update.title,
            status: update.status,
            kind: update.kind,
          });
          break;
        default:
          break;
      }
    }
  }

  private writeMessage(msg: unknown): void {
    this.child.stdin.write(JSON.stringify(msg) + '\n');
  }

  private request<T = any>(method: string, params: unknown): Promise<T> {
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.writeMessage({ jsonrpc: '2.0', id, method, params });
    });
  }

  private notify(method: string, params: unknown): void {
    this.writeMessage({ jsonrpc: '2.0', method, params });
  }

  async initialize(): Promise<void> {
    await this.request('initialize', {
      protocolVersion: 1,
      clientCapabilities: {},
      clientInfo: { name: 'claurst-vscode', version: '0.1.0' },
    });
  }

  async newSession(cwd: string): Promise<string> {
    const result = await this.request<{ sessionId: string }>('session/new', {
      cwd,
      mcpServers: [],
    });
    this.sessionId = result.sessionId;
    return result.sessionId;
  }

  async prompt(text: string): Promise<void> {
    if (!this.sessionId) {
      throw new Error('no active session; call newSession() first');
    }
    await this.request('session/prompt', {
      sessionId: this.sessionId,
      prompt: [{ type: 'text', text }],
    });
  }

  cancel(): void {
    if (this.sessionId) {
      this.notify('session/cancel', { sessionId: this.sessionId });
    }
  }

  dispose(): void {
    this.rl.close();
    this.child.kill();
  }
}

function extractText(content: any): string {
  if (!content) {
    return '';
  }
  if (content.type === 'text') {
    return content.text ?? '';
  }
  return '';
}
