# `editor-markdown`

Lossless, source-preserving CommonMark 0.31.2 + GFM Markdown parser, AST and serializer.

## Responsibilities

- Parse Markdown into a `Document` whose block and inline spans exactly partition the source byte range `[0, len)`.
- Store the original bytes inside every node as a `SourceSpan` plus `Trivia` (whitespace, marker characters, fence length, alignment, escaped delimiters, line endings).
- Serialize an unchanged `Document` to the exact original bytes (Invariant 1).
- Regenerate only dirty nodes after an edit, leaving all other nodes verbatim (Invariant 2).
- Preserve unknown or unsupported extension syntax as `UnknownBlock` / `UnknownInline` (Invariant 5).
- Support incremental reparse of an affected region after a text edit.

## Key types

- `Document`, `Block`, `Inline` — AST nodes carrying `SourceSpan` and trivia.
- `SourceSpan` — half-open byte range `[start, end)` into the original source.
- `Trivia` — non-semantic bytes (padding, blank lines, markers) that must be preserved.
- `MarkdownProfile` and `MarkdownExtension` — CommonMark/GFM selection and syntax-extension hooks.

## Design notes

The parser never discards formatting choices. For example, setext vs. ATX headings, `-` vs. `*` list markers, backtick vs. tilde fences, and the exact number of `#` characters are all preserved. The serializer walks the AST and, for each node, either emits the original `SourceSpan` or, for edited dirty nodes, regenerates the node from its updated content while preserving its stored trivia.
