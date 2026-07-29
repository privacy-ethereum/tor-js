// A minimal HTTP echo for the streaming-upload test. Unlike public echoes
// (postman-echo/httpbin), it reports the REQUEST framing it received —
// `chunked` and `contentLength` — so the test can prove tor-js actually sent
// Transfer-Encoding: chunked, not just that the body round-tripped.
//
// Deploy on a public host reachable from Tor exits (port 80 is the most
// exit-friendly), then point the test at it:
//   node echo-server.mjs                       # listens on :80 (needs privilege)
//   PORT=8080 node echo-server.mjs
//   ECHO=http://<public-ip>/ npm run test:stream-upload

import { createServer } from "node:http";

const port = Number(process.env.PORT || 80);

createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    const body = Buffer.concat(chunks);
    const te = String(req.headers["transfer-encoding"] || "").toLowerCase();
    res.writeHead(200, { "content-type": "application/json", "access-control-allow-origin": "*" });
    res.end(JSON.stringify({
      echo: "tor-js-stream-echo",
      method: req.method,
      chunked: te.split(/,\s*/).includes("chunked"),
      contentLength: req.headers["content-length"] ?? null,
      headers: req.headers,
      bodyLen: body.length,
      bodyBase64: body.toString("base64"),
    }));
  });
  req.on("error", () => { try { res.writeHead(400); res.end(); } catch {} });
}).listen(port, "0.0.0.0", () => console.log(`tor-js-stream-echo listening on :${port}`));
