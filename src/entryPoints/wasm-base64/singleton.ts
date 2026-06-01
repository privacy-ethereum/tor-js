// Entry point: tor-js/wasm-base64/singleton
// Singleton with WASM as a base64-encoded string. Self-contained, no external file needed.

import './index.js'; // side effect: registers base64 WASM source provider

export { tor } from '../../singleton.js';
export * from '../../commonExports.js';
