# Changelog

## 0.4.1

Additive only — no API or wire changes from 0.4.0.

- **Streaming request bodies.** `fetch()`'s `body` now accepts a `ReadableStream<Uint8Array>` alongside `string`, `Uint8Array` and `ArrayBuffer`. A stream is sent as it's produced, with `Transfer-Encoding: chunked` framing, instead of being buffered to learn its length — so uploads no longer need to fit in memory. Bodies of known size still use `Content-Length`. (Response bodies already streamed.)
- **Multiple gateways.** `gateway` accepts an array of KPS addresses to fail over and spread load between. The list is an **unordered set** — position implies no priority, and each client shuffles it, so independent clients don't all pile onto the same gateway. Within a preferred set of two, the gateway carrying the fewest connections is chosen, which also serves as the latency signal: a slow or stalled gateway stops being picked. Failures cool a gateway off with exponential backoff and it is re-admitted automatically once it recovers.
- **Fast bootstrap falls over.** Previously bootstrap was pinned to one gateway for the client's lifetime and silently degraded to slow bootstrap if that gateway was down. It now picks per attempt and tries the others. The happy path still contacts exactly one gateway.
- **Gateway attempts are bounded.** Dialing a gateway had no deadline, so one unreachable gateway could stall a relay connection indefinitely. Setup (dial, stream open, response head) now shares a 15s budget. Bootstrap body download is deliberately excluded, so a large snapshot over a slow link isn't mistaken for a dead gateway.
- **Injectable KPS transport.** `ArtiSocketProvider` accepts a `dial` function, and `TorClientOptions` accepts a `socketProvider`. Supplying a dialer means the built-in `@kpstreams` client is never loaded, so an embedder that already holds a KPS transport can bundle tor-js without it. The KPS address parser is vendored to keep that path free of runtime `@kpstreams` deps.
- Relay-connect failover keeps the target fixed across gateways, so a gateway cannot influence relay (guard) selection by refusing `CONNECT`s.
- `dist/anon-rpc-worker.js` is now built into the package: a hash-pinned [anon-rpc](https://github.com/privacy-ethereum/anon-rpc) worker that offers anonymized `fetch` from a sandboxed worker, reaching the network only through a host-granted KPS capability. Experimental and not yet a documented API; it is a hosted artifact rather than an import.

## 0.4.0

**Breaking: tor-js now connects through gateways over [KPS](https://github.com/privacy-ethereum/kps) instead of WebSocket/WebRTC.** A 0.4.0 client requires a KPS gateway and cannot talk to a pre-0.4.0 gateway (or vice versa).

- **`gateway` is now a KPS address, not a URL.** Pass `ip:port:certhash` (printed by the gateway at startup), e.g. `198.51.100.7:12298:uEiAxk…9Qw`, instead of `https://…`. The client dials it over WebRTC in browsers and QUIC in Node.js (via the optional `@kpstreams/quic-client` peer dependency); relay connections are HTTP `CONNECT` tunnels on KPS streams.
- Built on KPS 0.2.1 (`@kpstreams/*` client) with the gateway on the `kps` crate 0.2.2. 0.2.1 specifies read-termination semantics (KPS SPEC §9.2: a read errors on RESET/local-close/connection-loss, EOF only on peer FIN) and stops surfacing a bare `null` on teardown ([kps#3](https://github.com/privacy-ethereum/kps/issues/3)). Paired with a tor-js fix: the WASM runtime now drives `reader.cancel()` to completion on stream drop instead of dropping its promise, which under §9.2 rejects (the connection is closed first) and would otherwise crash Node with an unhandled rejection. The gateway's 0.2.2 bump fixes a WebRTC data-channel bug where a browser's reused SCTP stream id was routed to a stale closed channel, hanging reads after ~2 streams per connection ([kps#4](https://github.com/privacy-ethereum/kps/issues/4)).
- **Fast bootstrap moved to zstd** (`/bootstrap.zip.zst`), decompressed inside the WASM.
- **`ArtiSocket` is now stream-shaped** (`readable`/`writable`) rather than event-based, preserving backpressure end to end. This affects custom `socketProvider` implementations.
- Updated Arti to 2.5.0 (tor-* 0.44).
- The repository is now a monorepo: the KPS gateway lives in `crates/tor-js-gateway/`, and the website in `website/`.

### Migrating from 0.3.x

Replace your gateway URL with the gateway's KPS address, and run a KPS gateway (see [`crates/tor-js-gateway/`](crates/tor-js-gateway/)):

```js
// 0.3.x
new TorClient({ gateway: 'https://tor-js-gateway.example.com' });
// 0.4.0
new TorClient({ gateway: '198.51.100.7:12298:uEiAxk…9Qw' });
```
