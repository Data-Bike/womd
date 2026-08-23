# `editor-storage`

Document storage port and implementations: opening, reading partial byte ranges, UTF-8 boundary correction and atomic save.

## Responsibilities

- Define the `DocumentStorage` port for opening and saving documents.
- Support `InMemoryStorage` for tests and `MmapStorage` for very large files (ADR-006).
- Read arbitrary byte ranges and correct them to safe UTF-8 boundaries and line boundaries.
- Report whether a returned range is partial so the Markdown parser can use prefix/suffix context for fenced code, block quotes and lists.
- Perform atomic saves via write-to-temp, flush, fsync and rename over the original.

## Key types

- `DocumentStorage` / `InMemoryStorage` / `MmapStorage` — storage backends.
- `ByteSource` — an immutable, reference-counted view into the original bytes.
- `read_range` / `correct_utf8_boundary` — helpers for safe partial reads.

## Design notes

The original document bytes are immutable. For files larger than RAM, `MmapStorage` maps the file rather than reading it into memory (Invariant 6). The crate root denies `unsafe`; the single `mmap` call is re-allowed only in `src/mmap.rs` (ADR-006). This keeps source preservation and large-file support together.
