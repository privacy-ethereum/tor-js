// An anon-rpc worker that services fetch calls over Tor, using tor-js.
//
// Untrusted, hash-pinned code (SPEC §3.2/§4): its only platform is the global
// `anonRpcWorker` capability object. It reaches the network solely through the
// host-granted KPS transport (`anonRpcWorker.kps`) — tor-js is configured with
// an injected `dial` so it never opens KPS itself (it couldn't anyway: WebRTC
// isn't available in a worker). The WASM is embedded (wasm-base64 entry), so
// the whole bundle is covered by one keccak hash and pulls in no @kpstreams
// client code.

import type {
  AnonRpcWorkerApi,
  AnonFetchResponse,
  AnonRequestInit,
  HeaderList,
  StorageApi,
} from "./spec-types.js";
import {
  TorClient,
  ArtiSocketProvider,
  type DialFn,
  type FetchInit,
  type TorStorage,
} from "../entryPoints/wasm-base64/index.js";

declare const anonRpcWorker: AnonRpcWorkerApi;

const enc = new TextEncoder();
const dec = new TextDecoder();

// Exponential backoff for bootstrap retries. Bootstrap is retried indefinitely
// (the Tor way): a down/unreachable gateway is transient, so we keep trying
// rather than permanently failing readiness — but back off, capped and
// jittered, so a persistently-down gateway isn't hammered.
const BOOTSTRAP_RETRY_BASE_MS = 1_000;
const BOOTSTRAP_RETRY_MAX_MS = 60_000;

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

// min(base·2^(attempt-1), max), then 50–100% jitter to avoid synchronized
// retries across many workers.
function bootstrapBackoff(attempt: number): number {
  const exp = Math.min(BOOTSTRAP_RETRY_MAX_MS, BOOTSTRAP_RETRY_BASE_MS * 2 ** (attempt - 1));
  return Math.round(exp * (0.5 + Math.random() * 0.5));
}

// --- Transport: bridge the host's KPS capability into tor-js's `dial` seam ---
// anon-rpc's KpsConn/KpsStream are structurally the subset of @kpstreams/core's
// Connection/Stream that tor-js's KpsGateway uses (openStream, {readable,
// writable, closed:{ok,reason}, closeWrite, close}), so a cast suffices — and
// tor-js's built-in @kpstreams dialer is never loaded.
const dial: DialFn = async (addr) => {
  const conn = await anonRpcWorker.kps.dial(addr);
  return conn as unknown as Awaited<ReturnType<DialFn>>;
};

// --- Storage: map tor-js's TorStorage onto the host's storage capability -----
// TorStorage is string-valued with prefix queries + a writer lock; the host's
// StorageApi is byte-valued with `list`. The worker is the sole writer of its
// (address-scoped) store, so the lock is a no-op.
function makeTorStorage(s: StorageApi): TorStorage {
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
function resolveGateways(config: unknown): string[] {
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

(async () => {
  const { log, storage } = anonRpcWorker;

  // Construct the client. A missing/invalid gateway config (or a malformed
  // address) is a PERMANENT error, so reject the host's `.ready` via
  // signalFailed (§7) — retrying can't help. kps transport is reached via the
  // module-level `dial` above.
  let client!: TorClient;
  try {
    const gateways = resolveGateways(anonRpcWorker.config);
    log.info(`tor-js worker: using ${gateways.length} gateway(s)`);
    client = new TorClient({
      socketProvider: new ArtiSocketProvider({ gateway: gateways, dial }),
      storage: makeTorStorage(storage),
    });
  } catch (e) {
    anonRpcWorker.signalFailed({ message: errMsg(e) });
    return;
  }

  // Bootstrap the Tor way: retry indefinitely. A down/unreachable gateway is a
  // transient condition — keep trying rather than permanently failing. We only
  // signalReady once Tor is usable, so the host's `.ready` stays pending (never
  // rejected) while bootstrap is still in progress, just like vanilla tor-js
  // where each fetch re-attempts readiness.
  for (let attempt = 1; ; attempt++) {
    try {
      await client.ready();
      break;
    } catch (e) {
      const delay = bootstrapBackoff(attempt);
      log.warn(`tor-js worker: bootstrap attempt ${attempt} failed; retrying in ${(delay / 1000).toFixed(1)}s:`, errMsg(e));
      await sleep(delay);
    }
  }
  log.info("tor-js worker: Tor ready");
  anonRpcWorker.signalReady();

  for (;;) {
    let call;
    try {
      call = await anonRpcWorker.acceptCall();
    } catch (e) {
      log.error("acceptCall failed:", errMsg(e));
      client.close();
      return;
    }
    if (call.kind !== "fetch") continue; // ignore unknown kinds (§8)
    call.respond(handle(client, call.url, call.requestInit));
  }
})();

async function handle(
  client: TorClient,
  url: string,
  init?: AnonRequestInit,
): Promise<AnonFetchResponse> {
  const resp = await client.fetch(url, await toFetchInit(init));
  const headers: HeaderList = [];
  resp.headers.forEach((v, k) => headers.push([k, v]));
  return {
    status: resp.status,
    headers,
    // Stream the Tor response body straight through (transferred to the host),
    // preserving backpressure end to end.
    body: resp.body ?? new Uint8Array(0),
    url: resp.url,
  };
}

async function toFetchInit(init?: AnonRequestInit): Promise<FetchInit | undefined> {
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

function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
