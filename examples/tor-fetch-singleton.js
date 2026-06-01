#!/usr/bin/env node

// Make an HTTP request through Tor using the singleton API
//
// Build:   npm run build
// Usage:   examples/tor-fetch-singleton.js [url]
// Example: examples/tor-fetch-singleton.js https://check.torproject.org/api/ip

import { tor, Log } from '../dist/entryPoints/wasm-base64/singleton.js';

const url = process.argv[2] ?? 'https://check.torproject.org/api/ip';

console.log(`Fetching ${url} via Tor...`);

tor.configure({
  // log: new Log(),
});

const response = await tor.fetch(url);
const text = await response.text();

console.log(`Status: ${response.status}`);
console.log(text);

tor.close();
