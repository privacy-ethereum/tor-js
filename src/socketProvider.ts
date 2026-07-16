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

/**
 * A bidirectional byte pipe to a Tor relay.
 *
 * Assign `onmessage` and `onclose` after creation.
 * Call `send(data)` with Uint8Array and `close()` when done.
 */
export class ArtiSocket {
  #send: (data: Uint8Array) => void;
  #close: () => void;
  #closed = false;
  #onclose: (() => void) | null = null;

  /** Set by transport on error, before onclose fires. */
  _error: string | null = null;

  /** Receive callback — transport fires this with each incoming chunk. */
  onmessage: ((data: Uint8Array) => void) | null = null;

  constructor(
    send: (data: Uint8Array) => void,
    close: () => void,
  ) {
    this.#send = send;
    this.#close = close;
  }

  /** Setter that fires immediately if close already happened. */
  set onclose(fn: (() => void) | null) {
    this.#onclose = fn;
    if (this.#closed && fn) queueMicrotask(() => fn());
  }
  get onclose(): (() => void) | null { return this.#onclose; }

  /** @internal — called by transport wrappers when the underlying connection closes. */
  _notifyClose(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#onclose?.();
  }

  send(data: Uint8Array): void {
    this.#send(data);
  }

  close(): void {
    this.#close();
  }

  // -- Transport factories --------------------------------------------------

  /** Wrap a Node.js net.Socket (already connected). */
  static fromNodeSocket(socket: any): ArtiSocket {
    const sock = new ArtiSocket(
      (data) => socket.write(data),
      () => socket.destroy(),
    );
    socket.on('data', (buf: Buffer) => sock.onmessage?.(new Uint8Array(buf)));
    socket.on('close', () => sock._notifyClose());
    socket.on('error', () => {});
    return sock;
  }

  /** Wrap a Deno TCP connection. */
  static fromDenoConn(conn: any): ArtiSocket {
    const sock = new ArtiSocket(
      (data) => {
        const writer = conn.writable.getWriter();
        writer.write(data).then(() => writer.releaseLock());
      },
      () => conn.close(),
    );
    (async () => {
      try {
        for await (const chunk of conn.readable) {
          sock.onmessage?.(new Uint8Array(chunk));
        }
      } catch {}
      sock._notifyClose();
    })();
    return sock;
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
