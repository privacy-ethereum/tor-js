export type NodeDeps = {
  fs: typeof import('node:fs/promises');
  fsSync: typeof import('node:fs');
  os: typeof import('node:os');
  path: typeof import('node:path');
};

let promise: Promise<NodeDeps> | undefined;

export function getNodeDeps(): Promise<NodeDeps> {
  if (!promise) {
    promise = (async () => {
      const [fs, fsSync, os, path] = await Promise.all([
        import('node:fs/promises').then(m => m.default ?? m),
        import('node:fs').then(m => m.default ?? m),
        import('node:os').then(m => m.default ?? m),
        import('node:path').then(m => m.default ?? m),
      ]);
      return { fs, fsSync, os, path };
    })();
  }
  return promise;
}
