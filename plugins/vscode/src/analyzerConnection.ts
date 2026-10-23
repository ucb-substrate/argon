// Authenticated connections to the analyzer's RPC listener, which the GUI
// webview cannot open itself.
//
// This mirrors the analyzer's `transport` module: frames carry a 4-byte
// big-endian length, and both sides prove they hold the session token with
// HMAC-SHA256 over the handshake's nonces.

import { createHmac, randomBytes, timingSafeEqual } from 'crypto';
import * as net from 'net';

/** The handshake version the analyzer speaks. */
export const PROTOCOL_VERSION = 3;
const SECRET_BYTES = 32;
const HANDSHAKE_TIMEOUT_MS = 5000;
const MAX_HANDSHAKE_FRAME = 4096;

/** What a connection is for. */
export type Role = 'Gui' | 'GuiCallback' | 'Agent';

const ROLE_TAGS: Record<Role, number> = { Gui: 0, GuiCallback: 1, Agent: 2 };

/** Splits a byte stream into length-delimited frames. */
export class FrameReader {
  private buffer: Buffer = Buffer.alloc(0);

  /** Adds `chunk` and returns the frames it completes. */
  push(chunk: Buffer): Buffer[] {
    this.buffer = this.buffer.length === 0 ? chunk : Buffer.concat([this.buffer, chunk]);
    const frames: Buffer[] = [];
    while (this.buffer.length >= 4) {
      const length = this.buffer.readUInt32BE(0);
      if (this.buffer.length < 4 + length) {
        break;
      }
      frames.push(this.buffer.subarray(4, 4 + length));
      this.buffer = this.buffer.subarray(4 + length);
    }
    return frames;
  }

  /** The bytes of an incomplete frame. */
  get pending(): number {
    return this.buffer.length;
  }
}

/** `frame` with its length prefix. */
export function encodeFrame(frame: Uint8Array): Buffer {
  const header = Buffer.alloc(4);
  header.writeUInt32BE(frame.length, 0);
  return Buffer.concat([header, Buffer.from(frame.buffer, frame.byteOffset, frame.byteLength)]);
}

/** The proof that `prover` holds the token, bound to one handshake. */
export function proof(
  token: Buffer,
  prover: 'analyzer' | 'client',
  role: Role,
  clientNonce: Buffer,
  analyzerNonce: Buffer,
): Buffer {
  const version = Buffer.alloc(4);
  version.writeUInt32BE(PROTOCOL_VERSION, 0);
  return createHmac('sha256', token)
    .update(prover === 'analyzer' ? 'argon-analyzer' : 'argon-client')
    .update(version)
    .update(Buffer.from([ROLE_TAGS[role]]))
    .update(clientNonce)
    .update(analyzerNonce)
    .digest();
}

function fromHex(text: string): Buffer | undefined {
  return /^[0-9a-f]{64}$/i.test(text.trim()) ? Buffer.from(text.trim(), 'hex') : undefined;
}

/** What a connection reports once started. */
export interface ConnectionHandlers {
  frame(frame: Buffer): void;
  close(reason: string): void;
}

/** An authenticated connection that carries whole frames. */
export class AnalyzerConnection {
  private started = false;

  private constructor(
    private readonly socket: net.Socket,
    private readonly reader: FrameReader,
    /** Frames that arrived with the handshake's last reply. */
    private readonly early: Buffer[],
  ) {}

  /** Delivers frames, including any that arrived during the handshake. */
  start(handlers: ConnectionHandlers): void {
    if (this.started) {
      return;
    }
    this.started = true;
    for (const frame of this.early.splice(0)) {
      handlers.frame(frame);
    }
    for (const frame of this.reader.push(Buffer.alloc(0))) {
      handlers.frame(frame);
    }
    let reason = 'the analyzer closed the connection';
    this.socket.on('data', (chunk: Buffer) => {
      for (const frame of this.reader.push(chunk)) {
        handlers.frame(frame);
      }
    });
    this.socket.on('error', (error) => {
      reason = error.message;
    });
    this.socket.on('close', () => handlers.close(reason));
    this.socket.resume();
  }

  /** Connects to the analyzer on `port` as `role`, proving it holds `tokenHex`. */
  static connect(port: number, tokenHex: string, role: Role): Promise<AnalyzerConnection> {
    const token = fromHex(tokenHex);
    if (!token) {
      return Promise.reject(new Error('malformed session token'));
    }
    return new Promise((resolve, reject) => {
      const socket = net.connect({ host: '127.0.0.1', port });
      const reader = new FrameReader();
      const clientNonce = randomBytes(SECRET_BYTES);
      let analyzerNonce: Buffer | undefined;
      let settled = false;
      const fail = (error: Error) => {
        if (!settled) {
          settled = true;
          clearTimeout(timer);
          socket.destroy();
          reject(error);
        }
      };
      const timer = setTimeout(() => fail(new Error('the analyzer handshake timed out')), HANDSHAKE_TIMEOUT_MS);
      const send = (value: unknown) => socket.write(encodeFrame(Buffer.from(JSON.stringify(value))));
      const onData = (chunk: Buffer) => {
        if (!analyzerNonce && reader.pending + chunk.length > MAX_HANDSHAKE_FRAME + 4) {
          fail(new Error('the analyzer sent an oversized handshake'));
          return;
        }
        const frames = reader.push(chunk);
        for (const [index, frame] of frames.entries()) {
          let reply: any;
          try {
            reply = JSON.parse(frame.toString('utf8'));
          } catch {
            fail(new Error('the analyzer sent a malformed handshake'));
            return;
          }
          if (!analyzerNonce) {
            if (reply?.Rejected !== undefined) {
              fail(new Error(`the analyzer rejected the connection: ${reply.Rejected}`));
              return;
            }
            const nonce = fromHex(String(reply?.Challenge?.nonce ?? ''));
            const analyzerProof = fromHex(String(reply?.Challenge?.proof ?? ''));
            if (!nonce || !analyzerProof) {
              fail(new Error('the analyzer sent a malformed challenge'));
              return;
            }
            const expected = proof(token, 'analyzer', role, clientNonce, nonce);
            if (!timingSafeEqual(expected, analyzerProof)) {
              fail(new Error(`the process listening on port ${port} does not hold this session token`));
              return;
            }
            analyzerNonce = nonce;
            send({ proof: proof(token, 'client', role, clientNonce, nonce).toString('hex') });
          } else if (reply === 'Accepted') {
            settled = true;
            clearTimeout(timer);
            // Hold later frames until `start` adds its handlers.
            socket.pause();
            socket.off('data', onData);
            socket.removeAllListeners('error');
            socket.removeAllListeners('close');
            resolve(new AnalyzerConnection(socket, reader, frames.slice(index + 1)));
            return;
          } else {
            fail(new Error(`the analyzer rejected the connection: ${reply?.Rejected ?? 'unknown reason'}`));
            return;
          }
        }
      };
      socket.on('data', onData);
      socket.on('error', fail);
      socket.on('close', () => fail(new Error('the analyzer closed the connection during the handshake')));
      socket.on('connect', () => {
        send({ version: PROTOCOL_VERSION, role, nonce: clientNonce.toString('hex') });
      });
    });
  }

  /** Sends one frame. */
  send(frame: Uint8Array): void {
    this.socket.write(encodeFrame(frame));
  }

  close(): void {
    this.socket.destroy();
  }
}
