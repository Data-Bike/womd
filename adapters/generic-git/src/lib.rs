//! Generic Git `RepositoryHostAdapter` (§34): a plain Git remote with no provider API.
//! A GitLab/Bitbucket/Gitea repo works via this adapter even without a provider-specific
//! adapter (Invariant 4).
//!
//! Provider-API operations (PRs, issues) degrade to `NotSupported`. Repository metadata
//! and remote branches are read from the local `.git` config via the `git` CLI.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
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
        let out = run_with_timeout(&self.git_bin, &self.work_dir, args, CMD_TIMEOUT)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            Err(AdapterError::Other(String::from_utf8_lossy(&out.stderr).trim().to_string()))
        }
    }
}

const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// `Command::output()` has no timeout — a wedged git (stale lock waiting on a
/// dead mount, credential prompt on a non-existent terminal) hangs forever.
/// Drain both pipes on reader threads (a full pipe buffer blocks a healthy
/// child, which then looks like a hang), poll `try_wait`, kill on deadline.
fn run_with_timeout(
    bin: &str,
    dir: &Path,
    args: &[&str],
    timeout: std::time::Duration,
) -> Result<std::process::Output, AdapterError> {
    use std::io::Read;
    let mut child = Command::new(bin)
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| AdapterError::Other(e.to_string()))?;
    let mut out_pipe = child.stdout.take().expect("piped");
    let mut err_pipe = child.stderr.take().expect("piped");
    let out_reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out_pipe.read_to_end(&mut v);
        v
    });
    let err_reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err_pipe.read_to_end(&mut v);
        v
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(|e| AdapterError::Other(e.to_string()))? {
            Some(s) => break s,
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(AdapterError::Other(format!("{} timed out", bin)));
            }
        }
    };
    Ok(std::process::Output {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    })
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
