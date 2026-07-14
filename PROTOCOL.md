# tor-js-gateway wire protocol over KPS ("KPS-HTTP/1")

- **Status:** Draft for implementation
- **Home:** this file lives in the `tor-js-gateway` repo as `PROTOCOL.md` and is the
  single source of truth for the profile. The anon-rpc specification contains a
  normative copy of the *bundle-fetch subset* (§4.1–4.2 there); if the two ever
  disagree on that subset, fix the drift — they are intended to be identical.

## 1. Overview

[KPS](https://github.com/privacy-ethereum/kps) provides secure, multiplexed,
unnamed byte streams to a peer pinned by certificate hash. KPS deliberately has
no stream names or protocol negotiation: applications route and frame inside
the stream bytes.

This document defines the application protocol a tor-js-gateway speaks on those
streams. It is **HTTP/1.1 syntax under a strict profile, one exchange per
stream** — the shape of HTTP/3 (semantics mapped one-request-per-transport-stream)
with HTTP/1.1's text syntax, so standard HTTP software (hyper, curl through a
TCP↔KPS bridge) handles it unmodified. One server multiplexes:

- serving the anon-rpc worker bundle (hash-addressed immutable objects);
- serving `bootstrap.zip.br` (Tor directory fast-bootstrap archive);
- proxying TCP to Tor relays (`CONNECT`);
- capability discovery (`/metadata.json`);
- future capabilities, as new routes/methods, without protocol changes.

The key words MUST, MUST NOT, SHOULD, SHOULD NOT, MAY are per RFC 2119/8174.

## 2. Addresses and resolver strings

A KPS address is exactly as the KPS spec defines it:

```
<ipv4>:<udp-port>:<certhash>          e.g.  198.51.100.7:42298:uEiAxk...9Qw
[<ipv6>]:<udp-port>:<certhash>        e.g.  [2001:db8::7]:42298:uEiAxk...9Qw
```

The certhash is multibase-`u` (base64url, no padding) — alphabet
`A–Z a–z 0–9 - _`. It never contains `/`. Bracketed IPv6 hosts never contain
`/`. These facts make the resolver grammar unambiguous.

A **kps resolver string** locates a resource on a KPS server:

```
kps-resolver = "kps:" kps-addr path
kps-addr     = <a KPS Address, verbatim>
path         = "/" path-absolute        ; per RFC 3986
```

Example:

```
kps:198.51.100.7:42298:uEiAxk...9Qw/worker/194f04bde4925f6bbb0bd8bdfceca7251125eaa0664ce3c0c25dce2a1545338d.js
```

Rules:

- A kps resolver is deliberately **not a URL** and MUST NOT be fed to generic
  URL parsers (the address form is not a valid URL authority; WHATWG `new URL`
  throws on it). The absence of `//` after the scheme is intentional signalling.
- Parse by removing the `kps:` prefix and splitting at the **first `/`**: the
  left part is passed verbatim to the KPS dial operation; the right part
  (including the leading `/`) is the request target of the exchange (§3).
- The dial operation is transport-neutral: browser clients dial over WebRTC,
  native clients over QUIC, against the same address, per KPS.

## 3. The exchange profile

### 3.1 One exchange per stream

Each KPS stream carries **exactly one** request/response exchange (except
`CONNECT`, §4, where the exchange transitions into a tunnel). There is no
keep-alive, no pipelining, no connection reuse at the HTTP layer — reuse
happens at the KPS *connection* layer (one connection, many streams). After the
exchange completes, both sides close the stream.

### 3.2 Request

The client writes, in order:

1. A request line: `METHOD SP request-target SP HTTP/1.1 CRLF`.
   - For origin-form requests the target is an absolute path (optionally with
     query), e.g. `GET /bootstrap.zip.br HTTP/1.1`.
   - For `CONNECT` the target is authority-form, §4.
2. Header fields, one per line, `CRLF`-terminated, then an empty line (`CRLF`).
3. An optional body: raw bytes until the client half-closes.
4. `closeWrite()` (graceful FIN). The peer observes EOF; that EOF terminates
   the request body. `GET` and `HEAD` requests MUST NOT include a body.

Header requirements:

- The request MUST include a `Host` header (RFC 9112 §3.2 — a Host-less
  request is invalid HTTP/1.1 and strict stacks reject it).
- For origin-form requests, the `Host` value SHOULD be the **certhash of the
  dialed address, verbatim** (a bare certhash is a syntactically valid
  reg-name). Rationale: in KPS the certhash *is* the name — the analog of the
  DNS name that `Host` was invented to carry — and it is stable across every
  address (v4/v6) the identity is published at.
- Servers MUST NOT use the `Host` value as a trust input; trust comes from the
  KPS handshake. Servers MAY use it for routing (virtual hosting of multiple
  identities on one listener). A server that checks it and finds a mismatch
  SHOULD respond `421 Misdirected Request`; servers that don't virtual-host
  SHOULD ignore it. Note base64url is case-sensitive while `uri-host` is
  case-insensitive; comparisons MUST therefore be exact-match-or-reject, never
  case-normalized.
- `Content-Length`, when present, is **advisory** (useful for progress
  reporting); actual body length is delimited by EOF.

### 3.3 Response

The server writes:

1. A status line: `HTTP/1.1 SP status-code SP reason CRLF`.
2. Header fields, then an empty line.
3. The body: raw bytes until the server half-closes (`closeWrite()`). EOF
   terminates the body. `Content-Length` is advisory, as above. Responses to
   `HEAD`, and `1xx/204/304` responses, have no body.

A server MAY begin its response before it has read the complete request body
(standard HTTP behavior); the stream is full-duplex.

### 3.4 Forbidden HTTP/1.1 features

The following MUST NOT be sent by conforming clients or servers. A recipient
that observes any of them MUST abandon the exchange (reset/close the stream);
it MUST NOT attempt lenient recovery. With chunked encoding banned and no
connection reuse, the request-smuggling/desync class of HTTP/1.1
vulnerabilities structurally cannot occur — strictness here is the security
property.

- `Transfer-Encoding` in any message, any value (bodies are EOF-delimited).
- Multiple `Content-Length` fields, or a `Content-Length` disagreeing with
  another `Content-Length`.
- Persistent connections / pipelining (`Connection` header contents are
  ignored; a second request on a stream is a protocol error).
- Trailers.
- `Expect: 100-continue` and all interim `1xx` responses except as generated
  by unmodified serving stacks — clients MUST tolerate and skip `1xx` blocks.
- Obsolete line folding (`obs-fold`) in headers.
- Request or response upgrades (`Upgrade`), except the `CONNECT` transition
  of §4, which is not an Upgrade.

### 3.5 Cancellation and errors

- Client-side cancellation is transport-level: `resetWrite()` / `close()` on
  the stream. There is no in-band cancellation.
- HTTP status codes carry application errors. Error response bodies SHOULD be
  short `text/plain; charset=utf-8` diagnostics. Diagnostic text MUST NOT be
  parsed for control flow.
- Unknown path → `404`. Known path, unsupported method → `405`. Unknown
  method → `501`.

### 3.6 Limits (defaults; implementations MAY tune)

- Header block (request or response, request-line/status-line included):
  max **16 KiB**. Overflow → server responds `431` if possible, then resets;
  clients abandon the fetch.
- Time from stream open to complete request header block: **30 s** server-side,
  then reset.
- Body-size caps are per-route/per-consumer (e.g. the anon-rpc harness caps
  bundle fetches at 64 MiB).

### 3.7 Datagrams

KPS datagrams are **reserved**. Conforming endpoints MUST NOT send datagrams
under this protocol version and MUST ignore received ones. A future version
will define a tag scheme on datagram payloads before using them.

## 4. CONNECT — TCP tunneling

`CONNECT` proxies one TCP connection per stream, exactly the role CONNECT was
designed for.

Request:

```
CONNECT 185.220.101.4:9001 HTTP/1.1
Host: 185.220.101.4:9001
```

- The request-target is authority-form `<ip>:<port>`; IPv6 literals bracketed.
  Names are not permitted — targets are relay IPs from the consensus.
- Per standard proxy convention (and unlike §3.2), `Host` mirrors the
  authority-form target. hyper's CONNECT path expects this.
- There is no request body before the tunnel; the client MUST NOT `closeWrite`
  before the server's response if it intends to send tunnel bytes.

Server behavior:

1. Validate the target: parseable → else `400`; present in the current
   consensus relay allowlist → else `403`; per-connection/per-IP tunnel limits
   not exceeded → else `429`; local/reserved addresses always refused (`403`).
2. Dial the target TCP address. Failure → `502`; timeout → `504`.
3. On success respond `200` with an empty body **and no `Content-Length` or
   `Transfer-Encoding`**. Everything after the response header block, in both
   directions, is the raw TCP byte stream.

Tunnel lifecycle mapping:

- client `closeWrite` → TCP FIN to target; target FIN → server `closeWrite`.
- `resetWrite`/`close`/connection loss on either side → abortive close (RST)
  of the other side.
- Idle and max-lifetime timeouts are server policy (configurable), enforced by
  resetting the stream.

## 5. Routes and capability discovery

A server advertises what it supports at `GET /metadata.json`:

```json
{
  "protocol": "kps-http/1",
  "software": "tor-js-gateway",
  "version": "<server version>",
  "capabilities": ["metadata", "bootstrap", "connect", "worker-bundles", "relay-random"],
  "addresses": ["198.51.100.7:42298:uEiAxk...9Qw", "[2001:db8::7]:42298:uEiAxk...9Qw"]
}
```

- `metadata` (this route) is REQUIRED. All other capabilities are OPTIONAL and
  discoverable. `addresses` lets a server cross-publish its v4/v6 addresses
  (same certhash, per KPS dual-publish).
- New capabilities are added as new names + routes; clients MUST ignore
  unknown capability names.

Defined routes:

| Capability | Route | Notes |
|---|---|---|
| `metadata` | `GET /metadata.json` | `application/json` |
| `bootstrap` | `GET /bootstrap.zip.br` | brotli-compressed zip; MUST include `X-Decompressed-Content-Length` and SHOULD include `Content-Length`. There is **no transparent decompression** over raw streams — clients decompress themselves. |
| `worker-bundles` | `GET /worker/{keccak-hex}.js` | `{keccak-hex}` is 64 lowercase hex chars, no `0x`. The served bytes MUST satisfy `keccak256(bytes) == hash`. Immutable: `Cache-Control: public, max-age=31536000, immutable`. Unknown hash → `404`. |
| `connect` | `CONNECT <ip>:<port>` | §4 |
| `relay-random` | `GET /relay/random` | as in the pre-KPS gateway; JSON descriptor of a random consensus relay |

## 6. The anon-rpc bundle-fetch subset

The subset that anon-rpc makes normative for harnesses (its §4.1–4.2) is: the
resolver grammar of §2; a single `GET` exchange per §3 with `Host` REQUIRED;
body read to EOF; `Transfer-Encoding` → abort; redirects (3xx) MUST NOT be
followed (a resolver that redirects has failed; alternative locations are
expressed as additional resolver entries); any non-`200` status fails that
resolver. Integrity never depends on the transport — only on the harness's
keccak256 check against the on-chain `workerHash()`.

## 7. Security considerations

- The KPS handshake authenticates the server (certhash pin) and encrypts the
  stream; nothing in this profile adds or replaces trust. `Host` is routing
  metadata only (§3.2).
- CONNECT target validation (consensus allowlist, no local/reserved addresses)
  is what prevents the gateway from being an open proxy; it MUST be enforced
  regardless of any header content.
- Strict parsing (§3.4) is normative, not advisory: the parser rejecting
  forbidden constructs is the mitigation for the h1 desync bug class.
- Rate/resource limits (tunnels per connection, per client IP via the KPS
  connection's remote address, header caps, timeouts) are the DoS surface;
  ship with the defaults of §3.6 and the gateway's tunnel limits configurable.
