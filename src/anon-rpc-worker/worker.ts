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
} from "./spec-types.js";
import {
  TorClient,
  ArtiSocketProvider,
  type DialFn,
} from "../entryPoints/wasm-base64/index.js";
import {
  bootstrapBackoff,
  errMsg,
  makeTorStorage,
  resolveGateways,
  sleep,
  toFetchInit,
} from "./helpers.js";

declare const anonRpcWorker: AnonRpcWorkerApi;

// --- Transport: bridge the host's KPS capability into tor-js's `dial` seam ---
// anon-rpc's KpsConn/KpsStream are structurally the subset of @kpstreams/core's
// Connection/Stream that tor-js's KpsGateway uses (openStream, {readable,
// writable, closed:{ok,reason}, closeWrite, close}), so a cast suffices — and
// tor-js's built-in @kpstreams dialer is never loaded.
const dial: DialFn = async (addr) => {
  const conn = await anonRpcWorker.kps.dial(addr);
  return conn as unknown as Awaited<ReturnType<DialFn>>;
};

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
