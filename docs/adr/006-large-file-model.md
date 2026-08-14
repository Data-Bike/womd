# ADR-006: Large-file model

## Context
Files up to ~1 GB must open without full RAM load (§51–57, Invariant 6). Must support
byte-range loading (§52), UTF-8 boundary correction (§53), Markdown chunk context (§54),
virtualized rendering (§55), partial editing (§57), streaming save (§57).

## Options
1. **Read whole file into RAM.** Rejected (Invariant 6, §116).
2. **mmap the whole file + windowed access.** OS paging gives "lazy" load; but editing an
   mmap'd file in place violates source preservation (mutates original bytes) and is
   unsafe across platforms for write.
3. **mmap-backed immutable original + append-only edit buffers (Piece Table) + windowed
   parse with prefix/suffix context.** Original bytes are read-only (mmap or chunked
   read); edits live in a small edit log; the parser is given a window plus surrounding
   context to find sync points.

## Decision
**mmap-backed (or chunked-read) immutable original buffer + Piece Table edits + windowed
lossless parser with prefix/suffix context + virtualized rendering.**

- `editor-storage`: `trait DocumentStorage { len, read_range(offset, len) -> ByteChunk }`.
  Default impl: `MmapStorage` (memmap2) with a chunked-read fallback for network/odd FS.
- `read_range` (§52–53): fetch requested bytes + small prefix/suffix overlap; scan to a
  safe UTF-8 boundary; expand to line boundaries; return `ByteChunk { bytes, start_offset,
  leading_partial: bool, trailing_partial: bool }`.
- Chunk parser (§54): never assume the first byte of a chunk is document start. It
  consumes prefix context to determine enclosing block state (open fenced code? open list?
  block quote depth? open HTML block? link reference table?) then parses the window, then
  uses suffix context to avoid prematurely closing a block that continues below.
- `editor-text` Piece Table references the immutable original via `Arc<[u8]>` or an
  `Arc<MmapStorage>`; small edits keep the edit log tiny (Invariant 6).
- Virtualized rendering (§55): UI renders only visible lines + overscan; line metrics come
  from the line index, not from materializing all lines.
- Partial save (§57): stream `original segment | edit segment | …` to a temp file, fsync,
  atomic rename (§100).

## Consequences
- Invariant 6 satisfied structurally; Invariants 1–2 preserved (original bytes immutable).
- The parser must be context-aware at chunk boundaries — adds complexity, contained in
  `editor-markdown`'s `ChunkParser`.
- MVP delivers the storage abstraction + chunked read + UTF-8 boundary correction; full
  mmap + chunk parser context lands as the large-file vertical slice (§108, §95).
