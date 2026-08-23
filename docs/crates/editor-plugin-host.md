# `editor-plugin-host`

Plugin host: manifest loading, capability enforcement, WASM runtime skeleton and crash isolation.

## Responsibilities

- Load and validate plugin manifests written in TOML.
- Negotiate the API version between the host and each plugin.
- Enforce declared capabilities (default deny).
- Manage plugin lifecycle (load, activate, deactivate, unload).
- Isolate plugin crashes so a failed plugin is deactivated rather than crashing the editor (Invariant 8).

## Key types

- `PluginHost` — the main host controller.
- `ManifestLoader` and `CapabilityEnforcer` — manifest parsing and permission checks.
- `Runtime` — the WASM runtime abstraction (currently a skeleton around `wasmtime`).

## Design notes

The WASM runtime (`wasmtime`) is intentionally not fully wired at the MVP stage. The surrounding machinery — TOML parsing, API-version negotiation, capability enforcement and lifecycle management — is in place so that the runtime can be connected later without reworking the host. This satisfies Invariant 8: the editor continues to function even if a plugin fails.
