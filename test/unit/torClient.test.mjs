// Unit tests for the TypeScript client layer:
//   - log-listener registration in wasm.ts (level widening, per-listener filtering)
//   - TorClient.ready() promise caching and retry-after-failure
//   - close() semantics and Symbol.dispose
//   - the fast-bootstrap callback registered when a gateway is configured
//
// The generated wasm-bindgen module is replaced by a stub (see
// fixtures/wasmStub.mjs) via an esbuild alias on `#wasm`, so no WASM is loaded.
//
//   npm run test:unit

import { test, before, beforeEach, describe } from 'node:test'
import assert from 'node:assert/strict'
import { resolve } from 'node:path'
import { bundleTs, root } from './bundle.mjs'

let wasm, TorClient, MemoryStorage
let stub

const tick = (n = 1) => new Promise((r) => setTimeout(r, n))

before(async () => {
  wasm = await bundleTs('test/unit/fixtures/clientEntry.ts', 'torClient', {
    alias: { '#wasm': resolve(root, 'test/unit/fixtures/wasmStub.mjs') },
  })
  ;({ TorClient, MemoryStorage } = wasm)
  stub = globalThis.__wasmStub
})

// ===========================================================================
// wasm.ts: log listener management
// ===========================================================================

describe('log listeners', () => {
  // The listener registry is module state shared across the suite, so each test
  // removes what it adds.
  const lastLevel = () => stub.levels.at(-1)

  test('registering a listener sets the WASM filter to its level', () => {
    const remove = wasm.addLogListener(() => {}, 'info')
    assert.equal(lastLevel(), 'info')
    remove()
  })

  test('the default level is debug', () => {
    const remove = wasm.addLogListener(() => {})
    assert.equal(lastLevel(), 'debug')
    remove()
  })

  test('an unknown level falls back to debug', () => {
    const remove = wasm.addLogListener(() => {}, 'verbose')
    assert.equal(lastLevel(), 'debug')
    remove()
  })

  // The WASM subscriber is global: it has to emit at the broadest level any
  // listener asked for, or the quieter listeners silently starve the louder one.
  test('the filter widens to the broadest listener', () => {
    const a = wasm.addLogListener(() => {}, 'error')
    assert.equal(lastLevel(), 'error')
    const b = wasm.addLogListener(() => {}, 'trace')
    assert.equal(lastLevel(), 'trace')
    const c = wasm.addLogListener(() => {}, 'warn')
    assert.equal(lastLevel(), 'trace', 'a narrower listener must not narrow the filter')
    a(); b(); c()
  })

  test('removing the broadest listener narrows the filter again', () => {
    const a = wasm.addLogListener(() => {}, 'warn')
    const b = wasm.addLogListener(() => {}, 'trace')
    assert.equal(lastLevel(), 'trace')
    b()
    assert.equal(lastLevel(), 'warn')
    a()
  })

  test('setListenerLevel re-syncs the filter', () => {
    const a = wasm.addLogListener(() => {}, 'error')
    const cb = () => {}
    const b = wasm.addLogListener(cb, 'error')
    assert.equal(lastLevel(), 'error')

    wasm.setListenerLevel(cb, 'trace')
    assert.equal(lastLevel(), 'trace')
    wasm.setListenerLevel(cb, 'warn')
    assert.equal(lastLevel(), 'warn')
    a(); b()
  })

  test('setListenerLevel on an unknown callback is a no-op', () => {
    const remove = wasm.addLogListener(() => {}, 'info')
    const before = stub.levels.length
    wasm.setListenerLevel(() => {}, 'trace')
    assert.equal(stub.levels.length, before, 'no re-sync for a listener we do not hold')
    remove()
  })

  // Removing the last listener deliberately skips the re-sync: with nothing
  // registered the fan-out has no one to call, so the filter value cannot change
  // observable behaviour — it only leaves WASM formatting messages that are then
  // dropped.
  test('removing the last listener leaves the filter where it was', () => {
    const remove = wasm.addLogListener(() => {}, 'trace')
    const count = stub.levels.length
    remove()
    assert.equal(stub.levels.length, count, 'no additional sync call')
    assert.equal(lastLevel(), 'trace')
  })

  test('removing a listener twice is harmless', () => {
    const remove = wasm.addLogListener(() => {}, 'info')
    remove()
    remove()
  })

  describe('fan-out', () => {
    before(async () => {
      // Installing the fan-out is part of WASM init.
      wasm.setWasmSourceProvider(async () => new Uint8Array([0]))
      await wasm.ensureWasmInitialized()
      assert.ok(stub.fanout, 'wasm.ts should install a single fan-out callback')
    })

    test('each listener only sees events at or above its own level', () => {
      const quiet = []
      const loud = []
      const a = wasm.addLogListener((l, t, m) => quiet.push([l, m]), 'warn')
      const b = wasm.addLogListener((l, t, m) => loud.push([l, m]), 'trace')

      for (const level of ['trace', 'debug', 'info', 'warn', 'error']) {
        stub.fanout(level, 'tor', `${level} message`)
      }

      assert.deepEqual(quiet.map(([l]) => l), ['warn', 'error'])
      assert.deepEqual(loud.map(([l]) => l), ['trace', 'debug', 'info', 'warn', 'error'])
      a(); b()
    })

    test('the target and message are passed through', () => {
      const seen = []
      const remove = wasm.addLogListener((...args) => seen.push(args), 'trace')
      stub.fanout('info', 'tor_dirmgr', 'consensus ready')
      assert.deepEqual(seen, [['info', 'tor_dirmgr', 'consensus ready']])
      remove()
    })

    test('an unknown level from WASM is treated as debug', () => {
      const seen = []
      const remove = wasm.addLogListener((l) => seen.push(l), 'debug')
      stub.fanout('mystery', 'tor', 'x')
      assert.deepEqual(seen, ['mystery'], 'delivered at the debug threshold')
      remove()
    })

    test('a removed listener stops receiving events', () => {
      const seen = []
      const remove = wasm.addLogListener((l) => seen.push(l), 'trace')
      stub.fanout('info', 't', 'first')
      remove()
      stub.fanout('info', 't', 'second')
      assert.deepEqual(seen, ['info'])
    })

    test('WASM init is idempotent', async () => {
      const calls = stub.initCalls
      await wasm.ensureWasmInitialized()
      await wasm.ensureWasmInitialized()
      assert.equal(stub.initCalls, calls, 'the binary is loaded once')
    })

    test('the source overrides are refused once init has happened', () => {
      assert.throws(() => wasm.setWasmUrl('http://x/y.wasm'), /before any TorClient/)
      assert.throws(() => wasm.setWasmSourceProvider(async () => new Uint8Array()), /before any TorClient/)
    })
  })
})

// ===========================================================================
// TorClient
// ===========================================================================

describe('TorClient', () => {
  /** A socket provider stand-in; `gateway` truthiness drives fast bootstrap. */
  function fakeProvider({ gateway = null, gatewayFetch } = {}) {
    return {
      gateway,
      closeCalls: 0,
      connect: async () => { throw new Error('not used') },
      gatewayFetch: gatewayFetch ?? (async () => ({ status: 200, statusText: 'OK', body: new Uint8Array([1, 2, 3]) })),
      close() { this.closeCalls++ },
    }
  }

  const makeClient = (overrides = {}) =>
    new TorClient({
      storage: new MemoryStorage(),
      socketProvider: fakeProvider(),
      ...overrides,
    })

  beforeEach(() => {
    stub.failCreate = false
    stub.failReady = false
  })

  test('ready() resolves once the WASM client reports ready', async () => {
    const client = makeClient()
    await client.ready()
    assert.equal(stub.clients.at(-1).readyCalls, 1)
    client.close()
  })

  test('concurrent ready() callers share one attempt', async () => {
    const client = makeClient()
    await Promise.all([client.ready(), client.ready(), client.ready()])
    assert.equal(stub.clients.at(-1).readyCalls, 1, 'one underlying ready() call')
    client.close()
  })

  /// The cached promise is cleared when it settles, so a failed bootstrap does
  /// not poison the client for its whole life.
  test('a failed ready() is retried on the next call', async () => {
    stub.failReady = true
    const client = makeClient()
    await assert.rejects(client.ready(), /bootstrap failed/)

    stub.failReady = false
    await client.ready()
    assert.equal(stub.clients.at(-1).readyCalls, 2, 'the second call tried again')
    client.close()
  })

  test('a failed ready() does not raise an unhandled rejection', async () => {
    // ready() caches its promise and clears it on settle; if that bookkeeping
    // chains off the promise without handling rejection, a failed bootstrap
    // crashes the host process on top of the error the caller already saw.
    const seen = []
    const onUnhandled = (reason) => seen.push(reason)
    process.on('unhandledRejection', onUnhandled)
    try {
      stub.failReady = true
      const client = makeClient()
      await assert.rejects(client.ready(), /bootstrap failed/)
      await tick(10)
      client.close()
    } finally {
      process.off('unhandledRejection', onUnhandled)
    }
    assert.deepEqual(seen.map(String), [], 'no unhandled rejections')
  })

  test('a failed bootstrap does not raise an unhandled rejection on its own', async () => {
    // Constructing a client kicks off bootstrap immediately; a client that is
    // built and then never awaited must not crash the process.
    const seen = []
    const onUnhandled = (reason) => seen.push(reason)
    process.on('unhandledRejection', onUnhandled)
    try {
      stub.failCreate = true
      makeClient()
      await tick(20)
    } finally {
      process.off('unhandledRejection', onUnhandled)
    }
    assert.deepEqual(seen.map(String), [], 'no unhandled rejections')
  })

  test('fetch() waits for readiness and forwards the call', async () => {
    const client = makeClient()
    const res = await client.fetch('https://example.com/x', { method: 'POST' })
    assert.equal(res.status, 200)

    const wasmClient = stub.clients.at(-1)
    assert.ok(wasmClient.readyCalls >= 1, 'fetch awaited readiness')
    assert.deepEqual(wasmClient.fetches, [['https://example.com/x', { method: 'POST' }]])
    client.close()
  })

  test('fetch() surfaces a bootstrap failure', async () => {
    stub.failCreate = true
    const client = makeClient()
    await assert.rejects(client.fetch('https://example.com/'), /create failed/)
    client.close()
  })

  describe('close', () => {
    test('closes the WASM client and the socket provider', async () => {
      const provider = fakeProvider()
      const client = makeClient({ socketProvider: provider })
      await client.ready()
      const wasmClient = stub.clients.at(-1)

      client.close()
      await tick()
      assert.equal(provider.closeCalls, 1)
      assert.equal(wasmClient.closeCalls, 1)
    })

    test('is idempotent', async () => {
      const provider = fakeProvider()
      const client = makeClient({ socketProvider: provider })
      await client.ready()
      client.close()
      client.close()
      client.close()
      await tick()
      assert.equal(provider.closeCalls, 1, 'the provider is closed once')
    })

    test('refuses further use', async () => {
      const client = makeClient()
      client.close()
      await assert.rejects(client.ready(), /closed/)
      await assert.rejects(client.fetch('https://example.com/'), /closed/)
    })

    test('closing during bootstrap does not throw', async () => {
      const client = makeClient()
      client.close() // before bootstrap has finished
      await tick(20)
    })

    // Invoked directly rather than through `using`, which needs a newer Node
    // than the package supports; this is exactly what `using` calls.
    test('Symbol.dispose closes the client', async () => {
      const provider = fakeProvider()
      const client = makeClient({ socketProvider: provider })
      await client.ready()
      client[Symbol.dispose]()
      await tick()
      assert.equal(provider.closeCalls, 1)
    })

    test('the log listener is removed on close', async () => {
      const client = makeClient()
      await client.ready()
      client.close()
      // close() itself re-syncs while other listeners remain, so measure after.
      const after = stub.levels.length
      client.setLogLevel('trace')
      assert.equal(stub.levels.length, after, 'no re-sync from a closed client')
    })
  })

  describe('log level', () => {
    test('setLogLevel re-syncs the WASM filter', async () => {
      const client = makeClient({ logLevel: 'error' })
      await client.ready()
      client.setLogLevel('trace')
      assert.equal(stub.levels.at(-1), 'trace')
      client.close()
    })
  })

  describe('fast bootstrap', () => {
    test('is registered only when a gateway is configured', async () => {
      const without = makeClient({ socketProvider: fakeProvider({ gateway: null }) })
      await without.ready()
      assert.equal(stub.options.at(-1).fastBootstrap, null, 'no gateway, no fast bootstrap')
      without.close()

      const withGw = makeClient({ socketProvider: fakeProvider({ gateway: { address: 'x' } }) })
      await withGw.ready()
      assert.equal(typeof stub.options.at(-1).fastBootstrap, 'function')
      withGw.close()
    })

    test('the callback returns the archive bytes', async () => {
      const client = makeClient({ socketProvider: fakeProvider({ gateway: { address: 'x' } }) })
      await client.ready()
      const cb = stub.options.at(-1).fastBootstrap
      assert.deepEqual(await cb(), new Uint8Array([1, 2, 3]))
      client.close()
    })

    /// Bootstrap goes through the provider rather than a captured gateway, so a
    /// non-200 is an error the provider can retry against another gateway —
    /// never a silent fall back to slow bootstrap.
    test('a non-200 archive response is an error', async () => {
      const provider = fakeProvider({
        gateway: { address: 'x' },
        gatewayFetch: async () => ({ status: 503, statusText: 'Service Unavailable', body: new Uint8Array() }),
      })
      const client = makeClient({ socketProvider: provider })
      await client.ready()
      const cb = stub.options.at(-1).fastBootstrap
      await assert.rejects(cb(), /503 Service Unavailable/)
      client.close()
    })

    test('a provider that cannot reach any gateway propagates', async () => {
      const provider = fakeProvider({
        gateway: { address: 'x' },
        gatewayFetch: async () => { throw new Error('all gateways failed') },
      })
      const client = makeClient({ socketProvider: provider })
      await client.ready()
      await assert.rejects(stub.options.at(-1).fastBootstrap(), /all gateways failed/)
      client.close()
    })
  })

  describe('browser guard', () => {
    // The check is synchronous in the constructor, so the globals only need
    // faking for that call.
    const asBrowser = (fn) => {
      const realProcess = globalThis.process
      const hadWindow = 'window' in globalThis
      try {
        delete globalThis.process
        globalThis.window = {}
        return fn()
      } finally {
        globalThis.process = realProcess
        if (!hadWindow) delete globalThis.window
      }
    }

    test('a browser without a gateway is refused', () => {
      asBrowser(() => {
        assert.throws(() => new TorClient({}), /must configure a gateway/)
        assert.throws(() => new TorClient({ gateway: [] }), /must configure a gateway/)
        assert.throws(() => new TorClient({ gateway: '' }), /must configure a gateway/)
      })
    })

    test('a gateway or an injected provider satisfies it', () => {
      const clients = asBrowser(() => [
        new TorClient({ gateway: '1.2.3.4:12298:h', storage: new MemoryStorage() }),
        new TorClient({ gateway: ['1.2.3.4:12298:h'], storage: new MemoryStorage() }),
        new TorClient({ socketProvider: fakeProvider(), storage: new MemoryStorage() }),
      ])
      for (const c of clients) c.close()
    })

    test('Node needs no gateway', () => {
      const client = new TorClient({ storage: new MemoryStorage(), socketProvider: fakeProvider() })
      client.close()
    })
  })
})

// ===========================================================================
// singleton
// ===========================================================================

describe('tor singleton', () => {
  let tor
  const providers = []
  const provider = () => {
    const p = {
      gateway: null,
      closeCalls: 0,
      connect: async () => { throw new Error('not used') },
      gatewayFetch: async () => ({ status: 200, statusText: 'OK', body: new Uint8Array() }),
      close() { this.closeCalls++ },
    }
    providers.push(p)
    return p
  }

  before(() => { tor = wasm.tor })
  beforeEach(() => { tor.close(); providers.length = 0 })

  test('fetch() opens the client on first use', async () => {
    tor.configure({ storage: new MemoryStorage(), socketProvider: provider() })
    const clientsBefore = stub.clients.length
    const res = await tor.fetch('https://example.com/x')
    assert.equal(res.status, 200)
    assert.equal(stub.clients.length, clientsBefore + 1)
  })

  test('open() is idempotent', async () => {
    tor.configure({ storage: new MemoryStorage(), socketProvider: provider() })
    const before = stub.options.length
    tor.open()
    tor.open()
    // Bootstrap is async, so the options object appears a tick later.
    await tick(5)
    assert.equal(stub.options.length, before + 1, 'only one client constructed')
  })

  test('the same client serves repeated fetches', async () => {
    tor.configure({ storage: new MemoryStorage(), socketProvider: provider() })
    await tor.fetch('https://example.com/a')
    const client = stub.clients.at(-1)
    await tor.fetch('https://example.com/b')
    assert.equal(stub.clients.at(-1), client, 'no second client')
    assert.deepEqual(client.fetches.map(([u]) => u), [
      'https://example.com/a',
      'https://example.com/b',
    ])
  })

  /// Reconfiguring an open singleton has to replace the live client, or the new
  /// settings (a different gateway, say) silently never take effect.
  test('configure() while open closes and reopens', async () => {
    const first = provider()
    tor.configure({ storage: new MemoryStorage(), socketProvider: first })
    await tor.fetch('https://example.com/a')
    const firstClient = stub.clients.at(-1)

    const second = provider()
    tor.configure({ storage: new MemoryStorage(), socketProvider: second })
    await tick()
    assert.equal(first.closeCalls, 1, 'the old provider was closed')
    assert.equal(firstClient.closeCalls, 1, 'the old client was closed')

    await tor.fetch('https://example.com/b')
    assert.notEqual(stub.clients.at(-1), firstClient, 'a new client took over')
    assert.equal(second.closeCalls, 0)
  })

  test('configure() while closed does not open a client', () => {
    const before = stub.options.length
    tor.configure({ storage: new MemoryStorage(), socketProvider: provider() })
    assert.equal(stub.options.length, before, 'stays lazy until first use')
  })

  test('close() releases the client and a later fetch reopens', async () => {
    tor.configure({ storage: new MemoryStorage(), socketProvider: provider() })
    await tor.fetch('https://example.com/a')
    const firstClient = stub.clients.at(-1)

    tor.close()
    await tick()
    assert.equal(firstClient.closeCalls, 1)

    await tor.fetch('https://example.com/b')
    assert.notEqual(stub.clients.at(-1), firstClient, 'reopened with a fresh client')
  })

  test('close() twice is harmless', () => {
    tor.close()
    tor.close()
  })

  test('the latest configuration is used when reopening', async () => {
    const first = provider()
    tor.configure({ storage: new MemoryStorage(), socketProvider: first })
    await tor.fetch('https://example.com/a')
    tor.close()

    const second = provider()
    tor.configure({ storage: new MemoryStorage(), socketProvider: second })
    await tor.fetch('https://example.com/b')

    // The reopened client dials through the most recently configured provider:
    // closing the singleton closes that one, and never the superseded one again.
    assert.equal(second.closeCalls, 0)
    tor.close()
    await tick()
    assert.equal(second.closeCalls, 1, 'the current provider is the one closed')
    assert.equal(first.closeCalls, 1, 'the superseded provider is not closed twice')
  })
})
