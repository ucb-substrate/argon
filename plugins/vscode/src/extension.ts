// The Argon extension: one analyzer per open project, and the Argon commands.

import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';

import { AgentEdits, isArgonSource } from './agent';
import { subcommand } from './commandLine';
import { findRoot, isInside, logPath, stdCacheDir } from './paths';
import { cursorOf, promptArgument } from './prompt';
import type { CommandCompletionResult, FocusEditorParams } from './protocol';
import { type AnalyzerCommand, errorMessage, Session, type SessionHost } from './session';

/** Subcommands that take an argument the user types. */
const PROMPTED = ['openCell', 'newCell', 'renameCell', 'inst', 'sinst', 'device', 'setConfig', 'saveConfig'];

export class Extension implements SessionHost, vscode.Disposable {
  readonly agent: AgentEdits;
  readonly extensionUri: vscode.Uri;
  readonly sessions = new Map<string, Session>();
  private readonly starting = new Map<string, Promise<Session | undefined>>();
  private readonly outputChannel = vscode.window.createOutputChannel('Argon', { log: true });
  private readonly disposables: vscode.Disposable[] = [];

  constructor(context: vscode.ExtensionContext) {
    this.extensionUri = context.extensionUri;
    this.agent = new AgentEdits((enabled) => {
      void vscode.commands.executeCommand('setContext', 'argon.following', enabled);
      for (const session of this.sessions.values()) {
        void session.setFollowMode(enabled);
      }
    });
    this.disposables.push(
      this.agent,
      this.outputChannel,
      vscode.workspace.onDidOpenTextDocument((document) => void this.sessionFor(document)),
      vscode.window.onDidChangeVisibleTextEditors(() => {
        const column = this.agent.followColumn;
        if (column !== undefined && !vscode.window.visibleTextEditors.some((editor) => editor.viewColumn === column)) {
          this.agent.setFollow(undefined);
        }
      }),
    );
  }

  dispose(): void {
    this.disposables.forEach((disposable) => disposable.dispose());
  }

  async deactivate(): Promise<void> {
    await Promise.all([...this.sessions.values()].map((session) => session.stop()));
    this.sessions.forEach((session) => session.dispose());
    this.sessions.clear();
  }

  /** Starts sessions for the documents already open. */
  startOpenDocuments(): void {
    for (const document of vscode.workspace.textDocuments) {
      void this.sessionFor(document);
    }
  }

  /** The session for `document`'s project, starting it if needed. */
  async sessionFor(document: vscode.TextDocument): Promise<Session | undefined> {
    if (!isArgonSource(document)) {
      return undefined;
    }
    const file = document.uri.path;
    for (const session of this.sessions.values()) {
      if (session.owns(document)) {
        return session;
      }
    }
    // The standard library copy that go-to-definition opens has its own
    // `lib.ar`; the session the user jumped from already serves it.
    if (isInside(file, stdCacheDir())) {
      return undefined;
    }
    let root = findRoot(file);
    if (!root) {
      this.outputChannel.warn(`No lib.ar above ${file}; treating its directory as the project root.`);
      root = path.dirname(file);
    }
    return this.startSession(root);
  }

  private startSession(root: string): Promise<Session | undefined> {
    const existing = this.sessions.get(root);
    if (existing) {
      return Promise.resolve(existing);
    }
    const pending = this.starting.get(root);
    if (pending) {
      return pending;
    }
    const start = (async () => {
      const session = new Session(root, this, this.analyzerCommand(), this.outputChannel);
      try {
        await session.start(this.agent.followColumn !== undefined);
      } catch (error) {
        session.dispose();
        void vscode.window.showErrorMessage(
          `argon: Could not start argon-analyzer: ${errorMessage(error)}. Install it with Cargo or set argon.analyzer.path.`,
        );
        return undefined;
      }
      this.sessions.set(root, session);
      void vscode.commands.executeCommand('setContext', 'argon.active', true);
      return session;
    })().finally(() => this.starting.delete(root));
    this.starting.set(root, start);
    return start;
  }

  analyzerCommand(): AnalyzerCommand {
    const command = vscode.workspace.getConfiguration('argon').get<string>('analyzer.path') || 'argon-analyzer';
    return { command, args: guiInPanel() ? ['--embedded-gui'] : [] };
  }

  /** The session commands act on: the active document's, or the only one. */
  async activeSession(): Promise<Session | undefined> {
    const document = vscode.window.activeTextEditor?.document;
    if (document) {
      const session = await this.sessionFor(document);
      if (session) {
        return session;
      }
    }
    const sessions = [...this.sessions.values()];
    if (sessions.length <= 1) {
      if (sessions.length === 0) {
        void vscode.window.showErrorMessage('argon: No Argon project is open. Open a .ar file first.');
      }
      return sessions[0];
    }
    const choice = await vscode.window.showQuickPick(
      sessions.map((session) => ({ label: path.basename(session.root), description: session.root, session })),
      { title: 'Argon project' },
    );
    return choice?.session;
  }

  /** Handles `custom/focusEditor`: opens the command the GUI asked for. */
  focusEditor(session: Session, params: FocusEditorParams): void {
    void (async () => {
      const command = params.command;
      if (command === 'messages') {
        this.outputChannel.show(true);
        return;
      }
      const requested = subcommand(command);
      if (!requested) {
        await vscode.commands.executeCommand('workbench.action.quickOpen', '>Argon: ');
        return;
      }
      if (requested.submit && !PROMPTED.includes(requested.name)) {
        await this.run(session, requested.name, requested.argument);
      } else {
        await this.prompt(session, requested.name, requested.argument);
      }
      if (params.return_to_gui) {
        await session.request('custom/startGui');
      }
    })().catch((error: unknown) => {
      void vscode.window.showErrorMessage(`argon: ${errorMessage(error)}`);
    });
  }

  /** Asks for subcommand `name`'s argument and runs it. */
  async prompt(session: Session, name: string, argument = ''): Promise<void> {
    const complete = (command: string, text: string) =>
      session.client
        .sendRequest<CommandCompletionResult>('custom/commandCompletion', { command, text, cursor: cursorOf(text) })
        .catch(() => undefined);
    const value = await promptArgument(name, argument, complete);
    if (value !== undefined) {
      await this.run(session, name, value);
    }
  }

  /** Runs Argon subcommand `name` with `argument`. */
  async run(session: Session, name: string, argument: string): Promise<void> {
    const trimmed = argument.trim();
    if (guiInPanel() && SHOWN_IN_GUI.includes(name)) {
      session.showGui(true);
    }
    switch (name) {
      case 'gui':
        if (guiInPanel()) {
          session.showGui(false);
        }
        await session.request('custom/startGui');
        return;
      case 'openCell':
        await session.request('custom/openCell', { cell: trimmed });
        return;
      case 'newCell':
      case 'renameCell':
        if (!/^\S+$/.test(trimmed)) {
          void vscode.window.showErrorMessage(`Argon ${name}: expected one cell name`);
          return;
        }
        await session.request(`custom/${name}`, { name: trimmed, uri: session.activeDocumentUri() });
        return;
      case 'inst':
      case 'sinst':
        await session.request(`custom/${name}`, { cell: trimmed });
        return;
      case 'device':
        if (trimmed === '') {
          void vscode.window.showErrorMessage('Argon device: expected a device kind, such as `Nmos`');
          return;
        }
        await session.request('custom/device', { args: trimmed });
        return;
      case 'diagnostics':
        await vscode.commands.executeCommand('workbench.actions.view.problems');
        return;
      case 'reload':
      case 'reloadConfig':
        await session.request('custom/reloadConfig');
        return;
      case 'set':
      case 'setConfig': {
        const [key, ...value] = trimmed.split(/\s+/);
        if (!key) {
          void vscode.window.showErrorMessage(
            'Argon set: expected a dotted configuration key and optional TOML value',
          );
          return;
        }
        await session.request('custom/setConfig', { key, value: value.length > 0 ? value.join(' ') : null });
        return;
      }
      case 'saveConfig':
        await session.request('custom/saveConfig', {
          path: trimmed === '' ? null : path.resolve(session.root, expandHome(trimmed)),
        });
        return;
      default:
        void vscode.window.showErrorMessage(`Argon: Unknown subcommand: ${name}`);
    }
  }

  registerCommands(context: vscode.ExtensionContext): void {
    const withSession = (action: (session: Session) => Promise<void>) => async () => {
      const session = await this.activeSession();
      if (session) {
        await action(session);
      }
    };
    const register = (id: string, action: () => Promise<void> | void) =>
      context.subscriptions.push(vscode.commands.registerCommand(id, action));

    register('argon.gui', withSession((session) => this.run(session, 'gui', '')));
    register('argon.focusSource', () => {
      const sessions = [...this.sessions.values()];
      const session =
        sessions.find((session) => session.guiActive()) ?? (sessions.length === 1 ? sessions[0] : undefined);
      session?.focusEditor();
    });
    for (const name of PROMPTED) {
      register(`argon.${name}`, withSession((session) => this.prompt(session, name)));
    }
    register('argon.diagnostics', withSession((session) => this.run(session, 'diagnostics', '')));
    register('argon.reloadConfig', withSession((session) => this.run(session, 'reloadConfig', '')));
    register('argon.follow', () => {
      const column = vscode.window.activeTextEditor?.viewColumn;
      if (column === undefined) {
        void vscode.window.showErrorMessage('argon: Focus the editor that should follow agent edits first.');
        return;
      }
      this.agent.setFollow(column);
      vscode.window.setStatusBarMessage('argon: this editor follows agent edits', 3000);
    });
    register('argon.unfollow', () => {
      this.agent.setFollow(undefined);
      vscode.window.setStatusBarMessage('argon: no longer following agent edits', 3000);
    });
    register('argon.log', async () => {
      const log = logPath();
      if (!fs.existsSync(log)) {
        void vscode.window.showErrorMessage(`argon: ${log} does not exist yet`);
        return;
      }
      await vscode.window.showTextDocument(vscode.Uri.file(log), { preview: false });
    });
    register(
      'argon.restart',
      withSession(async (session) => {
        await session.stop();
        session.dispose();
        this.sessions.delete(session.root);
        await this.startSession(session.root);
      }),
    );
  }
}

/** Subcommands whose results appear in the GUI. */
const SHOWN_IN_GUI = ['openCell', 'newCell', 'inst', 'sinst', 'device'];

/** Whether the GUI is shown in a VS Code panel rather than a window of its own. */
function guiInPanel(): boolean {
  return vscode.workspace.getConfiguration('argon').get<string>('gui.location', 'panel') === 'panel';
}

function expandHome(file: string): string {
  return file.startsWith('~/') ? path.join(process.env.HOME ?? '', file.slice(2)) : file;
}

let extension: Extension | undefined;

export function activate(context: vscode.ExtensionContext): Extension {
  extension = new Extension(context);
  context.subscriptions.push(extension);
  extension.registerCommands(context);
  extension.startOpenDocuments();
  return extension;
}

export async function deactivate(): Promise<void> {
  await extension?.deactivate();
  extension = undefined;
}
