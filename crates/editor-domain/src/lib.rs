//! Domain primitives for WoMD.
//!
//! No I/O, no external dependencies, no business logic that depends on any OS, UI
//! framework, Git provider, or plugin. Everything else depends on this crate; this crate
//! depends on nothing (Invariant 3, 4; §73 hexagonal architecture).

#![forbid(unsafe_code)]

use std::sync::Arc;

pub mod ids;
pub mod coords;
pub mod errors;
pub mod profile;

pub use coords::{ByteOffset, ByteRange, LineColumn, LineIndex, ScalarIndex};
pub use errors::{DocumentError, StorageError};
pub use ids::DocumentId;
pub use profile::MarkdownProfile;

/// Re-export of `DocumentId` for convenience.
pub use ids::DocumentId as DocId;

/// A selection within a document (§59). Half-open `[anchor, focus]` ordered pair; the
/// caret is a selection where `anchor == focus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Selection {
    pub anchor: ByteOffset,
    pub focus: ByteOffset,
}

impl Selection {
    pub fn caret(at: ByteOffset) -> Self {
        Self { anchor: at, focus: at }
    }
    /// The smaller of anchor/focus.
    pub fn start(&self) -> ByteOffset {
        ByteOffset(self.anchor.0.min(self.focus.0))
    }
    /// The larger of anchor/focus (exclusive end).
    pub fn end(&self) -> ByteOffset {
        ByteOffset(self.anchor.0.max(self.focus.0))
    }
    pub fn is_caret(&self) -> bool {
        self.anchor == self.focus
    }
    pub fn len(&self) -> u64 {
        self.end().0 - self.start().0
    }
}

/// Document metadata preserved on open (§4, §23, §100).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentMeta {
    pub id: DocumentId,
    /// BOM present at start of file.
    pub has_bom: bool,
    /// Dominant line ending kind.
    pub line_ending: LineEnding,
    /// File ends with a newline.
    pub trailing_newline: bool,
    /// Encoding (always UTF-8 in MVP; field reserved for future).
    pub encoding: Encoding,
}

/// Line ending style, preserved verbatim (§4, §23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineEnding {
    Lf,
    Crlf,
    Cr,
    /// Mixed: used only as a detected summary; edits preserve per-line original.
    Mixed,
}

/// Text encoding. MVP supports UTF-8 only; the variant is reserved (§4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    Utf8,
    Utf8WithBom,
}

/// A source of immutable bytes that can be accessed without copying the entire
/// buffer into memory. Used by PieceTable to support mmap-backed files (Invariant 6:
/// >RAM files are mmap'd and lazily paged by the OS, never fully loaded).
///
/// Implementations: `Arc<[u8]>` (in-memory), `MmapSource` (mmap-backed, zero-copy).
pub trait ByteSource: Send + Sync {
    /// The full byte slice. For mmap-backed sources, this is a direct view into
    /// the memory-mapped file — the OS pages regions on demand.
    fn as_bytes(&self) -> &[u8];
    /// Total length in bytes.
    fn len(&self) -> usize {
        self.as_bytes().len()
    }
    /// Extract a byte range without copying the entire buffer.
    /// Default implementation slices from `as_bytes()`; mmap implementations
    /// can override to use `read_range` for partial reads.
    fn read_range(&self, start: usize, end: usize) -> Vec<u8> {
        let bytes = self.as_bytes();
        let s = start.min(bytes.len());
        let e = end.min(bytes.len()).max(s);
        bytes[s..e].to_vec()
    }
}

/// In-memory byte source wrapping `Arc<[u8]>`.
/// Used for small documents that fit in RAM.
#[derive(Debug, Clone)]
pub struct ArcByteSource {
    bytes: Arc<[u8]>,
}

impl ArcByteSource {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes: Arc::from(bytes.into_boxed_slice()) }
    }
    pub fn from_arc(bytes: Arc<[u8]>) -> Self {
        Self { bytes }
    }
}

impl ByteSource for ArcByteSource {
    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    fn len(&self) -> usize {
        self.bytes.len()
    }
    fn read_range(&self, start: usize, end: usize) -> Vec<u8> {
        let s = start.min(self.bytes.len());
        let e = end.min(self.bytes.len()).max(s);
        self.bytes[s..e].to_vec()
    }
}
