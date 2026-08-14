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

/// A polling-based file watcher. Checks modification times at each `poll_changes` call.
/// No background threads; the host layer calls `poll_changes` on its timer (§64).
pub struct PollingFileWatcher {
    watched: Mutex<HashMap<String, Option<SystemTime>>>,
}

impl PollingFileWatcher {
    pub fn new() -> Self {
        Self { watched: Mutex::new(HashMap::new()) }
    }
}

impl Default for PollingFileWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl FileWatcher for PollingFileWatcher {
    fn watch(&self, path: &str) -> Result<(), PlatformError> {
        let p = Path::new(path);
        let mtime = std::fs::metadata(p)
            .ok()
            .and_then(|m| m.modified().ok());
        self.watched.lock().unwrap().insert(path.to_string(), mtime);
        Ok(())
    }

    fn poll_changes(&self) -> Result<Vec<WatchEvent>, PlatformError> {
        let mut events = Vec::new();
        let mut watched = self.watched.lock().unwrap();
        for (path, prev_mtime) in watched.iter_mut() {
            let p = Path::new(path);
            let current_mtime = std::fs::metadata(p)
                .ok()
                .and_then(|m| m.modified().ok());
            let prev = prev_mtime.take();
            match (prev, current_mtime) {
                (Some(prev), Some(curr)) => {
                    if curr != prev {
                        events.push(WatchEvent::Modified { path: path.clone() });
                    }
                    *prev_mtime = Some(curr);
                }
                (None, Some(curr)) => {
                    events.push(WatchEvent::Created { path: path.clone() });
                    *prev_mtime = Some(curr);
                }
                (Some(_), None) => {
                    events.push(WatchEvent::Removed { path: path.clone() });
                    *prev_mtime = None;
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
        assert!(events.iter().any(|e| matches!(e, WatchEvent::Modified { path } if path == path_str)));
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
        assert!(events.iter().any(|e| matches!(e, WatchEvent::Created { path } if path == path_str)));
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
        assert!(events.iter().any(|e| matches!(e, WatchEvent::Removed { path } if path == path_str)));
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
