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
        // `for-each-ref` output format is stable (unlike `git branch` porcelain)
        // and carries the object name, so each branch reports its real sha.
        // Format: "<sha> <short-name> [<symref-target>]". Ref names cannot
        // contain spaces, so a line with a third field is a symref — e.g.
        // `refs/remotes/origin/HEAD` (local bookkeeping, not a real branch).
        let text = self.git_text(&[
            "for-each-ref",
            "--format=%(objectname) %(refname:short) %(symref)",
            "refs/remotes",
        ])?;
        let mut branches = Vec::new();
        for line in text.lines() {
            let Some((sha, name)) = line.trim().split_once(' ') else {
                continue;
            };
            if sha.is_empty() || name.is_empty() || name.contains(' ') {
                continue; // malformed line or symref entry (HEAD)
            }
            branches.push(RemoteBranch { name: name.to_string(), sha: sha.to_string() });
        }
        Ok(branches)
    }
    fn open_remote_url(&self, _target: RemoteUrlTarget) {}
}

/// Extract `owner/repo` from a remote URL — the same `nameWithOwner` shape the
/// GitHub adapter reports. Handles HTTPS (`https://host/owner/repo.git`),
/// scp-style SSH (`git@host:owner/repo.git`), `ssh://`, `file://` and local
/// paths (which keep as much of the path as is meaningful).
fn extract_repo_name(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return String::new();
    }
    let path = if let Some(idx) = url.find("://") {
        // scheme://host/owner/repo — drop everything through the first '/' of
        // the authority. `file:///p/a/t/h` has an empty authority, so the
        // first '/' keeps the whole local path.
        let rest = &url[idx + 3..];
        match rest.find('/') {
            Some(slash) => &rest[slash + 1..],
            None => rest, // "host" only, no path
        }
    } else if let Some(idx) = url.find(':') {
        // scp-style SSH "user@host:owner/repo" — but a bare Windows drive
        // "C:\repo" has its ':' at index 1 and is a local path, not SSH.
        if idx > 1 { &url[idx + 1..] } else { url }
    } else {
        url
    };
    path.trim_matches(['/', '\\']).trim_end_matches(".git").to_string()
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

    #[test]
    fn extract_repo_name_shapes() {
        // Same nameWithOwner shape as the GitHub adapter across URL styles.
        assert_eq!(
            extract_repo_name("https://github.com/owner/repo.git"),
            "owner/repo"
        );
        assert_eq!(extract_repo_name("git@github.com:owner/repo.git"), "owner/repo");
        assert_eq!(
            extract_repo_name("ssh://git@github.com/owner/repo.git"),
            "owner/repo"
        );
        // Host with no owner segment falls back to the bare repo name.
        assert_eq!(extract_repo_name("https://example.com/repo.git"), "repo");
        // Trailing slashes and a missing .git suffix are tolerated.
        assert_eq!(extract_repo_name("https://h/o/repo/"), "o/repo");
        assert_eq!(extract_repo_name("https://h/o/repo"), "o/repo");
        // Local file remotes keep a meaningful path instead of going blank.
        assert_eq!(extract_repo_name("file:///home/u/repo.git"), "home/u/repo");
        assert!(!extract_repo_name("C:\\repos\\x.git").is_empty());
        assert_eq!(extract_repo_name(""), "");
        assert_eq!(extract_repo_name("   "), "");
    }

    /// A real clone registers `refs/remotes/origin/HEAD` (a symref to the
    /// default branch). It must be filtered out, and real branches must
    /// carry their object name — `git branch -r` never populated `sha`.
    #[test]
    fn remote_branches_skips_head_symref_and_reports_sha() {
        if !git_available() {
            return;
        }
        let origin = make_repo();
        let clone_dir = tempfile::tempdir().expect("clone tempdir");
        let git = std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string());
        let out = Command::new(&git)
            .args([
                "clone",
                "-q",
                origin.path().to_str().expect("utf8 path"),
                clone_dir.path().to_str().expect("utf8 path"),
            ])
            .output()
            .expect("clone");
        assert!(out.status.success(), "clone failed: {:?}", out);
        // The clone has the HEAD symref bookkeeping ref.
        let has_head = Command::new(&git)
            .current_dir(clone_dir.path())
            .args(["rev-parse", "--verify", "refs/remotes/origin/HEAD"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(has_head, "clone should have refs/remotes/origin/HEAD");

        let adapter = GenericGitAdapter::new(clone_dir.path());
        let branches = adapter.remote_branches().expect("remote_branches");
        assert!(!branches.is_empty());
        for b in &branches {
            assert!(!b.name.ends_with("/HEAD"), "symref leaked: {}", b.name);
            assert!(b.name.starts_with("origin/"));
            assert_eq!(b.sha.len(), 40, "sha must be a full object name");
            assert!(b.sha.chars().all(|c| c.is_ascii_hexdigit()));
        }
        // The reported sha matches the real remote-tracking ref.
        let expected = Command::new(&git)
            .current_dir(clone_dir.path())
            .args(["rev-parse", "refs/remotes/origin/HEAD"])
            .output()
            .expect("rev-parse");
        let expected_sha = String::from_utf8_lossy(&expected.stdout).trim().to_string();
        let head_branch = branches
            .iter()
            .find(|b| !b.name.ends_with("/HEAD"))
            .expect("at least one real branch");
        assert_eq!(head_branch.sha, expected_sha);
    }
}
