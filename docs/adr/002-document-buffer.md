# ADR-002: Document buffer (text model)

## Context
The buffer must support fast insert/delete/replace, undo, line lookup, byte-offset lookup,
must not copy the whole document for small edits (§56–57), must support mmap-backed large
files (§51–53), and must be source-preserving by construction (§2, Invariants 1–2).

## Options
1. **Single `String`.** Rejected (§116): O(n) edits, full copy, no large-file support.
2. **Rope (balanced tree of strings).** O(log n) edits/lookups; but original bytes are
   fragmented and mutated, making "stitch original + edits" serialization and mmap backing
   awkward.
3. **Piece Table (original buffer + append-only edit buffer + list of pieces).** Original
   bytes are immutable (can be mmap-backed); edits only append to the edit buffer and
   rewrite the piece list. Naturally source-preserving: unchanged pieces reference the
   original buffer verbatim. Line/offset lookups need an auxiliary index.
4. **Rope/Piece Table hybrid with a line index.** Piece Table + a balanced index over
   pieces keyed by cumulative byte length and line counts → O(log n) offset<->line and
   piece lookup.

## Decision
**Piece Table backed by an immutable original buffer (mmap-capable) + append-only edit
buffers, augmented with a line-offset index (piece table + B-tree-like index by cumulative
byte length and line count).**

- `editor_text::PieceTable` holds `original: Arc<[u8]>` (or mmap handle via
  `editor-storage`) and `edit_log: Vec<u8>` (append-only).
- `pieces: Vec<Piece>` where `Piece { source: PieceSource, start: usize, len: usize }`,
  `PieceSource::Original | EditLog`.
- `LineIndex` maps `Line -> (piece_index, byte_offset_in_piece)` and provides
  `byte_offset_to_line` / `line_to_byte_offset` in O(log n).
- Edits replace a contiguous byte range with new text: split pieces at boundaries, splice
  in a new piece referencing the appended edit bytes. Original bytes are never mutated.
- Serialization = iterate pieces and copy their bytes; unchanged regions are literally the
  original bytes → byte-identical round-trip when no edits (Invariant 1) and minimal diff
  for small edits (Invariant 2).

## Consequences
- Source preservation is a structural property, not a discipline.
- Partial editing (§57): saving a huge file streams `original segment | edit segment | ...`
  to a temp file, then atomic rename.
- mmap backing for the original buffer enables >RAM files (Invariant 6); the edit log stays
  small for small edits.
- Coordinate conversions (§4.1, §8) are computed via the line index + per-line scalar/
  grapheme resolution.
- Undo is handled at the `EditTransaction` level in `editor-core` (ADR-002 complements
  §61), not by mutating the buffer.
