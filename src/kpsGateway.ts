/**
 * tor-js-gateway client (KPS edition).
 *
 * The gateway speaks KPS-HTTP/1 (see tor-js-gateway/PROTOCOL.md): strict
 * HTTP/1.1 syntax over KPS streams, one exchange per stream, bodies
 * delimited by stream FIN. Clients dial the gateway's `ip:port:certhash`
 * address — no URL, no CA, no DNS. In browsers the dial goes over WebRTC
 * (@kpstreams/webrtc-client); in Node it goes over QUIC via the optional
 * @kpstreams/quic-client package.
 *
 * Relay connections are HTTP CONNECT tunnels (PROTOCOL.md §4): after the
 * gateway's 200 the stream is the raw TCP byte pipe to the relay.
 */

import { parseAddress, type Connection, type Stream } from '@kpstreams/core';
import { ArtiSocket, type ArtiSocketCloseInfo } from './socketProvider.js';

const enc = new TextEncoder();
const dec = new TextDecoder();

/** Timeout for opening a new stream on an established KPS connection.
 * openStream() has no default timeout and can hang forever if the
 * connection dies mid-open (kps ISSUES #14), so always bound it. */
const OPEN_STREAM_TIMEOUT_MS = 20_000;

interface ResponseHead {
  status: number;
  statusText: string;
  headers: Record<string, string>;
  /** Body bytes that arrived in the same chunks as the head. */
  extra: Uint8Array;
}

/**
 * Read a response head (status line + headers) from a stream reader.
 */
async function readHead(reader: ReadableStreamDefaultReader<Uint8Array>): Promise<ResponseHead> {
  let buf = new Uint8Array(0);
  for (;;) {
    const sep = findHeadEnd(buf);
    if (sep !== -1) {
      const head = dec.decode(buf.subarray(0, sep));
      const lines = head.split('\r\n');
      const m = lines[0].match(/^HTTP\/1\.1 (\d{3})\s*(.*)$/);
      if (!m) throw new Error(`malformed status line: ${lines[0]}`);
      const headers: Record<string, string> = {};
      for (const line of lines.slice(1)) {
        const i = line.indexOf(':');
        if (i === -1) continue;
        headers[line.slice(0, i).toLowerCase()] = line.slice(i + 1).trim();
      }
      return {
        status: parseInt(m[1], 10),
        statusText: m[2],
        headers,
        extra: buf.subarray(sep + 4),
      };
    }
    const { done, value } = await reader.read();
    if (done) throw new Error('stream ended before response head');
    const next = new Uint8Array(buf.length + value.length);
    next.set(buf, 0);
    next.set(value, buf.length);
    buf = next;
  }
}

function findHeadEnd(buf: Uint8Array): number {
  for (let i = 0; i + 3 < buf.length; i++) {
    if (buf[i] === 13 && buf[i + 1] === 10 && buf[i + 2] === 13 && buf[i + 3] === 10) {
      return i;
    }
  }
  return -1;
}

function concat(chunks: Uint8Array[], length: number): Uint8Array {
  const out = new Uint8Array(length);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return out;
}

/**
 * Pick the KPS dialer for this environment: WebRTC in browsers, QUIC in
 * Node/Deno via the optional @kpstreams/quic-client package.
 */
async function kpsDial(address: string): Promise<Connection> {
  if (typeof (globalThis as any).RTCPeerConnection !== 'undefined') {
    const { dial } = await import('@kpstreams/webrtc-client');
    return dial(address);
  }
  // Non-literal specifier so bundlers don't try to resolve the optional
  // native package into browser builds.
  const quicClientPkg = '@kpstreams/quic-client';
  let mod: { dial: (addr: string) => Promise<Connection> };
  try {
    mod = await import(/* @vite-ignore */ quicClientPkg);
  } catch {
    throw new Error(
      'kps: no transport available. Browsers need RTCPeerConnection; ' +
      "in Node, install the optional '@kpstreams/quic-client' package to reach a gateway over QUIC.",
    );
  }
  return mod.dial(address);
}

export interface GatewayResponse {
  status: number;
  statusText: string;
  headers: Record<string, string>;
  body: Uint8Array;
}

/**
 * A connection to one tor-js-gateway, addressed by its KPS address
 * (`ip:port:certhash`). The underlying KPS connection is dialed lazily and
 * reused; every request/tunnel is its own stream on it. Re-dials
 * automatically if the connection drops.
 */
export class KpsGateway {
  #address: string;
  #certhash: string;
  #connPromise: Promise<Connection> | null = null;
  // Per-connection teardown callbacks. The JS kps client does not reliably
  // settle streams when the connection dies (kps ISSUES #4) — a reader
  // blocked on stream.readable can hang forever — so every socket/exchange
  // registers a teardown that conn.closed triggers.
  #teardowns = new Map<Connection, Set<() => void>>();
  #closed = false;

  /** @param address KPS address (`ip:port:certhash`). */
  constructor(address: string) {
    this.#address = address.trim();
    // Validates the address shape early and yields the certhash, which is
    // the Host value the protocol recommends (PROTOCOL.md §3.2).
    this.#certhash = parseAddress(this.#address).certhash;
  }

  get address(): string {
    return this.#address;
  }

  /** Dial (or reuse) the KPS connection. */
  async #connection(): Promise<Connection> {
    if (this.#closed) throw new Error('KpsGateway is closed');
    if (!this.#connPromise) {
      const p = kpsDial(this.#address).then(
        (conn) => {
          this.#teardowns.set(conn, new Set());
          // conn.closed may resolve OR reject (kps rejects it with the close
          // reason, e.g. null, on some teardowns); run cleanup either way and
          // never leave the rejection unhandled.
          const onClosed = () => {
            if (this.#connPromise === p) this.#connPromise = null;
            const teardowns = this.#teardowns.get(conn);
            this.#teardowns.delete(conn);
            for (const fn of teardowns ?? []) fn();
          };
          conn.closed.then(onClosed, onClosed);
          return conn;
        },
        (err) => {
          if (this.#connPromise === p) this.#connPromise = null;
          throw err;
        },
      );
      this.#connPromise = p;
    }
    return this.#connPromise;
  }

  #addTeardown(conn: Connection, fn: () => void): () => void {
    const set = this.#teardowns.get(conn);
    if (!set) {
      // Connection already closed; tear down immediately.
      queueMicrotask(fn);
      return () => {};
    }
    set.add(fn);
    return () => set.delete(fn);
  }

  async #openStream(conn: Connection): Promise<Stream> {
    return conn.openStream({ signal: AbortSignal.timeout(OPEN_STREAM_TIMEOUT_MS) });
  }

  /**
   * One KPS-HTTP/1 GET exchange (PROTOCOL.md §3): write the request, FIN,
   * then read the response; the body ends at EOF.
   *
   * @param path Absolute request path (e.g. "/bootstrap.zip.zst").
   */
  async fetch(path: string): Promise<GatewayResponse> {
    const conn = await this.#connection();
    const stream = await this.#openStream(conn);
    const reader = stream.readable.getReader();
    const removeTeardown = this.#addTeardown(conn, () => {
      reader.cancel(new Error('kps connection closed')).catch(() => {});
      stream.close().catch(() => {});
    });

    try {
      const writer = stream.writable.getWriter();
      await writer.write(enc.encode(`GET ${path} HTTP/1.1\r\nHost: ${this.#certhash}\r\n\r\n`));
      await writer.close(); // FIN — delimits the (empty) request body

      const { status, statusText, headers, extra } = await readHead(reader);
      const chunks: Uint8Array[] = [];
      let length = 0;
      if (extra.length) {
        chunks.push(extra);
        length += extra.length;
      }
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        if (value?.length) {
          chunks.push(value);
          length += value.length;
        }
      }
      return { status, statusText, headers, body: concat(chunks, length) };
    } finally {
      removeTeardown();
      stream.close().catch(() => {});
    }
  }

  /**
   * Open a TCP tunnel to a Tor relay via CONNECT (PROTOCOL.md §4). After
   * the gateway's 200 the stream is the raw byte pipe to the target.
   *
   * @param target Relay address as "ip:port" (consensus relays only).
   */
  async connect(target: string): Promise<ArtiSocket> {
    const conn = await this.#connection();
    const stream = await this.#openStream(conn);
    const reader = stream.readable.getReader();
    const writer = stream.writable.getWriter();
    // No FIN here — the write half stays open for tunnel bytes.
    await writer.write(enc.encode(`CONNECT ${target} HTTP/1.1\r\nHost: ${target}\r\n\r\n`));

    let head: ResponseHead;
    try {
      head = await readHead(reader);
    } catch (e) {
      stream.close().catch(() => {});
      throw e;
    }
    if (head.status !== 200) {
      // Error responses carry a short text/plain diagnostic; read it to EOF.
      let text = dec.decode(head.extra);
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          text += dec.decode(value, { stream: true });
        }
      } catch { /* best-effort diagnostic */ }
      stream.close().catch(() => {});
      throw new Error(`CONNECT ${target}: ${head.status} ${text.trim() || head.statusText}`);
    }

    // The CONNECT exchange is done; hand the raw tunnel to the consumer as
    // WHATWG streams. Release the writer we used for the request line so
    // ArtiSocket.writable is available; keep `reader` (it holds any body
    // bytes that arrived with the head) and expose it, extra bytes first,
    // as a pull-based readable that only pulls from the network on demand.
    writer.releaseLock();

    const readable = new ReadableStream<Uint8Array>({
      start(controller) {
        if (head.extra.length) controller.enqueue(new Uint8Array(head.extra));
      },
      async pull(controller) {
        try {
          const { done, value } = await reader.read();
          if (done) controller.close(); // server FIN — target closed its write half
          else if (value?.length) controller.enqueue(new Uint8Array(value));
        } catch (e) {
          controller.error(e);
        }
      },
      cancel(reason) {
        reader.cancel(reason).catch(() => {});
      },
    });

    // stream.closed may reject (kps surfaces the close reason that way, e.g.
    // null); normalize both outcomes to a resolved ArtiSocketCloseInfo so this
    // never becomes an unhandled rejection.
    const closed: Promise<ArtiSocketCloseInfo> = stream.closed.then(
      (info) => ({ ok: info.ok, reason: info.ok ? undefined : (info.reason?.code ?? 'error') }),
      (err) => ({ ok: false, reason: err?.code ?? err?.message ?? 'closed' }),
    );

    // The JS kps client doesn't reliably settle streams when the connection
    // dies (kps ISSUES #4), which would hang a pending read forever. Cancel
    // the reader and close the stream on connection teardown.
    const removeTeardown = this.#addTeardown(conn, () => {
      reader.cancel(new Error('kps connection closed')).catch(() => {});
      stream.close().catch(() => {});
    });
    stream.closed.then(removeTeardown, removeTeardown);

    return new ArtiSocket({
      readable,
      writable: stream.writable,
      closed,
      closeWrite: () => stream.closeWrite(),
      close: () => { stream.close().catch(() => {}); },
    });
  }

  /** Close the underlying KPS connection (all streams/tunnels with it). */
  close(): void {
    this.#closed = true;
    const p = this.#connPromise;
    this.#connPromise = null;
    if (p) {
      p.then((conn) => conn.close()).catch(() => {});
    }
  }
}
