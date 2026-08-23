# WoMD Project Documentation

This directory contains the architecture records and design documentation for **WoMD** — a word-like, source-preserving, Git-friendly Markdown document editor. The editor is written primarily in Rust with a Tauri 2 frontend.

For the high-level project overview, see the [root README](../README.md).

## Table of contents

1. [What is WoMD?](#what-is-womd)
2. [Design invariants](#design-invariants)
3. [System architecture](#system-architecture)
4. [Crate inventory](#crate-inventory)
5. [Data flow](#data-flow)
6. [Build, test and run](#build-test-and-run)
7. [Dependency rules](#dependency-rules)
8. [Threading and performance](#threading-and-performance)
9. [Plugin model](#plugin-model)
10. [Roadmap and known gaps](#roadmap-and-known-gaps)
11. [Document references](#document-references)

## What is WoMD?

WoMD is a desktop Markdown editor whose canonical storage is a plain `.md` file. Unlike WYSIWYG editors that round-trip through HTML, WoMD preserves the exact source bytes and only regenerates nodes that have actually changed. This is the foundation of its Git-friendliness: a single word change in a paragraph produces a single-word diff, not a reformatted paragraph.

The editor targets writers, maintainers and teams who treat Markdown as the source of truth and want Word-like conveniences — rich formatting, outline navigation, find/replace, git integration — without losing the ability to diff and version documents in the same way as code.

## Design invariants

1. **Source preservation.** `open` followed by `save` with no edits is byte-identical.
2. **Minimal diffs.** A small edit produces a small Git diff.
3. **Provider independence.** UI does not depend on GitHub API.
4. **Core independence.** Core does not depend on a specific repository provider.
5. **Unknown syntax is preserved.** Markdown constructs the editor does not understand are kept verbatim.
6. **Large-file support.** Files larger than RAM can be opened and edited.
7. **No blocking.** Network / Git operations never block typing.
8. **Plugin isolation.** A plugin crash does not crash the editor.
9. **Single design system.** All themes share the same token system.
10. **Raw Git diff is the source of truth for VCS.**

## System architecture

### Crate dependency graph

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
            |        v     v     v
            v       editor-text editor-markdown
            |          \    |    /
            |           \   |   /
            v            \  |  /
        editor-domain <----+  (all layers depend on the domain layer)
```

See [`architecture.md`](./architecture.md) for the full structural breakdown.

### Data flow

#### Opening a document

```
path -> DocumentStorage::open -> immutable original buffer (mmap or read)
      -> editor-text PieceTable (original-only, no edits)
      -> editor-markdown lossless parse -> SyntaxModel (AST with spans)
      -> editor-core DocumentBuffer (text + syntax + selection)
      -> editor-ui renders the visible viewport only (virtualized)
```

#### Editing

```
UI input event -> command dispatch -> semantic command (e.g. InsertText)
   -> resolves to Vec<TextEdit> (byte ranges against the PieceTable)
   -> EditTransaction { edits, selection_before, selection_after }
   -> UndoManager.push(transaction)
   -> PieceTable.apply(edits)         // O(log n) per edit, no full copy
   -> editor-markdown incremental reparse of the affected region
   -> SyntaxModel patch (spans outside the affected region are unchanged)
   -> editor-ui re-renders the changed viewport
```

#### Saving

```
DocumentBuffer -> serialize: stitch unchanged original segments + edited segments
              -> atomic write: temp file -> flush -> fsync -> rename over original
              -> invalidate Git diff cache
```

## Crate inventory

| Crate | Responsibility |
|-------|----------------|
| `editor-domain` | Domain primitives: `ByteOffset`, `Position`, `Selection`, `ByteRange`, `MarkdownProfile`, typed errors. No I/O and no external dependencies beyond `std`. |
| `editor-text` | Piece Table text buffer with immutable original buffer and append-only edit buffers. Provides byte / scalar / grapheme / line+column coordinates and a fast line index. |
| `editor-markdown` | Lossless CommonMark 0.31.2 + GFM parser. AST carries source spans and trivia (whitespace, markers, fence length, line endings). Serializer preserves unmodified nodes verbatim. |
| `editor-core` | `DocumentBuffer` facade, `TextEdit`, `EditTransaction`, `UndoManager`, `SelectionModel`, and command dispatch. |
| `editor-diff` | Myers line diff, hunk model, intraline token diff, semantic Markdown diff. |
| `editor-git` | `VersionControl` port plus a default implementation that shells out to the `git` CLI. |
| `editor-storage` | `DocumentStorage` port, byte-range loading, UTF-8 boundary correction, chunk parser context and atomic `temp + rename` save. |
| `editor-plugin-api` | Plugin traits, manifest schema, capabilities, extension points and a versioned SDK contract. |
| `editor-plugin-host` | Manifest loader, capability enforcement, WASM runtime skeleton and crash isolation. |
| `editor-platform` | OS-level ports: file watcher, secure storage, clipboard, URL opener, high-DPI info. |
| `editor-ui` | Tauri 2 shell with a Vue/Vite frontend. Sends commands to the Rust core and renders the virtualized block view and source view. |
| `adapters/github` | `RepositoryHostAdapter` for GitHub REST API. |
| `adapters/generic-git` | `RepositoryHostAdapter` for plain Git remotes (no provider-specific API). |

## Data flow details

### Coordinates

`editor-text` exposes five explicit coordinate systems and conversions between them:

| Coordinate | Meaning |
|------------|---------|
| `ByteOffset` | u8 index into UTF-8 bytes |
| `ScalarIndex` | Unicode scalar value index |
| `GraphemeIndex` | Extended grapheme cluster index |
| `LineColumn` | line (0-based) + column in scalars |
| `VisualColumn` | column after tab / wrap expansion (UI) |

The Piece Table's line index maps `Line -> ByteOffset` in O(log n); scalar and grapheme columns are computed on demand within a line.

### Document model

The `editor_markdown::ast::Document` stores, for every node, the exact source byte range plus trivia. The serializer walks the AST and emits the original `SourceSpan` bytes for unchanged nodes and regenerates only dirty nodes. Paragraphs and whitespace outside the edited region are never touched.

## Build, test and run

All commands are issued from the repository root unless otherwise noted.

### Build everything

```bash
cargo build --workspace
```

### Run Rust tests

```bash
cargo test --workspace
```

### Build the frontend

```bash
cd crates/editor-ui
npm run build
```

### Run frontend tests

```bash
cd crates/editor-ui
npx vitest run
```

### Launch the desktop app

```bash
cd crates/editor-ui
cargo tauri dev
```

## Dependency rules

The architecture enforces a strict one-way dependency direction:

- `editor-domain` has zero dependencies beyond `std`.
- `editor-text`, `editor-markdown`, `editor-diff`, `editor-git` and `editor-storage` depend only on `editor-domain` and external libraries.
- `editor-core` depends on `editor-domain`, `editor-text`, `editor-markdown` and `editor-diff`.
- `editor-plugin-api` depends only on `editor-domain`.
- `editor-plugin-host` depends on `editor-plugin-api` and `editor-core`.
- `editor-platform` and `editor-ui` depend on `editor-core` and ports.
- `adapters/*` depend on `editor-plugin-api` / `editor-git` ports, never on `editor-ui`.
- No crate depends on `editor-ui`. No crate depends on an adapter.
- GitHub never appears in `editor-domain`, `editor-core`, `editor-git` or `editor-markdown`.

## Threading and performance

The editor keeps the UI thread unblocked:

- **UI thread** — input dispatch, viewport render, command palette, panels.
- **Document worker** — Piece Table edits, incremental reparse, search.
- **Parser worker(s)** — heavy reparses and outline building for large files.
- **Git worker** — all `VersionControl` operations, async and cancellable.
- **Plugin workers** — one WASM instance per plugin, isolated.
- **Network worker** — HTTP for remote adapters such as GitHub.

All cross-thread communication is via channels and `CancellationToken`. The UI thread never blocks on I/O or compute that would exceed a single frame.

Large files use `mmap` for the original buffer and a virtualized, block-level render in the frontend. Only visible blocks are fetched and measured. Additional content is parsed in 10 MB chunks on demand.

## Plugin model

Plugins run in a WASM sandbox and implement traits from `editor-plugin-api` against a versioned SDK. A manifest declares capabilities (`filesystem`, `network`, `git_credentials`, `clipboard`, `process`) and the host enforces them. Extension points cover commands, toolbar/menus, shortcuts, Markdown syntax extensions, block/inline renderers, themes, exporters/importers, repository providers, asset handlers, sidebar panels and status bar items.

## Roadmap and known gaps

The current foundation is complete for the core document model and editing flow. Deliberately unfinished or simplified areas include:

- **Inline syntax highlighting** — currently block-level only.
- **WYSIWYG rendered editing** — the rendered view is click-to-edit per block.
- **Diff view** — diff data is produced but not rendered as a dedicated UI.
- **Settings UI** — settings are not yet exposed in the frontend.
- **File tree sidebar** — not yet implemented.
- **Multi-tab support** — single active document only.
- **WASM runtime** — plugin host has manifest/capability/crash isolation but the runtime is a skeleton.
- **Semantic diff regeneration** for HTML blocks, thematic breaks and code blocks still uses verbatim span emission.

## Document references

- [`architecture.md`](./architecture.md) — full architecture and crate design.
- [`adr/`](./adr/) — architecture decision records:
  - [ADR-001: UI framework](./adr/001-ui-framework.md)
  - [ADR-002: Document buffer](./adr/002-document-buffer.md)
  - [ADR-003: Markdown parser](./adr/003-markdown-parser.md)
  - [ADR-004: Git engine](./adr/004-git-engine.md)
  - [ADR-005: Plugin runtime](./adr/005-plugin-runtime.md)
  - [ADR-006: Large file model](./adr/006-large-file-model.md)
