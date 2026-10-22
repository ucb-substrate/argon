// The analyzer's custom LSP messages, as they appear on the wire.

import type { Position, Range, TextEdit } from 'vscode-languageclient';

/** A command the GUI asks the editor to open at its command prompt. */
export type EditorCommand =
  | 'messages'
  | { argon: { name: string; argument: string; submit: boolean } };

export interface FocusEditorParams {
  command?: EditorCommand | null;
  return_to_gui: boolean;
}

export interface HistoryParams {
  uri?: string | null;
}

export interface EnsureBufferParams {
  uri: string;
  create: boolean;
}

export interface EnsureBufferResult {
  ok: boolean;
  message?: string;
}

export interface ApplyAgentEditParams {
  uri: string;
  version: number;
  edits: TextEdit[];
  highlights: Range[];
  label: string;
  confirm: boolean;
  newText?: string | null;
}

export type ApplyAgentEditResult =
  | { status: 'applied'; version: number }
  | { status: 'stale'; version: number }
  | { status: 'rejected' }
  | { status: 'failed'; message: string };

export interface ReloadBufferParams {
  uri: string;
}

export type ReloadBufferResult = {
  status: 'reloaded' | 'unchanged' | 'modified' | 'closed';
};

export interface CompletionItem {
  label: string;
  insertText?: string | null;
}

export interface CommandCompletionResult {
  items?: CompletionItem[] | null;
  prefixLen?: number | null;
}

export type { Position, Range, TextEdit };
