import * as os from 'os';
import * as vscode from 'vscode';
import { AcpClient, PermissionOption, ToolCallUpdate } from './acpClient';

const EFFORT_LEVELS = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultracode'];
const COMMON_PROVIDERS = [
  'anthropic', 'openai', 'google', 'groq', 'cerebras', 'deepseek', 'mistral',
  'xai', 'openrouter', 'togetherai', 'cohere', 'ollama', 'azure', 'amazon-bedrock',
];

/** Owns one webview panel and its backing AcpClient/session. */
export class ChatPanel {
  public static current: ChatPanel | undefined;

  private readonly panel: vscode.WebviewPanel;
  private client: AcpClient | undefined;
  private readonly outputChannel: vscode.OutputChannel;
  private disposables: vscode.Disposable[] = [];

  /** While true, streamed events update `status` instead of the visible transcript. */
  private silent = false;
  private status: { model?: string; provider?: string; effort?: string } = {};

  static createOrShow(extensionUri: vscode.Uri, outputChannel: vscode.OutputChannel): ChatPanel {
    if (ChatPanel.current) {
      ChatPanel.current.panel.reveal();
      return ChatPanel.current;
    }
    const panel = vscode.window.createWebviewPanel(
      'claurstChat',
      'Claurst',
      vscode.ViewColumn.Beside,
      { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [vscode.Uri.joinPath(extensionUri, 'media')] },
    );
    ChatPanel.current = new ChatPanel(panel, extensionUri, outputChannel);
    return ChatPanel.current;
  }

  private constructor(panel: vscode.WebviewPanel, extensionUri: vscode.Uri, outputChannel: vscode.OutputChannel) {
    this.panel = panel;
    this.outputChannel = outputChannel;
    this.panel.webview.html = this.renderHtml(extensionUri);
    this.panel.onDidDispose(() => this.dispose(), null, this.disposables);
    this.panel.webview.onDidReceiveMessage(
      (msg) => this.handleWebviewMessage(msg),
      null,
      this.disposables,
    );
    this.startSession().catch((e) => this.reportError(e));
  }

  private renderHtml(extensionUri: vscode.Uri): string {
    const webview = this.panel.webview;
    const scriptUri = webview.asWebviewUri(vscode.Uri.joinPath(extensionUri, 'media', 'main.js'));
    const styleUri = webview.asWebviewUri(vscode.Uri.joinPath(extensionUri, 'media', 'main.css'));
    const nonce = String(Math.random()).slice(2);
    return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${webview.cspSource}; script-src 'nonce-${nonce}';" />
  <link href="${styleUri}" rel="stylesheet" />
  <title>Claurst</title>
</head>
<body>
  <div id="header">
    <button class="pill" id="model-pill" title="Change model">model: …</button>
    <button class="pill" id="provider-pill" title="Change provider">provider: …</button>
    <button class="pill" id="effort-pill" title="Change reasoning effort">effort: …</button>
  </div>
  <div id="messages"></div>
  <div id="input-row">
    <textarea id="input-box" rows="1" placeholder="Ask claurst..."></textarea>
    <button id="send-btn" title="Send (Enter)">Send</button>
    <button id="stop-btn" title="Cancel the current turn">Stop</button>
  </div>
  <script nonce="${nonce}" src="${scriptUri}"></script>
</body>
</html>`;
  }

  private async startSession(): Promise<void> {
    // Prefer the first workspace folder, but don't block chat on one being
    // open — fall back to the user's home directory so the panel is always
    // usable, matching how a plain terminal session would behave.
    const cwd = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? os.homedir();
    const executablePath = vscode.workspace.getConfiguration('claurst').get<string>('executablePath', 'claurst');

    this.client = new AcpClient(executablePath, cwd, {
      onTextChunk: (text, isThought) => {
        if (!this.silent) {
          this.postToWebview({ type: 'textChunk', text, isThought });
        }
      },
      onToolCall: (update) => {
        this.captureStatusFromToolResult(update);
        if (!this.silent) {
          this.postToWebview({ type: 'toolCall', ...toolCallPayload(update) });
        }
      },
      onToolCallUpdate: (update) => {
        this.captureStatusFromToolResult(update);
        if (!this.silent) {
          this.postToWebview({ type: 'toolCallUpdate', ...toolCallPayload(update) });
        }
      },
      onRequestPermission: (toolCall, options) => this.promptForPermission(toolCall, options),
      onStderr: (line) => this.outputChannel.appendLine(line),
      onExit: (code) => {
        this.postToWebview({ type: 'status', text: `claurst process exited (code ${code ?? 'unknown'}).` });
      },
    });

    try {
      await this.client.initialize();
      await this.client.newSession(cwd);
      this.postToWebview({ type: 'status', text: `Session started in ${cwd}` });
      await this.refreshStatus();
    } catch (e) {
      this.reportError(e);
    }
  }

  /** Silently asks the agent to report model/provider/effort via the Config
   * tool and updates the header pills. Doesn't appear in the visible transcript. */
  private async refreshStatus(): Promise<void> {
    if (!this.client) {
      return;
    }
    this.silent = true;
    try {
      await this.client.prompt(
        'Call the Config tool three times, once each with setting="model", setting="provider", ' +
        'and setting="effort" (omit "value" every time — these are reads, not writes). ' +
        'After the three tool calls finish, reply with just the word "ok".',
      );
    } catch (e) {
      this.outputChannel.appendLine(`[claurst-vscode] status refresh failed: ${e}`);
    } finally {
      this.silent = false;
      this.pushStatus();
    }
  }

  /** Parses `key = "value"` out of a Config tool result and updates the header. */
  private captureStatusFromToolResult(update: ToolCallUpdate): void {
    if (update.title !== 'Config' || !update.resultText) {
      return;
    }
    const match = update.resultText.match(/^(model|provider|effort)\s*=\s*"?([^"\n]+)"?/);
    if (!match) {
      return;
    }
    const [, key, value] = match;
    (this.status as any)[key] = value;
    this.pushStatus();
  }

  private pushStatus(): void {
    this.postToWebview({ type: 'headerUpdate', ...this.status });
  }

  private async promptForPermission(toolCall: ToolCallUpdate, options: PermissionOption[]): Promise<string> {
    const picked = await vscode.window.showQuickPick(
      options.map((o) => ({ label: o.name, description: o.kind, optionId: o.optionId })),
      { placeHolder: toolCall.title ?? 'Claurst is requesting permission', ignoreFocusOut: true },
    );
    // Falling through to the first (typically "allow once") option on dismissal
    // matches the ACP spec's guidance to avoid hanging the agent indefinitely,
    // while still defaulting to the least-privileged choice offered.
    return picked?.optionId ?? options[0]?.optionId ?? 'reject_once';
  }

  private handleWebviewMessage(msg: any): void {
    switch (msg.type) {
      case 'prompt':
        if (typeof msg.text === 'string') {
          this.runPrompt(msg.text);
        }
        break;
      case 'stop':
        this.cancelCurrentTurn();
        break;
      case 'pickModel':
        this.pickModel().catch((e) => this.reportError(e));
        break;
      case 'pickProvider':
        this.pickProvider().catch((e) => this.reportError(e));
        break;
      case 'pickEffort':
        this.pickEffort().catch((e) => this.reportError(e));
        break;
      default:
        break;
    }
  }

  private async pickModel(): Promise<void> {
    const value = await vscode.window.showInputBox({
      prompt: 'Model to use for new turns',
      value: this.status.model,
      placeHolder: 'e.g. claude-opus-5, claude-sonnet-4-6, gpt-5',
      ignoreFocusOut: true,
    });
    if (value) {
      this.sendConfigChange('model', value);
    }
  }

  private async pickProvider(): Promise<void> {
    const CUSTOM = '$(edit) Custom…';
    const picked = await vscode.window.showQuickPick([...COMMON_PROVIDERS, CUSTOM], {
      placeHolder: `Active provider (current: ${this.status.provider ?? 'unknown'})`,
      ignoreFocusOut: true,
    });
    if (!picked) {
      return;
    }
    if (picked === CUSTOM) {
      const custom = await vscode.window.showInputBox({ prompt: 'Provider id', ignoreFocusOut: true });
      if (custom) {
        this.sendConfigChange('provider', custom);
      }
      return;
    }
    this.sendConfigChange('provider', picked);
  }

  private async pickEffort(): Promise<void> {
    const picked = await vscode.window.showQuickPick(EFFORT_LEVELS, {
      placeHolder: `Reasoning effort (current: ${this.status.effort ?? 'unknown'})`,
      ignoreFocusOut: true,
    });
    if (picked) {
      this.sendConfigChange('effort', picked);
    }
  }

  /** Sends a real, visible prompt engineered to reliably trigger a single
   * Config tool call rather than a conversational reply. */
  private sendConfigChange(setting: 'model' | 'provider' | 'effort', value: string): void {
    this.postToWebview({ type: 'userEcho', text: `Set ${setting} to ${value}` });
    this.runPrompt(`Use the Config tool to set "${setting}" to "${value}".`);
  }

  /** Runs a visible prompt to completion, signalling turnEnded either way
   * so the webview can re-enable input. */
  private async runPrompt(text: string): Promise<void> {
    try {
      await this.client?.prompt(text);
    } catch (e) {
      this.reportError(e);
    } finally {
      this.postToWebview({ type: 'turnEnded' });
    }
  }

  cancelCurrentTurn(): void {
    this.client?.cancel();
  }

  private postToWebview(msg: unknown): void {
    this.panel.webview.postMessage(msg);
  }

  private reportError(e: unknown): void {
    const message = e instanceof Error ? e.message : String(e);
    this.outputChannel.appendLine(`[claurst-vscode] ${message}`);
    this.postToWebview({ type: 'status', text: `Error: ${message}` });
  }

  dispose(): void {
    ChatPanel.current = undefined;
    this.client?.dispose();
    this.panel.dispose();
    for (const d of this.disposables) {
      d.dispose();
    }
    this.disposables = [];
  }
}

function toolCallPayload(update: ToolCallUpdate) {
  return { toolCallId: update.toolCallId, title: update.title, status: update.status, kind: update.kind };
}
