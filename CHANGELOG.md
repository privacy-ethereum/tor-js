# Changelog

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
