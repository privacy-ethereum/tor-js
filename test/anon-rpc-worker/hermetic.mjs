// Hermetic test (runs in CI): fully offline — no gateway, no Tor. Builds the
// worker, loads it in the PUBLISHED @anon-rpc/browser-harness, and asserts the
// worker's boot contract: with NO gateway in config, resolveGateways throws and
// the worker calls signalFailed, so the host's `.ready` REJECTS (never hangs).
//
// The real Tor fetch is deliberately excluded here — it needs live Tor egress
// and can't be made hermetic (see run-e2e.mjs for the manual real-Tor path).
//
// Run: npm run test:hermetic

import { setupHarness, check, cleanup, fail, guard, WORKER_ADDR } from "./harness.mjs";

async function main() {
  const { page, ethCallMap } = await setupHarness();

  console.log("instantiating worker with no gateway in config (offline)...");
  const result = await page.evaluate(async (cfg) => {
    const provider = {
      request: async ({ method, params }) => {
        if (method !== "eth_call") throw new Error(`unexpected method ${method}`);
        const ret = cfg.ethCallMap[params[0].to]?.[params[0].data.slice(0, 10)];
        if (!ret) throw new Error(`no mock eth_call for ${params[0].to} ${params[0].data.slice(0, 10)}`);
        return ret;
      },
    };
    // Empty config → no gateway → the worker must reject `.ready` (signalFailed),
    // and it must do so promptly rather than hang.
    let noGatewayError = "";
    const bad = new window.AnonRpcWorker({ address: cfg.addr, config: {}, preExisting: { rpcProvider: provider } });
    try {
      await Promise.race([
        bad.ready,
        new Promise((_, rej) => setTimeout(() => rej(new Error("HANG: .ready neither resolved nor rejected")), 15000)),
      ]);
    } catch (e) {
      noGatewayError = String(e?.message ?? e);
    }
    bad.close();
    return { noGatewayError };
  }, { addr: WORKER_ADDR, ethCallMap });

  check(
    "no-gateway config rejects .ready via signalFailed (offline)",
    /no gateway configured/.test(result.noGatewayError),
    `err=${result.noGatewayError}`,
  );

  console.log("\n✅ hermetic test passed (worker builds, loads, and fails cleanly with no gateway)");
  cleanup();
  process.exit(0);
}

guard(60_000);
main().catch((e) => fail(e?.stack || String(e)));
