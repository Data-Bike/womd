//! Source-preserving Piece Table text buffer (ADR-002).
//!
//! The original document bytes are immutable (`Arc<[u8]>`); edits only append to an
//! append-only edit log and rewrite the piece list. Unchanged pieces reference the original
//! buffer verbatim, so:
//!
//! * round-tripping an unedited document is byte-identical (Invariant 1);
//! * a small edit produces a small diff because serialization stitches original segments
//!   for unchanged regions (Invariant 2);
//! * the original buffer can be mmap-backed for >RAM files (Invariant 6, ADR-006).
//!
//! A line-start index provides `byte_offset <-> line/column` lookups. The index is updated
//! incrementally per edit in `O(log n + edited_lines)`.

#![forbid(unsafe_code)]

use std::sync::Arc;

use editor_domain::{ByteOffset, ByteRange, LineColumn, LineIndex, ScalarIndex};

mod piece;

pub use piece::{Piece, PieceSource, PieceTable};

/// Convenience: number of Unicode scalar values in a UTF-8 slice.
pub fn scalar_len(bytes: &[u8]) -> u64 {
    // `char_count` is O(n); used only for single-line column resolution (lines are short).
    core::str::from_utf8(bytes)
        .map(|s| s.chars().count() as u64)
        .unwrap_or_else(|_| {
            // Fall back to counting non-continuation bytes for invalid UTF-8 (defensive).
            bytes.iter().filter(|b| *b & 0xC0 != 0x80).count() as u64
        })
}

/// Convert a `ByteOffset` within this table to a `LineColumn` (column in scalars).
pub fn byte_to_line_column(table: &PieceTable, off: ByteOffset) -> LineColumn {
    let line_starts = table.line_starts();
    let off = off.0.min(table.len());
    let line = match line_starts.binary_search(&off) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    let line_start = line_starts[line];
    let line_bytes = table.extract_bytes(ByteRange::new(ByteOffset(line_start), ByteOffset(off)));
    LineColumn {
        line: LineIndex(line as u32),
        column: ScalarIndex(scalar_len(&line_bytes)),
    }
}

/// Convert a `LineColumn` to a `ByteOffset`.
pub fn line_column_to_byte(table: &PieceTable, lc: LineColumn) -> ByteOffset {
    let line_starts = table.line_starts();
    let line = lc.line.0 as usize;
    if line >= line_starts.len() {
        return ByteOffset(table.len());
    }
    let line_start = line_starts[line];
    let next = if line + 1 < line_starts.len() {
        line_starts[line + 1]
    } else {
        table.len()
    };
    // Walk scalars within [line_start, next) until we reach the column.
    let line_bytes = table.extract_bytes(ByteRange::new(ByteOffset(line_start), ByteOffset(next)));
    let mut scalars_seen = 0u64;
    for (i, b) in line_bytes.iter().enumerate() {
        if b & 0xC0 != 0x80 {
            if scalars_seen == lc.column.0 {
                return ByteOffset(line_start + i as u64);
            }
            scalars_seen += 1;
        }
    }
    // Column at/past end of line: clamp to the line end (the next line's start,
    // or the document end for the last line). Never lands mid-scalar.
    ByteOffset(next)
}

/// Re-export the original buffer type for storage/mmap integration.
/// Uses `ByteSource` trait to support both in-memory and mmap-backed buffers.
pub type OriginalBuffer = Arc<dyn editor_domain::ByteSource>;

#[cfg(test)]
mod tests {
    use super::*;

    fn table(bytes: &[u8]) -> PieceTable {
        PieceTable::from_bytes(bytes.to_vec())
    }

    #[test]
    fn line_column_round_trip() {
        let t = table(b"ab\ncd\nef");
        for off in 0..=t.len() {
            let lc = byte_to_line_column(&t, ByteOffset(off));
            let back = line_column_to_byte(&t, lc);
            assert_eq!(back.0, off.min(t.len()), "offset {off} lc={lc:?}");
        }
    }

    #[test]
    fn line_column_past_eol_clamps_to_line_end() {
        let t = table(b"ab\ncd");
        // Line 0 has 2 scalars + newline; huge column clamps to next line start.
        assert_eq!(
            line_column_to_byte(
                &t,
                LineColumn {
                    line: LineIndex(0),
                    column: ScalarIndex(99)
                }
            )
            .0,
            3
        );
        // Last line clamps to document end, not the last byte.
        assert_eq!(
            line_column_to_byte(
                &t,
                LineColumn {
                    line: LineIndex(1),
                    column: ScalarIndex(99)
                }
            )
            .0,
            5
        );
    }

    #[test]
    fn line_column_never_lands_mid_scalar() {
        // 'é' is two UTF-8 bytes (0xC3 0xA9); '😀' is four.
        let t = table("aé😀".as_bytes());
        // Columns 0..=3 land on scalar boundaries 0,1,3,7. Column past the end
        // clamps to the document end — never inside a multi-byte scalar.
        let boundaries: Vec<u64> = (0..=3u64)
            .map(|c| {
                line_column_to_byte(
                    &t,
                    LineColumn {
                        line: LineIndex(0),
                        column: ScalarIndex(c),
                    },
                )
                .0
            })
            .collect();
        assert_eq!(boundaries, vec![0, 1, 3, 7]);
        assert_eq!(
            line_column_to_byte(
                &t,
                LineColumn {
                    line: LineIndex(0),
                    column: ScalarIndex(99)
                }
            )
            .0,
            t.len()
        );
    }

    #[test]
    fn line_column_empty_line() {
        let t = table(b"a\n\nb");
        assert_eq!(
            line_column_to_byte(
                &t,
                LineColumn {
                    line: LineIndex(1),
                    column: ScalarIndex(0)
                }
            )
            .0,
            2
        );
        assert_eq!(
            line_column_to_byte(
                &t,
                LineColumn {
                    line: LineIndex(1),
                    column: ScalarIndex(5)
                }
            )
            .0,
            3
        );
    }
}
