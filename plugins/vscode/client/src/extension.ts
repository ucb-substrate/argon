/* --------------------------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation. All rights reserved.
 * Licensed under the MIT License. See License.txt in the project root for license information.
 * ------------------------------------------------------------------------------------------ */

import * as path from "path";
import { commands, window, workspace, ExtensionContext, Uri } from "vscode";

import {
	LanguageClient,
	LanguageClientOptions,
	ServerOptions,
	TransportKind
} from "vscode-languageclient/node";

let client: LanguageClient;

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

	// Options to control the language client
	const clientOptions: LanguageClientOptions = {
		// Register the server for Argon documents
		documentSelector: [{ scheme: 'file', language: 'argon' }],
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

	// Start the client. This will also launch the server
	client.start();
}

export function deactivate(): Thenable<void> | undefined {
	if (!client) {
		return undefined;
	}
	return client.stop();
}