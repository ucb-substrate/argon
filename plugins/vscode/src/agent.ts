// Agent edits and file loads the analyzer asks the editor for.

import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import type { LanguageClient } from 'vscode-languageclient/node';

import type {
  ApplyAgentEditParams,
  ApplyAgentEditResult,
  EnsureBufferParams,
  EnsureBufferResult,
  ReloadBufferParams,
  ReloadBufferResult,
} from './protocol';

/** How long a changed range stays highlighted, in milliseconds. */
const HIGHLIGHT_MS = 1500;
/** How recently the user typed in the follow editor for it to stay put. */
const TYPING_MS = 1500;
/** How long to wait for VS Code to reread a file that changed on disk. */
const RELOAD_WAIT_MS = 1000;
/** The scheme of an agent edit's proposed text, shown in a diff. */
const PREVIEW_SCHEME = 'argon-agent-edit';

/** The open document for `uri`, matching an unsaved new file by its path. */
export function openDocument(uri: vscode.Uri): vscode.TextDocument | undefined {
  return vscode.workspace.textDocuments.find(
    (document) =>
      document.uri.toString() === uri.toString() ||
      (document.uri.scheme === 'untitled' && uri.scheme === 'file' && document.uri.path === uri.path),
  );
}

function displayName(document: vscode.TextDocument): string {
  return vscode.workspace.asRelativePath(document.uri.scheme === 'untitled' ? document.uri.path : document.uri);
}

/** Proposed texts of agent edits awaiting approval, served for diffs. */
class Previews implements vscode.TextDocumentContentProvider {
  private readonly texts = new Map<string, string>();
  private next = 0;

  add(document: vscode.TextDocument, text: string): vscode.Uri {
    const uri = vscode.Uri.from({
      scheme: PREVIEW_SCHEME,
      path: document.uri.path,
      query: String(this.next++),
    });
    this.texts.set(uri.toString(), text);
    return uri;
  }

  remove(uri: vscode.Uri): void {
    this.texts.delete(uri.toString());
  }

  provideTextDocumentContent(uri: vscode.Uri): string {
    return this.texts.get(uri.toString()) ?? '';
  }
}

/** Agent edits, follow mode, and the file loads they need. */
export class AgentEdits implements vscode.Disposable {
  /** The editor column that follows agent edits, if follow mode is on. */
  followColumn: vscode.ViewColumn | undefined;
  private readonly highlight = vscode.window.createTextEditorDecorationType({
    backgroundColor: new vscode.ThemeColor('editor.findMatchHighlightBackground'),
    borderColor: new vscode.ThemeColor('editor.findMatchHighlightBorder'),
  });
  private readonly previews = new Previews();
  /** When the user last changed each document. */
  private readonly typed = new Map<string, number>();
  private applying = 0;
  private readonly disposables: vscode.Disposable[];

  constructor(private readonly onFollowChange: (enabled: boolean) => void) {
    this.disposables = [
      this.highlight,
      vscode.workspace.registerTextDocumentContentProvider(PREVIEW_SCHEME, this.previews),
      vscode.workspace.onDidChangeTextDocument((event) => {
        if (this.applying === 0 && event.contentChanges.length > 0) {
          this.typed.set(event.document.uri.toString(), Date.now());
        }
      }),
      vscode.workspace.onDidCloseTextDocument((document) => this.typed.delete(document.uri.toString())),
    ];
  }

  dispose(): void {
    this.disposables.forEach((disposable) => disposable.dispose());
  }

  setFollow(column: vscode.ViewColumn | undefined): void {
    this.followColumn = column;
    this.onFollowChange(column !== undefined);
  }

  /** Handles `custom/ensureBuffer`. */
  async ensureBuffer(params: EnsureBufferParams, client: LanguageClient): Promise<EnsureBufferResult> {
    const uri = client.protocol2CodeConverter.asUri(params.uri);
    const exists = fs.existsSync(uri.fsPath);
    if (!params.create && !exists) {
      return { ok: false, message: `${uri.fsPath} does not exist` };
    }
    try {
      let document = openDocument(uri);
      if (!document) {
        document = await vscode.workspace.openTextDocument(exists ? uri : uri.with({ scheme: 'untitled' }));
      }
      if (document.languageId !== 'argon') {
        await vscode.languages.setTextDocumentLanguage(document, 'argon');
      }
      return { ok: true };
    } catch (error) {
      return { ok: false, message: `could not open ${uri.fsPath}: ${String(error)}` };
    }
  }

  /** Handles `custom/applyAgentEdit`. */
  async apply(params: ApplyAgentEditParams, client: LanguageClient): Promise<ApplyAgentEditResult> {
    const document = openDocument(client.protocol2CodeConverter.asUri(params.uri));
    if (!document) {
      return { status: 'failed', message: 'the buffer is not loaded' };
    }
    if (document.version !== params.version) {
      return { status: 'stale', version: document.version };
    }
    if (params.confirm) {
      if (!(await this.confirm(document, params))) {
        return { status: 'rejected' };
      }
      if (document.version !== params.version) {
        return { status: 'stale', version: document.version };
      }
    }
    const edit = new vscode.WorkspaceEdit();
    edit.set(document.uri, await client.protocol2CodeConverter.asTextEdits(params.edits));
    this.applying++;
    let applied: boolean;
    try {
      applied = await vscode.workspace.applyEdit(edit);
    } finally {
      this.applying--;
    }
    if (!applied) {
      return { status: 'failed', message: 'VS Code did not apply the edit' };
    }
    const highlights = params.highlights.map((range) => client.protocol2CodeConverter.asRange(range));
    void this.show(document, highlights, params.label);
    return { status: 'applied', version: document.version };
  }

  /** Handles `custom/reloadBuffer`. VS Code rereads clean files itself. */
  async reload(params: ReloadBufferParams, client: LanguageClient): Promise<ReloadBufferResult> {
    const document = openDocument(client.protocol2CodeConverter.asUri(params.uri));
    if (!document) {
      return { status: 'closed' };
    }
    if (document.isDirty) {
      return { status: 'modified' };
    }
    const before = document.version;
    const deadline = Date.now() + RELOAD_WAIT_MS;
    while (Date.now() < deadline) {
      if (document.version !== before) {
        return { status: 'reloaded' };
      }
      if (document.isClosed) {
        return { status: 'closed' };
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    return { status: 'unchanged' };
  }

  /** Asks the user whether to apply an agent edit, previewing it as a diff. */
  async confirm(document: vscode.TextDocument, params: ApplyAgentEditParams): Promise<boolean> {
    const name = displayName(document);
    const proposed = this.previews.add(document, params.newText ?? '');
    try {
      await vscode.commands.executeCommand('vscode.diff', document.uri, proposed, `Agent edit: ${name}`, {
        preview: true,
      });
      const choice = await vscode.window.showInformationMessage(
        `Apply agent edit "${params.label}" to ${name}?`,
        { modal: true },
        'Apply',
      );
      return choice === 'Apply';
    } finally {
      await closeDiff(proposed);
      this.previews.remove(proposed);
    }
  }

  /** Highlights the text an agent edit wrote and brings it into view. */
  private async show(document: vscode.TextDocument, highlights: vscode.Range[], label: string): Promise<void> {
    const first = highlights[0];
    const followed = first ? await this.follow(document, first) : false;
    const editors = vscode.window.visibleTextEditors.filter((editor) => editor.document === document);
    for (const editor of editors) {
      editor.setDecorations(
        this.highlight,
        highlights.filter((range) => !range.isEmpty),
      );
    }
    if (editors.length > 0) {
      setTimeout(() => {
        for (const editor of editors) {
          editor.setDecorations(this.highlight, []);
        }
      }, HIGHLIGHT_MS);
    }
    if (!followed && editors.length === 0) {
      const line = (first?.start.line ?? 0) + 1;
      vscode.window.setStatusBarMessage(`argon: agent edited ${displayName(document)}:${line} (${label})`, 5000);
    }
  }

  /** Moves the follow editor to `range`, unless the user is typing there. */
  private async follow(document: vscode.TextDocument, range: vscode.Range): Promise<boolean> {
    const column = this.followColumn;
    if (column === undefined) {
      return false;
    }
    const current = vscode.window.visibleTextEditors.find((editor) => editor.viewColumn === column);
    const typedAt = current ? this.typed.get(current.document.uri.toString()) : undefined;
    const typing =
      current === vscode.window.activeTextEditor && typedAt !== undefined && Date.now() - typedAt < TYPING_MS;
    if (typing) {
      return true;
    }
    const selection = new vscode.Selection(range.start, range.start);
    const editor = await vscode.window.showTextDocument(document, {
      viewColumn: column,
      preserveFocus: true,
      preview: false,
      selection,
    });
    editor.revealRange(range, vscode.TextEditorRevealType.InCenter);
    return true;
  }
}

/** Closes the diff tabs showing `proposed`. */
async function closeDiff(proposed: vscode.Uri): Promise<void> {
  const tabs = vscode.window.tabGroups.all
    .flatMap((group) => group.tabs)
    .filter(
      (tab) =>
        tab.input instanceof vscode.TabInputTextDiff && tab.input.modified.toString() === proposed.toString(),
    );
  if (tabs.length > 0) {
    await vscode.window.tabGroups.close(tabs);
  }
}

/** Whether `document` is a saved-to-disk or new Argon source file. */
export function isArgonSource(document: vscode.TextDocument): boolean {
  return (
    document.languageId === 'argon' &&
    (document.uri.scheme === 'file' || document.uri.scheme === 'untitled') &&
    path.extname(document.uri.path) === '.ar'
  );
}
