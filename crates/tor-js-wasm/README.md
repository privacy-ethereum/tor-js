# tor-js (Rust crate)

> **For usage docs** (install, API, examples), see the
> [npm package README](../README.md).

This crate contains the Rust/WASM side of tor-js: the `wasm-bindgen`
bindings that expose Arti's Tor client to JavaScript.

## What this crate does

The TypeScript wrapper at the repo root provides the user-facing API.
This crate (in `rust/`) provides the WASM module it calls into. The
boundary works like this:

```
JavaScript (TorClient)
  → wasm-bindgen FFI
    → WasmTorClient (this crate)
      → arti-client (TorClient, circuits, directory)
        → tor-rtcompat WasmRuntime (sleep, spawn, sockets via JS callback)
```

### Key modules

| File | What it does |
|------|-------------|
| `src/lib.rs` | `WasmTorClient` and `WasmTorClientOptions` exposed via `#[wasm_bindgen]`. Client creation, `fetch()` dispatch, `ready()` polling, `close()`. Logging setup via `tracing-wasm`. |
| `src/fetch.rs` | Raw HTTP/1.1 implementation over Tor streams. Builds request bytes, opens a TLS connection through the circuit (via `rustls`), parses response headers, handles chunked/content-length/EOF body framing. Returns a `web_sys::Response` with a `ReadableStream` body for streaming. |
| `src/storage.rs` | `CachedJsStorage` — bridges async JS storage (IndexedDB, filesystem) to Arti's synchronous storage reads. Preloads all entries into a `HashMap` on init, serves reads from cache, writes back via fire-and-forget `spawn_local`. Implements `KeyValueStore`. |
| `src/fast_bootstrap.rs` | Parses a pre-packaged `bootstrap.zip` containing consensus + microdescriptors + authority certs. Seeds the storage so Arti can bootstrap without downloading through Tor first. Uses `crypto.subtle.digest()` for SHA-256 verification. |
| `src/error.rs` | `JsTorError` — structured error type with error code, kind, message, and retryability. Serialized to JS via `serde_wasm_bindgen`. |

### JS ↔ Rust boundary

- **Socket connections**: JS provides a `connect(addr: string) → ArtiSocket`
  callback at client creation. The Rust side calls this when Arti needs a
  TCP connection to a relay. The callback returns a bidirectional byte pipe
  (WebSocket or WebRTC data channel through a gateway, or direct TCP in Node).

- **Storage**: JS provides a `TorStorage` object implementing get/set/delete/
  keys/lock. Rust wraps it in `CachedJsStorage` which preloads everything
  synchronously, then writes back asynchronously.

- **Fetch response**: The Rust `fetch()` returns a `web_sys::Response`
  constructed from the parsed HTTP response. The body is a `ReadableStream`
  backed by a Rust future that reads chunks from the Tor circuit.

- **Logging**: Uses `tracing-wasm` with a custom subscriber that forwards
  log events to JS callbacks registered per-client.

## Building

From the repo root:

```bash
# Development build
npm run build          # or: scripts/build.sh

# Release build (with wasm-opt)
npm run build:release  # or: scripts/build.sh --release
```

This runs `wasm-pack build rust` on this crate, then builds the
TypeScript wrapper. Output goes to `dist/`.

## Dependencies

- **arti-client** — Tor client (custom storage via `KeyValueStore`)
- **tor-rtcompat** — `WasmRuntime` for async operations
- **tor-dirmgr** — directory management with custom `BoxedDirStore`
- **rustls** + **rustls-rustcrypto** — pure-Rust TLS for relay and HTTPS connections
- **wasm-bindgen** / **web-sys** / **js-sys** — JS interop
- **wasm-streams** — `ReadableStream` for streaming response bodies
- **ruzstd** — pure-Rust zstd (via tor-dirclient's `zstd-wasm` feature)

## License

MIT OR Apache-2.0
