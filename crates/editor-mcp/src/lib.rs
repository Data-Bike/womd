//! MCP (Model Context Protocol) server for WoMD.
//!
//! Exposes a versioned, sandboxed tool surface so AI agents can author and edit
//! documents inside a designated root folder — without the ability to launch
//! programs or touch anything outside the root (ADR-007).
//!
//! * Transport: newline-delimited JSON-RPC 2.0 over stdio (`server::run_stdio`).
//! * Confinement: `sandbox::Workspace` canonicalizes every path and rejects OS
//!   system directories, the filesystem root, the home directory itself, and
//!   other sensitive locations — while keeping every nested folder inside an
//!   approved root fully accessible.
//! * Versioning: `MCP_API_VERSION` (semver). Every tool/resource/prompt carries a
//!   `since` version; the machine-readable contract is emitted by `manifest` and
//!   served at `womd://manifest`. See `docs/mcp.md` for the change protocol.

#![forbid(unsafe_code)]

pub mod manifest;
pub mod sandbox;
pub mod server;
pub mod tools;

/// Semantic version of the WoMD MCP tool/resource/prompt contract.
///
/// Bump rules (docs/mcp.md §Versioning):
/// * MAJOR — a tool was removed, renamed, or its schema/semantics changed
///   incompatibly.
/// * MINOR — a new tool/resource/prompt was added, or a tool gained an
///   *optional* parameter (backwards compatible).
/// * PATCH — description/implementation fixes with no contract change.
///
/// History: 1.0.0 initial contract; 2.0.0 mutating tools create a per-change
/// git commit (`commit_message` required unless `commit:false`); 3.0.0
/// hardened write policy — executable file types, credential files/dirs,
/// build/app/CI configuration files and editor/agent config dirs are denied
/// (writes that previously succeeded now fail), plus read-modify-write tools
/// refuse to clobber externally-changed files (CAS) and bulk traversal never
/// follows symlinks; 3.1.0 — `write_document` gained the optional
/// `expected_content` CAS precondition, and git calls run hardened
/// (no repository hooks, fsmonitor or external diff commands); 3.2.0 —
/// `resources/templates/list` now advertises the `womd://file/{path}` URI
/// template (was an empty list) and the file resource serves raw bytes
/// instead of the line-numbered tool view.
pub const MCP_API_VERSION: &str = "3.2.0";

/// MCP protocol revision this server speaks.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Older protocol revisions accepted during `initialize` negotiation.
pub const MCP_PROTOCOL_COMPAT: &[&str] = &["2024-11-05", "2025-03-26"];

/// Server identity reported in `initialize` results.
pub const SERVER_NAME: &str = "womd-mcp";
