// Spawns the gateway and a static page server, mirroring the kps repo's
// interop pattern (tests/interop there). The page imports the *published*
// @kpstreams packages from this package's node_modules via an import map.
// This test is self-contained (page/index.html) and independent of the
// top-level website, which has its own build and hosting.
import { spawn } from 'node:child_process'
import { createHash, randomBytes } from 'node:crypto'
import { createServer } from 'node:http'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import net from 'node:net'
import { tmpdir } from 'node:os'
import { dirname, extname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { zstdCompressSync } from 'node:zlib'

const here = dirname(fileURLToPath(import.meta.url))
// The built binary lands in the Cargo workspace-wide target/ (repo root, four
// levels up), not under the crate.
const workspaceRoot = resolve(here, '../../../..')
const gatewayBin =
  process.env.GATEWAY_BIN ?? join(workspaceRoot, 'target/debug/tor-js-gateway')
const stateFilePath = join(here, '.run-state.json')

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'application/javascript; charset=utf-8',
  '.map': 'application/json',
}

// Route the bare package specifiers to the installed dist trees (see
// page/index.html's import map); everything else serves from page/.
const STATIC_ROUTES = [
  ['/kps/core/', join(here, 'node_modules/@kpstreams/core/dist')],
  ['/kps/webrtc-client/', join(here, 'node_modules/@kpstreams/webrtc-client/dist')],
]

function startStaticServer() {
  const server = createServer(async (req, res) => {
    try {
      let p = decodeURIComponent(new URL(req.url, 'http://x').pathname)
      if (p.includes('..')) { res.writeHead(400); return res.end('bad') }
      let filePath
      const route = STATIC_ROUTES.find(([prefix]) => p.startsWith(prefix))
      if (route) {
        filePath = join(route[1], p.slice(route[0].length))
      } else {
        if (p === '/' || p === '') p = '/index.html'
        filePath = join(here, 'page', p)
      }
      const data = await readFile(filePath)
      res.writeHead(200, {
        'content-type': MIME[extname(filePath).toLowerCase()] ?? 'application/octet-stream',
      })
      res.end(data)
    } catch {
      res.writeHead(404)
      res.end('not found')
    }
  })
  return new Promise((res, rej) => {
    server.once('error', rej)
    server.listen(0, '127.0.0.1', () => res(server))
  })
}

/// A local TCP echo server for the CONNECT tunnel tests.
function startEcho() {
  const server = net.createServer(s => {
    s.on('error', () => {}) // aborted tunnels RST us by design (§4)
    s.pipe(s)
  })
  return new Promise(res =>
    server.listen(0, '127.0.0.1', () =>
      res({
        port: server.address().port,
        close: () => new Promise(r => server.close(r)),
      })
    )
  )
}

/// Data-dir fixtures so the browser can exercise the real data path: a
/// bootstrap archive to download, and a cached consensus whose relay allowlist
/// contains the echo server (CONNECT only permits advertised relays).
async function writeFixtures(dataDir, echoPort) {
  await mkdir(dataDir, { recursive: true })

  // Deliberately larger than a single WebRTC data-channel message, so a
  // chunking or reassembly bug shows up as a hash mismatch.
  const bootstrapZip = randomBytes(256 * 1024)
  await writeFile(join(dataDir, 'bootstrap.zip'), bootstrapZip)
  const compressed = zstdCompressSync(bootstrapZip)
  await writeFile(join(dataDir, 'bootstrap.zip.zst'), compressed)

  await writeFile(
    join(dataDir, 'consensus-microdesc.txt'),
    `r test AAAAAAAAAAAAAAAAAAAAAAAAAAA 2026-01-01 00:00:00 127.0.0.1 ${echoPort} 0\n`
  )

  return {
    bootstrapLen: compressed.length,
    bootstrapSha256: createHash('sha256').update(compressed).digest('hex'),
    uncompressedLen: bootstrapZip.length,
  }
}

async function startGateway(stateDir, echoPort) {
  const dataDir = join(stateDir, 'data')
  const fixtures = await writeFixtures(dataDir, echoPort)
  const cfg = {
    data_dir: dataDir,
    kps_port: 0,
    kps_key_file: join(stateDir, 'kps.key'),
    // No mirror: the browser suite covers the data plane, not worker bundles.
    keccak_repo: '',
    keccak_branch: '',
    keccak_poll_interval: 86400,
    keccak_manual_sync_min_interval: 1800,
    advertised_addresses: ['127.0.0.1'],
    tunnel_max: 8192,
    tunnel_per_ip: 16,
    tunnel_idle_timeout: 300,
    tunnel_max_lifetime: 3600,
  }
  const cfgPath = join(stateDir, 'config.json5')
  await writeFile(cfgPath, JSON.stringify(cfg))
  const child = spawn(gatewayBin, ['--config', cfgPath, 'run', '--no-sync'], {
    // The echo server is on loopback, which is_local refuses by default.
    env: { ...process.env, TOR_JS_GATEWAY_ALLOW_LOCAL_TARGETS: '1' },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  child.stderr.on('data', c => process.stderr.write(`[gateway] ${c}`))
  return new Promise((res, rej) => {
    let buf = ''
    const timer = setTimeout(() => {
      child.kill()
      rej(new Error(`gateway: timed out waiting for address\n${buf}`))
    }, 20_000)
    const onData = chunk => {
      buf += chunk.toString()
      process.stdout.write(`[gateway] ${chunk}`)
      const m = buf.match(/127\.0\.0\.1:\d+:u[A-Za-z0-9_-]+/)
      if (m) {
        clearTimeout(timer)
        res({ address: m[0], child, fixtures })
      }
    }
    child.stdout.on('data', onData)
    child.on('exit', code => {
      clearTimeout(timer)
      rej(new Error(`gateway exited (${code}) before printing address\n${buf}`))
    })
  })
}

export default async function globalSetup() {
  const stateDir = await mkdtemp(join(tmpdir(), 'tjg-browser-'))
  const echo = await startEcho()
  const gateway = await startGateway(stateDir, echo.port)
  const httpServer = await startStaticServer()
  const baseUrl = `http://127.0.0.1:${httpServer.address().port}`
  await writeFile(
    stateFilePath,
    JSON.stringify(
      {
        gatewayAddress: gateway.address,
        baseUrl,
        echoTarget: `127.0.0.1:${echo.port}`,
        ...gateway.fixtures,
      },
      null,
      2
    )
  )
  console.log(`[setup] gateway: ${gateway.address}`)
  console.log(`[setup] page:    ${baseUrl}`)
  console.log(`[setup] echo:    127.0.0.1:${echo.port}`)

  return async () => {
    await echo.close()
    await new Promise(res => httpServer.close(() => res()))
    if (gateway.child.exitCode === null) {
      gateway.child.kill('SIGTERM')
      await new Promise(res => {
        const t = setTimeout(() => {
          try { gateway.child.kill('SIGKILL') } catch {}
          res()
        }, 3000)
        gateway.child.on('exit', () => { clearTimeout(t); res() })
      })
    }
    try { await rm(stateDir, { recursive: true, force: true }) } catch {}
    try { await rm(stateFilePath, { force: true }) } catch {}
  }
}
