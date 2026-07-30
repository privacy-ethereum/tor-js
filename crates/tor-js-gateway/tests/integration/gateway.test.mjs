// Integration suite: a spawned gateway exercised over real KPS/QUIC streams
// with @kpstreams/quic-client (PROTOCOL.md is the reference for every
// assertion here).
import { test, before, after } from 'node:test'
import assert from 'node:assert/strict'
import { rm } from 'node:fs/promises'
import { join } from 'node:path'
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

test('bootstrap.zip.zst — bytes and headers', async () => {
  const res = await get(conn, '/bootstrap.zip.zst')
  assert.equal(res.status, 200)
  assert.equal(res.headers['content-length'], String(res.body.length))
  assert.equal(
    res.headers['x-decompressed-content-length'],
    String(fixtures.bootstrapZip.length)
  )
  assert.ok(!('transfer-encoding' in res.headers), 'no chunked responses (§3.4)')
  const { zstdDecompressSync } = await import('node:zlib')
  assert.deepEqual(zstdDecompressSync(res.body), fixtures.bootstrapZip)
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

/// Reads (discarding data) until the stream ends, and reports how it ended.
///
/// A §4 abort reaches the client as a read *error* (KPS SPEC §9.2: reads error
/// on RESET, and EOF only on a peer FIN), so this is what distinguishes an
/// abortive teardown from a graceful one.
async function readUntilEnd(reader, ms, label) {
  const deadline = new Promise(r => setTimeout(() => r('timeout'), ms))
  for (;;) {
    const outcome = await Promise.race([
      reader.read().then(
        ({ done }) => (done ? 'eof' : 'data'),
        e => `error: ${e?.message ?? e}`,
      ),
      deadline,
    ])
    if (outcome === 'data') continue
    if (outcome === 'timeout') throw new Error(`${label} did not happen within ${ms}ms`)
    return outcome
  }
}

test('bootstrap.zip.zst — ETag and If-None-Match get a bodyless 304', async () => {
  const first = await get(conn, '/bootstrap.zip.zst')
  assert.equal(first.status, 200)
  const etag = first.headers['etag']
  assert.ok(etag, 'an ETag is required for conditional requests')

  const second = await get(conn, '/bootstrap.zip.zst', `If-None-Match: ${etag}\r\n`)
  assert.equal(second.status, 304)
  assert.equal(second.body.length, 0, 'a 304 carries no body')

  // A stale validator gets the bytes back.
  const stale = await get(conn, '/bootstrap.zip.zst', 'If-None-Match: "stale"\r\n')
  assert.equal(stale.status, 200)
  assert.equal(stale.body.length, first.body.length)
})

test('bootstrap.zip.zst — 503 before the archive has been built', async () => {
  const fx = await makeFixtures({ allowedTargets: [] })
  await rm(join(fx.dataDir, 'bootstrap.zip.zst'))
  await rm(join(fx.dataDir, 'bootstrap.zip'))
  const gw = await spawnGateway(fx)
  try {
    const c = await dial(gw.address)
    // A freshly initialised gateway that has not synced yet must say so rather
    // than serve an empty archive.
    assert.equal((await get(c, '/bootstrap.zip.zst')).status, 503)
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
  }
})

test('relay/random — 503 with an empty allowlist', async () => {
  const fx = await makeFixtures({ allowedTargets: [] })
  const gw = await spawnGateway(fx)
  try {
    const c = await dial(gw.address)
    const res = await get(c, '/relay/random')
    assert.equal(res.status, 503)
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
  }
})

test('CONNECT — the global tunnel cap returns 429 regardless of client', async () => {
  // Only the per-IP cap was covered; this pins the server-wide ceiling.
  const fx = await makeFixtures({ allowedTargets: [`127.0.0.1:${echo.port}`] })
  const gw = await spawnGateway(fx, { config: { tunnel_max: 1, tunnel_per_ip: 16 } })
  try {
    const c = await dial(gw.address)
    assert.equal((await connectTunnel(c, `127.0.0.1:${echo.port}`)).status, 200)
    assert.equal((await connectTunnel(c, `127.0.0.1:${echo.port}`)).status, 429)
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
  }
})

test('CONNECT — an idle tunnel is torn down abortively', async () => {
  const fx = await makeFixtures({ allowedTargets: [`127.0.0.1:${echo.port}`] })
  const gw = await spawnGateway(fx, { config: { tunnel_idle_timeout: 1 } })
  try {
    const c = await dial(gw.address)
    const t = await connectTunnel(c, `127.0.0.1:${echo.port}`)
    assert.equal(t.status, 200)

    // Nothing flows in either direction, so the idle timer expires. §4 maps a
    // timeout to an abortive close, which the client sees as a read error —
    // never as a clean EOF, which would look like the target closing normally.
    const ending = await readUntilEnd(t.reader, 8000, 'idle teardown')
    assert.match(ending, /^error/, `expected an abortive close, got ${ending}`)
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
  }
})

test('CONNECT — a tunnel is torn down at its maximum lifetime', async () => {
  const fx = await makeFixtures({ allowedTargets: [`127.0.0.1:${echo.port}`] })
  const gw = await spawnGateway(fx, {
    config: { tunnel_max_lifetime: 1, tunnel_idle_timeout: 300 },
  })
  try {
    const c = await dial(gw.address)
    const t = await connectTunnel(c, `127.0.0.1:${echo.port}`)
    assert.equal(t.status, 200)

    // Traffic keeps the idle timer from firing, so only the lifetime cap can
    // end this tunnel — again abortively.
    await t.writer.write(enc.encode('still here'))
    const ending = await readUntilEnd(t.reader, 8000, 'lifetime teardown')
    assert.match(ending, /^error/, `expected an abortive close, got ${ending}`)
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
  }
})

test('CONNECT — resetting the stream releases the tunnel slot', async () => {
  // Cancellation has to free capacity; otherwise a client that aborts its
  // tunnels eventually locks itself out.
  const fx = await makeFixtures({ allowedTargets: [`127.0.0.1:${echo.port}`] })
  const gw = await spawnGateway(fx, { config: { tunnel_per_ip: 1 } })
  try {
    const c = await dial(gw.address)
    const t = await connectTunnel(c, `127.0.0.1:${echo.port}`)
    assert.equal(t.status, 200)
    assert.equal((await connectTunnel(c, `127.0.0.1:${echo.port}`)).status, 429, 'cap reached')

    await t.stream.close({ code: 'Reset' })
    // Give the gateway a moment to notice and drop the guard.
    for (let i = 0; i < 40; i++) {
      const retry = await connectTunnel(c, `127.0.0.1:${echo.port}`)
      if (retry.status === 200) {
        await c.close()
        return
      }
      await new Promise(r => setTimeout(r, 50))
    }
    assert.fail('the slot was never released after the stream reset')
  } finally {
    await gw.stop()
    await fx.cleanup()
  }
})

test('CONNECT — losing the connection mid-tunnel releases the slot', async () => {
  const fx = await makeFixtures({ allowedTargets: [`127.0.0.1:${echo.port}`] })
  const gw = await spawnGateway(fx, { config: { tunnel_per_ip: 1 } })
  try {
    const first = await dial(gw.address)
    const t = await connectTunnel(first, `127.0.0.1:${echo.port}`)
    assert.equal(t.status, 200)
    // Drop the whole connection with the tunnel still open.
    await first.close()

    const second = await dial(gw.address)
    for (let i = 0; i < 40; i++) {
      const retry = await connectTunnel(second, `127.0.0.1:${echo.port}`)
      if (retry.status === 200) {
        await second.close()
        return
      }
      await new Promise(r => setTimeout(r, 50))
    }
    assert.fail('the slot was never released after the connection dropped')
  } finally {
    await gw.stop()
    await fx.cleanup()
  }
})
