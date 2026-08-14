//! Editor Git: `VersionControl` port + `CredentialProvider` + default libgit2 implementation.
//!
//! MVP skeleton. UI depends only on the `VersionControl` trait (Invariant 3). The default
//! implementation will use `git2` (ADR-004); provider-specific APIs live in `adapters/*`
//! behind `RepositoryHostAdapter` (Invariant 4).

#![forbid(unsafe_code)]

mod cli;

pub use cli::{file_history, read_file_at_revision, FileHistoryEntry, GitCli, SystemCredentialProvider};

use editor_domain::{ids::RepositoryId, ByteRange};

/// Typed Git error (§89). Extended as operations are implemented.
///
/// `thiserror` is intentionally avoided at skeleton stage to keep the dependency surface
/// minimal (§111); `Display` and `std::error::Error` are implemented manually below.
#[derive(Debug)]
pub enum GitError {
    NotFound(String),
    InvalidState(String),
    AuthFailed(String),
    Conflict(String),
    Io(String),
    Cancelled,
    Other(String),
}

impl core::fmt::Display for GitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&match self {
            Self::NotFound(s) => format!("repository not found at {s}"),
            Self::InvalidState(s) => format!("invalid state: {s}"),
            Self::AuthFailed(s) => format!("authentication failed for {s}"),
            Self::Conflict(s) => format!("conflict: {s}"),
            Self::Io(s) => format!("io error: {s}"),
            Self::Cancelled => "operation cancelled".to_string(),
            Self::Other(s) => format!("other: {s}"),
        })
    }
}

impl std::error::Error for GitError {}

/// A Git operation result.
pub type GitResult<T> = Result<T, GitError>;

/// File change kind shown in the Git panel (§36).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Unmodified,
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

/// One file entry in `RepositoryStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub status: FileStatus,
    /// For `Renamed`: the previous path.
    pub old_path: Option<String>,
}

/// Snapshot of the repository working state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryStatus {
    pub changes: Vec<FileChange>,
    pub staged: Vec<FileChange>,
    pub untracked: Vec<FileChange>,
    pub conflicted: Vec<FileChange>,
    pub head_branch: Option<String>,
    pub dirty: bool,
}

/// A branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub is_remote: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// A commit id (40-char hex).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommitId(pub String);

/// Request to compute a diff between two sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRequest {
    WorkingTreeVsIndex,
    IndexVsHead,
    WorkingTreeVsHead,
    CommitVsCommit { a: CommitId, b: CommitId },
    BranchVsBranch { a: String, b: String },
    HeadVsBranch { branch: String },
    FileVersionVsVersion { path: String, a: CommitId, b: CommitId },
}

/// A diff result for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub hunks: Vec<editor_diff::Hunk>,
}

/// Selection of changes to stage/unstage (§41).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeSelection {
    All,
    File { path: String },
    Hunk { path: String, hunk_index: usize },
    Lines { path: String, range: ByteRange },
}

/// Request to create a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRequest {
    pub message: String,
    pub amend: bool,
}

/// A revision to checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revision {
    Branch(String),
    Commit(CommitId),
    Tag(String),
}

/// The Version Control port (§32). UI depends only on this trait.
pub trait VersionControl {
    fn repository_id(&self) -> &RepositoryId;
    fn status(&self) -> GitResult<RepositoryStatus>;
    fn diff(&self, request: DiffRequest) -> GitResult<Vec<FileDiff>>;
    fn stage(&self, selection: ChangeSelection) -> GitResult<()>;
    fn unstage(&self, selection: ChangeSelection) -> GitResult<()>;
    fn commit(&self, request: CommitRequest) -> GitResult<CommitId>;
    fn branches(&self) -> GitResult<Vec<Branch>>;
    fn checkout(&self, target: Revision) -> GitResult<()>;
    fn fetch(&self) -> GitResult<()>;
    fn pull(&self) -> GitResult<()>;
    fn push(&self) -> GitResult<()>;
}

/// Credentials for a remote (§35). Secrets are never logged (§87).
#[derive(Debug, Clone)]
pub enum Credentials {
    SshKey { key_path: String, passphrase: Option<String> },
    SshAgent,
    HttpsToken { token: String },
    OAuth { token: String },
}

/// Provider of credentials for a remote URL.
pub trait CredentialProvider {
    fn credentials_for(&self, remote_url: &str) -> GitResult<Credentials>;
}
