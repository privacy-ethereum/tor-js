// End-to-end test: the tor-js anon-rpc worker, run through the PUBLISHED
// @anon-rpc/browser-harness, making a real Tor request via the live gateway.
//
// Pipeline:
//   1. build the worker bundle (dist artifact) and keccak-hash it
//   2. mock a WorkerSpecifier (workerHash()/workerResolvers()) via an in-page
//      EIP-1193 provider; serve the bundle over HTTP as its resolver
//   3. bundle the published host for the page; drive headless Chromium:
//      instantiate AnonRpcWorker with config.gateways = [live gateway], await
//      ready (Tor bootstrap over the host's KPS/WebRTC bridge), then
//      w.fetch("https://check.torproject.org/api/ip") and assert { IsTor: true }
//
// Run: npm run test:e2e   (hits the live gateway + real Tor)

import { readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { keccak_256 } from "@noble/hashes/sha3";
import { build as esbuild } from "esbuild";
import { chromium } from "playwright";
import { buildAnonRpcWorker } from "../build.mjs";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const torjsRoot = resolve(here, "../../..");

const LIVE_GATEWAY =
  process.env.GATEWAY ||
  "170.64.236.147:12298:uEiBHwUMNRTetrbqScahm81Di57Xv2OphNrx-CurJGOq3ww";
const WORKER_ADDR = "0xabc0000000000000000000000000000000000001";

const cleanups = [];
const cleanup = () => cleanups.splice(0).reverse().forEach((fn) => { try { fn(); } catch {} });
process.on("exit", cleanup);
function fail(msg) { console.error(`\n❌ ${msg}`); cleanup(); process.exit(1); }
function check(label, ok, detail) {
  if (!ok) fail(`${label}${detail ? `\n   ${detail}` : ""}`);
  console.log(`  ✓ ${label}`);
}

/* --- minimal ABI encoders matching @anon-rpc host/specifier.ts decoders --- */
const enc = new TextEncoder();
const toHex = (b) => [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
const selector = (sig) => "0x" + toHex(keccak_256(enc.encode(sig))).slice(0, 8);
const concat = (arrs) => { const o = new Uint8Array(arrs.reduce((s, a) => s + a.length, 0)); let n = 0; for (const a of arrs) { o.set(a, n); n += a.length; } return o; };
const pad32 = (b) => { const o = new Uint8Array(Math.ceil(b.length / 32) * 32 || 32); o.set(b); return o; };
const word = (n) => { const o = new Uint8Array(32); for (let i = 31; i >= 0 && n > 0; i--) { o[i] = n & 0xff; n = Math.floor(n / 256); } return o; };
function encodeStringArray(strings) {
  const items = strings.map((s) => enc.encode(s));
  const heads = []; const tails = []; let off = items.length * 32;
  for (const item of items) { heads.push(word(off)); const tail = concat([word(item.length), pad32(item)]); tails.push(tail); off += tail.length; }
  return concat([word(0x20), concat([word(items.length), ...heads, ...tails])]);
}

async function main() {
  // 1. Build the worker bundle + hash it.
  console.log("building tor-js anon-rpc worker...");
  const workerFile = resolve(here, ".tmp-worker.js");
  cleanups.push(() => rm(workerFile, { force: true }).catch(() => {}));
  await buildAnonRpcWorker({ root: torjsRoot, outfile: workerFile, ensureWasm: true });
  const workerBytes = new Uint8Array(await readFile(workerFile));
  const workerHash = "0x" + toHex(keccak_256(workerBytes));
  console.log(`worker: ${(workerBytes.length / 1024 / 1024).toFixed(2)} MB, keccak256=${workerHash.slice(0, 18)}…`);

  // 2. Bundle the published host for the page.
  const hostBundled = await esbuild({
    entryPoints: [require.resolve("@anon-rpc/browser-harness")],
    bundle: true, format: "esm", platform: "browser", target: "es2022", write: false, logLevel: "warning",
  });
  const hostBundle = hostBundled.outputFiles[0].contents;

  // 3. HTTP server: page, /host.js, /worker.js
  const pageHtml = `<!doctype html><meta charset="utf-8"><body>
<script type="module">
  import { AnonRpcWorker } from "/host.js";
  window.AnonRpcWorker = AnonRpcWorker;
</script>`;
  let origin = "";
  const server = createServer((req, res) => {
    const url = req.url.split("?")[0];
    const send = (status, type, body) =>
      res.writeHead(status, { "content-type": type, "access-control-allow-origin": "*" }).end(body);
    if (url === "/") return send(200, "text/html", pageHtml);
    if (url === "/host.js") return send(200, "text/javascript", Buffer.from(hostBundle));
    if (url === "/worker.js") return send(200, "text/javascript", Buffer.from(workerBytes));
    send(404, "text/plain", "not found");
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  cleanups.push(() => server.close());
  origin = `http://127.0.0.1:${server.address().port}`;
  console.log(`http server: ${origin}`);

  const ethCallMap = {
    [WORKER_ADDR]: {
      [selector("workerHash()")]: workerHash,
      [selector("workerResolvers()")]: "0x" + toHex(encodeStringArray([`${origin}/worker.js`])),
    },
  };

  // 4. Drive Chromium.
  const browser = await chromium.launch({ args: ["--no-sandbox"] });
  cleanups.push(() => browser.close());
  const page = await browser.newPage();
  page.on("console", (m) => console.log(`  [page:${m.type()}] ${m.text()}`));
  page.on("pageerror", (e) => console.log(`  [page:error] ${e.message}`));
  await page.goto(`${origin}/`);
  await page.waitForFunction(() => "AnonRpcWorker" in window, null, { timeout: 10000 });

  console.log("instantiating worker, bootstrapping Tor via the live gateway (may take ~30s)...");
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

  console.log("\n✅ e2e passed: tor-js anon-rpc worker made a real Tor request via the live gateway");
  cleanup();
  process.exit(0);
}

// Overall guard so a dead gateway / stuck bootstrap fails loudly rather than hangs.
const GUARD_MS = 150_000;
const guard = setTimeout(() => fail(`timed out after ${GUARD_MS / 1000}s`), GUARD_MS);
guard.unref?.();
main().catch((e) => fail(e?.stack || String(e)));
