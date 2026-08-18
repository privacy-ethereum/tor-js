# tor-js

Make HTTPS requests through Tor from JavaScript. Works in browsers and Node.js.

Uses [Arti](https://gitlab.torproject.org/tpo/core/arti) (the Tor Project's Rust implementation) compiled to WebAssembly.

**[Live Demo](https://ethereum.github.io/tor-js/demo.html)**

## Status

[Experimental](https://github.com/ethereum/tor-js/issues/6).

It is your responsibilty to decide whether tor-js meets your security requirements. This software is provided for free and without warranty, per the MIT license.

Please reach out ([on github](https://github.com/ethereum/tor-js/issues/6) or otherwise) if you'd like to see more security validation for tor-js.

## Quick start

```
npm install tor-js
```

```javascript
import { TorClient } from 'tor-js';

const client = new TorClient({
  // gateway: '198.51.100.7:12298:uEiAxk...9Qw',

  // (A gateway's KPS address, "ip:port:certhash" — printed by
  // the gateway at startup. In NodeJS you can leave this
  // commented, but browsers don't have raw TCP and so require
  // help to connect to the tor network. See the Gateway
  // section below.)
});

const response = await client.fetch('https://check.torproject.org/api/ip');
console.log(await response.json()); // { IsTor: true, IP: "..." }

client.close();
```

## Entry points

The package offers three ways to load the WASM binary. All export the same API.

| Import | WASM loading | Size (gzip) | Best for |
|---|---|---|---|
| `tor-js` | Fetched from CDN, cached locally | 32 kB | Production web apps |
| `tor-js/wasm-base64` | Embedded in the JS bundle | 2.3 MB | Single-file deploys |
| `tor-js/wasm-file` | Loaded from `tor_js_bg.wasm` next to the module | 31 kB + 1.7 MB | Self-hosted, server-side |

Each also has a `/singleton` variant (see [Singleton](#singleton) below).

## API

### `new TorClient(options)`

Creates a Tor client and begins bootstrapping immediately.

```typescript
type TorClientOptions = {
  gateway?: string | string[];         // Gateway KPS address(es) "ip:port:certhash" (required in browsers, optional in Node.js/Deno)
  log?: Log;                           // Logger instance (default: silent)
  storage?: TorStorage;                // Persistent storage (default: auto-detected)
  logLevel?: LogLevel;                 // 'trace' | 'debug' | 'info' | 'warn' | 'error'
  socketProvider?: ArtiSocketProvider; // Custom transport (overrides `gateway`)
};
```

The gateway is dialed over [KPS](https://ethereum.github.io/kps/) (WebRTC in browsers; QUIC in Node.js via the optional `@kpstreams/quic-client` package) and tunnels relay connections with HTTP `CONNECT`. In Node.js/Deno, connections go via direct TCP and the gateway is only used for fast bootstrap (optional).

Pass several gateways for redundancy:

```javascript
new TorClient({ gateway: ['198.51.100.7:12298:uEiAxk…9Qw', '203.0.113.4:12298:uEiBz…7Kw'] });
```

The list is an **unordered set** — position implies no priority. Each client shuffles it and prefers one gateway, so independent clients spread across your fleet rather than all choosing the first. Under concurrent load traffic spreads across a small preferred set, favouring whichever gateway is carrying the least, and a gateway that fails is cooled off with exponential backoff and re-admitted once it recovers.

### `client.fetch(url, init?)`

Make an HTTP request through Tor. Returns a standard `Response` object.

Waits for the client to be fully ready before sending the request.

```typescript
const res = await client.fetch('https://example.com', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ key: 'value' }),
  signal: AbortSignal.timeout(30_000),
});
```

```typescript
type FetchInit = {
  method?: string;
  headers?: Record<string, string>;
  body?: string | Uint8Array | ArrayBuffer | ReadableStream<Uint8Array>;
  signal?: AbortSignal;
};
```

Response bodies stream: `res.body` is a `ReadableStream` you can consume as it arrives, so a large download needn't be buffered. A body that ends early — fewer bytes than `Content-Length`, or a chunked body without its terminating chunk — errors the stream rather than completing short.

Request bodies stream too. A `ReadableStream` body is sent as it's produced, using `Transfer-Encoding: chunked` — so an upload of unknown or unbounded size never has to fit in memory:

```javascript
await client.fetch('https://example.com/upload', {
  method: 'POST',
  body: fileOrStream,   // any ReadableStream<Uint8Array>
});
```

Bodies of known size (`string`, `Uint8Array`, `ArrayBuffer`) are sent with `Content-Length` instead.

### `client.ready()`

Wait for the client to be ready for traffic (guard connected, usable consensus, sufficient microdescs). Called automatically by `fetch()`, but useful to call early if you want to measure bootstrap time or show a loading state.

```typescript
const client = new TorClient({ ... });
await client.ready();
console.log('Bootstrap complete');
```

### `client.setLogLevel(level)`

Change the log level at runtime. Accepts `'trace'`, `'debug'`, `'info'`, `'warn'`, or `'error'`.

### `client.close()`

Close the client and release resources. Also available as `Symbol.dispose` for use with `using`:

```typescript
{
  using client = new TorClient({ ... });
  await client.fetch('https://example.com');
} // automatically closed
```

## Gateway

Browsers can't open raw TCP sockets, so to reach Tor relays from a browser you connect through a **gateway** — a small server that proxies relay connections and serves a fast-bootstrap snapshot of the Tor directory. (In Node.js/Deno tor-js opens TCP directly, so a gateway is optional there — used only for fast bootstrap.)

The client dials the gateway by its **KPS address**, `ip:port:certhash` (printed by the gateway at startup), over [KPS](https://ethereum.github.io/kps/) — WebRTC in browsers, QUIC in Node.js. Each Tor relay connection is an HTTP `CONNECT` tunnel on its own KPS stream; fast bootstrap is a zstd-compressed directory snapshot fetched over the same connection. The full wire protocol is specified in [PROTOCOL.md](PROTOCOL.md).

The gateway lives in this repo at [`crates/tor-js-gateway/`](crates/tor-js-gateway/) — see [its README](crates/tor-js-gateway/README.md) to build and run your own. The [live demo](https://ethereum.github.io/tor-js/demo.html) uses a public gateway that is for demonstration only (limited capacity, may disappear at any time); host your own for anything real.

## Singleton

For simple use cases, import the singleton wrapper:

```javascript
import { tor } from 'tor-js/singleton';

// tor.configure({
//   gateway: '198.51.100.7:12298:uEiAxk...9Qw',
//
//   (A gateway's KPS address, "ip:port:certhash". In NodeJS
//   you can leave this commented, but browsers don't have
//   raw TCP and so require help to connect to the tor network.
//   See the Gateway section above.)
// });

const response = await tor.fetch('https://check.torproject.org/api/ip');
```

The singleton auto-opens on first `fetch()`. Use `tor.configure(options)` to change settings, or `tor.close()` to shut down.

## Storage

By default, `TorClient` auto-detects the best storage for the environment:

- **Browser**: IndexedDB
- **Node.js**: `~/.local/share/tor-js/`

Cached consensus and microdescriptors are persisted, so subsequent connections bootstrap faster.

You can provide your own storage:

```javascript
import { TorClient, storage } from 'tor-js';

// Explicit IndexedDB
const client = new TorClient({
  storage: new storage.IndexedDBStorage('my-app'),
  // ...
});

// In-memory (no persistence)
const client = new TorClient({
  storage: new storage.MemoryStorage(),
  // ...
});
```

## Logging

Pass a `Log` instance to see bootstrap progress and debug info:

```javascript
import { TorClient, Log } from 'tor-js';

const client = new TorClient({
  log: new Log(),       // logs to console with timestamps
  logLevel: 'info',     // minimum level (default: 'debug')
  // ...
});
```

Custom log sink:

```javascript
const log = new Log({
  rawLog: (level, ...args) => myLogger[level](...args),
});
```

## Verifying the build

The WASM binary is reproducible: a clean-tree `npm run build` of a given commit
produces byte-identical `dist/tor_js_bg.wasm` and `dist/anon-rpc-worker.js` on
any host, including a different CPU architecture (verified across `x86_64` and
`aarch64` Linux). Each CI run prints the commit, the toolchain versions and both
hashes, so you can check a release against your own build.

Two things to know if a hash doesn't match: the toolchain is pinned
(`rust-toolchain.toml`, plus wasm-pack and binaryen versions asserted by
`scripts/build.sh`), and a **dirty working tree deliberately won't reproduce** —
it embeds a build timestamp so an unverifiable build is self-identifying. Build
from a clean checkout, and note that a stale `target/` directory is not
guaranteed canonical.

## License

MIT OR Apache-2.0
