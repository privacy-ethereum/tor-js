# tor-js

Make HTTPS requests through Tor from JavaScript. Works in browsers and Node.js.

Uses [Arti](https://gitlab.torproject.org/tpo/core/arti) (the Tor Project's Rust implementation) compiled to WebAssembly.

**[Live Demo](https://privacy-ethereum.github.io/tor-js/demo.html)**

## Status

[Experimental](https://github.com/privacy-ethereum/tor-js/issues/6).

It is your responsibilty to decide whether tor-js meets your security requirements. This software is provided for free and without warranty, per the MIT license.

Please reach out ([on github](https://github.com/privacy-ethereum/tor-js/issues/6) or otherwise) if you'd like to see more security validation for tor-js.

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
| `tor-js` | Fetched from CDN, cached locally | 30 kB | Production web apps |
| `tor-js/wasm-base64` | Embedded in the JS bundle | 2.2 MB | Single-file deploys |
| `tor-js/wasm-file` | Loaded from `tor_js_bg.wasm` next to the module | 29 kB + 1.7 MB | Self-hosted, server-side |

Each also has a `/singleton` variant (see [Singleton](#singleton) below).

## API

### `new TorClient(options)`

Creates a Tor client and begins bootstrapping immediately.

```typescript
type TorClientOptions = {
  gateway?: string;       // Gateway KPS address "ip:port:certhash" (required in browsers, optional in Node.js/Deno)
  log?: Log;              // Logger instance (default: silent)
  storage?: TorStorage;   // Persistent storage (default: auto-detected)
  logLevel?: LogLevel;    // 'trace' | 'debug' | 'info' | 'warn' | 'error'
};
```

The gateway is dialed over [KPS](https://privacy-ethereum.github.io/kps/) (WebRTC in browsers; QUIC in Node.js via the optional `@kpstreams/quic-client` package) and tunnels relay connections with HTTP `CONNECT`. In Node.js/Deno, connections go via direct TCP and the gateway is only used for fast bootstrap (optional).

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

The client dials the gateway by its **KPS address**, `ip:port:certhash` (printed by the gateway at startup), over [KPS](https://privacy-ethereum.github.io/kps/) — WebRTC in browsers, QUIC in Node.js. Each Tor relay connection is an HTTP `CONNECT` tunnel on its own KPS stream; fast bootstrap is a zstd-compressed directory snapshot fetched over the same connection. The full wire protocol is specified in [PROTOCOL.md](PROTOCOL.md).

The gateway lives in this repo at [`crates/tor-js-gateway/`](crates/tor-js-gateway/) — see [its README](crates/tor-js-gateway/README.md) to build and run your own. The [live demo](https://privacy-ethereum.github.io/tor-js/demo.html) uses a public gateway that is for demonstration only (limited capacity, may disappear at any time); host your own for anything real.

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

## License

MIT OR Apache-2.0
