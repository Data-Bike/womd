# ADR-005: Plugin runtime

## Context
Plugins are fundamental (§65–71). Core must not depend on plugins (Dependency Inversion).
Plugin ABI must not be tied to unstable Rust ABI (§66). Capability/permission model (§68).
Plugin failure must not crash the editor (Invariant 8). Extension points are broad (§69).

## Options
1. **Dynamic native plugins (`libloading` + Rust ABI).** Rejected as primary: Rust ABI is
   unstable across compiler versions; ABI compat burden is large; crash isolation is poor
   (a segfault in a plugin kills the process) — violates Invariant 8.
2. **WASM sandbox (`wasmtime`).** Stable ABI via a versioned SDK, strong isolation
   (crash/panic contained), capability-based host calls, cross-platform, mature.
3. **Stable IPC protocol (separate process per plugin).** Strong isolation, language-
   agnostic, but higher latency and operational complexity.

## Decision
**WASM sandbox via `wasmtime` as the primary plugin runtime**, with a **versioned manifest
+ versioned SDK** (§66–67). A stable IPC protocol is retained as a fallback for
out-of-process plugins in Phase 2 if needed.

- `editor-plugin-api`: stable trait definitions (`MarkdownExtension`, `Command`,
  `RepositoryHostAdapter`, `AssetStorageAdapter`, `Exporter`, `Importer`,
  `ThemeProvider`, …), manifest schema, capability enum, `EventBus` contract.
- `editor-plugin-host`: manifest loading, capability enforcement, WASM instantiation,
  host-call dispatch, crash isolation (panic in a plugin → deactivated + logged, editor
  continues — Invariant 8), API-version negotiation.
- Manifest (§67): `id`, `name`, `version`, `api_version`, `[permissions]`, `[contributes]`.
- Permissions (§68): `filesystem`, `network`, `git_credentials`, `clipboard`, `process` —
  default deny; host enforces every host call against the manifest.
- EventBus (§70): controlled subscribe/publish; plugins subscribe only to events their
  permissions allow.

## Consequences
- Invariant 8 satisfied by sandboxing; Invariant 3/4 preserved (plugins implement ports).
- MVP ships a **skeleton**: API traits, manifest, host that loads manifests and enforces
  capabilities, with WASM execution wired but no first-party plugins yet (§108).
- `wasmtime` adds binary size/weight; acceptable for the safety/isolation guarantee.
- Native plugins are explicitly NOT supported at MVP to avoid ABI instability (§66).
