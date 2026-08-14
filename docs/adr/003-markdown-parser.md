# ADR-003: Markdown parser & document model

## Context
Must implement CommonMark 0.31.2 + GFM (§3–22), be **lossless/source-preserving** (§2,
Invariants 1–2), support profiles (`CommonMark`/`Gfm`/`Custom`), an extension API for
non-standard syntax (§24), incremental reparsing (§58), and never destroy unknown syntax
(Invariant 5). Regex parsers are forbidden (§116).

## Options
1. **`pulldown-cmark` (event-based).** Mature, fast, CommonMark+GFM. Rejected as the
   *document model*: it is event-based, drops trivia (whitespace, marker style, delimiter
   runs), and cannot round-trip byte-identically. Could be used only as an optional
   *validator*, but we avoid a second parser to keep one source of truth (§111).
2. **`markdown-rs` (CM AST).** CommonMark conformant but not lossless; same trivia problem.
3. **Hand-written lossless parser.** Stores, for every node, the exact `SourceSpan`
   (byte range) plus trivia and marker choices. Round-trips byte-identically. Full control
   over incremental reparse regions. More implementation effort, but the only option that
   satisfies Invariants 1, 2, 5.

## Decision
**Hand-written lossless parser in `editor-markdown`**, producing a `Document` AST where
every node carries its `SourceSpan` and preserved trivia (marker char, fence length, info
string, closing `#`s, emphasis delimiter run kind/length, link reference style, line
endings, etc.).

- Profile: `enum MarkdownProfile { CommonMark, Gfm, Custom(ProfileId) }` (§3).
- Parser is a two-stage block/inline parser following CommonMark 0.31.2 algorithm
  (delimiter runs, link reference resolution, etc.), NOT regex.
- Extension API: `trait MarkdownExtension { id, parse_block, parse_inline, render,
  serialize, contribute_commands }` (§24). Extensions plug into the parser at well-defined
  precedence points. Unknown extension syntax → `UnknownBlock`/`UnknownInline` nodes
  holding the verbatim `SourceSpan`; serializer emits them unchanged (Invariant 5).
- Serializer: walk AST; for unchanged nodes emit `buffer[span]` verbatim; for edited nodes
  regenerate only that node from content + preserved trivia. Never reformat outside edits.
- Incremental reparse (§58): a `TextEdit` → find affected block boundaries → expand to safe
  sync points → reparse only that region → patch `SyntaxModel` spans (unaffected spans stay
  byte-identical).

## Consequences
- Invariants 1, 2, 5 are enforceable by construction and by tests (§91–92).
- We own conformance; a CommonMark/GFM conformance suite (§90) is mandatory.
- More upfront code; mitigated by implementing in vertical slices (paragraphs → headings →
  lists → code → inline emphasis/links → tables → task lists → …).
- `pulldown-cmark`/`markdown-rs` may be used *only* in the conformance test harness as a
  cross-check, never in the editor data path.
