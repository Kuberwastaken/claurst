import * as vscode from 'vscode';
import { AcpClient, PermissionOption, ToolCallUpdate } from './acpClient';

/** Owns one webview panel and its backing AcpClient/session. */
export class ChatPanel {
  public static current: ChatPanel | undefined;

  private readonly panel: vscode.WebviewPanel;
  private client: AcpClient | undefined;
  private readonly outputChannel: vscode.OutputChannel;
  private disposables: vscode.Disposable[] = [];

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
  <div id="messages"></div>
  <div id="input-row">
    <textarea id="input-box" rows="2" placeholder="Ask claurst..."></textarea>
    <button id="send-btn">Send</button>
    <button id="stop-btn" title="Cancel the current turn">Stop</button>
  </div>
  <script nonce="${nonce}" src="${scriptUri}"></script>
</body>
</html>`;
  }

  private async startSession(): Promise<void> {
    const cwd = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (!cwd) {
      vscode.window.showErrorMessage('Claurst: open a folder or workspace before starting a chat.');
      return;
    }
    const executablePath = vscode.workspace.getConfiguration('claurst').get<string>('executablePath', 'claurst');

    this.client = new AcpClient(executablePath, cwd, {
      onTextChunk: (text, isThought) => this.postToWebview({ type: 'textChunk', text, isThought }),
      onToolCall: (update) => this.postToWebview({ type: 'toolCall', ...toolCallPayload(update) }),
      onToolCallUpdate: (update) => this.postToWebview({ type: 'toolCallUpdate', ...toolCallPayload(update) }),
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
    } catch (e) {
      this.reportError(e);
    }
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
    if (msg.type === 'prompt' && typeof msg.text === 'string') {
      this.client?.prompt(msg.text).catch((e) => this.reportError(e));
    } else if (msg.type === 'stop') {
      this.cancelCurrentTurn();
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
  return { title: update.title, status: update.status, kind: update.kind };
}
