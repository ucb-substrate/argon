// A prompt for an Argon subcommand's argument, with completion.

import * as vscode from 'vscode';

import {
  byteLength,
  CONFIG_KEYS,
  completions,
  DEVICE_KINDS,
  isCellExpressionCommand,
  wordCompletions,
} from './commandLine';
import type { CommandCompletionResult } from './protocol';

/** How a subcommand's argument is completed and described. */
interface ArgumentSpec {
  title: string;
  placeholder: string;
}

const SPECS: Record<string, ArgumentSpec> = {
  openCell: { title: 'Argon: Open Cell', placeholder: 'A cell invocation, such as top(100.)' },
  newCell: { title: 'Argon: New Cell', placeholder: 'The new cell name' },
  renameCell: { title: 'Argon: Rename Cell', placeholder: 'The new name of the open cell' },
  inst: { title: 'Argon: Place Instance', placeholder: 'A cell invocation to place in the layout' },
  sinst: { title: 'Argon: Place Schematic Instance', placeholder: 'A cell invocation to place in the schematic' },
  device: { title: 'Argon: Place Device', placeholder: 'A device kind, model, and parameters, such as Nmos "nch", w=1.' },
  setConfig: { title: 'Argon: Set Configuration', placeholder: 'A dotted key and an optional TOML value' },
  saveConfig: { title: 'Argon: Save Configuration', placeholder: 'A path, or empty for the default location' },
};

type Complete = (command: string, text: string) => Promise<CommandCompletionResult | undefined>;

interface PromptItem extends vscode.QuickPickItem {
  /** The argument this item leaves when chosen, or `undefined` to run. */
  argument?: string;
}

/**
 * Asks for the argument of subcommand `name`, starting from `argument`.
 * Resolves to the argument, or `undefined` if the user cancels.
 */
export function promptArgument(name: string, argument: string, complete: Complete): Promise<string | undefined> {
  const spec = SPECS[name] ?? { title: `Argon: ${name}`, placeholder: 'Argument' };
  const quickPick = vscode.window.createQuickPick<PromptItem>();
  quickPick.title = spec.title;
  quickPick.placeholder = spec.placeholder;
  quickPick.value = argument;
  quickPick.matchOnDescription = false;
  quickPick.matchOnDetail = false;
  let generation = 0;

  const refresh = async (text: string) => {
    const current = ++generation;
    const run: PromptItem = {
      label: `$(play) ${name} ${text}`.trimEnd(),
      description: 'Run',
      alwaysShow: true,
    };
    quickPick.items = [run, ...quickPick.items.filter((item) => item.argument !== undefined)];
    const candidates = await argumentCompletions(name, text, complete);
    if (current !== generation) {
      return;
    }
    quickPick.items = [
      run,
      ...candidates.map((candidate) => ({
        label: candidate.label,
        description: candidate.argument,
        argument: candidate.argument,
        alwaysShow: true,
      })),
    ];
    quickPick.activeItems = [run];
  };

  return new Promise((resolve) => {
    let settled = false;
    const finish = (value: string | undefined) => {
      if (!settled) {
        settled = true;
        resolve(value);
        quickPick.dispose();
      }
    };
    quickPick.onDidChangeValue((text) => void refresh(text));
    quickPick.onDidAccept(() => {
      const chosen = quickPick.activeItems[0];
      if (chosen?.argument !== undefined) {
        quickPick.value = chosen.argument;
        void refresh(chosen.argument);
        return;
      }
      finish(quickPick.value);
    });
    quickPick.onDidHide(() => finish(undefined));
    quickPick.show();
    void refresh(argument);
  });
}

async function argumentCompletions(
  name: string,
  text: string,
  complete: Complete,
): Promise<{ label: string; argument: string }[]> {
  if (isCellExpressionCommand(name)) {
    const result = await complete(name, text);
    return completions(text, result?.prefixLen ?? 0, result?.items ?? []);
  }
  const words = name === 'device' ? DEVICE_KINDS : name === 'setConfig' ? CONFIG_KEYS : [];
  return wordCompletions(text, words).map((word) => ({ label: word, argument: `${word} ` }));
}

/** The cursor offset the analyzer expects for `text`. */
export function cursorOf(text: string): number {
  return byteLength(text);
}
