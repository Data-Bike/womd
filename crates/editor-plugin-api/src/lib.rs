//! Stable plugin API: traits, manifest, capabilities/permissions, extension points,
//! EventBus contract (§65–71). Core never depends on a plugin; plugins implement these
//! traits against a versioned SDK and run sandboxed (ADR-005).

#![forbid(unsafe_code)]

use editor_domain::ids::{ExtensionId, PluginId, ProviderId};

/// Semantic version of the plugin SDK the plugin targets (§66).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiVersion {
    pub major: u32,
    pub minor: u32,
}

/// Capability/permission model (§68). Default deny; host enforces every host call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    Filesystem,
    Network,
    GitCredentials,
    Clipboard,
    Process,
}

/// Plugin manifest (§67).
#[derive(Debug, Clone)]
pub struct PluginManifest {
    pub id: PluginId,
    pub name: String,
    pub version: String,
    pub api_version: ApiVersion,
    pub permissions: Vec<Capability>,
    pub contributes: Contributes,
}

/// What a plugin contributes (§69).
#[derive(Debug, Clone, Default)]
pub struct Contributes {
    pub commands: bool,
    pub markdown_extensions: bool,
    pub renderers: bool,
    pub themes: bool,
    pub exporters: bool,
    pub importers: bool,
    pub repository_providers: bool,
    pub asset_handlers: bool,
    pub sidebar_panels: bool,
    pub status_bar_items: bool,
}

/// A command id.
pub type CommandId = String;

/// Outcome of executing a command.
#[derive(Debug)]
pub enum CommandResult {
    Ok,
    Cancelled,
    Err(String),
}

/// Opaque command context provided by the host to a plugin command.
pub struct CommandContext<'a> {
    pub plugin_id: &'a PluginId,
}

/// A command contributed by a plugin (§28, §69).
pub trait Command {
    fn id(&self) -> CommandId;
    fn execute(&self, ctx: CommandContext<'_>) -> CommandResult;
}

/// Markdown syntax extension (§24). Unknown syntax without a matching extension is
/// preserved verbatim (Invariant 5).
pub trait MarkdownExtension {
    fn id(&self) -> ExtensionId;
    /// Parse priority relative to other extensions (higher runs first).
    fn precedence(&self) -> i32 {
        0
    }
}

/// Repository hosting provider adapter (§33). Implementations live in `adapters/*`.
pub trait RepositoryHostAdapter {
    fn provider_id(&self) -> ProviderId;
    fn authenticate(&self) -> Result<AuthSession, AdapterError>;
    fn repository_metadata(&self) -> Result<RepositoryMetadata, AdapterError>;
    fn pull_requests(&self) -> Result<Vec<PullRequest>, AdapterError>;
    fn create_pull_request(&self, title: &str, body: &str) -> Result<PullRequest, AdapterError>;
    fn remote_branches(&self) -> Result<Vec<RemoteBranch>, AdapterError>;
    fn open_remote_url(&self, target: RemoteUrlTarget);
}

/// Authentication session returned by a provider.
#[derive(Debug, Clone)]
pub struct AuthSession {
    pub provider: ProviderId,
    pub user: String,
    pub token: String,
}

/// Repository metadata from a hosting provider.
#[derive(Debug, Clone)]
pub struct RepositoryMetadata {
    pub full_name: String,
    pub default_branch: String,
    pub html_url: String,
}

/// A pull/merge request.
#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub html_url: String,
}

/// A remote branch.
#[derive(Debug, Clone)]
pub struct RemoteBranch {
    pub name: String,
    pub sha: String,
}

/// What to open in the browser.
#[derive(Debug, Clone)]
pub enum RemoteUrlTarget {
    Repository,
    Branch(String),
    PullRequest(u64),
    Commit(String),
}

/// Typed adapter error (§89).
#[derive(Debug)]
pub enum AdapterError {
    Auth(String),
    Network(String),
    NotFound(String),
    NotSupported,
    RateLimited,
    Other(String),
}

impl core::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&match self {
            Self::Auth(s) => format!("auth: {s}"),
            Self::Network(s) => format!("network: {s}"),
            Self::NotFound(s) => format!("not found: {s}"),
            Self::NotSupported => "not supported by this provider".to_string(),
            Self::RateLimited => "rate limited".to_string(),
            Self::Other(s) => s.clone(),
        })
    }
}
impl std::error::Error for AdapterError {}

/// Asset storage adapter (§13): import/resolve images and other assets.
pub trait AssetStorageAdapter {
    fn import_asset(&self, source: &str) -> Result<String, AdapterError>;
    fn resolve_asset(&self, reference: &str) -> Result<String, AdapterError>;
}

/// Exporter (e.g. PDF/DOCX) — Phase 2 (§109).
pub trait Exporter {
    fn id(&self) -> ExtensionId;
    fn export(&self, document_bytes: &[u8]) -> Result<Vec<u8>, AdapterError>;
}

/// Importer (e.g. HTML -> Markdown) — used for paste and file import (§77).
pub trait Importer {
    fn id(&self) -> ExtensionId;
    fn import(&self, bytes: &[u8]) -> Result<Vec<u8>, AdapterError>;
}

/// Controlled event bus contract (§70). Plugins subscribe only to events their permissions
/// allow.
pub trait EventBus {
    fn subscribe(&self, plugin: &PluginId, event: EventKind);
}

/// Kinds of events plugins may observe (§70).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    DocumentOpened,
    DocumentClosed,
    DocumentChanged,
    DocumentSaved,
    SelectionChanged,
    RepositoryOpened,
    RepositoryStatusChanged,
    BranchChanged,
    ThemeChanged,
    PluginActivated,
    PluginDeactivated,
}
