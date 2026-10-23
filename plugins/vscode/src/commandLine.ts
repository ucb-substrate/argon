// Argon subcommands and their arguments, independent of VS Code.

import type { CompletionItem, EditorCommand } from './protocol';

/** The variants of `std::schematic::DeviceKind`, for `device`. */
export const DEVICE_KINDS = [
  'Mos',
  'Nmos',
  'Pmos',
  'Res',
  'Cap',
  'Diode',
  'Bjt',
  'Npn',
  'Pnp',
  'Subckt',
];

/** Configuration keys `setConfig` offers. */
export const CONFIG_KEYS = [
  'agent.approval',
  'analyzer.compile_debounce_ms',
  'gui.dark_mode',
  'gui.font_size',
  'gui.hierarchy_depth',
  'gui.icon_size',
  'gui.option_as_meta',
  'log.level',
];

/** Subcommands whose argument is a cell invocation the analyzer completes. */
export const CELL_EXPRESSION_COMMANDS = ['openCell', 'inst', 'sinst'] as const;

export type CellExpressionCommand = (typeof CELL_EXPRESSION_COMMANDS)[number];

export function isCellExpressionCommand(name: string): name is CellExpressionCommand {
  return (CELL_EXPRESSION_COMMANDS as readonly string[]).includes(name);
}

/** A subcommand the GUI asked for, with the argument typed so far. */
export interface Subcommand {
  name: string;
  argument: string;
  submit: boolean;
}

/** The subcommand in `command`, or `undefined` for the message log or none. */
export function subcommand(command: EditorCommand | null | undefined): Subcommand | undefined {
  if (typeof command !== 'object' || command === null) {
    return undefined;
  }
  const argon = command.argon;
  if (typeof argon !== 'object' || argon === null || typeof argon.name !== 'string') {
    return undefined;
  }
  return {
    name: argon.name,
    argument: typeof argon.argument === 'string' ? argon.argument : '',
    submit: argon.submit === true,
  };
}

/** The UTF-8 length of `text`, which the analyzer measures offsets in. */
export function byteLength(text: string): number {
  return Buffer.byteLength(text, 'utf8');
}

/** `text` without its last `bytes` UTF-8 bytes. */
export function dropTrailingBytes(text: string, bytes: number): string {
  const encoded = Buffer.from(text, 'utf8');
  const keep = Math.max(0, encoded.length - Math.max(0, bytes));
  return encoded.subarray(0, keep).toString('utf8');
}

/** The identifier at the end of `text`, which a completion replaces. */
export function identifierPrefix(text: string): string {
  return /[A-Za-z0-9_]*$/.exec(text)?.[0] ?? '';
}

/**
 * The candidates for `items` that extend the identifier before the cursor,
 * each as the whole argument it would leave.
 */
export function completions(
  text: string,
  prefixLen: number,
  items: readonly CompletionItem[],
): { label: string; argument: string }[] {
  const clamped = Math.min(Math.max(0, prefixLen), byteLength(text));
  const head = dropTrailingBytes(text, clamped);
  const prefix = text.slice(head.length);
  return items
    .filter((item) => item.label.startsWith(prefix))
    .map((item) => ({ label: item.label, argument: head + (item.insertText ?? item.label) }));
}

/** The words of `candidates` that start the last word of `text`. */
export function wordCompletions(text: string, candidates: readonly string[]): string[] {
  if (/\s/.test(text.trimStart())) {
    return [];
  }
  const lead = text.trimStart();
  return candidates.filter((candidate) => candidate.startsWith(lead));
}
