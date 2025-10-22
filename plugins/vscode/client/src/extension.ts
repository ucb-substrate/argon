/* --------------------------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation. All rights reserved.
 * Licensed under the MIT License. See License.txt in the project root for license information.
 * ------------------------------------------------------------------------------------------ */

import * as path from "path";
import * as fs from 'fs';
import { commands, window, workspace, WorkspaceFolder, ExtensionContext, Uri } from "vscode";

import {
	LanguageClient,
	LanguageClientOptions,
	ServerOptions,
	TransportKind
} from "vscode-languageclient/node";

let client: LanguageClient;

/**
 * Find the first enclosing folder (upwards) that contains `lib.ar`.
 */
function findWorkspaceRoot(startPath: string): string | undefined {
  let dir = path.dirname(startPath);

  while (true) {
    const candidate = path.join(dir, 'lib.ar');
    if (fs.existsSync(candidate)) {
      return dir;
    }

    const parent = path.dirname(dir);
    if (parent === dir) break; // reached filesystem root
    dir = parent;
  }

  return undefined;
}

export function activate(context: ExtensionContext) {
	// The server is implemented in node
	const serverModule = path.join(workspace.getConfiguration(undefined, undefined).argonLsp.argonRepoDir, 'target', 'debug', 'lsp-server')
	console.log(serverModule);

	// If the extension is launched in debug mode then the debug server options are used
	// Otherwise the run options are used
	const serverOptions: ServerOptions = {
		run: { command: serverModule, transport: TransportKind.stdio },
		debug: {
			command: serverModule,
			transport: TransportKind.stdio,
		}
	};

	const activeEditor = window.activeTextEditor;
	if (!activeEditor) return;

	const filePath = activeEditor.document.uri.fsPath;
	let workspaceRoot = findWorkspaceRoot(filePath);

	if (!workspaceRoot) {
		window.showWarningMessage('No lib.ar found in parent folders.');
		workspaceRoot = path.dirname(filePath);
	}
	// Options to control the language client
	const clientOptions: LanguageClientOptions = {
		// Register the server for Argon documents
		documentSelector: [{ scheme: 'file', language: 'argon' }],
		workspaceFolder: {
			uri: Uri.file(workspaceRoot),
			name: path.basename(workspaceRoot),
			index: 0,
		},
	};

	// Create the language client and start the client.
	client = new LanguageClient(
		'argonLsp',
		'Argon LSP Client',
		serverOptions,
		clientOptions
	);

	const startGui = async () => {
		client.sendRequest("custom/startGui");
	};

	const openCell = async () => {
		const cell = await window.showInputBox({ prompt: 'Enter cell invocation' });
		if (cell) {
			client.sendRequest("custom/openCell", { cell: cell });
		}
	};

	context.subscriptions.push(commands.registerCommand("argonLsp.startGui", startGui));
	context.subscriptions.push(commands.registerCommand("argonLsp.openCell", openCell));

	client.onRequest("custom/forceSave", async (file: string) => {
		let doc = workspace.textDocuments.find(d => d.uri.fsPath === file);
		if (!doc) {
			try {
				doc = await workspace.openTextDocument(Uri.file(file));
				await window.showTextDocument(doc, { preview: false });
			} catch (err) {
				console.error('Failed to open document:', err);
				return false;
			}
		}

		// Save the document
		const saved = await doc.save();
	});

	client.onRequest("custom/undo", async () => {
		const activeEditor = window.activeTextEditor;
		if (!activeEditor) return;

		let doc = activeEditor.document;
		if (doc) {
			await window.showTextDocument(doc, { preview: false });
			await commands.executeCommand('undo');
			await doc.save();
		}
	});

	client.onRequest("custom/redo", async () => {
		const activeEditor = window.activeTextEditor;
		if (!activeEditor) return;

		let doc = activeEditor.document;
		if (doc) {
			await window.showTextDocument(doc, { preview: false });
			await commands.executeCommand('redo');
			await doc.save();
		}
	});

	// Start the client. This will also launch the server
	client.start();
}

export function deactivate(): Thenable<void> | undefined {
	if (!client) {
		return undefined;
	}
	return client.stop();
}