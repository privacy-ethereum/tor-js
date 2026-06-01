// Entry point: tor-js/wasm-base64
// Includes WASM as a base64-encoded string. Self-contained, no external file needed.

import { setWasmSourceProvider } from '../../wasm.js';
import { wasmBase64 } from '#wasm-base64-data';

export { TorClient } from '../../TorClient.js';
export * from '../../commonExports.js';

setWasmSourceProvider(async () => {
  const binaryString = atob(wasmBase64);
  const bytes = new Uint8Array(binaryString.length);
  for (let i = 0; i < binaryString.length; i++) {
    bytes[i] = binaryString.charCodeAt(i);
  }
  return bytes;
});
