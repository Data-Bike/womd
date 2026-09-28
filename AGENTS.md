# WoMD — project notes for coding agents

## Build / test
- `cargo build --workspace` — build everything (zero warnings expected).
- `cargo test --workspace` — run all tests (625 across crates).
- `cargo tauri dev` (from `crates/editor-ui/`) — launch the Tauri 2 UI shell in dev mode.
  Requires `cargo-tauri` (`cargo install tauri-cli --version "^2"`).
- `cargo run` — runs the `womd` binary which exercises the editor core and verifies
  the source-preservation invariants end-to-end.
- `npm test` (from `crates/editor-ui/`) — runs the Vitest suite for pure frontend
  logic (currently `src/lib/textEditing.js`, the shared block/source-view formatting
  helpers). `npm run build` still must pass too.
- `npm run test:e2e` (from `crates/editor-ui/`) — Playwright click-driven probe
  (`e2e/probe.mjs`) against the Vite dev server with a mocked Tauri backend
  (`e2e/mock-tauri.mjs`). Requires `vite dev` (or `cargo tauri dev`) already
  running on :5173. Screenshots land in `e2e/shots/` (gitignored).
- `cargo run -p editor-mcp --bin womd-mcp -- --root <dir>` — MCP stdio server
  exposing sandboxed document tools to AI agents (ADR-007, docs/mcp.md).
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
  intraline LCS diff, semantic Markdown diff. 31 tests.
- **editor-storage** (§51–57, ADR-006): `DocumentStorage` port, `InMemoryStorage`,
  `MmapStorage` (memmap2-backed, zero-copy reads, >RAM files), UTF-8 boundary correction,
  chunk parser context (fenced code / block quote / list / indented code detection),
  atomic save (temp+rename, preserves file permissions), streaming save
  (patch-based, source-preserving). 43 tests
  including a generated 10 MB large-file test (§95).
- **editor-git** (§31–50): `VersionControl` port + `GitCli` impl (shells out to `git`
  behind the port; a `git2`-backed impl can replace it without touching callers).
  status/diff/stage/unstage/commit/branches/checkout/fetch/pull/push, file history,
  read-file-at-revision. `SystemCredentialProvider` defers to git's own credential
  resolution (secrets never enter the editor process, §87). Hunk-level staging via
  `git apply --cached` (§42; the hunk→patch pipeline is byte-exact — non-UTF-8
  files and extended headers like `new file mode`/`rename from/to` survive).
  `resolve_tool_binary` (pub) resolves `git`/`gh` to absolute PATH entries.
  Hardened handles additionally scrub the inherited environment (GIT_DIR,
  GIT_CONFIG_*, GIT_EXTERNAL_DIFF, GIT_EXEC_PATH, GIT_SSH*, GIT_TRACE*,
  BASH_ENV, LD_PRELOAD …) and run diffs with --no-ext-diff.
  138 tests with temp repos (§93).
- **editor-markdown** (§3, ADR-003): Lossless CommonMark 0.31.2 + GFM parser, AST,
  serializer. Two-pass link reference resolution (§100). Nested block parsing (block
  quote & list children recursion). Dirty node regeneration for lists and block quotes.
  128 round-trip and structural tests covering paragraphs, headings, lists, code, tables,
  links, images, emphasis, HTML, hard breaks, blank lines, and edge cases.
- **editor-core** (§2, ADR-003): `DocumentBuffer` with piece table, incremental reparse
  (ADR-003 §58 — only affected blocks reparsed), undo/redo, selection tracking. 51 tests.
- **editor-plugin-host** (§65–71, ADR-005): TOML manifest parsing, API-version
  negotiation, capability enforcement (default deny), crash isolation (crashed plugins
  are deactivated, editor continues — Invariant 8). 19 tests. WASM runtime (`wasmtime`)
  is a skeleton (§108).
- **editor-platform** (§74, §86–88, §97): OS abstractions — `Clipboard` (in-memory +
  system via clip/pbcopy/xclip), `FileWatcher` (polling-based, §64), `SecureStorage`
  (in-memory + OS keychain trait), `UrlOpener` (safe-scheme validation, §86), `HighDpiInfo`.
  17 tests.
- **adapters/github** (§34): GitHub `RepositoryHostAdapter` via `gh` CLI — PRs, metadata,
  branches, auth status. `gh`/`git` resolve to absolute PATH entries (a planted
  `gh.exe`/`git.exe` in a hostile repo can't run via cwd search). 18 tests
  (JSON parsing helpers + subprocess drain/timeout).
- **adapters/generic-git** (§34): Plain Git `RepositoryHostAdapter` — metadata and
  branches from local repo, PRs degrade to `NotSupported`. `git` resolves to an
  absolute PATH entry. 6 tests.
- **editor-ui** (ADR-001): Tauri 2 shell with vanilla HTML/CSS/JS frontend. Exposes
  `editor-core`'s `DocumentBuffer` via 12 Tauri commands (open, new, get_text, replace,
  insert, undo, redo, save, get_syntax_tree, get_git_status, is_dirty, get_file_name).
  Dark theme, line numbers, block-level syntax highlighting, git status bar, keyboard
  shortcuts (Ctrl+N/O/S/Z/Y). `cargo tauri dev` from `crates/editor-ui/` to launch.
  External-change watcher (750 ms mtime poll on open tabs): a clean buffer is
  reloaded automatically on external writes (e.g. MCP), a dirty buffer keeps its
  edits and raises an `external-change` conflict banner naming the version commit
  (`Generated-By: womd-mcp` trailer marks MCP-authored commits). Saving a document
  inside a repository creates a version commit too (`commit_save`, path-scoped
  `--only` commit, `Generated-By: womd-ui`), so human saves and MCP edits share one
  linear version history. Read-only git commands (status/diff/log/history/show,
  tags/remotes/stash list, repo_root, `latest_commit_for`) open a HARDENED git
  handle (`open_git_for_active_hardened` / `GitCli::open_hardened`) so repository
  config (`diff.*.textconv`, `diff.external`, `core.fsmonitor`) cannot execute
  programs when the user merely inspects a repo; write and network ops keep the
  plain handle because they legitimately need credential helpers, SSH env, LFS
  filters and commit signing. 94 tests.
- **editor-mcp** (ADR-007, docs/mcp.md): MCP stdio server so AI agents can author and
  edit documents. Sandboxed to `--root` (system dirs, home root, Desktop, credential
  dirs, `.git` denied; `..`/absolute/symlink escapes rejected; no execution). 19
  versioned tools: CRUD, atomic multi-op `edit_document` (+`dry_run`), search,
  `markdown_outline`/`markdown_block_edit`/`markdown_toggle_task` on the lossless
  parser + `streaming_save`, read-only `git_status`/`git_diff`/`git_log`,
  `api_manifest`. **Every mutating call creates its own git commit with an
  agent-chosen `commit_message`** (`git commit --only` on `:(top,literal)` touched
  paths via `GitCli::commit_paths`; workspace auto-`git init`'d on first commit;
  `commit:false` opts out). Write policy (API 3.0.0): executable file types,
  credential files/dirs (`.env`, keys, `.ssh`/`.aws`/`.config`…), build/CI/app
  config (`build.rs`, `Makefile`, `package.json`, `.github/workflows`, editor/agent
  dirs) are denied; bulk traversal never follows symlinks OR junctions (descent is
  canonical-containment-checked — a MOUNT_POINT reparse can't escape the root);
  read-modify-write tools
  are compare-and-save (refuse to clobber externally-changed files, plus optional
  `expected_content` CAS on `write_document`, API 3.1.0); git queries
  are scoped to the workspace subtree; the git binary resolves to an absolute PATH
  entry (a planted `git.exe` can't run); all git calls run hardened — no repo
  hooks, fsmonitor, or external-diff programs can execute. `MCP_API_VERSION` semver
  (currently 3.2.0); contract served via `tools/list` metadata, `api_manifest`,
  `womd://manifest`. Junctions pointing INSIDE the root are deduplicated by
  canonical target — a `a/self -> a` junction cannot flood traversal or listing
  with cycles. 68 tests.
  **Sync rule: when editor capabilities change, update the MCP surface in the same
  commit — checklist in docs/mcp.md §Change protocol.**

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
- `editor-ui` uses granular `replace_text` for block-level edits (commitEdit,
  applyInlineFormat, applyLineFormat, applyHeading, applyLink, thematic break
  and code block insertion). Full-text replace is only used for initial load,
  file dialog import, and fallbacks when block source can't be found.
- `editor-ui` uses `editor_storage::MmapStorage` for file reads (zero-copy,
  >RAM file support via mmap, Invariant 6) and `editor_storage::atomic_save`
  for file saves (temp + rename, §55).
- **Multi-window (tear-off) caveat**: `AppState.active` is a single global index
  shared by all webview windows — a `switch_tab` in one window retargets
  `replace_text`/`save_document` (they act on the *active* tab) in the other.
  Correct fix: commands should resolve the tab through the invoking
  `WebviewWindow`'s own active map. Until then, avoid editing in two windows
  simultaneously — a save could target the wrong tab's buffer.
