/**
 * Socket provider for connecting to Tor relays via direct TCP or a
 * tor-js-gateway reached over KPS (WebRTC in browsers, QUIC in Node).
 *
 * ArtiSocketProvider auto-detects available strategies based on environment:
 * - Node.js/Deno: tries direct TCP first, then the KPS gateway if one is set
 * - Browsers: KPS gateway only (browsers can't open TCP sockets)
 *
 * Each `connect(target)` call returns an {@link ArtiSocket} — a uniform
 * bidirectional byte pipe regardless of transport.
 */

import { KpsGateway } from './kpsGateway.js';

// ---------------------------------------------------------------------------
// Environment detection
// ---------------------------------------------------------------------------

const HAS_DENO = typeof (globalThis as any).Deno !== 'undefined';
const HAS_NODE = typeof (globalThis as any).process?.versions?.node !== 'undefined';

function defaultStrategies(hasGateway: boolean): string[] {
  const s: string[] = [];
  if (HAS_DENO || HAS_NODE) s.push('direct');
  if (hasGateway) s.push('kps');
  return s;
}

// ---------------------------------------------------------------------------
// ArtiSocket — uniform bidirectional byte pipe
// ---------------------------------------------------------------------------

/** How a socket ended: cleanly, or with an error reason. */
export interface ArtiSocketCloseInfo {
  ok: boolean;
  reason?: string;
}

/** The parts a transport supplies to build an {@link ArtiSocket}. */
export interface ArtiSocketParts {
  readable: ReadableStream<Uint8Array>;
  writable: WritableStream<Uint8Array>;
  closed: Promise<ArtiSocketCloseInfo>;
  closeWrite: () => Promise<void>;
  close: () => void;
}

/**
 * A bidirectional byte pipe to a Tor relay, shaped like a KPS `Stream`.
 *
 * Data flows through the WHATWG `readable`/`writable` streams, which carry
 * backpressure end to end: the consumer (the WASM runtime) pulls from
 * `readable` on demand, so the transport only pulls from the network when
 * arti actually reads, and writes await the sink so a slow relay throttles
 * the writer. There is no intermediate buffering or event queue.
 */
export class ArtiSocket {
  /** Inbound bytes. Pull-based: reading drives the transport's network pull. */
  readonly readable: ReadableStream<Uint8Array>;
  /** Outbound bytes. The writer's backpressure reflects the transport buffer. */
  readonly writable: WritableStream<Uint8Array>;
  /** Resolves when the socket is fully closed. */
  readonly closed: Promise<ArtiSocketCloseInfo>;

  #closeWrite: () => Promise<void>;
  #close: () => void;

  constructor(parts: ArtiSocketParts) {
    this.readable = parts.readable;
    this.writable = parts.writable;
    this.closed = parts.closed;
    this.#closeWrite = parts.closeWrite;
    this.#close = parts.close;
  }

  /** Half-close the write side (the peer sees a TCP FIN); reads continue. */
  closeWrite(): Promise<void> {
    return this.#closeWrite();
  }

  /** Tear down both halves of the socket. */
  close(): void {
    this.#close();
  }

  // -- Transport factories --------------------------------------------------

  /** Wrap a Node.js net.Socket (already connected) as WHATWG streams. */
  static async fromNodeSocket(socket: any): Promise<ArtiSocket> {
    // Duplex.toWeb bridges the socket to WHATWG streams with backpressure in
    // both directions (it pauses the socket when the reader isn't pulling).
    const { Duplex } = await import('node:stream');
    const { readable, writable } = Duplex.toWeb(socket);
    return new ArtiSocket({
      readable: readable as ReadableStream<Uint8Array>,
      writable: writable as WritableStream<Uint8Array>,
      closed: new Promise<ArtiSocketCloseInfo>((resolve) => {
        socket.once('close', (hadError: boolean) => resolve({ ok: !hadError }));
      }),
      closeWrite: () => new Promise<void>((resolve) => socket.end(resolve)),
      close: () => socket.destroy(),
    });
  }

  /** Wrap a Deno TCP connection (whose readable/writable are already WHATWG). */
  static fromDenoConn(conn: any): ArtiSocket {
    let onClosed!: () => void;
    const closed = new Promise<ArtiSocketCloseInfo>((resolve) => {
      onClosed = () => resolve({ ok: true });
    });
    return new ArtiSocket({
      readable: conn.readable,
      writable: conn.writable,
      closed,
      closeWrite: () => (conn.closeWrite ? conn.closeWrite() : Promise.resolve()),
      close: () => {
        try { conn.close(); } catch { /* already closed */ }
        onClosed();
      },
    });
  }
}

// ---------------------------------------------------------------------------
// ArtiSocketProvider — multi-strategy connection manager
// ---------------------------------------------------------------------------

/**
 * Options for creating an ArtiSocketProvider.
 */
export interface ArtiSocketProviderOptions {
  /**
   * Gateway KPS address (`ip:port:certhash`, e.g.
   * `"198.51.100.7:12298:uEiAxk...9Qw"`).
   * Required in browsers for relay connections.
   * Optional in Node.js/Deno (enables fast bootstrap and gateway fallback
   * when provided; requires the optional `@kpstreams/quic-client` package).
   */
  gateway?: string;

  /**
   * Ordered list of strategies to try: `"direct"`, `"kps"`.
   * Defaults based on environment and whether a gateway address is provided.
   */
  strategies?: string[];
}

/**
 * Opens sockets to Tor relays via configurable strategies (direct TCP,
 * KPS gateway tunnels) with automatic fallback.
 *
 * The gateway address is optional — without it, only the `direct` strategy
 * is available (Node.js/Deno native TCP). With a gateway address, the `kps`
 * strategy becomes available (the only option in browsers).
 */
export class ArtiSocketProvider {
  #gateway: KpsGateway | null = null;
  #strategies: string[];

  constructor(options: ArtiSocketProviderOptions = {}) {
    if (options.gateway) {
      if (/^(https?|wss?):\/\//.test(options.gateway)) {
        throw new Error(
          `gateway is now a KPS address ("ip:port:certhash"), not a URL — got "${options.gateway}". ` +
          'Gateways expose their address at startup and in /metadata.json.',
        );
      }
      this.#gateway = new KpsGateway(options.gateway);
    }
    this.#strategies = options.strategies ?? defaultStrategies(!!this.#gateway);
  }

  /** The KPS gateway client, when a gateway address was configured. */
  get gateway(): KpsGateway | null {
    return this.#gateway;
  }

  /**
   * Open a relay socket to the given target (e.g. "198.51.100.1:9001").
   * Tries each configured strategy in order until one succeeds.
   */
  async connect(target: string): Promise<ArtiSocket> {
    const errors: string[] = [];

    for (const strategy of this.#strategies) {
      try {
        switch (strategy) {
          case 'direct':
            return await this.#connectDirect(target);
          case 'kps':
            return await this.#connectKps(target);
          default:
            throw new Error(`unknown strategy: ${strategy}`);
        }
      } catch (e: any) {
        errors.push(`${strategy}: ${e.message}`);
      }
    }

    throw new Error(`all strategies failed for ${target}: ${errors.join('; ')}`);
  }

  /** Close the KPS gateway connection and release resources. */
  close(): void {
    this.#gateway?.close();
  }

  // -- Direct TCP strategy (Node.js / Deno) ---------------------------------

  async #connectDirect(target: string): Promise<ArtiSocket> {
    const [host, portStr] = target.split(':');
    const port = parseInt(portStr, 10);

    if (HAS_DENO) {
      const conn = await (globalThis as any).Deno.connect({ hostname: host, port });
      return ArtiSocket.fromDenoConn(conn);
    }

    if (HAS_NODE) {
      const net = await import('node:net');
      const socket = net.createConnection({ host, port });
      await new Promise<void>((resolve, reject) => {
        socket.once('connect', resolve);
        socket.once('error', reject);
      });
      return ArtiSocket.fromNodeSocket(socket);
    }

    throw new Error('direct TCP not available in this environment');
  }

  // -- KPS gateway strategy -------------------------------------------------

  async #connectKps(target: string): Promise<ArtiSocket> {
    if (!this.#gateway) throw new Error('kps strategy requires a gateway address');
    return this.#gateway.connect(target);
  }
}
