# WoMD — Architecture

> Word-like Markdown document editor. Rust-first, Git-friendly, source-preserving, plugin-friendly, cross-platform.

## 1. Requirements analysis (summary)

The spec defines a document editor (not an IDE) whose canonical storage is `.md`, with four
non-negotiable invariants (§115):

1. `open -> save` without edits is byte-identical (source preservation).
2. A small edit produces a small Git diff.
3. UI does not depend on GitHub API.
4. Core does not depend on a specific repository provider.
5. Unknown Markdown syntax is never destroyed.
6. Large files need not be fully in memory.
7. Network/Git never block typing.
8. Plugin failure does not crash the editor.
9. All themes share one design system.
10. Raw Git diff is the source of truth for VCS.

The dominant technical risk is the **source-preserving document model** (§2): the naive
`Markdown -> AST -> HTML -> regenerate Markdown` pipeline is forbidden because it produces
huge meaningless diffs. Instead we use a **lossless AST** that stores, for every node, the
exact source byte range (including trivia: whitespace, marker style, delimiter runs, fence
length, line endings, BOM, trailing newline). Edits translate to minimal byte-range
`TextEdit`s against the original buffer; serialization stitches original segments and edit
segments back together without reformatting.

Secondary risks: large-file memory (§51–57), UI-thread latency (§81–83), plugin isolation
(§65–68), Git-as-first-class-citizen with provider abstraction (§31–34).

## 2. Crate layout

```
crates/
  editor-domain/      Domain primitives: ids, Position, ByteRange, Selection,
                      MarkdownProfile, typed errors, Event types. No I/O, no deps.
  editor-text/        Piece Table text buffer + coordinate systems
                      (byte / Unicode scalar / grapheme / line+column) + line index.
                      Backed by immutable original buffer (mmap-capable) + append-only
                      edit buffers. Source-preserving by construction.
  editor-core/        TextEdit, EditTransaction, UndoManager, SelectionModel,
                      DocumentBuffer facade (composes text + syntax + selection).
  editor-markdown/    Lossless/source-preserving CommonMark 0.31.2 + GFM parser,
                      AST with source spans + trivia, serializer (round-trip),
                      incremental reparse region, MarkdownExtension trait, profile.
  editor-diff/        Line diff, intraline diff, hunks, stage-hunk model,
                      semantic Markdown diff (built on editor-markdown).
  editor-git/         VersionControl port + default libgit2 implementation.
                      Branches, status, diff, stage/unstage, commit, fetch/pull/push.
  editor-storage/     DocumentStorage port, byte-range loading, UTF-8 boundary
                      correction, chunk parser context, atomic save.
  editor-plugin-api/  Plugin traits, manifest, permissions/capabilities,
                      extension points, EventBus contract, versioned SDK.
  editor-plugin-host/ Plugin host: manifest loading, capability enforcement,
                      WASM runtime (skeleton), crash isolation.
  editor-platform/    OS abstractions: file watcher, secure secret storage,
                      open-url, clipboard, high-DPI info. No business logic.
  editor-ui/          UI shell (Tauri 2 + custom editing surface). Thin view;
                      all editing logic delegated to Rust core.

adapters/
  github/             RepositoryHostAdapter impl for GitHub REST API.
  generic-git/        RepositoryHostAdapter impl for plain Git remotes
                      (no provider API; degrades to pure Git).

plugins/              First-party plugins (WASM). Empty in MVP skeleton.

docs/                 architecture.md, adr/, plugin-api.md, git-architecture.md,
                      document-model.md, markdown-compatibility.md, performance.md.
```

The exact split may evolve, but the **dependency direction is one-way** (see §3).

## 3. Dependency graph (arrows = "depends on")

```
                       editor-ui
                          |
            +-------------+-------------+
            |             |             |
      editor-platform  editor-plugin-host
            |             |
            |             v
            |       editor-plugin-api
            |             |
            v             |
       editor-git     editor-core  <-- editor-diff
            |          /   |   \
            |         /    |    \
            v        v     v     v
       editor-storage editor-text editor-markdown
            |          \    |    /
            |           \   |   /
            v            \  |  /
        editor-domain <----+ (everything depends on domain)
```

Rules:
- `editor-domain` depends on nothing (only `std` + small vendored utils).
- `editor-text`, `editor-markdown`, `editor-diff`, `editor-git`, `editor-storage` depend
  only on `editor-domain` (+ external libs).
- `editor-core` depends on `editor-domain`, `editor-text`, `editor-markdown`.
- `editor-plugin-api` depends only on `editor-domain`.
- `editor-plugin-host` depends on `editor-plugin-api`, `editor-core`.
- `editor-platform`, `editor-ui` depend on `editor-core` + ports.
- `adapters/*` depend on `editor-plugin-api`/`editor-git` ports, never on `editor-ui`.
- No crate depends on `editor-ui`. No crate depends on an adapter.
- GitHub never appears in `editor-domain`, `editor-core`, `editor-git` core, or
  `editor-markdown`.

## 4. Data flow

### 4.1 Open document
```
path -> DocumentStorage::open -> immutable original buffer (mmap or read)
      -> editor-text PieceTable (original-only, no edits)
      -> editor-markdown lossless parse -> SyntaxModel (AST with spans)
      -> editor-core DocumentBuffer (text + syntax + selection)
      -> editor-ui renders visible viewport only (virtualized)
```

### 4.2 Typing (interactive edit)
```
UI input event -> CommandSystem dispatch -> semantic command (e.g. InsertText)
   -> resolves to Vec<TextEdit> (byte ranges against PieceTable)
   -> EditTransaction { edits, selection_before, selection_after }
   -> UndoManager.push(transaction)
   -> PieceTable.apply(edits)         // O(log n) per edit, no full copy
   -> editor-markdown incremental reparse of affected region
   -> SyntaxModel patch (spans outside affected region unchanged)
   -> editor-ui re-renders only changed viewport
```
Typing never touches Git, network, or plugins; it is purely local + synchronous on a
worker, with the UI thread only dispatching and receiving a patched snapshot.

### 4.3 Save (source-preserving)
```
DocumentBuffer -> serialize: stitch original segments + edit segments
              -> atomic write: temp file -> flush -> fsync -> rename over original
              -> invalidate Git diff cache (key includes working-tree metadata)
```
Serialization does **not** regenerate Markdown from a semantic model; it emits the original
bytes for unchanged regions and the edited bytes for changed regions. This is what makes
the diff minimal.

### 4.4 Git operation
```
UI -> CommandSystem -> GitCommand -> VersionControl port
   -> editor-git impl runs on Git worker (async, cancellable)
   -> EventBus publishes RepositoryStatusChanged
   -> UI updates panels; typing unaffected
```

## 5. Plugin boundaries

```
Core (Rust)  --depends on-->  Ports (traits in editor-plugin-api / editor-domain)
                                  ^
                                  |
                              Plugin API (versioned SDK)
                                  ^
                                  |
                              Plugins (WASM, sandboxed)
```

- Plugins never link against core ABI; they implement traits from `editor-plugin-api`
  against a versioned SDK and run in a WASM sandbox (ADR-005).
- Capability model (§68): a plugin manifest declares permissions
  (`filesystem`, `network`, `git_credentials`, `clipboard`, `process`); the host enforces
  them. No ambient authority.
- Extension points (§69): commands, toolbar/menus, shortcuts, Markdown syntax extensions,
  block/inline renderers, themes, exporters/importers, repository providers, asset
  handlers, sidebar panels, status bar items.
- Unknown Markdown syntax (§24, Invariant 5): if an extension is absent, the parser
  preserves the raw source span verbatim and exposes it as an `UnknownBlock`/`UnknownInline`
  node; serialization emits it unchanged.

## 6. Git adapters (Ports & Adapters)

```
editor-git: trait VersionControl { status, diff, stage, unstage, commit,
            branches, checkout, fetch, pull, push, ... }
            + trait CredentialProvider { credentials_for(remote) }
            + default impl VersionControlGit2 (libgit2)

adapters/:  trait RepositoryHostAdapter { provider_id, authenticate,
            repository_metadata, pull_requests, create_pull_request,
            remote_branches, open_remote_url }
            - github:    GitHubAdapter (REST API)
            - generic-git: GenericGitAdapter (degrades to pure Git; no API)
```

UI talks only to `VersionControl` and `RepositoryHostAdapter`. Adding GitLab/Bitbucket/
Gitea/Forgejo/AzureDevOps requires a new adapter crate, zero core changes (Invariant 4).
A GitLab repo works as a plain Git repo even without a GitLab adapter (§34).

## 7. Document model (lossless)

The AST is `editor_markdown::ast::Document` where every node carries:

```rust
pub struct SourceSpan { pub start: ByteOffset, pub end: ByteOffset } // half-open
pub struct Trivia { pub span: SourceSpan } // whitespace, blank lines, marker chars

pub enum Block {
    Paragraph(Paragraph),
    Heading(Heading),        // ATX | Setext, with closing #'s, marker style
    ThematicBreak(ThematicBreak), // --- | *** | ___ + spacing
    BlockQuote(BlockQuote),  // '> ' markers per line preserved
    List(List),              // marker char (-*+ | 1.|1)), start value, tight/loose
    ListItem(ListItem),
    CodeBlock(CodeBlock),    // fenced (backtick|tilde, fence length, info string) | indented
    Table(Table),            // GFM: alignment, cell padding, escaped pipes
    HtmlBlock(HtmlBlock),
    LinkReferenceDefinition(LinkReferenceDefinition),
    UnknownBlock(UnknownBlock), // extension absent -> verbatim source
}
```

Each variant stores the **trivia and marker choices** from source. The serializer walks the
AST and, for each node, either emits its original `SourceSpan` bytes (unchanged) or, for
edited nodes, regenerates only that node's text from its (possibly modified) content +
preserved trivia. Paragraphs/whitespace outside the edit are never touched.

## 8. Coordinate system (§4.1)

`editor-text` exposes five coordinate kinds and explicit conversions:

| Coordinate            | Meaning                                  |
|-----------------------|------------------------------------------|
| `ByteOffset`          | u8 index into UTF-8 bytes                |
| `ScalarIndex`         | Unicode scalar value index               |
| `GraphemeIndex`       | Extended grapheme cluster index          |
| `LineColumn`          | line (0-based) + column in scalars       |
| `VisualColumn`        | column after tab/wrap expansion (UI)     |

Conversions are explicit (never `byte == char`). The Piece Table's line index maps
`Line -> ByteOffset` in O(log n); within a line, scalar/grapheme columns are computed on
demand (lines are short).

## 9. Threading model (§82)

```
UI thread          : input dispatch, viewport render, command palette, panels.
Document worker    : PieceTable edits, incremental reparse, search.
Parser worker(s)   : heavy reparses, outline building for large files.
Git worker         : all VersionControl operations (async, cancellable).
Plugin workers     : one per plugin (WAMR/wasmtime instance), isolated.
Network worker     : adapter HTTP (reqwest) for GitHub etc.
```

All cross-thread communication is via channels + `CancellationToken`. The UI thread never
blocks on I/O or compute >1 frame.

## 10. MVP scope (§108) mapped to crates

| MVP feature                | Crate(s)                              |
|----------------------------|---------------------------------------|
| open/save `.md`            | editor-storage, editor-core           |
| source-preserving editing  | editor-text, editor-markdown, editor-core |
| Document/Source/Split mode | editor-ui (modes)                     |
| CommonMark + GFM           | editor-markdown                       |
| tables, task lists, images, code, links | editor-markdown          |
| folder tree, outline       | editor-ui, editor-markdown (outline)  |
| Git: status/diff/stage/commit/branch/checkout/fetch/pull/push/file history | editor-git, adapters/generic-git |
| diff: inline/side-by-side/intraline/hunks | editor-diff, editor-ui   |
| themes, command palette, shortcuts | editor-ui, editor-core        |
| plugin API skeleton        | editor-plugin-api, editor-plugin-host |
| partial file reading       | editor-storage                        |

Phase 2/3 features (§109–110) map onto the same ports without core rework.
