// Shared harness for the anon-rpc worker tests. Builds the worker, serves it +
// the PUBLISHED @anon-rpc/browser-harness host + a page, mocks the worker's
// WorkerSpecifier (workerHash()/workerResolvers()), launches headless Chromium
// and waits for AnonRpcWorker. The mock EIP-1193 provider is created in-page by
// each test (it can't cross into page.evaluate as a function).
//
// Used by:
//   run-e2e.mjs  — real Tor fetch via a gateway (manual; not in CI)
//   hermetic.mjs — offline signalFailed check (CI; no gateway, no Tor)

import { readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { keccak_256 } from "@noble/hashes/sha3";
import { build as esbuild } from "esbuild";
import { chromium } from "playwright";
import { buildAnonRpcWorker } from "../../build.mjs";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));

export const WORKER_ADDR = "0xabc0000000000000000000000000000000000001";

export const cleanups = [];
export const cleanup = () => cleanups.splice(0).reverse().forEach((fn) => { try { fn(); } catch {} });
process.on("exit", cleanup);

export function fail(msg) { console.error(`\n❌ ${msg}`); cleanup(); process.exit(1); }
export function check(label, ok, detail) {
  if (!ok) fail(`${label}${detail ? `\n   ${detail}` : ""}`);
  console.log(`  ✓ ${label}`);
}
export function guard(ms) {
  const t = setTimeout(() => fail(`timed out after ${ms / 1000}s`), ms);
  t.unref?.();
  return t;
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

/**
 * Build the worker, stand up the page/host/worker HTTP server + mock specifier,
 * launch Chromium and wait for `window.AnonRpcWorker`.
 * @returns {Promise<{ page: import("playwright").Page, ethCallMap: object, workerAddr: string }>}
 */
export async function setupHarness() {
  console.log("building tor-js anon-rpc worker...");
  const workerFile = resolve(here, ".tmp-worker.js");
  cleanups.push(() => rm(workerFile, { force: true }).catch(() => {}));
  await buildAnonRpcWorker({ outfile: workerFile, ensureWasm: true });
  const workerBytes = new Uint8Array(await readFile(workerFile));
  const workerHash = "0x" + toHex(keccak_256(workerBytes));
  console.log(`worker: ${(workerBytes.length / 1024 / 1024).toFixed(2)} MB, keccak256=${workerHash.slice(0, 18)}…`);

  // Bundle the published host for the page (deps resolved from node_modules).
  const hostBundled = await esbuild({
    entryPoints: [require.resolve("@anon-rpc/browser-harness")],
    bundle: true, format: "esm", platform: "browser", target: "es2022", write: false, logLevel: "warning",
  });
  const hostBundle = hostBundled.outputFiles[0].contents;

  const pageHtml = `<!doctype html><meta charset="utf-8"><body>
<script type="module">
  import { AnonRpcWorker } from "/host.js";
  window.AnonRpcWorker = AnonRpcWorker;
</script>`;
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
  const origin = `http://127.0.0.1:${server.address().port}`;
  console.log(`http server: ${origin}`);

  const ethCallMap = {
    [WORKER_ADDR]: {
      [selector("workerHash()")]: workerHash,
      [selector("workerResolvers()")]: "0x" + toHex(encodeStringArray([`${origin}/worker.js`])),
    },
  };

  const browser = await chromium.launch({ args: ["--no-sandbox"] });
  cleanups.push(() => browser.close());
  const page = await browser.newPage();
  page.on("console", (m) => console.log(`  [page:${m.type()}] ${m.text()}`));
  page.on("pageerror", (e) => console.log(`  [page:error] ${e.message}`));
  await page.goto(`${origin}/`);
  await page.waitForFunction(() => "AnonRpcWorker" in window, null, { timeout: 10000 });

  return { page, ethCallMap, workerAddr: WORKER_ADDR };
}
