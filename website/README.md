# website

The tor-js-gateway demo/inspector site: landing page, bootstrap inspector,
relay connection tester, and the `torJsGateway.js` client library.

This is a self-contained subproject with its own hosting story — the gateway
binary no longer serves any pages (it is KPS-only; see [../PROTOCOL.md](../PROTOCOL.md)).

**Status: not yet ported to KPS.** These pages still speak the old HTTP/WS/WebRTC
endpoints (`fetch` against gateway URLs, `/socket/{target}`, `/rtc/connect`),
which no longer exist. The porting workstream (handover Task 6):

- Replace `torJsGateway.js`'s transport code with [`@kpstreams/webrtc-client`](https://www.npmjs.com/package/@kpstreams/webrtc-client)
  dials plus the KPS-HTTP/1 exchange (`GET /bootstrap.zip.br`, `CONNECT`).
- The user pastes/configures a gateway KPS address (`<ip>:<port>:<certhash>`)
  instead of a URL.
- `smartBootstrapDownload`'s WASM-brotli path becomes the only path — there is
  no transparent decompression on raw streams; remove the transparent branch.
- Progress events keep working off `X-Decompressed-Content-Length`/`Content-Length`.
