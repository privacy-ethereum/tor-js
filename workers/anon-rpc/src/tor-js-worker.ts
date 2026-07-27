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
  ByteBody,
  HeaderList,
  StorageApi,
} from "./spec-types.js";
import {
  TorClient,
  ArtiSocketProvider,
  type DialFn,
  type FetchInit,
  type TorStorage,
} from "../../../src/entryPoints/wasm-base64/index.js";

declare const anonRpcWorker: AnonRpcWorkerApi;

// Fallback gateway when the host supplies none via `config`. A hash-pinned
// worker's baked gateway list is part of what an auditor reviews.
const DEFAULT_GATEWAYS = [
  "170.64.236.147:12298:uEiBHwUMNRTetrbqScahm81Di57Xv2OphNrx-CurJGOq3ww",
];

const enc = new TextEncoder();
const dec = new TextDecoder();

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

function resolveGateways(config: unknown): string[] {
  const asList = (v: unknown): string[] | null =>
    typeof v === "string" && v ? [v]
    : Array.isArray(v) && v.length && v.every((x) => typeof x === "string") ? (v as string[])
    : null;
  return (
    asList(config) ??
    asList((config as { gateways?: unknown } | null)?.gateways) ??
    DEFAULT_GATEWAYS
  );
}

(async () => {
  const { log, kps, storage, config } = anonRpcWorker;
  void kps; // used via `dial` above
  const gateways = resolveGateways(config);
  log.info(`tor-js worker: bootstrapping over ${gateways.length} gateway(s)`);

  const client = new TorClient({
    socketProvider: new ArtiSocketProvider({ gateway: gateways, dial }),
    storage: makeTorStorage(storage),
  });

  // Wait for Tor to be usable before declaring readiness, so the host's
  // `ready` means "warm" and the first fetch doesn't eat full bootstrap.
  try {
    await client.ready();
  } catch (e) {
    log.error("tor-js worker: bootstrap failed:", errMsg(e));
    return; // never signalReady — the host sees the worker fail to come up
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
  if (init.body !== undefined) {
    // tor-js's FetchInit takes bytes, not a stream — buffer a streaming body.
    out.body = init.body instanceof ReadableStream ? await readAll(init.body) : init.body;
  }
  if (init.signal) out.signal = init.signal;
  // Note: tor-js's FetchInit has no `redirect`; anon-rpc's is not forwarded.
  return out;
}

async function readAll(body: ByteBody): Promise<Uint8Array> {
  if (body instanceof Uint8Array) return body;
  const chunks: Uint8Array[] = [];
  let total = 0;
  const reader = body.getReader();
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    if (value) {
      chunks.push(value);
      total += value.byteLength;
    }
  }
  const out = new Uint8Array(total);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.byteLength;
  }
  return out;
}

function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
