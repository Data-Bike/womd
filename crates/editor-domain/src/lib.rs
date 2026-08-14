//! Domain primitives for WoMD.
//!
//! No I/O, no external dependencies, no business logic that depends on any OS, UI
//! framework, Git provider, or plugin. Everything else depends on this crate; this crate
//! depends on nothing (Invariant 3, 4; §73 hexagonal architecture).

#![forbid(unsafe_code)]

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
