// Where Argon projects and the analyzer's files live, independent of VS Code.

import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

/** The nearest directory at or above `file`'s directory holding `lib.ar`. */
export function findRoot(file: string): string | undefined {
  let directory = path.dirname(file);
  for (;;) {
    if (fs.existsSync(path.join(directory, 'lib.ar'))) {
      return directory;
    }
    const parent = path.dirname(directory);
    if (parent === directory) {
      return undefined;
    }
    directory = parent;
  }
}

/** Whether `file` is `directory` or inside it. */
export function isInside(file: string, directory: string): boolean {
  const relative = path.relative(directory, file);
  return relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative));
}

function home(env: NodeJS.ProcessEnv): string {
  return env.HOME || os.homedir();
}

/**
 * The directory the analyzer writes the embedded standard library into:
 * `$XDG_CACHE_HOME/argon/std`, or `~/.cache/argon/std`.
 */
export function stdCacheDir(env: NodeJS.ProcessEnv = process.env): string {
  const base = env.XDG_CACHE_HOME || path.join(home(env), '.cache');
  return path.join(base, 'argon', 'std');
}

/** The Argon log: `$XDG_STATE_HOME/argon/argon.log`, or under `~/.local/state`. */
export function logPath(env: NodeJS.ProcessEnv = process.env): string {
  const base = env.XDG_STATE_HOME || path.join(home(env), '.local', 'state');
  return path.join(base, 'argon', 'argon.log');
}

/** A glob matching every file under `directory`, with its special characters escaped. */
export function globUnder(directory: string): string {
  const escaped = directory
    .split(path.sep)
    .join('/')
    .replace(/[*?[\]{}]/g, (character) => `[${character}]`);
  return `${escaped.replace(/\/$/, '')}/**/*`;
}
