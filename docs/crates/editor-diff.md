# `editor-diff`

Line, intraline and semantic Markdown diff for WoMD. This crate computes what changed and how to present it to the user.

## Responsibilities

- Myers `O(ND)` line diff for fast, accurate comparison of two text buffers.
- Hunk generation with configurable context for the Git diff view.
- Intraline token-level LCS diff for highlighting changed words inside a line.
- Semantic Markdown diff built on `editor-markdown` for structured comparison of AST nodes.
- Stage/unstage hunk model used by the Git integration.

## Key types

- `Diff` and `Hunk` — line-level changes with before/after byte ranges.
- `IntraLineDiff` — token-level changes within a line.
- `SemanticDiff` — structural diff of Markdown ASTs.

## Design notes

Raw Git diff remains the source of truth for version control (Invariant 10). The algorithms here are pure Rust and fully testable without a Git repository. They are used both to render Git diffs in the UI and to provide an additional semantic Markdown diff view for users who want to see changes at the document-structure level.
