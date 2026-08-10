// Shared helpers: gateway fixtures/spawning and minimal KPS-HTTP/1 exchanges
// (PROTOCOL.md §3) over @kpstreams/quic-client streams.
import { spawn } from 'node:child_process'
import { createHash, randomBytes } from 'node:crypto'
import { createServer } from 'node:http'
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises'
import net from 'node:net'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { zstdCompressSync } from 'node:zlib'
import { keccak_256 } from '@noble/hashes/sha3'

const here = dirname(fileURLToPath(import.meta.url))
// The Cargo workspace root is four levels up (tests/integration →
// crates/tor-js-gateway → crates → repo root); the built binary lands in the
// workspace-wide target/ there, not under the crate.
const workspaceRoot = resolve(here, '../../../..')

export const GATEWAY_BIN =
  process.env.GATEWAY_BIN ?? join(workspaceRoot, 'target/debug/tor-js-gateway')

const hex = b => Buffer.from(b).toString('hex')

/// Creates a gateway working dir: bootstrap fixtures and a cached consensus
/// whose relay allowlist contains `allowedTargets`.
///
/// Hash-addressed objects are not seeded here by default — the mirror owns
/// <dataDir>/keccak and prunes anything the branch doesn't list. Tests that
/// want files there without a mirror use `seedObject`/`seedRawObject` together
/// with `--no-mirror`.
export async function makeFixtures({ allowedTargets = [] } = {}) {
  const dir = await mkdtemp(join(tmpdir(), 'tjg-it-'))
  const dataDir = join(dir, 'data')
  const keccakDir = join(dataDir, 'keccak')
  await mkdir(dataDir)
  await mkdir(keccakDir)

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

  return {
    dir,
    dataDir,
    keccakDir,
    bootstrapZip,
    /// Place a correctly-named object straight into the mirror's directory,
    /// bypassing the mirror. Only meaningful under --no-mirror, since a sync
    /// prunes whatever the branch does not list.
    async seedObject(content) {
      const bytes = Buffer.from(content)
      const h = hex(keccak_256(bytes))
      await mkdir(join(keccakDir, h.slice(0, 2)), { recursive: true })
      await writeFile(join(keccakDir, h.slice(0, 2), h.slice(2)), bytes)
      return { hash: h, bytes }
    },
    /// Place bytes at a hash path they do not hash to — the case the route's
    /// own verification exists to catch.
    async seedRawObject(hash, content) {
      await mkdir(join(keccakDir, hash.slice(0, 2)), { recursive: true })
      await writeFile(join(keccakDir, hash.slice(0, 2), hash.slice(2)), Buffer.from(content))
      return hash
    },
    async cleanup() {
      await rm(dir, { recursive: true, force: true })
    },
  }
}

/// A stand-in for the three GitHub endpoints the object mirror uses: the branch
/// ref, the recursive tree, and raw blob bytes. One server carries both bases —
/// `/repos/…` is the API, `/raw/…` is raw.githubusercontent.com — so a gateway
/// only needs two env vars pointed here.
///
/// The branch is mutable: `publish`/`unpublish` change the tree and move the
/// head commit, which is what a `git push` looks like from the mirror's side.
export async function startFakeGitHub({ repo = 'owner/repo', branch = 'keccak' } = {}) {
  const files = new Map() // path ("<hh>/<rest>", or anything else) -> Buffer
  const sizeOverrides = new Map() // path -> size the tree listing claims
  const requests = []
  let commitCounter = 0
  let commit = nextCommit()
  let truncated = false

  function nextCommit() {
    commitCounter += 1
    return createHash('sha1').update(`commit-${commitCounter}`).digest('hex')
  }

  function treeDoc() {
    // Real recursive listings carry a `tree` entry per directory; keep them so
    // the mirror's "ignore anything that isn't an object blob" path is
    // exercised the way it will be in production.
    const shards = new Set(
      [...files.keys()].filter(p => p.includes('/')).map(p => p.split('/')[0])
    )
    const tree = [
      ...[...shards].map(path => ({ path, type: 'tree', mode: '040000', sha: 'a'.repeat(40) })),
      ...[...files.entries()].map(([path, bytes]) => ({
        path,
        type: 'blob',
        mode: '100644',
        sha: createHash('sha1').update(bytes).digest('hex'),
        size: sizeOverrides.get(path) ?? bytes.length,
      })),
    ]
    return { sha: commit, tree, truncated }
  }

  const server = createServer((req, res) => {
    const path = decodeURIComponent(new URL(req.url, 'http://localhost').pathname)
    requests.push({ method: req.method, path })

    const json = (status, body) => {
      const text = JSON.stringify(body)
      res.writeHead(status, {
        'content-type': 'application/json',
        'content-length': Buffer.byteLength(text),
      })
      res.end(text)
    }

    const refPrefix = `/repos/${repo}/git/ref/heads/`
    const treePrefix = `/repos/${repo}/git/trees/`
    const rawPrefix = `/raw/${repo}/`

    if (path.startsWith(refPrefix)) {
      if (path.slice(refPrefix.length) !== branch) return json(404, { message: 'Not Found' })
      return json(200, { ref: `refs/heads/${branch}`, object: { sha: commit, type: 'commit' } })
    }
    if (path.startsWith(treePrefix)) {
      if (path.slice(treePrefix.length) !== commit) return json(404, { message: 'Not Found' })
      return json(200, treeDoc())
    }
    if (path.startsWith(rawPrefix)) {
      // <sha>/<object path>
      const rest = path.slice(rawPrefix.length)
      const slash = rest.indexOf('/')
      if (rest.slice(0, slash) !== commit) return json(404, { message: 'Not Found' })
      const bytes = files.get(rest.slice(slash + 1))
      if (!bytes) return json(404, { message: 'Not Found' })
      res.writeHead(200, { 'content-type': 'text/plain', 'content-length': bytes.length })
      return res.end(bytes)
    }
    return json(404, { message: 'Not Found' })
  })

  const port = await new Promise(res =>
    server.listen(0, '127.0.0.1', () => res(server.address().port))
  )
  const origin = `http://127.0.0.1:${port}`

  return {
    repo,
    branch,
    apiBase: origin,
    rawBase: `${origin}/raw`,
    requests,
    get commit() {
      return commit
    },
    /// Add a hash-addressed object; returns its hash, bytes and tree path.
    publish(content) {
      const bytes = Buffer.from(content)
      const hash = hex(keccak_256(bytes))
      const path = `${hash.slice(0, 2)}/${hash.slice(2)}`
      files.set(path, bytes)
      commit = nextCommit()
      return { hash, bytes, path }
    },
    /// Add a file at an arbitrary path — a README, or an object path whose
    /// contents do not hash to it.
    publishAt(path, content) {
      files.set(path, Buffer.from(content))
      commit = nextCommit()
      return path
    },
    unpublish(path) {
      files.delete(path)
      sizeOverrides.delete(path)
      commit = nextCommit()
    },
    /// Claim a size in the tree listing without transferring those bytes, so
    /// the per-object cap can be tested without moving 64 MiB.
    claimSize(path, size) {
      sizeOverrides.set(path, size)
      commit = nextCommit()
    },
    setTruncated(value) {
      truncated = value
      commit = nextCommit()
    },
    close() {
      return new Promise(res => server.close(res))
    },
  }
}

/// Runs the gateway binary as a *client* (`tor-js-gateway sync …`) and resolves
/// with its exit code and streams. Never throws on a non-zero exit — the exit
/// code is part of what the subcommand promises.
export function runCli(args, { env = {} } = {}) {
  return new Promise((res, rej) => {
    const child = spawn(GATEWAY_BIN, args, {
      env: { ...process.env, ...env },
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    let stdout = ''
    let stderr = ''
    child.stdout.on('data', c => (stdout += c))
    child.stderr.on('data', c => (stderr += c))
    child.on('error', rej)
    child.on('close', code => res({ code, stdout, stderr }))
  })
}

/// Poll GET /keccak/sync until the mirror reports `count` objects.
export async function waitForMirrorObjects(conn, count, ms = 20_000) {
  const deadline = Date.now() + ms
  let last
  while (Date.now() < deadline) {
    const res = await get(conn, '/keccak/sync')
    last = res.body.toString()
    if (res.status === 200 && JSON.parse(last).objects === count) return JSON.parse(last)
    await new Promise(r => setTimeout(r, 50))
  }
  throw new Error(`mirror did not reach ${count} object(s) within ${ms}ms; last: ${last}`)
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
///
/// `github` (from `startFakeGitHub`) both configures the mirror's repo/branch
/// and points its two origin env vars at the fake, so no test reaches the real
/// api.github.com. Without it the worker-bundles capability stays off, which is
/// what an operator who never set keccak_repo gets.
export async function spawnGateway(
  fixtures,
  { config = {}, env = {}, args = [], github } = {}
) {
  const port = await freeUdpPort()
  const cfg = {
    data_dir: fixtures.dataDir,
    kps_port: port,
    kps_key_file: join(fixtures.dir, 'kps.key'),
    keccak_repo: github?.repo ?? '',
    keccak_branch: github?.branch ?? '',
    keccak_poll_interval: 86400,
    keccak_manual_sync_min_interval: 1800,
    advertised_addresses: ['127.0.0.1'],
    tunnel_max: 8192,
    tunnel_per_ip: 16,
    tunnel_idle_timeout: 300,
    tunnel_max_lifetime: 3600,
    ...config,
  }
  const cfgPath = join(fixtures.dir, 'config.json5')
  await writeFile(cfgPath, JSON.stringify(cfg, null, 2))

  const child = spawn(GATEWAY_BIN, ['--config', cfgPath, 'run', '--no-sync', ...args], {
    env: {
      ...process.env,
      TOR_JS_GATEWAY_ALLOW_LOCAL_TARGETS: '1',
      // Never let a test fall through to the real GitHub: an unset base would
      // silently start polling it.
      TOR_JS_GATEWAY_GITHUB_API: github?.apiBase ?? 'http://127.0.0.1:1',
      TOR_JS_GATEWAY_GITHUB_RAW: github?.rawBase ?? 'http://127.0.0.1:1',
      // A token would change the request shape against the fake, and there is
      // nothing to authenticate to.
      GITHUB_TOKEN: '',
      ...env,
    },
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
    // The `sync` subcommand reads this to derive the local gateway's address.
    configPath: cfgPath,
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
