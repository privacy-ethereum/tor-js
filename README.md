# tor-js

# Superseded

tor-js is now based on Tor Project's rewrite in rust - Arti (via WebAssembly).

It currently lives at https://github.com/voltrevo/arti/tree/main/crates/tor-js/ts-wrapper .

# About the Old Version

Embedded Tor implemented in JS.

**🎯 Try the Live Demo:** https://voltrevo.github.io/tor-js/

## ⚠️ Use at Your Own Risk

This is [experimental software](https://github.com/voltrevo/tor-js/issues/4).

## Installation

```bash
npm install tor-js
```

## CLI Usage

`curlTor` is a curl-like command-line tool that makes HTTP requests through the Tor network.

### Quick Start

```bash
# Using npx (no installation needed)
npx curlTor -v https://check.torproject.org/api/ip

# curlTor has similar options to curl:
npx curlTor -h

# Or install globally
npm install -g tor-js
curlTor https://check.torproject.org/api/ip
```

## Demo Development

If you've cloned this repository and want to run the demo locally:

```bash
npm install
npm run dev
```

## Usage

### Basic Example

```typescript
import { TorClient, Log } from 'tor-js';

const tor = new TorClient({
  snowflakeUrl: 'wss://snowflake.pse.dev/',
  log: new Log().child('Tor'),
});

const response = await tor.fetch('https://check.torproject.org/api/ip');
const data = await response.json();
console.log('Your Tor IP:', data.IP);

tor.close();
```

### No Static Certs

About half the bundle size of the standard version is attributable to the https
root certificates being statically included.

This version is much smaller, and will fetch (using regular fetch) https root
certificates instead. Certificates are still statically pinned using hashes, so
only the same certs as the static version will be used.

```typescript
import { TorClient, Log } from 'tor-js/noStaticCerts';

// Otherwise the same as the basic example
```

### Singleton

If you just want a single tor-powered fetch instance across your application
(similar to how standard fetch is a single instance), you can use this version
for convenience.

```typescript
import { tor } from 'tor-js/singleton';

const response = await tor.fetch('https://check.torproject.org/api/ip');
const data = await response.json();
console.log('Your Tor IP:', data.IP);
```

Based on the noStaticCerts build.

## TorClient API

### Constructor Options

```typescript
interface TorClientOptions {
  /** The Snowflake bridge WebSocket URL for Tor connections */
  snowflakeUrl: string;
  /** Timeout in milliseconds for establishing initial connections (default: 15000) */
  connectionTimeout?: number;
  /** Timeout in milliseconds for circuit creation and readiness (default: 90000) */
  circuitTimeout?: number;
  /** Number of circuits to pre-create and maintain in buffer (default: 2) */
  circuitBuffer?: number;
  /** Maximum lifetime in milliseconds for circuits before disposal (default: 600000 = 10 minutes) */
  maxCircuitLifetime?: number;
  /** Optional logger instance for hierarchical logging */
  log?: Log;
  /**
   * Optional storage interface for caching (default: tmp dir in nodejs,
   * indexeddb in browser).
   * Use `new storage.MemoryStorage()` to avoid secondary storage and only cache
   * during the current session.
   */
  storage?: IStorage;
}
```

### Instance Methods

#### `fetch(url: string, options?: RequestInit): Promise<Response>`

Makes an HTTP request through the persistent Tor circuit.

```typescript
const response = await tor.fetch('https://api.example.com/data');
const json = await response.json();
```

Same as [native fetch](https://developer.mozilla.org/en-US/docs/Web/API/Fetch_API).

#### `waitForCircuit(): Promise<void>`

Waits for a circuit to be ready.

#### `getCircuitStatus()` and `getCircuitStatusString()`

Get current circuit status information.

#### `close(): void`

Closes connection, cleans up resources.

## Technical Details

`tor-js` is a fork of [`@hazae41/echalote`](https://www.npmjs.com/package/@hazae41/echalote).

## License

MIT
