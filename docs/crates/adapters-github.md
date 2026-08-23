# `adapters/github`

GitHub `RepositoryHostAdapter` for WoMD. This crate lives in `adapters/` and never depends on `editor-ui` or the core Git implementation.

## Responsibilities

- Implement the `RepositoryHostAdapter` port for GitHub: repository metadata, remote branches, pull requests and related operations.
- Use the `gh` CLI for GitHub API operations rather than embedding tokens or HTTP clients directly.
- Degrade provider-only operations (e.g. creating a PR) to `NotSupported` if `gh` is not installed.

## Key types

- `GitHubAdapter` — the adapter implementation.
- `RepositoryHostAdapter` — the port it implements from `editor-plugin-api`/`editor-git`.

## Design notes

By using `gh auth login`, authentication and credential storage are handled by the `gh` CLI and the OS keychain. Secrets never enter the editor process (§87). A future `reqwest`-backed adapter can replace this crate behind the same port without touching any callers. This demonstrates Invariant 4: the core does not depend on GitHub, and the GitHub adapter does not depend on the UI.
