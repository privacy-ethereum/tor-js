// Manual test for STREAMING request bodies: the worker POSTs a ReadableStream
// body through Tor to a public echo, and we diff the echoed bytes against what
// was sent. A ReadableStream has no known length, so tor-js sends it with
// Transfer-Encoding: chunked — if the echoed body matches, the chunked encoder
// round-trips correctly end to end.
//
// Hits live Tor + a public echo, so it's NOT in CI (same as run-e2e.mjs). Run:
//
//   npm run test:stream-upload
//   GATEWAY=<ip:port:certhash> ECHO=https://postman-echo.com/post npm run test:stream-upload
//
// ECHO must be an endpoint that echoes the POST body. Defaults to postman-echo
// (returns the body as a Buffer — binary-safe). httpbin.org/post also works
// (returns it as a string).

import { setupHarness, check, cleanup, fail, guard, WORKER_ADDR } from "./harness.mjs";

const GATEWAY =
  process.env.GATEWAY ||
  "170.64.236.147:12298:uEiBHwUMNRTetrbqScahm81Di57Xv2OphNrx-CurJGOq3ww";
const ECHO = process.env.ECHO || "https://postman-echo.com/post";

async function main() {
  const { page, ethCallMap } = await setupHarness();

  console.log(`streaming a chunked POST body through Tor to ${ECHO} …`);
  const result = await page.evaluate(async (cfg) => {
    const provider = {
      request: async ({ method, params }) => {
        if (method !== "eth_call") throw new Error(`unexpected method ${method}`);
        const ret = cfg.ethCallMap[params[0].to]?.[params[0].data.slice(0, 10)];
        if (!ret) throw new Error(`no mock eth_call for ${params[0].to} ${params[0].data.slice(0, 10)}`);
        return ret;
      },
    };
    const w = new window.AnonRpcWorker({
      address: cfg.addr,
      config: { gateways: cfg.gateways },
      preExisting: { rpcProvider: provider },
    });
    await w.ready;

    // Build a multi-chunk request body (printable ASCII so both Buffer- and
    // string-style echoes round-trip). Enqueuing several chunks makes the
    // worker emit several chunked frames.
    const enc = new TextEncoder();
    const parts = [];
    for (let i = 0; i < 6; i++) parts.push(enc.encode(`chunk-${i}:` + "x".repeat(4096) + "\n"));
    const sent = (() => {
      const total = parts.reduce((n, p) => n + p.length, 0);
      const out = new Uint8Array(total);
      let off = 0;
      for (const p of parts) { out.set(p, off); off += p.length; }
      return out;
    })();
    const body = new ReadableStream({
      start(c) { for (const p of parts) c.enqueue(p); c.close(); },
    });

    const r = await w.fetch(cfg.echo, {
      method: "POST",
      headers: { "content-type": "application/octet-stream" },
      body,
    });
    const status = r.status;
    const j = await r.json();

    // Reconstruct the echoed body across the common echo shapes.
    const extract = (d) => {
      if (d && typeof d === "object" && d.type === "Buffer" && Array.isArray(d.data)) {
        return new Uint8Array(d.data); // postman-echo
      }
      if (typeof d === "string" && d.startsWith("data:")) {
        const bin = atob(d.split(",", 2)[1]); // httpbingo base64 data-uri
        const u = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) u[i] = bin.charCodeAt(i);
        return u;
      }
      if (typeof d === "string") return new TextEncoder().encode(d); // httpbin raw
      return null;
    };
    const got = extract(j?.data);

    let ok = false, mismatchAt = -1;
    if (got && got.length === sent.length) {
      ok = true;
      for (let i = 0; i < sent.length; i++) if (got[i] !== sent[i]) { ok = false; mismatchAt = i; break; }
    }
    // Best-effort: what framing did the echo see? (Some strip it after decoding.)
    const hdrs = j?.headers || {};
    const te = hdrs["transfer-encoding"] || hdrs["Transfer-Encoding"] || null;
    const cl = hdrs["content-length"] || hdrs["Content-Length"] || null;

    w.close();
    return { status, ok, sentLen: sent.length, gotLen: got ? got.length : -1, mismatchAt, te, cl };
  }, { addr: WORKER_ADDR, gateways: [GATEWAY], echo: ECHO, ethCallMap });

  console.log(
    `HTTP ${result.status}; sent ${result.sentLen}B, echoed ${result.gotLen}B; ` +
    `request framing seen by echo: transfer-encoding=${result.te} content-length=${result.cl}`,
  );
  check("streaming POST returned HTTP 200", result.status === 200, `status=${result.status}`);
  check(
    "echoed body matches the streamed request body",
    result.ok,
    result.gotLen !== result.sentLen
      ? `length ${result.gotLen} != ${result.sentLen}`
      : `first mismatch at byte ${result.mismatchAt}`,
  );

  console.log("\n✅ streaming request body round-tripped through Tor (chunked encoding verified)");
  cleanup();
  process.exit(0);
}

guard(150_000);
main().catch((e) => fail(e?.stack || String(e)));
