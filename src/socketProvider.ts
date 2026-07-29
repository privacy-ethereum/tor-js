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

import { KpsGateway, type GatewayResponse } from './kpsGateway.js';
import type { DialFn } from './kpsDial.js';

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
// Gateway selection
// ---------------------------------------------------------------------------

/** How many gateways carry traffic concurrently (the "preferred set"). */
const PREFERRED_GATEWAYS = 2;

/** Per-attempt deadline and post-failure cooldown, in ms. */
export interface GatewayTiming {
  /** Budget for one gateway attempt: dial + open stream + response head. */
  attemptTimeoutMs: number;
  /** Cooldown after a first failure; doubles per consecutive failure. */
  cooldownBaseMs: number;
  /** Ceiling on the cooldown. */
  cooldownMaxMs: number;
}

const DEFAULT_TIMING: GatewayTiming = {
  attemptTimeoutMs: 15_000,
  cooldownBaseMs: 2_000,
  cooldownMaxMs: 60_000,
};

/** Liveness and load bookkeeping for one configured gateway. */
interface GatewayState {
  gw: KpsGateway;
  /** Open tunnels (and in-progress attempts) carried by this gateway. */
  inFlight: number;
  /** Consecutive failures; drives the cooldown length. */
  failures: number;
  /** Epoch ms before which this gateway is passed over. */
  notBefore: number;
}

/**
 * Fisher-Yates. The gateway list is an unordered set, so each client picks its
 * own preference order at construction: no address is privileged by its
 * position in config, and independent clients spread across an operator's
 * gateways instead of all piling onto the first one.
 */
function shuffled<T>(items: T[]): T[] {
  const out = items.slice();
  for (let i = out.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [out[i], out[j]] = [out[j], out[i]];
  }
  return out;
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
   * Gateway KPS address(es) (`ip:port:certhash`, e.g.
   * `"198.51.100.7:12298:uEiAxk...9Qw"`). Pass several redundant gateways to
   * fail over and spread load between them.
   *
   * The list is an **unordered set**: position carries no priority, and each
   * client shuffles it to pick its own preference. (Explicit priorities or
   * weights may be added later.) Required in browsers for relay connections.
   * Optional in Node.js/Deno (enables fast bootstrap and gateway fallback when
   * provided; requires the optional `@kpstreams/quic-client` package).
   */
  gateway?: string | string[];

  /**
   * Custom KPS dialer, applied to every gateway. Defaults to the built-in
   * WebRTC/QUIC dialer. Inject one to reach gateways over a transport you
   * already hold (e.g. a KPS capability granted to a sandboxed worker).
   */
  dial?: DialFn;

  /**
   * Ordered list of strategies to try: `"direct"`, `"kps"`.
   * Defaults based on environment and whether a gateway address is provided.
   */
  strategies?: string[];

  /**
   * Override the per-attempt deadline and failure cooldown. Advanced; mainly
   * useful for tests that need short timings.
   */
  timing?: Partial<GatewayTiming>;
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
  #states: GatewayState[] = [];
  #strategies: string[];
  #timing: GatewayTiming;

  constructor(options: ArtiSocketProviderOptions = {}) {
    const addresses =
      options.gateway == null ? []
      : Array.isArray(options.gateway) ? options.gateway
      : [options.gateway];
    for (const address of shuffled(addresses)) {
      if (/^(https?|wss?):\/\//.test(address)) {
        throw new Error(
          `gateway is now a KPS address ("ip:port:certhash"), not a URL — got "${address}". ` +
          'Gateways expose their address at startup and in /metadata.json.',
        );
      }
      this.#states.push({
        gw: new KpsGateway(address, { dial: options.dial }),
        inFlight: 0,
        failures: 0,
        notBefore: 0,
      });
    }
    this.#strategies = options.strategies ?? defaultStrategies(this.#states.length > 0);
    this.#timing = { ...DEFAULT_TIMING, ...options.timing };
  }

  /**
   * The gateway currently preferred for single-gateway work, or null if none is
   * configured. Prefer {@link gatewayFetch} for requests — it falls over.
   */
  get gateway(): KpsGateway | null {
    return this.#candidates()[0]?.gw ?? null;
  }

  /**
   * Gateways in the order to try them for one operation; the first is the pick,
   * the rest are fallbacks.
   *
   * Least-outstanding decides between members of the preferred set, and is the
   * whole latency story here: a slow or stalled gateway accumulates in-flight
   * work and stops being chosen, with no probing or RTT bookkeeping. Because
   * the sort is stable, equal load keeps the construction-time shuffle — so a
   * client whose tunnels don't overlap reuses one gateway (and fast bootstrap
   * contacts exactly one), while concurrent load spreads across the set.
   */
  #candidates(): GatewayState[] {
    const now = Date.now();
    const ready = this.#states.filter((s) => s.notBefore <= now);
    // Membership of the preferred set is by shuffle order, so it's stable and
    // only shifts when a member starts cooling down — that keeps a large
    // configured list from fanning traffic out to every gateway at once.
    // Within the set, least-outstanding picks the winner.
    const preferred = ready
      .slice(0, PREFERRED_GATEWAYS)
      .sort((a, b) => a.inFlight - b.inFlight);
    const rest = ready.slice(PREFERRED_GATEWAYS);
    // Cooling-down gateways stay as a last resort, soonest-ready first: better
    // to attempt a recently-failed gateway than to fail without trying any.
    const cooling = this.#states
      .filter((s) => s.notBefore > now)
      .sort((a, b) => a.notBefore - b.notBefore);
    return [...preferred, ...rest, ...cooling];
  }

  #onSuccess(s: GatewayState): void {
    s.failures = 0;
    s.notBefore = 0;
  }

  /** Cool a failed gateway off for min(base·2^(n-1), max), 50-100% jittered. */
  #onFailure(s: GatewayState): void {
    s.failures += 1;
    const { cooldownBaseMs, cooldownMaxMs } = this.#timing;
    const exp = Math.min(cooldownMaxMs, cooldownBaseMs * 2 ** (s.failures - 1));
    s.notBefore = Date.now() + Math.round(exp * (0.5 + Math.random() * 0.5));
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

  /** Close all KPS gateway connections and release resources. */
  close(): void {
    for (const s of this.#states) s.gw.close();
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
    if (!this.#states.length) throw new Error('kps strategy requires a gateway address');

    // Every candidate is tried for the SAME target: gateway failure must never
    // change which relay we connect to, or a hostile gateway could steer relay
    // (guard) selection by selectively refusing CONNECTs.
    // TODO(#9): richer selection — measured latency, explicit priority/weights,
    // and persistent guard-set semantics — is still open.
    const errors: string[] = [];
    for (const s of this.#candidates()) {
      s.inFlight += 1;
      try {
        const sock = await s.gw.connect(target, {
          signal: AbortSignal.timeout(this.#timing.attemptTimeoutMs),
        });
        this.#onSuccess(s);
        // Hold the count for the tunnel's lifetime, not just the handshake, so
        // a gateway carrying many live tunnels reads as busy.
        const release = () => { s.inFlight -= 1; };
        sock.closed.then(release, release);
        return sock;
      } catch (e: any) {
        s.inFlight -= 1;
        this.#onFailure(s);
        errors.push(`${s.gw.address}: ${e.message}`);
      }
    }
    throw new Error(`all gateways failed for ${target}: ${errors.join('; ')}`);
  }

  /**
   * One KPS-HTTP/1 GET against a gateway (used for fast bootstrap), falling
   * over to the next candidate on failure. The happy path contacts exactly one
   * gateway — bootstrap is not raced.
   */
  async gatewayFetch(path: string): Promise<GatewayResponse> {
    if (!this.#states.length) throw new Error('no gateway configured');
    const errors: string[] = [];
    for (const s of this.#candidates()) {
      try {
        const res = await s.gw.fetch(path, {
          signal: AbortSignal.timeout(this.#timing.attemptTimeoutMs),
        });
        this.#onSuccess(s);
        return res;
      } catch (e: any) {
        this.#onFailure(s);
        errors.push(`${s.gw.address}: ${e.message}`);
      }
    }
    throw new Error(`all gateways failed for ${path}: ${errors.join('; ')}`);
  }
}
