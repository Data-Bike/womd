//! File watcher (§64). Polling-based implementation that checks file modification times
//! at intervals. A native `notify`-backed implementation can replace this behind the
//! `FileWatcher` trait without touching callers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::PlatformError;

/// A file change event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    Modified { path: String },
    Created { path: String },
    Removed { path: String },
}

/// File watcher trait (§64).
pub trait FileWatcher {
    /// Start watching a path (file or directory).
    fn watch(&self, path: &str) -> Result<(), PlatformError>;
    /// Poll for changes since the last poll. Returns events detected.
    fn poll_changes(&self) -> Result<Vec<WatchEvent>, PlatformError>;
}

/// What `poll_changes` remembers about a watched path. mtime alone is not
/// enough: a rewrite that preserves the timestamp (coarse-granularity
/// filesystems, or a writer that restores mtime) changes content without
/// changing `modified` — the file length must be part of the stamp.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileStamp {
    mtime: SystemTime,
    len: u64,
}

fn stamp_of(p: &Path) -> Option<FileStamp> {
    let m = std::fs::metadata(p).ok()?;
    Some(FileStamp {
        mtime: m.modified().ok()?,
        len: m.len(),
    })
}

/// A polling-based file watcher. Checks modification times at each `poll_changes` call.
/// No background threads; the host layer calls `poll_changes` on its timer (§64).
pub struct PollingFileWatcher {
    watched: Mutex<HashMap<String, Option<FileStamp>>>,
}

impl PollingFileWatcher {
    pub fn new() -> Self {
        Self {
            watched: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for PollingFileWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl FileWatcher for PollingFileWatcher {
    fn watch(&self, path: &str) -> Result<(), PlatformError> {
        let stamp = stamp_of(Path::new(path));
        // A poisoned mutex still holds a consistent map — recover the guard
        // rather than panicking the caller.
        self.watched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_string(), stamp);
        Ok(())
    }

    fn poll_changes(&self) -> Result<Vec<WatchEvent>, PlatformError> {
        let mut events = Vec::new();
        let mut watched = self.watched.lock().unwrap_or_else(|e| e.into_inner());
        for (path, prev_stamp) in watched.iter_mut() {
            let current = stamp_of(Path::new(path));
            let prev = prev_stamp.take();
            match (prev, current) {
                (Some(prev), Some(curr)) => {
                    if curr != prev {
                        events.push(WatchEvent::Modified { path: path.clone() });
                    }
                    *prev_stamp = Some(curr);
                }
                (None, Some(curr)) => {
                    events.push(WatchEvent::Created { path: path.clone() });
                    *prev_stamp = Some(curr);
                }
                (Some(_), None) => {
                    events.push(WatchEvent::Removed { path: path.clone() });
                    *prev_stamp = None;
                }
                (None, None) => {}
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn detect_file_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        fs::write(&path, b"hello\n").unwrap();
        let path_str = path.to_str().unwrap();

        let watcher = PollingFileWatcher::new();
        watcher.watch(path_str).unwrap();

        // No changes yet.
        let events = watcher.poll_changes().unwrap();
        assert!(events.is_empty());

        // Modify the file.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(b"hello world\n").unwrap();
        f.flush().unwrap();

        let events = watcher.poll_changes().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WatchEvent::Modified { path } if path == path_str))
        );
    }

    #[test]
    fn detect_file_creation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.md");
        let path_str = path.to_str().unwrap();

        let watcher = PollingFileWatcher::new();
        // Watch a non-existent file.
        watcher.watch(path_str).unwrap();

        // Create it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(&path, b"new\n").unwrap();

        let events = watcher.poll_changes().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WatchEvent::Created { path } if path == path_str))
        );
    }

    #[test]
    fn detect_file_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("temp.md");
        fs::write(&path, b"data\n").unwrap();
        let path_str = path.to_str().unwrap();

        let watcher = PollingFileWatcher::new();
        watcher.watch(path_str).unwrap();

        // Remove it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::remove_file(&path).unwrap();

        let events = watcher.poll_changes().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WatchEvent::Removed { path } if path == path_str))
        );
    }

    #[test]
    fn same_mtime_different_len_is_detected() {
        // A rewrite that preserves mtime (coarse FS granularity or a writer
        // that restores the timestamp) must still be reported as Modified —
        // tracked via the length component of the stamp.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.md");
        fs::write(&path, b"short\n").unwrap();
        let path_str = path.to_str().unwrap();

        let watcher = PollingFileWatcher::new();
        watcher.watch(path_str).unwrap();
        let real = watcher.watched.lock().unwrap()[path_str].unwrap();

        // Rewrite with different content length, then doctor the stored stamp
        // to have the CURRENT mtime (as if the filesystem did not bump it).
        fs::write(&path, b"much much longer content\n").unwrap();
        let now = stamp_of(&path).unwrap();
        watcher.watched.lock().unwrap().insert(
            path_str.to_string(),
            Some(FileStamp {
                mtime: now.mtime,
                len: real.len,
            }),
        );

        let events = watcher.poll_changes().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WatchEvent::Modified { path } if path == path_str))
        );
    }

    #[test]
    fn no_changes_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stable.md");
        fs::write(&path, b"stable\n").unwrap();

        let watcher = PollingFileWatcher::new();
        watcher.watch(path.to_str().unwrap()).unwrap();
        let events = watcher.poll_changes().unwrap();
        assert!(events.is_empty());
        // Second poll also empty.
        let events = watcher.poll_changes().unwrap();
        assert!(events.is_empty());
    }
}
