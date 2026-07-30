// Unit tests for the storage layer:
//   - the filename mangling that keys map through on disk
//   - FilesystemStorage CRUD against a temp directory
//   - the degraded-mode overlay in addLocking(), and Node lock-file takeover
//
// No network. Filesystem tests use a temp dir; the lock tests point HOME at one,
// since the Node lock path is derived from the home directory.
//
//   npm run test:unit

import { test, before, after, describe } from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import { bundleTs } from './bundle.mjs'

let mangleKey, unmangleKey, FilesystemStorage, MemoryStorage, addLocking

const tmpRoot = path.join(os.tmpdir(), `tor-js-storage-test-${process.pid}`)
let nextDir = 0
const freshDir = async () => {
  const dir = path.join(tmpRoot, `d${nextDir++}`)
  await fs.mkdir(dir, { recursive: true })
  return dir
}

before(async () => {
  ;({ mangleKey, unmangleKey, FilesystemStorage } = await bundleTs(
    'src/storage/filesystem.ts',
    'filesystem',
  ))
  ;({ MemoryStorage, addLocking } = await bundleTs('src/storage/index.ts', 'storageIndex'))
  await fs.mkdir(tmpRoot, { recursive: true })
})

after(async () => {
  await fs.rm(tmpRoot, { recursive: true, force: true })
})

// ---------------------------------------------------------------------------

describe('key mangling', () => {
  test('alphanumerics pass through untouched', () => {
    assert.equal(mangleKey('abcXYZ019'), 'abcXYZ019')
    assert.equal(unmangleKey('abcXYZ019'), 'abcXYZ019')
  })

  test('everything else is hex-escaped', () => {
    assert.equal(mangleKey(':'), '_3a_')
    assert.equal(mangleKey('_'), '_5f_')
    assert.equal(mangleKey('/'), '_2f_')
    assert.equal(mangleKey('.'), '_2e_')
    // Above U+00FF the escape widens to four digits.
    assert.equal(mangleKey('€'), '_20ac_')
  })

  test('the escapes keep real keys off the filesystem grammar', () => {
    // Path separators and traversal can never survive into a filename.
    for (const key of ['../etc/passwd', 'a/b', 'a\\b', '.', '..', 'con:', 'a\0b']) {
      const name = mangleKey(key)
      assert.ok(!name.includes('/'), `${key} -> ${name}`)
      assert.ok(!name.includes('\\'), `${key} -> ${name}`)
      assert.ok(!name.includes('\0'), `${key} -> ${name}`)
      assert.ok(name !== '.' && name !== '..', `${key} -> ${name}`)
      assert.equal(unmangleKey(name), key)
    }
  })

  test('the keys arti actually uses round-trip', () => {
    for (const key of [
      'dir:consensus:microdesc',
      'dir:md:0123abcdef',
      'dir:authcert:AAAA+BBBB/CCCC',
      'state/guards.json',
    ]) {
      assert.equal(unmangleKey(mangleKey(key)), key, key)
    }
  })

  // The escape delimiter is itself escaped, so a key that looks like an escape
  // must not be confused for one. This is where a naive decoder breaks.
  test('keys shaped like escapes round-trip', () => {
    for (const key of [
      '_',
      '__',
      '_3a_',
      '_5f_',
      '_20ac_',
      'a_00e9_b',
      '_ff_',
      '_zz_',
      '____',
      '_3a',
      '3a_',
      '_3',
    ]) {
      assert.equal(unmangleKey(mangleKey(key)), key, JSON.stringify(key))
    }
  })

  test('round-trips a fuzz corpus', () => {
    // Deterministic pseudo-random keys over a deliberately nasty alphabet.
    const alphabet = [...'ab09_:./\\-%é€\u{1f600}\0 \t"\'']
    let seed = 0x2f6e2b1
    const rand = () => {
      seed = (seed * 1103515245 + 12345) & 0x7fffffff
      return seed / 0x7fffffff
    }
    for (let i = 0; i < 2000; i++) {
      const len = 1 + Math.floor(rand() * 12)
      let key = ''
      for (let j = 0; j < len; j++) key += alphabet[Math.floor(rand() * alphabet.length)]
      assert.equal(unmangleKey(mangleKey(key)), key, JSON.stringify(key))
    }
  })

  test('an empty key round-trips', () => {
    assert.equal(mangleKey(''), '')
    assert.equal(unmangleKey(''), '')
  })

  test('mangling is stable', () => {
    // Filenames are the storage identity: the same key must always map to the
    // same file, or a version bump silently orphans the cache.
    assert.equal(mangleKey('dir:consensus:microdesc'), 'dir_3a_consensus_3a_microdesc')
  })
})

// ---------------------------------------------------------------------------

describe('FilesystemStorage', () => {
  test('stores, reads back, and deletes', async () => {
    const s = new FilesystemStorage(await freshDir())
    assert.equal(await s.get('missing'), null)

    await s.set('dir:a', 'value-a')
    assert.equal(await s.get('dir:a'), 'value-a')

    await s.set('dir:a', 'replaced')
    assert.equal(await s.get('dir:a'), 'replaced')

    await s.delete('dir:a')
    assert.equal(await s.get('dir:a'), null)
  })

  test('deleting something absent is not an error', async () => {
    const s = new FilesystemStorage(await freshDir())
    await s.delete('never-existed')
  })

  test('creates its directory on first use', async () => {
    const dir = path.join(await freshDir(), 'not', 'yet')
    const s = new FilesystemStorage(dir)
    await s.set('k', 'v')
    assert.equal(await s.get('k'), 'v')
  })

  test('keys() filters by prefix and sorts', async () => {
    const s = new FilesystemStorage(await freshDir())
    await s.set('dir:b', '2')
    await s.set('dir:a', '1')
    await s.set('other:c', '3')

    assert.deepEqual(await s.keys('dir:'), ['dir:a', 'dir:b'])
    assert.deepEqual(await s.keys('other:'), ['other:c'])
    assert.deepEqual(await s.keys('nothing:'), [])
    assert.deepEqual(await s.keys(''), ['dir:a', 'dir:b', 'other:c'])
  })

  test('getAll() returns matching entries', async () => {
    const s = new FilesystemStorage(await freshDir())
    await s.set('dir:a', '1')
    await s.set('dir:b', '2')
    await s.set('other:c', '3')

    const entries = await s.getAll('dir:')
    assert.deepEqual(entries.sort(), [
      ['dir:a', '1'],
      ['dir:b', '2'],
    ])
    assert.deepEqual(await s.getAll('nothing:'), [])
  })

  test('listing a directory that does not exist yields nothing', async () => {
    const s = new FilesystemStorage(path.join(await freshDir(), 'absent'))
    // ensureDir creates it, so this is really "no files yet".
    assert.deepEqual(await s.keys(''), [])
    assert.deepEqual(await s.getAll(''), [])
  })

  test('values survive characters that need escaping in the key', async () => {
    const s = new FilesystemStorage(await freshDir())
    const keys = ['a/b', '../x', '_5f_', 'a€b', 'dir:md:AA+BB/CC']
    for (const [i, k] of keys.entries()) await s.set(k, `v${i}`)
    for (const [i, k] of keys.entries()) assert.equal(await s.get(k), `v${i}`, k)
    assert.deepEqual((await s.keys('')).sort(), [...keys].sort())
  })

  test('unicode values round-trip as UTF-8', async () => {
    const s = new FilesystemStorage(await freshDir())
    const value = 'consensus\n€\u{1f600}\ttail'
    await s.set('k', value)
    assert.equal(await s.get('k'), value)
  })

  test('an empty value is not confused with a missing key', async () => {
    const s = new FilesystemStorage(await freshDir())
    await s.set('k', '')
    assert.equal(await s.get('k'), '')
    assert.deepEqual(await s.keys(''), ['k'])
    // getAll skips null (missing) but must keep the empty string.
    assert.deepEqual(await s.getAll(''), [['k', '']])
  })
})

// ---------------------------------------------------------------------------

describe('addLocking', () => {
  let realHome
  let homeDir

  before(async () => {
    realHome = process.env.HOME
  })

  after(() => {
    if (realHome === undefined) delete process.env.HOME
    else process.env.HOME = realHome
  })

  // Each test gets its own HOME so the lock path is isolated.
  const withHome = async () => {
    homeDir = await freshDir()
    process.env.HOME = homeDir
    return homeDir
  }

  const lockPath = (name) => path.join(homeDir, '.local', 'share', name, '.lock')

  test('the first holder writes through to the inner store', async () => {
    await withHome()
    const inner = new MemoryStorage()
    const s = addLocking(inner, 'first-holder')

    assert.equal(await s.tryLock(), true)
    await s.set('k', 'v')
    assert.equal(await inner.get('k'), 'v', 'not diverted to an overlay')
    assert.equal(await s.get('k'), 'v')

    await s.unlock()
  })

  test('acquiring creates the lock file and unlock removes it', async () => {
    await withHome()
    const s = addLocking(new MemoryStorage(), 'lockfile')

    await s.tryLock()
    const lp = lockPath('lockfile')
    assert.equal(await fs.readFile(lp, 'utf-8'), String(process.pid))

    await s.unlock()
    await assert.rejects(fs.stat(lp), /ENOENT/)
  })

  test('a second holder degrades to an overlay but still reports success', async () => {
    await withHome()
    const inner = new MemoryStorage()
    await inner.set('shared', 'from-disk')

    const first = addLocking(inner, 'contended')
    assert.equal(await first.tryLock(), true)

    const second = addLocking(inner, 'contended')
    // tryLock always resolves true: callers must be able to proceed read-only.
    assert.equal(await second.tryLock(), true)

    // Reads fall through to the inner store...
    assert.equal(await second.get('shared'), 'from-disk')
    // ...writes do not reach it.
    await second.set('shared', 'overlaid')
    assert.equal(await second.get('shared'), 'overlaid')
    assert.equal(await inner.get('shared'), 'from-disk', 'inner store untouched')

    await first.unlock()
    await second.unlock()
  })

  test('overlay deletes are tombstones, not writes', async () => {
    await withHome()
    const inner = new MemoryStorage()
    await inner.set('a', '1')
    await inner.set('b', '2')

    const holder = addLocking(inner, 'tombstones')
    await holder.tryLock()
    const degraded = addLocking(inner, 'tombstones')
    await degraded.tryLock()

    await degraded.delete('a')
    assert.equal(await degraded.get('a'), null)
    assert.equal(await inner.get('a'), '1', 'the inner value stays')

    // A tombstoned key disappears from both listings.
    assert.deepEqual(await degraded.keys(''), ['b'])
    assert.deepEqual(await degraded.getAll(''), [['b', '2']])

    await holder.unlock()
    await degraded.unlock()
  })

  test('overlay writes appear in listings, merged with the inner store', async () => {
    await withHome()
    const inner = new MemoryStorage()
    await inner.set('dir:a', '1')

    const holder = addLocking(inner, 'listings')
    await holder.tryLock()
    const degraded = addLocking(inner, 'listings')
    await degraded.tryLock()

    await degraded.set('dir:b', '2')
    await degraded.set('other:c', '3')

    assert.deepEqual(await degraded.keys('dir:'), ['dir:a', 'dir:b'])
    assert.deepEqual(await degraded.keys(''), ['dir:a', 'dir:b', 'other:c'])
    // Prefix filtering applies to overlay entries too.
    assert.deepEqual(await degraded.keys('other:'), ['other:c'])

    const all = await degraded.getAll('dir:')
    assert.deepEqual(new Map(all), new Map([['dir:a', '1'], ['dir:b', '2']]))

    await holder.unlock()
    await degraded.unlock()
  })

  test('an overlay write shadows the inner value in getAll', async () => {
    await withHome()
    const inner = new MemoryStorage()
    await inner.set('k', 'old')

    const holder = addLocking(inner, 'shadow')
    await holder.tryLock()
    const degraded = addLocking(inner, 'shadow')
    await degraded.tryLock()

    await degraded.set('k', 'new')
    assert.deepEqual(await degraded.getAll(''), [['k', 'new']])

    await holder.unlock()
    await degraded.unlock()
  })

  test('unlock drops the overlay, so writes reach the inner store again', async () => {
    await withHome()
    const inner = new MemoryStorage()

    const holder = addLocking(inner, 'drop-overlay')
    await holder.tryLock()
    const degraded = addLocking(inner, 'drop-overlay')
    await degraded.tryLock()

    await degraded.set('k', 'overlaid')
    assert.equal(await inner.get('k'), null)

    await degraded.unlock()
    await degraded.set('k', 'direct')
    assert.equal(await inner.get('k'), 'direct')

    await holder.unlock()
  })

  test('a second tryLock from the same holder reports false', async () => {
    await withHome()
    const s = addLocking(new MemoryStorage(), 'double-lock')
    assert.equal(await s.tryLock(), true)
    // Already holding it: this is how a caller learns it is re-entering.
    assert.equal(await s.tryLock(), false)
    await s.unlock()
  })

  test('a stale lock file is taken over', async () => {
    const home = await withHome()
    const name = 'stale'
    const dir = path.join(home, '.local', 'share', name)
    await fs.mkdir(dir, { recursive: true })
    const lp = path.join(dir, '.lock')

    // A lock left behind by a process that died: mtime older than the 30 s
    // staleness window.
    await fs.writeFile(lp, '999999')
    const old = new Date(Date.now() - 60_000)
    await fs.utimes(lp, old, old)

    const inner = new MemoryStorage()
    const s = addLocking(inner, name)
    assert.equal(await s.tryLock(), true)

    // Took the lock for real: writes go through rather than into an overlay.
    await s.set('k', 'v')
    assert.equal(await inner.get('k'), 'v')
    assert.equal(await fs.readFile(lp, 'utf-8'), String(process.pid))

    await s.unlock()
  })

  test('a fresh lock file is respected', async () => {
    const home = await withHome()
    const name = 'fresh'
    const dir = path.join(home, '.local', 'share', name)
    await fs.mkdir(dir, { recursive: true })
    await fs.writeFile(path.join(dir, '.lock'), '999999')

    const inner = new MemoryStorage()
    const s = addLocking(inner, name)
    assert.equal(await s.tryLock(), true)
    await s.set('k', 'v')
    assert.equal(await inner.get('k'), null, 'should have degraded to an overlay')

    await s.unlock()
  })

  test('unlock is safe when the lock file has already gone', async () => {
    await withHome()
    const s = addLocking(new MemoryStorage(), 'vanished')
    await s.tryLock()
    await fs.rm(lockPath('vanished'))
    await s.unlock()
  })

  test('unlock without a lock is a no-op', async () => {
    await withHome()
    const s = addLocking(new MemoryStorage(), 'never-locked')
    await s.unlock()
  })

  /// The lock file lives in the storage directory, so an unprefixed listing of a
  /// FilesystemStorage sees it. Every caller queries with a prefix, which is
  /// what keeps it out of the way.
  test('the lock file does not answer to any real key prefix', async () => {
    const home = await withHome()
    const name = 'lock-visibility'
    const dir = path.join(home, '.local', 'share', name)
    const inner = new FilesystemStorage(dir)
    const s = addLocking(inner, name)
    await s.tryLock()
    await s.set('dir:consensus:microdesc', 'c')

    assert.deepEqual(await s.keys('dir:'), ['dir:consensus:microdesc'])
    assert.deepEqual(await s.keys('state'), [])
    assert.ok((await s.keys('')).includes('.lock'), 'documents the unprefixed case')

    await s.unlock()
  })
})
