// Unit tests for the anon-rpc worker's pure helpers
// (src/anon-rpc-worker/helpers.ts): gateway config parsing, bootstrap retry
// pacing, the storage adapter, and the request-init mapping.
//
// The worker entry point itself can't be imported (it runs against the global
// `anonRpcWorker` capability), which is why these live in their own module. The
// hermetic worker test covers only the no-gateway path, so the accepted config
// shapes are pinned here.
//
//   npm run test:unit

import { test, before, describe } from 'node:test'
import assert from 'node:assert/strict'
import { bundleTs } from './bundle.mjs'

let bootstrapBackoff, resolveGateways, toFetchInit, errMsg, makeTorStorage
let BOOTSTRAP_RETRY_BASE_MS, BOOTSTRAP_RETRY_MAX_MS

before(async () => {
  ;({
    bootstrapBackoff,
    resolveGateways,
    toFetchInit,
    errMsg,
    makeTorStorage,
    BOOTSTRAP_RETRY_BASE_MS,
    BOOTSTRAP_RETRY_MAX_MS,
  } = await bundleTs('src/anon-rpc-worker/helpers.ts', 'workerHelpers'))
})

const ADDR = '170.64.236.147:12298:uEiBHwUMNRTetrbqScahm81Di57Xv2OphNrx-CurJGOq3ww'
const ADDR2 = '203.0.113.4:12298:uEiBz7Kw'

describe('resolveGateways', () => {
  test('accepts a single address string', () => {
    assert.deepEqual(resolveGateways(ADDR), [ADDR])
  })

  test('accepts an array of addresses', () => {
    assert.deepEqual(resolveGateways([ADDR, ADDR2]), [ADDR, ADDR2])
  })

  test('accepts the { gateways } object form', () => {
    assert.deepEqual(resolveGateways({ gateways: [ADDR] }), [ADDR])
    assert.deepEqual(resolveGateways({ gateways: ADDR }), [ADDR])
  })

  test('ignores unrelated config keys alongside gateways', () => {
    assert.deepEqual(resolveGateways({ gateways: [ADDR], logLevel: 'info' }), [ADDR])
  })

  // A gateway sees all of the worker's relay traffic, so silently falling back
  // to some default would move that choice out of the deploying app's hands.
  test('refuses every empty or wrongly-typed config', () => {
    for (const config of [
      undefined,
      null,
      '',
      [],
      {},
      { gateways: [] },
      { gateways: '' },
      { gateways: null },
      { gateway: ADDR }, // singular key is not the accepted spelling
      0,
      false,
      true,
      42,
      [ADDR, 42],
      [null],
      [[ADDR]],
      { gateways: [ADDR, { a: 1 }] },
    ]) {
      assert.throws(
        () => resolveGateways(config),
        /no gateway configured/,
        `config ${JSON.stringify(config)}`,
      )
    }
  })

  test('the error tells the operator exactly what to supply', () => {
    assert.throws(() => resolveGateways(undefined), (e) => {
      assert.match(e.message, /WorkerInit\.config/)
      assert.match(e.message, /gateways/)
      assert.match(e.message, /no default gateway/)
      return true
    })
  })

  test('does not validate the address itself', () => {
    // Parsing happens later, in ArtiSocketProvider; this only shapes the config.
    assert.deepEqual(resolveGateways('not-an-address'), ['not-an-address'])
  })
})

describe('bootstrapBackoff', () => {
  test('the first attempt waits about the base delay', () => {
    for (let i = 0; i < 100; i++) {
      const d = bootstrapBackoff(1)
      assert.ok(
        d >= BOOTSTRAP_RETRY_BASE_MS / 2 && d <= BOOTSTRAP_RETRY_BASE_MS,
        `attempt 1 gave ${d}`,
      )
    }
  })

  test('delays grow exponentially, within the jitter band', () => {
    // Each attempt's window is [exp/2, exp] where exp = base·2^(attempt-1).
    for (const attempt of [1, 2, 3, 4, 5, 6]) {
      const exp = Math.min(BOOTSTRAP_RETRY_MAX_MS, BOOTSTRAP_RETRY_BASE_MS * 2 ** (attempt - 1))
      for (let i = 0; i < 50; i++) {
        const d = bootstrapBackoff(attempt)
        assert.ok(d >= exp / 2 && d <= exp, `attempt ${attempt} gave ${d}, expected ≤ ${exp}`)
      }
    }
  })

  test('the delay is capped', () => {
    for (const attempt of [7, 10, 20, 50, 1000]) {
      for (let i = 0; i < 20; i++) {
        const d = bootstrapBackoff(attempt)
        assert.ok(d <= BOOTSTRAP_RETRY_MAX_MS, `attempt ${attempt} gave ${d}`)
        assert.ok(d >= BOOTSTRAP_RETRY_MAX_MS / 2, `attempt ${attempt} gave ${d}`)
      }
    }
  })

  test('a very large attempt count does not overflow into NaN or Infinity', () => {
    const d = bootstrapBackoff(2000)
    assert.ok(Number.isFinite(d), `got ${d}`)
    assert.ok(d <= BOOTSTRAP_RETRY_MAX_MS)
  })

  // Jitter exists so a fleet of workers pointed at one gateway doesn't retry in
  // lockstep after an outage.
  test('successive delays are jittered, not identical', () => {
    const seen = new Set()
    for (let i = 0; i < 200; i++) seen.add(bootstrapBackoff(5))
    assert.ok(seen.size > 20, `only ${seen.size} distinct delays`)
  })

  test('delays are whole milliseconds', () => {
    for (let i = 0; i < 50; i++) assert.ok(Number.isInteger(bootstrapBackoff(3)))
  })
})

describe('toFetchInit', () => {
  test('no init stays undefined', async () => {
    assert.equal(await toFetchInit(undefined), undefined)
  })

  test('an empty init maps to an empty object', async () => {
    assert.deepEqual(await toFetchInit({}), {})
  })

  test('the method is carried across', async () => {
    assert.equal((await toFetchInit({ method: 'POST' })).method, 'POST')
  })

  test('header pairs become a record', async () => {
    const out = await toFetchInit({
      headers: [
        ['content-type', 'application/json'],
        ['x-custom', 'v'],
      ],
    })
    assert.deepEqual(out.headers, { 'content-type': 'application/json', 'x-custom': 'v' })
  })

  test('a repeated header name keeps the last value', async () => {
    // A record cannot hold both; the mapping has to pick one deterministically.
    const out = await toFetchInit({ headers: [['x', '1'], ['x', '2']] })
    assert.deepEqual(out.headers, { x: '2' })
  })

  test('an empty header list still produces a record', async () => {
    assert.deepEqual((await toFetchInit({ headers: [] })).headers, {})
  })

  test('a bytes body is forwarded unchanged', async () => {
    const body = new Uint8Array([1, 2, 3])
    const out = await toFetchInit({ body })
    assert.equal(out.body, body, 'forwarded by reference, never copied')
  })

  test('a stream body is forwarded without being drained', async () => {
    // Buffering here would defeat streaming uploads end to end.
    const body = new ReadableStream({ start(c) { c.enqueue(new Uint8Array([1])); c.close() } })
    const out = await toFetchInit({ body })
    assert.equal(out.body, body)
    assert.equal(body.locked, false, 'must not have been read')
  })

  test('an empty body is still forwarded', async () => {
    const body = new Uint8Array(0)
    assert.equal((await toFetchInit({ body })).body, body)
    assert.ok('body' in (await toFetchInit({ body })))
  })

  test('an abort signal is passed through', async () => {
    const signal = AbortSignal.abort()
    assert.equal((await toFetchInit({ signal })).signal, signal)
  })

  test('absent fields are left out rather than set to undefined', async () => {
    const out = await toFetchInit({ method: 'GET' })
    assert.deepEqual(Object.keys(out), ['method'])
  })

  test("anon-rpc's redirect is not forwarded", async () => {
    // tor-js's FetchInit has no redirect field; silently dropping it is the
    // documented behaviour.
    const out = await toFetchInit({ method: 'GET', redirect: 'follow' })
    assert.ok(!('redirect' in out))
  })
})

describe('makeTorStorage', () => {
  /** Minimal in-memory stand-in for the host's byte-valued StorageApi. */
  function hostStorage(initial = {}) {
    const map = new Map(Object.entries(initial).map(([k, v]) => [k, new TextEncoder().encode(v)]))
    return {
      map,
      async get(k) { return map.get(k) ?? null },
      async set(k, v) { map.set(k, v) },
      async delete(k) { map.delete(k) },
      async *list({ prefix }) {
        for (const k of [...map.keys()].sort()) if (k.startsWith(prefix)) yield k
      },
    }
  }

  test('strings are encoded on the way in and decoded on the way out', async () => {
    const host = hostStorage()
    const s = makeTorStorage(host)

    await s.set('k', 'héllo €')
    assert.ok(host.map.get('k') instanceof Uint8Array, 'stored as bytes')
    assert.equal(await s.get('k'), 'héllo €')
  })

  test('a missing key reads as null', async () => {
    assert.equal(await makeTorStorage(hostStorage()).get('absent'), null)
  })

  test('delete removes the entry', async () => {
    const host = hostStorage({ k: 'v' })
    const s = makeTorStorage(host)
    await s.delete('k')
    assert.equal(await s.get('k'), null)
  })

  test('keys() lists by prefix', async () => {
    const s = makeTorStorage(hostStorage({ 'dir:a': '1', 'dir:b': '2', 'other': '3' }))
    assert.deepEqual(await s.keys('dir:'), ['dir:a', 'dir:b'])
    assert.deepEqual(await s.keys(''), ['dir:a', 'dir:b', 'other'])
    assert.deepEqual(await s.keys('nope'), [])
  })

  test('getAll() pairs keys with decoded values', async () => {
    const s = makeTorStorage(hostStorage({ 'dir:a': '1', 'dir:b': '2', 'other': '3' }))
    assert.deepEqual(await s.getAll('dir:'), [['dir:a', '1'], ['dir:b', '2']])
  })

  // The adapter guards on the buffer being truthy to tell a miss from a hit. An
  // empty Uint8Array is still an object, so a zero-length value survives rather
  // than reading back as a missing key.
  test('an empty stored value is not confused with a missing key', async () => {
    const s = makeTorStorage(hostStorage())
    await s.set('empty', '')
    assert.deepEqual(await s.keys(''), ['empty'])
    assert.equal(await s.get('empty'), '')
    assert.deepEqual(await s.getAll(''), [['empty', '']])
  })

  test('the writer lock is a no-op: the worker owns its own store', async () => {
    const s = makeTorStorage(hostStorage())
    assert.equal(await s.tryLock(), true)
    assert.equal(await s.tryLock(), true, 'never contended')
    assert.equal(await s.unlock(), undefined)
  })
})

describe('errMsg', () => {
  test('uses an Error message', () => {
    assert.equal(errMsg(new Error('boom')), 'boom')
    assert.equal(errMsg(new TypeError('bad type')), 'bad type')
  })

  test('stringifies anything else', () => {
    assert.equal(errMsg('plain string'), 'plain string')
    assert.equal(errMsg(42), '42')
    assert.equal(errMsg(null), 'null')
    assert.equal(errMsg(undefined), 'undefined')
    assert.equal(errMsg({ code: 1 }), '[object Object]')
  })
})
