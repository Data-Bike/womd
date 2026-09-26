/**
 * Guarded directory listing for FileTree.
 *
 * `load_directory` is async and users can change the tree root quickly; a slow
 * response for root A must never overwrite the entries already loaded for
 * root B. Every load takes a monotonic ticket and applies its result only
 * while that ticket is still the newest one handed out. Clearing the root
 * also takes a ticket, so an in-flight load cannot resurrect entries after
 * the folder was closed.
 */
export function createDirectoryLoader(invokeFn, { setEntries, setLoading }) {
  let seq = 0;

  return async function loadEntries(dir) {
    // Always take a ticket first — including for an empty root — so a stale
    // in-flight response is invalidated by ANY newer load call.
    const id = ++seq;
    if (!dir) {
      setEntries([]);
      setLoading(false);
      return;
    }
    setLoading(true);
    try {
      const list = await invokeFn('list_directory', { dirPath: dir });
      if (id !== seq) return; // superseded by a newer request
      setEntries(list.map(e => ({ ...e, children: null, expanded: false })));
    } catch (e) {
      if (id !== seq) return;
      console.error('loadEntries:', e);
      setEntries([]);
    } finally {
      // A stale request must not clear the loading flag of a newer one.
      if (id === seq) setLoading(false);
    }
  };
}
