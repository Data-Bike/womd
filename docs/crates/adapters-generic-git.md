# `adapters/generic-git`

Generic Git `RepositoryHostAdapter` for WoMD. This adapter works with any plain Git remote that does not have a provider-specific API.

## Responsibilities

- Implement `RepositoryHostAdapter` for a plain Git remote.
- Read repository metadata and remote branches from the local `.git` config using the `git` CLI.
- Degrade provider-API operations (pull requests, issues, etc.) to `NotSupported`.

## Key types

- `GenericGitAdapter` — the adapter implementation.
- `RepositoryHostAdapter` — the port it implements.

## Design notes

This is the fallback adapter. A GitLab, Bitbucket, Gitea or any other Git remote works through this adapter even if no provider-specific adapter exists (Invariant 4). It intentionally does not use any network API; it relies on the local Git repository configuration. Provider-aware features are left to dedicated adapters in `adapters/<provider>`.
