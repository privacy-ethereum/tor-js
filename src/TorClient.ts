import {
  ensureWasmInitialized,
  WasmTorClient,
  WasmTorClientOptions,
  addLogListener,
  setListenerLevel,
} from './wasm.js';
import type { TorClientOptions, FetchInit, LogLevel } from './types.js';
import { Log } from './Log.js';
import { createAutoStorage } from './storage/index.js';
import { ArtiSocketProvider } from './socketProvider.js';

function isBrowser(): boolean {
  const g = globalThis as any;
  const hasNode = typeof g.process?.versions?.node !== 'undefined';
  const hasDeno = typeof g.Deno !== 'undefined';
  return !hasNode && !hasDeno && typeof g.window !== 'undefined';
}

export class TorClient {
  private log: Log;
  private clientPromise: Promise<WasmTorClient>;
  private removeLogListener: (() => void) | null = null;
  private wasmCallback: ((level: string, target: string, message: string) => void) | null = null;
  private closed = false;
  private readyPromise: Promise<void> | null = null;
  private socketProvider: ArtiSocketProvider | null = null;

  constructor(options: TorClientOptions = {}) {
    const hasGateway = Array.isArray(options.gateway)
      ? options.gateway.length > 0
      : !!options.gateway;
    if (isBrowser() && !hasGateway && !options.socketProvider) {
      throw new Error(
        'TorClient: in the browser, you must configure a gateway (KPS address "ip:port:certhash") ' +
        'because browsers can\'t open regular TCP sockets.',
      );
    }
    this.log = (options.log ?? new Log({ rawLog: () => {} }));
    this.clientPromise = this.bootstrap(options);
  }

  private async bootstrap(options: TorClientOptions): Promise<WasmTorClient> {
    await ensureWasmInitialized();

    // Register log listener with per-client level filtering.
    // The WASM subscriber auto-widens to the broadest level across all listeners.
    this.wasmCallback = this.log._makeWasmCallback();
    this.removeLogListener = addLogListener(this.wasmCallback, options.logLevel);

    // ArtiSocketProvider handles relay connections. In browsers it needs a
    // gateway KPS address ("ip:port:certhash") for tunneling; in Node.js/Deno
    // it connects via direct TCP.
    this.socketProvider = options.socketProvider ?? new ArtiSocketProvider({ gateway: options.gateway });
    const sp = this.socketProvider;

    let wasmOptions = new WasmTorClientOptions(
      (addr: string) => sp.connect(addr),
    );

    const storage = options.storage ?? createAutoStorage();
    wasmOptions = wasmOptions.withStorage(storage);

    // Auto-attempt fast bootstrap from gateway — only when one is configured.
    // The archive is zstd-compressed; the WASM side decompresses it.
    const gw = sp.gateway;
    if (gw) {
      wasmOptions = wasmOptions.withFastBootstrap(async (): Promise<Uint8Array> => {
        this.log.info('Fast bootstrap: fetching bootstrap.zip.zst...');
        const res = await gw.fetch('/bootstrap.zip.zst');
        if (res.status !== 200) {
          throw new Error(`Fast bootstrap fetch failed: ${res.status} ${res.statusText}`);
        }
        this.log.info(`Fast bootstrap: received ${res.body.byteLength} bytes (compressed)`);
        return res.body;
      });
    }

    // Create client (WASM constructor returns a Promise)
    this.log.info('Bootstrapping...');
    const client = await WasmTorClient.create(wasmOptions);
    this.log.info('Bootstrap complete');
    return client;
  }

  /**
   * Make an HTTP fetch request through Tor.
   * Returns a standard browser Response object.
   */
  async fetch(url: string, init?: FetchInit): Promise<Response> {
    if (this.closed) throw new Error('TorClient is closed');
    const client = await this.clientPromise;
    await this.ready();
    this.log.info(`Fetching ${url}`);
    return client.fetch(url, init);
  }

  /**
   * Wait for the Tor client to be ready for traffic
   * (guard connected, usable consensus, and sufficient microdescs).
   *
   * Parallel callers share the same underlying promise — a single WS
   * connection failure rejects all waiters. The cached promise is cleared
   * on settle so the next call creates a fresh attempt.
   */
  async ready(): Promise<void> {
    if (this.closed) throw new Error('TorClient is closed');
    if (this.readyPromise) return this.readyPromise;

    const p = (async () => {
      const startTime = Date.now();
      this.log.info('Waiting for client');
      const client = await this.clientPromise;
      this.log.info('Waiting for client to be ready');
      await client.ready();
      this.log.info(`Client ready in ${Date.now() - startTime}ms`);
    })();

    this.readyPromise = p;
    p.finally(() => { this.readyPromise = null; });
    return p;
  }

  /**
   * Change the log level for this client's listener.
   * Also re-syncs the global WASM filter to the broadest level across all clients.
   */
  setLogLevel(level: LogLevel): void {
    if (this.wasmCallback) {
      setListenerLevel(this.wasmCallback, level);
    }
  }

  /**
   * Close the TorClient and release resources.
   */
  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.removeLogListener?.();
    this.removeLogListener = null;
    this.wasmCallback = null;
    this.socketProvider?.close();
    this.socketProvider = null;
    this.clientPromise.then(client => client.close()).catch(() => {});
  }

  [Symbol.dispose](): void {
    this.close();
  }
}
