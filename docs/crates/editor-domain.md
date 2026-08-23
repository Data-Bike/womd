# `editor-domain`

Domain primitives for WoMD. This is the innermost layer of the crate dependency graph and has no dependencies beyond the Rust standard library and a few small vendored utilities.

## Responsibilities

- Defines the shared vocabulary that every other crate uses: `ByteOffset`, `ByteRange`, `Position`, `Selection`, `LineColumn`, `VisualColumn`, `MarkdownProfile`, `EditResult`, `Encoding` and typed error types.
- Provides the identifier types for documents, blocks, edits, tabs and plugins.
- Exposes the event bus abstractions that the UI and plugins observe.

## Key types

- `ByteOffset` / `ByteRange` — half-open UTF-8 byte offsets used by the piece table, parser and backend search.
- `Selection` — caret or range in the document, always expressed in `ByteOffset` internally.
- `Position` / `LineColumn` / `VisualColumn` — user-facing coordinate spaces.
- `MarkdownProfile` — CommonMark vs. GFM and extension flags.
- `DomainError` and friends — the common error vocabulary shared by all crates.

## Design notes

`editor-domain` deliberately contains no I/O, no OS calls, no UI code and no Markdown parsing logic. It is the only crate that does not depend on `editor-domain` — every other crate depends on it. This keeps the domain model stable and makes the hexagonal architecture possible.
