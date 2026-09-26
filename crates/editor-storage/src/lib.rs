//! Editor storage: `DocumentStorage` port, byte-range loading, UTF-8 boundary correction,
//! chunk parser context, and atomic save (§51–57, §100, ADR-006).
//!
//! The original document bytes are immutable and may be mmap-backed, so files larger than
//! RAM can be opened (Invariant 6). `read_range` corrects requested byte windows to safe
//! UTF-8 and line boundaries and reports whether the chunk is partial (so the Markdown
//! chunk parser can use prefix/suffix context — §54).
//!
//! `unsafe` is denied at the crate root and only re-allowed in `src/mmap.rs` (the single
//! mmap call, ADR-006).

#![deny(unsafe_code)]

use std::fs::File;
use std::io::Write;
use std::path::Path;

use editor_domain::{errors::StorageError, ByteOffset, DocumentId};

pub type StorageResult<T> = Result<T, StorageError>;

mod mmap;
mod chunk;

pub use mmap::{MmapSource, MmapStorage};
pub use chunk::{analyze_context, ChunkContext};

/// A chunk of bytes read from a document, with its absolute start offset and partial-edge
/// flags (§52–54).
#[derive(Debug, Clone)]
pub struct ByteChunk {
    /// Absolute byte offset of the first byte in `bytes`.
    pub start_offset: ByteOffset,
    pub bytes: Vec<u8>,
    /// True if the chunk begins mid-line / mid-block (caller must use prefix context).
    pub leading_partial: bool,
    /// True if the chunk ends mid-line / mid-block (caller must use suffix context).
    pub trailing_partial: bool,
}

/// Abstraction over document storage supporting byte-range reads (§51).
pub trait DocumentStorage {
    fn id(&self) -> &DocumentId;
    fn len(&self) -> StorageResult<u64>;
    fn read_range(&self, offset: ByteOffset, length: usize) -> StorageResult<ByteChunk>;
}

/// In-memory storage (used by tests and small documents).
pub struct InMemoryStorage {
    id: DocumentId,
    bytes: Vec<u8>,
}

impl InMemoryStorage {
    pub fn new(id: DocumentId, bytes: Vec<u8>) -> Self {
        Self { id, bytes }
    }
}

impl DocumentStorage for InMemoryStorage {
    fn id(&self) -> &DocumentId {
        &self.id
    }
    fn len(&self) -> StorageResult<u64> {
        Ok(self.bytes.len() as u64)
    }
    fn read_range(&self, offset: ByteOffset, length: usize) -> StorageResult<ByteChunk> {
        let start = offset.0 as usize;
        if start > self.bytes.len() {
            return Err(StorageError::OutOfRange);
        }
        // `start + length` can overflow usize (huge requested length); saturate.
        let end = start.saturating_add(length).min(self.bytes.len());
        let bytes = self.bytes[start..end].to_vec();
        Ok(ByteChunk {
            start_offset: offset,
            bytes,
            leading_partial: start != 0 && self.bytes.get(start.wrapping_sub(1)) != Some(&b'\n'),
            // The chunk is trailing-partial when its last byte is not a newline
            // (the line continues past the chunk). Checking `bytes[end]` asks
            // about the *next* byte instead — that inverted both answers for
            // ranges ending exactly on, or just before, a line boundary.
            trailing_partial: end < self.bytes.len() && end > 0 && self.bytes[end - 1] != b'\n',
        })
    }
}

/// Corrects a `[offset, offset+length)` byte range to safe UTF-8 + line boundaries (§53).
///
/// Returns the corrected `(start, end)` byte offsets within `bytes` for the window
/// starting at `base`. Scans backward/forward to the nearest char boundary and to the
/// nearest newline boundary.
pub fn correct_utf8_boundaries(bytes: &[u8], base: usize, offset: usize, length: usize) -> (usize, usize) {
    let raw_start = offset.min(bytes.len());
    // `offset + length` can overflow usize for a huge requested length.
    let raw_end = offset.saturating_add(length).min(bytes.len());
    // Walk back to a UTF-8 char boundary.
    let mut start = raw_start;
    while start > 0 && !is_char_boundary(&bytes[start..]) {
        start -= 1;
    }
    // Walk forward to a char boundary.
    let mut end = raw_end;
    while end < bytes.len() && !is_char_boundary(&bytes[end..]) {
        end += 1;
    }
    // Expand to line boundaries (start at column 0, end right after a newline).
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    while end > 0 && end < bytes.len() && bytes[end - 1] != b'\n' {
        end += 1;
    }
    let _ = base;
    (start, end)
}

fn is_char_boundary(slice: &[u8]) -> bool {
    if slice.is_empty() {
        return true;
    }
    // A byte is a char boundary if it is NOT a continuation byte (0b10xxxxxx).
    slice[0] & 0xC0 != 0x80
}

/// Atomic save: write to a temp file, flush, fsync, then rename over the target (§100).
///
/// `permissions` is reserved for future use (§100). On platforms where atomic rename is
/// not possible over an existing file, the caller may fall back to a non-atomic replace;
/// this implementation uses `rename` which is atomic on POSIX and best-effort on Windows.
/// Temp-file name unique to this process — two editors saving the same file
/// must not race on one shared temp path (last rename still wins, but a
/// shared temp file could interleave both payloads).
fn temp_name_for(path: &Path) -> String {
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "document".to_string());
    format!("{base}.womd-tmp-{}", std::process::id())
}

pub fn atomic_save(path: &Path, bytes: &[u8]) -> StorageResult<()> {
    let mut tmp_path = path.to_path_buf();
    tmp_path.set_file_name(temp_name_for(path));

    let result = (|| -> StorageResult<()> {
        {
            let mut file = File::create(&tmp_path).map_err(|e| StorageError::Io(e.to_string()))?;
            file.write_all(bytes).map_err(|e| StorageError::Io(e.to_string()))?;
            // fsync the temp file BEFORE the rename — `File::sync_all` works
            // cross-platform (fsync on POSIX, FlushFileBuffers on Windows). Without
            // this, a crash between rename and page-cache flush can leave a
            // zero-length target.
            file.sync_all().map_err(|e| StorageError::Io(e.to_string()))?;
        }
        std::fs::rename(&tmp_path, path).map_err(|e| StorageError::Io(e.to_string()))
    })();
    // A failed save must not litter the user's directory with `.womd-tmp-*`
    // files — they would show up in the file tree and confuse the next save.
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result?;
    // Best-effort fsync of the parent directory so the rename itself is durable
    // (opening a directory as a file only works on POSIX; harmless to skip).
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Streaming save for partial editing (§57): stitch `original segment | edit segment | …`
/// from a list of `(source_range, replacement)` patches into a temp file, then atomic
/// rename. Only the patched regions differ from the original; unchanged regions are copied
/// verbatim from `source`, preserving source bytes (Invariant 1, 2).
pub fn streaming_save(
    path: &Path,
    source: &[u8],
    patches: &[(ByteRange, Vec<u8>)],
) -> StorageResult<()> {
    let mut tmp_path = path.to_path_buf();
    tmp_path.set_file_name(temp_name_for(path));

    // Validate patches: sorted, non-overlapping, and within the source bounds.
    // A malformed patch would otherwise panic on `source[s..e]` or silently
    // write a corrupted file.
    let mut prev_end = 0u64;
    for (range, _) in patches {
        if range.start.0 > range.end.0
            || range.start.0 < prev_end
            || range.end.0 > source.len() as u64
        {
            return Err(StorageError::OutOfRange);
        }
        prev_end = range.end.0;
    }

    let result = (|| -> StorageResult<()> {
        let mut file = File::create(&tmp_path).map_err(|e| StorageError::Io(e.to_string()))?;
        let mut cursor: u64 = 0;
        for (range, replacement) in patches {
            // Copy unchanged bytes from cursor to the patch start.
            if range.start.0 > cursor {
                let s = cursor as usize;
                let e = range.start.0 as usize;
                file.write_all(&source[s..e]).map_err(|e2| StorageError::Io(e2.to_string()))?;
            }
            // Write the replacement.
            file.write_all(replacement).map_err(|e2| StorageError::Io(e2.to_string()))?;
            cursor = range.end.0;
        }
        // Trailing unchanged bytes.
        if (cursor as usize) < source.len() {
            file.write_all(&source[cursor as usize..]).map_err(|e2| StorageError::Io(e2.to_string()))?;
        }
        file.sync_all().map_err(|e| StorageError::Io(e.to_string()))?;
        std::fs::rename(&tmp_path, path).map_err(|e| StorageError::Io(e.to_string()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Re-export of `ByteRange` for patch consumers.
pub use editor_domain::ByteRange;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn in_memory_read_range_basic() {
        let s = InMemoryStorage::new(DocumentId::new("t"), b"hello\nworld\n".to_vec());
        let chunk = s.read_range(ByteOffset(6), 5).unwrap();
        assert_eq!(chunk.bytes, b"world");
        assert_eq!(chunk.start_offset, ByteOffset(6));
        // offset 6 is right after a newline -> not leading partial.
        assert!(!chunk.leading_partial);
    }

    #[test]
    fn in_memory_partial_flags() {
        let s = InMemoryStorage::new(DocumentId::new("t"), b"hello\nworld\n".to_vec());
        // Read "el" (offset 1, length 2): mid-word on both sides.
        let chunk = s.read_range(ByteOffset(1), 2).unwrap();
        assert_eq!(chunk.bytes, b"el");
        assert!(chunk.leading_partial);
        assert!(chunk.trailing_partial);
    }

    /// `trailing_partial` reflects whether the chunk's *last* byte is a line
    /// boundary — a chunk ending exactly on `\n` is complete, and a chunk
    /// ending just before `\n` is partial (it split the line).
    #[test]
    fn trailing_partial_reflects_chunk_end() {
        let s = InMemoryStorage::new(DocumentId::new("t"), b"ab\ncd\n".to_vec());
        // [0,3) = "ab\n" ends on a newline -> not partial, even though the
        // document continues with "cd".
        let chunk = s.read_range(ByteOffset(0), 3).unwrap();
        assert_eq!(chunk.bytes, b"ab\n");
        assert!(!chunk.trailing_partial);
        // [0,2) = "ab" ends mid-line -> partial, even though the next byte is '\n'.
        let chunk = s.read_range(ByteOffset(0), 2).unwrap();
        assert!(chunk.trailing_partial);
        // [3,6) = "cd\n" -> complete.
        let chunk = s.read_range(ByteOffset(3), 3).unwrap();
        assert!(!chunk.trailing_partial);
    }

    #[test]
    fn streaming_save_rejects_overlapping_patches() {
        let dir = std::env::temp_dir();
        let path = dir.join("womd_streaming_overlap_test.md");
        let original = b"abcdef\n";
        let patches = vec![
            (ByteRange::new(ByteOffset(0), ByteOffset(4)), b"X".to_vec()),
            (ByteRange::new(ByteOffset(2), ByteOffset(6)), b"Y".to_vec()),
        ];
        assert!(matches!(
            streaming_save(&path, original, &patches),
            Err(StorageError::OutOfRange)
        ));
    }

    #[test]
    fn streaming_save_rejects_out_of_bounds_patch() {
        let dir = std::env::temp_dir();
        let path = dir.join("womd_streaming_oob_test.md");
        let original = b"abc\n";
        let patches = vec![(ByteRange::new(ByteOffset(0), ByteOffset(100)), b"X".to_vec())];
        assert!(matches!(
            streaming_save(&path, original, &patches),
            Err(StorageError::OutOfRange)
        ));
    }

    #[test]
    fn out_of_range() {
        let s = InMemoryStorage::new(DocumentId::new("t"), b"hi\n".to_vec());
        assert!(matches!(s.read_range(ByteOffset(100), 1), Err(StorageError::OutOfRange)));
    }

    #[test]
    fn utf8_boundary_correction_keeps_lines() {
        // "a\né\nb\n" where é is 2 bytes (0xC3 0xA9).
        let bytes = "a\né\nb\n".as_bytes();
        // Request a range starting mid-é (offset 3 = the 0xA9 continuation byte).
        let (start, end) = correct_utf8_boundaries(bytes, 0, 3, 1);
        // Should expand back to line start (offset 2) and forward to line end (offset 4,
        // right after the newline at index 3... actually newline is at index 4).
        assert_eq!(start, 2);
        assert_eq!(end, 5); // includes the newline after é
    }

    #[test]
    fn atomic_save_roundtrips() {
        let dir = std::env::temp_dir();
        let path = dir.join("womd_atomic_save_test.md");
        let payload = b"# Test\n\nHello.\n";
        atomic_save(&path, payload).unwrap();
        let mut read_back = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut read_back).unwrap();
        assert_eq!(read_back, payload);
        let _ = std::fs::remove_file(&path);
    }

    /// Patches given out of order must be rejected — the stitch loop assumes
    /// sorted, non-overlapping ranges.
    #[test]
    fn streaming_save_rejects_unsorted_patches() {
        let dir = std::env::temp_dir();
        let path = dir.join("womd_streaming_unsorted_test.md");
        let original = b"abcdef\n";
        let patches = vec![
            (ByteRange::new(ByteOffset(4), ByteOffset(6)), b"X".to_vec()),
            (ByteRange::new(ByteOffset(0), ByteOffset(2)), b"Y".to_vec()),
        ];
        assert!(matches!(
            streaming_save(&path, original, &patches),
            Err(StorageError::OutOfRange)
        ));
        // The temp file must not be left behind on validation failure.
        assert!(!path.exists());
    }

    /// Read-range flags must hold at EVERY split point: `leading_partial`
    /// iff the byte before the chunk isn't `\n`, `trailing_partial` iff the
    /// chunk's last byte isn't `\n` and the file continues.
    #[test]
    fn read_range_flags_at_every_boundary() {
        let s = InMemoryStorage::new(DocumentId::new("t"), b"ab\ncd\nef".to_vec());
        let bytes = b"ab\ncd\nef";
        for start in 0..bytes.len() {
            for end in start..=bytes.len() {
                if start == end {
                    continue;
                }
                let chunk = s.read_range(ByteOffset(start as u64), end - start).unwrap();
                assert_eq!(chunk.bytes, &bytes[start..end], "[{start},{end})");
                assert_eq!(
                    chunk.leading_partial,
                    start > 0 && bytes[start - 1] != b'\n',
                    "leading_partial at [{start},{end})"
                );
                assert_eq!(
                    chunk.trailing_partial,
                    end < bytes.len() && bytes[end - 1] != b'\n',
                    "trailing_partial at [{start},{end})"
                );
            }
        }
    }

    #[test]
    fn streaming_save_applies_patches() {
        let dir = std::env::temp_dir();
        let path = dir.join("womd_streaming_save_test.md");
        let original = b"# Title\n\nHello beautiful world.\n";
        // Replace "beautiful" (offsets 15..24) with "great".
        let patches = vec![(ByteRange::new(ByteOffset(15), ByteOffset(24)), b"great".to_vec())];
        streaming_save(&path, original, &patches).unwrap();
        let mut read_back = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut read_back).unwrap();
        assert_eq!(read_back, b"# Title\n\nHello great world.\n");
        let _ = std::fs::remove_file(&path);
    }
}
