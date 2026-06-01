import type { Log } from './Log.js';
import type { TorStorage } from '#wasm';
import type { ArtiSocketProvider } from './socketProvider.js';

export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error';

/**
 * Options for creating a TorClient.
 *
 * In browsers, provide a gateway URL for WebSocket/WebRTC relay connections
 * and fast bootstrap. In Node.js/Deno, the gateway connects via direct TCP
 * without a URL; providing one enables fast bootstrap from that server.
 */
export type TorClientOptions = {
  /** Gateway URL. Required in browsers for relay connections; optional in Node.js/Deno (enables fast bootstrap). */
  gateway?: string;

  /**
   * Optional logger instance.
   * Note: WASM logging is global, so all TorClient instances receive all WASM
   * log events, not just their own. This is because wasm-bindgen generates a
   * single module-level instance (`let wasm;`), so all Rust global state
   * (including the tracing subscriber) is shared.
   */
  log?: Log;

  /** Optional storage for persistent state (implements TorStorage). */
  storage?: TorStorage;

  /**
   * Minimum log level for this client's log listener. Defaults to 'debug'.
   * Can be changed at runtime via `TorClient.setLogLevel()`.
   * The WASM subscriber auto-widens to the broadest level across all clients.
   */
  logLevel?: LogLevel;

  /** Optional custom socket provider. When set, overrides the default ArtiSocketProvider created from the gateway URL. */
  socketProvider?: ArtiSocketProvider;
};

export type { TorStorage } from '#wasm';

export interface FetchInit {
  method?: string;
  headers?: Record<string, string>;
  body?: string | Uint8Array | ArrayBuffer;
  signal?: AbortSignal;
}
