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
            git_bin: resolve_on_path("git", "GIT_BIN"),
        }
    }

    fn git_text(&self, args: &[&str]) -> Result<String, AdapterError> {
        let out = run_with_timeout(&self.git_bin, &self.work_dir, args, CMD_TIMEOUT)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            Err(AdapterError::Other(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ))
        }
    }
}

/// Resolve `name` to an absolute path found on PATH.
///
/// `Command::new(name)` with `current_dir(work_dir)` is dangerous on
/// Windows: `CreateProcessW` searches the working directory before PATH, so
/// a `git.exe` planted inside the repository (e.g. a malicious checkout)
/// would run instead of the real binary. An absolute path removes the
/// working directory from the search entirely (same hardening as
/// `editor-git`'s `resolve_git_binary`).
///
/// `env_var` overrides everything (operator/test control). Falls back to
/// the bare name when nothing is found on PATH — same behaviour as before.
fn resolve_on_path(name: &str, env_var: &str) -> String {
    if let Ok(b) = std::env::var(env_var) {
        // Keep it absolute: a relative override resolves through the
        // child's working directory — the same hole this function closes.
        if let Ok(abs) = Path::new(&b).canonicalize() {
            return abs.to_string_lossy().to_string();
        }
        return b;
    }
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    find_on_path(name, std::env::split_paths(&path_var)).unwrap_or_else(|| name.to_string())
}

/// Search `dirs` for `name` (plus PATHEXT extensions on Windows) and return
/// the canonical — hence absolute — first hit.
fn find_on_path(name: &str, dirs: impl Iterator<Item = PathBuf>) -> Option<String> {
    #[cfg(windows)]
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .map(|e| e.to_lowercase())
        .collect();
    for dir in dirs {
        #[cfg(windows)]
        let cands: Vec<PathBuf> = exts
            .iter()
            .map(|ext| dir.join(format!("{name}{ext}")))
            .collect();
        #[cfg(not(windows))]
        let cands: Vec<PathBuf> = vec![dir.join(name)];
        for cand in cands {
            // `canonicalize` makes even a relative PATH entry absolute —
            // a "." entry would otherwise resolve through the child's
            // working directory (the hole this exists to close).
            if cand.is_file()
                && let Ok(abs) = cand.canonicalize()
            {
                return Some(abs.to_string_lossy().to_string());
            }
        }
    }
    None
}

const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Maximum bytes read from each child pipe. A misbehaving tool cannot
/// exhaust memory: once the cap is hit the pipe fills, the child blocks on
/// write, and the deadline kills it (surfaced as "timed out").
const MAX_OUTPUT: usize = 64 * 1024 * 1024;

/// Grace period for pipe readers to hit EOF after the child exits — a
/// detached grandchild inheriting the pipe's write end keeps it open
/// forever, and joining the reader thread would hang despite the timeout.
const EOF_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Shared pipe drain: the reader appends up to `cap` bytes, `done` flips on
/// EOF/error. Bytes stay readable even while the thread is blocked.
struct PipeDrain {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

fn spawn_pipe_drain(mut pipe: impl std::io::Read + Send + 'static, cap: usize) -> PipeDrain {
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let b = buf.clone();
    let d = done.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            let room = {
                let g = b.lock().unwrap_or_else(|e| e.into_inner());
                cap.saturating_sub(g.len())
            };
            if room == 0 {
                break;
            }
            let want = room.min(chunk.len());
            match pipe.read(&mut chunk[..want]) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut g = b.lock().unwrap_or_else(|e| e.into_inner());
                    g.extend_from_slice(&chunk[..n]);
                }
            }
        }
        d.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    PipeDrain { buf, done }
}

fn collect_drain(drain: &PipeDrain, grace: std::time::Duration) -> Vec<u8> {
    let deadline = std::time::Instant::now() + grace;
    while !drain.done.load(std::sync::atomic::Ordering::Relaxed)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    std::mem::take(&mut *drain.buf.lock().unwrap_or_else(|e| e.into_inner()))
}

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
    let mut child = Command::new(bin)
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| AdapterError::Other(e.to_string()))?;
    let out_drain = spawn_pipe_drain(child.stdout.take().expect("piped"), MAX_OUTPUT);
    let err_drain = spawn_pipe_drain(child.stderr.take().expect("piped"), MAX_OUTPUT);
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child
            .try_wait()
            .map_err(|e| AdapterError::Other(e.to_string()))?
        {
            Some(s) => break s,
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AdapterError::Other(format!("{} timed out", bin)));
            }
        }
    };
    Ok(std::process::Output {
        status,
        stdout: collect_drain(&out_drain, EOF_GRACE),
        stderr: collect_drain(&err_drain, EOF_GRACE),
    })
}

impl RepositoryHostAdapter for GenericGitAdapter {
    fn provider_id(&self) -> ProviderId {
        self.provider.clone()
    }
    fn authenticate(&self) -> Result<AuthSession, AdapterError> {
        Err(AdapterError::Auth(
            "generic git has no provider auth".to_string(),
        ))
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
            branches.push(RemoteBranch {
                name: name.to_string(),
                sha: sha.to_string(),
            });
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
    // Strip `.git` first, then re-trim separators: `o/.git` -> `o`, not `o/`.
    path.trim_matches(['/', '\\'])
        .trim_end_matches(".git")
        .trim_matches(['/', '\\'])
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn make_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string());
        let run = |args: &[&str]| {
            Command::new(&git)
                .current_dir(dir.path())
                .args(args)
                .output()
                .expect("git");
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
        assert!(matches!(
            adapter.pull_requests(),
            Err(AdapterError::NotSupported)
        ));
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
        assert_eq!(
            extract_repo_name("git@github.com:owner/repo.git"),
            "owner/repo"
        );
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
        // A repo literally named ".git" trims to its owner, not "o/".
        assert_eq!(extract_repo_name("https://h/o/.git"), "o");
        assert_eq!(extract_repo_name("https://h/o/repo.git/"), "o/repo");
    }

    /// `find_on_path` must return an ABSOLUTE path — a relative hit would
    /// resolve through the child's working directory on Windows
    /// (CreateProcessW searches it before PATH), re-opening the planted-
    /// `git.exe` hole this function exists to close.
    #[test]
    fn find_on_path_returns_absolute_hit() {
        let dir = tempfile::tempdir().expect("tempdir");
        #[cfg(windows)]
        let bin = dir.path().join("fake-tool.exe");
        #[cfg(not(windows))]
        let bin = dir.path().join("fake-tool");
        fs::write(&bin, b"x").expect("write");
        let found = find_on_path("fake-tool", [dir.path().to_path_buf()].into_iter())
            .expect("binary must be found");
        assert!(Path::new(&found).is_absolute(), "{found}");
        // A missing name yields None (caller falls back to the bare name).
        assert!(find_on_path("no-such-tool-xyz", [dir.path().to_path_buf()].into_iter()).is_none());
        // Not a file → skipped.
        let sub = dir.path().join("adir");
        fs::create_dir(&sub).unwrap();
        assert!(find_on_path("adir", [dir.path().to_path_buf()].into_iter()).is_none());
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
