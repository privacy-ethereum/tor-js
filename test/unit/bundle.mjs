// Shared esbuild helper for the unit tests.
//
// The sources under test are TypeScript, so each suite bundles the module it
// needs into `.tmp-*.mjs` (gitignored) and imports that — the same approach
// test/anon-rpc-worker uses to build the worker under test.

import { build } from 'esbuild'
import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'

const here = dirname(fileURLToPath(import.meta.url))
export const root = resolve(here, '../..')

/**
 * Bundle `src/<entry>` and import it.
 *
 * @param {string} entry  path relative to the repo root, e.g. 'src/kpsAddress.ts'
 * @param {string} name   basename for the temp bundle, unique per suite
 * @param {object} [opts]
 * @param {string[]} [opts.external]  extra packages to leave unbundled
 * @param {Record<string,string>} [opts.alias]  module substitutions (for stubs)
 */
export async function bundleTs(entry, name, { external = [], alias } = {}) {
  const outfile = resolve(here, `.tmp-${name}.mjs`)
  await build({
    entryPoints: [resolve(root, entry)],
    bundle: true,
    format: 'esm',
    platform: 'node',
    outfile,
    external: ['@kpstreams/*', 'node:*', ...external],
    ...(alias ? { alias } : {}),
    logLevel: 'silent',
  })
  return import(outfile)
}
