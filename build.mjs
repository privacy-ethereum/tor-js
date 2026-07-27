import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { execSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';
import { build as esbuildBuild } from 'esbuild';

const __dirname = dirname(fileURLToPath(import.meta.url));
const pkgDir = resolve(__dirname, 'crates/tor-js-wasm/pkg');
const distDir = resolve(__dirname, 'dist');

async function main() {
  const { wasmBytes, wasmHash } = computeWasmHash();
  const version = readPackageVersion();
  const base64File = generateBase64Data(wasmBytes);

  const steps = [
    cleanDist,
    () => runTsup(wasmHash, version),
    () => cleanBase64Data(base64File),
    generateDeclarationMaps,
    copyWasmFiles,
    buildWorker,
    verifyDeclarationMaps,
    verifyGzipSizes,
  ];

  for (const step of steps) await step();
}

// Bundle the anon-rpc worker into the package dist so CDNs host it (its CDN
// URLs can then serve as anon-rpc specifier resolvers). See workers/anon-rpc/.
async function buildWorker() {
  const { bytes, sha256 } = await buildAnonRpcWorker({ outfile: resolve(distDir, 'anon-rpc-worker.js') });
  console.log(`Built anon-rpc worker: dist/anon-rpc-worker.js ${(bytes / 1024 / 1024).toFixed(2)} MB (sha256 ${sha256.slice(0, 16)}…)`);
}

/**
 * Bundle the anon-rpc worker (src/anon-rpc-worker/) to a single standalone IIFE
 * with the WASM embedded — its bytes are the anon-rpc §4 artifact (keccak256 =
 * the pinned workerHash). tor-js FROM SOURCE with @kpstreams/* external, so no
 * KPS client code enters the bundle (the host provides KPS as a capability).
 * Exported so the e2e can build the exact shipped artifact.
 * @param {{ outfile: string, ensureWasm?: boolean }} opts
 * @returns {Promise<{ bytes: number, sha256: string }>}
 */
export async function buildAnonRpcWorker({ outfile, ensureWasm = false }) {
  const wasmPath = resolve(pkgDir, 'tor_js_bg.wasm');
  if (!existsSync(wasmPath)) {
    if (!ensureWasm) throw new Error(`anon-rpc worker: WASM not built at ${wasmPath}`);
    execSync('wasm-pack build crates/tor-js-wasm --target web --release', { cwd: __dirname, stdio: 'inherit' });
  }
  const wasmBytes = readFileSync(wasmPath);
  const base64File = resolve(__dirname, 'src/anon-rpc-worker/.wasm-base64-data.generated.js');
  writeFileSync(base64File, `export const wasmBase64 = ${JSON.stringify(wasmBytes.toString('base64'))};\n`);
  const version = JSON.parse(readFileSync(resolve(__dirname, 'package.json'), 'utf-8')).version;

  mkdirSync(dirname(outfile), { recursive: true });
  try {
    await esbuildBuild({
      entryPoints: [resolve(__dirname, 'src/anon-rpc-worker/worker.ts')],
      outfile,
      bundle: true, format: 'iife', platform: 'browser', target: 'es2022', logLevel: 'warning',
      // Host provides KPS — never bundle the client. Node built-ins are only in
      // dead (Node-only) tor-js paths a browser worker never runs.
      external: [
        '@kpstreams/webrtc-client', '@kpstreams/quic-client', '@kpstreams/core',
        'node:net', 'node:stream', 'node:crypto',
        'node:fs', 'node:fs/promises', 'node:os', 'node:path', 'node:url',
      ],
      define: { __WASM_SHA256__: '""', __PACKAGE_VERSION__: JSON.stringify(version) },
      plugins: [{
        name: 'torjs-wasm-resolve',
        setup(b) {
          b.onResolve({ filter: /^#wasm$/ }, () => ({ path: resolve(pkgDir, 'tor_js.js') }));
          b.onResolve({ filter: /^#wasm-base64-data$/ }, () => ({ path: base64File }));
        },
      }],
    });
  } finally {
    rmSync(base64File, { force: true });
  }
  const out = readFileSync(outfile);
  // sha256 is a build fingerprint; the on-chain pin is keccak256 (specifier tooling).
  return { bytes: out.length, sha256: createHash('sha256').update(out).digest('hex') };
}

function cleanDist() {
  rmSync(distDir, { recursive: true, force: true });
}

function computeWasmHash() {
  const wasmPath = resolve(pkgDir, 'tor_js_bg.wasm');
  const wasmBytes = readFileSync(wasmPath);
  const wasmHash = createHash('sha256').update(wasmBytes).digest('hex');
  console.log(`WASM SHA256: ${wasmHash}`);
  return { wasmBytes, wasmHash };
}

function readPackageVersion() {
  const packageJson = JSON.parse(readFileSync(resolve(__dirname, 'package.json'), 'utf-8'));
  console.log(`Package version: ${packageJson.version}`);
  return packageJson.version;
}

function generateBase64Data(wasmBytes) {
  const base64Data = wasmBytes.toString('base64');
  const base64File = resolve(__dirname, 'src', '_wasm-base64-data.generated.js');
  writeFileSync(base64File, `export const wasmBase64 = "${base64Data}";\n`);
  console.log(`Generated base64 data (${(base64Data.length / 1024 / 1024).toFixed(1)} MB)`);
  return base64File;
}

function runTsup(wasmHash, version) {
  execSync('npx tsup', {
    stdio: 'inherit',
    cwd: __dirname,
    env: {
      ...process.env,
      WASM_SHA256: wasmHash,
      PACKAGE_VERSION: version,
    },
  });
}

function cleanBase64Data(base64File) {
  rmSync(base64File);
}

function generateDeclarationMaps() {
  // Overwrites tsup's .d.ts files with versions that include
  // //# sourceMappingURL= comments, enabling "jump to definition" in editors.
  execSync('npx tsc --emitDeclarationOnly --declarationMap', {
    stdio: 'inherit',
    cwd: __dirname,
  });
  console.log('Generated declaration maps (.d.ts.map files)');
}

function copyWasmFiles() {
  const distWasm = resolve(distDir, 'wasm-pkg');
  mkdirSync(distWasm, { recursive: true });
  copyFileSync(resolve(pkgDir, 'tor_js.js'), resolve(distWasm, 'tor_js.js'));
  copyFileSync(resolve(pkgDir, 'tor_js.d.ts'), resolve(distWasm, 'tor_js.d.ts'));
  copyFileSync(resolve(pkgDir, 'tor_js_bg.wasm'), resolve(distDir, 'tor_js_bg.wasm'));
  console.log('Copied WASM runtime files to dist/');
}

function verifyDeclarationMaps() {
  const pkg = JSON.parse(readFileSync(resolve(__dirname, 'package.json'), 'utf-8'));
  const errors = [];
  for (const [exportPath, conditions] of Object.entries(pkg.exports)) {
    const dtsPath = resolve(__dirname, conditions.types);
    const dts = readFileSync(dtsPath, 'utf-8');
    if (!dts.includes('//# sourceMappingURL=')) {
      errors.push(`${conditions.types} missing sourceMappingURL comment`);
    }
    const mapPath = dtsPath + '.map';
    if (!existsSync(mapPath)) {
      errors.push(`${conditions.types}.map does not exist`);
    }
  }
  if (errors.length > 0) {
    console.error('Declaration map verification failed:');
    for (const err of errors) console.error(`  - ${err}`);
    process.exit(1);
  }
  console.log('Verified declaration maps for all exports');
}

function verifyGzipSizes() {
  const entries = [
    { name: 'tor-js', files: ['dist/entryPoints/wasm-cdn/index.js'] },
    { name: 'tor-js/wasm-base64', files: ['dist/entryPoints/wasm-base64/index.js'] },
    { name: 'tor-js/wasm-file', files: ['dist/entryPoints/wasm-file/index.js', 'dist/tor_js_bg.wasm'] },
  ];

  function formatSize(bytes) {
    if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
    return `${Math.round(bytes / 1024)} kB`;
  }

  function parseSize(str) {
    const m = str.match(/^([\d.]+)\s*(kB|MB)$/);
    if (!m) return null;
    const n = parseFloat(m[1]);
    return m[2] === 'MB' ? n * 1024 * 1024 : n * 1024;
  }

  const measured = {};
  for (const ep of entries) {
    const sizes = ep.files.map(f => gzipSync(readFileSync(resolve(distDir, '..', f))).length);
    const display = sizes.map(formatSize).join(' + ');
    console.log(`  ${ep.name}: ${display}`);
    measured[ep.name] = sizes;
  }

  const readme = readFileSync(resolve(__dirname, 'README.md'), 'utf-8');
  const errors = [];
  for (const ep of entries) {
    const row = readme.split('\n').find(line => line.includes(`\`${ep.name}\``) && line.includes('Size (gzip)') === false && line.startsWith('|'));
    if (!row) { errors.push(`${ep.name}: not found in README table`); continue; }
    const cells = row.split('|').map(c => c.trim());
    const sizeCell = cells[3]; // 4th column (0=empty, 1=Import, 2=WASM loading, 3=Size)
    if (!sizeCell) { errors.push(`${ep.name}: no size column in README`); continue; }
    const parts = sizeCell.split('+').map(s => s.trim());
    const readmeSizes = parts.map(parseSize);
    if (readmeSizes.some(s => s === null)) { errors.push(`${ep.name}: could not parse README size "${sizeCell}"`); continue; }
    if (readmeSizes.length !== measured[ep.name].length) { errors.push(`${ep.name}: README has ${readmeSizes.length} size parts but actual has ${measured[ep.name].length}`); continue; }
    for (let i = 0; i < readmeSizes.length; i++) {
      const actual = formatSize(measured[ep.name][i]);
      const drift = Math.abs(measured[ep.name][i] - readmeSizes[i]) / readmeSizes[i];
      if (drift > 0.05) {
        errors.push(`${ep.name}: README says "${parts[i]}" but actual is ${actual} (${(drift * 100).toFixed(1)}% drift)`);
      } else if (actual !== parts[i]) {
        console.warn(`  WARN: ${ep.name}: README says "${parts[i]}" but actual is ${actual} (${(drift * 100).toFixed(1)}% drift)`);
      }
    }
  }
  if (errors.length > 0) {
    console.error('Gzip size validation failed:');
    for (const err of errors) console.error(`  - ${err}`);
    process.exit(1);
  }
  console.log('Gzip sizes match README (within 5%)');
}

// Run the full package build only when executed directly — importing this
// module (e.g. the e2e importing buildAnonRpcWorker) must not trigger a build.
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((err) => {
    console.error(err);
    process.exit(1);
  });
}
