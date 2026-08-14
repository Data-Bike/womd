//! mmap-backed `DocumentStorage` (ADR-006, §51–53).
//!
//! The file is mapped read-only and treated as an immutable original buffer. Byte-range
//! reads copy out the requested window (plus the partial-edge metadata the chunk parser
//! needs). The OS pager lazily loads pages, so a >RAM file does not need to fit in memory
//! (Invariant 6).
//!
//! `unsafe` is required to create a memory map (the OS guarantees the mapping is stable
//! for the lifetime of the `Mmap` handle). This module locally re-enables `unsafe_code`
//! for that single operation; all other crates remain `unsafe_code = "deny"`.

#![allow(unsafe_code)]

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use editor_domain::{errors::StorageError, ByteOffset, DocumentId};

use crate::{ByteChunk, DocumentStorage, StorageResult};

/// mmap-backed storage. The map is kept alive for the lifetime of this struct.
pub struct MmapStorage {
    id: DocumentId,
    path: PathBuf,
    // `Arc` so the buffer can be shared with the Piece Table as its immutable original.
    map: Arc<memmap2::Mmap>,
}

impl MmapStorage {
    /// Open a file read-only and map it.
    pub fn open(path: impl Into<PathBuf>, id: DocumentId) -> StorageResult<Self> {
        let path = path.into();
        let file = File::open(&path).map_err(|e| StorageError::Io(e.to_string()))?;
        let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(Self { id, path, map: Arc::new(map) })
    }

    /// The immutable mapped buffer, shareable with the Piece Table.
    pub fn buffer(&self) -> Arc<[u8]> {
        // Cheap: `Arc<[u8]>` from `Arc<Mmap>` via a pointer-length construction would avoid
        // the copy, but `Arc<[u8]>` cannot wrap an external allocation without unsafe
        // custom allocators. For MVP we expose the slice via a method instead.
        Arc::from(&self.map[..])
    }

    /// The mapped bytes as a slice (zero-copy).
    pub fn as_slice(&self) -> &[u8] {
        &self.map
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl DocumentStorage for MmapStorage {
    fn id(&self) -> &DocumentId {
        &self.id
    }
    fn len(&self) -> StorageResult<u64> {
        Ok(self.map.len() as u64)
    }
    fn read_range(&self, offset: ByteOffset, length: usize) -> StorageResult<ByteChunk> {
        let start = offset.0 as usize;
        if start > self.map.len() {
            return Err(StorageError::OutOfRange);
        }
        let end = (start + length).min(self.map.len());
        let bytes = self.map[start..end].to_vec();
        Ok(ByteChunk {
            start_offset: offset,
            bytes,
            leading_partial: start != 0 && self.map.get(start.wrapping_sub(1)) != Some(&b'\n'),
            trailing_partial: end < self.map.len() && self.map.get(end) != Some(&b'\n'),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomic_save;

    fn write_tmp(name: &str, content: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(name);
        atomic_save(&path, content).unwrap();
        path
    }

    #[test]
    fn mmap_open_and_read() {
        let path = write_tmp("womd_mmap_test.md", b"# Hello\n\nWorld.\n");
        let storage = MmapStorage::open(&path, DocumentId::new("t")).unwrap();
        assert_eq!(storage.len().unwrap(), b"# Hello\n\nWorld.\n".len() as u64);
        let chunk = storage.read_range(ByteOffset(0), 7).unwrap();
        assert_eq!(chunk.bytes, b"# Hello");
        assert!(!chunk.leading_partial);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mmap_partial_flags() {
        let path = write_tmp("womd_mmap_partial.md", b"hello\nworld\n");
        let storage = MmapStorage::open(&path, DocumentId::new("t")).unwrap();
        // Read "el" (offset 1, length 2): mid-word on both sides.
        let chunk = storage.read_range(ByteOffset(1), 2).unwrap();
        assert_eq!(chunk.bytes, b"el");
        assert!(chunk.leading_partial);
        assert!(chunk.trailing_partial);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mmap_large_file_generated() {
        // §95: generate a large file at test time (not committed as a fixture).
        // ~10 MB of repeated lines.
        let line = b"this is a line of markdown content for large-file testing\n";
        let mut content = Vec::with_capacity(10 * 1024 * 1024);
        while content.len() < 10 * 1024 * 1024 {
            content.extend_from_slice(line);
        }
        let path = write_tmp("womd_mmap_large.md", &content);
        let storage = MmapStorage::open(&path, DocumentId::new("large")).unwrap();
        assert!(storage.len().unwrap() > 10 * 1024 * 1024);
        // Read a 2 MB window from the middle (§52).
        let mid = 5 * 1024 * 1024;
        let chunk = storage.read_range(ByteOffset(mid), 2 * 1024 * 1024).unwrap();
        assert_eq!(chunk.bytes.len(), 2 * 1024 * 1024);
        // The window starts at a line boundary (mid is a multiple of line length? not
        // necessarily), but content is deterministic so just check we got bytes.
        assert!(!chunk.bytes.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
