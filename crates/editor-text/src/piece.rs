//! Piece Table implementation (ADR-002).

use std::sync::Arc;

use editor_domain::{ByteOffset, ByteRange, ByteSource};

/// Where a piece's bytes live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceSource {
    /// Bytes from the immutable original buffer.
    Original,
    /// Bytes from the append-only edit log.
    EditLog,
}

/// A contiguous slice of bytes from one of the two buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    pub source: PieceSource,
    /// Start offset within the source buffer.
    pub start: u64,
    /// Length in bytes.
    pub len: u64,
}

/// The source-preserving text buffer.
pub struct PieceTable {
    /// Immutable original document bytes. Never mutated (Invariant 1, 2, 6).
    /// Uses `ByteSource` trait to support mmap-backed files without copying
    /// the entire file into memory (Invariant 6: >RAM files).
    original: Arc<dyn ByteSource>,
    /// Append-only edit log.
    edit_log: Vec<u8>,
    /// Ordered list of pieces describing the current document.
    pieces: Vec<Piece>,
    /// Total byte length (cached).
    total_len: u64,
    /// Sorted byte offsets of line starts (line 0 starts at 0).
    line_starts: Vec<u64>,
}

impl PieceTable {
    /// Create a table from the original document bytes. No edits yet.
    pub fn from_original(original: Arc<dyn ByteSource>) -> Self {
        let line_starts = compute_line_starts(original.as_bytes());
        let total_len = original.len() as u64;
        let pieces = if total_len > 0 {
            vec![Piece { source: PieceSource::Original, start: 0, len: total_len }]
        } else {
            Vec::new()
        };
        Self { original, edit_log: Vec::new(), pieces, total_len, line_starts }
    }

    /// Create from a owned byte vector (convenience for tests / small docs).
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        let original: Arc<dyn ByteSource> = Arc::new(editor_domain::ArcByteSource::new(bytes));
        Self::from_original(original)
    }

    /// Total byte length of the current document.
    pub fn len(&self) -> u64 {
        self.total_len
    }

    /// Whether the document is empty.
    pub fn is_empty(&self) -> bool {
        self.total_len == 0
    }

    /// The immutable original buffer (for storage/mmap integration).
    pub fn original(&self) -> &Arc<dyn ByteSource> {
        &self.original
    }

    /// Sorted byte offsets of line starts.
    pub fn line_starts(&self) -> &[u64] {
        &self.line_starts
    }

    /// Extract the bytes of a half-open range as an owned `Vec<u8>`.
    pub fn extract_bytes(&self, range: ByteRange) -> Vec<u8> {
        let start = range.start.0.min(self.total_len);
        let end = range.end.0.min(self.total_len).max(start);
        let mut out = Vec::with_capacity((end - start) as usize);
        let mut remaining_start = start;
        let target_end = end;
        let mut cursor = 0u64;
        for piece in &self.pieces {
            let piece_end = cursor + piece.len;
            if piece_end <= remaining_start {
                cursor = piece_end;
                continue;
            }
            if cursor >= target_end {
                break;
            }
            let local_start = remaining_start.saturating_sub(cursor);
            let local_end = (target_end - cursor).min(piece.len);
            let src = self.piece_bytes(piece);
            out.extend_from_slice(&src[local_start as usize..local_end as usize]);
            remaining_start = cursor + local_end;
            cursor = piece_end;
        }
        out
    }

    /// All current bytes (serialization). Unchanged regions are literally the original
    /// bytes (Invariant 1, 2).
    pub fn to_bytes(&self) -> Vec<u8> {
        self.extract_bytes(ByteRange::new(ByteOffset(0), ByteOffset(self.total_len)))
    }

    /// Extract a byte range [start, end) without serializing the entire document.
    /// Used for virtualized rendering of large documents — only the requested
    /// block's bytes are copied, not the full 100+ MB buffer.
    pub fn to_range(&self, start: u64, end: u64) -> Vec<u8> {
        let s = start.min(self.total_len);
        let e = end.min(self.total_len).max(s);
        self.extract_bytes(ByteRange::new(ByteOffset(s), ByteOffset(e)))
    }

    /// Apply an edit: replace `range` with `replacement`. Returns the byte length of the
    /// inserted text.
    pub fn apply_edit(&mut self, range: ByteRange, replacement: &[u8]) -> u64 {
        let start = range.start.0.min(self.total_len);
        let end = range.end.0.min(self.total_len).max(start);

        let edit_log_start = self.edit_log.len() as u64;
        self.edit_log.extend_from_slice(replacement);
        let inserted_len = replacement.len() as u64;

        // Split pieces at [start, end) boundaries, then splice.
        let (left, removed, right) = self.split_at(start, end);
        let _ = removed; // discarded bytes (no longer referenced)

        let new_piece = if inserted_len > 0 {
            Some(Piece { source: PieceSource::EditLog, start: edit_log_start, len: inserted_len })
        } else {
            None
        };

        let mut pieces = left;
        if let Some(p) = new_piece {
            pieces.push(p);
        }
        pieces.extend(right);
        self.pieces = pieces;
        self.total_len = self.pieces.iter().map(|p| p.len).sum();

        self.reindex_lines(start, end, inserted_len);
        inserted_len
    }

    /// Insert text at `pos`.
    pub fn insert(&mut self, pos: ByteOffset, text: &[u8]) -> u64 {
        self.apply_edit(ByteRange::empty(pos), text)
    }

    /// Delete a half-open range.
    pub fn delete(&mut self, range: ByteRange) -> u64 {
        self.apply_edit(range, &[])
    }

    /// Replace a range with new text.
    pub fn replace(&mut self, range: ByteRange, text: &[u8]) -> u64 {
        self.apply_edit(range, text)
    }

    fn piece_bytes(&self, piece: &Piece) -> &[u8] {
        match piece.source {
            PieceSource::Original => {
                let orig = self.original.as_bytes();
                &orig[piece.start as usize..(piece.start + piece.len) as usize]
            }
            PieceSource::EditLog => &self.edit_log[piece.start as usize..(piece.start + piece.len) as usize],
        }
    }

    /// Split the piece list into (left, removed_pieces, right) at byte boundaries `start`
    /// and `end`. `left` ends exactly at `start`; `right` begins exactly at `end`.
    fn split_at(&self, start: u64, end: u64) -> (Vec<Piece>, Vec<Piece>, Vec<Piece>) {
        let mut left = Vec::new();
        let mut removed = Vec::new();
        let mut right = Vec::new();
        let mut cursor = 0u64;
        for piece in &self.pieces {
            let piece_end = cursor + piece.len;
            if piece_end <= start {
                left.push(*piece);
                cursor = piece_end;
                continue;
            }
            if cursor >= end {
                right.push(*piece);
                cursor = piece_end;
                continue;
            }
            // Piece overlaps [start, end).
            let local_start = start.saturating_sub(cursor);
            let local_end = (end - cursor).min(piece.len);
            // Head before start.
            if local_start > 0 {
                left.push(Piece { source: piece.source, start: piece.start, len: local_start });
            }
            // Removed middle.
            if local_end > local_start {
                removed.push(Piece {
                    source: piece.source,
                    start: piece.start + local_start,
                    len: local_end - local_start,
                });
            }
            // Tail after end.
            if piece.len > local_end {
                right.push(Piece {
                    source: piece.source,
                    start: piece.start + local_end,
                    len: piece.len - local_end,
                });
            }
            cursor = piece_end;
        }
        (left, removed, right)
    }

    /// Incrementally update `line_starts` after replacing [old_start, old_end) with
    /// `inserted_len` bytes (ADR-002).
    fn reindex_lines(&mut self, old_start: u64, old_end: u64, inserted_len: u64) {
        let delta = inserted_len as i64 - (old_end - old_start) as i64;

        // Find the line start that contains `old_start` (largest line_start <= old_start).
        let containing = match self.line_starts.binary_search(&old_start) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        // Keep line starts [0..=containing]; drop line starts strictly inside (old_start, old_end].
        let mut kept: Vec<u64> = self.line_starts[..=containing].to_vec();
        // Shift tail line starts by `delta` without overflowing i64.
        let shift = |ls: u64| -> u64 {
            let shifted = (ls as i128) + (delta as i128);
            if shifted < 0 {
                0
            } else if shifted > u64::MAX as i128 {
                u64::MAX
            } else {
                shifted as u64
            }
        };
        let mut tail: Vec<u64> = self
            .line_starts
            .iter()
            .copied()
            .filter(|&ls| ls > old_end)
            .map(shift)
            .collect();

        // New line starts from the inserted text, at absolute offset old_start + pos + 1
        // for each newline. A trailing newline legitimately starts the next (shifted)
        // line, so we include it; dedup below collapses any collision with shifted tail.
        // Clamp the end to total_len in case inserted_len overflows u64 (defensive).
        let inserted_end = old_start.saturating_add(inserted_len).min(self.total_len);
        let inserted_bytes = self.extract_bytes(ByteRange::new(
            ByteOffset(old_start),
            ByteOffset(inserted_end),
        ));
        let mut new_starts: Vec<u64> = Vec::new();
        for (i, &b) in inserted_bytes.iter().enumerate() {
            // `\n` always ends a line. `\r` ends a line only when the next
            // byte is not `\n` — inside `\r\n` the break belongs to the `\n`.
            // The next byte may sit just past the inserted window.
            let is_break_end = b == b'\n'
                || (b == b'\r' && {
                    inserted_bytes.get(i + 1).copied().or_else(|| {
                        self.extract_bytes(ByteRange::new(
                            ByteOffset(inserted_end),
                            ByteOffset((inserted_end + 1).min(self.total_len)),
                        ))
                        .first()
                        .copied()
                    }) != Some(b'\n')
                });
            if is_break_end {
                new_starts.push(old_start.saturating_add(i as u64).saturating_add(1));
            }
        }

        // CRLF pairing across the left edit boundary: a bare `\r` at
        // `old_start - 1` used to end a line at `old_start`. If the byte now
        // at `old_start` is `\n` (inserted `\n` after `\r`, or a deletion
        // that joined `\r` to a `\n` after it), the two form ONE terminator
        // ending at `\n` — the kept start at `old_start` points mid-line.
        if old_start > 0
            && kept.last() == Some(&old_start)
            && self
                .extract_bytes(ByteRange::new(ByteOffset(old_start - 1), ByteOffset(old_start)))
                .first()
                == Some(&b'\r')
            && old_start < self.total_len
            && self
                .extract_bytes(ByteRange::new(ByteOffset(old_start), ByteOffset(old_start + 1)))
                .first()
                == Some(&b'\n')
        {
            kept.pop();
        }
        kept.append(&mut new_starts);
        kept.append(&mut tail);
        kept.sort_unstable();
        kept.dedup();
        // Boundary fixup: `compute_line_starts` drops the line start at `total`
        // when the document ends in `\n`. The kept prefix therefore may lack a
        // legitimate start at `old_start` (insert at EOF after a trailing
        // newline) — re-derive it from the byte preceding `old_start`.
        if old_start > 0
            && old_start < self.total_len
            && matches!(
                self
                    .extract_bytes(ByteRange::new(ByteOffset(old_start - 1), ByteOffset(old_start)))
                    .first(),
                Some(&b'\n') | Some(&b'\r')
            )
            // ...unless the break is a `\r` that now pairs with the `\n` at
            // `old_start` — the CRLF fixup above already dropped that start.
            && !(self
                .extract_bytes(ByteRange::new(ByteOffset(old_start - 1), ByteOffset(old_start)))
                .first()
                == Some(&b'\r')
                && self
                    .extract_bytes(ByteRange::new(ByteOffset(old_start), ByteOffset(old_start + 1)))
                    .first()
                    == Some(&b'\n'))
            && kept.binary_search(&old_start).is_err()
        {
            kept.push(old_start);
            kept.sort_unstable();
        }
        // Mirror `compute_line_starts`: a document ending in a line break
        // (`\n` or trailing `\r`) has no line start at `total` — the break
        // does not open an extra empty line.
        if self.total_len > 0
            && matches!(
                self
                    .extract_bytes(ByteRange::new(
                        ByteOffset(self.total_len - 1),
                        ByteOffset(self.total_len),
                    ))
                    .first(),
                Some(&b'\n') | Some(&b'\r')
            )
        {
            kept.retain(|&v| v != self.total_len);
        }
        self.line_starts = kept;
    }
}

fn compute_line_starts(bytes: &[u8]) -> Vec<u64> {
    let mut starts = vec![0u64];
    for (i, &b) in bytes.iter().enumerate() {
        // A line break ends at `\n` or at a `\r` not followed by `\n` (bare
        // CR — classic Mac line ending). Inside `\r\n` only the `\n` counts.
        let is_break_end = b == b'\n' || (b == b'\r' && bytes.get(i + 1) != Some(&b'\n'));
        if is_break_end {
            starts.push((i + 1) as u64);
        }
    }
    // Remove a trailing empty line start if the file ends with a line break
    // (no extra line). A trailing `\r` is a bare CR — `bytes.get(len)` is
    // never `\n` — so the same single check covers `\n`, `\r\n`, and `\r`.
    if matches!(bytes.last(), Some(&b'\n') | Some(&b'\r')) {
        starts.pop();
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_unchanged_is_byte_identical() {
        let original = b"# Hello\n\nSome **bold** text.\n\n- a\n- b\n".to_vec();
        let table = PieceTable::from_bytes(original.clone());
        assert_eq!(table.to_bytes(), original);
    }

    #[test]
    fn small_edit_preserves_rest() {
        let original = b"# Test\n\nHello beautiful world.\n".to_vec();
        let mut table = PieceTable::from_bytes(original.clone());
        // Replace "beautiful" with "great".
        let word_start = original.windows(9).position(|w| w == b"beautiful").unwrap();
        table.replace(
            ByteRange::new(ByteOffset(word_start as u64), ByteOffset((word_start + 9) as u64)),
            b"great",
        );
        let result = table.to_bytes();
        assert_eq!(result, b"# Test\n\nHello great world.\n");
        // Everything after the edit is byte-identical to the original tail.
        let tail_start = word_start + 9;
        assert_eq!(&result[word_start + 5..], &original[tail_start..]);
    }

    #[test]
    fn line_index_after_edit() {
        let mut table = PieceTable::from_bytes(b"a\nb\nc\n".to_vec());
        table.insert(ByteOffset(0), b"X\nY\n");
        let bytes = table.to_bytes();
        assert_eq!(bytes, b"X\nY\na\nb\nc\n");
        assert_eq!(table.line_starts(), &[0, 2, 4, 6, 8]);
    }

    #[test]
    fn delete_collapses_pieces() {
        let mut table = PieceTable::from_bytes(b"Hello, world!".to_vec());
        table.delete(ByteRange::new(ByteOffset(5), ByteOffset(7)));
        assert_eq!(table.to_bytes(), b"Helloworld!");
    }

    /// Incremental `line_starts` must always match a fresh `compute_line_starts`
    /// over the same bytes — across a battery of edits at every position.
    #[test]
    fn line_starts_incremental_matches_fresh_index() {
        let docs: Vec<&[u8]> = vec![
            b"", b"a", b"a\n", b"a\nb", b"a\nb\n", b"\n", b"\n\n", b"a\n\nb\n",
            // Bare-CR (classic Mac) and CRLF families — the index must count
            // `\r` not followed by `\n` as a break too.
            b"a\rb", b"a\r", b"a\rb\rc", b"a\r\nb", b"a\r\nb\r\nc", b"\r", b"\r\n",
            b"a\r\n\rb", b"\ra", b"\r\na",
        ];
        let edits: Vec<&[u8]> = vec![
            b"x", b"x\n", b"\nx", b"\n", b"",
            // Insertions that create, complete, or break CRLF pairs at the
            // edit boundaries.
            b"\r", b"\r\n", b"x\r", b"\rx", b"x\ry", b"\n\r",
        ];
        for doc in &docs {
            let len = doc.len() as u64;
            for pos in 0..=len {
                for ins in &edits {
                    let mut table = PieceTable::from_bytes(doc.to_vec());
                    table.insert(ByteOffset(pos), ins);
                    let fresh = PieceTable::from_bytes(table.to_bytes());
                    assert_eq!(
                        table.line_starts(),
                        fresh.line_starts(),
                        "doc={:?} insert {:?} at {}",
                        String::from_utf8_lossy(doc),
                        String::from_utf8_lossy(ins),
                        pos
                    );
                }
            }
            // Also check replacements of each byte range.
            for start in 0..len {
                for end in start..len {
                    for rep in [&b"z\n"[..], &b"\r"[..], &b"z\r\n"[..]] {
                        let mut table = PieceTable::from_bytes(doc.to_vec());
                        table.replace(
                            ByteRange::new(ByteOffset(start), ByteOffset(end + 1)),
                            rep,
                        );
                        let fresh = PieceTable::from_bytes(table.to_bytes());
                        assert_eq!(
                            table.line_starts(),
                            fresh.line_starts(),
                            "doc={:?} replace [{},{}) with {:?}",
                            String::from_utf8_lossy(doc),
                            start,
                            end + 1,
                            String::from_utf8_lossy(rep)
                        );
                    }
                }
            }
        }
    }

    /// Inserting at EOF after a trailing newline must create a line start at
    /// the insertion point (the old index dropped it via the trailing-'\n' pop).
    #[test]
    fn insert_at_eof_after_trailing_newline_adds_line_start() {
        let mut table = PieceTable::from_bytes(b"a\n".to_vec());
        table.insert(ByteOffset(2), b"x");
        assert_eq!(table.to_bytes(), b"a\nx");
        assert_eq!(table.line_starts(), &[0, 2]);
    }

    /// Replacing a middle segment ending in '\n' at the very end must not leave
    /// a phantom line start at `total`.
    #[test]
    fn replace_ending_in_newline_at_eof_has_no_phantom_start() {
        let mut table = PieceTable::from_bytes(b"a\nX\n".to_vec());
        table.replace(ByteRange::new(ByteOffset(2), ByteOffset(4)), b"Y\n");
        assert_eq!(table.to_bytes(), b"a\nY\n");
        assert_eq!(table.line_starts(), &[0, 2]);
    }
}
