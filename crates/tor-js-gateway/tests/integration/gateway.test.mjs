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
  startFakeGitHub,
  exchange,
  get,
  connectTunnel,
  runCli,
  waitForLog,
  waitForMirrorObjects,
} from './helpers.mjs'

const enc = new TextEncoder()

let fixtures, echo, gateway, conn, github, bundle

before(async () => {
  echo = await startEcho()
  // One object published on the mirrored branch before the gateway starts, so
  // the shared gateway has something to serve at /keccak/.
  github = await startFakeGitHub()
  bundle = github.publish(`export const fixture = '${Date.now()}'\n`)
  fixtures = await makeFixtures({ allowedTargets: [`127.0.0.1:${echo.port}`] })
  gateway = await spawnGateway(fixtures, { github })
  conn = await dial(gateway.address)
  await waitForMirrorObjects(conn, 1)
})

after(async () => {
  await conn?.close()
  await gateway?.stop()
  await echo?.close()
  await fixtures?.cleanup()
  await github?.close()
})

test('metadata.json — capability discovery', async () => {
  const res = await get(conn, '/metadata.json')
  assert.equal(res.status, 200)
  assert.equal(res.headers['content-type'], 'application/json')
  assert.equal(res.headers['content-length'], String(res.body.length))
  const meta = JSON.parse(res.body.toString())
  assert.equal(meta.protocol, 'kps-http/1')
  assert.equal(meta.software, 'tor-js-gateway')
  for (const cap of [
    'metadata',
    'bootstrap',
    'connect',
    'relay-random',
    'worker-bundles',
    'worker-bundles-sync',
  ]) {
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

test('worker bundle — a mirrored object is immutable and length-delimited', async () => {
  const h = bundle.hash
  const res = await get(conn, `/keccak/${h.slice(0, 2)}/${h.slice(2)}`)
  assert.equal(res.status, 200)
  assert.equal(res.headers['content-type'], 'text/javascript')
  assert.equal(res.headers['cache-control'], 'public, max-age=31536000, immutable')
  assert.equal(res.headers['content-length'], String(res.body.length))
  assert.deepEqual(res.body, bundle.bytes)
})

test('worker bundle — unknown hash and malformed paths are 404', async () => {
  assert.equal((await get(conn, `/keccak/11/${'1'.repeat(62)}`)).status, 404)
  assert.equal((await get(conn, `/keccak/aa/${'a'.repeat(61)}`)).status, 404) // too short
  assert.equal((await get(conn, `/keccak/AA/${'a'.repeat(62)}`)).status, 404) // uppercase
  assert.equal((await get(conn, `/keccak/${'a'.repeat(64)}`)).status, 404) // unsharded
  assert.equal((await get(conn, `/keccak/a/${'a'.repeat(63)}`)).status, 404) // bad split
  const h = bundle.hash
  assert.equal((await get(conn, `/keccak/${h.slice(0, 2)}/${h.slice(2)}.js`)).status, 404) // extension
  assert.equal((await get(conn, `/worker/${h}.js`)).status, 404) // old route is gone
})

test('mirror status — GET /keccak/sync reports the tracked branch', async () => {
  const res = await get(conn, '/keccak/sync')
  assert.equal(res.status, 200)
  assert.equal(res.headers['content-type'], 'application/json')
  const status = JSON.parse(res.body.toString())
  assert.equal(status.repo, github.repo)
  assert.equal(status.branch, github.branch)
  assert.equal(status.commit, github.commit)
  assert.equal(status.objects, 1)
  assert.equal(status.last_error, null)
  assert.ok(status.last_success, 'a successful sync is recorded')
})

test('mirror — the branch is the only source: non-object paths are ignored', async () => {
  // A README in the branch must not break a sync or become an object.
  github.publishAt('README.md', '# objects\n')
  const res = await exchange(conn, 'POST /keccak/sync HTTP/1.1\r\nHost: x\r\n\r\n')
  assert.equal(res.status, 200)
  const outcome = JSON.parse(res.body.toString())
  assert.equal(outcome.added, 0)
  assert.equal(outcome.removed, 0)
  assert.equal(outcome.objects, 1)
  assert.ok(outcome.ignored >= 1, `ignored: ${outcome.ignored}`)
})

test('mirror — a client trigger is refused inside the throttle window', async () => {
  // The previous test consumed the window (default 1800 s), and the window is
  // what bounds how often anyone can make this gateway talk to its origin.
  const res = await exchange(conn, 'POST /keccak/sync HTTP/1.1\r\nHost: x\r\n\r\n')
  assert.equal(res.status, 429)
  const retryAfter = Number(res.headers['retry-after'])
  assert.ok(retryAfter > 1700 && retryAfter <= 1801, `retry-after: ${retryAfter}`)
  assert.equal(JSON.parse(res.body.toString()).retry_after, retryAfter)
})

test('mirror — a trigger picks up a newly published object', async () => {
  const gh = await startFakeGitHub()
  const first = gh.publish(`export const one = '${Date.now()}'\n`)
  const fx = await makeFixtures({ allowedTargets: [] })
  // No throttle: this test needs more than one trigger.
  const gw = await spawnGateway(fx, {
    github: gh,
    config: { keccak_manual_sync_min_interval: 0 },
  })
  try {
    const c = await dial(gw.address)
    await waitForMirrorObjects(c, 1)

    const second = gh.publish(`export const two = '${Date.now()}'\n`)
    // Not served until a sync notices it — the poll is a day away.
    assert.equal(
      (await get(c, `/keccak/${second.hash.slice(0, 2)}/${second.hash.slice(2)}`)).status,
      404
    )

    const res = await exchange(c, 'POST /keccak/sync HTTP/1.1\r\nHost: x\r\n\r\n')
    assert.equal(res.status, 200)
    const outcome = JSON.parse(res.body.toString())
    assert.equal(outcome.added, 1)
    assert.equal(outcome.objects, 2)
    assert.equal(outcome.commit, gh.commit)
    assert.equal(outcome.unchanged, false)

    // Both objects are now served, byte for byte.
    for (const o of [first, second]) {
      const got = await get(c, `/keccak/${o.hash.slice(0, 2)}/${o.hash.slice(2)}`)
      assert.equal(got.status, 200)
      assert.deepEqual(got.body, o.bytes)
    }
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
    await gh.close()
  }
})

test('mirror — dropping a file from the branch unpublishes the object', async () => {
  const gh = await startFakeGitHub()
  const keep = gh.publish('export const keep = 1\n')
  const drop = gh.publish('export const drop = 1\n')
  const fx = await makeFixtures({ allowedTargets: [] })
  const gw = await spawnGateway(fx, {
    github: gh,
    config: { keccak_manual_sync_min_interval: 0 },
  })
  try {
    const c = await dial(gw.address)
    await waitForMirrorObjects(c, 2)

    gh.unpublish(drop.path)
    const res = await exchange(c, 'POST /keccak/sync HTTP/1.1\r\nHost: x\r\n\r\n')
    assert.equal(res.status, 200)
    const outcome = JSON.parse(res.body.toString())
    assert.equal(outcome.removed, 1)
    assert.equal(outcome.objects, 1)

    assert.equal(
      (await get(c, `/keccak/${drop.hash.slice(0, 2)}/${drop.hash.slice(2)}`)).status,
      404,
      'an unpublished object stops being served'
    )
    assert.equal(
      (await get(c, `/keccak/${keep.hash.slice(0, 2)}/${keep.hash.slice(2)}`)).status,
      200
    )
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
    await gh.close()
  }
})

test('mirror — a file whose contents do not hash to its name is never stored', async () => {
  const gh = await startFakeGitHub()
  const good = gh.publish('export const good = 1\n')
  // Same shape, wrong contents: the branch is lying about what this object is.
  const liar = `ab/${'c'.repeat(62)}`
  gh.publishAt(liar, 'export const evil = 1\n')
  const fx = await makeFixtures({ allowedTargets: [] })
  const gw = await spawnGateway(fx, { github: gh })
  try {
    const c = await dial(gw.address)
    await waitForMirrorObjects(c, 1)
    // The good object landed; the liar was refused, and refusing it did not
    // stop the rest of the sync.
    assert.equal(
      (await get(c, `/keccak/${good.hash.slice(0, 2)}/${good.hash.slice(2)}`)).status,
      200
    )
    assert.equal((await get(c, `/keccak/${liar}`)).status, 404)
    await waitForLog(gw, /keccak256 of the contents is/, 10_000)
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
    await gh.close()
  }
})

test('mirror — an oversized object is skipped, not fetched', async () => {
  const gh = await startFakeGitHub()
  const small = gh.publish('export const small = 1\n')
  const big = gh.publish('export const big = 1\n')
  gh.claimSize(big.path, 65 * 1024 * 1024) // over the 64 MiB object cap
  const fx = await makeFixtures({ allowedTargets: [] })
  const gw = await spawnGateway(fx, { github: gh })
  try {
    const c = await dial(gw.address)
    await waitForMirrorObjects(c, 1)
    assert.equal(
      (await get(c, `/keccak/${small.hash.slice(0, 2)}/${small.hash.slice(2)}`)).status,
      200
    )
    assert.equal(
      (await get(c, `/keccak/${big.hash.slice(0, 2)}/${big.hash.slice(2)}`)).status,
      404
    )
    // Skipped on the listing alone: its bytes were never requested.
    assert.ok(
      !gh.requests.some(r => r.path.endsWith(big.path)),
      'the oversized blob must not be downloaded'
    )
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
    await gh.close()
  }
})

test('mirror — a truncated tree listing fails the sync instead of pruning', async () => {
  const gh = await startFakeGitHub()
  const obj = gh.publish('export const kept = 1\n')
  const fx = await makeFixtures({ allowedTargets: [] })
  const gw = await spawnGateway(fx, {
    github: gh,
    config: { keccak_manual_sync_min_interval: 0 },
  })
  try {
    const c = await dial(gw.address)
    await waitForMirrorObjects(c, 1)

    // A truncated listing is not a smaller branch: acting on one would delete
    // objects that are still published.
    gh.setTruncated(true)
    const res = await exchange(c, 'POST /keccak/sync HTTP/1.1\r\nHost: x\r\n\r\n')
    assert.equal(res.status, 502)
    assert.match(JSON.parse(res.body.toString()).error, /truncated/)

    assert.equal(
      (await get(c, `/keccak/${obj.hash.slice(0, 2)}/${obj.hash.slice(2)}`)).status,
      200,
      'a failed sync keeps serving what was already mirrored'
    )
    const status = JSON.parse((await get(c, '/keccak/sync')).body.toString())
    assert.match(status.last_error, /truncated/)
    assert.equal(status.objects, 1)
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
    await gh.close()
  }
})

test('mirror — an unreachable origin leaves the mirrored objects served', async () => {
  const gh = await startFakeGitHub()
  const obj = gh.publish('export const survives = 1\n')
  const fx = await makeFixtures({ allowedTargets: [] })
  const gw = await spawnGateway(fx, {
    github: gh,
    config: { keccak_manual_sync_min_interval: 0 },
  })
  try {
    const c = await dial(gw.address)
    await waitForMirrorObjects(c, 1)
    await gh.close() // the origin goes away

    const res = await exchange(c, 'POST /keccak/sync HTTP/1.1\r\nHost: x\r\n\r\n')
    assert.equal(res.status, 502, 'a failed sync is a bad-gateway, not a 500')
    assert.equal(
      (await get(c, `/keccak/${obj.hash.slice(0, 2)}/${obj.hash.slice(2)}`)).status,
      200
    )
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
  }
})

test('worker bundle — a mismatched file on disk is refused (and logged) on request', async () => {
  // The mirror never lands such a file, and would prune it; --no-mirror is how
  // one gets to exist, and the route's own check is what catches it.
  const gh = await startFakeGitHub()
  const fx = await makeFixtures({ allowedTargets: [] })
  await fx.seedRawObject('00' + '0'.repeat(62), '// wrong hash\n')
  const { hash, bytes } = await fx.seedObject('export const honest = 1\n')
  const gw = await spawnGateway(fx, { github: gh, args: ['--no-mirror'] })
  try {
    const c = await dial(gw.address)
    // Verification is lazy: nothing is logged until the bad path is requested.
    assert.equal((await get(c, `/keccak/00/${'0'.repeat(62)}`)).status, 404)
    await waitForLog(gw, /REFUSING .*00\/0{62}/)
    // The correctly-named neighbour is unaffected.
    const ok = await get(c, `/keccak/${hash.slice(0, 2)}/${hash.slice(2)}`)
    assert.equal(ok.status, 200)
    assert.deepEqual(ok.body, bytes)
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
    await gh.close()
  }
})

test('mirror — --no-mirror refuses triggers rather than contacting the branch', async () => {
  const gh = await startFakeGitHub()
  gh.publish('export const unseen = 1\n')
  const fx = await makeFixtures({ allowedTargets: [] })
  const gw = await spawnGateway(fx, { github: gh, args: ['--no-mirror'] })
  try {
    const c = await dial(gw.address)
    const res = await exchange(c, 'POST /keccak/sync HTTP/1.1\r\nHost: x\r\n\r\n')
    assert.equal(res.status, 503)
    assert.match(JSON.parse(res.body.toString()).error, /disabled/)
    assert.deepEqual(gh.requests, [], 'the branch must not be contacted at all')
    // Status still answers, so an operator can see what is (not) happening.
    assert.equal((await get(c, '/keccak/sync')).status, 200)
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
    await gh.close()
  }
})

test('sync CLI — triggers over KPS, reports state, and exits non-zero when refused', async () => {
  const gh = await startFakeGitHub()
  const first = gh.publish('export const cli = 1\n')
  const fx = await makeFixtures({ allowedTargets: [] })
  const gw = await spawnGateway(fx, {
    github: gh,
    config: { keccak_manual_sync_min_interval: 1800 },
  })
  try {
    const c = await dial(gw.address)
    await waitForMirrorObjects(c, 1)
    await c.close()

    // --status reads without triggering, and with no ADDRESS the subcommand
    // derives the local gateway's address from the config's port and key.
    const status = await runCli(['--config', gw.configPath, 'sync', '--status'])
    assert.equal(status.code, 0, status.stderr)
    const state = JSON.parse(status.stdout)
    assert.equal(state.repo, gh.repo)
    assert.equal(state.branch, gh.branch)
    assert.equal(state.objects, 1)

    // A trigger picks up a push, and prints the summary the gateway returned.
    const second = gh.publish('export const cli2 = 1\n')
    const synced = await runCli(['--config', gw.configPath, 'sync'])
    assert.equal(synced.code, 0, synced.stderr)
    const outcome = JSON.parse(synced.stdout)
    assert.equal(outcome.added, 1)
    assert.equal(outcome.objects, 2)
    assert.equal(outcome.commit, gh.commit)

    // Both objects really are being served afterwards.
    const c2 = await dial(gw.address)
    for (const o of [first, second]) {
      assert.equal(
        (await get(c2, `/keccak/${o.hash.slice(0, 2)}/${o.hash.slice(2)}`)).status,
        200
      )
    }
    await c2.close()

    // Refusals must be visible to a script, not just to a reader.
    const throttled = await runCli(['--config', gw.configPath, 'sync'])
    assert.equal(throttled.code, 1, 'a 429 has to be a non-zero exit')
    assert.match(throttled.stderr, /429/)
    assert.equal(JSON.parse(throttled.stdout).retry_after > 0, true)

    // An explicit address is the remote-gateway form.
    const remote = await runCli(['--config', gw.configPath, 'sync', gw.address, '--status'])
    assert.equal(remote.code, 0, remote.stderr)
    assert.equal(JSON.parse(remote.stdout).objects, 2)
  } finally {
    await gw.stop()
    await fx.cleanup()
    await gh.close()
  }
})

test('sync CLI — says so when the gateway serves no bundles', async () => {
  const fx = await makeFixtures({ allowedTargets: [] })
  const gw = await spawnGateway(fx) // no github: capability off
  try {
    const res = await runCli(['--config', gw.configPath, 'sync'])
    assert.equal(res.code, 1)
    // Caught from the config before dialing, so the message names the fields.
    assert.match(res.stderr, /keccak_repo/)
  } finally {
    await gw.stop()
    await fx.cleanup()
  }
})

test('worker bundles — unconfigured means the capability is simply absent', async () => {
  // No keccak_repo/keccak_branch: the routes are gone and nothing is advertised.
  const fx = await makeFixtures({ allowedTargets: [] })
  const gw = await spawnGateway(fx)
  try {
    const c = await dial(gw.address)
    const meta = JSON.parse((await get(c, '/metadata.json')).body.toString())
    assert.ok(!meta.capabilities.includes('worker-bundles'))
    assert.ok(!meta.capabilities.includes('worker-bundles-sync'))
    assert.equal((await get(c, `/keccak/aa/${'a'.repeat(62)}`)).status, 404)
    assert.equal((await get(c, '/keccak/sync')).status, 404)
    assert.equal(
      (await exchange(c, 'POST /keccak/sync HTTP/1.1\r\nHost: x\r\n\r\n')).status,
      404
    )
    await c.close()
  } finally {
    await gw.stop()
    await fx.cleanup()
  }
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
