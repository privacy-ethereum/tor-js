// Builds the tor-js anon-rpc worker to a single standalone IIFE with the WASM
// embedded — its bytes are the anon-rpc §4 artifact (keccak256 = the pinned
// workerHash). tor-js FROM SOURCE with @kpstreams/* external, so the KPS client
// code (the host provides KPS via the capability) never enters the bundle.
//
// Exposed as `buildAnonRpcWorker()` so the main tor-js build (build.mjs) can
// emit it straight into the package `dist/` — CDNs then host it and its CDN
// URLs can serve as specifier resolvers. Run directly for standalone dev.

import { build } from "esbuild";
import { execSync } from "node:child_process";
import { readFileSync, writeFileSync, existsSync, rmSync, mkdirSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";

const here = dirname(fileURLToPath(import.meta.url));

/**
 * Bundle the worker to `outfile`. Assumes the tor-js WASM package
 * (`crates/tor-js-wasm/pkg`) already exists unless `ensureWasm` is set.
 * @param {{ root: string, outfile: string, ensureWasm?: boolean }} opts
 * @returns {Promise<{ bytes: number, sha256: string }>}
 */
export async function buildAnonRpcWorker({ root, outfile, ensureWasm = false }) {
  const pkgDir = resolve(root, "crates/tor-js-wasm/pkg");
  const wasmPath = resolve(pkgDir, "tor_js_bg.wasm");

  if (!existsSync(wasmPath)) {
    if (!ensureWasm) throw new Error(`anon-rpc worker: WASM not built at ${wasmPath}`);
    console.log("[anon-rpc-worker] building WASM (wasm-pack, release)...");
    execSync("wasm-pack build crates/tor-js-wasm --target web --release", { cwd: root, stdio: "inherit" });
  }

  // Embed the WASM as base64 — the `#wasm-base64-data` module tor-js's
  // wasm-base64 entry imports.
  const wasmBytes = readFileSync(wasmPath);
  const base64File = resolve(here, ".wasm-base64-data.generated.js");
  writeFileSync(base64File, `export const wasmBase64 = ${JSON.stringify(wasmBytes.toString("base64"))};\n`);
  const version = JSON.parse(readFileSync(resolve(root, "package.json"), "utf-8")).version;

  mkdirSync(dirname(outfile), { recursive: true });
  try {
    await build({
      entryPoints: [resolve(here, "src/tor-js-worker.ts")],
      outfile,
      bundle: true,
      format: "iife",
      platform: "browser",
      target: "es2022",
      logLevel: "warning",
      // Host provides KPS — never bundle the client. Node built-ins are only in
      // dead (Node-only) tor-js paths a browser worker never runs.
      external: [
        "@kpstreams/webrtc-client", "@kpstreams/quic-client", "@kpstreams/core",
        "node:net", "node:stream", "node:crypto",
        "node:fs", "node:fs/promises", "node:os", "node:path", "node:url",
      ],
      define: { __WASM_SHA256__: '""', __PACKAGE_VERSION__: JSON.stringify(version) },
      plugins: [{
        name: "torjs-wasm-resolve",
        setup(b) {
          b.onResolve({ filter: /^#wasm$/ }, () => ({ path: resolve(pkgDir, "tor_js.js") }));
          b.onResolve({ filter: /^#wasm-base64-data$/ }, () => ({ path: base64File }));
        },
      }],
    });
  } finally {
    rmSync(base64File, { force: true });
  }

  const out = readFileSync(outfile);
  // sha256 is a build fingerprint; the on-chain pin is keccak256 (computed by
  // the anon-rpc specifier tooling).
  return { bytes: out.length, sha256: createHash("sha256").update(out).digest("hex") };
}

// Standalone dev build: node build.mjs → dist/tor-js-worker.js
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const root = resolve(here, "../..");
  const outfile = resolve(here, "dist/tor-js-worker.js");
  const { bytes, sha256 } = await buildAnonRpcWorker({ root, outfile, ensureWasm: true });
  console.log(`[anon-rpc-worker] ${outfile}  ${(bytes / 1024 / 1024).toFixed(2)} MB  (sha256 ${sha256.slice(0, 16)}…)`);
}
