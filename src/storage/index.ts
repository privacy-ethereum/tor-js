export { MemoryStorage } from './memory.js';
export { IndexedDBStorage } from './indexeddb.js';
export { FilesystemStorage } from './filesystem.js';
export { addLocking, type TorStorageSimple } from './locking.js';

import type { TorStorage } from '#wasm';
import { IndexedDBStorage } from './indexeddb.js';
import { FilesystemStorage } from './filesystem.js';
import { addLocking } from './locking.js';

export function createAutoStorage(name: string = 'tor-js'): TorStorage {
  if (typeof globalThis !== 'undefined' && typeof globalThis.indexedDB !== 'undefined') {
    return addLocking(new IndexedDBStorage(name), name);
  }
  if (typeof process !== 'undefined' && process.versions?.node) {
    return addLocking(FilesystemStorage.localShare(name), name);
  }
  throw new Error(
    'No persistent storage available: need IndexedDB (browser) or filesystem (Node.js)',
  );
}
