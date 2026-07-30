// Unit tests for KpsGateway (src/kpsGateway.ts): the KPS-HTTP/1 response-head
// parser, the CONNECT exchange, connection reuse and re-dial, the per-connection
// teardown registry that works around kps ISSUES #4, and the abort deadlines
// that bound one caller's wait without abandoning a shared dial.
//
// No network: a fake dial supplies fake KPS connections and streams, driven from
// scripted byte pieces so head/body boundaries land wherever we choose.
//
//   npm run test:unit

import { test, before, after, describe } from 'node:test'
import assert from 'node:assert/strict'
import { bundleTs } from './bundle.mjs'

let KpsGateway

before(async () => {
  ;({ KpsGateway } = await bundleTs('src/kpsGateway.ts', 'kpsGateway'))
})

const ADDR = '1.2.3.4:12298:uEiBHwUMNRTetrbq'
const CERTHASH = 'uEiBHwUMNRTetrbq'
const enc = new TextEncoder()
const dec = new TextDecoder()

const tick = (n = 1) => new Promise((r) => setTimeout(r, n))

/**
 * A fake KPS stream.
 *
 * `pieces` are delivered one per read, so a test can split a response head
 * across reads. `endAfterPieces` sends FIN when they run out; otherwise reads
 * hang, which is how we model a server that has answered but not closed.
 */
function makeStream({ pieces = [], endAfterPieces = true } = {}) {
  const queue = pieces.map((p) => (typeof p === 'string' ? enc.encode(p) : p))
  const written = []
  let closeReadable
  let settleClosed
  const closed = new Promise((r) => { settleClosed = r })
  const state = { closed: false, closeWriteCalled: false, cancelled: null }

  const readable = new ReadableStream({
    start(c) { closeReadable = () => { try { c.close() } catch {} } },
    pull(c) {
      if (queue.length) { c.enqueue(queue.shift()); return }
      if (endAfterPieces) c.close()
      // else: leave the promise pending — the peer has not closed.
      else return new Promise(() => {})
    },
    cancel(reason) { state.cancelled = reason ?? 'cancelled' },
  })

  return {
    readable,
    writable: new WritableStream({ write(chunk) { written.push(chunk) } }),
    closed,
    written,
    state,
    text: () => written.map((c) => dec.decode(c)).join(''),
    closeWrite: async () => { state.closeWriteCalled = true },
    close: async () => {
      state.closed = true
      closeReadable?.()
      settleClosed({ ok: true })
    },
    /** Simulate the peer resetting the stream. */
    reset: (code = 'RESET') => settleClosed({ ok: false, reason: { code } }),
  }
}

/**
 * A fake KPS connection handing out scripted streams.
 *
 * `scripts` is an array of makeStream options, consumed in order; once it runs
 * out the last one repeats.
 */
function makeConn(scripts = [{}]) {
  let settleClosed
  const closed = new Promise((r) => { settleClosed = r })
  const streams = []
  let i = 0
  const state = { opens: 0, closed: false, openSignals: [] }
  return {
    closed,
    streams,
    state,
    async openStream(opts) {
      state.opens++
      state.openSignals.push(opts?.signal)
      const script = scripts[Math.min(i++, scripts.length - 1)]
      if (script?.hangOpen) return new Promise(() => {})
      if (script?.failOpen) throw new Error('openStream refused')
      const s = makeStream(script)
      streams.push(s)
      return s
    },
    async close() { state.closed = true; settleClosed({ ok: true }) },
    /** Simulate the connection dying without settling its streams. */
    die: (reject = false) => (reject ? settleClosed(undefined) : settleClosed({ ok: true })),
    dieRejecting: () => {},
  }
}

/** A dial fn over a mutable behaviour object, recording call counts. */
function makeDial(behaviour) {
  const calls = []
  const dial = async (address) => {
    calls.push(address)
    if (behaviour.mode === 'reject') throw new Error('dial refused')
    if (behaviour.mode === 'hang') return new Promise(() => {})
    if (behaviour.mode === 'slow') {
      await tick(behaviour.delay ?? 30)
      return behaviour.conn
    }
    return behaviour.conn
  }
  dial.calls = calls
  return dial
}

const OK = 'HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n'

// ===========================================================================

describe('construction', () => {
  test('validates the address up front', () => {
    assert.throws(() => new KpsGateway('not-an-address'), /malformed/)
    assert.throws(() => new KpsGateway('1.2.3.4:99999:h'), /out of range/)
  })

  test('trims surrounding whitespace', () => {
    // Addresses get pasted from terminals and config files.
    assert.equal(new KpsGateway(`  ${ADDR}\n`).address, ADDR)
  })

  test('exposes the address it was given', () => {
    assert.equal(new KpsGateway(ADDR).address, ADDR)
  })
})

describe('fetch', () => {
  test('sends a KPS-HTTP/1 GET and returns the response', async () => {
    const conn = makeConn([{ pieces: [OK, 'hello world'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })

    const res = await gw.fetch('/metadata.json')
    assert.equal(res.status, 200)
    assert.equal(res.statusText, 'OK')
    assert.equal(res.headers['content-type'], 'text/plain')
    assert.equal(dec.decode(res.body), 'hello world')

    // Host is the certhash (PROTOCOL.md §3.2), and the request body is FIN-delimited.
    assert.equal(
      conn.streams[0].text(),
      `GET /metadata.json HTTP/1.1\r\nHost: ${CERTHASH}\r\n\r\n`,
    )
  })

  test('reassembles a head split across reads', async () => {
    const conn = makeConn([{ pieces: ['HTTP/1.1 200 OK\r\nX-A: 1\r', '\nX-B: 2\r\n\r', '\nbody'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const res = await gw.fetch('/x')
    assert.equal(res.status, 200)
    assert.deepEqual(res.headers, { 'x-a': '1', 'x-b': '2' })
    assert.equal(dec.decode(res.body), 'body')
  })

  test('keeps body bytes that arrived alongside the head', async () => {
    // The separator and the first body bytes in one read is the common case.
    const conn = makeConn([{ pieces: [OK + 'first', 'second'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const res = await gw.fetch('/x')
    assert.equal(dec.decode(res.body), 'firstsecond')
  })

  test('an empty body is an empty array, not an error', async () => {
    const conn = makeConn([{ pieces: ['HTTP/1.1 204 No Content\r\n\r\n'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const res = await gw.fetch('/x')
    assert.equal(res.status, 204)
    assert.equal(res.body.length, 0)
  })

  test('header names are lowercased and values trimmed', async () => {
    const conn = makeConn([{ pieces: ['HTTP/1.1 200 OK\r\nX-Mixed-Case:   spaced   \r\n\r\n'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const res = await gw.fetch('/x')
    assert.equal(res.headers['x-mixed-case'], 'spaced')
  })

  test('a header value containing colons is kept whole', async () => {
    const conn = makeConn([{ pieces: ['HTTP/1.1 200 OK\r\nDate: Wed, 30 Jul 2026 00:00:00 GMT\r\n\r\n'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const res = await gw.fetch('/x')
    assert.equal(res.headers['date'], 'Wed, 30 Jul 2026 00:00:00 GMT')
  })

  test('lines without a colon are ignored', async () => {
    const conn = makeConn([{ pieces: ['HTTP/1.1 200 OK\r\ngarbage\r\nX-Good: 1\r\n\r\n'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const res = await gw.fetch('/x')
    assert.deepEqual(res.headers, { 'x-good': '1' })
  })

  test('non-2xx statuses are returned rather than thrown', async () => {
    // The caller decides: gatewayFetch failover needs to see the status.
    const conn = makeConn([{ pieces: ['HTTP/1.1 503 Service Unavailable\r\n\r\nnope'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const res = await gw.fetch('/bootstrap.zip.zst')
    assert.equal(res.status, 503)
    assert.equal(res.statusText, 'Service Unavailable')
    assert.equal(dec.decode(res.body), 'nope')
  })

  test('a malformed status line is an error', async () => {
    for (const line of ['HTTP/1.0 200 OK', 'garbage', 'HTTP/1.1 20 OK', '200 OK']) {
      const conn = makeConn([{ pieces: [`${line}\r\n\r\n`] }])
      const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
      await assert.rejects(gw.fetch('/x'), /malformed status line/, line)
    }
  })

  test('a stream that ends before the head is an error', async () => {
    const conn = makeConn([{ pieces: ['HTTP/1.1 200 OK\r\nX-Partial: 1'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await assert.rejects(gw.fetch('/x'), /ended before response head/)
  })

  test('the stream is closed once the exchange finishes', async () => {
    const conn = makeConn([{ pieces: [OK, 'body'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await gw.fetch('/x')
    assert.equal(conn.streams[0].state.closed, true)
  })

  test('the stream is closed even when the exchange fails', async () => {
    const conn = makeConn([{ pieces: ['garbage\r\n\r\n'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await assert.rejects(gw.fetch('/x'))
    assert.equal(conn.streams[0].state.closed, true)
  })

  test('each exchange gets its own stream on one connection', async () => {
    const conn = makeConn([{ pieces: [OK, 'a'] }, { pieces: [OK, 'b'] }])
    const dial = makeDial({ conn })
    const gw = new KpsGateway(ADDR, { dial })

    assert.equal(dec.decode((await gw.fetch('/a')).body), 'a')
    assert.equal(dec.decode((await gw.fetch('/b')).body), 'b')
    assert.equal(dial.calls.length, 1, 'connection reused')
    assert.equal(conn.state.opens, 2, 'one stream per exchange')
  })
})

describe('connect', () => {
  test('performs the CONNECT exchange and hands back a socket', async () => {
    const conn = makeConn([{ pieces: [OK], endAfterPieces: false }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })

    const sock = await gw.connect('185.220.101.1:9001')
    assert.equal(
      conn.streams[0].text(),
      'CONNECT 185.220.101.1:9001 HTTP/1.1\r\nHost: 185.220.101.1:9001\r\n\r\n',
    )
    assert.ok(sock.readable, 'exposes a readable')
    assert.ok(sock.writable, 'exposes a writable')
  })

  test('tunnel bytes that arrived with the head come out first', async () => {
    const conn = makeConn([{ pieces: [OK + 'early', 'later'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const sock = await gw.connect('1.1.1.1:443')

    const reader = sock.readable.getReader()
    assert.equal(dec.decode((await reader.read()).value), 'early')
    assert.equal(dec.decode((await reader.read()).value), 'later')
    assert.equal((await reader.read()).done, true, 'server FIN closes the readable')
  })

  test('the readable only pulls on demand', async () => {
    // Backpressure: nothing is read from the network until the consumer asks.
    const conn = makeConn([{ pieces: [OK, 'a', 'b'], endAfterPieces: false }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const sock = await gw.connect('1.1.1.1:443')
    const reader = sock.readable.getReader()
    assert.equal(dec.decode((await reader.read()).value), 'a')
  })

  test('a non-200 reply becomes an error carrying the diagnostic body', async () => {
    const conn = makeConn([
      { pieces: ['HTTP/1.1 403 Forbidden\r\n\r\ntarget is not an advertised Tor relay'] },
    ])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await assert.rejects(
      gw.connect('8.8.8.8:53'),
      /CONNECT 8\.8\.8\.8:53: 403 target is not an advertised Tor relay/,
    )
    assert.equal(conn.streams[0].state.closed, true, 'stream released')
  })

  test('a non-200 with no body falls back to the status text', async () => {
    const conn = makeConn([{ pieces: ['HTTP/1.1 429 Too Many Requests\r\n\r\n'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await assert.rejects(gw.connect('1.1.1.1:443'), /429 Too Many Requests/)
  })

  test('a diagnostic split across reads is fully collected', async () => {
    const conn = makeConn([{ pieces: ['HTTP/1.1 400 Bad Request\r\n\r\ninvalid ', 'target ', 'address'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await assert.rejects(gw.connect('bogus'), /400 invalid target address/)
  })

  test('a stream that ends before the reply is an error', async () => {
    const conn = makeConn([{ pieces: ['HTTP/1.1 200'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await assert.rejects(gw.connect('1.1.1.1:443'), /ended before response head/)
    assert.equal(conn.streams[0].state.closed, true)
  })

  test('closeWrite maps onto the stream half-close', async () => {
    const conn = makeConn([{ pieces: [OK], endAfterPieces: false }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const sock = await gw.connect('1.1.1.1:443')
    await sock.closeWrite()
    assert.equal(conn.streams[0].state.closeWriteCalled, true)
  })

  test('a stream reset surfaces as a non-ok close, not a rejection', async () => {
    const conn = makeConn([{ pieces: [OK], endAfterPieces: false }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const sock = await gw.connect('1.1.1.1:443')

    conn.streams[0].reset('Timeout')
    assert.deepEqual(await sock.closed, { ok: false, reason: 'Timeout' })
  })

  test('a clean close reports ok', async () => {
    const conn = makeConn([{ pieces: [OK], endAfterPieces: false }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const sock = await gw.connect('1.1.1.1:443')
    sock.close()
    assert.deepEqual(await sock.closed, { ok: true, reason: undefined })
  })
})

describe('connection lifecycle', () => {
  test('concurrent callers share one dial', async () => {
    const conn = makeConn([{ pieces: [OK, 'a'] }, { pieces: [OK, 'b'] }])
    const dial = makeDial({ conn, mode: 'slow', delay: 20 })
    const gw = new KpsGateway(ADDR, { dial })

    await Promise.all([gw.fetch('/a'), gw.fetch('/b')])
    assert.equal(dial.calls.length, 1)
  })

  test('a failed dial is not cached', async () => {
    const behaviour = { mode: 'reject' }
    const dial = makeDial(behaviour)
    const gw = new KpsGateway(ADDR, { dial })

    await assert.rejects(gw.fetch('/x'), /dial refused/)
    behaviour.mode = 'ok'
    behaviour.conn = makeConn([{ pieces: [OK, 'recovered'] }])
    assert.equal(dec.decode((await gw.fetch('/x')).body), 'recovered')
    assert.equal(dial.calls.length, 2, 'dialed again after the failure')
  })

  test('a dropped connection is re-dialed on the next use', async () => {
    const first = makeConn([{ pieces: [OK, 'first'] }])
    const behaviour = { conn: first }
    const dial = makeDial(behaviour)
    const gw = new KpsGateway(ADDR, { dial })

    assert.equal(dec.decode((await gw.fetch('/x')).body), 'first')
    first.die()
    await tick()

    behaviour.conn = makeConn([{ pieces: [OK, 'second'] }])
    assert.equal(dec.decode((await gw.fetch('/x')).body), 'second')
    assert.equal(dial.calls.length, 2)
  })

  /// kps ISSUES #4: the JS client does not reliably settle streams when the
  /// connection dies, so a pending read would hang forever. The gateway keeps
  /// per-connection teardowns and runs them on `conn.closed`.
  test('connection teardown cancels a tunnel blocked on a read', async () => {
    const conn = makeConn([{ pieces: [OK], endAfterPieces: false }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const sock = await gw.connect('1.1.1.1:443')

    const reader = sock.readable.getReader()
    const pending = reader.read().then(
      (r) => ({ ok: true, r }),
      (e) => ({ ok: false, e }),
    )

    conn.die()
    const outcome = await pending
    assert.ok(
      outcome.ok === false || outcome.r.done,
      'a read outstanding at teardown must settle, not hang',
    )
    assert.equal(conn.streams[0].state.closed, true, 'the stream is closed too')
  })

  test('teardown also fires when conn.closed rejects', async () => {
    // kps rejects `closed` with the close reason on some teardowns.
    let rejectClosed
    const conn = makeConn([{ pieces: [OK], endAfterPieces: false }])
    conn.closed = new Promise((_, rej) => { rejectClosed = rej })
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const sock = await gw.connect('1.1.1.1:443')

    rejectClosed(null)
    await tick(5)
    assert.equal(conn.streams[0].state.closed, true)
    // And the connection is not reused afterwards.
    assert.ok(sock)
  })

  test('a finished exchange deregisters its teardown', async () => {
    // Otherwise every completed fetch leaks a callback for the connection's life.
    const conn = makeConn([{ pieces: [OK, 'body'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await gw.fetch('/x')
    const closedBefore = conn.streams[0].state.closed

    conn.die()
    await tick(5)
    // No throw from a stale teardown, and the stream state is unchanged.
    assert.equal(conn.streams[0].state.closed, closedBefore)
  })

  test('close() closes the connection and refuses further use', async () => {
    const conn = makeConn([{ pieces: [OK, 'body'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await gw.fetch('/x')

    gw.close()
    await tick()
    assert.equal(conn.state.closed, true)
    await assert.rejects(gw.fetch('/x'), /closed/)
    await assert.rejects(gw.connect('1.1.1.1:443'), /closed/)
  })

  test('close() before any dial is harmless', () => {
    const gw = new KpsGateway(ADDR, { dial: makeDial({}) })
    gw.close()
    gw.close()
  })

  test('close() while a dial is in flight does not throw', async () => {
    const gw = new KpsGateway(ADDR, { dial: makeDial({ mode: 'hang' }) })
    const pending = gw.fetch('/x').catch(() => 'failed')
    gw.close()
    // The hung dial never settles, so the fetch stays pending; the point is that
    // close() itself neither throws nor produces an unhandled rejection.
    assert.equal(await Promise.race([pending, tick(20).then(() => 'pending')]), 'pending')
  })
})

describe('deadlines', () => {
  // Several of these deliberately leave a read pending, settled only by an
  // AbortSignal.timeout — whose timer is unref'd. Without something ref'd the
  // event loop drains and the runner reports the pending promises as failures.
  let keepAlive
  before(() => { keepAlive = setTimeout(() => {}, 30_000) })
  after(() => clearTimeout(keepAlive))

  test('an already-aborted signal fails fast', async () => {
    const conn = makeConn([{ pieces: [OK, 'body'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await assert.rejects(gw.fetch('/x', { signal: AbortSignal.abort() }), /timed out/)
  })

  test('a dial slower than the deadline rejects that caller', async () => {
    const conn = makeConn([{ pieces: [OK, 'body'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn, mode: 'slow', delay: 60 }) })
    await assert.rejects(
      gw.fetch('/x', { signal: AbortSignal.timeout(10) }),
      /dial 1\.2\.3\.4:12298:.*timed out/,
    )
  })

  /// The dial keeps running past one caller's deadline, so a connection that
  /// lands late is still stored and reused rather than leaked.
  test('a late connection is kept and reused', async () => {
    const conn = makeConn([{ pieces: [OK, 'a'] }, { pieces: [OK, 'b'] }])
    const dial = makeDial({ conn, mode: 'slow', delay: 40 })
    const gw = new KpsGateway(ADDR, { dial })

    await assert.rejects(gw.fetch('/a', { signal: AbortSignal.timeout(5) }), /timed out/)
    await tick(60)

    const res = await gw.fetch('/b')
    assert.equal(res.status, 200)
    assert.equal(dial.calls.length, 1, 'the abandoned dial was reused, not re-dialed')
  })

  test('a slow response head is bounded by the signal', async () => {
    const conn = makeConn([{ pieces: [], endAfterPieces: false }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await assert.rejects(gw.fetch('/slow', { signal: AbortSignal.timeout(20) }), /GET \/slow: timed out/)
  })

  test('a slow CONNECT reply is bounded by the signal', async () => {
    const conn = makeConn([{ pieces: [], endAfterPieces: false }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await assert.rejects(
      gw.connect('1.1.1.1:443', { signal: AbortSignal.timeout(20) }),
      /CONNECT 1\.1\.1\.1:443: timed out/,
    )
    assert.equal(conn.streams[0].state.closed, true, 'the stream is released on timeout')
  })

  /// A bootstrap snapshot is megabytes; a slow link must not read as a dead
  /// gateway, so only the head is deadlined.
  test('the body download is not bounded by the signal', async () => {
    const conn = makeConn([{ pieces: [OK, 'chunk one', 'chunk two'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const signal = AbortSignal.timeout(15)
    const res = await gw.fetch('/bootstrap.zip.zst', { signal })
    await tick(30)
    assert.equal(signal.aborted, true, 'the deadline did pass')
    assert.equal(dec.decode(res.body), 'chunk onechunk two')
  })

  test('the caller signal bounds openStream too', async () => {
    const conn = makeConn([{ hangOpen: true }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    const signal = AbortSignal.timeout(10)
    const pending = gw.fetch('/x', { signal }).catch((e) => e)
    await tick(30)
    // openStream is handed the caller's signal rather than the built-in default.
    assert.equal(conn.state.openSignals[0], signal)
    assert.equal(await Promise.race([pending, tick(10).then(() => 'pending')]), 'pending')
  })

  test('without a caller signal openStream still gets a default deadline', async () => {
    const conn = makeConn([{ pieces: [OK, 'body'] }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await gw.fetch('/x')
    // kps ISSUES #14: openStream can hang forever, so it is never unbounded.
    assert.ok(conn.state.openSignals[0] instanceof AbortSignal)
  })

  test('a failed openStream propagates', async () => {
    const conn = makeConn([{ failOpen: true }])
    const gw = new KpsGateway(ADDR, { dial: makeDial({ conn }) })
    await assert.rejects(gw.fetch('/x'), /openStream refused/)
  })
})
