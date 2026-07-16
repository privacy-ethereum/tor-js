// Shared helpers: gateway fixtures/spawning and minimal KPS-HTTP/1 exchanges
// (PROTOCOL.md §3) over @kpstreams/quic-client streams.
import { spawn } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises'
import net from 'node:net'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { zstdCompressSync } from 'node:zlib'
import { keccak_256 } from '@noble/hashes/sha3'

const here = dirname(fileURLToPath(import.meta.url))
const repoRoot = resolve(here, '../..')

export const GATEWAY_BIN =
  process.env.GATEWAY_BIN ?? join(repoRoot, 'target/debug/tor-js-gateway')

const hex = b => Buffer.from(b).toString('hex')

/// Creates a gateway working dir: bootstrap fixtures, worker bundles
/// (one valid, one with a lying filename), and a cached consensus whose relay
/// allowlist contains `allowedTargets`.
export async function makeFixtures({ allowedTargets = [] } = {}) {
  const dir = await mkdtemp(join(tmpdir(), 'tjg-it-'))
  const dataDir = join(dir, 'data')
  const bundlesDir = join(dir, 'bundles')
  await mkdir(dataDir)
  await mkdir(bundlesDir)

  // The gateway serves the bootstrap archive opaquely, so arbitrary bytes do.
  const bootstrapZip = randomBytes(1024)
  await writeFile(join(dataDir, 'bootstrap.zip'), bootstrapZip)
  await writeFile(join(dataDir, 'bootstrap.zip.zst'), zstdCompressSync(bootstrapZip))

  // Consensus 'r' lines pre-populate the relay allowlist at startup.
  const rLines = allowedTargets
    .map(t => {
      const [ip, port] = t.split(':')
      return `r test AAAAAAAAAAAAAAAAAAAAAAAAAAA 2026-01-01 00:00:00 ${ip} ${port} 0`
    })
    .join('\n')
  await writeFile(join(dataDir, 'consensus-microdesc.txt'), rLines + '\n')

  // Hash-addressed objects at <keccak_dir>/<hh>/<rest> (disk layout mirrors
  // the /keccak/ route): one correctly placed, one whose path lies.
  const bundleBytes = Buffer.from(`export const fixture = '${randomBytes(8).toString('hex')}'\n`)
  const bundleHash = hex(keccak_256(bundleBytes))
  await mkdir(join(bundlesDir, bundleHash.slice(0, 2)))
  await writeFile(join(bundlesDir, bundleHash.slice(0, 2), bundleHash.slice(2)), bundleBytes)
  await mkdir(join(bundlesDir, '00'))
  await writeFile(join(bundlesDir, '00', '0'.repeat(62)), Buffer.from('// wrong hash\n'))

  return {
    dir,
    dataDir,
    bundlesDir,
    bootstrapZip,
    bundleBytes,
    bundleHash,
    // Drop a new hash-addressed object into the tree at runtime; returns its
    // keccak256 hex (its /keccak/<hh>/<rest> path). Used to prove lazy loading.
    async addBundle(content) {
      const bytes = Buffer.from(content)
      const h = hex(keccak_256(bytes))
      await mkdir(join(bundlesDir, h.slice(0, 2)), { recursive: true })
      await writeFile(join(bundlesDir, h.slice(0, 2), h.slice(2)), bytes)
      return { hash: h, bytes }
    },
    async cleanup() {
      await rm(dir, { recursive: true, force: true })
    },
  }
}

/// Wait until the gateway's captured logs match `pattern` (its stderr can
/// trail the HTTP response it accompanies). Throws if not seen within `ms`.
export async function waitForLog(gateway, pattern, ms = 3000) {
  const deadline = Date.now() + ms
  while (Date.now() < deadline) {
    if (pattern.test(gateway.logs.out)) return
    await new Promise(r => setTimeout(r, 25))
  }
  throw new Error(`log did not match ${pattern} within ${ms}ms; logs:\n${gateway.logs.out}`)
}

function freeUdpPort() {
  return new Promise((res, rej) => {
    const s = net.createServer() // TCP probe is close enough for a free number
    s.once('error', rej)
    s.listen(0, '127.0.0.1', () => {
      const port = s.address().port
      s.close(() => res(port))
    })
  })
}

/// Spawns the gateway with `run --no-sync` over the fixture dir and resolves
/// once it prints its dialable address. Returns { address, child, logs, stop }.
export async function spawnGateway(fixtures, { config = {}, env = {} } = {}) {
  const port = await freeUdpPort()
  const cfg = {
    data_dir: fixtures.dataDir,
    kps_port: port,
    kps_key_file: join(fixtures.dir, 'kps.key'),
    keccak_dir: fixtures.bundlesDir,
    advertised_addresses: ['127.0.0.1'],
    tunnel_max: 8192,
    tunnel_per_ip: 16,
    tunnel_idle_timeout: 300,
    tunnel_max_lifetime: 3600,
    ...config,
  }
  const cfgPath = join(fixtures.dir, 'config.json5')
  await writeFile(cfgPath, JSON.stringify(cfg, null, 2))

  const child = spawn(GATEWAY_BIN, ['--config', cfgPath, 'run', '--no-sync'], {
    env: { ...process.env, TOR_JS_GATEWAY_ALLOW_LOCAL_TARGETS: '1', ...env },
    stdio: ['ignore', 'pipe', 'pipe'],
  })

  const logs = { out: '' }
  const address = await new Promise((resolveAddr, rejectAddr) => {
    const timer = setTimeout(() => {
      child.kill()
      rejectAddr(new Error(`gateway did not print an address; logs:\n${logs.out}`))
    }, 20_000)
    const onData = chunk => {
      logs.out += chunk.toString()
      const m = logs.out.match(/127\.0\.0\.1:\d+:u[A-Za-z0-9_-]+/)
      if (m) {
        clearTimeout(timer)
        resolveAddr(m[0])
      }
    }
    child.stdout.on('data', onData)
    child.stderr.on('data', onData)
    child.on('exit', code => {
      clearTimeout(timer)
      rejectAddr(new Error(`gateway exited (${code}) before printing address; logs:\n${logs.out}`))
    })
  })

  return {
    address,
    child,
    logs,
    async stop() {
      if (child.exitCode !== null) return
      child.kill('SIGTERM')
      await new Promise(res => {
        const t = setTimeout(() => {
          try { child.kill('SIGKILL') } catch {}
          res()
        }, 3000)
        child.on('exit', () => { clearTimeout(t); res() })
      })
    },
  }
}

/// A local TCP echo server; returns { port, close }.
export function startEcho() {
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

const enc = new TextEncoder()

/// One KPS-HTTP/1 exchange (§3): write the request, FIN, read to EOF.
export async function exchange(conn, request) {
  const stream = await conn.openStream()
  const writer = stream.writable.getWriter()
  await writer.write(typeof request === 'string' ? enc.encode(request) : request)
  await writer.close()
  const chunks = []
  const reader = stream.readable.getReader()
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    chunks.push(value)
  }
  return parseResponse(Buffer.concat(chunks))
}

/// Convenience GET with the certhash-style Host the protocol recommends.
export function get(conn, path, extraHeaders = '') {
  return exchange(conn, `GET ${path} HTTP/1.1\r\nHost: x\r\n${extraHeaders}\r\n`)
}

export function parseResponse(buf) {
  const sep = buf.indexOf('\r\n\r\n')
  if (sep === -1) return { status: NaN, headers: {}, body: Buffer.alloc(0), raw: buf }
  const head = buf.subarray(0, sep).toString('latin1').split('\r\n')
  const status = parseInt(head[0].split(' ')[1], 10)
  const headers = Object.fromEntries(
    head.slice(1).map(l => {
      const i = l.indexOf(':')
      return [l.slice(0, i).toLowerCase(), l.slice(i + 1).trim()]
    })
  )
  return { status, headers, body: buf.subarray(sep + 4), raw: buf }
}

/// Opens a CONNECT tunnel; resolves with the parsed response head plus
/// reader/writer positioned inside the tunnel (any body bytes already read
/// are returned as `extra`).
export async function connectTunnel(conn, target) {
  const stream = await conn.openStream()
  const writer = stream.writable.getWriter()
  const reader = stream.readable.getReader()
  await writer.write(enc.encode(`CONNECT ${target} HTTP/1.1\r\nHost: ${target}\r\n\r\n`))
  let buf = Buffer.alloc(0)
  for (;;) {
    const sep = buf.indexOf('\r\n\r\n')
    if (sep !== -1) break
    const { value, done } = await reader.read()
    if (done) break
    buf = Buffer.concat([buf, value])
  }
  const res = parseResponse(buf)
  return { ...res, extra: res.body, reader, writer, stream }
}
