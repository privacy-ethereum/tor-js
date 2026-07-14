// Integration suite: a spawned gateway exercised over real KPS/QUIC streams
// with @kpstreams/quic-client (PROTOCOL.md is the reference for every
// assertion here).
import { test, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { dial } from '@kpstreams/quic-client'
import {
  makeFixtures,
  spawnGateway,
  startEcho,
  exchange,
  get,
  connectTunnel,
  waitForLog,
} from './helpers.mjs'

const enc = new TextEncoder()

let fixtures, echo, gateway, conn

before(async () => {
  echo = await startEcho()
  fixtures = await makeFixtures({ allowedTargets: [`127.0.0.1:${echo.port}`] })
  gateway = await spawnGateway(fixtures)
  conn = await dial(gateway.address)
})

after(async () => {
  await conn?.close()
  await gateway?.stop()
  await echo?.close()
  await fixtures?.cleanup()
})

test('metadata.json — capability discovery', async () => {
  const res = await get(conn, '/metadata.json')
  assert.equal(res.status, 200)
  assert.equal(res.headers['content-type'], 'application/json')
  assert.equal(res.headers['content-length'], String(res.body.length))
  const meta = JSON.parse(res.body.toString())
  assert.equal(meta.protocol, 'kps-http/1')
  assert.equal(meta.software, 'tor-js-gateway')
  for (const cap of ['metadata', 'bootstrap', 'connect', 'relay-random', 'worker-bundles']) {
    assert.ok(meta.capabilities.includes(cap), `capability ${cap}`)
  }
  assert.deepEqual(meta.addresses, [gateway.address])
})

test('bootstrap.zip.br — bytes and headers', async () => {
  const res = await get(conn, '/bootstrap.zip.br')
  assert.equal(res.status, 200)
  assert.equal(res.headers['content-length'], String(res.body.length))
  assert.equal(
    res.headers['x-decompressed-content-length'],
    String(fixtures.bootstrapZip.length)
  )
  assert.ok(!('transfer-encoding' in res.headers), 'no chunked responses (§3.4)')
  const { brotliDecompressSync } = await import('node:zlib')
  assert.deepEqual(brotliDecompressSync(res.body), fixtures.bootstrapZip)
})

test('worker bundle — happy path is immutable and length-delimited', async () => {
  const h = fixtures.bundleHash
  const res = await get(conn, `/keccak/${h.slice(0, 2)}/${h.slice(2)}`)
  assert.equal(res.status, 200)
  assert.equal(res.headers['content-type'], 'text/javascript')
  assert.equal(res.headers['cache-control'], 'public, max-age=31536000, immutable')
  assert.equal(res.headers['content-length'], String(res.body.length))
  assert.deepEqual(res.body, fixtures.bundleBytes)
})

test('worker bundle — unknown hash and malformed paths are 404', async () => {
  assert.equal((await get(conn, `/keccak/11/${'1'.repeat(62)}`)).status, 404)
  assert.equal((await get(conn, `/keccak/aa/${'a'.repeat(61)}`)).status, 404) // too short
  assert.equal((await get(conn, `/keccak/AA/${'a'.repeat(62)}`)).status, 404) // uppercase
  assert.equal((await get(conn, `/keccak/${'a'.repeat(64)}`)).status, 404) // unsharded
  assert.equal((await get(conn, `/keccak/a/${'a'.repeat(63)}`)).status, 404) // bad split
  const h = fixtures.bundleHash
  assert.equal((await get(conn, `/keccak/${h.slice(0, 2)}/${h.slice(2)}.js`)).status, 404) // extension
  assert.equal((await get(conn, `/worker/${h}.js`)).status, 404) // old route is gone
})

test('worker bundle — a mismatched file is refused (and logged) on request', async () => {
  // Verification is lazy: nothing is logged until the bad path is requested.
  const res = await get(conn, `/keccak/00/${'0'.repeat(62)}`)
  assert.equal(res.status, 404)
  // The server's stderr log line can trail the HTTP response slightly.
  await waitForLog(gateway, /REFUSING .*00\/0{62}/)
})

test('worker bundle — a file added after startup is served (lazy load)', async () => {
  const { hash, bytes } = await fixtures.addBundle(
    `export const late = '${Date.now()}'\n`
  )
  const res = await get(conn, `/keccak/${hash.slice(0, 2)}/${hash.slice(2)}`)
  assert.equal(res.status, 200)
  assert.deepEqual(res.body, bytes)
})

test('relay/random — served from the preloaded allowlist', async () => {
  const res = await get(conn, '/relay/random')
  assert.equal(res.status, 200)
  assert.equal(res.body.toString(), `127.0.0.1:${echo.port}`)
})

test('unknown path 404, wrong method 405', async () => {
  assert.equal((await get(conn, '/nope')).status, 404)
  const res = await exchange(
    conn,
    'POST /metadata.json HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n'
  )
  assert.equal(res.status, 405)
})

test('request without Host is rejected (§3.2)', async () => {
  const res = await exchange(conn, 'GET /metadata.json HTTP/1.1\r\n\r\n')
  assert.equal(res.status, 400)
})

test('Transfer-Encoding is rejected (§3.4)', async () => {
  const res = await exchange(
    conn,
    'GET /metadata.json HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n'
  )
  assert.equal(res.status, 400)
})

test('header block over 16 KiB is refused (§3.6)', async () => {
  const big = 'X-Pad: ' + 'a'.repeat(20 * 1024) + '\r\n'
  const res = await exchange(conn, `GET /metadata.json HTTP/1.1\r\nHost: x\r\n${big}\r\n`)
    .catch(() => ({ status: NaN, raw: Buffer.alloc(0) }))
  // The server must abandon the exchange: either an error status (431 if the
  // stack manages one) or a reset/EOF with no 200.
  assert.ok(Number.isNaN(res.status) || res.status >= 400, `got status ${res.status}`)
})

test('CONNECT — happy path tunnels to an allowlisted target', async () => {
  const t = await connectTunnel(conn, `127.0.0.1:${echo.port}`)
  assert.equal(t.status, 200)
  assert.ok(!('content-length' in t.headers), 'CONNECT 200 has no Content-Length (§4)')
  assert.ok(!('transfer-encoding' in t.headers), 'CONNECT 200 has no Transfer-Encoding (§4)')

  const payload = 'ping through the tunnel'
  await t.writer.write(enc.encode(payload))
  let echoed = t.extra
  while (echoed.length < payload.length) {
    const { value, done } = await t.reader.read()
    if (done) break
    echoed = Buffer.concat([echoed, value])
  }
  assert.equal(echoed.toString(), payload)

  // Client FIN → target FIN → echo closes → server FINs back (§4 lifecycle).
  await t.writer.close()
  const { done } = await t.reader.read()
  assert.equal(done, true)
})

test('CONNECT — names and unparseable targets are 400', async () => {
  assert.equal((await connectTunnel(conn, 'relay.example.com:9001')).status, 400)
  assert.equal((await connectTunnel(conn, '999.1.2.3:1')).status, 400)
  assert.equal((await connectTunnel(conn, '127.0.0.1')).status, 400) // no port
})

test('CONNECT — targets outside the consensus allowlist are 403', async () => {
  const res = await connectTunnel(conn, '203.0.113.7:9001')
  assert.equal(res.status, 403)
})

test('concurrent streams on one connection', async () => {
  const results = await Promise.all(
    Array.from({ length: 8 }, () => get(conn, '/metadata.json'))
  )
  for (const r of results) assert.equal(r.status, 200)
})

test('CONNECT — local targets are always 403 without the test override', async () => {
  // Separate instance: same fixtures (echo IS allowlisted), but the
  // local-target escape hatch off — is_local must still refuse it.
  const fx = await makeFixtures({ allowedTargets: [`127.0.0.1:${echo.port}`] })
  const gw = await spawnGateway(fx, { env: { TOR_JS_GATEWAY_ALLOW_LOCAL_TARGETS: '0' } })
  try {
    const c = await dial(gw.address)
    const res = await connectTunnel(c, `127.0.0.1:${echo.port}`)
    assert.equal(res.status, 403)
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
  }
})

test('CONNECT — per-IP tunnel limit returns 429', async () => {
  const fx = await makeFixtures({ allowedTargets: [`127.0.0.1:${echo.port}`] })
  const gw = await spawnGateway(fx, { config: { tunnel_per_ip: 2 } })
  try {
    const c = await dial(gw.address)
    const t1 = await connectTunnel(c, `127.0.0.1:${echo.port}`)
    const t2 = await connectTunnel(c, `127.0.0.1:${echo.port}`)
    assert.equal(t1.status, 200)
    assert.equal(t2.status, 200)
    const t3 = await connectTunnel(c, `127.0.0.1:${echo.port}`)
    assert.equal(t3.status, 429)
    // Releasing a slot frees capacity again.
    await t1.writer.close()
    await t1.reader.read() // drain to FIN
    await new Promise(r => setTimeout(r, 300))
    const t4 = await connectTunnel(c, `127.0.0.1:${echo.port}`)
    assert.equal(t4.status, 200)
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
  }
})
