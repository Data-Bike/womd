# WoMD MCP server (`editor-mcp`)

MCP (Model Context Protocol) surface that lets AI agents **author and edit documents**
in a designated folder while a human reviews and refines the result in the WoMD editor.
It is deliberately powerful for documents and deliberately incapable of everything
else: **no program execution, no network, no access outside the workspace root.**

## Running

```bash
cargo run -p editor-mcp --bin womd-mcp -- --root path\to\workspace
# or: WOMD_MCP_ROOT=path\to\workspace womd-mcp
womd-mcp --manifest   # dump the versioned contract (JSON)
```

Transport: newline-delimited JSON-RPC 2.0 on stdio (one message per line; stdout is
protocol-only, logs go to stderr). Example client config (Claude Desktop style):

```json
{
  "mcpServers": {
    "womd": {
      "command": "womd-mcp",
      "args": ["--root", "C:\\Users\\me\\Documents\\project-notes"]
    }
  }
}
```

## Security policy

**Root selection** (`Workspace::new`) — the root must be an existing directory and is
rejected if it is:

- a filesystem/drive root (`/`, `C:\`, …), a mounted-volume root, or an *ancestor* of
  the home directory (`C:\Users`, `/home`, `/Users` — would expose the whole profile);
- the home directory itself or a top-level profile folder (`Desktop`, `Documents`,
  `Downloads`, `Pictures`, `Music`, `Videos` — pick a *subfolder* instead);
- inside a denied system subtree — Windows: `%SystemRoot%`, `Program Files*`,
  `ProgramData`, `$Recycle.Bin`, `~\AppData`, `C:\Users\Public`; Unix: `/etc`, `/usr`,
  `/bin`, `/sbin`, `/lib*`, `/boot`, `/proc`, `/sys`, `/dev`, `/run`, `/var`, `/opt`,
  `/snap`, `/root`, `/srv`; macOS additionally `/System`, `/Library`, `/private`,
  `/Applications`, `~/Library`;
- inside credential/config dirs: `~/.ssh`, `~/.aws`, `~/.azure`, `~/.gnupg`,
  `~/.kube`, `~/.docker`, `~/.config`, `~/.local`, `~/.cache`, `~/.cargo`, `~/.npm`.

Exceptions (allowed despite sitting under a denied subtree): OS temp space (`/tmp`,
`/var/tmp`, `/var/folders`, `%TEMP%`/`%TMP%`/`TMPDIR`).

**Per-request confinement** — every tool path is relative to the root. Absolute paths,
`..` escapes, symlink escapes (including through newly-created paths' ancestors) and
dangling symlinks are rejected. `.git` internals and `*.womd-tmp-*` temp files are
invisible to all tools. The root itself cannot be deleted or moved. Checks are
applied case-insensitively *and* re-run on the canonicalized result, so NTFS 8.3
aliases (`GIT~1` → `.git`), odd casing, trailing dots/spaces and ADS separators
cannot smuggle a denied name through.

**Name hygiene** — every path component is validated: no NTFS-illegal characters
(`<>:"|?*`, control chars), no Windows device names (`CON`, `PRN`, `AUX`, `NUL`,
`COM1-9`, `LPT1-9` — checked on the stem, so `NUL.txt` is denied too), no trailing
dots/spaces. This also prevents pathspec-magic file names (`:(glob)*`) from ever
reaching git.

**No side effects beyond files** — there is no shell/exec tool. Writes go through
`editor_storage::atomic_save` / `streaming_save` (temp-file + rename), so machine
edits keep the same byte-preservation guarantees as the UI (Invariants 1–2).

**Executables cannot be planted** (API 3.0.0) — mutating tools refuse file types the
OS treats as programs: `exe dll com scr pif msi msp mst bat cmd ps1 psd1 psm1 vbs
vbe js jse mjs cjs wsf wsh hta jar gadget msc cpl ocx sys drv reg lnk url inf ins
isp sct shb app action workflow command dmg pkg deb rpm apk ipa run`. The same
"no runnable code" policy covers interpreter scripts — `sh bash zsh fish ksh py
pyw pyc pyo rb pl pm php lua tcl scpt applescript osascript` — since `sh x.sh`
or `python x.py` executes just like a `.bat`. Scripts remain *readable*. Because
no executable may exist in the workspace, a planted `git.exe` can't hijack
commits — and as defence-in-depth `editor-git` resolves the real git to an
absolute PATH entry, never via the working directory.

**App/build/CI configuration is read-only** (API 3.0.0) — writes are denied to:
build scripts (`build.rs`, `Makefile`, `CMakeLists.txt`, `setup.py`, `build.gradle`),
package manifests with install hooks (`package.json`, `Cargo.toml`, `pyproject.toml`,
`Gemfile`, `Pipfile`), CI/CD definitions (`.github/workflows/**`, `.gitlab-ci.yml`,
`.travis.yml`, `appveyor.yml`, `azure-pipelines.yml`, `Jenkinsfile`, `bitrise.yml`,
`circle.yml`, `.drone.yml`, `codefresh.yml`), container specs (`Dockerfile`,
`docker-compose.*`, `compose.*`, `Vagrantfile`), OS folder metadata (`desktop.ini`,
`Thumbs.db`, `.DS_Store`, `autorun.inf`), git-hook runners (`.husky/**`,
`lefthook.yml`, `.pre-commit-config.yaml`, `.huskyrc*`), devcontainer specs
(`.devcontainer/**` — `postCreateCommand` executes), `.gitignore` (controls what
`git` sees — writing it could hide planted files from commits), and editor/agent
config dirs (`.vscode`, `.idea`, `.vs`, `.fleet`, `.zed`, `.cursor`, `.claude`,
`.devin`, `.agents`, `.windsurf`, `.codeium`, `.aider`, `.continue`). Reads stay
allowed for context.

**Secrets are invisible** (API 3.0.0) — neither read nor write: `.env` and `.env.*`
(except `.env.example` / `.env.sample` / `.env.template` / `.env.dist`), private
keys (`id_rsa`/`id_dsa`/`id_ecdsa`/`id_ed25519*`, `*.pem`, `*.key`, `*.pfx`, `*.p12`,
`*.keystore`, `*.jks`, `*.kdbx`, `*.kdb`, `*.ppk`, `*.ovpn`), token stores (`.netrc`,
`.npmrc`, `.yarnrc`, `.yarnrc.yml`, `.pypirc`, `.git-credentials`, `.pgpass`,
`.my.cnf`, `*.kubeconfig`, `.htpasswd`, `.htdigest`), `.envrc` (direnv — executes
on `cd`), IaC secret carriers (`*.tfstate`, `*.tfvars`, `*.tfvars.json`, `*.tfplan`)
and credential dirs anywhere in the tree (`.ssh`, `.aws`, `.azure`, `.gnupg`,
`.kube`, `.docker`, `.config`, `.gcloud`, `.secrets`, `.credentials`, `.cargo`).
VCS/tooling internals other than `.git` are likewise invisible: `.svn`, `.hg`,
`.bzr`, `.terraform`. Denied names never appear in listings or search results.

**Traversal never follows symlinks** (API 3.0.0) — `list_directory`, `find_files`
and `search_documents` use non-following metadata: a symlinked directory inside the
workspace cannot leak outside filenames or content. Symlinks show in listings as
`type:"symlink"`. Direct `resolve` calls still canonicalize (inside→inside links
work, escapes fail).

**Git queries are scoped** (API 3.0.0) — when the workspace sits inside a larger
repository, `git_status` entries outside the workspace subtree are filtered out and
`git_log`/`git_diff` are limited to it. The agent cannot enumerate files outside
its sandbox. Commit paths are committed via `:(top,literal)` pathspecs so a file
whose name collides with pathspec magic cannot widen a commit.

**Repository config cannot execute code** — all MCP git calls run through a
hardened handle: `core.hooksPath` points at an empty dir (no pre/post-commit
hooks, ever), `core.fsmonitor` is off (a configured fsmonitor command is an
external program), and `commit.gpgsign`/`tag.gpgsign` are forced off (a
missing `gpg.program` would otherwise make every snapshot commit spawn a
binary). On top of the static overrides, the merged config
(system+global+repo+worktree) is enumerated at open time and every key that
names an external program or credential channel is cleared per-exec:
`filter.*.clean/.smudge/.command/.process` (executed by `git add`),
`diff.*.command/.textconv/.cachetextconv` (executed by `git diff` on
attr-matched files), `diff.external`, `credential.*`,
`core.sshCommand`/`gitProxy`, `sendmail.*`, `gpg.*`, `ssh.*`,
`url.*.insteadOf`, `include[If].*` and `alias.*`. Clearing sets an empty value
(`-c key=`), which fails closed — git cannot spawn the empty command — and
the server's own diff commands additionally pass `--no-ext-diff`, so diffs
stay correct even when `diff.external` is configured. Hardened exec also
scrubs the inherited environment: `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`
(repo redirection), `GIT_CONFIG_*`/`GIT_CONFIG_PARAMETERS`/`XDG_CONFIG_HOME`
(config injection), `GIT_EXTERNAL_DIFF`/`GIT_DIFF_OPTS`/`GIT_EXEC_PATH`/
`GIT_SSH*`/`GIT_ASKPASS`/`GIT_PROXY_COMMAND`/`GIT_EDITOR`/`GIT_PAGER`
(program channels), `GIT_TRACE*` (arbitrary file writes), `BASH_ENV`/
`LD_PRELOAD`-family (shell/loader injection) and pathspec reinterpretation
variables are removed before spawn. Version-snapshot commits via
`commit_paths` — including UI saves — always bypass hooks; only an interactive
`git commit` through the VersionControl port runs them. The git binary itself
is resolved to an absolute, canonicalized PATH entry, so neither a workspace
file nor a relative PATH entry can substitute a fake `git`.

**Compare-and-save** (API 3.0.0) — read-modify-write tools (`edit_document`,
`append_document`, `markdown_block_edit`, `markdown_toggle_task`) re-verify the
file's bytes right before saving; if a human save or another writer landed in
between, the call fails with "file changed on disk — re-read and retry" instead of
silently clobbering the newer version.

**Limits** — 64 MiB per text file / write payload, 72 MiB per JSON-RPC frame,
traversal caps (50k walked entries, 20k files searched, 5k list entries) with
`truncated` flags, glob matching is O(p·n) iterative (no ReDoS via `*a*a*b` patterns),
regex search uses the linear-time `regex` crate.

## Per-change git versioning

**Every mutating tool call produces its own git commit, named by the agent.**
Mutating tools — `create_document`, `write_document`, `append_document`,
`edit_document`, `delete_document`, `move_document`, `create_directory`,
`markdown_block_edit`, `markdown_toggle_task` — accept:

- `commit_message` (string, **required** unless `commit:false`) — the agent writes a
  short descriptive name for the change (max 16 KiB, no control characters; it lands
  in `argv` where the Windows command line caps at ~32k chars); a
  `Generated-By: womd-mcp/<api>` trailer is appended automatically;
- `commit` (boolean, default `true`) — `false` opts out for scratch work.

Semantics:

- The commit contains *only* the paths the call touched (`git add -A -- <paths>` +
  `git commit --only -- <paths>`) — unrelated staged work elsewhere in the repo is
  never swept in.
- If the workspace is not inside a repository, it is `git init`'d at the root on the
  first commit (`repo_initialized: true` in the response); a local commit identity
  (`WoMD MCP <womd-mcp@localhost>`) is seeded only when none is configured.
- The response carries `commit: {committed, sha?, message, repo_initialized?}` —
  failures are reported there (the file change still applied), not as tool errors.
- `edit_document` with `dry_run:true` never commits (nothing is written).
- Git queries (`git_status`, `git_diff`, `git_log`) remain read-only; commit creation
  is part of the file-mutating tools, not a separate git-write surface.

## Co-editing with the human (editor-ui sync)

The desktop app watches open files (750 ms mtime poll) so agent writes surface
promptly:

- **Clean buffer** → reloaded automatically; a toast reports the version commit
  the content now reflects.
- **Dirty buffer** → the user's unsaved edits are never overwritten. A conflict
  banner names the version commit (sha + your `commit_message`) and offers:
  *Load saved version* (discard local edits), *Keep my edits* (save later —
  their save commits on top of yours), *View changes* (`git show` of the version).
- The user's own saves inside a repository create commits too
  (`editor: save <name>`, `Generated-By: womd-ui`), so human and agent edits
  share one linear `git_log`.

Practical consequence for agents: **write in small, well-named commits** — the
commit message is what the human reads in the conflict banner, and per-change
commits keep "Load saved version" granular. If you must rewrite heavily while
the document might be open for editing, prefer several `edit_document` ops over
one `write_document` so each step is reviewable and reloadable independently.
MCP writes go through atomic save (temp + rename), so an open mmap'd buffer in
the editor never blocks your write (and Windows in-place writes to a mapped file
would fail — use the tools, they already do the right thing).

## Interaction strategy (for agents)

Also served to clients as the `womd_interaction_guide` prompt, the
`womd://docs/interaction-guide` resource, and the `initialize` instructions.

1. `list_directory` (depth 2–3) → `read_document` / `markdown_outline` to orient.
2. Edit with the smallest tool: `edit_document` ops or `markdown_block_edit`, not
   `write_document` — minimal diffs are a project invariant.
3. `edit_document` with `dry_run:true` before committing risky rewrites.
4. `search_documents` / `find_files` for cross-file work.
5. `git_status` / `git_diff` / `git_log` for context (read-only by design).
6. Name every change: pass `commit_message` on each mutating call — it becomes a
   separate git commit. `commit:false` for scratch work.
7. Unsure about capabilities → `api_manifest` returns the live contract.

## Tool catalog (API 3.0.0)

| Tool | Purpose |
|---|---|
| `list_directory` | Tree listing with depth/hidden control |
| `read_document` | UTF-8 read, 1-based line numbering, paged |
| `document_info` | size, lines, mtime, text/binary detection |
| `create_document` | Create (parents auto-made, `overwrite` opt-in) → commit |
| `write_document` | Full-content replace (atomic, optional `expected_content` CAS) → commit |
| `append_document` | Append via patch save (zero rewrite of head) → commit |
| `edit_document` | Atomic multi-op edit: `replace`, `replace_lines`, `insert_lines`, `delete_lines`, `replace_bytes`; `dry_run` preview → commit |
| `delete_document` | File/dir delete (`recursive` for non-empty dirs) → commit |
| `move_document` | Rename/move within workspace → commit |
| `create_directory` | mkdir -p → commit (empty dirs untracked by git) |
| `search_documents` | Literal or regex content search, glob filter, context lines |
| `find_files` | Filename glob (`*`, `?`) |
| `markdown_outline` | Block index/kind/spans/line ranges/heading text |
| `markdown_block_edit` | `replace_block`, `replace_block_range`, `delete_block`, `insert_block` → commit |
| `markdown_toggle_task` | Flip `[ ]`/`[x]` on the task item at a line → commit |
| `git_status`, `git_diff`, `git_log` | Read-only repository introspection |
| `api_manifest` | This contract, machine-readable |

Resources: `womd://manifest`, `womd://tree`, `womd://file/<relpath>`,
`womd://docs/interaction-guide`. Prompts: `womd_interaction_guide`,
`womd_editing_playbook`.

## API versioning

`MCP_API_VERSION` (currently **3.2.0**) is semver over the tool/resource/prompt
contract — *independent* of the MCP protocol revision (negotiated in `initialize`;
server speaks `2025-06-18`, accepts `2024-11-05`/`2025-03-26`).

- **MAJOR** — tool removed/renamed, required parameter added/changed, or response
  shape breaks compat.
- **MINOR** — tool/resource/prompt added; optional parameter added; new `op`/`edit`
  type added to an existing tool (agents must tolerate unknown enum values anyway).
- **PATCH** — docs, descriptions, bug fixes with no contract delta.

Deprecation: a tool is marked `deprecated`/`deprecated_in` in the manifest and keeps
working for one MAJOR window before removal. Schema/semantic revisions are marked
`revised_in`. Discovery: `initialize` returns `womd.apiVersion`; `tools/list`
annotates each entry with `womd.since`; `api_manifest`/`womd://manifest` return the
full contract.

Changelog: **3.2.0** — `resources/templates/list` advertises the
`womd://file/{path}` URI template (was empty); the file resource serves raw
file bytes instead of `read_document`'s line-numbered, line-capped view.
**3.1.0** — `write_document` gained optional `expected_content`
CAS precondition; all git calls run hardened (no repository hooks, fsmonitor
or external-diff programs); `delete_document` removes symlinks without
following them. **3.0.0** — hardened write policy (executable types, credential files,
build/app/CI config and editor/agent config dirs are denied — writes that
previously succeeded now fail); compare-and-save on read-modify-write tools;
bulk traversal no longer follows symlinks; git queries scoped to the workspace
subtree; payload/traversal limits documented. **2.0.0** — mutating tools create
per-change git commits (`commit_message` required unless `commit:false`); auto
`git init` of the workspace; `revised_in` field added. **1.0.0** — initial
contract.

## Change protocol — keeping MCP in sync with the editor

**When editor capabilities change, the MCP surface must be reviewed in the same
commit.** Checklist for agents modifying the system:

1. New capability that should be agent-accessible → add a `ToolSpec` in
   `manifest.rs` (set `since` to the next MINOR), implement in `tools.rs`, bump
   `MCP_API_VERSION`.
2. Behaviour change of an existing tool → compatible? bump MINOR/PATCH; breaking?
   deprecate the old name, add the new one, bump MAJOR on removal.
3. A capability that must *not* be agent-accessible (execution, credentials,
   network) → document the exclusion here; never expose it.
4. Update the catalog table, `interaction_guide`/`editing_playbook` text in
   `server.rs` when workflow semantics change, and the "Implemented subsystems"
   entry in AGENTS.md.
5. Tests must hold: `manifest_lists_all_dispatched_tools` (spec↔dispatch parity)
   plus a behaviour test per new tool.
