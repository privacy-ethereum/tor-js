// Pure helpers for the anon-rpc worker: config parsing, retry pacing, and the
// adapters between anon-rpc's capability shapes and tor-js's.
//
// Split out of worker.ts because that module is an entry point — it runs an IIFE
// against the global `anonRpcWorker` capability on import, so nothing in it can
// be imported for testing. Everything here is free of that global.

import type { AnonRequestInit, StorageApi } from "./spec-types.js";
import type { FetchInit, TorStorage } from "../entryPoints/wasm-base64/index.js";

const enc = new TextEncoder();
const dec = new TextDecoder();

// Exponential backoff for bootstrap retries. Bootstrap is retried indefinitely
// (the Tor way): a down/unreachable gateway is transient, so we keep trying
// rather than permanently failing readiness — but back off, capped and
// jittered, so a persistently-down gateway isn't hammered.
export const BOOTSTRAP_RETRY_BASE_MS = 1_000;
export const BOOTSTRAP_RETRY_MAX_MS = 60_000;

export const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

// min(base·2^(attempt-1), max), then 50–100% jitter to avoid synchronized
// retries across many workers.
export function bootstrapBackoff(attempt: number): number {
  const exp = Math.min(BOOTSTRAP_RETRY_MAX_MS, BOOTSTRAP_RETRY_BASE_MS * 2 ** (attempt - 1));
  return Math.round(exp * (0.5 + Math.random() * 0.5));
}

// --- Storage: map tor-js's TorStorage onto the host's storage capability -----
// TorStorage is string-valued with prefix queries + a writer lock; the host's
// StorageApi is byte-valued with `list`. The worker is the sole writer of its
// (address-scoped) store, so the lock is a no-op.
export function makeTorStorage(s: StorageApi): TorStorage {
  return {
    async get(key) {
      const b = await s.get(key);
      return b ? dec.decode(b) : null;
    },
    async set(key, value) {
      await s.set(key, enc.encode(value));
    },
    async delete(key) {
      await s.delete(key);
    },
    async keys(prefix) {
      const out: string[] = [];
      for await (const k of s.list({ prefix })) out.push(k);
      return out;
    },
    async getAll(prefix) {
      const out: [string, string][] = [];
      for await (const k of s.list({ prefix })) {
        const b = await s.get(k);
        if (b) out.push([k, dec.decode(b)]);
      }
      return out;
    },
    async tryLock() {
      return true;
    },
    async unlock() {},
  };
}

// Gateways are MANDATORY and come from the host's WorkerInit.config — either a
// KPS address string, an array of them, or `{ gateways: [...] }`. There is no
// default: a gateway sees all of this worker's relay traffic, so the choice
// must be the deploying app's, not baked into shared worker code.
export function resolveGateways(config: unknown): string[] {
  const asList = (v: unknown): string[] | null =>
    typeof v === "string" && v ? [v]
    : Array.isArray(v) && v.length && v.every((x) => typeof x === "string") ? (v as string[])
    : null;
  const gateways = asList(config) ?? asList((config as { gateways?: unknown } | null)?.gateways);
  if (!gateways) {
    throw new Error(
      "tor-js worker: no gateway configured. Supply gateway KPS address(es) via the host's " +
      'WorkerInit.config — e.g. config: { gateways: ["<ip>:<port>:<certhash>", ...] } or ' +
      'config: "<ip>:<port>:<certhash>". There is no default gateway.',
    );
  }
  return gateways;
}

export async function toFetchInit(init?: AnonRequestInit): Promise<FetchInit | undefined> {
  if (!init) return undefined;
  const out: FetchInit = {};
  if (init.method) out.method = init.method;
  if (init.headers) {
    const h: Record<string, string> = {};
    for (const [k, v] of init.headers) h[k] = v;
    out.headers = h;
  }
  // tor-js's fetch accepts bytes or a ReadableStream (streamed as chunked), so
  // forward the body as-is — a streaming request body is never buffered.
  if (init.body !== undefined) out.body = init.body;
  if (init.signal) out.signal = init.signal;
  // Note: tor-js's FetchInit has no `redirect`; anon-rpc's is not forwarded.
  return out;
}

export function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
