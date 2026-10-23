// One analyzer, serving the Argon project rooted at one `lib.ar`.

import * as path from 'path';
import * as vscode from 'vscode';
import {
  CancellationToken,
  LanguageClient,
  type LanguageClientOptions,
  RevealOutputChannelOn,
  type ServerOptions,
} from 'vscode-languageclient/node';

import { type AgentEdits, isArgonSource, openDocument } from './agent';
import { GuiEditHistory } from './guiHistory';
import { GuiPanel, type GuiEndpoint } from './guiPanel';
import { globUnder, isInside } from './paths';
import type {
  ApplyAgentEditParams,
  EnsureBufferParams,
  FocusEditorParams,
  HistoryParams,
  ReloadBufferParams,
} from './protocol';

/** What a session needs from the rest of the extension. */
export interface SessionHost {
  readonly agent: AgentEdits;
  readonly extensionUri: vscode.Uri;
  focusEditor(session: Session, params: FocusEditorParams): void;
}

/** How to start an analyzer. */
export interface AnalyzerCommand {
  command: string;
  args: string[];
}

export class Session implements vscode.Disposable {
  readonly client: LanguageClient;
  /** The GUI panel, when the GUI is shown in VS Code and open. */
  private gui: GuiPanel | undefined;
  /** The project's editor that the GUI last handed focus away from. */
  private lastEditor: vscode.TextEditor | undefined;
  /** The workspace modified state last reported to the analyzer. */
  private reportedModified: boolean | undefined;
  /** The edits the analyzer applied, which GUI undo and redo revisit. */
  private readonly history = new GuiEditHistory();
  private readonly disposables: vscode.Disposable[] = [];

  constructor(
    readonly root: string,
    private readonly host: SessionHost,
    analyzer: AnalyzerCommand,
    outputChannel: vscode.LogOutputChannel,
  ) {
    // Without a transport the client talks over stdio and adds no `--stdio` flag.
    const executable = { command: analyzer.command, args: analyzer.args };
    const serverOptions: ServerOptions = { run: executable, debug: executable };
    const pattern = globUnder(root);
    const clientOptions: LanguageClientOptions = {
      documentSelector: [
        { scheme: 'file', language: 'argon', pattern },
        { scheme: 'untitled', language: 'argon', pattern },
      ],
      workspaceFolder: {
        uri: vscode.Uri.file(root),
        name: path.basename(root),
        index: 0,
      },
      outputChannel,
      revealOutputChannelOn: RevealOutputChannelOn.Never,
      middleware: {
        workspace: {
          handleApplyEdit: async (params, next) => {
            const uris = [
              ...Object.keys(params.edit.changes ?? {}),
              ...(params.edit.documentChanges ?? []).flatMap((change) =>
                'textDocument' in change ? [change.textDocument.uri] : [],
              ),
            ].map((uri) => this.client.protocol2CodeConverter.asUri(uri));
            const snapshot = this.history.snapshot(uris);
            const result = await next(params, CancellationToken.None);
            if (result && typeof result === 'object' && 'applied' in result && result.applied) {
              this.history.record(snapshot);
            }
            return result;
          },
        },
        window: {
          showDocument: async (params, token, next) => {
            if (params.external) {
              return next(params, token);
            }
            const converter = this.client.protocol2CodeConverter;
            const uri = converter.asUri(params.uri);
            const selection = params.selection && converter.asRange(params.selection);
            const shown = this.visibleEditor(uri);
            if (shown && params.takeFocus !== true) {
              if (selection) {
                shown.selection = new vscode.Selection(selection.start, selection.end);
                shown.revealRange(selection);
              }
              return { success: true };
            }
            try {
              await vscode.window.showTextDocument(uri, {
                viewColumn: shown?.viewColumn ?? this.sourceColumn(),
                preserveFocus: params.takeFocus !== true,
                selection,
              });
              return { success: true };
            } catch {
              return { success: false };
            }
          },
        },
      },
      uriConverters: {
        // An unsaved new file reaches the analyzer under the path it will
        // be saved to.
        code2Protocol: (uri) =>
          (uri.scheme === 'untitled' ? vscode.Uri.file(uri.path) : uri).toString(),
        protocol2Code: (value) => {
          const uri = vscode.Uri.parse(value);
          return openDocument(uri)?.uri ?? uri;
        },
      },
    };
    this.client = new LanguageClient('argon', `Argon (${path.basename(root)})`, serverOptions, clientOptions);
    this.registerHandlers();
    const update = () => void this.notifyWorkspaceModified();
    this.disposables.push(
      vscode.workspace.onDidChangeTextDocument((event) => {
        if (this.owns(event.document)) {
          update();
        }
      }),
      vscode.workspace.onDidSaveTextDocument(update),
      vscode.workspace.onDidOpenTextDocument(update),
      vscode.workspace.onDidCloseTextDocument(update),
      vscode.window.onDidChangeActiveTextEditor((editor) => {
        if (editor && this.owns(editor.document)) {
          this.lastEditor = editor;
        }
      }),
    );
  }

  /**
   * Shows the GUI panel, opening it beside the source if needed, and focuses
   * it unless `preserveFocus`.
   */
  showGui(preserveFocus: boolean): GuiPanel {
    if (!this.gui) {
      const panel = new GuiPanel(
        this.host.extensionUri,
        {
          title: `Argon: ${path.basename(this.root)}`,
          endpoint: () => this.client.sendRequest<GuiEndpoint>('custom/guiEndpoint'),
          focusEditor: () => this.focusEditor(),
        },
        vscode.ViewColumn.Beside,
      );
      panel.onDidClose(() => {
        if (this.gui === panel) {
          this.gui = undefined;
        }
      });
      this.gui = panel;
      if (!preserveFocus) {
        panel.reveal();
      }
      return panel;
    }
    this.gui.reveal(preserveFocus);
    return this.gui;
  }

  /** Whether the GUI panel is open and focused. */
  guiActive(): boolean {
    return this.gui?.panel.active === true;
  }

  /**
   * Focuses the project's source editor, the one last used if it is visible.
   * With none visible, shows the last used source, or `lib.ar`, beside the GUI.
   */
  focusEditor(): void {
    const editor =
      this.lastEditor && vscode.window.visibleTextEditors.includes(this.lastEditor)
        ? this.lastEditor
        : vscode.window.visibleTextEditors.find((editor) => this.owns(editor.document));
    if (editor) {
      void vscode.window.showTextDocument(editor.document, editor.viewColumn);
      return;
    }
    const last = this.lastEditor?.document;
    const target = last && !last.isClosed ? last.uri : vscode.Uri.file(path.join(this.root, 'lib.ar'));
    void vscode.window.showTextDocument(target, { viewColumn: this.sourceColumn(), preview: false });
  }

  /** An editor outside the GUI's group that shows `uri`. */
  private visibleEditor(uri: vscode.Uri): vscode.TextEditor | undefined {
    const guiColumn = this.gui?.panel.viewColumn;
    return vscode.window.visibleTextEditors.find(
      (editor) => editor.document.uri.toString() === uri.toString() && editor.viewColumn !== guiColumn,
    );
  }

  /**
   * The group to open source in while the GUI panel is open, so it never
   * covers the GUI, or undefined for the active group.
   */
  private sourceColumn(): vscode.ViewColumn | undefined {
    const guiColumn = this.gui?.panel.viewColumn;
    if (guiColumn === undefined) {
      return undefined;
    }
    const editors = vscode.window.visibleTextEditors.filter(
      (editor) => editor.viewColumn !== undefined && editor.viewColumn !== guiColumn,
    );
    const editor = this.lastEditor && editors.includes(this.lastEditor) ? this.lastEditor : editors[0];
    return editor?.viewColumn ?? (guiColumn === vscode.ViewColumn.One ? vscode.ViewColumn.Two : vscode.ViewColumn.One);
  }

  /** Whether `document` belongs to this session's project. */
  owns(document: vscode.TextDocument): boolean {
    return isArgonSource(document) && isInside(document.uri.path, this.root);
  }

  /** The project's open Argon documents. */
  documents(): vscode.TextDocument[] {
    return vscode.workspace.textDocuments.filter((document) => this.owns(document));
  }

  async start(followEnabled: boolean): Promise<void> {
    await this.client.start();
    this.reportedModified = undefined;
    await Promise.all([this.notifyWorkspaceModified(), this.setFollowMode(followEnabled)]);
  }

  async stop(): Promise<void> {
    if (this.client.isRunning()) {
      await this.client.stop();
    }
  }

  dispose(): void {
    this.gui?.dispose();
    this.disposables.forEach((disposable) => disposable.dispose());
    void this.client.dispose();
  }

  /** Sends a request, reporting failures to the user. */
  async request<R = unknown>(method: string, params?: unknown): Promise<R | undefined> {
    try {
      // A request without params must omit them: `null` is sent as `[null]`.
      return await (params === undefined
        ? this.client.sendRequest<R>(method)
        : this.client.sendRequest<R>(method, params));
    } catch (error) {
      void vscode.window.showErrorMessage(`argon: ${errorMessage(error)}`);
      return undefined;
    }
  }

  /** The current document as the analyzer names it, for commands that act on it. */
  activeDocumentUri(): string {
    const document = vscode.window.activeTextEditor?.document;
    const target = document && this.owns(document) ? document.uri : vscode.Uri.file(path.join(this.root, 'lib.ar'));
    return this.client.code2ProtocolConverter.asUri(target);
  }

  async setFollowMode(enabled: boolean): Promise<void> {
    if (this.client.isRunning()) {
      await this.client.sendRequest('custom/followMode', { enabled }).catch(() => undefined);
    }
  }

  /** Reports whether any of the project's documents has unsaved changes. */
  async notifyWorkspaceModified(): Promise<void> {
    if (!this.client.isRunning()) {
      this.reportedModified = undefined;
      return;
    }
    const modified = this.documents().some((document) => document.isDirty);
    if (this.reportedModified === modified) {
      return;
    }
    this.reportedModified = modified;
    try {
      await this.client.sendRequest('custom/workspaceModified', { modified });
    } catch (error) {
      if (this.reportedModified === modified) {
        this.reportedModified = undefined;
      }
      void vscode.window.showErrorMessage(`argon: Could not update GUI save state: ${errorMessage(error)}`);
    }
  }

  private registerHandlers(): void {
    const client = this.client;
    client.onRequest('custom/save', async () => {
      await Promise.all(this.documents().filter((document) => document.isDirty).map((document) => document.save()));
      return null;
    });
    client.onRequest('custom/undo', async (params: HistoryParams | null) => {
      await this.undoRedo('undo', params);
      return null;
    });
    client.onRequest('custom/redo', async (params: HistoryParams | null) => {
      await this.undoRedo('redo', params);
      return null;
    });
    client.onNotification('custom/focusEditor', (params: FocusEditorParams) => {
      this.host.focusEditor(this, params);
    });
    client.onRequest('custom/ensureBuffer', (params: EnsureBufferParams) =>
      this.host.agent.ensureBuffer(params, client),
    );
    client.onRequest('custom/applyAgentEdit', (params: ApplyAgentEditParams) => this.host.agent.apply(params, client));
    client.onRequest('custom/reloadBuffer', (params: ReloadBufferParams) => this.host.agent.reload(params, client));
  }

  /** Runs `undo` or `redo` in the document the GUI last edited. */
  private async undoRedo(command: 'undo' | 'redo', params: HistoryParams | null): Promise<void> {
    const edited = params?.uri ? openDocument(this.client.protocol2CodeConverter.asUri(params.uri)) : undefined;
    const document = edited ?? vscode.window.activeTextEditor?.document;
    if (!document) {
      return;
    }
    if (await this.history[command](document)) {
      return;
    }
    // VS Code runs undo in the focused editor, so focus moves there and back.
    const returnToGui = this.guiActive();
    await vscode.window.showTextDocument(document, {
      viewColumn: this.visibleEditor(document.uri)?.viewColumn ?? this.sourceColumn(),
      preserveFocus: false,
      preview: false,
    });
    await vscode.commands.executeCommand(command);
    if (returnToGui) {
      this.gui?.reveal();
    }
  }
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  if (typeof error === 'object' && error !== null && 'message' in error) {
    return String((error as { message: unknown }).message);
  }
  return String(error);
}
