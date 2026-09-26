import { describe, it, expect } from 'vitest';
import { createDirectoryLoader } from './directoryLoader.js';

/** Deferred promise for deterministic out-of-order resolution. */
function deferred() {
  let resolve, reject;
  const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

function makeLoader(invokeFn) {
  const state = { entries: null, loading: null };
  const load = createDirectoryLoader(invokeFn, {
    setEntries: (v) => { state.entries = v; },
    setLoading: (v) => { state.loading = v; },
  });
  return { state, load };
}

describe('createDirectoryLoader', () => {
  it('loads and maps entries for the requested dir', async () => {
    const calls = [];
    const { state, load } = makeLoader(async (cmd, args) => {
      calls.push([cmd, args]);
      return [{ name: 'a.md', path: '/r/a.md', is_dir: false }];
    });
    await load('/root');
    expect(calls).toEqual([['list_directory', { dirPath: '/root' }]]);
    expect(state.entries).toEqual([
      { name: 'a.md', path: '/r/a.md', is_dir: false, children: null, expanded: false },
    ]);
    expect(state.loading).toBe(false);
  });

  it('a slow response for the old root must not overwrite the new root', async () => {
    // Regression: switching root A -> B quickly let A's late response
    // clobber B's entries.
    const pending = {};
    const { state, load } = makeLoader((_cmd, args) => {
      pending[args.dirPath] = deferred();
      return pending[args.dirPath].promise;
    });

    const loadA = load('/A');
    const loadB = load('/B');

    pending['/B'].resolve([{ name: 'b.md', path: '/B/b.md', is_dir: false }]);
    await loadB;
    expect(state.entries[0].path).toBe('/B/b.md');
    expect(state.loading).toBe(false);

    // A resolves late — must be dropped entirely.
    pending['/A'].resolve([{ name: 'stale.md', path: '/A/stale.md', is_dir: false }]);
    await loadA;
    expect(state.entries).toHaveLength(1);
    expect(state.entries[0].path).toBe('/B/b.md');
  });

  it('a stale request must not clear the newer request\'s loading flag', async () => {
    const pending = {};
    const { state, load } = makeLoader((_cmd, args) => {
      pending[args.dirPath] = deferred();
      return pending[args.dirPath].promise;
    });

    const loadA = load('/A');
    const loadB = load('/B');
    expect(state.loading).toBe(true);

    // Stale A finishes first (success and failure alike) — loading stays
    // true because B is still in flight.
    pending['/A'].resolve([]);
    await loadA;
    expect(state.loading).toBe(true);

    pending['/B'].resolve([{ name: 'b.md', path: '/B/b.md', is_dir: false }]);
    await loadB;
    expect(state.loading).toBe(false);
  });

  it('a stale error must not clear entries of the newer root', async () => {
    const pending = {};
    const { state, load } = makeLoader((_cmd, args) => {
      pending[args.dirPath] = deferred();
      return pending[args.dirPath].promise;
    });

    const loadA = load('/A');
    const loadB = load('/B');
    pending['/B'].resolve([{ name: 'b.md', path: '/B/b.md', is_dir: false }]);
    await loadB;

    pending['/A'].reject(new Error('dir gone'));
    await loadA;
    expect(state.entries).toHaveLength(1);
    expect(state.entries[0].path).toBe('/B/b.md');
  });

  it('an empty root invalidates an in-flight load', async () => {
    // Closing the folder while a load is in flight: the late response must
    // not resurrect entries for a root that no longer exists.
    const pending = {};
    const { state, load } = makeLoader((_cmd, args) => {
      pending[args.dirPath] = deferred();
      return pending[args.dirPath].promise;
    });

    const loadA = load('/A');
    await load(null); // folder closed
    expect(state.entries).toEqual([]);

    pending['/A'].resolve([{ name: 'ghost.md', path: '/A/ghost.md', is_dir: false }]);
    await loadA;
    expect(state.entries).toEqual([]);
  });

  it('an error on the latest request clears entries and loading', async () => {
    const { state, load } = makeLoader(async () => { throw new Error('io'); });
    await load('/root');
    expect(state.entries).toEqual([]);
    expect(state.loading).toBe(false);
  });

  it('keeps only the newest of several rapid loads', async () => {
    const pending = {};
    const { state, load } = makeLoader((_cmd, args) => {
      pending[args.dirPath] = deferred();
      return pending[args.dirPath].promise;
    });

    const loads = ['/1', '/2', '/3'].map((d) => load(d));
    // Resolve newest -> oldest; only /3 may apply.
    pending['/3'].resolve([{ name: 'c', path: '/3/c', is_dir: false }]);
    pending['/1'].resolve([{ name: 'a', path: '/1/a', is_dir: false }]);
    pending['/2'].resolve([{ name: 'b', path: '/2/b', is_dir: false }]);
    await Promise.all(loads);
    expect(state.entries).toEqual([
      { name: 'c', path: '/3/c', is_dir: false, children: null, expanded: false },
    ]);
  });
});
