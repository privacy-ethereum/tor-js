#!/usr/bin/env node

// Test AbortSignal support in tor-js fetch
//
// Exercises three scenarios:
//   1. Pre-aborted signal  — rejected before any Tor work starts
//   2. Abort mid-stream    — body cancelled between chunks
//   3. Unused signal       — normal fetch with a signal that is never fired
//
// Build:   npm run build
// Usage:   examples/tor-fetch-abort.js

import { TorClient, Log } from '../dist/entryPoints/wasm-base64/index.js';

function pass(label) { console.log(`  [PASS] ${label}`); }
function fail(label, detail) { console.error(`  [FAIL] ${label}: ${detail}`); process.exitCode = 1; }

async function main() {
  const log = new Log();

  log.info('Creating TorClient...');
  const startTime = performance.now();
  const client = new TorClient({
    log,
  });
  await client.ready();
  const connectTime = ((performance.now() - startTime) / 1000).toFixed(1);
  log.info(`Connected in ${connectTime}s`);
  log.info('');

  // ── Test 1: pre-aborted signal ──────────────────────────────────────────────
  // signal.aborted is already true when fetch() is called; should fail at the
  // first check_aborted() call, before any Tor connection is attempted.
  log.info('Test 1: pre-aborted signal');
  {
    const controller = new AbortController();
    controller.abort();
    try {
      await client.fetch('https://check.torproject.org/api/ip', { signal: controller.signal });
      fail('pre-aborted', 'fetch resolved unexpectedly');
    } catch (err) {
      if (err?.code === 'ABORT') {
        pass(`rejected with code ABORT before connecting`);
      } else {
        fail('pre-aborted', `wrong error: ${JSON.stringify(err)}`);
      }
    }
  }
  log.info('');

  // ── Test 2: abort mid-stream ────────────────────────────────────────────────
  // Fetch a large file, abort after receiving the first body chunk.
  // The abort is detected at the next checkpoint (before the next read_chunk call).
  log.info('Test 2: abort mid-stream');
  {
    const controller = new AbortController();
    // A large text file (~5 MB) so body streaming takes multiple chunks/roundtrips
    const url = 'https://www.gutenberg.org/cache/epub/100/pg100.txt';

    const response = await client.fetch(url, { signal: controller.signal });
    log.info(`  Headers received (status ${response.status}), reading first chunk...`);

    const reader = response.body.getReader();
    let chunks = 0;
    let abortErr = null;

    // Read the first chunk, then abort
    const { done, value } = await reader.read();
    if (!done) {
      chunks++;
      log.info(`  Chunk ${chunks}: ${value.byteLength} bytes — aborting now`);
      controller.abort();
    }

    // The next read() should hit the aborted check and error the stream
    try {
      while (true) {
        const result = await reader.read();
        if (result.done) break;
        chunks++;
      }
    } catch (err) {
      abortErr = err;
    }

    if (abortErr?.code === 'ABORT') {
      pass(`stream aborted after ${chunks} chunk(s)`);
    } else if (abortErr) {
      fail('abort mid-stream', `unexpected error: ${JSON.stringify(abortErr)}`);
    } else {
      // Stream completed (file was tiny enough to arrive in one chunk — unlikely but OK)
      fail('abort mid-stream', `stream ended normally (${chunks} chunks); abort not detected`);
    }
  }
  log.info('');

  // ── Test 3: unused signal ───────────────────────────────────────────────────
  // Pass a live signal that is never fired; fetch should succeed normally.
  log.info('Test 3: normal fetch with unused signal');
  {
    const controller = new AbortController();
    try {
      const response = await client.fetch('https://check.torproject.org/api/ip', { signal: controller.signal });
      const data = JSON.parse(await response.text());
      if (data.IsTor === true) {
        pass(`fetch succeeded, IsTor: true`);
      } else {
        fail('unused signal', `unexpected response body: ${JSON.stringify(data)}`);
      }
    } catch (err) {
      fail('unused signal', `unexpected error: ${JSON.stringify(err)}`);
    }
  }

  client.close();

  await shutdown();
}

main().catch(err => {
  console.error(err);
  process.exit(1);
});

async function shutdown() {
  await softDelay(2000);
  console.log('\nWaiting up to 10s for graceful exit.');
  await softDelay(8000);
  console.warn('WARN: Graceful exit did not occur. Ignoring.\n')
  process.exit(0);
}

async function softDelay(ms) {
  return new Promise(resolve => {
    setTimeout(resolve, ms).unref();
  });
}
