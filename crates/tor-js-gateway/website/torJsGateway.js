/**
 * tor-js-gateway client library (KPS edition).
 *
 * The gateway speaks KPS-HTTP/1 (see ../PROTOCOL.md): strict HTTP/1.1
 * syntax over KPS streams, one exchange per stream, bodies delimited by
 * stream FIN. Browsers dial the gateway's `<ip>:<port>:<certhash>` address
 * over WebRTC via @kpstreams/webrtc-client — no URL, no CA, no DNS.
 *
 * All functions accept an optional onEvent callback for instrumentation.
 * Events are plain objects with a `type` string and relevant data fields.
 */

import { dial } from '@kpstreams/webrtc-client';
import { parseAddress } from '@kpstreams/core';
import { decompress as zstdDecompress, Decompress as ZstdDecompress } from 'fzstd';

const enc = new TextEncoder();
const dec = new TextDecoder();

// --- KPS-HTTP/1 plumbing ---

/**
 * Read a response head (status line + headers) from a stream reader.
 * Returns { status, statusText, headers, extra } where `extra` is any body
 * bytes that arrived in the same chunks as the head.
 */
async function readHead(reader) {
  let buf = new Uint8Array(0);
  for (;;) {
    const sep = findHeadEnd(buf);
    if (sep !== -1) {
      const head = dec.decode(buf.subarray(0, sep));
      const lines = head.split('\r\n');
      const m = lines[0].match(/^HTTP\/1\.1 (\d{3})\s*(.*)$/);
      if (!m) throw new Error(`malformed status line: ${lines[0]}`);
      const headers = {};
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

function findHeadEnd(buf) {
  for (let i = 0; i + 3 < buf.length; i++) {
    if (buf[i] === 13 && buf[i + 1] === 10 && buf[i + 2] === 13 && buf[i + 3] === 10) {
      return i;
    }
  }
  return -1;
}

function concat(chunks, length) {
  const out = new Uint8Array(length);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return out;
}

// --- Gateway ---

/**
 * A connection to one tor-js-gateway, addressed by its KPS address
 * (`<ip>:<port>:<certhash>`). The underlying KPS connection is dialed
 * lazily and reused; every request/tunnel is its own stream on it.
 *
 * Events emitted via onEvent:
 * - { type: "dialing", address }
 * - { type: "connected", address }
 * - { type: "disconnected" }
 * - { type: "tunnel-open", target }
 *
 * @example
 * const gw = new Gateway('198.51.100.7:42298:uEiAxk...9Qw');
 * const meta = await gw.metadata();
 * const sock = await gw.connect('198.51.100.1:9001');
 * sock.send(new Uint8Array([0x16, 0x03, 0x01]));
 * sock.onmessage = (data) => { /* Uint8Array *\/ };
 * gw.close();
 */
export class Gateway {
  #address;
  #certhash;
  #onEvent;
  #connPromise = null;

  /**
   * @param {string} address - KPS address (`ip:port:certhash`).
   * @param {object} [options]
   * @param {function} [options.onEvent] - Optional instrumentation callback.
   */
  constructor(address, options = {}) {
    this.#address = address.trim();
    // Validates the address shape early and yields the certhash, which is
    // the Host value the protocol recommends (PROTOCOL.md §3.2).
    this.#certhash = parseAddress(this.#address).certhash;
    this.#onEvent = options.onEvent || null;
  }

  get address() {
    return this.#address;
  }

  /** Dial (or reuse) the KPS connection. */
  async #connection() {
    if (!this.#connPromise) {
      this.#onEvent?.({ type: 'dialing', address: this.#address });
      const p = dial(this.#address).then(
        (conn) => {
          this.#onEvent?.({ type: 'connected', address: this.#address });
          conn.closed.then(() => {
            if (this.#connPromise === p) this.#connPromise = null;
            this.#onEvent?.({ type: 'disconnected' });
          });
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

  /**
   * One KPS-HTTP/1 GET exchange with a streaming body (PROTOCOL.md §3):
   * write the request, FIN, then read the response; the body ends at EOF.
   *
   * @param {string} path - Absolute request path (e.g. "/metadata.json").
   * @returns {Promise<{status, statusText, headers, body: AsyncGenerator<Uint8Array>}>}
   */
  async fetchStream(path) {
    const conn = await this.#connection();
    const stream = await conn.openStream();
    const writer = stream.writable.getWriter();
    await writer.write(enc.encode(`GET ${path} HTTP/1.1\r\nHost: ${this.#certhash}\r\n\r\n`));
    await writer.close(); // FIN — delimits the (empty) request body

    const reader = stream.readable.getReader();
    const { status, statusText, headers, extra } = await readHead(reader);
    async function* body() {
      if (extra.length) yield extra;
      for (;;) {
        const { done, value } = await reader.read();
        if (done) return;
        if (value?.length) yield value;
      }
    }
    return { status, statusText, headers, body: body() };
  }

  /**
   * Like fetchStream, but buffers the whole body.
   * @returns {Promise<{status, statusText, headers, body: Uint8Array}>}
   */
  async fetch(path) {
    const res = await this.fetchStream(path);
    const chunks = [];
    let length = 0;
    for await (const c of res.body) {
      chunks.push(c);
      length += c.length;
    }
    return { ...res, body: concat(chunks, length) };
  }

  /** GET /metadata.json — the gateway's capability document. */
  async metadata() {
    const res = await this.fetch('/metadata.json');
    if (res.status !== 200) throw new Error(`metadata: HTTP ${res.status}`);
    return JSON.parse(dec.decode(res.body));
  }

  /** GET /relay/random — a random consensus relay address ("ip:port"). */
  async randomRelay() {
    const res = await this.fetch('/relay/random');
    if (res.status !== 200) throw new Error(`relay/random: HTTP ${res.status}`);
    return dec.decode(res.body).trim();
  }

  /**
   * Open a TCP tunnel to a Tor relay via CONNECT (PROTOCOL.md §4). After
   * the gateway's 200 the stream is the raw byte pipe to the target.
   *
   * @param {string} target - Relay address as "ip:port" (consensus relays only).
   * @returns {Promise<RelaySocket>}
   */
  async connect(target) {
    const conn = await this.#connection();
    const stream = await conn.openStream();
    const writer = stream.writable.getWriter();
    // No FIN here — the write half stays open for tunnel bytes.
    await writer.write(enc.encode(`CONNECT ${target} HTTP/1.1\r\nHost: ${target}\r\n\r\n`));

    const reader = stream.readable.getReader();
    const head = await readHead(reader);
    if (head.status !== 200) {
      // Error responses carry a short text/plain diagnostic; read it to EOF.
      let text = dec.decode(head.extra);
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          text += dec.decode(value, { stream: true });
        }
      } catch {}
      stream.close().catch(() => {});
      throw new Error(`CONNECT ${target}: ${head.status} ${text.trim() || head.statusText}`);
    }

    this.#onEvent?.({ type: 'tunnel-open', target });
    return RelaySocket.fromKpsStream(stream, reader, writer, head.extra);
  }

  /** Close the underlying KPS connection (all streams/tunnels with it). */
  async close() {
    const p = this.#connPromise;
    this.#connPromise = null;
    if (p) {
      const conn = await p.catch(() => null);
      await conn?.close();
    }
  }
}

// --- RelaySocket ---

/**
 * A relay tunnel socket. Assign `onmessage` and `onclose` handlers after
 * creation; call `send(data)` with Uint8Array and `close()` when done.
 * `closeWrite()` half-closes (the relay sees TCP FIN) while reads continue.
 */
export class RelaySocket {
  #stream;
  #writer;
  #closed = false;
  #onclose = null;
  onmessage = null;
  /** Set to a reason string when the tunnel ended abnormally. */
  _error = null;
  /** Transport marker (kept for API compatibility). */
  strategy = 'kps';

  /** Setter that fires immediately if close already happened. */
  set onclose(fn) {
    this.#onclose = fn;
    if (this.#closed && fn) queueMicrotask(() => fn());
  }
  get onclose() {
    return this.#onclose;
  }

  /** @internal */
  _notifyClose() {
    if (this.#closed) return;
    this.#closed = true;
    this.#onclose?.();
  }

  send(data) {
    this.#writer.write(data).catch(() => {});
  }

  /** Gracefully finish the write half (target observes TCP FIN). */
  closeWrite() {
    this.#writer.close().catch(() => {});
  }

  close() {
    this.#stream.close().catch(() => {});
    this._notifyClose();
  }

  get readyState() {
    return this.#closed ? 'closed' : 'open';
  }

  /** @internal Wrap a KPS stream that has completed the CONNECT exchange. */
  static fromKpsStream(stream, reader, writer, extra) {
    const sock = new RelaySocket();
    sock.#stream = stream;
    sock.#writer = writer;

    (async () => {
      // Let the caller attach onmessage before the first delivery.
      await new Promise((r) => setTimeout(r, 0));
      try {
        if (extra?.length) sock.onmessage?.(new Uint8Array(extra));
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break; // server FIN — target closed its write half
          if (value?.length) sock.onmessage?.(new Uint8Array(value));
        }
      } catch (e) {
        sock._error = sock._error || String(e?.message || e);
      }
      sock._notifyClose();
    })();

    stream.closed.then((info) => {
      if (!info.ok && info.reason) {
        sock._error = sock._error || `stream ${info.reason.code || 'error'}`;
      }
      sock._notifyClose();
    });

    return sock;
  }
}

// --- Bootstrap ---

/**
 * Download bootstrap.zip.zst from a gateway and decompress it.
 *
 * There is no transparent decompression on raw KPS streams: the gateway
 * always serves raw zstd bytes and the client decompresses them here with
 * fzstd (pure JS, streaming as chunks arrive).
 *
 * Events emitted:
 * - { type: "fetch-start" }
 * - { type: "fetch-progress", loaded, total }        (compressed bytes)
 * - { type: "fetch-done", bytes }
 * - { type: "decompress-start" }
 * - { type: "decompress-progress", loaded, total }   (decompressed bytes)
 * - { type: "decompress-done", method: "zstd", bytes }
 *
 * `total` for fetch-progress comes from `Content-Length` and for
 * decompress-progress from `X-Decompressed-Content-Length` (both advisory
 * per the protocol, used only for progress display).
 *
 * @param {Gateway|string} gateway - A Gateway or a KPS address.
 * @param {function} [onEvent] - Optional event callback.
 * @returns {Promise<Uint8Array>} The decompressed zip bytes.
 */
export async function smartBootstrapDownload(gateway, onEvent) {
  const gw = typeof gateway === 'string' ? new Gateway(gateway) : gateway;
  onEvent?.({ type: 'fetch-start' });

  const res = await gw.fetchStream('/bootstrap.zip.zst');
  if (res.status !== 200) {
    throw new Error(`bootstrap fetch failed: HTTP ${res.status} ${res.statusText}`);
  }

  const compressedTotal = parseInt(res.headers['content-length'], 10) || undefined;
  const decompressedTotal =
    parseInt(res.headers['x-decompressed-content-length'], 10) || undefined;

  // Stream: fzstd emits decompressed chunks as compressed ones are pushed.
  const outChunks = [];
  let outLen = 0;
  const dec = new ZstdDecompress((chunk) => {
    outChunks.push(chunk);
    outLen += chunk.length;
  });

  const inChunks = [];
  let compressedLoaded = 0;
  let decompressStarted = false;
  let streamFailed = false;

  for await (const value of res.body) {
    const chunk = new Uint8Array(value);
    inChunks.push(chunk);
    compressedLoaded += chunk.byteLength;
    onEvent?.({ type: 'fetch-progress', loaded: compressedLoaded, total: compressedTotal });

    if (!streamFailed) {
      if (!decompressStarted) {
        decompressStarted = true;
        onEvent?.({ type: 'decompress-start' });
      }
      try {
        dec.push(chunk, false);
        onEvent?.({ type: 'decompress-progress', loaded: outLen, total: decompressedTotal });
      } catch (e) {
        console.warn('zstd stream failed, falling back to one-shot:', e);
        streamFailed = true;
        outChunks.length = 0;
        outLen = 0;
      }
    }
  }

  onEvent?.({ type: 'fetch-done', bytes: compressedLoaded });

  let decompressed;
  if (!streamFailed) {
    try {
      dec.push(new Uint8Array(0), true); // finalize the frame
      decompressed = concat(outChunks, outLen);
    } catch (e) {
      console.warn('zstd finalize failed, falling back to one-shot:', e);
      streamFailed = true;
    }
  }
  if (streamFailed) {
    decompressed = zstdDecompress(concat(inChunks, compressedLoaded));
  }

  onEvent?.({ type: 'decompress-done', method: 'zstd', bytes: decompressed.byteLength });
  return decompressed;
}

/**
 * Parse a bootstrap zip archive into its constituent documents.
 *
 * The zip uses Stored compression (no deflate), so we parse the
 * local file headers directly without a decompression library.
 *
 * Events emitted:
 * - { type: "parse-done", consensus: string, microdescs: string[], authcerts: string[] }
 *
 * @param {Uint8Array} zip - The raw zip bytes.
 * @param {function} [onEvent] - Optional event callback.
 * @returns {{ consensus: string, microdescs: string[], authcerts: string[] }}
 */
export function parseBootstrapZip(zip, onEvent) {
  const view = new DataView(zip.buffer, zip.byteOffset, zip.byteLength);
  const decoder = new TextDecoder();
  const files = {};

  let offset = 0;
  while (offset + 30 <= zip.byteLength) {
    const sig = view.getUint32(offset, true);
    if (sig !== 0x04034b50) break;

    const method = view.getUint16(offset + 8, true);
    if (method !== 0) {
      throw new Error(
        `unsupported compression method ${method}, expected Stored (0)`,
      );
    }

    const compressedSize = view.getUint32(offset + 18, true);
    const nameLen = view.getUint16(offset + 26, true);
    const extraLen = view.getUint16(offset + 28, true);
    const name = decoder.decode(
      zip.subarray(offset + 30, offset + 30 + nameLen),
    );
    const dataStart = offset + 30 + nameLen + extraLen;
    const data = zip.subarray(dataStart, dataStart + compressedSize);

    files[name] = decoder.decode(data);
    offset = dataStart + compressedSize;
  }

  const consensus = files['bootstrap/consensus-microdesc.txt'];
  const microdescBlob = files['bootstrap/microdescs.txt'];
  const authcertBlob = files['bootstrap/authority-certs.txt'];

  if (!consensus) {
    throw new Error('missing bootstrap/consensus-microdesc.txt in zip');
  }

  const result = {
    consensus,
    microdescs: splitDocuments(microdescBlob || '', 'onion-key\n'),
    authcerts: splitDocuments(authcertBlob || '', 'dir-key-certificate-version '),
  };

  onEvent?.({ type: 'parse-done', ...result });
  return result;
}

/**
 * Download, decompress, and parse a bootstrap archive in one call.
 *
 * Combines smartBootstrapDownload + parseBootstrapZip, forwarding all
 * events from both, plus a final { type: "done" } event.
 *
 * @param {Gateway|string} gateway - A Gateway or a KPS address.
 * @param {function} [onEvent] - Optional event callback.
 * @returns {Promise<{ consensus: string, microdescs: string[], authcerts: string[] }>}
 */
export async function bootstrap(gateway, onEvent) {
  const zipBytes = await smartBootstrapDownload(gateway, onEvent);
  const result = parseBootstrapZip(zipBytes, onEvent);
  onEvent?.({ type: 'done' });
  return result;
}

function splitDocuments(blob, marker) {
  if (!blob) return [];
  const docs = [];
  let pos = 0;
  while (pos < blob.length) {
    let next = blob.indexOf(`\n${marker}`, pos);
    if (next === -1) {
      docs.push(blob.slice(pos));
      break;
    }
    docs.push(blob.slice(pos, next + 1));
    pos = next + 1;
  }
  return docs.filter((d) => d.trim().length > 0);
}
