// Undo and redo of the edits the analyzer applies for the GUI.
//
// VS Code's `undo` command acts on the focused editor, which the GUI cannot
// rely on: it may be in another window, or VS Code may not be the active
// application. So the edits are recorded here and reverted by applying the
// earlier text, as long as the document still has the text the edit left.

import * as vscode from 'vscode';

import { minimalReplacement } from './replacement';

/** One recorded edit to one document. */
interface Entry {
  before: string;
  after: string;
}

/** How many edits are kept per document. */
const LIMIT = 100;

export class GuiEditHistory {
  private readonly undoStacks = new Map<string, Entry[]>();
  private readonly redoStacks = new Map<string, Entry[]>();
  /** Set while this history applies an edit itself, so it isn't recorded. */
  private applying = false;

  /** The open documents' texts, before an edit that touches `uris`. */
  snapshot(uris: vscode.Uri[]): Map<vscode.TextDocument, string> {
    const texts = new Map<vscode.TextDocument, string>();
    for (const uri of uris) {
      const document = vscode.workspace.textDocuments.find((document) => document.uri.toString() === uri.toString());
      if (document) {
        texts.set(document, document.getText());
      }
    }
    return texts;
  }

  /** Records the edits that turned each snapshot into the document's current text. */
  record(snapshot: Map<vscode.TextDocument, string>): void {
    if (this.applying) {
      return;
    }
    for (const [document, before] of snapshot) {
      const after = document.getText();
      if (after === before) {
        continue;
      }
      const key = document.uri.toString();
      const stack = this.undoStacks.get(key) ?? [];
      stack.push({ before, after });
      if (stack.length > LIMIT) {
        stack.shift();
      }
      this.undoStacks.set(key, stack);
      this.redoStacks.delete(key);
    }
  }

  /** Reverts `document`'s last recorded edit, returning whether there was one to revert. */
  undo(document: vscode.TextDocument): Promise<boolean> {
    return this.move(document, this.undoStacks, this.redoStacks, 'before');
  }

  /** Reapplies `document`'s last reverted edit, returning whether there was one. */
  redo(document: vscode.TextDocument): Promise<boolean> {
    return this.move(document, this.redoStacks, this.undoStacks, 'after');
  }

  private async move(
    document: vscode.TextDocument,
    from: Map<string, Entry[]>,
    to: Map<string, Entry[]>,
    side: 'before' | 'after',
  ): Promise<boolean> {
    const key = document.uri.toString();
    const entry = from.get(key)?.at(-1);
    if (!entry) {
      return false;
    }
    const expected = side === 'before' ? entry.after : entry.before;
    if (document.getText() !== expected) {
      // The document changed some other way since, so this history no
      // longer describes it.
      from.delete(key);
      to.delete(key);
      return false;
    }
    const target = entry[side];
    const { start, end, text } = minimalReplacement(document.getText(), target);
    const edit = new vscode.WorkspaceEdit();
    edit.replace(document.uri, new vscode.Range(document.positionAt(start), document.positionAt(end)), text);
    this.applying = true;
    let applied: boolean;
    try {
      applied = await vscode.workspace.applyEdit(edit);
    } finally {
      this.applying = false;
    }
    if (!applied) {
      return false;
    }
    from.get(key)?.pop();
    const stack = to.get(key) ?? [];
    stack.push(entry);
    to.set(key, stack);
    return true;
  }
}
