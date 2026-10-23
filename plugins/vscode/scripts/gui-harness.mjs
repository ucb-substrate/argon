// Runs the browser GUI outside VS Code, for developing and checking it.
//
// The harness starts an analyzer for a project, serves the GUI page, and
// relays the page's analyzer connections the way the extension does. With
// --screenshot it opens the page in headless Chrome and saves a screenshot.
//
//   node scripts/gui-harness.mjs <PROJECT> [--cell 'top()'] [--port 8123]
//        [--screenshot out.png] [--wait 4000] [--size 1400x900]
//
// Build the GUI first with `npm run build:gui`, and the analyzer with
// `cargo build -p argon --bin argon-analyzer`, or set ARGON_ANALYZER.

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { createMessageConnection, StreamMessageReader, StreamMessageWriter } = require('vscode-jsonrpc/node');
const { WebSocketServer } = require('ws');

const extension = fileURLToPath(new URL('..', import.meta.url));
const repository = path.resolve(extension, '../..');
const { AnalyzerConnection } = require(path.join(extension, 'out', 'analyzerConnection.js'));

function option(name, fallback) {
  const index = process.argv.indexOf(`--${name}`);
  return index >= 0 ? process.argv[index + 1] : fallback;
}

const project = path.resolve(process.argv[2] ?? '.');
const cell = option('cell', 'top()');
const port = Number(option('port', '8123'));
const screenshot = option('screenshot');
const wait = Number(option('wait', '4000'));
const [width, height] = option('size', '1400x900').split('x').map(Number);
const analyzerPath =
  process.env.ARGON_ANALYZER ?? path.join(repository, 'target', 'debug', 'argon-analyzer');

// The analyzer, driven as a minimal LSP client.
const analyzer = spawn(analyzerPath, ['--embedded-gui'], { stdio: ['pipe', 'pipe', 'inherit'] });
const lsp = createMessageConnection(new StreamMessageReader(analyzer.stdout), new StreamMessageWriter(analyzer.stdin));
lsp.onRequest((method) => (method === 'workspace/applyEdit' ? { applied: false } : null));
lsp.onNotification('window/showMessage', (params) => console.log(`analyzer: ${params.message}`));
lsp.listen();
const rootUri = pathToFileURL(project).toString();
await lsp.sendRequest('initialize', {
  processId: process.pid,
  rootUri,
  workspaceFolders: [{ uri: rootUri, name: path.basename(project) }],
  capabilities: {},
});
lsp.sendNotification('initialized', {});
const lib = path.join(project, 'lib.ar');
lsp.sendNotification('textDocument/didOpen', {
  textDocument: { uri: pathToFileURL(lib).toString(), languageId: 'argon', version: 1, text: fs.readFileSync(lib, 'utf8') },
});
const endpoint = await lsp.sendRequest('custom/guiEndpoint');

// The page, the GUI module, and the relay.
const media = path.join(extension, 'media', 'gui');
const page = `<!doctype html>
<html><head><meta charset="utf-8"><title>Argon GUI</title>
<style>html, body { margin: 0; width: 100%; height: 100%; overflow: hidden; background: #1e1e1e; }</style>
</head><body><script type="module">
Error.stackTraceLimit = 200;
import init, { start, receive } from './argone_web.js';
await init({ module_or_path: './argone_web_bg.wasm' });
const socket = new WebSocket('ws://' + location.host + '/relay');
await new Promise((resolve) => socket.addEventListener('open', resolve));
socket.addEventListener('message', (event) => {
  const message = JSON.parse(event.data);
  if (message.data !== undefined) message.data = Uint8Array.from(atob(message.data), (c) => c.charCodeAt(0));
  receive(message);
});
start((message) => {
  if (message.data instanceof Uint8Array) {
    let text = '';
    for (const byte of message.data) text += String.fromCharCode(byte);
    message = { ...message, data: btoa(text) };
  }
  socket.send(JSON.stringify(message));
});
window.argonStarted = true;
</script></body></html>`;
const types = { '.js': 'text/javascript', '.wasm': 'application/wasm' };
const server = http.createServer((request, response) => {
  const name = new URL(request.url, 'http://localhost').pathname.slice(1);
  if (name === '' || name === 'index.html') {
    response.writeHead(200, { 'content-type': 'text/html' }).end(page);
    return;
  }
  const file = path.join(media, path.basename(name));
  if (!fs.existsSync(file)) {
    response.writeHead(404).end();
    return;
  }
  response.writeHead(200, { 'content-type': types[path.extname(file)] ?? 'application/octet-stream' });
  fs.createReadStream(file).pipe(response);
});
const roles = { gui: 'Gui', callback: 'GuiCallback' };
let guiConnected;
const connected = new Promise((resolve) => (guiConnected = resolve));
new WebSocketServer({ server, path: '/relay' }).on('connection', (socket) => {
  const connections = new Map();
  const post = (message) => socket.send(JSON.stringify(message));
  socket.on('message', async (raw) => {
    const message = JSON.parse(raw.toString());
    if (message.type === 'connect') {
      try {
        const connection = await AnalyzerConnection.connect(endpoint.port, endpoint.token, roles[message.role]);
        connections.set(message.id, connection);
        post({ type: 'opened', id: message.id });
        connection.start({
          frame: (frame) => post({ type: 'frame', id: message.id, data: frame.toString('base64') }),
          close: (reason) => post({ type: 'closed', id: message.id, reason }),
        });
        if (message.role === 'callback') {
          guiConnected();
        }
      } catch (error) {
        post({ type: 'closed', id: message.id, reason: String(error) });
      }
    } else if (message.type === 'frame') {
      connections.get(message.id)?.send(Buffer.from(message.data, 'base64'));
    } else if (message.type === 'close') {
      connections.get(message.id)?.close();
    } else {
      console.log(`page: ${JSON.stringify(message)}`);
    }
  });
  socket.on('close', () => connections.forEach((connection) => connection.close()));
});
await new Promise((resolve) => server.listen(port, '127.0.0.1', resolve));
console.log(`Argon GUI at http://127.0.0.1:${port}/`);

connected.then(async () => {
  console.log(`opening ${cell}`);
  await lsp.sendRequest('custom/openCell', { cell });
});

if (screenshot) {
  const puppeteer = require('puppeteer-core');
  const browser = await puppeteer.launch({
    executablePath:
      process.env.CHROME ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    headless: true,
    args: ['--enable-unsafe-webgpu', '--enable-features=Vulkan'],
    defaultViewport: { width, height },
  });
  const tab = await browser.newPage();
  tab.on('console', (message) => console.log(`console: ${message.text()}`));
  tab.on('pageerror', (error) => console.log(`page error: ${error.message}`));
  await tab.goto(`http://127.0.0.1:${port}/`);
  await connected;
  await new Promise((resolve) => setTimeout(resolve, wait));
  await tab.screenshot({ path: screenshot });
  console.log(`saved ${screenshot}`);
  await browser.close();
  analyzer.kill();
  server.close();
  process.exit(0);
}
