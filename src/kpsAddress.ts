/**
 * KPS address parsing, vendored from `@kpstreams/core` so tor-js carries no
 * *runtime* dependency on that package — only its erasable types. This lets an
 * embedder that injects its own KPS transport (e.g. a sandboxed worker granted
 * a KPS capability) mark `@kpstreams/*` external and ship a bundle with no KPS
 * client code. Keep in sync with `@kpstreams/core`'s address parser.
 *
 * Wire format: `<ip>:<udp-port>:<certhash>`, IPv6 hosts bracketed
 * (`[<ipv6>]:<port>:<certhash>`, since the literal itself contains colons).
 */

export interface KpsAddress {
  ip: string;
  port: number;
  certhash: string;
}

export function parseAddress(s: string): KpsAddress {
  const malformed = () =>
    new Error(
      `address: malformed (expected <ip>:<port>:<certhash> or [ipv6]:<port>:<certhash>): ${s}`,
    );
  let ip: string;
  let rest: string;
  if (s.startsWith('[')) {
    const end = s.indexOf(']');
    if (end < 0 || s[end + 1] !== ':') throw malformed();
    ip = s.slice(1, end);
    rest = s.slice(end + 2);
  } else {
    const i = s.indexOf(':');
    if (i < 0) throw malformed();
    ip = s.slice(0, i);
    rest = s.slice(i + 1);
  }
  // rest is "<port>:<certhash>"; the certhash never contains ':'.
  const j = rest.indexOf(':');
  if (j < 0) throw malformed();
  const portStr = rest.slice(0, j);
  const certhash = rest.slice(j + 1);
  if (!/^\d+$/.test(portStr)) throw malformed(); // reject 0x1bb, 1e3, whitespace, etc.
  const port = Number(portStr);
  if (port < 1 || port > 65535) throw new Error('address: port out of range');
  if (!ip || !certhash) throw malformed();
  return { ip, port, certhash };
}

export function formatAddress(addr: KpsAddress): string {
  const host = addr.ip.includes(':') ? `[${addr.ip}]` : addr.ip;
  return `${host}:${addr.port}:${addr.certhash}`;
}
