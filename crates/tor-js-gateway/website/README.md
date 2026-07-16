# website

The tor-js-gateway demo/inspector site: landing page, bootstrap inspector,
and relay connection tester, all speaking **KPS** — the browser dials a
gateway's `<ip>:<port>:<certhash>` address over WebRTC with
[`@kpstreams/webrtc-client`](https://www.npmjs.com/package/@kpstreams/webrtc-client)
and runs KPS-HTTP/1 exchanges (see [../PROTOCOL.md](../PROTOCOL.md)) on its
streams. There are no gateway URLs anywhere: the user pastes a gateway KPS
address (persisted in localStorage across the pages).

This is a self-contained static site with its own build and hosting story —
the gateway binary serves no pages.

## Build

```
npm install
npm run build     # bundles torJsGateway.js (+ @kpstreams deps) into dist/
```

Then host the directory on any static file server. For local preview:

```
npm run serve     # build + node serve.mjs (http://127.0.0.1:8080, PORT to override)
```

## Pages

| Page | What it does |
|---|---|
| `index.html` | Landing page; set the gateway address, see its `/metadata.json` (dialed live over KPS) |
| `bootstrap.html` | Downloads `/bootstrap.zip.zst` over a KPS stream with progress, decompresses via fzstd (pure JS — there is no transparent decompression on raw streams), and renders the consensus |
| `connect.html` | Opens `CONNECT` tunnels to consensus relays on KPS streams; hex console per tunnel |

## Library

`torJsGateway.js` is the client library the pages share (bundled to
`dist/torJsGateway.js`):

- `new Gateway(address)` — lazy-dials the gateway over WebRTC and reuses the
  connection; `fetch`/`fetchStream` run one KPS-HTTP/1 exchange per stream.
- `gateway.connect(target)` → `RelaySocket` — a `CONNECT` tunnel
  (`send`/`onmessage`/`onclose`/`closeWrite`/`close`).
- `bootstrap(gatewayOrAddress, onEvent)` — download + zstd (fzstd) decompress +
  parse, with progress events driven by `Content-Length` (compressed) and
  `X-Decompressed-Content-Length` (decompressed).
