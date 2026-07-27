// Bundles the tor-js anon-rpc worker to a single standalone IIFE with the WASM
// embedded — its bytes are the §4 artifact (keccak256 = the pinned workerHash).
//
// Builds tor-js FROM SOURCE with @kpstreams/* external, so the KPS client code
// (the host provides KPS via the capability) never enters the bundle.

import { build } from "esbuild";
import { execSync } from "node:child_process";
import { readFileSync, writeFileSync, existsSync, rmSync, mkdirSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../.."); // tor-js monorepo root
const pkgDir = resolve(root, "crates/tor-js-wasm/pkg");
const wasmPath = resolve(pkgDir, "tor_js_bg.wasm");

// 1. Ensure the WASM (+ wasm-bindgen glue) is built.
if (!existsSync(wasmPath)) {
  console.log("[worker] WASM not found — building it (wasm-pack, release)...");
  execSync("wasm-pack build crates/tor-js-wasm --target web --release", { cwd: root, stdio: "inherit" });
}

// 2. Embed the WASM as base64 — the `#wasm-base64-data` module tor-js's
//    wasm-base64 entry imports.
const wasmBytes = readFileSync(wasmPath);
const base64File = resolve(here, ".wasm-base64-data.generated.js");
writeFileSync(base64File, `export const wasmBase64 = ${JSON.stringify(wasmBytes.toString("base64"))};\n`);

const version = JSON.parse(readFileSync(resolve(root, "package.json"), "utf-8")).version;

// 3. Bundle to a single IIFE.
mkdirSync(resolve(here, "dist"), { recursive: true });
try {
  await build({
    entryPoints: [resolve(here, "src/tor-js-worker.ts")],
    outfile: resolve(here, "dist/tor-js-worker.js"),
    bundle: true,
    format: "iife",
    platform: "browser",
    target: "es2022",
    logLevel: "info",
    // The host provides KPS — never bundle the client. Node built-ins appear
    // only in dead (Node-only) tor-js paths a browser worker never runs.
    external: [
      "@kpstreams/webrtc-client",
      "@kpstreams/quic-client",
      "@kpstreams/core",
      "node:net",
      "node:stream",
      "node:crypto",
      "node:fs",
      "node:fs/promises",
      "node:os",
      "node:path",
      "node:url",
    ],
    define: {
      __WASM_SHA256__: '""',
      __PACKAGE_VERSION__: JSON.stringify(version),
    },
    plugins: [
      {
        name: "torjs-wasm-resolve",
        setup(b) {
          b.onResolve({ filter: /^#wasm$/ }, () => ({ path: resolve(pkgDir, "tor_js.js") }));
          b.onResolve({ filter: /^#wasm-base64-data$/ }, () => ({ path: base64File }));
        },
      },
    ],
  });
} finally {
  rmSync(base64File, { force: true });
}

const out = readFileSync(resolve(here, "dist/tor-js-worker.js"));
// sha256 is just a build fingerprint — the on-chain pin is keccak256, computed
// by the anon-rpc specifier tooling.
const sha = createHash("sha256").update(out).digest("hex");
console.log(`[worker] dist/tor-js-worker.js  ${(out.length / 1024 / 1024).toFixed(2)} MB  (sha256 ${sha.slice(0, 16)}…)`);
