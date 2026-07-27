// Manual end-to-end test: the tor-js anon-rpc worker making a REAL Tor request
// via a gateway, through the published @anon-rpc/browser-harness. Hits live Tor,
// so it is intentionally NOT part of CI (which stays hermetic). Run locally:
//
//   npm run test:e2e                 # uses the demo gateway (may be down)
//   GATEWAY=<ip:port:certhash> npm run test:e2e
//
// The offline boot/contract checks live in hermetic.mjs (that one runs in CI).

import { setupHarness, check, cleanup, fail, guard, WORKER_ADDR } from "./harness.mjs";

const LIVE_GATEWAY =
  process.env.GATEWAY ||
  "170.64.236.147:12298:uEiBHwUMNRTetrbqScahm81Di57Xv2OphNrx-CurJGOq3ww";

async function main() {
  const { page, ethCallMap } = await setupHarness();

  console.log("instantiating worker, bootstrapping Tor via the gateway (may take ~30s)...");
  const result = await page.evaluate(async (cfg) => {
    const provider = {
      request: async ({ method, params }) => {
        if (method !== "eth_call") throw new Error(`unexpected method ${method}`);
        const ret = cfg.ethCallMap[params[0].to]?.[params[0].data.slice(0, 10)];
        if (!ret) throw new Error(`no mock eth_call for ${params[0].to} ${params[0].data.slice(0, 10)}`);
        return ret;
      },
    };
    const w = new window.AnonRpcWorker({
      address: cfg.addr,
      config: { gateways: cfg.gateways },
      preExisting: { rpcProvider: provider },
    });
    const t0 = performance.now();
    await w.ready;
    const readyMs = Math.round(performance.now() - t0);
    const r = await w.fetch("https://check.torproject.org/api/ip");
    const body = await r.json();
    w.close();
    return { status: r.status, body, readyMs };
  }, { addr: WORKER_ADDR, gateways: [LIVE_GATEWAY], ethCallMap });

  console.log(`ready in ${result.readyMs}ms; HTTP ${result.status}; body: ${JSON.stringify(result.body)}`);
  check("fetch through Tor returned HTTP 200", result.status === 200, `status=${result.status}`);
  check("check.torproject.org reports IsTor:true", result.body?.IsTor === true, `body=${JSON.stringify(result.body)}`);

  console.log("\n✅ real-Tor e2e passed: worker made a Tor request via the gateway");
  cleanup();
  process.exit(0);
}

guard(150_000);
main().catch((e) => fail(e?.stack || String(e)));
