import * as assert from 'assert';
import { type ChildProcessWithoutNullStreams, spawn } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';
import * as readline from 'readline';
import * as vscode from 'vscode';

import type { Extension } from '../../../extension';
import { GuiEditHistory } from '../../../guiHistory';
import type { Session } from '../../../session';

const project = process.env.ARGON_TEST_PROJECT!;
const analyzer = process.env.ARGON_ANALYZER!;
const libPath = path.join(project, 'lib.ar');

const RECT = 'let a = rect("met1", x0=0., y0=0., x1=100., y1=100.);';
const SECOND_RECT = 'let b = rect("met2", x0=0., y0=0., x1=50., y1=50.);';
const SOURCE = `use std::layout::rect;\n\ncell top() {\n    ${RECT}\n}\n`;

/** Polls `check` until it returns a value other than `undefined`. */
async function eventually<T>(description: string, check: () => T | undefined | Promise<T | undefined>): Promise<T> {
  const deadline = Date.now() + 30_000;
  for (;;) {
    const value = await check();
    if (value !== undefined) {
      return value;
    }
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for ${description}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

/** A minimal client for the analyzer's MCP bridge. */
class Bridge {
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly pending = new Map<number, (response: any) => void>();
  private next = 1;

  constructor() {
    this.child = spawn(analyzer, ['mcp', '--root', project], { env: process.env });
    readline.createInterface({ input: this.child.stdout }).on('line', (line) => {
      const message = JSON.parse(line);
      this.pending.get(message.id)?.(message);
      this.pending.delete(message.id);
    });
  }

  request(method: string, params: unknown): Promise<any> {
    const id = this.next++;
    return new Promise((resolve) => {
      this.pending.set(id, resolve);
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
    });
  }

  async call(name: string, args: Record<string, unknown>): Promise<{ text: string; isError: boolean }> {
    const response = await this.request('tools/call', { name, arguments: args });
    return { text: response.result.content[0].text, isError: response.result.isError === true };
  }

  close(): void {
    this.child.kill();
  }
}

describe('Argon extension', () => {
  let extension: Extension;
  let session: Session;
  let document: vscode.TextDocument;
  let bridge: Bridge;

  before(async () => {
    fs.writeFileSync(libPath, SOURCE);
    document = await vscode.workspace.openTextDocument(libPath);
    await vscode.window.showTextDocument(document);
    const installed = vscode.extensions.getExtension<Extension>('ucb-substrate.argon')!;
    extension = await installed.activate();
    session = await eventually('the analyzer to start', () => extension.sessions.get(project));
    bridge = new Bridge();
    await bridge.request('initialize', { protocolVersion: '2025-06-18', capabilities: {} });
  });

  after(() => bridge?.close());

  it('starts one analyzer for the project', async () => {
    assert.strictEqual(document.languageId, 'argon');
    assert.ok(session.client.isRunning());
    assert.strictEqual(extension.sessions.size, 1);
  });

  it('publishes diagnostics for the open document', async () => {
    const insert = new vscode.WorkspaceEdit();
    insert.insert(document.uri, new vscode.Position(3, 4), 'let = ;\n    ');
    await vscode.workspace.applyEdit(insert);
    await eventually('a diagnostic', () =>
      vscode.languages.getDiagnostics(document.uri).length > 0 ? true : undefined,
    );
    const remove = new vscode.WorkspaceEdit();
    remove.delete(document.uri, new vscode.Range(3, 4, 4, 4));
    await vscode.workspace.applyEdit(remove);
    await eventually('the diagnostic to clear', () =>
      vscode.languages.getDiagnostics(document.uri).length === 0 ? true : undefined,
    );
  });

  it('applies agent edits to the open document and reports it unsaved', async () => {
    const result = await bridge.call('edit_file', {
      path: 'lib.ar',
      label: 'add a second rect',
      edits: [{ old_string: RECT, new_string: `${RECT}\n    ${SECOND_RECT}` }],
    });
    assert.ok(!result.isError, result.text);
    assert.match(result.text, /Applied the edit to the editor buffer/);
    assert.ok(document.getText().includes(SECOND_RECT));
    assert.ok(document.isDirty);
    assert.strictEqual(fs.readFileSync(libPath, 'utf8'), SOURCE, 'the file on disk is unchanged');

    const read = await bridge.call('read_file', { path: 'lib.ar' });
    assert.match(read.text, /from the editor buffer/);
    assert.ok(read.text.includes('let b = rect('), read.text);
    const status = await eventually('the unsaved state', async () => {
      const status = await bridge.call('status', {});
      return /Unsaved changes in the editor: yes/.test(status.text) ? status : undefined;
    });
    assert.match(status.text, /Open in the editor:/);
  });

  it('undoes and redoes GUI edits in the document the GUI last edited', async () => {
    const history = new GuiEditHistory();
    const other = await vscode.workspace.openTextDocument({ language: 'plaintext', content: 'other' });
    await vscode.window.showTextDocument(other);
    const before = document.getText();
    const snapshot = history.snapshot([document.uri]);
    const edit = new vscode.WorkspaceEdit();
    edit.insert(document.uri, new vscode.Position(0, 0), '// first\n');
    await vscode.workspace.applyEdit(edit);
    history.record(snapshot);
    const second = history.snapshot([document.uri]);
    const append = new vscode.WorkspaceEdit();
    append.insert(document.uri, document.lineAt(document.lineCount - 1).range.end, '// second\n');
    await vscode.workspace.applyEdit(append);
    history.record(second);

    assert.ok(await history.undo(document));
    assert.ok(!document.getText().includes('// second'));
    assert.ok(await history.undo(document));
    assert.strictEqual(document.getText(), before);
    assert.ok(!(await history.undo(document)), 'nothing is left to undo');
    assert.ok(await history.redo(document));
    assert.ok(document.getText().startsWith('// first\n'));
    assert.strictEqual(other.getText(), 'other', 'the focused editor is untouched');

    const typed = new vscode.WorkspaceEdit();
    typed.insert(document.uri, new vscode.Position(0, 0), 'x');
    await vscode.workspace.applyEdit(typed);
    assert.ok(!(await history.redo(document)), 'other changes end the history');
    const cleanup = new vscode.WorkspaceEdit();
    cleanup.replace(document.uri, new vscode.Range(0, 0, document.lineCount, 0), before);
    await vscode.workspace.applyEdit(cleanup);
  });

  it('asks before applying agent edits when approval is on', async () => {
    const original = extension.agent.confirm;
    let asked = 0;
    let accept = false;
    extension.agent.confirm = async () => {
      asked++;
      return accept;
    };
    try {
      await session.request('custom/setConfig', { key: 'agent.approval', value: '"always"' });
      const edit = { path: 'lib.ar', edits: [{ old_string: SECOND_RECT, new_string: '' }] };
      const rejected = await bridge.call('edit_file', edit);
      assert.ok(rejected.isError);
      assert.match(rejected.text, /rejected this edit in the editor/);
      assert.ok(document.getText().includes(SECOND_RECT));
      accept = true;
      const applied = await bridge.call('edit_file', edit);
      assert.ok(!applied.isError, applied.text);
      assert.ok(!document.getText().includes(SECOND_RECT));
      assert.strictEqual(asked, 2);
    } finally {
      extension.agent.confirm = original;
      await session.request('custom/setConfig', { key: 'agent.approval', value: '"never"' });
    }
  });

  it('creates new files as unsaved documents', async () => {
    const created = await bridge.call('create_file', {
      path: 'utils.ar',
      contents: 'fn width() -> Float {\n    100.\n}\n',
    });
    assert.ok(!created.isError, created.text);
    const utils = vscode.workspace.textDocuments.find((document) => document.uri.path.endsWith('/utils.ar'));
    assert.ok(utils, 'the new file is open');
    assert.strictEqual(utils.uri.scheme, 'untitled');
    assert.ok(utils.getText().includes('fn width()'));
    assert.ok(!fs.existsSync(path.join(project, 'utils.ar')), 'nothing is written to disk');
    const read = await bridge.call('read_file', { path: 'utils.ar' });
    assert.match(read.text, /from the editor buffer/);
  });

  it('shows the GUI in a panel connected to the analyzer', async () => {
    await vscode.commands.executeCommand('argon.gui');
    await eventually('the GUI panel to open', () => {
      const tabs = vscode.window.tabGroups.all.flatMap((group) => group.tabs);
      return tabs.some((tab) => tab.input instanceof vscode.TabInputWebview && tab.label.startsWith('Argon'))
        ? true
        : undefined;
    }).catch((error) => {
      const tabs = vscode.window.tabGroups.all.flatMap((group) => group.tabs);
      throw new Error(`${error}; tabs: ${tabs.map((tab) => `${tab.label} (${tab.input?.constructor?.name})`).join(', ')}`);
    });
    await eventually('the GUI to connect', async () =>
      /GUI: connected/.test((await bridge.call('status', {})).text) ? true : undefined,
    );
  });

  it('sends requests that take no params', async () => {
    assert.strictEqual(await session.request('custom/reloadConfig'), null);
  });

  it('shows source the GUI edits without covering the GUI', async () => {
    await vscode.commands.executeCommand('argon.gui');
    const showDocument = session.client.clientOptions.middleware!.window!.showDocument!;
    const show = (uri: vscode.Uri) =>
      showDocument(
        { uri: session.client.code2ProtocolConverter.asUri(uri) },
        new vscode.CancellationTokenSource().token,
        async () => assert.fail('the default handler opens documents in the GUI group'),
      );
    const guiGroup = () =>
      vscode.window.tabGroups.all.find((group) => group.tabs.some((tab) => tab.input instanceof vscode.TabInputWebview))!;
    const tabsOf = (uri: vscode.Uri) =>
      vscode.window.tabGroups.all
        .flatMap((group) => group.tabs.map((tab) => ({ group, tab })))
        .filter(({ tab }) => tab.input instanceof vscode.TabInputText && tab.input.uri.toString() === uri.toString());
    const guiShown = () => assert.ok(guiGroup().activeTab?.input instanceof vscode.TabInputWebview, 'the GUI is shown');

    const before = tabsOf(document.uri).length;
    assert.deepStrictEqual(await show(document.uri), { success: true });
    assert.strictEqual(tabsOf(document.uri).length, before, 'the open editor is reused');
    guiShown();

    const other = vscode.Uri.file(path.join(project, 'other.ar'));
    fs.writeFileSync(other.fsPath, '');
    assert.deepStrictEqual(await show(other), { success: true });
    const opened = tabsOf(other);
    assert.strictEqual(opened.length, 1);
    assert.notStrictEqual(opened[0].group, guiGroup(), 'new editors open outside the GUI group');
    guiShown();
  });

  it('tells the analyzer when an editor follows agent edits', async () => {
    await vscode.window.showTextDocument(document);
    await vscode.commands.executeCommand('argon.follow');
    await eventually('follow mode', async () =>
      /Follow mode: on/.test((await bridge.call('status', {})).text) ? true : undefined,
    );
    await vscode.commands.executeCommand('argon.unfollow');
    await eventually('follow mode to end', async () =>
      /Follow mode: off/.test((await bridge.call('status', {})).text) ? true : undefined,
    );
  });
});
