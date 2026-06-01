#!/usr/bin/env node

// Stream a large file through Tor, counting text matches in real-time
//
// Demonstrates the streaming Response API: fetch() resolves on headers,
// then body chunks are processed incrementally — nothing is buffered.
//
// Build:   npm run build
// Usage:   examples/tor-fetch-streaming.js [url] [pattern]
// Example: examples/tor-fetch-streaming.js https://norvig.com/big.txt the

import { TorClient, Log } from '../dist/entryPoints/wasm-base64/index.js';

async function main() {
  const url = process.argv[2] ?? 'https://www.gutenberg.org/cache/epub/100/pg100.txt';
  const pattern = process.argv[3] ?? 'the';
  const regex = new RegExp(pattern, 'gi');

  const log = new Log();

  log.info();
  log.info(`Creating TorClient...`);

  const startTime = performance.now();

  const client = new TorClient({
    log,
  });

  await client.ready();

  const connectTime = ((performance.now() - startTime) / 1000).toFixed(1);
  log.info();
  log.info(`Connected in ${connectTime}s`);
  log.info(`Streaming ${url}`);
  log.info(`Counting case-insensitive matches of /${pattern}/`);

  const fetchStart = performance.now();
  const response = await client.fetch(url);

  const headersTime = ((performance.now() - fetchStart) / 1000).toFixed(1);
  const contentLength = response.headers.get('content-length');
  log.info(`Headers received in ${headersTime}s (status ${response.status})`);
  if (contentLength) {
    log.info(`Content-Length: ${(contentLength / 1024 / 1024).toFixed(1)} MB`);
  }
  log.info();

  // Stream body chunk-by-chunk, counting matches without buffering
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let totalBytes = 0;
  let matches = 0;
  let chunks = 0;

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    chunks++;
    totalBytes += value.byteLength;
    const text = decoder.decode(value, { stream: true });
    matches += (text.match(regex) || []).length;

    const mb = (totalBytes / 1024 / 1024).toFixed(2);
    const pct = contentLength ? ` (${(totalBytes / contentLength * 100).toFixed(0)}%)` : '';
    process.stdout.write(`\r  ${mb} MB${pct} | ${matches} matches | ${chunks} chunks`);
  }

  const totalTime = ((performance.now() - fetchStart) / 1000).toFixed(1);
  const throughput = (totalBytes / 1024 / ((performance.now() - fetchStart) / 1000)).toFixed(1);

  log.info();
  log.info(`Done in ${totalTime}s`);
  log.info(`  ${(totalBytes / 1024 / 1024).toFixed(2)} MB in ${chunks} chunks`);
  log.info(`  ${matches} matches of /${pattern}/`);
  log.info(`  ${throughput} KB/s throughput`);

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