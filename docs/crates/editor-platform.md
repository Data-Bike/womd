# `editor-platform`

OS-level platform abstractions with no business logic. This crate wraps system services behind traits so the rest of the editor remains platform-agnostic and testable.

## Responsibilities

- `Clipboard` — get and set the system clipboard.
- `SecureStorage` — OS keychain where available; in-memory fallback for tests.
- `UrlOpener` — open a URL in the OS default browser/handler safely.
- `FileWatcher` — file change notifications (polling-based fallback).
- `HighDpiInfo` — screen scale factor for rendering.

## Key types

- `Clipboard`, `SecureStorage`, `UrlOpener`, `FileWatcher`, `HighDpiInfo` — one trait per service.
- Platform-specific implementations are feature-gated so the right backend is selected at compile time.

## Design notes

Implementations are intentionally simple at the MVP stage and may call external CLI tools (e.g. `clip`, `xclip`, `pbcopy` for clipboard). These can later be replaced with native libraries (`arboard`, `keyring`, `notify`) behind the same traits. No crate above `editor-platform` contains OS-specific code.
