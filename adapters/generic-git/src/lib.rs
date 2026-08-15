//! Generic Git `RepositoryHostAdapter` (§34): a plain Git remote with no provider API.
//! A GitLab/Bitbucket/Gitea repo works via this adapter even without a provider-specific
//! adapter (Invariant 4).
//!
//! Provider-API operations (PRs, issues) degrade to `NotSupported`. Repository metadata
//! and remote branches are read from the local `.git` config via the `git` CLI.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::Command;

use editor_domain::ids::ProviderId;
use editor_plugin_api::{
    AdapterError, AuthSession, PullRequest, RemoteBranch, RemoteUrlTarget, RepositoryHostAdapter,
    RepositoryMetadata,
};

/// Generic Git adapter: reads metadata from the local repo; degrades provider-API
/// operations to "unsupported" and relies on the `VersionControl` port for Git operations.
pub struct GenericGitAdapter {
    provider: ProviderId,
    work_dir: PathBuf,
    git_bin: String,
}

impl GenericGitAdapter {
    /// Create an adapter for a repository at `work_dir`.
    pub fn new(work_dir: impl Into<PathBuf>) -> Self {
        Self {
            provider: ProviderId::from_str_unchecked("generic-git"),
            work_dir: work_dir.into(),
            git_bin: std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string()),
        }
    }

    fn git_text(&self, args: &[&str]) -> Result<String, AdapterError> {
        let out = Command::new(&self.git_bin)
            .current_dir(&self.work_dir)
            .args(args)
            .output()
            .map_err(|e| AdapterError::Other(e.to_string()))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            Err(AdapterError::Other(String::from_utf8_lossy(&out.stderr).trim().to_string()))
        }
    }
}

impl RepositoryHostAdapter for GenericGitAdapter {
    fn provider_id(&self) -> ProviderId {
        self.provider.clone()
    }
    fn authenticate(&self) -> Result<AuthSession, AdapterError> {
        Err(AdapterError::Auth("generic git has no provider auth".to_string()))
    }
    fn repository_metadata(&self) -> Result<RepositoryMetadata, AdapterError> {
        let remote_url = self
            .git_text(&["config", "--get", "remote.origin.url"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let head = self
            .git_text(&["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let name = extract_repo_name(&remote_url);
        Ok(RepositoryMetadata {
            full_name: name,
            default_branch: head,
            html_url: remote_url,
        })
    }
    fn pull_requests(&self) -> Result<Vec<PullRequest>, AdapterError> {
        Err(AdapterError::NotSupported)
    }
    fn create_pull_request(&self, _title: &str, _body: &str) -> Result<PullRequest, AdapterError> {
        Err(AdapterError::NotSupported)
    }
    fn remote_branches(&self) -> Result<Vec<RemoteBranch>, AdapterError> {
        let text = self.git_text(&["branch", "-r", "--list"])?;
        let mut branches = Vec::new();
        for line in text.lines() {
            let name = line.trim().to_string();
            if !name.is_empty() && !name.contains(" -> ") {
                branches.push(RemoteBranch { name, sha: String::new() });
            }
        }
        Ok(branches)
    }
    fn open_remote_url(&self, _target: RemoteUrlTarget) {}
}

/// Extract a repository name from a remote URL.
/// Handles HTTPS (`https://host/owner/repo.git`),
/// SSH (`git@host:owner/repo.git`), and bare names.
fn extract_repo_name(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    // SSH style: git@github.com:owner/repo.git (no "://" in the URL)
    if !url.contains("://") {
        if let Some(idx) = url.rfind(':') {
            return url[idx + 1..].trim_end_matches(".git").to_string();
        }
    }
    // HTTPS style: https://host/owner/repo.git
    url.rsplit('/')
        .next()
        .map(|s| s.trim_end_matches(".git").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
    }

    fn make_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string());
        let run = |args: &[&str]| {
            Command::new(&git).current_dir(dir.path()).args(args).output().expect("git");
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "t@t"]);
        run(&["remote", "add", "origin", "https://example.com/repo.git"]);
        fs::write(dir.path().join("a.md"), b"a\n").expect("write");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn metadata_from_local_repo() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        let adapter = GenericGitAdapter::new(dir.path());
        let meta = adapter.repository_metadata().expect("metadata");
        assert_eq!(meta.full_name, "repo");
        assert_eq!(meta.html_url, "https://example.com/repo.git");
        assert!(!meta.default_branch.is_empty());
    }

    #[test]
    fn pull_requests_not_supported() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        let adapter = GenericGitAdapter::new(dir.path());
        assert!(matches!(adapter.pull_requests(), Err(AdapterError::NotSupported)));
    }

    #[test]
    fn create_pr_not_supported() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        let adapter = GenericGitAdapter::new(dir.path());
        assert!(matches!(
            adapter.create_pull_request("t", "b"),
            Err(AdapterError::NotSupported)
        ));
    }
}
