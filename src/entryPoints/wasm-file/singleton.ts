// Entry point: tor-js/wasm-file/singleton
// Singleton that loads WASM from a file alongside the JS module.

import './index.js'; // side effect: registers file WASM source provider

export { tor } from '../../singleton.js';
export * from '../../commonExports.js';
