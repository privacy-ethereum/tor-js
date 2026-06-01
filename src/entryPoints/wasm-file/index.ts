// Entry point: tor-js/wasm-file
// Loads WASM from a file alongside the JS module (Node.js filesystem or browser URL).

import { setWasmSourceProvider } from '../../wasm.js';

export { TorClient } from '../../TorClient.js';
export * from '../../commonExports.js';

setWasmSourceProvider(async () => {
  if (typeof process !== 'undefined' && process.versions?.node) {
    const { readFile } = await import('node:fs/promises');
    const { fileURLToPath } = await import('node:url');
    const wasmPath = fileURLToPath(new URL('../../tor_js_bg.wasm', import.meta.url));
    return readFile(wasmPath);
  }
  const resp = await fetch(new URL('../../tor_js_bg.wasm', import.meta.url));
  if (!resp.ok) throw new Error(`Failed to fetch WASM: HTTP ${resp.status}`);
  return new Uint8Array(await resp.arrayBuffer());
});
