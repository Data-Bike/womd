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

use editor_domain::{errors::StorageError, ByteOffset, ByteSource, DocumentId};

use crate::{ByteChunk, DocumentStorage, StorageResult};

/// Zero-copy `ByteSource` backed by an mmap'd file.
///
/// The `Mmap` handle is kept alive in an `Arc`, and `as_bytes()` returns a
/// direct slice into the memory-mapped region. The OS lazily pages regions
/// on access, so a >RAM file never needs to fit entirely in physical memory
/// (Invariant 6).
///
/// This replaces the old `MmapStorage::buffer()` which copied the entire file
/// into an `Arc<[u8]>`.
pub struct MmapSource {
    /// `None` for a zero-length file — `Mmap::map` refuses empty maps on both
    /// Windows and Unix, so an empty file is represented as no mapping at all.
    map: Option<Arc<memmap2::Mmap>>,
}

impl MmapSource {
    /// Open a file read-only and create a zero-copy byte source.
    pub fn open(path: impl AsRef<std::path::Path>) -> StorageResult<Self> {
        let file = File::open(path.as_ref()).map_err(|e| StorageError::Io(e.to_string()))?;
        let map = map_file(&file)?;
        Ok(Self { map })
    }

    /// Create from an existing `Option<Arc<Mmap>>` (used by `MmapStorage`).
    pub fn from_mmap(map: Option<Arc<memmap2::Mmap>>) -> Self {
        Self { map }
    }
}

/// Map a file, returning `None` for a zero-length file (empty maps are
/// rejected by the OS).
fn map_file(file: &File) -> StorageResult<Option<Arc<memmap2::Mmap>>> {
    let len = file.metadata().map_err(|e| StorageError::Io(e.to_string()))?.len();
    if len == 0 {
        return Ok(None);
    }
    let map = unsafe { memmap2::Mmap::map(file) }
        .map_err(|e| StorageError::Io(e.to_string()))?;
    Ok(Some(Arc::new(map)))
}

impl ByteSource for MmapSource {
    fn as_bytes(&self) -> &[u8] {
        self.map.as_ref().map(|m| &m[..]).unwrap_or(&[])
    }
    fn len(&self) -> usize {
        self.map.as_ref().map_or(0, |m| m.len())
    }
    /// For mmap, `read_range` slices from the mapped region — the OS pages
    /// only the requested range from disk (zero-copy for already-paged regions).
    fn read_range(&self, start: usize, end: usize) -> Vec<u8> {
        let bytes = self.as_bytes();
        let s = start.min(bytes.len());
        let e = end.min(bytes.len()).max(s);
        bytes[s..e].to_vec()
    }
}

/// mmap-backed storage. The map is kept alive for the lifetime of this struct.
pub struct MmapStorage {
    id: DocumentId,
    path: PathBuf,
    // `Arc` so the buffer can be shared with the Piece Table as its immutable
    // original. `None` for a zero-length file.
    map: Option<Arc<memmap2::Mmap>>,
}

impl MmapStorage {
    /// Open a file read-only and map it.
    pub fn open(path: impl Into<PathBuf>, id: DocumentId) -> StorageResult<Self> {
        let path = path.into();
        let file = File::open(&path).map_err(|e| StorageError::Io(e.to_string()))?;
        let map = map_file(&file)?;
        Ok(Self { id, path, map })
    }

    /// The immutable mapped buffer as a zero-copy `ByteSource`.
    /// This replaces the old `buffer()` method that copied the entire file.
    /// The returned `Arc<dyn ByteSource>` can be shared with the PieceTable
    /// without copying any file content.
    pub fn byte_source(&self) -> Arc<dyn ByteSource> {
        Arc::new(MmapSource::from_mmap(self.map.clone()))
    }

    /// The mapped bytes as a slice (zero-copy).
    pub fn as_slice(&self) -> &[u8] {
        self.map.as_ref().map(|m| &m[..]).unwrap_or(&[])
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
        Ok(self.as_slice().len() as u64)
    }
    fn read_range(&self, offset: ByteOffset, length: usize) -> StorageResult<ByteChunk> {
        let map = self.as_slice();
        let start = offset.0 as usize;
        if start > map.len() {
            return Err(StorageError::OutOfRange);
        }
        // `start + length` can overflow usize (huge requested length); saturate.
        let end = start.saturating_add(length).min(map.len());
        let bytes = map[start..end].to_vec();
        Ok(ByteChunk {
            start_offset: offset,
            bytes,
            leading_partial: start != 0 && map.get(start.wrapping_sub(1)) != Some(&b'\n'),
            // Same semantics as `InMemoryStorage`: the chunk is trailing-partial
            // when its *last* byte is not a newline. Checking `map[end]` asks
            // about the next byte and inverts both answers on line boundaries.
            trailing_partial: end < map.len() && end > 0 && map[end - 1] != b'\n',
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

    /// `trailing_partial` must have the same semantics as `InMemoryStorage`:
    /// a chunk ending exactly on `\n` is complete; ending before `\n` is partial.
    #[test]
    fn mmap_trailing_partial_matches_inmemory_semantics() {
        let path = write_tmp("womd_mmap_trailing.md", b"ab\ncd\n");
        let storage = MmapStorage::open(&path, DocumentId::new("t")).unwrap();
        // [0,3) = "ab\n" ends on a newline -> not partial.
        let chunk = storage.read_range(ByteOffset(0), 3).unwrap();
        assert_eq!(chunk.bytes, b"ab\n");
        assert!(!chunk.trailing_partial, "chunk ending on \\n must be complete");
        // [0,2) = "ab" ends mid-line -> partial.
        let chunk = storage.read_range(ByteOffset(0), 2).unwrap();
        assert!(chunk.trailing_partial, "chunk ending before \\n must be partial");
        // [3,6) = "cd\n" -> complete.
        let chunk = storage.read_range(ByteOffset(3), 3).unwrap();
        assert!(!chunk.trailing_partial);
        let _ = std::fs::remove_file(&path);
    }

    /// `Mmap::map` rejects zero-length files — an empty `.md` must still open
    /// and behave as an empty document (Invariant 6 must not break `open` on
    /// legitimately empty files).
    #[test]
    fn mmap_empty_file_opens() {
        let path = write_tmp("womd_mmap_empty.md", b"");
        let storage = MmapStorage::open(&path, DocumentId::new("t")).unwrap();
        assert_eq!(storage.len().unwrap(), 0);
        assert!(storage.as_slice().is_empty());
        assert!(storage.byte_source().as_bytes().is_empty());
        let chunk = storage.read_range(ByteOffset(0), 10).unwrap();
        assert!(chunk.bytes.is_empty());
        assert!(!chunk.leading_partial && !chunk.trailing_partial);
        assert!(MmapSource::open(&path).unwrap().as_bytes().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// `start + length` must saturate instead of overflowing usize.
    #[test]
    fn mmap_huge_length_does_not_overflow() {
        let path = write_tmp("womd_mmap_huge_len.md", b"abc\n");
        let storage = MmapStorage::open(&path, DocumentId::new("t")).unwrap();
        let chunk = storage.read_range(ByteOffset(1), usize::MAX).unwrap();
        assert_eq!(chunk.bytes, b"bc\n");
        let _ = std::fs::remove_file(&path);
    }
}
