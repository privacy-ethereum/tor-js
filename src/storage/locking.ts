import type { TorStorage } from '#wasm';
import { getNodeDeps } from './node-deps.js';

/** Storage interface without locking — just the CRUD methods. */
export type TorStorageSimple = Omit<TorStorage, 'tryLock' | 'unlock'>;

function isNodeError(err: unknown): err is NodeJS.ErrnoException {
  return err instanceof Error && 'code' in err;
}

/**
 * Wrap a simple storage with platform-detected locking.
 *
 * - Browser: Web Locks API (`navigator.locks`)
 * - Node.js: lock file at `~/.local/share/${name}/.lock`
 *
 * If the real lock can't be acquired (another tab/process holds it),
 * the wrapper degrades gracefully: reads fall through to the inner storage,
 * writes go to an in-memory overlay, and `tryLock()` still returns `true`.
 */
export function addLocking(inner: TorStorageSimple, name: string): TorStorage {
  let hasRealLock = false;
  let overlay: Map<string, string | null> | null = null;

  // Web Locks state
  let releaseLock: (() => void) | undefined;
  let lockRequestDone: Promise<unknown> | undefined;

  // Filesystem lock state
  let lockPath: string | null = null;
  let exitHandler: (() => void) | null = null;
  let heartbeatTimer: ReturnType<typeof setInterval> | null = null;

  const STALE_MS = 30_000;
  const HEARTBEAT_MS = 10_000;

  async function tryAcquireReal(): Promise<boolean> {
    // Browser
    if (typeof navigator !== 'undefined' && navigator.locks) {
      let resolveAcquired!: (v: boolean) => void;
      const acquired = new Promise<boolean>(r => { resolveAcquired = r; });

      lockRequestDone = navigator.locks.request(
        `tor-js:${name}`,
        { ifAvailable: true },
        (lock) => {
          if (lock) {
            resolveAcquired(true);
            return new Promise<void>(r => { releaseLock = r; });
          }
          resolveAcquired(false);
        },
      );

      return acquired;
    }

    // Node.js
    if (typeof process !== 'undefined' && process.versions?.node) {
      try {
        const { fs, fsSync, path, os } = await getNodeDeps();
        const dir = path.join(os.homedir(), '.local', 'share', name);
        await fs.mkdir(dir, { recursive: true });
        const lp = path.join(dir, '.lock');

        // Try exclusive create
        try {
          await fs.writeFile(lp, `${process.pid}`, { flag: 'wx' });
        } catch (err) {
          if (!isNodeError(err) || err.code !== 'EEXIST') throw err;
          // Check if the existing lock is stale (mtime older than STALE_MS)
          const stat = await fs.stat(lp);
          if (Date.now() - stat.mtimeMs < STALE_MS) return false;
          // Stale — take over
          await fs.writeFile(lp, `${process.pid}`);
        }

        lockPath = lp;

        // Heartbeat: touch the lock file periodically so others know we're alive
        heartbeatTimer = setInterval(async () => {
          try {
            const now = new Date();
            await fs.utimes(lp, now, now);
          } catch {}
        }, HEARTBEAT_MS);
        if (heartbeatTimer.unref) heartbeatTimer.unref();

        exitHandler = () => {
          try { fsSync.unlinkSync(lp); } catch {}
        };
        process.on('exit', exitHandler);

        return true;
      } catch (err) {
        return false;
      }
    }

    throw new Error("Failed to detect suitable locking mechanism");
  }

  async function releaseReal(): Promise<void> {
    // Web Locks — resolve the held promise, then wait for the browser
    // to actually free the lock before returning.
    if (releaseLock) {
      releaseLock();
      releaseLock = undefined;
      await lockRequestDone;
      lockRequestDone = undefined;
    }

    // Filesystem
    if (heartbeatTimer) {
      clearInterval(heartbeatTimer);
      heartbeatTimer = null;
    }
    if (lockPath) {
      const { fs } = await getNodeDeps();
      try {
        await fs.unlink(lockPath);
      } catch (err) {
        if (!isNodeError(err) || err.code !== 'ENOENT') throw err;
      }
      lockPath = null;
    }
    if (exitHandler) {
      process.removeListener('exit', exitHandler);
      exitHandler = null;
    }
  }

  return {
    async get(key: string): Promise<string | null> {
      if (overlay?.has(key)) return overlay.get(key)!;
      return inner.get(key);
    },

    async set(key: string, value: string): Promise<void> {
      if (overlay) {
        overlay.set(key, value);
        return;
      }
      return inner.set(key, value);
    },

    async delete(key: string): Promise<void> {
      if (overlay) {
        overlay.set(key, null);
        return;
      }
      return inner.delete(key);
    },

    async keys(prefix: string): Promise<string[]> {
      const base = await inner.keys(prefix);
      if (!overlay) return base;
      const result = new Set(base);
      for (const [k, v] of overlay) {
        if (!k.startsWith(prefix)) continue;
        if (v !== null) result.add(k);
        else result.delete(k);
      }
      return [...result].sort();
    },

    async getAll(prefix: string): Promise<[string, string][]> {
      const base = await inner.getAll(prefix);
      if (!overlay) return base;
      const merged = new Map<string, string>(base);
      for (const [k, v] of overlay) {
        if (!k.startsWith(prefix)) continue;
        if (v !== null) merged.set(k, v);
        else merged.delete(k);
      }
      return [...merged.entries()];
    },

    async tryLock(): Promise<boolean> {
      if (hasRealLock) return false;

      const acquired = await tryAcquireReal();
      hasRealLock = acquired;
      overlay = acquired ? null : (overlay ?? new Map());

      return true;
    },

    async unlock(): Promise<void> {
      await releaseReal();
      hasRealLock = false;
      overlay = null;
    },
  };
}