/**
 * Socket provider for connecting to Tor relays via direct TCP, WebSocket,
 * or WebRTC.
 *
 * ArtiSocketProvider auto-detects available strategies based on environment:
 * - Node.js/Deno: tries direct TCP first, then WebSocket/WebRTC if a gateway URL is set
 * - Browsers: tries WebRTC first (if available), then WebSocket (requires gateway URL)
 *
 * Each `connect(target)` call returns an {@link ArtiSocket} — a uniform
 * bidirectional byte pipe regardless of transport.
 */

// ---------------------------------------------------------------------------
// Environment detection
// ---------------------------------------------------------------------------

const HAS_DENO = typeof (globalThis as any).Deno !== 'undefined';
const HAS_NODE = typeof (globalThis as any).process?.versions?.node !== 'undefined';
const HAS_RTC = typeof (globalThis as any).RTCPeerConnection !== 'undefined';
const HAS_WS =
  typeof (globalThis as any).WebSocket !== 'undefined' || HAS_DENO || HAS_NODE;

function defaultStrategies(hasUrl: boolean): string[] {
  const s: string[] = [];
  if (HAS_DENO || HAS_NODE) s.push('direct');
  if (hasUrl && HAS_RTC) s.push('webrtc');
  if (hasUrl && HAS_WS) s.push('websocket');
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

  /** Wrap a browser WebSocket (already open). */
  static fromWebSocket(ws: WebSocket): ArtiSocket {
    const sock = new ArtiSocket(
      (data) => ws.send(data),
      () => ws.close(),
    );
    ws.onmessage = (ev) => sock.onmessage?.(new Uint8Array(ev.data));
    ws.onclose = () => sock._notifyClose();
    return sock;
  }

  /** Wrap a WebRTC data channel (already open). */
  static fromDataChannel(dc: RTCDataChannel): ArtiSocket {
    const sock = new ArtiSocket(
      (data) => dc.send(data),
      () => dc.close(),
    );
    dc.onmessage = (ev) => sock.onmessage?.(new Uint8Array(ev.data));
    dc.onclose = () => sock._notifyClose();
    return sock;
  }

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
   * Gateway URL (e.g., `"https://tor-js-gateway.example.com"`).
   * Required in browsers for WebRTC/WebSocket relay connections.
   * Optional in Node.js/Deno (enables fast bootstrap when provided).
   */
  gateway?: string;

  /**
   * Ordered list of strategies to try: `"direct"`, `"webrtc"`, `"websocket"`.
   * Defaults based on environment and whether a gateway URL is provided.
   */
  strategies?: string[];
}

interface TrackedEntry {
  dc: RTCDataChannel;
  sock: ArtiSocket | null;
  reject: ((err: Error) => void) | null;
}

/**
 * Opens sockets to Tor relays via configurable strategies (direct TCP,
 * WebRTC data channels, WebSocket) with automatic fallback.
 *
 * The gateway URL is optional — without it, only the `direct` strategy is
 * available (Node.js/Deno native TCP). With a gateway URL, WebRTC and
 * WebSocket strategies become available for browser environments.
 */
export class ArtiSocketProvider {
  #url: string | null;
  #strategies: string[];

  // WebRTC state (lazily created, reused across connect() calls)
  #rtcPc: RTCPeerConnection | null = null;
  #rtcAlive = false;
  #signalChannel: RTCDataChannel | null = null;
  // Tracked data channels: before open has reject, after open has sock.
  #tracked: TrackedEntry[] = [];

  constructor(options: ArtiSocketProviderOptions = {}) {
    this.#url = options.gateway ? options.gateway.replace(/\/+$/, '') : null;
    this.#strategies = options.strategies ?? defaultStrategies(!!this.#url);
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
          case 'webrtc':
            return await this.#connectWebRTC(target);
          case 'websocket':
            return await this.#connectWebSocket(target);
          default:
            throw new Error(`unknown strategy: ${strategy}`);
        }
      } catch (e: any) {
        errors.push(`${strategy}: ${e.message}`);
      }
    }

    throw new Error(`all strategies failed for ${target}: ${errors.join('; ')}`);
  }

  /** Close WebRTC peer connection and release resources. */
  close(): void {
    if (this.#rtcPc) {
      this.#rtcPc.close();
      this.#rtcPc = null;
      this.#rtcAlive = false;
      this.#signalChannel = null;
    }
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

  // -- WebSocket strategy ---------------------------------------------------

  async #connectWebSocket(target: string): Promise<ArtiSocket> {
    if (!this.#url) throw new Error('websocket strategy requires a gateway URL');
    const wsUrl = `${this.#url.replace(/^http/, 'ws')}/socket/${target}`;
    const ws = new WebSocket(wsUrl);
    ws.binaryType = 'arraybuffer';

    await new Promise<void>((resolve, reject) => {
      ws.onopen = () => resolve();
      ws.onerror = () => reject(new Error('websocket connection failed'));
    });

    return ArtiSocket.fromWebSocket(ws);
  }

  // -- WebRTC strategy ------------------------------------------------------

  async #connectWebRTC(target: string): Promise<ArtiSocket> {
    if (!this.#url) throw new Error('webrtc strategy requires a gateway URL');
    if (typeof RTCPeerConnection === 'undefined') {
      throw new Error('RTCPeerConnection not available');
    }

    // Create or reuse peer connection
    if (!this.#rtcAlive) {
      if (this.#rtcPc) this.#rtcPc.close();
      await this.#setupRtcPeerConnection();
    }

    const dc = this.#rtcPc!.createDataChannel(target);
    dc.binaryType = 'arraybuffer';

    const entry = { dc, sock: null as ArtiSocket | null, reject: null as ((err: Error) => void) | null };
    this.#tracked.push(entry);

    // Race: channel opens vs server rejects via _signal
    await new Promise<void>((resolve, reject) => {
      entry.reject = reject;
      dc.onopen = () => resolve();
      dc.onerror = (e: any) => {
        this.#removeTracked(entry);
        reject(new Error(`data channel error: ${e.error?.message || e}`));
      };
    });

    // Channel is open — dc.id now available
    entry.reject = null;
    const sock = ArtiSocket.fromDataChannel(dc);
    entry.sock = sock;
    dc.onclose = () => {
      this.#removeTracked(entry);
      sock._notifyClose();
    };
    return sock;
  }

  async #setupRtcPeerConnection(): Promise<void> {
    const pc = new RTCPeerConnection();

    // Signal channel for control messages (hello, ping/pong, rejections)
    const signal = pc.createDataChannel('_signal');
    signal.onmessage = (ev) => this.#handleSignalMessage(ev.data);

    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);

    // Wait for ICE gathering to complete
    await new Promise<void>((resolve) => {
      if (pc.iceGatheringState === 'complete') return resolve();
      pc.addEventListener('icegatheringstatechange', () => {
        if (pc.iceGatheringState === 'complete') resolve();
      });
    });

    const res = await fetch(`${this.#url}/rtc/connect`, {
      method: 'POST',
      body: JSON.stringify(pc.localDescription),
    });

    if (!res.ok) {
      pc.close();
      throw new Error(`rtc signaling failed: ${res.status} ${await res.text()}`);
    }

    const answer = await res.json();
    await pc.setRemoteDescription(answer);

    // Wait for connection
    await new Promise<void>((resolve, reject) => {
      if (pc.connectionState === 'connected') return resolve();
      pc.addEventListener('connectionstatechange', () => {
        if (pc.connectionState === 'connected') resolve();
        if (pc.connectionState === 'failed') reject(new Error('WebRTC connection failed'));
      });
    });

    this.#rtcPc = pc;
    this.#rtcAlive = true;
    this.#signalChannel = signal;

    pc.addEventListener('connectionstatechange', () => {
      const s = pc.connectionState;
      if (s === 'disconnected' || s === 'closed' || s === 'failed') {
        this.#rtcAlive = false;
        this.#signalChannel = null;
      }
    });
  }

  #findTracked(sctpId: number | null, label: string) {
    return this.#tracked.find(e => e.dc.id != null && e.dc.id === sctpId)
      ?? this.#tracked.find(e => e.dc.label === label);
  }

  #removeTracked(entry: TrackedEntry): void {
    const i = this.#tracked.indexOf(entry);
    if (i !== -1) this.#tracked.splice(i, 1);
  }

  #handleSignalMessage(data: string): void {
    try {
      const msg = JSON.parse(data);
      switch (msg.type) {
        case 'rejected': {
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
          break;
        }
        case 'closed': {
          const entry = this.#findTracked(msg.sctp_id, msg.channel);
          if (entry) {
            this.#removeTracked(entry);
            if (entry.sock) {
              entry.sock.close();
              entry.sock._notifyClose();
            }
          }
          break;
        }
      }
    } catch { /* ignore malformed signal messages */ }
  }
}
