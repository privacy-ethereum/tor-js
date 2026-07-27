/**
 * The built-in KPS dialer: WebRTC in browsers (`@kpstreams/webrtc-client`),
 * QUIC in Node/Deno (the optional `@kpstreams/quic-client` package).
 *
 * This lives in its own module — {@link KpsGateway} imports it *lazily*, and
 * only when no custom `dial` was injected — so an embedder that supplies its
 * own transport can mark `@kpstreams/*` external and ship a bundle that never
 * pulls in the KPS client code.
 */

import type { Connection } from '@kpstreams/core';

/** Opens a KPS connection to a gateway address (`ip:port:certhash`). */
export type DialFn = (address: string) => Promise<Connection>;

/**
 * Pick the KPS dialer for this environment: WebRTC in browsers, QUIC in
 * Node/Deno via the optional `@kpstreams/quic-client` package.
 */
export const kpsDial: DialFn = async (address) => {
  if (typeof (globalThis as any).RTCPeerConnection !== 'undefined') {
    const { dial } = await import('@kpstreams/webrtc-client');
    return dial(address);
  }
  // Non-literal specifier so bundlers don't try to resolve the optional
  // native package into browser builds.
  const quicClientPkg = '@kpstreams/quic-client';
  let mod: { dial: (addr: string) => Promise<Connection> };
  try {
    mod = await import(/* @vite-ignore */ quicClientPkg);
  } catch {
    throw new Error(
      'kps: no transport available. Browsers need RTCPeerConnection; ' +
      "in Node, install the optional '@kpstreams/quic-client' package to reach a gateway over QUIC.",
    );
  }
  return mod.dial(address);
};
