// Stand-in for the generated wasm-bindgen module (the `#wasm` import), so
// src/wasm.ts and src/TorClient.ts can be unit-tested without loading a 2 MB
// WASM binary or touching the network.
//
// Recorded calls and injectable behaviour live on `globalThis.__wasmStub`, which
// the bundle and the test file both reach — the stub is bundled into the module
// under test, so it can't be imported directly by the test.

const stub = (globalThis.__wasmStub = {
  /** Levels passed to the WASM filter, in order. */
  levels: [],
  /** The fan-out callback wasm.ts installs. */
  fanout: null,
  initCalls: 0,
  initOptions: [],
  wasmInitCalls: 0,
  /** Clients handed out by TorClient.create. */
  clients: [],
  /** Options objects constructed. */
  options: [],
  /** Set to make initWasm reject. */
  failInit: false,
  /** Set to make TorClient.create reject. */
  failCreate: false,
  /** Set to make client.ready() reject; cleared by the test between attempts. */
  failReady: false,
  reset() {
    this.levels = []
    this.fanout = null
    this.initCalls = 0
    this.initOptions = []
    this.wasmInitCalls = 0
    this.clients = []
    this.options = []
    this.failInit = false
    this.failCreate = false
    this.failReady = false
  },
})

export default async function initWasm(options) {
  stub.initCalls++
  stub.initOptions.push(options)
  if (stub.failInit) throw new Error('wasm init failed')
}

export function init() {
  stub.wasmInitCalls++
}

export function setLogCallback(cb) {
  stub.fanout = cb
}

export function setLogLevel(level) {
  stub.levels.push(level)
}

export class TorClientOptions {
  constructor(connect) {
    this.connect = connect
    this.storage = null
    this.fastBootstrap = null
    stub.options.push(this)
  }

  withStorage(storage) {
    this.storage = storage
    return this
  }

  withFastBootstrap(cb) {
    this.fastBootstrap = cb
    return this
  }
}

export class TorClient {
  constructor(options) {
    this.options = options
    this.readyCalls = 0
    this.fetches = []
    this.closeCalls = 0
  }

  static async create(options) {
    if (stub.failCreate) throw new Error('create failed')
    const client = new TorClient(options)
    stub.clients.push(client)
    return client
  }

  async ready() {
    this.readyCalls++
    if (stub.failReady) throw new Error('bootstrap failed')
  }

  async fetch(url, init) {
    this.fetches.push([url, init])
    return new Response('body', { status: 200 })
  }

  close() {
    this.closeCalls++
  }
}
