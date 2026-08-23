# `editor-git`

Git operations behind a port, keeping the rest of the editor independent from any specific Git implementation.

## Responsibilities

- Define the `VersionControl` port used by the UI and core: status, diff, stage, unstage, commit, branches, checkout, fetch, pull, push, file history and more.
- Provide a default implementation that shells out to the local `git` CLI.
- Define the `CredentialProvider` port so credentials never enter the editor process.
- Keep Git running on a dedicated worker so it never blocks typing (Invariant 7).

## Key types

- `VersionControl` — the primary trait the UI depends on.
- `CredentialProvider` — resolves credentials for a remote.
- `GitCli` — the current default Git CLI-based implementation.

## Design notes

The default implementation is intentionally simple at the MVP stage. A `libgit2`-backed or other implementation can replace it behind the same port. The Git engine lives behind `editor-core` in the dependency graph; the UI and `adapters/*` only talk to `VersionControl` and `RepositoryHostAdapter` traits. This makes it possible to swap implementations without touching the frontend.
