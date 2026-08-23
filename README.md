# WoMD — Word-like Markdown Editor

> A source-preserving, Git-friendly Markdown document editor with a Rust core and a cross-platform Tauri 2 UI.

WoMD (Word-like Markdown Document editor) is a desktop Markdown editor where the canonical file is a `.md` file on disk. It is designed around a few non-negotiable invariants:

1. **Source preservation.** Opening a file and saving it without edits is byte-identical.
2. **Small diffs.** A small edit produces a small Git diff, not a massive reformatting.
3. **Markdown is the source of truth.** The editor never destroys Markdown syntax it does not explicitly support.
4. **Large-file friendly.** Files larger than RAM can be opened and edited without loading the whole document into the UI.
5. **Git is a first-class citizen.** Status, diff, stage, commit, branch, push, pull and file history are native operations.

## Features

- **Source / rendered / split views** — edit raw Markdown or click-to-edit blocks in a virtualized rendered view.
- **Lossless CommonMark 0.31.2 + GFM parser** — preserves the exact formatting, marker style, fence length, indentation and line endings of the original source.
- **Piece Table text buffer** — non-destructive edits with minimal byte-level diffs and full undo/redo.
- **Virtualized block rendering** — large documents are rendered as a sliding window of Markdown blocks with measured heights.
- **Find / Replace** — regex, case sensitivity, match counter and replacement, executed on the Rust core over IPC.
- **Git integration** — status, diff, stage/unstage, commit, branch, checkout, fetch, pull, push, hunk-level diff and file history.
- **Plugin system (skeleton)** — versioned plugin API and WASM host with capability enforcement.
- **Theming, command palette, menu bar and context menu** — dark-first, Catppuccin-inspired theme.

## Quick start

### Prerequisites

- [Rust](https://rustup.rs/) 1.95+
- [Node.js](https://nodejs.org/) 18+ and npm
- [Tauri 2 CLI](https://v2.tauri.app/): `cargo install tauri-cli --version "^2"`
- `cargo-tauri` on `PATH`

### Build

```bash
# Build the full Rust workspace
cargo build --workspace

# Build the production UI bundle
pushd crates/editor-ui && npm run build && popd
```

### Test

```bash
# Run all Rust tests
cargo test --workspace

# Run frontend unit tests
pushd crates/editor-ui && npx vitest run && popd
```

### Run the desktop app

```bash
pushd crates/editor-ui && cargo tauri dev && popd
```

## Architecture

The project is a Rust workspace with a one-way dependency graph:

```
editor-ui (Tauri 2 + Vue/Vite)
├── editor-core (DocumentBuffer, commands, undo)
│   ├── editor-text (Piece Table)
│   ├── editor-markdown (lossless parser/serializer)
│   └── editor-diff (diff + hunks)
├── editor-git (VersionControl port + Git CLI/CLI2 adapters)
├── editor-platform (file watcher, clipboard, secrets, high-DPI)
└── editor-plugin-host (WASM host)

adapters/
├── github      (GitHub REST API adapter)
└── generic-git (plain Git fallback adapter)
```

See [docs/architecture.md](docs/architecture.md) and [docs/adr/](docs/adr/) for the full design.

## Crate inventory

| Crate | Responsibility |
|-------|----------------|
| `editor-domain` | Core primitives (`ByteOffset`, `Selection`, `Position`, typed errors). No I/O. |
| `editor-text` | Piece Table text buffer, line index, coordinate conversions. |
| `editor-markdown` | Lossless CommonMark + GFM parser, AST with source spans, serializer. |
| `editor-core` | `DocumentBuffer` facade, `TextEdit`, `EditTransaction`, undo, selection. |
| `editor-diff` | Myers line diff, intraline diff, hunks, semantic Markdown diff. |
| `editor-git` | Git operations behind the `VersionControl` port. |
| `editor-storage` | `DocumentStorage` port, mmap, atomic save, partial chunk loading. |
| `editor-plugin-api` | Plugin traits, manifest, capabilities, SDK contract. |
| `editor-plugin-host` | Plugin loader, WASM runtime, crash isolation. |
| `editor-platform` | OS abstractions: clipboard, file watcher, secrets, URL opener. |
| `editor-ui` | Tauri 2 frontend shell. |
| `adapters/github` | GitHub `RepositoryHostAdapter`. |
| `adapters/generic-git` | Plain Git `RepositoryHostAdapter`. |

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
