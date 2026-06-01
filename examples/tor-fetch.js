#!/usr/bin/env node

// Make an HTTP request through Tor with persistent filesystem storage
//
// Build:   npm run build
// Usage:   examples/tor-fetch.js [--websocket <gateway-url>] [--in-memory] [url]
// Example: examples/tor-fetch.js --websocket https://tor-js-gateway.HOSTME.com https://check.torproject.org/api/ip
//
// State is persisted to ~/.local/share/tor-js/
// Subsequent runs will load cached state for faster bootstrap.

import { TorClient, Log, ArtiSocketProvider, storage } from '../dist/entryPoints/wasm-base64/index.js';

function parseArgs(argv) {
  const args = argv.slice(2);
  let websocketGateway;
  let inMemory = false;
  let url = 'https://check.torproject.org/api/ip';

  for (let i = 0; i < args.length; i++) {
    if (args[i] === '--websocket' && i + 1 < args.length) {
      websocketGateway = args[++i];
    } else if (args[i] === '--in-memory') {
      inMemory = true;
    } else {
      url = args[i];
    }
  }

  return { websocketGateway, inMemory, url };
}

async function main() {
  const { websocketGateway, inMemory, url } = parseArgs(process.argv);

  const log = new Log();

  log.info();
  log.info(`Creating TorClient with persistent storage...`);

  const startTime = performance.now();

  const options = { log };
  if (inMemory) {
    options.storage = new storage.MemoryStorage();
  }
  if (websocketGateway) {
    options.gateway = websocketGateway;
    options.socketProvider = new ArtiSocketProvider({
      gateway: websocketGateway,
      strategies: ['websocket'],
    });
  }

  const client = new TorClient(options);

  await client.ready();

  const connectTime = ((performance.now() - startTime) / 1000).toFixed(1);
  log.info();
  log.info(`Connected in ${connectTime}s, fetching ${url}...`);

  // Make fetch request
  const fetchStart = performance.now();
  const response = await client.fetch(url);
  const responseText = await response.text();
  const fetchTime = ((performance.now() - fetchStart) / 1000).toFixed(1);

  // Cleanup
  client.close();

  // Wait just a little bit so that the last log is our output
  await new Promise(resolve => setTimeout(resolve, 50));

  log.info();
  log.info(`Status: ${response.status}`);
  log.info(`Connect time: ${connectTime}s`);
  log.info(`Fetch time: ${fetchTime}s`);
  log.info('Response:');
  log.info(responseText);

  // This does not block exit (see .unref), but logs if nodejs stays alive
  // without exiting gracefully. This suggests resource use (presumably in arti)
  // that persists beyond .close().
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
