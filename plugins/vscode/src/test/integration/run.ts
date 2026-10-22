// Runs the integration suite in a downloaded VS Code against a built analyzer.

import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { runTests } from '@vscode/test-electron';

async function main(): Promise<void> {
  const extensionDevelopmentPath = path.resolve(__dirname, '../../..');
  const repository = path.resolve(extensionDevelopmentPath, '../..');
  const analyzer = process.env.ARGON_ANALYZER ?? path.join(repository, 'target', 'debug', 'argon-analyzer');
  if (!fs.existsSync(analyzer)) {
    throw new Error(`${analyzer} does not exist; run \`cargo build -p argon-analyzer\` or set ARGON_ANALYZER`);
  }
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'argon-vscode-'));
  const project = path.join(scratch, 'project');
  fs.mkdirSync(path.join(project, '.vscode'), { recursive: true });
  fs.writeFileSync(
    path.join(project, '.vscode', 'settings.json'),
    JSON.stringify({ 'argon.analyzer.path': analyzer }, null, 2),
  );
  try {
    await runTests({
      extensionDevelopmentPath,
      extensionTestsPath: path.resolve(__dirname, 'suite'),
      launchArgs: [
        project,
        '--disable-extensions',
        '--skip-welcome',
        '--skip-release-notes',
        // Lets the GUI render with software WebGL on machines without a GPU.
        '--enable-unsafe-swiftshader',
      ],
      extensionTestsEnv: {
        ARGON_ANALYZER: analyzer,
        ARGON_TEST_PROJECT: project,
        XDG_STATE_HOME: path.join(scratch, 'state'),
        XDG_CACHE_HOME: path.join(scratch, 'cache'),
      },
    });
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
}

main().catch((error: unknown) => {
  console.error(error);
  process.exit(1);
});
