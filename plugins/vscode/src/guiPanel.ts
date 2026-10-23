// The Argon GUI in a webview, with its analyzer connections relayed through
// the extension host.

import { randomBytes } from 'crypto';
import * as vscode from 'vscode';

import { AnalyzerConnection, type Role } from './analyzerConnection';

/** The analyzer's RPC address and token, from `custom/guiEndpoint`. */
export interface GuiEndpoint {
  port: number;
  token: string;
}

/** What the panel needs from the session it shows. */
export interface GuiPanelHost {
  readonly title: string;
  endpoint(): Promise<GuiEndpoint>;
  /** Focuses the source editor the GUI hands off to. */
  focusEditor(): void;
}

const ROLES: Record<string, Role> = { gui: 'Gui', callback: 'GuiCallback' };

export class GuiPanel implements vscode.Disposable {
  readonly panel: vscode.WebviewPanel;
  private readonly connections = new Map<number, AnalyzerConnection>();
  private readonly disposables: vscode.Disposable[] = [];
  private disposed = false;
  private readonly closed = new vscode.EventEmitter<void>();
  /** Fires once the panel is closed. */
  readonly onDidClose = this.closed.event;

  constructor(
    extensionUri: vscode.Uri,
    private readonly host: GuiPanelHost,
    column: vscode.ViewColumn,
  ) {
    const media = vscode.Uri.joinPath(extensionUri, 'media', 'gui');
    this.panel = vscode.window.createWebviewPanel(
      'argon.gui',
      host.title,
      { viewColumn: column, preserveFocus: true },
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [media],
      },
    );
    this.panel.webview.html = this.html(media);
    this.disposables.push(
      this.panel.webview.onDidReceiveMessage((message) => void this.receive(message)),
      this.panel.onDidDispose(() => this.dispose()),
    );
  }

  /** Shows the panel, focusing it unless `preserveFocus`. */
  reveal(preserveFocus = false): void {
    this.panel.reveal(undefined, preserveFocus);
  }

  dispose(): void {
    if (this.disposed) {
      return;
    }
    this.disposed = true;
    for (const connection of this.connections.values()) {
      connection.close();
    }
    this.connections.clear();
    this.disposables.splice(0).forEach((disposable) => disposable.dispose());
    this.panel.dispose();
    this.closed.fire();
    this.closed.dispose();
  }

  private post(message: unknown): void {
    void this.panel.webview.postMessage(message);
  }

  private async receive(message: any): Promise<void> {
    switch (message?.type) {
      case 'connect':
        await this.connect(Number(message.id), ROLES[String(message.role)]);
        return;
      case 'frame': {
        const data = message.data instanceof Uint8Array ? message.data : new Uint8Array(message.data ?? []);
        this.connections.get(Number(message.id))?.send(data);
        return;
      }
      case 'close':
        this.connections.get(Number(message.id))?.close();
        this.connections.delete(Number(message.id));
        return;
      case 'reveal':
        this.reveal();
        return;
      case 'focusEditor':
        this.host.focusEditor();
        return;
      default:
        return;
    }
  }

  private async connect(id: number, role: Role | undefined): Promise<void> {
    if (!role) {
      this.post({ type: 'closed', id, reason: 'unknown connection role' });
      return;
    }
    let connection: AnalyzerConnection;
    try {
      const endpoint = await this.host.endpoint();
      connection = await AnalyzerConnection.connect(endpoint.port, endpoint.token, role);
    } catch (error) {
      this.post({ type: 'closed', id, reason: error instanceof Error ? error.message : String(error) });
      return;
    }
    if (this.disposed) {
      connection.close();
      return;
    }
    this.connections.set(id, connection);
    this.post({ type: 'opened', id });
    connection.start({
      frame: (frame) => this.post({ type: 'frame', id, data: new Uint8Array(frame) }),
      close: (reason) => {
        this.connections.delete(id);
        this.post({ type: 'closed', id, reason });
      },
    });
  }

  private html(media: vscode.Uri): string {
    const webview = this.panel.webview;
    const nonce = randomBytes(16).toString('base64');
    const script = webview.asWebviewUri(vscode.Uri.joinPath(media, 'argone_web.js'));
    const wasm = webview.asWebviewUri(vscode.Uri.joinPath(media, 'argone_web_bg.wasm'));
    const csp = [
      "default-src 'none'",
      `script-src 'nonce-${nonce}' 'wasm-unsafe-eval' ${webview.cspSource}`,
      `connect-src ${webview.cspSource}`,
      `img-src ${webview.cspSource} data: blob:`,
      `font-src ${webview.cspSource} data:`,
      "style-src 'unsafe-inline'",
    ].join('; ');
    return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
html, body { margin: 0; padding: 0; width: 100%; height: 100%; overflow: hidden; background: var(--vscode-editor-background); }
#status { position: absolute; inset: 0; display: flex; align-items: center; justify-content: center; font: 13px var(--vscode-font-family); color: var(--vscode-descriptionForeground); }
</style>
</head>
<body>
<div id="status">Loading the Argon GUI…</div>
<script type="module" nonce="${nonce}">
import init, { start, receive } from ${JSON.stringify(script.toString())};
const vscode = acquireVsCodeApi();
const status = document.getElementById('status');
try {
  await init({ module_or_path: ${JSON.stringify(wasm.toString())} });
  window.addEventListener('message', (event) => receive(event.data));
  start((message) => vscode.postMessage(message));
  status.remove();
} catch (error) {
  status.textContent = 'Could not start the Argon GUI: ' + error;
}
</script>
</body>
</html>`;
  }
}
