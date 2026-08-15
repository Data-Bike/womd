# WoMD — project notes for coding agents

## Build / test
- `cargo build --workspace` — build everything (zero warnings expected).
- `cargo test --workspace` — run all tests (154 at current stage).
- `cargo tauri dev` (from `crates/editor-ui/`) — launch the Tauri 2 UI shell in dev mode.
  Requires `cargo-tauri` (`cargo install tauri-cli --version "^2"`).
- `cargo run` — runs the `womd` binary which exercises the editor core and verifies
  the source-preservation invariants end-to-end.
- Workspace uses `resolver = "3"`, edition 2024, Rust 1.95+.
- `unsafe_code = "deny"` across the workspace (§74, §85). Exception: `editor-storage`
  uses `warn` + crate-root `deny` + `allow` only in `src/mmap.rs` (mmap requires unsafe,
  ADR-006).

## Architecture (see docs/architecture.md and docs/adr/)
- Dependency direction is one-way: `editor-domain` <- text/markdown/diff/git/storage <-
  core <- plugin-host/platform <- ui. Adapters depend on ports only.
- **No crate may depend on `editor-ui` or on an adapter.** GitHub code lives only in
  `adapters/github` (Invariant 3, 4).
- `editor-domain` has zero dependencies (only `std`).

## Source preservation (the core invariant)
- `editor-text::PieceTable` keeps the original bytes immutable; edits append to an edit log
  and rewrite the piece list. `to_bytes()` stitches original segments for unchanged
  regions → byte-identical round-trip (Invariant 1) and minimal diff (Invariant 2).
- `editor-markdown` parser partitions `[0, len)` into contiguous block spans; the
  serializer emits `source[span]` verbatim for unchanged nodes, regenerates only `dirty`
  nodes. Round-trip tests in `editor-markdown` cover paragraphs, ATX/setext headings,
  thematic breaks, block quotes, lists (ordered/unordered/task, start values), fenced &
  indented code, emphasis/strong/code spans/links/images/autolinks/strikethrough, tables,
  link reference definitions, blank-line trivia.
- Editing flow: `DocumentBuffer::apply(EditTransaction)` -> `PieceTable` edit -> reparse.
  `replace_text_run` and `toggle_task_item` produce minimal byte-level diffs.

## Implemented subsystems
- **editor-diff** (§37–44): Myers O(ND) line diff, hunks with context, token-level
  intraline LCS diff, semantic Markdown diff. 21 tests.
- **editor-storage** (§51–57, ADR-006): `DocumentStorage` port, `InMemoryStorage`,
  `MmapStorage` (memmap2-backed, zero-copy reads, >RAM files), UTF-8 boundary correction,
  chunk parser context (fenced code / block quote / list / indented code detection),
  atomic save (temp+rename), streaming save (patch-based, source-preserving). 15 tests
  including a generated 10 MB large-file test (§95).
- **editor-git** (§31–50): `VersionControl` port + `GitCli` impl (shells out to `git`
  behind the port; a `git2`-backed impl can replace it without touching callers).
  status/diff/stage/unstage/commit/branches/checkout/fetch/pull/push, file history,
  read-file-at-revision. `SystemCredentialProvider` defers to git's own credential
  resolution (secrets never enter the editor process, §87). Hunk-level staging via
  `git apply --cached` (§42). 9 tests with temp repos (§93).
- **editor-markdown** (§3, ADR-003): Lossless CommonMark 0.31.2 + GFM parser, AST,
  serializer. Two-pass link reference resolution (§100). Nested block parsing (block
  quote & list children recursion). Dirty node regeneration for lists and block quotes.
  65 round-trip and structural tests covering paragraphs, headings, lists, code, tables,
  links, images, emphasis, HTML, hard breaks, blank lines, and edge cases.
- **editor-core** (§2, ADR-003): `DocumentBuffer` with piece table, incremental reparse
  (ADR-003 §58 — only affected blocks reparsed), undo/redo, selection tracking. 7 tests.
- **editor-plugin-host** (§65–71, ADR-005): TOML manifest parsing, API-version
  negotiation, capability enforcement (default deny), crash isolation (crashed plugins
  are deactivated, editor continues — Invariant 8). 11 tests. WASM runtime (`wasmtime`)
  is a skeleton (§108).
- **editor-platform** (§74, §86–88, §97): OS abstractions — `Clipboard` (in-memory +
  system via clip/pbcopy/xclip), `FileWatcher` (polling-based, §64), `SecureStorage`
  (in-memory + OS keychain trait), `UrlOpener` (safe-scheme validation, §86), `HighDpiInfo`.
  14 tests.
- **adapters/github** (§34): GitHub `RepositoryHostAdapter` via `gh` CLI — PRs, metadata,
  branches, auth status. 5 tests (JSON parsing helpers).
- **adapters/generic-git** (§34): Plain Git `RepositoryHostAdapter` — metadata and
  branches from local repo, PRs degrade to `NotSupported`. 3 tests.
- **editor-ui** (ADR-001): Tauri 2 shell with vanilla HTML/CSS/JS frontend. Exposes
  `editor-core`'s `DocumentBuffer` via 12 Tauri commands (open, new, get_text, replace,
  insert, undo, redo, save, get_syntax_tree, get_git_status, is_dirty, get_file_name).
  Dark theme, line numbers, block-level syntax highlighting, git status bar, keyboard
  shortcuts (Ctrl+N/O/S/Z/Y). `cargo tauri dev` from `crates/editor-ui/` to launch.

## Lint policy
- `missing_docs` is `allow` at workspace level during foundation stage (would be ~350
  noise warnings on internal `pub` helpers). Re-enable per-crate as each stabilizes.
- `clippy::all` = warn, `clippy::pedantic` = allow.

## Known follow-ups (by design, not bugs)
- `editor-ui` Tauri 2 shell is implemented and runs, but the frontend is a foundation
  (vanilla HTML/CSS/JS, no framework). Follow-ups: inline syntax highlighting (currently
  block-level only), WYSIWYG rendering mode, diff view, plugin panel, settings UI,
  file tree sidebar, multi-tab support. A JS framework (React/Svelte/Solid) can be
  added later via `npm` (requires `Set-ExecutionPolicy RemoteSigned -Scope CurrentUser`
  for PowerShell).
- `editor-plugin-host` WASM runtime (`wasmtime`) — skeleton only; manifest loading,
  capability enforcement, and crash isolation are implemented and tested.
- Line-level staging (precise line range within a hunk) — currently stages the whole
  hunk; line-precise staging is a UI-layer follow-up.
- Incremental reparse uses `Refs::default()` for the reparsed window (reference links in
  the window won't resolve against definitions outside it). The full reparse path (initial
  parse, undo/redo) does two-pass resolution. Acceptable for the foundation.
- Dirty regeneration for code blocks, tables, HTML blocks, and thematic breaks still
  uses verbatim span emission. Edits to those node types go through the PieceTable
  byte-level path (already minimal-diff).
- `editor-platform` system clipboard/watcher/secret implementations are CLI-based; a
  native `notify`/`keyring`/`arboard`-backed impl can replace them behind the traits.
- `editor-ui` sends full-text replace on each edit (debounced 300ms). Granular
  range-based edits via `replace_text` command are available but not yet wired to
  fine-grained DOM diffing in the frontend.
- `editor-ui` uses `GitCli` directly (concrete impl) instead of the `VersionControl`
  /`GitExtended` traits. 9 `exec_text()` calls bypass the port. Refactoring to trait
  methods requires adding diff-vs-commit, file-history, rev-parse, and branch-list
  operations to the port interface.
- `editor-ui` uses `std::fs` directly for file I/O instead of the `editor-storage`
  `DocumentStorage` port. Integrating `MmapStorage` would enable mmap-backed large
  file support and atomic saves as designed in §51-57.
- `editor-ui` depends on `editor-platform` but doesn't use its traits (`Clipboard`,
  `FileWatcher`, `SecureStorage`, `UrlOpener`). Either wire the platform abstractions
  or remove the dependency.
