/**
 * tor-js-gateway client library.
 *
 * All functions accept an optional onEvent callback for instrumentation.
 * Events are plain objects with a `type` string and relevant data fields.
 */

/**
 * Download bootstrap.zip from a tor-js-gateway server.
 *
 * Events emitted:
 * - { type: "fetch-start" }
 * - { type: "fetch-progress", loaded: number, total: number | undefined }
 * - { type: "fetch-done", bytes: number }
 * - { type: "decompress-start" }  (only if manual brotli needed)
 * - { type: "decompress-progress", loaded: number, total: number | undefined }
 * - { type: "decompress-done", method: "transparent" | "wasm", bytes: number }
 *
 * The server sends an `X-Decompressed-Content-Length` header with the
 * uncompressed zip size. When the browser handles brotli transparently,
 * the stream delivers decompressed bytes, so we use this header as the
 * progress total instead of `Content-Length` (which reflects compressed size).
 *
 * When the browser does NOT handle brotli, we stream-decompress via WASM,
 * emitting both fetch-progress (compressed) and decompress-progress
 * (decompressed) events simultaneously.
 *
 * @param {string} url - The bootstrap.zip.br endpoint URL.
 * @param {function} [onEvent] - Optional event callback.
 * @returns {Promise<Uint8Array>} The decompressed zip bytes.
 */
export async function smartBootstrapDownload(url, onEvent) {
  onEvent?.({ type: "fetch-start" });

  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`fetch failed: ${res.status} ${res.statusText}`);
  }

  const contentType = res.headers.get("content-type") || "";
  const contentLen = res.headers.get("content-length");
  const decompressedLen = res.headers.get("x-decompressed-content-length");

  if (contentType.includes("application/zip")) {
    // Transparent path — browser decompressed brotli
    const total = decompressedLen
      ? parseInt(decompressedLen, 10)
      : contentLen
        ? parseInt(contentLen, 10)
        : undefined;
    const bytes = await readResponseWithProgress(res, total, onEvent, { transparent: true });
    const wireBytes = contentLen ? parseInt(contentLen, 10) : bytes.byteLength;
    onEvent?.({ type: "fetch-done", bytes: wireBytes, transparent: true });
    onEvent?.({
      type: "decompress-done",
      method: "transparent",
      bytes: bytes.byteLength,
    });
    return bytes;
  }

  // WASM streaming path
  return streamDecompress(res, contentLen, decompressedLen, onEvent);
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

  const consensus = files["bootstrap/consensus-microdesc.txt"];
  const microdescBlob = files["bootstrap/microdescs.txt"];
  const authcertBlob = files["bootstrap/authority-certs.txt"];

  if (!consensus) {
    throw new Error("missing bootstrap/consensus-microdesc.txt in zip");
  }

  const result = {
    consensus,
    microdescs: splitDocuments(microdescBlob || "", "onion-key\n"),
    authcerts: splitDocuments(authcertBlob || "", "dir-key-certificate-version "),
  };

  onEvent?.({ type: "parse-done", ...result });
  return result;
}

/**
 * Download, decompress, and parse a bootstrap archive in one call.
 *
 * Combines smartBootstrapDownload + parseBootstrapZip, forwarding all
 * events from both, plus a final { type: "done" } event.
 *
 * @param {string} url - The bootstrap.zip.br endpoint URL.
 * @param {function} [onEvent] - Optional event callback.
 * @returns {Promise<{ consensus: string, microdescs: string[], authcerts: string[] }>}
 */
export async function bootstrap(url, onEvent) {
  const zipBytes = await smartBootstrapDownload(url, onEvent);
  const result = parseBootstrapZip(zipBytes, onEvent);
  onEvent?.({ type: "done" });
  return result;
}

/**
 * Stream-download and decompress brotli via WASM, emitting both
 * fetch-progress (compressed) and decompress-progress (decompressed)
 * events simultaneously as chunks arrive.
 */
async function streamDecompress(res, contentLen, decompressedLen, onEvent) {
  const { default: init } = await import(
    "https://cdn.jsdelivr.net/npm/brotli-wasm@3.0.1/index.web.js",
  );
  const brotli = await init;

  const compressedTotal = contentLen ? parseInt(contentLen, 10) : undefined;
  const decompressedTotal = decompressedLen
    ? parseInt(decompressedLen, 10)
    : undefined;

  if (!res.body) {
    // Fallback: no ReadableStream
    const buf = await res.arrayBuffer();
    const bytes = new Uint8Array(buf);
    onEvent?.({
      type: "fetch-progress",
      loaded: buf.byteLength,
      total: compressedTotal,
    });
    onEvent?.({ type: "fetch-done", bytes: buf.byteLength });
    onEvent?.({ type: "decompress-start" });
    const decompressed = brotli.decompress(bytes);
    onEvent?.({
      type: "decompress-done",
      method: "wasm",
      bytes: decompressed.byteLength,
    });
    return decompressed;
  }

  // Try simultaneous download + decompress via DecompressStream
  const canStream = !!brotli.DecompressStream;
  const stream = canStream ? new brotli.DecompressStream() : null;
  const outChunks = [];
  const inChunks = [];
  let compressedLoaded = 0;
  let decompressStarted = false;
  let streamFailed = false;

  const reader = res.body.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;

    const chunk = new Uint8Array(value); // copy for clean buffer
    inChunks.push(chunk);
    compressedLoaded += chunk.byteLength;
    onEvent?.({
      type: "fetch-progress",
      loaded: compressedLoaded,
      total: compressedTotal,
    });

    if (stream && !streamFailed) {
      if (!decompressStarted) {
        decompressStarted = true;
        onEvent?.({ type: "decompress-start" });
      }
      try {
        let consumed = 0;
        for (;;) {
          const result = stream.decompress(chunk.subarray(consumed), 65536);
          if (result.buf.length > 0) outChunks.push(result.buf);
          consumed += result.input_offset;
          if (result.code === 1 || result.code === 2) break;
        }
        onEvent?.({
          type: "decompress-progress",
          loaded: stream.total_out(),
          total: decompressedTotal,
        });
      } catch (e) {
        console.warn("DecompressStream failed, falling back to one-shot:", e);
        streamFailed = true;
        outChunks.length = 0;
      }
    }
  }

  onEvent?.({ type: "fetch-done", bytes: compressedLoaded });

  let decompressed;
  if (stream && !streamFailed) {
    // Streaming succeeded — concatenate decompressed chunks
    const totalOut = stream.total_out();
    decompressed = new Uint8Array(totalOut);
    let off = 0;
    for (const chunk of outChunks) {
      decompressed.set(chunk, off);
      off += chunk.byteLength;
    }
  } else {
    // Fallback: concatenate compressed chunks, one-shot decompress
    if (!decompressStarted) {
      onEvent?.({ type: "decompress-start" });
    }
    const compressed = new Uint8Array(compressedLoaded);
    let cOff = 0;
    for (const chunk of inChunks) {
      compressed.set(chunk, cOff);
      cOff += chunk.byteLength;
    }
    decompressed = brotli.decompress(compressed);
  }

  onEvent?.({
    type: "decompress-done",
    method: "wasm",
    bytes: decompressed.byteLength,
  });
  return decompressed;
}

/**
 * Read a fetch Response body with progress events via ReadableStream.
 * @param {Response} res - The fetch response.
 * @param {number|undefined} total - Expected total bytes for progress display.
 * @param {function} [onEvent] - Optional event callback.
 */
async function readResponseWithProgress(res, total, onEvent, extra) {
  if (!res.body) {
    // Fallback for environments without ReadableStream
    const buf = await res.arrayBuffer();
    onEvent?.({ type: "fetch-progress", loaded: buf.byteLength, total, ...extra });
    return new Uint8Array(buf);
  }

  const reader = res.body.getReader();
  const chunks = [];
  let loaded = 0;

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    loaded += value.byteLength;
    onEvent?.({ type: "fetch-progress", loaded, total, ...extra });
  }

  const result = new Uint8Array(loaded);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return result;
}

// --- Environment detection ---

const HAS_DENO = typeof globalThis.Deno !== "undefined";
const HAS_NODE =
  typeof globalThis.process?.versions?.node !== "undefined";
const HAS_RTC = typeof globalThis.RTCPeerConnection !== "undefined";
const HAS_WS =
  typeof globalThis.WebSocket !== "undefined" ||
  HAS_DENO ||
  HAS_NODE;

function defaultStrategies() {
  const s = [];
  if (HAS_DENO || HAS_NODE) s.push("direct");
  if (HAS_RTC) s.push("webrtc");
  if (HAS_WS) s.push("websocket");
  return s;
}

// --- Unified Gateway ---

/**
 * Unified gateway client. Opens relay sockets via configurable strategies
 * (direct TCP, WebRTC data channels, WebSocket) with automatic fallback.
 *
 * Events emitted via onEvent:
 * - { type: "strategy", strategy: string, target: string }
 * - { type: "strategy-failed", strategy: string, target: string, error: string }
 * - { type: "rtc-signaling" }
 * - { type: "rtc-connected" }
 * - { type: "rtc-disconnected" }
 * - { type: "connected", strategy: string, target: string }
 *
 * @example
 * const gw = new Gateway('https://gateway.example.com');
 * const sock = await gw.connect('198.51.100.1:9001');
 * sock.send(new Uint8Array([0x00, 0x07]));
 * sock.onmessage = (data) => console.log(data);
 * sock.close();
 * gw.close();
 */
export class Gateway {
  #url;
  #strategies;
  #onEvent;
  #rtcPc = null; // RTCPeerConnection | null
  #rtcAlive = false;
  #signalChannel = null;
  // All tracked data channels: { dc, sock?, reject? }
  // Before open: reject is set. After open: sock is set.
  // Matched by dc.id (sctp_id) when available, or by dc.label as fallback.
  #tracked = [];

  /**
   * @param {string} url - Gateway origin (e.g. "https://example.com").
   * @param {object} [options]
   * @param {string[]} [options.strategies] - Ordered list of strategies to try.
   *   Valid values: "direct", "webrtc", "websocket". Defaults based on environment.
   * @param {function} [options.onEvent] - Optional instrumentation callback.
   */
  constructor(url, options = {}) {
    this.#url = url.replace(/\/+$/, "");
    this.#strategies = options.strategies || defaultStrategies();
    this.#onEvent = options.onEvent || null;
  }

  /**
   * Open a relay socket to the given target.
   * Tries each configured strategy in order until one succeeds.
   *
   * @param {string} target - Relay address as "ip:port".
   * @returns {Promise<RelaySocket>}
   */
  async connect(target) {
    const errors = [];

    for (const strategy of this.#strategies) {
      this.#onEvent?.({ type: "strategy", strategy, target });
      try {
        let sock;
        switch (strategy) {
          case "direct":
            sock = await this.#connectDirect(target);
            break;
          case "webrtc":
            sock = await this.#connectWebRTC(target);
            break;
          case "websocket":
            sock = await this.#connectWebSocket(target);
            break;
          default:
            throw new Error(`unknown strategy: ${strategy}`);
        }
        sock.strategy = strategy;
        this.#onEvent?.({ type: "connected", strategy, target });
        return sock;
      } catch (e) {
        this.#onEvent?.({
          type: "strategy-failed",
          strategy,
          target,
          error: e.message,
        });
        errors.push(`${strategy}: ${e.message}`);
      }
    }

    throw new Error(
      `all strategies failed for ${target}: ${errors.join("; ")}`,
    );
  }

  /** Close WebRTC peer connection and release resources. */
  close() {
    if (this.#rtcPc) {
      this.#rtcPc.close();
      this.#rtcPc = null;
      this.#rtcAlive = false;
    }
  }

  // --- Strategy: direct TCP (Node.js / Deno) ---

  async #connectDirect(target) {
    const [host, portStr] = target.split(":");
    const port = parseInt(portStr, 10);

    if (HAS_DENO) {
      const conn = await Deno.connect({ hostname: host, port });
      return RelaySocket.fromDenoConn(conn);
    }

    if (HAS_NODE) {
      const net = await import("node:net");
      const socket = net.createConnection({ host, port });
      await new Promise((resolve, reject) => {
        socket.once("connect", resolve);
        socket.once("error", reject);
      });
      return RelaySocket.fromNodeSocket(socket);
    }

    throw new Error("direct TCP not available in this environment");
  }

  // --- Strategy: WebRTC data channel ---

  async #connectWebRTC(target) {
    if (!HAS_RTC) throw new Error("RTCPeerConnection not available");

    // Create or reuse peer connection.
    if (!this.#rtcAlive) {
      if (this.#rtcPc) this.#rtcPc.close();
      await this.#setupRtcPeerConnection();
    }

    const dc = this.#rtcPc.createDataChannel(target);
    dc.binaryType = "arraybuffer";

    const entry = { dc, sock: null, reject: null };
    this.#tracked.push(entry);

    // Race: channel opens vs server rejects via _signal.
    await new Promise((resolve, reject) => {
      entry.reject = reject;
      dc.onopen = resolve;
      dc.onerror = (e) => {
        this.#removeTracked(entry);
        reject(new Error(`data channel error: ${e.error?.message || e}`));
      };
    });

    // Channel is open (dc.id now available), but rejection/close may still arrive.
    entry.reject = null;
    const sock = RelaySocket.fromDataChannel(dc);
    entry.sock = sock;
    dc.onclose = () => {
      this.#removeTracked(entry);
      sock._notifyClose();
    };
    return sock;
  }

  async #setupRtcPeerConnection() {
    const pc = new RTCPeerConnection();

    // Signal channel for control messages (hello, ping/pong, rejections).
    const signal = pc.createDataChannel("_signal");
    signal.onmessage = (ev) => this.#handleSignalMessage(ev.data);

    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);

    // Wait for ICE gathering to complete.
    await new Promise((resolve) => {
      if (pc.iceGatheringState === "complete") return resolve();
      pc.addEventListener("icegatheringstatechange", () => {
        if (pc.iceGatheringState === "complete") resolve();
      });
    });

    this.#onEvent?.({ type: "rtc-signaling" });

    const res = await fetch(`${this.#url}/rtc/connect`, {
      method: "POST",
      body: JSON.stringify(pc.localDescription),
    });

    if (!res.ok) {
      pc.close();
      throw new Error(
        `rtc signaling failed: ${res.status} ${await res.text()}`,
      );
    }

    const answer = await res.json();
    await pc.setRemoteDescription(answer);

    // Wait for connection.
    await new Promise((resolve, reject) => {
      if (pc.connectionState === "connected") return resolve();
      pc.addEventListener("connectionstatechange", () => {
        if (pc.connectionState === "connected") resolve();
        if (pc.connectionState === "failed")
          reject(new Error("WebRTC connection failed"));
      });
    });

    this.#rtcPc = pc;
    this.#rtcAlive = true;
    this.#signalChannel = signal;
    this.#onEvent?.({ type: "rtc-connected" });

    pc.addEventListener("connectionstatechange", () => {
      if (
        pc.connectionState === "disconnected" ||
        pc.connectionState === "closed" ||
        pc.connectionState === "failed"
      ) {
        this.#rtcAlive = false;
        this.#signalChannel = null;
        this.#onEvent?.({ type: "rtc-disconnected" });
      }
    });
  }

  #findTracked(sctpId, label) {
    // Match by sctp_id first (exact), fall back to label (first match).
    return this.#tracked.find(e => e.dc.id != null && e.dc.id === sctpId)
      || this.#tracked.find(e => e.dc.label === label);
  }

  #removeTracked(entry) {
    const i = this.#tracked.indexOf(entry);
    if (i !== -1) this.#tracked.splice(i, 1);
  }

  #handleSignalMessage(data) {
    try {
      const msg = JSON.parse(data);
      switch (msg.type) {
        case "hello":
          this.#onEvent?.({ type: "rtc-hello", server: msg.server, ipv6: msg.ipv6 });
          break;
        case "pong":
          this.#onEvent?.({ type: "rtc-pong", ts: msg.ts });
          break;
        case "rejected": {
          const entry = this.#findTracked(msg.sctp_id, msg.channel);
          if (entry) {
            this.#removeTracked(entry);
            if (entry.reject) {
              entry.reject(new Error(`rejected: ${msg.reason}`));
            } else if (entry.sock) {
              entry.sock._error = msg.reason;
              entry.sock.close();
              entry.sock._notifyClose();
            }
          }
          this.#onEvent?.({ type: "rtc-rejected", channel: msg.channel, reason: msg.reason });
          break;
        }
        case "closed": {
          const entry = this.#findTracked(msg.sctp_id, msg.channel);
          if (entry) {
            this.#removeTracked(entry);
            if (entry.sock) {
              entry.sock.close();
              entry.sock._notifyClose();
            }
          }
          this.#onEvent?.({ type: "rtc-closed", channel: msg.channel });
          break;
        }
        case "error":
          console.warn(`[tor-js-gateway] ${msg.message}`);
          this.#onEvent?.({ type: "rtc-error", message: msg.message });
          break;
      }
    } catch {}
  }

  /**
   * Send a ping on the signal channel. The server will respond with a pong
   * containing the same `ts` value (emitted as an rtc-pong event).
   */
  ping() {
    if (this.#signalChannel?.readyState === "open") {
      this.#signalChannel.send(JSON.stringify({ type: "ping", ts: Date.now() }));
    }
  }

  // --- Strategy: WebSocket ---

  async #connectWebSocket(target) {
    const wsUrl = `${this.#url.replace(/^http/, "ws")}/socket/${target}`;
    const ws = new WebSocket(wsUrl);
    ws.binaryType = "arraybuffer";

    await new Promise((resolve, reject) => {
      ws.onopen = resolve;
      ws.onerror = () => reject(new Error("websocket connection failed"));
    });

    return RelaySocket.fromWebSocket(ws);
  }
}

// --- Unified RelaySocket ---

/**
 * A relay socket providing a uniform interface regardless of transport.
 *
 * Assign `onmessage` and `onclose` handlers after creation.
 * Call `send(data)` with Uint8Array and `close()` when done.
 */
export class RelaySocket {
  #send;
  #close;
  #readyState;
  #closed = false;
  #onclose = null;
  onmessage = null;
  /** Which strategy produced this socket (set by Gateway.connect). */
  strategy = null;

  constructor(send, close, readyState) {
    this.#send = send;
    this.#close = close;
    this.#readyState = readyState;
  }

  /** Setter that fires immediately if close already happened. */
  set onclose(fn) {
    this.#onclose = fn;
    if (this.#closed && fn) queueMicrotask(() => fn());
  }
  get onclose() { return this.#onclose; }

  /** @internal — called by transport wrappers. */
  _notifyClose() {
    if (this.#closed) return;
    this.#closed = true;
    this.#onclose?.();
  }

  send(data) {
    this.#send(data);
  }

  close() {
    this.#close();
  }

  get readyState() {
    return this.#readyState();
  }

  /** Wrap a WebRTC data channel. */
  static fromDataChannel(dc) {
    const sock = new RelaySocket(
      (data) => dc.send(data),
      () => dc.close(),
      () => dc.readyState,
    );
    dc.onmessage = (ev) => sock.onmessage?.(new Uint8Array(ev.data));
    dc.onclose = () => sock._notifyClose();
    return sock;
  }

  /** Wrap a WebSocket. */
  static fromWebSocket(ws) {
    const sock = new RelaySocket(
      (data) => ws.send(data),
      () => ws.close(),
      () => {
        switch (ws.readyState) {
          case WebSocket.CONNECTING: return "connecting";
          case WebSocket.OPEN: return "open";
          case WebSocket.CLOSING: return "closing";
          case WebSocket.CLOSED: return "closed";
          default: return "closed";
        }
      },
    );
    ws.onmessage = (ev) => sock.onmessage?.(new Uint8Array(ev.data));
    ws.onclose = () => sock._notifyClose();
    return sock;
  }

  /** Wrap a Deno TCP connection. */
  static fromDenoConn(conn) {
    const sock = new RelaySocket(
      (data) => {
        const writer = conn.writable.getWriter();
        writer.write(data).then(() => writer.releaseLock());
      },
      () => conn.close(),
      () => "open",
    );
    // Read loop
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

  /** Wrap a Node.js net.Socket. */
  static fromNodeSocket(socket) {
    const sock = new RelaySocket(
      (data) => socket.write(data),
      () => socket.destroy(),
      () => (socket.destroyed ? "closed" : "open"),
    );
    socket.on("data", (buf) => sock.onmessage?.(new Uint8Array(buf)));
    socket.on("close", () => sock._notifyClose());
    socket.on("error", () => {});
    return sock;
  }
}

// --- Backward-compatible lower-level exports ---

/**
 * Create a WebRTC peer connection to the gateway (lower-level API).
 * Prefer `new Gateway(url).connect(target)` for most use cases.
 */
export async function connectRtc(gatewayUrl, onEvent) {
  const gw = new Gateway(gatewayUrl, {
    strategies: ["webrtc"],
    onEvent,
  });
  // Force peer connection setup by connecting to a dummy and extracting state.
  // Instead, expose the underlying GatewayConnection for compat.
  const pc = new RTCPeerConnection();
  const signalChannel = pc.createDataChannel("_signal");

  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);

  await new Promise((resolve) => {
    if (pc.iceGatheringState === "complete") return resolve();
    pc.addEventListener("icegatheringstatechange", () => {
      if (pc.iceGatheringState === "complete") resolve();
    });
  });

  onEvent?.({ type: "rtc-signaling" });

  const url = gatewayUrl.replace(/\/+$/, "");
  const res = await fetch(`${url}/rtc/connect`, {
    method: "POST",
    body: JSON.stringify(pc.localDescription),
  });

  if (!res.ok) {
    pc.close();
    throw new Error(`rtc signaling failed: ${res.status} ${await res.text()}`);
  }

  await pc.setRemoteDescription(await res.json());

  await new Promise((resolve, reject) => {
    if (pc.connectionState === "connected") return resolve();
    pc.addEventListener("connectionstatechange", () => {
      if (pc.connectionState === "connected") resolve();
      if (pc.connectionState === "failed")
        reject(new Error("WebRTC connection failed"));
    });
  });

  onEvent?.({ type: "rtc-connected" });
  return new GatewayConnection(pc, onEvent);
}

/**
 * Lower-level WebRTC connection wrapper. Prefer Gateway class.
 */
export class GatewayConnection {
  #pc;
  #onEvent;

  constructor(pc, onEvent) {
    this.#pc = pc;
    this.#onEvent = onEvent;
    pc.addEventListener("connectionstatechange", () => {
      if (
        pc.connectionState === "disconnected" ||
        pc.connectionState === "closed"
      ) {
        onEvent?.({ type: "rtc-disconnected" });
      }
    });
  }

  async openSocket(target) {
    const dc = this.#pc.createDataChannel(target);
    dc.binaryType = "arraybuffer";
    await new Promise((resolve, reject) => {
      dc.onopen = resolve;
      dc.onerror = (e) =>
        reject(new Error(`data channel error: ${e.error?.message || e}`));
    });
    return RelaySocket.fromDataChannel(dc);
  }

  close() {
    this.#pc.close();
  }

  get connected() {
    return this.#pc.connectionState === "connected";
  }
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
