// Spawns the gateway and a static page server, mirroring the kps repo's
// interop pattern (tests/interop there). The page imports the *published*
// @kpstreams packages from this package's node_modules via an import map.
import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { createServer } from 'node:http'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, extname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const repoRoot = resolve(here, '../..')
const gatewayBin =
  process.env.GATEWAY_BIN ?? join(repoRoot, 'target/debug/tor-js-gateway')
const websiteDir = join(repoRoot, 'website')
const stateFilePath = join(here, '.run-state.json')

function run(cmd, args, opts) {
  return new Promise((res, rej) => {
    const p = spawn(cmd, args, { stdio: 'inherit', ...opts })
    p.on('exit', code =>
      code === 0 ? res() : rej(new Error(`${cmd} ${args.join(' ')} exited ${code}`))
    )
    p.on('error', rej)
  })
}

// The website spec drives the real site; build its bundle first.
async function buildWebsite() {
  if (!existsSync(join(websiteDir, 'node_modules'))) {
    console.log('[setup] npm install (website)...')
    await run('npm', ['install', '--no-audit', '--no-fund'], { cwd: websiteDir })
  }
  console.log('[setup] npm run build (website)...')
  await run('npm', ['run', 'build'], { cwd: websiteDir })
}

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
  ['/website/', websiteDir],
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

async function startGateway(stateDir) {
  const cfg = {
    data_dir: join(stateDir, 'data'),
    kps_port: 0,
    kps_key_file: join(stateDir, 'kps.key'),
    keccak_dir: '',
    advertised_addresses: ['127.0.0.1'],
    tunnel_max: 8192,
    tunnel_per_ip: 16,
    tunnel_idle_timeout: 300,
    tunnel_max_lifetime: 3600,
  }
  const cfgPath = join(stateDir, 'config.json5')
  await writeFile(cfgPath, JSON.stringify(cfg))
  const child = spawn(gatewayBin, ['--config', cfgPath, 'run', '--no-sync'], {
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
        res({ address: m[0], child })
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
  await buildWebsite()
  const stateDir = await mkdtemp(join(tmpdir(), 'tjg-browser-'))
  const gateway = await startGateway(stateDir)
  const httpServer = await startStaticServer()
  const baseUrl = `http://127.0.0.1:${httpServer.address().port}`
  await writeFile(
    stateFilePath,
    JSON.stringify({ gatewayAddress: gateway.address, baseUrl }, null, 2)
  )
  console.log(`[setup] gateway: ${gateway.address}`)
  console.log(`[setup] page:    ${baseUrl}`)

  return async () => {
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
