// Single bundle entry for the TorClient unit tests.
//
// TorClient and wasm.ts must come from ONE bundle: each esbuild bundle carries
// its own copy of every module, so bundling them separately would give the test
// a different wasm.ts instance (and a different stub) from the one TorClient
// uses. Outside tsconfig's `include`, so `tsc --noEmit` ignores it.

export * from '../../../src/wasm.js';
export { TorClient } from '../../../src/TorClient.js';
export { MemoryStorage } from '../../../src/storage/index.js';
export { tor } from '../../../src/singleton.js';
