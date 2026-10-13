import * as cp from 'child_process';
import * as readline from 'readline';
import { extractText, parseLine } from './acpProtocol';

/** Speaks ACP to a `claurst acp` child process over stdio. Wire parsing
 * itself lives in acpProtocol.ts; this class owns the process, the
 * pending-request map, and dispatch to caller-supplied event callbacks. */

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
  /** Return the chosen option id, or `undefined` to cancel the request
   * (e.g. the user dismissed the picker without choosing). */
  onRequestPermission?: (toolCall: ToolCallUpdate, options: PermissionOption[]) => Promise<string | undefined>;
  onStderr?: (line: string) => void;
  onExit?: (code: number | null) => void;
}

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
    const parsed = parseLine(line);
    if (!parsed) {
      if (line.trim().length > 0) {
        this.events.onStderr?.(`[claurst-vscode] malformed line from agent: ${line.trim()}`);
      }
      return;
    }

    switch (parsed.kind) {
      case 'response': {
        const pending = this.pending.get(parsed.id);
        if (!pending) {
          return;
        }
        this.pending.delete(parsed.id);
        if (parsed.error) {
          pending.reject(Object.assign(new Error(parsed.error.message ?? 'ACP error'), { data: parsed.error }));
        } else {
          pending.resolve(parsed.result);
        }
        return;
      }
      case 'request':
        // Agent → client request. Only session/request_permission is expected in v1.
        this.handleIncomingRequest(parsed.id, parsed.method, parsed.params).catch((e) => {
          this.events.onStderr?.(`[claurst-vscode] failed to handle ${parsed.method}: ${e}`);
        });
        return;
      case 'notification':
        this.handleNotification(parsed.method, parsed.params);
        return;
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
      const chosen = await this.events.onRequestPermission?.(toolCall, options);
      // No selection (dismissed picker, or no handler wired up) must NOT
      // grant an option — respond Cancelled, matching the ACP spec's
      // Cancelled outcome rather than guessing an option to grant.
      const result = chosen
        ? { outcome: { outcome: 'selected', optionId: chosen } }
        : { outcome: { outcome: 'cancelled' } };
      this.writeMessage({ jsonrpc: '2.0', id, result });
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
