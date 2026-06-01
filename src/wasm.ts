import './polyfills.js';
import initWasm, {
  init as wasmInit,
  setLogCallback as wasmSetLogCallback,
  setLogLevel as wasmSetLogLevel,
  TorClient as WasmTorClient,
  TorClientOptions as WasmTorClientOptions,
} from '#wasm';

export { WasmTorClient, WasmTorClientOptions };

// ============================================================================
// Log listener management
// ============================================================================

type WasmLogCallback = (level: string, target: string, message: string) => void;

const LEVEL_ORDER = ['trace', 'debug', 'info', 'warn', 'error'] as const;

function levelIndex(level: string): number {
  const idx = LEVEL_ORDER.indexOf(level as typeof LEVEL_ORDER[number]);
  return idx === -1 ? 1 : idx; // default to debug
}

interface LogListener {
  callback: WasmLogCallback;
  levelIdx: number;
}

const logListeners = new Map<WasmLogCallback, LogListener>();

/** Recompute the broadest level across all listeners and update the WASM filter. */
function syncWasmLogLevel(): void {
  let broadestIdx = LEVEL_ORDER.length - 1; // start at narrowest (error)
  for (const listener of logListeners.values()) {
    if (listener.levelIdx < broadestIdx) {
      broadestIdx = listener.levelIdx;
    }
  }
  wasmSetLogLevel(LEVEL_ORDER[broadestIdx]);
}

/**
 * Register a log callback at a given level. The WASM subscriber is
 * automatically widened to the broadest level across all listeners.
 * Each listener only receives events at or above its own level.
 * Returns an unregister function.
 */
export function addLogListener(cb: WasmLogCallback, level: string = 'debug'): () => void {
  logListeners.set(cb, { callback: cb, levelIdx: levelIndex(level) });
  syncWasmLogLevel();
  return () => {
    logListeners.delete(cb);
    if (logListeners.size > 0) {
      syncWasmLogLevel();
    }
  };
}

/**
 * Update the level for an existing listener and re-sync the WASM filter.
 */
export function setListenerLevel(cb: WasmLogCallback, level: string): void {
  const listener = logListeners.get(cb);
  if (listener) {
    listener.levelIdx = levelIndex(level);
    syncWasmLogLevel();
  }
}

// ============================================================================
// WASM initialization
// ============================================================================

type WasmSourceProvider = () => Promise<BufferSource | Uint8Array>;

let initPromise: Promise<void> | null = null;
let customWasmUrl: string | URL | undefined;
let wasmSourceProvider: WasmSourceProvider | undefined;

/**
 * Override the WASM binary URL. Must be called before any TorClient is created.
 */
export function setWasmUrl(url: string | URL): void {
  if (initPromise) {
    throw new Error('setWasmUrl() must be called before any TorClient is created');
  }
  customWasmUrl = url;
}

/**
 * Set a custom WASM source provider. Called by entry points to configure
 * how the WASM binary is loaded (e.g. base64 decode, CDN fetch).
 * Must be called before any TorClient is created.
 */
export function setWasmSourceProvider(provider: WasmSourceProvider): void {
  if (initPromise) {
    throw new Error('setWasmSourceProvider() must be called before any TorClient is created');
  }
  wasmSourceProvider = provider;
}

/**
 * Ensures the WASM module is loaded and initialized. Idempotent.
 */
export async function ensureWasmInitialized(): Promise<void> {
  if (initPromise) return initPromise;
  initPromise = doInit();
  return initPromise;
}

async function doInit(): Promise<void> {
  if (customWasmUrl) {
    await initWasm({ module_or_path: customWasmUrl });
  } else if (wasmSourceProvider) {
    await initWasm({ module_or_path: await wasmSourceProvider() });
  } else {
    throw new Error(
      'No WASM source configured. Import from a specific entry point '
      + '(tor-js/wasm-base64, tor-js/wasm-cdn, or tor-js/wasm-file) '
      + 'or call setWasmUrl() before creating a TorClient.',
    );
  }
  wasmInit();

  // Install a single fan-out callback that dispatches to matching listeners
  wasmSetLogCallback((level: string, target: string, message: string) => {
    const lvl = levelIndex(level);
    for (const listener of logListeners.values()) {
      if (lvl >= listener.levelIdx) {
        listener.callback(level, target, message);
      }
    }
  });
}