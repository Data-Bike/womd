# ADR-004: Git engine

## Context
Git is a first-class component (§31–50). UI must not know which Git implementation is used
(§32, Invariant 3). Must support status/diff/stage/unstage/commit/branches/checkout/
fetch/pull/push/file history, async + cancellable + non-blocking (§50, Invariant 7),
multiple auth methods (§35), and provider adapters (§33–34). Generic Git repos must work
without a provider API (§34, Invariant 4).

## Options
1. **Shell out to `git` CLI.** Simple, complete, but parsing porcelain output is fragile,
   cross-platform path/encoding issues, hard to cancel, security surface from spawning
   processes.
2. **`git2` (libgit2 bindings).** Mature, cross-platform, in-process, supports all needed
   operations, cancellable via thread + flag. Good auth support incl. SSH agent/keys.
3. **`gitoxide` (pure Rust).** Modern, pure Rust, async-friendly; but some advanced
   operations (merge, certain transports) less mature than libgit2 at MVP time.

## Decision
**`git2` (libgit2) as the default `VersionControl` implementation**, behind the
`VersionControl` port in `editor-git`. `gitoxide` is noted as a future replacement; the
port isolates the choice.

- `trait VersionControl` (§32) in `editor-git` (or `editor-domain` for the trait, impl in
  `editor-git`) — UI depends only on the trait.
- `trait CredentialProvider` (§35): impls for SSH key, SSH agent, HTTPS token, OAuth,
  system credential manager (via `editor-platform`), env provider. Secrets never plaintext
  (§87); use OS secure storage where available.
- `trait RepositoryHostAdapter` (§33) in `editor-plugin-api` (or a dedicated port crate):
  `adapters/github` (REST), `adapters/generic-git` (pure Git, no API). GitLab/Bitbucket/
  Gitea/Forgejo/AzureDevOps added later with zero core changes.
- All operations run on a dedicated Git worker, async + `CancellationToken`, with
  structured errors (`GitError`) and progress (§50, §89).
- Diff cache (§84) keyed by `(repo state, HEAD, index, working-tree metadata, file id)`.

## Consequences
- Invariants 3, 4, 7 are satisfied by the port + worker isolation.
- `git2` adds a native dependency (libgit2); acceptable, widely packaged.
- GitHub-specific code lives only in `adapters/github`; never in domain/core/git-core.
- A plain GitLab repo works via `VersionControl` even without a GitLab adapter (§34).
