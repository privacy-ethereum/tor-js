import type { TorStorage } from '#wasm';

export class MemoryStorage implements TorStorage {
  private data = new Map<string, string>();
  private locked = false;

  async get(key: string): Promise<string | null> {
    return this.data.get(key) ?? null;
  }

  async set(key: string, value: string): Promise<void> {
    this.data.set(key, value);
  }

  async delete(key: string): Promise<void> {
    this.data.delete(key);
  }

  async keys(prefix: string): Promise<string[]> {
    return [...this.data.keys()].filter(k => k.startsWith(prefix)).sort();
  }

  async getAll(prefix: string): Promise<[string, string][]> {
    const result: [string, string][] = [];
    for (const [key, value] of this.data) {
      if (key.startsWith(prefix)) {
        result.push([key, value]);
      }
    }
    return result;
  }

  async tryLock(): Promise<boolean> {
    if (this.locked) return false;
    this.locked = true;
    return true;
  }

  async unlock(): Promise<void> {
    this.locked = false;
  }
}
