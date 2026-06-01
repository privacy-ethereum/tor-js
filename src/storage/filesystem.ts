import type { TorStorageSimple } from './locking.js';
import { getNodeDeps } from './node-deps.js';

function isNodeError(err: unknown): err is NodeJS.ErrnoException {
  return err instanceof Error && 'code' in err;
}

/**
 * Encode a storage key into a filesystem-safe filename.
 * Alphanumeric characters pass through; everything else becomes _XX_ or _XXXX_.
 */
function mangleKey(key: string): string {
  let result = '';
  for (let i = 0; i < key.length; i++) {
    const code = key.charCodeAt(i);
    if (
      (code >= 97 && code <= 122) || // a-z
      (code >= 65 && code <= 90)  || // A-Z
      (code >= 48 && code <= 57)     // 0-9
    ) {
      result += key[i];
    } else if (code <= 0xff) {
      result += '_' + code.toString(16).padStart(2, '0') + '_';
    } else {
      result += '_' + code.toString(16).padStart(4, '0') + '_';
    }
  }
  return result;
}

/**
 * Decode a mangled filename back to the original key.
 */
function unmangleKey(filename: string): string {
  let result = '';
  let i = 0;
  while (i < filename.length) {
    if (filename[i] === '_') {
      // Try _XXXX_ (4-digit)
      if (i + 5 < filename.length && filename[i + 5] === '_') {
        const hex = filename.slice(i + 1, i + 5);
        if (/^[0-9a-f]{4}$/i.test(hex)) {
          result += String.fromCharCode(parseInt(hex, 16));
          i += 6;
          continue;
        }
      }
      // Try _XX_ (2-digit)
      if (i + 3 < filename.length && filename[i + 3] === '_') {
        const hex = filename.slice(i + 1, i + 3);
        if (/^[0-9a-f]{2}$/i.test(hex)) {
          result += String.fromCharCode(parseInt(hex, 16));
          i += 4;
          continue;
        }
      }
      result += '_';
      i++;
    } else {
      result += filename[i];
      i++;
    }
  }
  return result;
}

export class FilesystemStorage implements TorStorageSimple {
  private dirPath: string | null;
  private name: string | null;
  private resolvedDirPath: string | null = null;
  private initialized = false;

  constructor(dirPath: string) {
    this.dirPath = dirPath;
    this.name = null;
  }

  static localShare(name: string): FilesystemStorage {
    const s = new FilesystemStorage('');
    s.dirPath = null;
    s.name = name;
    return s;
  }

  private async resolvedDir(): Promise<string> {
    if (!this.resolvedDirPath) {
      if (this.dirPath) {
        this.resolvedDirPath = this.dirPath;
      } else {
        const { os, path } = await getNodeDeps();
        this.resolvedDirPath = path.join(os.homedir(), '.local', 'share', this.name!);
      }
    }
    return this.resolvedDirPath!;
  }

  private async ensureDir(): Promise<void> {
    if (!this.initialized) {
      const { fs } = await getNodeDeps();
      await fs.mkdir(await this.resolvedDir(), { recursive: true });
      this.initialized = true;
    }
  }

  private async filePath(key: string): Promise<string> {
    const { path } = await getNodeDeps();
    return path.join(await this.resolvedDir(), mangleKey(key));
  }

  async get(key: string): Promise<string | null> {
    const { fs } = await getNodeDeps();
    await this.ensureDir();
    try {
      return await fs.readFile(await this.filePath(key), 'utf-8');
    } catch (err) {
      if (isNodeError(err) && err.code === 'ENOENT') return null;
      throw err;
    }
  }

  async set(key: string, value: string): Promise<void> {
    const { fs } = await getNodeDeps();
    await this.ensureDir();
    await fs.writeFile(await this.filePath(key), value, 'utf-8');
  }

  async delete(key: string): Promise<void> {
    const { fs } = await getNodeDeps();
    await this.ensureDir();
    try {
      await fs.unlink(await this.filePath(key));
    } catch (err) {
      if (isNodeError(err) && err.code === 'ENOENT') return;
      throw err;
    }
  }

  async keys(prefix: string): Promise<string[]> {
    const { fs } = await getNodeDeps();
    await this.ensureDir();
    try {
      const files = await fs.readdir(await this.resolvedDir());
      return files
        .map(unmangleKey)
        .filter(k => k.startsWith(prefix))
        .sort();
    } catch (err) {
      if (isNodeError(err) && err.code === 'ENOENT') return [];
      throw err;
    }
  }

  async getAll(prefix: string): Promise<[string, string][]> {
    const { fs } = await getNodeDeps();
    await this.ensureDir();
    try {
      const files = await fs.readdir(await this.resolvedDir());
      const keys = files.map(unmangleKey).filter(k => k.startsWith(prefix));
      const entries = await Promise.all(
        keys.map(async (key): Promise<[string, string] | null> => {
          const value = await this.get(key);
          return value !== null ? [key, value] : null;
        })
      );
      return entries.filter((e): e is [string, string] => e !== null);
    } catch (err) {
      if (isNodeError(err) && err.code === 'ENOENT') return [];
      throw err;
    }
  }
}
