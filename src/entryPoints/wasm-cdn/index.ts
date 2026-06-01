// Entry point: tor-js (default), tor-js/wasm-cdn
// Downloads WASM from CDN with SHA256 hash verification.
// Caches the downloaded WASM using createAutoStorage for faster subsequent loads.
// The GitHub CDN source stores AES-256-GCM encrypted files keyed by the WASM hash,
// with filenames derived from hash(hash) so the key isn't revealed by the URL.

import { setWasmSourceProvider } from '../../wasm.js';
import { createAutoStorage } from '../../storage/index.js';

export { TorClient } from '../../TorClient.js';
export * from '../../commonExports.js';

const CACHE_KEY = 'wasm';

function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < hex.length; i += 2) {
    bytes[i / 2] = parseInt(hex.slice(i, i + 2), 16);
  }
  return bytes;
}

function bytesToBase64(bytes: Uint8Array): string {
  if (typeof Buffer !== 'undefined') {
    return Buffer.from(bytes).toString('base64');
  }
  let binary = '';
  for (let i = 0; i < bytes.length; i++) {
    binary += String.fromCharCode(bytes[i]);
  }
  return btoa(binary);
}

function base64ToBytes(base64: string): Uint8Array {
  if (typeof Buffer !== 'undefined') {
    return new Uint8Array(Buffer.from(base64, 'base64'));
  }
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

async function sha256hex(bytes: ArrayBuffer | Uint8Array): Promise<string> {
  if (typeof crypto !== 'undefined' && crypto.subtle) {
    const buf = bytes instanceof Uint8Array ? bytes.buffer as ArrayBuffer : bytes;
    const hashBuf = await crypto.subtle.digest('SHA-256', buf);
    return [...new Uint8Array(hashBuf)].map(b => b.toString(16).padStart(2, '0')).join('');
  }
  // Node.js fallback
  const { createHash } = await import('node:crypto');
  return createHash('sha256').update(new Uint8Array(bytes)).digest('hex');
}

async function decryptAesGcm(encrypted: ArrayBuffer, keyBytes: Uint8Array): Promise<ArrayBuffer> {
  const iv = keyBytes.slice(0, 12);

  if (typeof crypto !== 'undefined' && crypto.subtle) {
    const key = await crypto.subtle.importKey('raw', keyBytes.buffer as ArrayBuffer, 'AES-GCM', false, ['decrypt']);
    return crypto.subtle.decrypt({ name: 'AES-GCM', iv }, key, encrypted);
  }

  // Node.js fallback — crypto.subtle output is ciphertext || 16-byte auth tag
  const { createDecipheriv } = await import('node:crypto');
  const data = new Uint8Array(encrypted);
  const authTag = data.slice(-16);
  const ciphertext = data.slice(0, -16);
  const decipher = createDecipheriv('aes-256-gcm', keyBytes, iv);
  decipher.setAuthTag(authTag);
  const decrypted = Buffer.concat([decipher.update(ciphertext), decipher.final()]);
  return decrypted.buffer.slice(decrypted.byteOffset, decrypted.byteOffset + decrypted.byteLength);
}

setWasmSourceProvider(async () => {
  let cache: ReturnType<typeof createAutoStorage> | undefined;
  try {
    cache = createAutoStorage('tor-js-wasm');
  } catch {
    // No persistent storage available — proceed without caching
  }

  // Try loading from cache
  if (cache) {
    try {
      const cached = await cache.get(CACHE_KEY);
      if (cached) {
        const bytes = base64ToBytes(cached);
        const hash = await sha256hex(bytes);
        if (hash === __WASM_SHA256__) {
          return bytes;
        }
        // Hash mismatch — stale cache, delete it
        await cache.delete(CACHE_KEY);
      }
    } catch {
      // Cache read failed — proceed to download
    }
  }

  // Download from CDN
  const hashBytes = hexToBytes(__WASM_SHA256__);
  const hashHash = await sha256hex(hashBytes);
  const hashHashPrefix = hashHash.slice(0, 2);

  const githubBase = `https://raw.githubusercontent.com/voltrevo/arti/hash-artifacts/`;

  type Source = { urls: string[], encrypted: boolean };

  const sources: Source[] = [
    { urls: [`https://cdn.jsdelivr.net/npm/tor-js@${__PACKAGE_VERSION__}/dist/tor_js_bg.wasm`], encrypted: false },
    { urls: [`https://unpkg.com/tor-js@${__PACKAGE_VERSION__}/dist/tor_js_bg.wasm`], encrypted: false },
    { urls: [`${githubBase}${hashHashPrefix}/${hashHash}`, `${githubBase}tmp/${hashHash}`], encrypted: true },
  ];

  // Shuffle sources for load balancing
  for (let i = sources.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [sources[i], sources[j]] = [sources[j], sources[i]];
  }

  const errors: string[] = [];
  for (const source of sources) {
    for (const url of source.urls) {
      try {
        const resp = await fetch(url);
        if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
        let bytes = await resp.arrayBuffer();

        if (source.encrypted) {
          bytes = await decryptAesGcm(bytes, hashBytes);
        }

        const hash = await sha256hex(bytes);
        if (hash !== __WASM_SHA256__) {
          throw new Error(`SHA256 mismatch: expected ${__WASM_SHA256__}, got ${hash}`);
        }

        const result = new Uint8Array(bytes);

        // Cache for next time (fire and forget)
        if (cache) {
          cache.set(CACHE_KEY, bytesToBase64(result)).catch(() => {});
        }

        return result;
      } catch (err) {
        errors.push(`${url}: ${err instanceof Error ? err.message : err}`);
      }
    }
  }

  throw new Error(`Failed to load WASM from any CDN:\n  ${errors.join('\n  ')}`);
});
