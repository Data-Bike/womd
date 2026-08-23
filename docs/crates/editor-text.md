# `editor-text`

Source-preserving piece table text buffer for WoMD. This is the canonical text model that the rest of the editor builds on.

## Responsibilities

- Store the original document bytes immutably while appending edits to an append-only edit log.
- Provide `O(log n)` piece lookup and edits by rewriting the piece list, never the original buffer.
- Offer multiple coordinate systems — byte, Unicode scalar, grapheme cluster and line/column — with explicit conversions between them.
- Maintain an incremental line-start index that updates in `O(log n + edited_lines)` per edit.
- Support `mmap`-backed original buffers so files larger than RAM can be opened without a full copy.

## Key types

- `PieceTable` — the main text buffer. Original bytes are referenced as `Arc<[u8]>`; edits become new pieces.
- `Piece`, `PieceChain` — the internal representation of the original + edit buffers.
- `TextCursor` / `LineIndex` — fast coordinate conversions and line lookups.

## Design notes

Because unchanged pieces still reference the original buffer, `open` → `save` with no edits is byte-identical (Invariant 1). A small edit only replaces the affected pieces, producing a small diff (Invariant 2). The piece table is the foundation of the source-preservation guarantees. It can also be backed by `mmap` for very large files (Invariant 6).
