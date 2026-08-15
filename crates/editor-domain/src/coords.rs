//! Coordinate system primitives (§4.1, §8).
//!
//! Five coordinate kinds are distinguished; conversions are explicit. `byte offset !=
//! character offset` is enforced by type separation.

/// A byte offset into the UTF-8 document buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteOffset(pub u64);

impl ByteOffset {
    pub const ZERO: Self = Self(0);
    pub fn checked_add(self, n: u64) -> Option<Self> {
        self.0.checked_add(n).map(Self)
    }
}

/// A Unicode scalar value index (code point count, not byte count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScalarIndex(pub u64);

/// An extended grapheme cluster index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphemeIndex(pub u64);

/// A line (0-based) + column (in Unicode scalars, 0-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineColumn {
    pub line: LineIndex,
    pub column: ScalarIndex,
}

/// A 0-based line number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineIndex(pub u32);

impl LineIndex {
    pub const ZERO: Self = Self(0);
}

/// A half-open byte range `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ByteRange {
    pub start: ByteOffset,
    pub end: ByteOffset,
}

impl ByteRange {
    pub fn new(start: ByteOffset, end: ByteOffset) -> Self {
        debug_assert!(start <= end, "ByteRange start must be <= end");
        Self { start, end }
    }
    pub fn empty(at: ByteOffset) -> Self {
        Self { start: at, end: at }
    }
    pub fn len(&self) -> u64 {
        self.end.0.saturating_sub(self.start.0)
    }
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
    pub fn contains(&self, off: ByteOffset) -> bool {
        self.start <= off && off < self.end
    }
}
