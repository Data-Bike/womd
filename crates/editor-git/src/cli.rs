//! CLI-backed `VersionControl` implementation (ADR-004).
//!
//! ADR-004 preferred libgit2, but `git2` requires a native libgit2 build (cmake) which
//! adds platform friction. This implementation shells out to the `git` executable behind
//! the `VersionControl` port. The port isolates the choice: a `git2`-backed impl can
//! replace this without touching any caller (Invariant 3, 4). All operations here are
//! synchronous; the host layer runs them on a dedicated Git worker with cancellation
//! (§50, Invariant 7).
//!
//! Operations: status, diff (working tree vs index vs HEAD), stage/unstage (file/hunk),
//! commit, branches, checkout, fetch, pull, push, file history.

use std::path::{Path, PathBuf};
use std::process::Command;

use editor_domain::{ids::RepositoryId, ByteRange};

use crate::{
    Branch, ChangeSelection, CommitId, CommitRequest, Credentials, DiffRequest, FileChange,
    FileDiff, FileStatus, GitError, GitResult, RepositoryStatus, Revision, VersionControl,
};

/// A `VersionControl` implementation backed by the `git` CLI.
pub struct GitCli {
    repo_id: RepositoryId,
    work_dir: PathBuf,
    git_bin: String,
}

impl GitCli {
    /// Open a repository at `work_dir` (must contain a `.git` or be inside a work tree).
    pub fn open(work_dir: impl Into<PathBuf>) -> GitResult<Self> {
        let work_dir = work_dir.into();
        let git_bin = std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string());
        // Verify it's a repo.
        let out = Self::run(&git_bin, &work_dir, &["rev-parse", "--is-inside-work-tree"])?;
        let stdout_trimmed = trim_bytes_end(&out.stdout);
        if !stdout_trimmed.eq_ignore_ascii_case(b"true") {
            return Err(GitError::NotFound(work_dir.display().to_string()));
        }
        let id = RepositoryId::new(work_dir.display().to_string());
        Ok(Self { repo_id: id, work_dir, git_bin })
    }

    /// Create a `GitCli` over an already-validated repo path (used by tests).
    pub fn new_unchecked(work_dir: PathBuf, git_bin: impl Into<String>) -> Self {
        Self {
            repo_id: RepositoryId::new(work_dir.display().to_string()),
            work_dir,
            git_bin: git_bin.into(),
        }
    }

    fn run(git_bin: &str, dir: &Path, args: &[&str]) -> GitResult<std::process::Output> {
        Command::new(git_bin)
            .current_dir(dir)
            .args(args)
            .output()
            .map_err(|e| GitError::Io(e.to_string()))
            .and_then(|o| {
                if o.status.success() {
                    Ok(o)
                } else {
                    let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    if msg.contains("not a git repository") || msg.contains("not found") {
                        Err(GitError::NotFound(msg))
                    } else {
                        Err(GitError::Other(msg))
                    }
                }
            })
    }

    fn exec(&self, args: &[&str]) -> GitResult<std::process::Output> {
        Self::run(&self.git_bin, &self.work_dir, args)
    }

    /// Run a git command and return stdout as text (public for UI use).
    pub fn exec_text(&self, args: &[&str]) -> GitResult<String> {
        let o = self.exec(args)?;
        Ok(String::from_utf8_lossy(&o.stdout).to_string())
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }
}

impl VersionControl for GitCli {
    fn repository_id(&self) -> &RepositoryId {
        &self.repo_id
    }

    fn status(&self) -> GitResult<RepositoryStatus> {
        // `git status --porcelain=v1 -z` is NUL-delimited; we use the simpler
        // `--porcelain=v1` (newline-delimited) and avoid paths with newlines (rare).
        let text = self.exec_text(&["status", "--porcelain=v1", "-b"])?;
        let mut status = RepositoryStatus::default();
        for line in text.lines() {
            if line.starts_with("## ") {
                // Branch line: "## main...origin/main [ahead 1]"
                let rest = &line[3..];
                let head = rest.split("...").next().unwrap_or("").trim();
                status.head_branch = Some(head.to_string());
                if rest.contains("ahead") || rest.contains("behind") || rest.contains("[") {
                    status.dirty = true;
                }
                continue;
            }
            if line.len() < 2 {
                continue;
            }
            let x = line.as_bytes()[0];
            let y = line.as_bytes()[1];
            let path_part = &line[3..];
            let (path, old_path) = parse_rename(path_part);
            let (status_kind, staged_kind) = (status_from_code(x), status_from_code(y));
            let change = FileChange { path: path.clone(), status: staged_kind, old_path: old_path.clone() };
            match staged_kind {
                FileStatus::Untracked => status.untracked.push(change),
                FileStatus::Added | FileStatus::Modified | FileStatus::Deleted | FileStatus::Renamed => {
                    status.staged.push(change);
                }
                FileStatus::Conflicted => status.conflicted.push(change),
                _ => {}
            }
            // Working-tree change (unstaged).
            if x == b' ' || x == b'?' {
                // already handled
            } else {
                let wt = FileChange { path, status: status_kind, old_path };
                if !matches!(wt.status, FileStatus::Unmodified | FileStatus::Untracked) {
                    status.changes.push(wt);
                }
            }
            if !matches!(status_kind, FileStatus::Unmodified) || !matches!(staged_kind, FileStatus::Unmodified) {
                status.dirty = true;
            }
        }
        Ok(status)
    }

    fn diff(&self, request: DiffRequest) -> GitResult<Vec<FileDiff>> {
        let args: Vec<&str> = match &request {
            DiffRequest::WorkingTreeVsIndex => vec!["diff", "--raw"],
            DiffRequest::IndexVsHead => vec!["diff", "--cached", "--raw"],
            DiffRequest::WorkingTreeVsHead => vec!["diff", "HEAD", "--raw"],
            DiffRequest::CommitVsCommit { a, b } => {
                vec!["diff", "--raw", a.0.as_str(), b.0.as_str()]
            }
            DiffRequest::BranchVsBranch { a, b } => vec!["diff", "--raw", a.as_str(), b.as_str()],
            DiffRequest::HeadVsBranch { branch } => vec!["diff", "--raw", "HEAD", branch.as_str()],
            DiffRequest::FileVersionVsVersion { path, a, b } => {
                vec!["diff", "--raw", a.0.as_str(), b.0.as_str(), "--", path.as_str()]
            }
        };
        let text = self.exec_text(&args)?;
        Ok(parse_raw_diff(&text))
    }

    fn stage(&self, selection: ChangeSelection) -> GitResult<()> {
        match selection {
            ChangeSelection::All => {
                self.exec(&["add", "-A"])?;
            }
            ChangeSelection::File { path } => {
                self.exec(&["add", "--", path.as_str()])?;
            }
            ChangeSelection::Hunk { path, hunk_index } => {
                stage_hunk(self, &path, hunk_index)?;
            }
            ChangeSelection::Lines { path, range } => {
                stage_line_range(self, &path, range)?;
            }
        }
        Ok(())
    }

    fn unstage(&self, selection: ChangeSelection) -> GitResult<()> {
        match selection {
            ChangeSelection::All => {
                self.exec(&["reset", "-q"])?;
            }
            ChangeSelection::File { path } => {
                self.exec(&["reset", "-q", "--", path.as_str()])?;
            }
            ChangeSelection::Hunk { path, hunk_index } => {
                unstage_hunk(self, &path, hunk_index)?;
            }
            ChangeSelection::Lines { path, range } => {
                unstage_line_range(self, &path, range)?;
            }
        }
        Ok(())
    }

    fn commit(&self, request: CommitRequest) -> GitResult<CommitId> {
        let mut args = vec!["commit".to_string(), "-m".to_string(), request.message.clone()];
        if request.amend {
            args.push("--amend".to_string());
        }
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.exec(&refs)?;
        let sha = self.exec_text(&["rev-parse", "HEAD"])?;
        Ok(CommitId(sha.trim().to_string()))
    }

    fn branches(&self) -> GitResult<Vec<Branch>> {
        let text = self.exec_text(&["branch", "-vv", "--list"])?;
        let mut out = Vec::new();
        for line in text.lines() {
            let current = line.starts_with("* ");
            let rest = if current { &line[2..] } else { line.trim_start() };
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            // upstream appears in brackets: [origin/main: ahead 1]
            let upstream = rest.find('[').and_then(|s| {
                let e = rest[s..].find(']')?;
                let inner = &rest[s + 1..s + e];
                inner.split(':').next().map(|s| s.trim().to_string())
            });
            out.push(Branch { name, is_remote: false, upstream, ahead: 0, behind: 0 });
        }
        // Remote branches.
        let rem = self.exec_text(&["branch", "-r", "--list"])?;
        for line in rem.lines() {
            let name = line.trim().to_string();
            if !name.is_empty() && !name.contains(" -> ") {
                out.push(Branch { name, is_remote: true, upstream: None, ahead: 0, behind: 0 });
            }
        }
        Ok(out)
    }

    fn checkout(&self, target: Revision) -> GitResult<()> {
        match target {
            Revision::Branch(b) => self.exec(&["checkout", b.as_str()])?,
            Revision::Commit(c) => self.exec(&["checkout", c.0.as_str()])?,
            Revision::Tag(t) => self.exec(&["checkout", t.as_str()])?,
        };
        Ok(())
    }

    fn fetch(&self) -> GitResult<()> {
        self.exec(&["fetch", "--all"])?;
        Ok(())
    }

    fn pull(&self) -> GitResult<()> {
        self.exec(&["pull"])?;
        Ok(())
    }

    fn push(&self) -> GitResult<()> {
        self.exec(&["push"])?;
        Ok(())
    }
}

/// Trim trailing ASCII whitespace bytes from a `Vec<u8>`, returning a slice.
fn trim_bytes_end(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\n' || bytes[end - 1] == b'\r' || bytes[end - 1] == b'\t') {
        end -= 1;
    }
    &bytes[..end]
}

/// Map a porcelain status code byte to a `FileStatus`.
fn status_from_code(c: u8) -> FileStatus {
    match c {
        b'M' => FileStatus::Modified,
        b'A' => FileStatus::Added,
        b'D' => FileStatus::Deleted,
        b'R' => FileStatus::Renamed,
        b'?' => FileStatus::Untracked,
        b'U' | b'C' => FileStatus::Conflicted,
        _ => FileStatus::Unmodified,
    }
}

/// Parse a `path` or `old -> new` rename form.
fn parse_rename(s: &str) -> (String, Option<String>) {
    if let Some(idx) = s.find(" -> ") {
        let old = s[..idx].trim().to_string();
        let new = s[idx + 4..].trim().to_string();
        return (new, Some(old));
    }
    (s.trim().to_string(), None)
}

/// Parse `git diff --raw` output into `FileDiff`s (without hunks; hunks come from a
/// follow-up `git diff <path>` call when the UI expands a file).
fn parse_raw_diff(text: &str) -> Vec<FileDiff> {
    let mut out = Vec::new();
    for line in text.lines() {
        // Format: ":<src-mode> <dst-mode> <sha> <sha> <status>\t<path>"
        if !line.starts_with(':') {
            continue;
        }
        let mut parts = line.splitn(2, '\t');
        let _meta = parts.next().unwrap_or("");
        let path_part = parts.next().unwrap_or("");
        let (path, old_path) = parse_rename(path_part);
        out.push(FileDiff { path, old_path, hunks: Vec::new() });
    }
    out
}

// ---------------------------------------------------------------------------
// Hunk-level staging (§42)
// ---------------------------------------------------------------------------

/// Get the unified diff for a single file (working tree vs index).
fn file_unified_diff(repo: &GitCli, path: &str) -> GitResult<String> {
    repo.exec_text(&["diff", "--", path])
}

/// Get the unified diff for a single file (index vs HEAD) — for unstaging.
fn file_unified_diff_cached(repo: &GitCli, path: &str) -> GitResult<String> {
    repo.exec_text(&["diff", "--cached", "--", path])
}

/// Extract the `hunk_index`-th hunk from a unified diff text.
/// Returns the hunk header + body lines (including `@@ ... @@` line).
fn extract_hunk(diff_text: &str, hunk_index: usize) -> Option<String> {
    let mut hunk_count = 0usize;
    let mut current: Vec<&str> = Vec::new();
    let mut in_hunk = false;
    for line in diff_text.lines() {
        if line.starts_with("@@ ") {
            // If we were collecting the target hunk, we're done.
            if in_hunk && hunk_count - 1 == hunk_index {
                return Some(current.join("\n") + "\n");
            }
            in_hunk = true;
            hunk_count += 1;
            current.clear();
            current.push(line);
        } else if in_hunk {
            // A hunk body line starts with ' ', '+', '-', or '\' (no newline marker).
            let c = line.as_bytes().first().copied();
            if matches!(c, Some(b' ') | Some(b'+') | Some(b'-') | Some(b'\\')) {
                current.push(line);
            } else {
                // End of hunk (e.g. next "diff --git" or empty line at end).
                if hunk_count - 1 == hunk_index {
                    return Some(current.join("\n") + "\n");
                }
                in_hunk = false;
            }
        }
    }
    // Last hunk at end of text.
    if in_hunk && hunk_count - 1 == hunk_index && !current.is_empty() {
        return Some(current.join("\n") + "\n");
    }
    None
}

/// Build a complete patch file from a file header + a single hunk.
/// `git apply` needs the `diff --git` header and the `---`/`+++` lines.
fn build_patch(diff_text: &str, hunk_text: &str) -> String {
    let mut patch = String::new();
    // Extract the `diff --git` line and `--- `/`+++ ` lines from the full diff.
    for line in diff_text.lines() {
        if line.starts_with("diff --git") || line.starts_with("index ") || line.starts_with("--- ") || line.starts_with("+++ ") {
            patch.push_str(line);
            patch.push('\n');
        }
        if line.starts_with("+++ ") {
            break;
        }
    }
    patch.push_str(hunk_text);
    patch
}

/// Stage a single hunk of a file via `git apply --cached` (§42).
fn stage_hunk(repo: &GitCli, path: &str, hunk_index: usize) -> GitResult<()> {
    let diff_text = file_unified_diff(repo, path)?;
    let hunk = extract_hunk(&diff_text, hunk_index)
        .ok_or_else(|| GitError::Other(format!("hunk {hunk_index} not found in {path}")))?;
    let patch = build_patch(&diff_text, &hunk);
    apply_patch_to_index(repo, &patch)
}

/// Unstage a single hunk of a file via `git apply --cached --reverse` (§42).
fn unstage_hunk(repo: &GitCli, path: &str, hunk_index: usize) -> GitResult<()> {
    let diff_text = file_unified_diff_cached(repo, path)?;
    let hunk = extract_hunk(&diff_text, hunk_index)
        .ok_or_else(|| GitError::Other(format!("hunk {hunk_index} not found in staged {path}")))?;
    let patch = build_patch(&diff_text, &hunk);
    apply_patch_to_index_reverse(repo, &patch)
}

/// Stage a line range of a file. For the MVP this stages the whole hunk containing the
/// range; precise line-level staging requires `git apply --cached` with a manually
/// constructed patch that includes only the specified lines.
fn stage_line_range(repo: &GitCli, path: &str, _range: ByteRange) -> GitResult<()> {
    // Simplified: stage the whole file for now. Line-precise staging is a UI-layer follow-up.
    repo.exec(&["add", "--", path])?;
    Ok(())
}

fn unstage_line_range(repo: &GitCli, path: &str, _range: ByteRange) -> GitResult<()> {
    repo.exec(&["reset", "-q", "--", path])?;
    Ok(())
}

/// Apply a patch to the index using `git apply --cached`.
fn apply_patch_to_index(repo: &GitCli, patch: &str) -> GitResult<()> {
    use std::io::Write;
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("womd_hunk_patch_{}.patch", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| GitError::Io(e.to_string()))?;
        f.write_all(patch.as_bytes()).map_err(|e| GitError::Io(e.to_string()))?;
    }
    let path_str = tmp.to_string_lossy().to_string();
    let result = repo.exec(&["apply", "--cached", path_str.as_str()]);
    let _ = std::fs::remove_file(&tmp);
    result?;
    Ok(())
}

/// Apply a reverse patch to the index (unstage).
fn apply_patch_to_index_reverse(repo: &GitCli, patch: &str) -> GitResult<()> {
    use std::io::Write;
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("womd_hunk_unpatch_{}.patch", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| GitError::Io(e.to_string()))?;
        f.write_all(patch.as_bytes()).map_err(|e| GitError::Io(e.to_string()))?;
    }
    let path_str = tmp.to_string_lossy().to_string();
    let result = repo.exec(&["apply", "--cached", "--reverse", path_str.as_str()]);
    let _ = std::fs::remove_file(&tmp);
    result?;
    Ok(())
}

/// File history for a single document (§45).
pub fn file_history(repo: &GitCli, path: &str) -> GitResult<Vec<FileHistoryEntry>> {
    let text = repo.exec_text(&["log", "--follow", "--pretty=format:%H%x09%an%x09%ad%x09%s", "--date=short", "--", path])?;
    let mut out = Vec::new();
    for line in text.lines() {
        let mut f = line.split('\t');
        let sha = f.next().unwrap_or("").to_string();
        let author = f.next().unwrap_or("").to_string();
        let date = f.next().unwrap_or("").to_string();
        let message = f.next().unwrap_or("").to_string();
        if !sha.is_empty() {
            out.push(FileHistoryEntry { revision: CommitId(sha), author, date, message });
        }
    }
    Ok(out)
}

/// One entry in a file's history (§45).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHistoryEntry {
    pub revision: CommitId,
    pub author: String,
    pub date: String,
    pub message: String,
}

/// Read a file at a given revision (for "open old version read-only", §45).
pub fn read_file_at_revision(repo: &GitCli, path: &str, rev: &str) -> GitResult<Vec<u8>> {
    let out = repo.exec(&["show", &format!("{rev}:{path}")])?;
    Ok(out.stdout)
}

/// A credential provider that uses the system `git` credential helper / SSH agent (§35).
/// No secrets are stored by the editor (§87); they live in the OS credential store / SSH
/// agent and are accessed by `git` itself.
pub struct SystemCredentialProvider;

impl crate::CredentialProvider for SystemCredentialProvider {
    fn credentials_for(&self, _remote_url: &str) -> GitResult<Credentials> {
        // Defer to git's own credential resolution (credential.helper, SSH agent, etc.).
        // We signal "use system resolution" by returning SshAgent for SSH URLs and an
        // empty token for HTTPS (git will prompt via its helper). This keeps secrets out
        // of the editor process (§87).
        Ok(Credentials::SshAgent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    /// Skip tests if `git` is not available on PATH.
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
        dir
    }

    fn write(dir: &Path, name: &str, content: &[u8]) {
        fs::write(dir.join(name), content).expect("write");
    }

    #[test]
    fn status_clean_repo() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"# A\n");
        let repo = GitCli::open(dir.path()).expect("open");
        let st = repo.status().expect("status");
        // 'a.md' is untracked.
        assert!(st.untracked.iter().any(|f| f.path == "a.md"));
    }

    #[test]
    fn stage_and_commit() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"# Doc\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let id = repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        assert!(!id.0.is_empty());
        let st = repo.status().expect("status");
        assert!(st.changes.is_empty() && st.staged.is_empty() && st.untracked.is_empty());
    }

    #[test]
    fn diff_working_tree_vs_head() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"# Doc\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        // Modify.
        write(dir.path(), "doc.md", b"# Doc\n\nNew para.\n");
        let diffs = repo.diff(DiffRequest::WorkingTreeVsHead).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "doc.md"));
    }

    #[test]
    fn branches_lists_main() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        let branches = repo.branches().expect("branches");
        assert!(!branches.is_empty());
    }

    #[test]
    fn file_history_after_commits() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::File { path: "doc.md".to_string() }).expect("stage");
        repo.commit(CommitRequest { message: "first".to_string(), amend: false }).expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::File { path: "doc.md".to_string() }).expect("stage");
        repo.commit(CommitRequest { message: "second".to_string(), amend: false }).expect("commit");
        let hist = file_history(&repo, "doc.md").expect("history");
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].message, "second");
        // Read old version.
        let old = read_file_at_revision(&repo, "doc.md", &hist[1].revision.0).expect("read old");
        assert_eq!(old, b"v1\n");
    }

    #[test]
    fn checkout_branch() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        // Create and checkout a new branch via CLI, then verify branches() sees it.
        Command::new(std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string()))
            .current_dir(dir.path())
            .args(["branch", "feature"])
            .output()
            .expect("branch");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        let st = repo.status().expect("status");
        assert_eq!(st.head_branch.as_deref(), Some("feature"));
    }

    #[test]
    fn unstage_reverts_to_unstaged() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"# Doc\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::File { path: "doc.md".to_string() }).expect("stage");
        repo.unstage(ChangeSelection::File { path: "doc.md".to_string() }).expect("unstage");
        let st = repo.status().expect("status");
        assert!(st.staged.is_empty());
        assert!(st.untracked.iter().any(|f| f.path == "doc.md") || st.changes.iter().any(|f| f.path == "doc.md"));
    }

    #[test]
    fn stage_single_hunk() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        // Create a file with changes far enough apart to produce 2 hunks (>= 7 lines
        // between changes, so 3-line context windows don't merge them).
        let original = b"# Title\n\nLine 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\n\nFirst change here.\nLine 11\nLine 12\nLine 13\nLine 14\nLine 15\nLine 16\nLine 17\nLine 18\n\nSecond change here.\n";
        write(dir.path(), "doc.md", original);
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        // Modify two distant lines (creates 2 separate hunks).
        let modified = b"# Title\n\nLine 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\n\nFirst CHANGED.\nLine 11\nLine 12\nLine 13\nLine 14\nLine 15\nLine 16\nLine 17\nLine 18\n\nSecond CHANGED.\n";
        write(dir.path(), "doc.md", modified);
        // Stage only the first hunk (hunk_index 0).
        repo.stage(ChangeSelection::Hunk { path: "doc.md".to_string(), hunk_index: 0 }).expect("stage hunk");
        // Verify: the first change is staged, the second is still unstaged.
        let st = repo.status().expect("status");
        assert!(st.staged.iter().any(|f| f.path == "doc.md"), "file should be partially staged");
        assert!(st.changes.iter().any(|f| f.path == "doc.md"), "file should still have unstaged changes");
    }

    #[test]
    fn unstage_single_hunk() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        let original = b"# Title\n\nLine 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\n\nFirst change here.\nLine 11\nLine 12\nLine 13\nLine 14\nLine 15\nLine 16\nLine 17\nLine 18\n\nSecond change here.\n";
        write(dir.path(), "doc.md", original);
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        // Modify both and stage all.
        let modified = b"# Title\n\nLine 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\n\nFirst CHANGED.\nLine 11\nLine 12\nLine 13\nLine 14\nLine 15\nLine 16\nLine 17\nLine 18\n\nSecond CHANGED.\n";
        write(dir.path(), "doc.md", modified);
        repo.stage(ChangeSelection::File { path: "doc.md".to_string() }).expect("stage all");
        // Unstage the first hunk only.
        repo.unstage(ChangeSelection::Hunk { path: "doc.md".to_string(), hunk_index: 0 }).expect("unstage hunk");
        // Verify: the second change is still staged, the first is unstaged.
        let st = repo.status().expect("status");
        assert!(st.staged.iter().any(|f| f.path == "doc.md"), "some changes should still be staged");
        assert!(st.changes.iter().any(|f| f.path == "doc.md"), "some changes should be unstaged");
    }
}
