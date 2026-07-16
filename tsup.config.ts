import { defineConfig } from 'tsup';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const pkgDir = resolve(__dirname, 'crates/tor-js-wasm/pkg');

export default defineConfig({
  entry: {
    'entryPoints/wasm-cdn/index': 'src/entryPoints/wasm-cdn/index.ts',
    'entryPoints/wasm-cdn/singleton': 'src/entryPoints/wasm-cdn/singleton.ts',
    'entryPoints/wasm-base64/index': 'src/entryPoints/wasm-base64/index.ts',
    'entryPoints/wasm-base64/singleton': 'src/entryPoints/wasm-base64/singleton.ts',
    'entryPoints/wasm-file/index': 'src/entryPoints/wasm-file/index.ts',
    'entryPoints/wasm-file/singleton': 'src/entryPoints/wasm-file/singleton.ts',
  },
  format: ['esm'],
  dts: true,
  sourcemap: true,
  clean: false, // build.mjs handles cleaning
  splitting: false,
  target: 'es2022',
  external: ['node:fs/promises', 'node:os', 'node:path', 'node:url', 'node:crypto'],
  // Bundle the KPS browser client (small, zero-dep ESM) so the dist works
  // without a bundler or import map. The optional Node QUIC transport is
  // imported via a non-literal specifier and stays external.
  noExternal: ['@kpstreams/core', '@kpstreams/webrtc-client'],
  esbuildPlugins: [{
    name: 'resolve-for-bundling',
    setup(build) {
      build.onResolve({ filter: /^#wasm$/ }, () => ({
        path: resolve(pkgDir, 'tor_js.js'),
      }));
      build.onResolve({ filter: /^#wasm-base64-data$/ }, () => ({
        path: resolve(__dirname, 'src', '_wasm-base64-data.generated.js'),
      }));
    },
  }],
  define: {
    '__WASM_SHA256__': JSON.stringify(process.env.WASM_SHA256 || ''),
    '__PACKAGE_VERSION__': JSON.stringify(process.env.PACKAGE_VERSION || ''),
  },
});