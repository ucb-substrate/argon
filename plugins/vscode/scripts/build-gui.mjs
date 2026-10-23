// Builds the Argon GUI as WebAssembly into media/gui, for the GUI panel.
//
// Needs the wasm32-unknown-unknown Rust target and a wasm-bindgen CLI whose
// version matches the wasm-bindgen crate in Cargo.lock.

import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const extension = fileURLToPath(new URL('..', import.meta.url));
const repository = path.resolve(extension, '../..');
// `--debug` builds quickly for development; the default is the small build
// the extension ships.
const debug = process.argv.includes('--debug');
const profile = debug ? 'dev' : 'web';
const outputDirectory = debug ? 'debug' : 'web';

execFileSync('cargo', ['build', '-p', 'argone-web', '--target', 'wasm32-unknown-unknown', '--profile', profile], {
  cwd: repository,
  stdio: 'inherit',
});
execFileSync(
  'wasm-bindgen',
  [
    '--target',
    'web',
    '--no-typescript',
    '--out-dir',
    path.join(extension, 'media', 'gui'),
    path.join(repository, 'target', 'wasm32-unknown-unknown', outputDirectory, 'argone_web.wasm'),
  ],
  { stdio: 'inherit' },
);
