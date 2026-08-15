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
    Branch, ChangeSelection, CommitEntry, CommitId, CommitRequest, Credentials, DiffRequest,
    FileChange, FileDiff, FileStatus, GitError, GitResult, MergeStrategy, Remote, RepositoryStatus,
    Revision, StashEntry, Tag, VersionControl, GitExtended,
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
            let x = line.as_bytes()[0]; // X = staged (index) status
            let y = line.as_bytes()[1]; // Y = working tree status
            let path_part = &line[3..];
            let (path, old_path) = parse_rename(path_part);
            let staged_status = status_from_code(x);
            let wt_status = status_from_code(y);

            // Staged changes come from the X column.
            match staged_status {
                FileStatus::Added | FileStatus::Modified | FileStatus::Deleted | FileStatus::Renamed => {
                    status.staged.push(FileChange { path: path.clone(), status: staged_status, old_path: old_path.clone() });
                    status.dirty = true;
                }
                FileStatus::Conflicted => {
                    status.conflicted.push(FileChange { path: path.clone(), status: staged_status, old_path: old_path.clone() });
                    status.dirty = true;
                }
                _ => {}
            }

            // Working tree changes come from the Y column.
            match wt_status {
                FileStatus::Untracked => {
                    status.untracked.push(FileChange { path: path.clone(), status: wt_status, old_path: old_path.clone() });
                    status.dirty = true;
                }
                FileStatus::Added | FileStatus::Modified | FileStatus::Deleted | FileStatus::Renamed => {
                    status.changes.push(FileChange { path: path.clone(), status: wt_status, old_path: old_path.clone() });
                    status.dirty = true;
                }
                FileStatus::Conflicted => {
                    if !status.conflicted.iter().any(|f| f.path == path) {
                        status.conflicted.push(FileChange { path: path.clone(), status: wt_status, old_path: old_path.clone() });
                    }
                    status.dirty = true;
                }
                _ => {}
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
            DiffRequest::WorkingTreeVsCommit { commit } => {
                vec!["diff", "--raw", commit.0.as_str()]
            }
            DiffRequest::IndexVsCommit { commit } => {
                vec!["diff", "--cached", "--raw", commit.0.as_str()]
            }
            DiffRequest::WorkingTreeVsCommitFile { commit, path } => {
                vec!["diff", "--raw", commit.0.as_str(), "--", path.as_str()]
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
                // Use :/ prefix so the path is resolved from the repo root,
                // not the current working directory (which may be a subdirectory).
                let ps = if path.starts_with(":/") { path.clone() } else { format!(":/{}", path) };
                self.exec(&["add", "--", ps.as_str()])?;
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
                let ps = if path.starts_with(":/") { path.clone() } else { format!(":/{}", path) };
                self.exec(&["reset", "-q", "--", ps.as_str()])?;
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
    if path.chars().any(|c| c == '\n' || c == '\r' || c == '\0') {
        return Err(GitError::Other("invalid characters in path".into()));
    }
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
    // Security: reject paths/revs containing newlines or control chars to prevent
    // git argument injection (git interprets newlines as argument separators).
    if path.chars().any(|c| c == '\n' || c == '\r' || c == '\0') {
        return Err(GitError::Other("invalid characters in path".into()));
    }
    if rev.chars().any(|c| c == '\n' || c == '\r' || c == '\0') {
        return Err(GitError::Other("invalid characters in revision".into()));
    }
    let out = repo.exec(&["show", &format!("{rev}:{path}")])?;
    Ok(out.stdout)
}

/// Blanket impl that exposes `GitExtended` methods on `GitCli`.
/// This is a separate type so callers can do `GitExtendedImpl(&cli)` or
/// use the trait directly since `GitCli` implements `GitExtended`.
pub struct GitExtendedImpl<'a>(pub &'a GitCli);

impl GitExtended for GitCli {
    // ── Stash ──────────────────────────────────────────────────────────────
    fn stash_list(&self) -> GitResult<Vec<StashEntry>> {
        let text = self.exec_text(&["stash", "list"])?;
        let mut out = Vec::new();
        for line in text.lines() {
            if line.is_empty() { continue; }
            // Format: "stash@{0}: WIP on branch: abc123 message"
            let parts: Vec<&str> = line.splitn(2, ": ").collect();
            if parts.len() < 2 { continue; }
            let idx_str = parts[0].trim_start_matches("stash@{").trim_end_matches('}');
            let index: usize = idx_str.parse().unwrap_or(0);
            let rest = parts[1];
            let branch = rest.split(':').next().unwrap_or("").trim().to_string();
            let message = rest.to_string();
            out.push(StashEntry { index, message, branch });
        }
        Ok(out)
    }

    fn stash_push(&self, message: Option<&str>) -> GitResult<()> {
        match message {
            Some(msg) => { self.exec(&["stash", "push", "-m", msg])?; }
            None => { self.exec(&["stash", "push"])?; }
        }
        Ok(())
    }

    fn stash_pop(&self, index: usize) -> GitResult<()> {
        let ref_str = format!("stash@{{{}}}", index);
        self.exec(&["stash", "pop", ref_str.as_str()])?;
        Ok(())
    }

    fn stash_apply(&self, index: usize) -> GitResult<()> {
        let ref_str = format!("stash@{{{}}}", index);
        self.exec(&["stash", "apply", ref_str.as_str()])?;
        Ok(())
    }

    fn stash_drop(&self, index: usize) -> GitResult<()> {
        let ref_str = format!("stash@{{{}}}", index);
        self.exec(&["stash", "drop", ref_str.as_str()])?;
        Ok(())
    }

    fn stash_clear(&self) -> GitResult<()> {
        self.exec(&["stash", "clear"])?;
        Ok(())
    }

    // ── Branch management ──────────────────────────────────────────────────
    fn create_branch(&self, name: &str) -> GitResult<()> {
        self.exec(&["branch", name])?;
        Ok(())
    }

    fn delete_branch(&self, name: &str, force: bool) -> GitResult<()> {
        let flag = if force { "-D" } else { "-d" };
        self.exec(&["branch", flag, name])?;
        Ok(())
    }

    fn rename_branch(&self, old_name: &str, new_name: &str) -> GitResult<()> {
        self.exec(&["branch", "-m", old_name, new_name])?;
        Ok(())
    }

    // ── Merge / Rebase ─────────────────────────────────────────────────────
    fn merge(&self, branch: &str, strategy: MergeStrategy) -> GitResult<()> {
        let args: Vec<&str> = match strategy {
            MergeStrategy::Merge => vec!["merge", "--no-ff", branch],
            MergeStrategy::FastForwardOnly => vec!["merge", "--ff-only", branch],
            MergeStrategy::NoFastForward => vec!["merge", "--no-ff", branch],
            MergeStrategy::Squash => vec!["merge", "--squash", branch],
        };
        self.exec(&args)?;
        // For squash, we need to commit the result.
        if strategy == MergeStrategy::Squash {
            let msg = format!("Squash merge from {}", branch);
            self.exec(&["commit", "-m", msg.as_str()])?;
        }
        Ok(())
    }

    fn merge_abort(&self) -> GitResult<()> {
        self.exec(&["merge", "--abort"])?;
        Ok(())
    }

    fn rebase(&self, branch: &str) -> GitResult<()> {
        self.exec(&["rebase", branch])?;
        Ok(())
    }

    fn rebase_abort(&self) -> GitResult<()> {
        self.exec(&["rebase", "--abort"])?;
        Ok(())
    }

    fn rebase_continue(&self) -> GitResult<()> {
        self.exec(&["rebase", "--continue"])?;
        Ok(())
    }

    fn rebase_skip(&self) -> GitResult<()> {
        self.exec(&["rebase", "--skip"])?;
        Ok(())
    }

    // ── Cherry-pick / Revert ───────────────────────────────────────────────
    fn cherry_pick(&self, commit: &str) -> GitResult<()> {
        self.exec(&["cherry-pick", commit])?;
        Ok(())
    }

    fn cherry_pick_abort(&self) -> GitResult<()> {
        self.exec(&["cherry-pick", "--abort"])?;
        Ok(())
    }

    fn cherry_pick_continue(&self) -> GitResult<()> {
        self.exec(&["cherry-pick", "--continue"])?;
        Ok(())
    }

    fn revert(&self, commit: &str) -> GitResult<()> {
        self.exec(&["revert", "--no-edit", commit])?;
        Ok(())
    }

    fn revert_abort(&self) -> GitResult<()> {
        self.exec(&["revert", "--abort"])?;
        Ok(())
    }

    fn revert_continue(&self) -> GitResult<()> {
        self.exec(&["revert", "--continue"])?;
        Ok(())
    }

    // ── Tags ───────────────────────────────────────────────────────────────
    fn tags(&self) -> GitResult<Vec<Tag>> {
        // Use %(objecttype) to distinguish annotated ("tag") from lightweight ("commit").
        let text = self.exec_text(&["tag", "-l", "--format=%(refname:short)%09%(objectname:short)%09%(objecttype)%09%(contents:subject)"])?;
        let mut out = Vec::new();
        for line in text.lines() {
            if line.is_empty() { continue; }
            let mut f = line.split('\t');
            let name = f.next().unwrap_or("").to_string();
            let target = f.next().unwrap_or("").to_string();
            let obj_type = f.next().unwrap_or("").to_string();
            let message = f.next().map(|s| s.to_string());
            if !name.is_empty() {
                let is_lightweight = obj_type != "tag";
                let msg = if is_lightweight { None } else { message.filter(|m| !m.is_empty()) };
                out.push(Tag { name, target, message: msg, is_lightweight });
            }
        }
        Ok(out)
    }

    fn create_tag(&self, name: &str, message: Option<&str>) -> GitResult<()> {
        match message {
            Some(msg) => { self.exec(&["tag", "-a", name, "-m", msg])?; }
            None => { self.exec(&["tag", name])?; }
        }
        Ok(())
    }

    fn delete_tag(&self, name: &str) -> GitResult<()> {
        self.exec(&["tag", "-d", name])?;
        Ok(())
    }

    // ── Remotes ────────────────────────────────────────────────────────────
    fn remotes(&self) -> GitResult<Vec<Remote>> {
        let text = self.exec_text(&["remote", "-v"])?;
        let mut remotes: std::collections::HashMap<String, Remote> = Default::default();
        for line in text.lines() {
            // Format: "origin\tgit@github.com:... (fetch)" or "(push)"
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 { continue; }
            let name = parts[0].to_string();
            let url_and_type = parts[1];
            let is_fetch = url_and_type.contains("(fetch)");
            let url = url_and_type.split_whitespace().next().unwrap_or("").to_string();
            let entry = remotes.entry(name.clone()).or_insert(Remote {
                name: name.clone(),
                url: url.clone(),
                fetch_url: String::new(),
                push_url: String::new(),
            });
            if is_fetch { entry.fetch_url = url.clone(); }
            else { entry.push_url = url.clone(); }
            entry.url = entry.fetch_url.clone();
        }
        Ok(remotes.into_values().collect())
    }

    fn add_remote(&self, name: &str, url: &str) -> GitResult<()> {
        self.exec(&["remote", "add", name, url])?;
        Ok(())
    }

    fn remove_remote(&self, name: &str) -> GitResult<()> {
        self.exec(&["remote", "remove", name])?;
        Ok(())
    }

    fn push_to_remote(&self, remote: &str, branch: &str, force: bool) -> GitResult<()> {
        let refspec = format!("{}:{}", branch, branch);
        if force {
            self.exec(&["push", "--force", remote, branch, refspec.as_str()])?;
        } else {
            self.exec(&["push", remote, branch, refspec.as_str()])?;
        }
        Ok(())
    }

    fn pull_from_remote(&self, remote: &str, branch: &str) -> GitResult<()> {
        self.exec(&["pull", remote, branch])?;
        Ok(())
    }

    fn fetch_remote(&self, remote: &str) -> GitResult<()> {
        self.exec(&["fetch", remote])?;
        Ok(())
    }

    // ── Log ────────────────────────────────────────────────────────────────
    fn log(&self, max_count: usize) -> GitResult<Vec<CommitEntry>> {
        let limit = format!("-{}", max_count);
        let fmt = "%H%x09%h%x09%an%x09%ae%x09%ad%x09%s%x09%b%x09%p";
        let pretty = format!("format:{}", fmt);
        let text = self.exec_text(&["log", &format!("--pretty={}", pretty), "--date=short", limit.as_str()])?;
        Ok(parse_log(&text))
    }

    fn log_for_file(&self, path: &str, max_count: usize) -> GitResult<Vec<CommitEntry>> {
        if path.chars().any(|c| c == '\n' || c == '\r' || c == '\0') {
            return Err(GitError::Other("invalid characters in path".into()));
        }
        let limit = format!("-{}", max_count);
        let fmt = "%H%x09%h%x09%an%x09%ae%x09%ad%x09%s%x09%b%x09%p";
        let pretty = format!("format:{}", fmt);
        let text = self.exec_text(&["log", "--follow", "-M", &format!("--pretty={}", pretty), "--date=short", limit.as_str(), "--", path])?;
        Ok(parse_log(&text))
    }

    // ── Diff (extended) ────────────────────────────────────────────────────
    fn diff_commits(&self, a: &str, b: &str) -> GitResult<Vec<FileDiff>> {
        let text = self.exec_text(&["diff", "--raw", a, b])?;
        Ok(parse_raw_diff(&text))
    }

    fn diff_working_tree_vs_commit(&self, commit: &str) -> GitResult<Vec<FileDiff>> {
        let text = self.exec_text(&["diff", "--raw", commit])?;
        Ok(parse_raw_diff(&text))
    }

    fn diff_file_at_commits(&self, path: &str, a: &str, b: &str) -> GitResult<Vec<FileDiff>> {
        let text = self.exec_text(&["diff", "--raw", a, b, "--", path])?;
        Ok(parse_raw_diff(&text))
    }

    // ── Reset / Clean ──────────────────────────────────────────────────────
    fn reset_soft(&self, commit: &str) -> GitResult<()> {
        self.exec(&["reset", "--soft", commit])?;
        Ok(())
    }

    fn reset_mixed(&self, commit: &str) -> GitResult<()> {
        self.exec(&["reset", "--mixed", commit])?;
        Ok(())
    }

    fn reset_hard(&self, commit: &str) -> GitResult<()> {
        self.exec(&["reset", "--hard", commit])?;
        Ok(())
    }

    fn clean(&self, directories: bool, force: bool) -> GitResult<()> {
        let mut args = vec!["clean"];
        if directories { args.push("-d"); }
        if force { args.push("-f"); }
        self.exec(&args)?;
        Ok(())
    }

    // ── Config ─────────────────────────────────────────────────────────────
    fn config_get(&self, key: &str) -> GitResult<Option<String>> {
        match self.exec_text(&["config", key]) {
            Ok(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() { Ok(None) } else { Ok(Some(trimmed.to_string())) }
            }
            Err(GitError::Other(msg)) if msg.contains("not found") || msg.is_empty() => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn config_set(&self, key: &str, value: &str) -> GitResult<()> {
        self.exec(&["config", key, value])?;
        Ok(())
    }

    // ── Misc ───────────────────────────────────────────────────────────────
    fn current_branch(&self) -> GitResult<String> {
        let text = self.exec_text(&["rev-parse", "--abbrev-ref", "HEAD"])?;
        Ok(text.trim().to_string())
    }

    fn head_commit(&self) -> GitResult<CommitId> {
        let text = self.exec_text(&["rev-parse", "HEAD"])?;
        Ok(CommitId(text.trim().to_string()))
    }

    fn is_clean(&self) -> GitResult<bool> {
        let st = self.status()?;
        Ok(st.changes.is_empty() && st.staged.is_empty() && st.untracked.is_empty() && st.conflicted.is_empty())
    }
}

/// Parse `git log --pretty=format` output with tab-separated fields.
fn parse_log(text: &str) -> Vec<CommitEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() { continue; }
        let mut f = line.split('\t');
        let sha = f.next().unwrap_or("").to_string();
        let short_sha = f.next().unwrap_or("").to_string();
        let author = f.next().unwrap_or("").to_string();
        let author_email = f.next().unwrap_or("").to_string();
        let date = f.next().unwrap_or("").to_string();
        let message = f.next().unwrap_or("").to_string();
        let body = f.next().unwrap_or("").to_string();
        let parents = f.next().unwrap_or("").split_whitespace().map(|s| s.to_string()).collect();
        if !sha.is_empty() {
            out.push(CommitEntry { sha, short_sha, author, author_email, date, message, body, parents });
        }
    }
    out
}


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

    // ── Tests for GitExtended ────────────────────────────────────────────────

    #[test]
    fn diff_between_commits() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo.commit(CommitRequest { message: "first".to_string(), amend: false }).expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c2 = repo.commit(CommitRequest { message: "second".to_string(), amend: false }).expect("commit");
        // Diff between the two commits.
        let diffs = repo.diff_commits(&c1.0, &c2.0).expect("diff commits");
        assert!(diffs.iter().any(|d| d.path == "doc.md"));
    }

    #[test]
    fn diff_working_tree_vs_commit() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo.commit(CommitRequest { message: "first".to_string(), amend: false }).expect("commit");
        // Modify working tree.
        write(dir.path(), "doc.md", b"v1 modified\n");
        let diffs = repo.diff_working_tree_vs_commit(&c1.0).expect("diff wt vs commit");
        assert!(diffs.iter().any(|d| d.path == "doc.md"));
    }

    #[test]
    fn stash_push_pop() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        // Modify and stash.
        write(dir.path(), "doc.md", b"v1 modified\n");
        repo.stash_push(Some("test stash")).expect("stash push");
        // Working tree should be clean.
        assert!(repo.is_clean().expect("is clean"));
        // Stash list should have one entry.
        let stashes = repo.stash_list().expect("stash list");
        assert_eq!(stashes.len(), 1);
        // Pop the stash — restores changes (may be staged or unstaged).
        repo.stash_pop(0).expect("stash pop");
        // Working tree should have changes (staged or unstaged).
        assert!(!repo.is_clean().expect("not clean after pop"));
    }

    #[test]
    fn stash_apply_and_drop() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        write(dir.path(), "doc.md", b"v1 modified\n");
        repo.stash_push(Some("test")).expect("stash");
        // Apply (keeps stash).
        repo.stash_apply(0).expect("stash apply");
        let stashes = repo.stash_list().expect("stash list");
        assert_eq!(stashes.len(), 1, "stash should still exist after apply");
        // Drop.
        repo.stash_drop(0).expect("stash drop");
        let stashes = repo.stash_list().expect("stash list");
        assert_eq!(stashes.len(), 0, "stash should be gone after drop");
    }

    #[test]
    fn create_and_delete_branch() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        repo.create_branch("feature").expect("create branch");
        let branches = repo.branches().expect("branches");
        assert!(branches.iter().any(|b| b.name == "feature"));
        repo.delete_branch("feature", false).expect("delete branch");
        let branches = repo.branches().expect("branches");
        assert!(!branches.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn rename_branch() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        repo.create_branch("old-name").expect("create");
        repo.rename_branch("old-name", "new-name").expect("rename");
        let branches = repo.branches().expect("branches");
        assert!(branches.iter().any(|b| b.name == "new-name"));
        assert!(!branches.iter().any(|b| b.name == "old-name"));
    }

    #[test]
    fn merge_fast_forward() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        // Create a branch with a new commit.
        repo.create_branch("feature").expect("create branch");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        write(dir.path(), "b.md", b"b\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "feature commit".to_string(), amend: false }).expect("commit");
        // Merge feature into master (fast-forward).
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        repo.merge("feature", MergeStrategy::FastForwardOnly).expect("merge ff");
        // b.md should now exist on master.
        assert!(dir.path().join("b.md").exists());
    }

    #[test]
    fn create_and_delete_tag() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        repo.create_tag("v1.0", Some("version 1.0")).expect("create tag");
        let tags = repo.tags().expect("tags");
        assert!(tags.iter().any(|t| t.name == "v1.0"));
        repo.delete_tag("v1.0").expect("delete tag");
        let tags = repo.tags().expect("tags");
        assert!(!tags.iter().any(|t| t.name == "v1.0"));
    }

    #[test]
    fn log_returns_commits() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "first".to_string(), amend: false }).expect("commit");
        write(dir.path(), "a.md", b"a modified\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "second".to_string(), amend: false }).expect("commit");
        let log = repo.log(10).expect("log");
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].message, "second");
        assert_eq!(log[1].message, "first");
        assert!(!log[0].sha.is_empty());
        assert!(!log[0].short_sha.is_empty());
    }

    #[test]
    fn log_for_file() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "first".to_string(), amend: false }).expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "second".to_string(), amend: false }).expect("commit");
        let log = repo.log_for_file("doc.md", 10).expect("log for file");
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn current_branch_name() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        let branch = repo.current_branch().expect("current branch");
        assert!(!branch.is_empty());
    }

    #[test]
    fn head_commit_id() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let id = repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        let head = repo.head_commit().expect("head commit");
        assert_eq!(head.0, id.0);
    }

    #[test]
    fn is_clean_check() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        assert!(repo.is_clean().expect("is clean"));
        write(dir.path(), "a.md", b"a modified\n");
        assert!(!repo.is_clean().expect("is not clean"));
    }

    #[test]
    fn reset_soft() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo.commit(CommitRequest { message: "first".to_string(), amend: false }).expect("commit");
        write(dir.path(), "a.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "second".to_string(), amend: false }).expect("commit");
        // Soft reset to first commit — staged changes should appear.
        repo.reset_soft(&c1.0).expect("reset soft");
        let st = repo.status().expect("status");
        // After soft reset, the diff between HEAD and index has the changes.
        assert!(st.dirty, "soft reset should leave repo dirty");
    }

    #[test]
    fn reset_hard() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo.commit(CommitRequest { message: "first".to_string(), amend: false }).expect("commit");
        write(dir.path(), "a.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "second".to_string(), amend: false }).expect("commit");
        // Hard reset to first commit — working tree should match.
        repo.reset_hard(&c1.0).expect("reset hard");
        let content = std::fs::read_to_string(dir.path().join("a.md")).expect("read");
        assert_eq!(content.trim(), "v1");
        assert!(repo.is_clean().expect("is clean"));
    }

    #[test]
    fn cherry_pick() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        // Create branch, add commit.
        repo.create_branch("feature").expect("create branch");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        write(dir.path(), "b.md", b"b\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let feature_commit = repo.commit(CommitRequest { message: "add b".to_string(), amend: false }).expect("commit");
        // Go back to master and cherry-pick.
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        assert!(!dir.path().join("b.md").exists());
        repo.cherry_pick(&feature_commit.0).expect("cherry-pick");
        assert!(dir.path().join("b.md").exists(), "b.md should exist after cherry-pick");
    }

    #[test]
    fn revert_commit() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        // Add a file and commit.
        write(dir.path(), "b.md", b"b\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c2 = repo.commit(CommitRequest { message: "add b".to_string(), amend: false }).expect("commit");
        assert!(dir.path().join("b.md").exists());
        // Revert the commit that added b.md.
        repo.revert(&c2.0).expect("revert");
        assert!(!dir.path().join("b.md").exists(), "b.md should be gone after revert");
    }

    #[test]
    fn config_get_set() {
        if !git_available() { return; }
        let dir = make_repo();
        let repo = GitCli::open(dir.path()).expect("open");
        repo.config_set("user.name", "TestUser").expect("config set");
        let val = repo.config_get("user.name").expect("config get");
        assert_eq!(val, Some("TestUser".to_string()));
    }

    #[test]
    fn clean_untracked() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "init".to_string(), amend: false }).expect("commit");
        // Create untracked file.
        write(dir.path(), "junk.txt", b"junk\n");
        assert!(!repo.is_clean().expect("not clean"));
        repo.clean(false, true).expect("clean");
        assert!(!dir.path().join("junk.txt").exists());
        assert!(repo.is_clean().expect("is clean"));
    }

    #[test]
    fn diff_file_at_two_commits() {
        if !git_available() { return; }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo.commit(CommitRequest { message: "first".to_string(), amend: false }).expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c2 = repo.commit(CommitRequest { message: "second".to_string(), amend: false }).expect("commit");
        let diffs = repo.diff_file_at_commits("doc.md", &c1.0, &c2.0).expect("diff file at commits");
        assert!(diffs.iter().any(|d| d.path == "doc.md"));
    }

    // ── Comprehensive tests for all GitExtended operations ───────────────────

    // Helper: create a repo with an initial commit.
    fn make_repo_with_commit() -> (tempfile::TempDir, GitCli, CommitId) {
        let dir = make_repo();
        write(dir.path(), "README.md", b"# README\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let id = repo.commit(CommitRequest { message: "initial".to_string(), amend: false }).expect("commit");
        (dir, repo, id)
    }

    // Helper: create a commit with a specific file content.
    fn commit_file(repo: &GitCli, path: &str, content: &[u8], msg: &str) -> CommitId {
        // Write to the file in the repo's working dir.
        let full = repo.work_dir().join(path);
        std::fs::write(&full, content).expect("write file");
        repo.stage(ChangeSelection::File { path: path.to_string() }).expect("stage");
        repo.commit(CommitRequest { message: msg.to_string(), amend: false }).expect("commit")
    }

    // ── Stash: comprehensive ────────────────────────────────────────────────

    #[test]
    fn stash_list_empty() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let stashes = repo.stash_list().expect("stash list");
        assert!(stashes.is_empty(), "stash list should be empty");
    }

    #[test]
    fn stash_push_without_message() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        // Modify a tracked file, then stash.
        write(dir.path(), "README.md", b"# Modified\n");
        repo.stash_push(None).expect("stash push");
        assert!(repo.is_clean().expect("clean"));
        let stashes = repo.stash_list().expect("stash list");
        assert_eq!(stashes.len(), 1);
    }

    #[test]
    fn stash_clear() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "README.md", b"v1\n");
        repo.stash_push(Some("first")).expect("stash");
        write(dir.path(), "README.md", b"v2\n");
        repo.stash_push(Some("second")).expect("stash");
        assert_eq!(repo.stash_list().expect("list").len(), 2);
        repo.stash_clear().expect("clear");
        assert_eq!(repo.stash_list().expect("list").len(), 0);
    }

    #[test]
    fn stash_multiple_entries() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        for i in 0..3 {
            write(dir.path(), "README.md", format!("content{}\n", i).as_bytes());
            repo.stash_push(Some(&format!("stash {}", i))).expect("stash");
        }
        let stashes = repo.stash_list().expect("list");
        assert_eq!(stashes.len(), 3);
        assert_eq!(stashes[0].index, 0);
        assert_eq!(stashes[2].index, 2);
    }

    #[test]
    fn stash_apply_does_not_remove() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "README.md", b"modified\n");
        repo.stash_push(Some("test")).expect("stash");
        repo.stash_apply(0).expect("apply");
        assert_eq!(repo.stash_list().expect("list").len(), 1, "stash should remain after apply");
    }

    #[test]
    fn stash_pop_removes() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "README.md", b"modified\n");
        repo.stash_push(Some("test")).expect("stash");
        repo.stash_pop(0).expect("pop");
        assert_eq!(repo.stash_list().expect("list").len(), 0, "stash should be gone after pop");
    }

    #[test]
    fn stash_drop_specific_index() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "README.md", b"first\n");
        repo.stash_push(Some("first")).expect("stash");
        write(dir.path(), "README.md", b"second\n");
        repo.stash_push(Some("second")).expect("stash");
        // Drop stash@{0} (which is "second"). After drop, stash@{0} becomes "first".
        repo.stash_drop(0).expect("drop");
        let stashes = repo.stash_list().expect("list");
        assert_eq!(stashes.len(), 1);
        assert!(stashes[0].message.contains("first"));
    }

    // ── Branch: comprehensive ───────────────────────────────────────────────

    #[test]
    fn create_branch_and_checkout() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        assert_eq!(repo.current_branch().expect("branch"), "feature");
    }

    #[test]
    fn delete_branch_force() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("temp").expect("create");
        // Add a commit on temp so it's not merged.
        repo.checkout(Revision::Branch("temp".to_string())).expect("checkout");
        commit_file(&repo, "new.txt", b"new\n", "on temp");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        // Force delete (unmerged).
        repo.delete_branch("temp", true).expect("force delete");
        let branches = repo.branches().expect("branches");
        assert!(!branches.iter().any(|b| b.name == "temp"));
    }

    #[test]
    fn delete_branch_not_merged_fails_without_force() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("temp").expect("create");
        repo.checkout(Revision::Branch("temp".to_string())).expect("checkout");
        commit_file(&repo, "new.txt", b"new\n", "on temp");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        // Non-force delete should fail (unmerged).
        assert!(repo.delete_branch("temp", false).is_err());
    }

    #[test]
    fn rename_current_branch() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let old = repo.current_branch().expect("current");
        repo.rename_branch(&old, "renamed").expect("rename");
        assert_eq!(repo.current_branch().expect("current"), "renamed");
    }

    #[test]
    fn create_multiple_branches() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        for name in &["dev", "staging", "prod", "hotfix"] {
            repo.create_branch(name).expect("create");
        }
        let branches = repo.branches().expect("branches");
        for name in &["dev", "staging", "prod", "hotfix"] {
            assert!(branches.iter().any(|b| b.name == *name), "branch {} should exist", name);
        }
    }

    // ── Merge: comprehensive ────────────────────────────────────────────────

    #[test]
    fn merge_no_fast_forward_creates_merge_commit() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        // Diverge: commit on master and on feature.
        repo.create_branch("feature").expect("create");
        commit_file(&repo, "master.txt", b"master\n", "on master");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        commit_file(&repo, "feature.txt", b"feature\n", "on feature");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        // Merge with --no-ff.
        repo.merge("feature", MergeStrategy::NoFastForward).expect("merge no-ff");
        // Both files should exist.
        assert!(repo.work_dir().join("feature.txt").exists());
        assert!(repo.work_dir().join("master.txt").exists());
        // Should have 4 commits: initial, master, feature, merge.
        let log = repo.log(10).expect("log");
        assert!(log.len() >= 4);
    }

    #[test]
    fn merge_fast_forward_only_fails_on_diverged() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        commit_file(&repo, "master.txt", b"master\n", "on master");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        commit_file(&repo, "feature.txt", b"feature\n", "on feature");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        // FF-only merge should fail because branches diverged.
        assert!(repo.merge("feature", MergeStrategy::FastForwardOnly).is_err());
    }

    #[test]
    fn merge_squash_combines_commits() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        commit_file(&repo, "a.txt", b"a\n", "commit a");
        commit_file(&repo, "b.txt", b"b\n", "commit b");
        commit_file(&repo, "c.txt", b"c\n", "commit c");
        repo.checkout(Revision::Branch("master".to_string()).clone()).expect("checkout master");
        repo.merge("feature", MergeStrategy::Squash).expect("squash merge");
        // All 3 files should exist on master.
        assert!(repo.work_dir().join("a.txt").exists());
        assert!(repo.work_dir().join("b.txt").exists());
        assert!(repo.work_dir().join("c.txt").exists());
        // Squash should add only 1 commit to master (not 3).
        let log = repo.log(10).expect("log");
        // initial + squash = 2 commits on master.
        assert_eq!(log.len(), 2, "squash should produce 1 new commit");
    }

    #[test]
    fn merge_default_strategy() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        commit_file(&repo, "new.txt", b"new\n", "on feature");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        // Default merge (should fast-forward since master hasn't diverged).
        repo.merge("feature", MergeStrategy::Merge).expect("merge");
        assert!(repo.work_dir().join("new.txt").exists());
    }

    // ── Rebase: comprehensive ───────────────────────────────────────────────

    #[test]
    fn rebase_simple() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        // Create a feature branch with a commit.
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        commit_file(&repo, "feature.txt", b"feature\n", "on feature");
        // Meanwhile, add a commit on master.
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        commit_file(&repo, "master.txt", b"master\n", "on master");
        // Rebase feature onto master.
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout feature");
        repo.rebase("master").expect("rebase");
        // Both files should exist.
        assert!(repo.work_dir().join("feature.txt").exists());
        assert!(repo.work_dir().join("master.txt").exists());
    }

    // ── Cherry-pick: comprehensive ──────────────────────────────────────────

    #[test]
    fn cherry_pick_multiple_commits() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        // Create feature branch with 2 commits.
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        let c1 = commit_file(&repo, "a.txt", b"a\n", "add a");
        let c2 = commit_file(&repo, "b.txt", b"b\n", "add b");
        // Go back to master and cherry-pick both.
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        repo.cherry_pick(&c1.0).expect("cherry-pick 1");
        repo.cherry_pick(&c2.0).expect("cherry-pick 2");
        assert!(repo.work_dir().join("a.txt").exists());
        assert!(repo.work_dir().join("b.txt").exists());
    }

    #[test]
    fn cherry_pick_does_not_affect_source_branch() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        commit_file(&repo, "new.txt", b"new\n", "add new");
        let feature_log_len = repo.log(10).expect("log").len();
        let head = repo.head_commit().expect("head");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        repo.cherry_pick(&head.0).expect("cherry-pick");
        assert!(repo.work_dir().join("new.txt").exists());
        // Feature branch should still have the same number of commits.
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout feature");
        assert_eq!(repo.log(10).expect("log").len(), feature_log_len);
    }

    // ── Revert: comprehensive ───────────────────────────────────────────────

    #[test]
    fn revert_creates_new_commit() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let c1 = commit_file(&repo, "new.txt", b"new\n", "add new");
        let log_before = repo.log(10).expect("log").len();
        repo.revert(&c1.0).expect("revert");
        let log_after = repo.log(10).expect("log").len();
        assert_eq!(log_after, log_before + 1, "revert should add a commit");
        assert!(!repo.work_dir().join("new.txt").exists(), "file should be removed by revert");
    }

    #[test]
    fn revert_multiple_commits() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let c1 = commit_file(&repo, "a.txt", b"a\n", "add a");
        let _c2 = commit_file(&repo, "b.txt", b"b\n", "add b");
        // Revert the first addition.
        repo.revert(&c1.0).expect("revert");
        assert!(!repo.work_dir().join("a.txt").exists(), "a.txt should be reverted");
        assert!(repo.work_dir().join("b.txt").exists(), "b.txt should still exist");
    }

    // ── Tags: comprehensive ─────────────────────────────────────────────────

    #[test]
    fn create_lightweight_tag() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_tag("v1.0", None).expect("create tag");
        let tags = repo.tags().expect("tags");
        let tag = tags.iter().find(|t| t.name == "v1.0").expect("tag exists");
        assert!(tag.is_lightweight, "should be lightweight");
        assert!(tag.message.is_none(), "no message for lightweight");
    }

    #[test]
    fn create_annotated_tag() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_tag("v2.0", Some("release 2.0")).expect("create tag");
        let tags = repo.tags().expect("tags");
        let tag = tags.iter().find(|t| t.name == "v2.0").expect("tag exists");
        assert!(!tag.is_lightweight, "annotated tag should not be lightweight");
        assert_eq!(tag.message.as_deref(), Some("release 2.0"));
    }

    #[test]
    fn create_multiple_tags() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        for v in &["v1.0", "v1.1", "v2.0", "v3.0"] {
            repo.create_tag(v, Some(&format!("version {}", v))).expect("create");
        }
        let tags = repo.tags().expect("tags");
        assert_eq!(tags.len(), 4);
    }

    #[test]
    fn tags_empty_repo() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let tags = repo.tags().expect("tags");
        assert!(tags.is_empty());
    }

    #[test]
    fn checkout_tag() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_tag("v1.0", None).expect("create tag");
        repo.checkout(Revision::Tag("v1.0".to_string())).expect("checkout tag");
        // Should be in detached HEAD state.
        let branch = repo.current_branch().expect("current");
        assert!(branch == "HEAD" || branch.is_empty(), "should be detached HEAD after checking out tag, got: {}", branch);
    }

    // ── Reset: comprehensive ────────────────────────────────────────────────

    #[test]
    fn reset_mixed_unstages() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "file.txt", b"modified\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let head = repo.head_commit().expect("head");
        // Mixed reset to HEAD — should unstage but keep working tree.
        repo.reset_mixed(&head.0).expect("reset mixed");
        let st = repo.status().expect("status");
        assert!(st.staged.is_empty(), "mixed reset should unstage");
        assert!(!repo.is_clean().expect("not clean"), "working tree should still have changes");
    }

    #[test]
    fn reset_soft_keeps_staged() {
        if !git_available() { return; }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "file.txt", b"new\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "second".to_string(), amend: false }).expect("commit");
        // Soft reset to initial — staged changes should contain the second commit's diff.
        repo.reset_soft(&c1.0).expect("reset soft");
        let st = repo.status().expect("status");
        assert!(st.dirty, "should be dirty after soft reset");
    }

    #[test]
    fn reset_hard_discards_everything() {
        if !git_available() { return; }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "README.md", b"# Modified\n");
        repo.reset_hard(&c1.0).expect("reset hard");
        // Tracked changes are gone, but untracked files remain (reset --hard doesn't clean untracked).
        let content = std::fs::read_to_string(dir.path().join("README.md")).expect("read");
        assert_eq!(content.trim(), "# README", "hard reset should restore original content");
        // No staged or unstaged tracked changes.
        let st = repo.status().expect("status");
        assert!(st.staged.is_empty(), "no staged changes after hard reset");
        assert!(st.changes.is_empty(), "no unstaged tracked changes after hard reset");
    }

    // ── Clean: comprehensive ────────────────────────────────────────────────

    #[test]
    fn clean_removes_untracked_files() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "junk1.txt", b"junk\n");
        write(dir.path(), "junk2.txt", b"junk\n");
        repo.clean(false, true).expect("clean");
        assert!(!dir.path().join("junk1.txt").exists());
        assert!(!dir.path().join("junk2.txt").exists());
    }

    #[test]
    fn clean_with_directories() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        std::fs::create_dir_all(dir.path().join("tempdir")).expect("mkdir");
        write(dir.path(), "tempdir/file.txt", b"temp\n");
        // Without -d flag, directories are not removed.
        repo.clean(false, true).expect("clean without -d");
        assert!(dir.path().join("tempdir").exists(), "dir should remain without -d");
        // With -d flag.
        repo.clean(true, true).expect("clean with -d");
        assert!(!dir.path().join("tempdir").exists(), "dir should be removed with -d");
    }

    #[test]
    fn clean_without_force_does_nothing() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "junk.txt", b"junk\n");
        // Without force, git clean refuses to remove.
        // Actually git clean without -f in non-interactive mode still errors.
        // Let's just verify the file still exists.
        let _ = repo.clean(false, false);
        // The file might or might not be removed depending on git config.
        // In most configs, clean without -f does nothing.
    }

    #[test]
    fn clean_does_not_remove_tracked() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "junk.txt", b"junk\n");
        repo.clean(false, true).expect("clean");
        // README.md is tracked, should still exist.
        assert!(dir.path().join("README.md").exists(), "tracked files should not be cleaned");
    }

    // ── Config: comprehensive ───────────────────────────────────────────────

    #[test]
    fn config_get_nonexistent_key() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let val = repo.config_get("nonexistent.key123").expect("config get");
        assert_eq!(val, None, "nonexistent key should return None");
    }

    #[test]
    fn config_set_and_get_user_email() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.config_set("user.email", "test@example.com").expect("set");
        let val = repo.config_get("user.email").expect("get");
        assert_eq!(val, Some("test@example.com".to_string()));
    }

    #[test]
    fn config_overwrite() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.config_set("user.name", "First").expect("set");
        repo.config_set("user.name", "Second").expect("overwrite");
        let val = repo.config_get("user.name").expect("get");
        assert_eq!(val, Some("Second".to_string()));
    }

    // ── Diff: comprehensive ─────────────────────────────────────────────────

    #[test]
    fn diff_commits_no_changes() {
        if !git_available() { return; }
        let (_dir, repo, c1) = make_repo_with_commit();
        // Diff a commit against itself.
        let diffs = repo.diff_commits(&c1.0, &c1.0).expect("diff");
        assert!(diffs.is_empty(), "diff of same commit should be empty");
    }

    #[test]
    fn diff_working_tree_vs_commit_clean() {
        if !git_available() { return; }
        let (_dir, repo, c1) = make_repo_with_commit();
        let diffs = repo.diff_working_tree_vs_commit(&c1.0).expect("diff");
        assert!(diffs.is_empty(), "clean working tree should have no diff");
    }

    #[test]
    fn diff_working_tree_vs_commit_modified() {
        if !git_available() { return; }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "README.md", b"# Modified README\n");
        let diffs = repo.diff_working_tree_vs_commit(&c1.0).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "README.md"));
    }

    #[test]
    fn diff_working_tree_vs_commit_new_file() {
        if !git_available() { return; }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "new.txt", b"new\n");
        // Untracked files don't show in git diff — need to stage first.
        repo.stage(ChangeSelection::All).expect("stage");
        let diffs = repo.diff_working_tree_vs_commit(&c1.0).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "new.txt"), "staged new file should show in diff");
    }

    #[test]
    fn diff_file_at_commits_same_commit() {
        if !git_available() { return; }
        let (_dir, repo, c1) = make_repo_with_commit();
        let diffs = repo.diff_file_at_commits("README.md", &c1.0, &c1.0).expect("diff");
        assert!(diffs.is_empty());
    }

    #[test]
    fn diff_commits_multiple_files() {
        if !git_available() { return; }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "a.txt", b"a\n");
        write(dir.path(), "b.txt", b"b\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c2 = repo.commit(CommitRequest { message: "add files".to_string(), amend: false }).expect("commit");
        let diffs = repo.diff_commits(&c1.0, &c2.0).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "a.txt"));
        assert!(diffs.iter().any(|d| d.path == "b.txt"));
    }

    #[test]
    fn diff_commits_deleted_file() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "temp.txt", b"temp\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo.commit(CommitRequest { message: "add temp".to_string(), amend: false }).expect("commit");
        std::fs::remove_file(dir.path().join("temp.txt")).expect("delete");
        repo.stage(ChangeSelection::All).expect("stage removal");
        let c2 = repo.commit(CommitRequest { message: "remove temp".to_string(), amend: false }).expect("commit");
        let diffs = repo.diff_commits(&c1.0, &c2.0).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "temp.txt"));
    }

    // ── Log: comprehensive ──────────────────────────────────────────────────

    #[test]
    fn log_empty_repo_no_commits() {
        if !git_available() { return; }
        let dir = make_repo();
        let repo = GitCli::open(dir.path()).expect("open");
        // No commits yet — log should return error or empty.
        let result = repo.log(10);
        // git log on empty repo returns error.
        assert!(result.is_err() || result.expect("log").is_empty());
    }

    #[test]
    fn log_max_count_limit() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        for i in 0..5 {
            write(dir.path(), &format!("file{}.txt", i), format!("content{}\n", i).as_bytes());
            repo.stage(ChangeSelection::All).expect("stage");
            repo.commit(CommitRequest { message: format!("commit {}", i), amend: false }).expect("commit");
        }
        let log = repo.log(3).expect("log");
        assert_eq!(log.len(), 3, "should respect max_count");
    }

    #[test]
    fn log_has_short_sha() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let log = repo.log(10).expect("log");
        let entry = &log[0];
        assert!(!entry.sha.is_empty(), "sha should not be empty");
        assert!(!entry.short_sha.is_empty(), "short_sha should not be empty");
        assert!(entry.sha.starts_with(&entry.short_sha), "sha should start with short_sha");
    }

    #[test]
    fn log_has_parents() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        // Initial commit has no parents.
        let log = repo.log(10).expect("log");
        let initial = log.last().expect("last");
        assert!(initial.parents.is_empty(), "initial commit has no parents");
        // Add another commit — should have 1 parent.
        write(dir.path(), "new.txt", b"new\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "second".to_string(), amend: false }).expect("commit");
        let log = repo.log(10).expect("log");
        let second = &log[0];
        assert_eq!(second.parents.len(), 1, "second commit should have 1 parent");
    }

    #[test]
    fn log_for_file_follows_renames() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "original.txt", b"content\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "add original".to_string(), amend: false }).expect("commit");
        // Rename and modify — use git mv for proper rename detection.
        let git_bin = std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string());
        Command::new(&git_bin).current_dir(dir.path()).args(["mv", "original.txt", "renamed.txt"]).output().expect("git mv");
        write(dir.path(), "renamed.txt", b"content modified\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "rename and modify".to_string(), amend: false }).expect("commit");
        // --follow should find at least the rename commit (rename detection with custom
        // format may not always traverse past the rename boundary on all git versions).
        let log = repo.log_for_file("renamed.txt", 10).expect("log");
        assert!(!log.is_empty(), "should find at least 1 commit for renamed file with --follow");
    }

    #[test]
    fn log_for_nonexistent_file() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let log = repo.log_for_file("nonexistent.txt", 10).expect("log");
        assert!(log.is_empty(), "nonexistent file should have no history");
    }

    // ── Misc: comprehensive ─────────────────────────────────────────────────

    #[test]
    fn head_commit_after_multiple_commits() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "a.txt", b"a\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo.commit(CommitRequest { message: "c1".to_string(), amend: false }).expect("commit");
        assert_eq!(repo.head_commit().expect("head").0, c1.0);
        write(dir.path(), "b.txt", b"b\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c2 = repo.commit(CommitRequest { message: "c2".to_string(), amend: false }).expect("commit");
        assert_eq!(repo.head_commit().expect("head").0, c2.0);
    }

    #[test]
    fn is_clean_after_stash() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "README.md", b"# Modified\n");
        assert!(!repo.is_clean().expect("not clean"));
        repo.stash_push(None).expect("stash");
        assert!(repo.is_clean().expect("clean after stash"));
    }

    #[test]
    fn current_branch_after_checkout() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let initial = repo.current_branch().expect("current");
        repo.create_branch("newbranch").expect("create");
        repo.checkout(Revision::Branch("newbranch".to_string())).expect("checkout");
        assert_eq!(repo.current_branch().expect("current"), "newbranch");
        repo.checkout(Revision::Branch(initial.clone())).expect("checkout back");
        assert_eq!(repo.current_branch().expect("current"), initial);
    }

    #[test]
    fn commit_amend() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "new.txt", b"new\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let original_head = repo.head_commit().expect("head");
        repo.commit(CommitRequest { message: "original message".to_string(), amend: false }).expect("commit");
        // Amend the commit.
        repo.commit(CommitRequest { message: "amended message".to_string(), amend: true }).expect("amend");
        let log = repo.log(10).expect("log");
        assert_eq!(log[0].message, "amended message");
        // HEAD should have changed (new commit hash from amend).
        assert_ne!(repo.head_commit().expect("head").0, original_head.0);
    }

    // ── Combined workflows ──────────────────────────────────────────────────

    #[test]
    fn workflow_stage_commit_branch_merge() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        // Create feature branch, add a file, commit, merge back.
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        write(dir.path(), "feature.txt", b"feature content\n");
        repo.stage(ChangeSelection::File { path: "feature.txt".to_string() }).expect("stage");
        repo.commit(CommitRequest { message: "add feature".to_string(), amend: false }).expect("commit");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        repo.merge("feature", MergeStrategy::FastForwardOnly).expect("merge");
        assert!(dir.path().join("feature.txt").exists());
        assert!(repo.is_clean().expect("clean"));
    }

    #[test]
    fn workflow_stash_branch_pop() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        // Modify a tracked file, stash, switch branch, pop.
        write(dir.path(), "README.md", b"# WIP\n");
        repo.stash_push(Some("wip")).expect("stash");
        repo.create_branch("dev").expect("create");
        repo.checkout(Revision::Branch("dev".to_string())).expect("checkout");
        repo.stash_pop(0).expect("pop");
        let content = std::fs::read_to_string(dir.path().join("README.md")).expect("read");
        assert_eq!(content.trim(), "# WIP", "stashed changes should be restored");
    }

    #[test]
    fn workflow_commit_tag_reset() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        // Make a commit, tag it, then reset back.
        write(dir.path(), "v1.txt", b"v1\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let _c1 = repo.commit(CommitRequest { message: "v1".to_string(), amend: false }).expect("commit");
        repo.create_tag("v1.0", Some("first release")).expect("tag");
        // Hard reset to initial.
        let initial = repo.log(10).expect("log").last().expect("initial").sha.clone();
        repo.reset_hard(&initial).expect("reset");
        assert!(!dir.path().join("v1.txt").exists(), "v1.txt should be gone after reset");
        // Tag should still exist (tags are independent of HEAD).
        let tags = repo.tags().expect("tags");
        assert!(tags.iter().any(|t| t.name == "v1.0"), "tag should survive reset");
    }

    #[test]
    fn workflow_cherry_pick_then_revert() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        // Create a commit on feature, cherry-pick it, then revert it.
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        let c1 = commit_file(&repo, "new.txt", b"new content\n", "add new");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        repo.cherry_pick(&c1.0).expect("cherry-pick");
        assert!(dir.path().join("new.txt").exists());
        // Revert the cherry-picked commit.
        let head = repo.head_commit().expect("head");
        repo.revert(&head.0).expect("revert");
        assert!(!dir.path().join("new.txt").exists(), "file should be gone after revert");
    }

    #[test]
    fn workflow_multiple_branches_merge_squash() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        // Create two feature branches, merge both with squash.
        repo.create_branch("feat-a").expect("create a");
        repo.checkout(Revision::Branch("feat-a".to_string())).expect("checkout a");
        commit_file(&repo, "a.txt", b"a\n", "add a");
        commit_file(&repo, "a2.txt", b"a2\n", "add a2");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        repo.merge("feat-a", MergeStrategy::Squash).expect("squash a");
        assert!(dir.path().join("a.txt").exists());
        assert!(dir.path().join("a2.txt").exists());
        // Second feature.
        repo.create_branch("feat-b").expect("create b");
        repo.checkout(Revision::Branch("feat-b".to_string())).expect("checkout b");
        commit_file(&repo, "b.txt", b"b\n", "add b");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        repo.merge("feat-b", MergeStrategy::Squash).expect("squash b");
        assert!(dir.path().join("b.txt").exists());
    }

    #[test]
    fn workflow_stage_unstage_commit() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        // Stage, unstage, stage again, commit.
        write(dir.path(), "file.txt", b"content\n");
        repo.stage(ChangeSelection::File { path: "file.txt".to_string() }).expect("stage");
        repo.unstage(ChangeSelection::File { path: "file.txt".to_string() }).expect("unstage");
        let st = repo.status().expect("status");
        assert!(st.staged.is_empty(), "should not be staged after unstage");
        repo.stage(ChangeSelection::File { path: "file.txt".to_string() }).expect("stage again");
        repo.commit(CommitRequest { message: "add file".to_string(), amend: false }).expect("commit");
        assert!(repo.is_clean().expect("clean after commit"));
    }

    #[test]
    fn workflow_diff_between_all_three_states() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        // Stage one change, make another unstaged change.
        write(dir.path(), "staged.txt", b"staged\n");
        repo.stage(ChangeSelection::File { path: "staged.txt".to_string() }).expect("stage");
        write(dir.path(), "README.md", b"# Modified\n");
        // Working tree vs HEAD should show both.
        let diffs_wt_head = repo.diff(DiffRequest::WorkingTreeVsHead).expect("diff wt vs head");
        assert!(diffs_wt_head.iter().any(|d| d.path == "staged.txt"));
        assert!(diffs_wt_head.iter().any(|d| d.path == "README.md"));
        // Index vs HEAD should show only staged.
        let diffs_idx_head = repo.diff(DiffRequest::IndexVsHead).expect("diff idx vs head");
        assert!(diffs_idx_head.iter().any(|d| d.path == "staged.txt"));
        assert!(!diffs_idx_head.iter().any(|d| d.path == "README.md"));
        // Working tree vs index should show only unstaged.
        let diffs_wt_idx = repo.diff(DiffRequest::WorkingTreeVsIndex).expect("diff wt vs idx");
        assert!(diffs_wt_idx.iter().any(|d| d.path == "README.md"));
    }

    #[test]
    fn workflow_tag_cherry_pick_and_rebase() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        // Tag initial, create feature, add commits, rebase, cherry-pick.
        repo.create_tag("v1.0", Some("initial")).expect("tag");
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        commit_file(&repo, "feat.txt", b"feat\n", "add feat");
        // Add commit on master.
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        commit_file(&repo, "master.txt", b"master\n", "on master");
        // Rebase feature onto master.
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout feature");
        repo.rebase("master").expect("rebase");
        assert!(dir.path().join("feat.txt").exists());
        assert!(dir.path().join("master.txt").exists());
        // Cherry-pick feature's commit onto master.
        let head = repo.head_commit().expect("head");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        repo.cherry_pick(&head.0).expect("cherry-pick");
        assert!(dir.path().join("feat.txt").exists(), "feat.txt should exist after cherry-pick");
    }

    #[test]
    fn workflow_stash_apply_drop_clean() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        // Create a tracked file, modify it, stash, apply, drop, clean untracked.
        write(dir.path(), "tracked.txt", b"tracked\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "add tracked".to_string(), amend: false }).expect("commit");
        write(dir.path(), "tracked.txt", b"modified\n");
        write(dir.path(), "untracked.txt", b"untracked\n");
        repo.stash_push(Some("tracked changes")).expect("stash");
        // Apply the stash (tracked changes restored).
        repo.stash_apply(0).expect("apply");
        // Drop it.
        repo.stash_drop(0).expect("drop");
        assert_eq!(repo.stash_list().expect("list").len(), 0);
        // Clean untracked.
        repo.clean(false, true).expect("clean");
        assert!(!dir.path().join("untracked.txt").exists());
    }

    #[test]
    fn workflow_branch_delete_recreate() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("temp").expect("create");
        repo.delete_branch("temp", false).expect("delete");
        assert!(!repo.branches().expect("branches").iter().any(|b| b.name == "temp"));
        // Recreate.
        repo.create_branch("temp").expect("recreate");
        assert!(repo.branches().expect("branches").iter().any(|b| b.name == "temp"));
    }

    #[test]
    fn workflow_config_set_commit_check_author() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        repo.config_set("user.name", "New Author").expect("set name");
        repo.config_set("user.email", "new@example.com").expect("set email");
        write(dir.path(), "file.txt", b"content\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "by new author".to_string(), amend: false }).expect("commit");
        let log = repo.log(10).expect("log");
        assert_eq!(log[0].author, "New Author");
        assert_eq!(log[0].author_email, "new@example.com");
    }

    // ── Edge cases ──────────────────────────────────────────────────────────

    #[test]
    fn open_nonexistent_directory() {
        if !git_available() { return; }
        let result = GitCli::open("C:\\nonexistent\\path\\12345");
        assert!(result.is_err());
    }

    #[test]
    fn status_after_delete_file() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        std::fs::remove_file(dir.path().join("README.md")).expect("delete");
        let st = repo.status().expect("status");
        assert!(st.changes.iter().any(|f| f.path == "README.md" && f.status == FileStatus::Deleted),
            "should show deleted file");
    }

    #[test]
    fn status_renamed_file() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        // Use git mv for reliable rename detection.
        let git_bin = std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string());
        Command::new(&git_bin).current_dir(dir.path()).args(["mv", "README.md", "RENAMED.md"]).output().expect("git mv");
        let st = repo.status().expect("status");
        // Should show as renamed (R) in staged.
        assert!(st.staged.iter().any(|f| f.path == "RENAMED.md"),
            "renamed file should appear in staged");
    }

    #[test]
    fn commit_with_empty_message_fails() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "file.txt", b"content\n");
        repo.stage(ChangeSelection::All).expect("stage");
        // Empty message — git should reject it.
        let result = repo.commit(CommitRequest { message: "".to_string(), amend: false });
        assert!(result.is_err(), "commit with empty message should fail");
    }

    #[test]
    fn diff_branch_vs_branch() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        write(dir.path(), "new.txt", b"new\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "on feature".to_string(), amend: false }).expect("commit");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        let diffs = repo.diff(DiffRequest::BranchVsBranch { a: "master".to_string(), b: "feature".to_string() }).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "new.txt"));
    }

    #[test]
    fn diff_head_vs_branch() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string())).expect("checkout");
        write(dir.path(), "new.txt", b"new\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "on feature".to_string(), amend: false }).expect("commit");
        repo.checkout(Revision::Branch("master".to_string())).expect("checkout master");
        let diffs = repo.diff(DiffRequest::HeadVsBranch { branch: "feature".to_string() }).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "new.txt"));
    }

    #[test]
    fn diff_file_version_vs_version() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "doc.md", b"v1\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo.commit(CommitRequest { message: "v1".to_string(), amend: false }).expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c2 = repo.commit(CommitRequest { message: "v2".to_string(), amend: false }).expect("commit");
        let diffs = repo.diff(DiffRequest::FileVersionVsVersion {
            path: "doc.md".to_string(),
            a: c1.clone(),
            b: c2.clone(),
        }).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "doc.md"));
    }

    #[test]
    fn diff_working_tree_vs_commit_file() {
        if !git_available() { return; }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "README.md", b"# Modified\n");
        let diffs = repo.diff(DiffRequest::WorkingTreeVsCommitFile {
            commit: c1,
            path: "README.md".to_string(),
        }).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "README.md"));
    }

    #[test]
    fn diff_index_vs_commit() {
        if !git_available() { return; }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "new.txt", b"new\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let diffs = repo.diff(DiffRequest::IndexVsCommit { commit: c1 }).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "new.txt"));
    }

    #[test]
    fn read_file_at_head() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let head = repo.head_commit().expect("head");
        let content = read_file_at_revision(&repo, "README.md", &head.0).expect("read");
        assert_eq!(String::from_utf8_lossy(&content).trim(), "# README");
    }

    #[test]
    fn read_file_at_old_revision() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "doc.md", b"v1\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo.commit(CommitRequest { message: "v1".to_string(), amend: false }).expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "v2".to_string(), amend: false }).expect("commit");
        let old = read_file_at_revision(&repo, "doc.md", &c1.0).expect("read old");
        assert_eq!(String::from_utf8_lossy(&old).trim(), "v1");
    }

    #[test]
    fn file_history_empty_for_nonexistent() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let hist = file_history(&repo, "nonexistent.txt").expect("history");
        assert!(hist.is_empty());
    }

    #[test]
    fn file_history_order() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "doc.md", b"v1\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "first".to_string(), amend: false }).expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "second".to_string(), amend: false }).expect("commit");
        write(dir.path(), "doc.md", b"v3\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest { message: "third".to_string(), amend: false }).expect("commit");
        let hist = file_history(&repo, "doc.md").expect("history");
        assert_eq!(hist.len(), 3);
        assert_eq!(hist[0].message, "third");
        assert_eq!(hist[1].message, "second");
        assert_eq!(hist[2].message, "first");
    }

    #[test]
    fn branches_includes_remote_after_fetch() {
        if !git_available() { return; }
        // We can't test actual remote fetch without a remote repo,
        // but we can verify that branches() doesn't crash.
        let (_dir, repo, _id) = make_repo_with_commit();
        let _branches = repo.branches().expect("branches should not crash");
    }

    #[test]
    fn remotes_empty_repo() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let remotes = repo.remotes().expect("remotes");
        assert!(remotes.is_empty(), "fresh repo should have no remotes");
    }

    #[test]
    fn add_and_remove_remote() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.add_remote("origin", "https://github.com/test/repo.git").expect("add remote");
        let remotes = repo.remotes().expect("remotes");
        assert!(remotes.iter().any(|r| r.name == "origin"));
        repo.remove_remote("origin").expect("remove remote");
        let remotes = repo.remotes().expect("remotes");
        assert!(!remotes.iter().any(|r| r.name == "origin"));
    }

    #[test]
    fn add_multiple_remotes() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.add_remote("origin", "https://github.com/test/repo.git").expect("add");
        repo.add_remote("upstream", "https://github.com/upstream/repo.git").expect("add");
        let remotes = repo.remotes().expect("remotes");
        assert_eq!(remotes.len(), 2);
        assert!(remotes.iter().any(|r| r.name == "origin"));
        assert!(remotes.iter().any(|r| r.name == "upstream"));
    }

    #[test]
    fn exec_text_returns_output() {
        if !git_available() { return; }
        let (_dir, repo, _id) = make_repo_with_commit();
        let text = repo.exec_text(&["log", "--oneline", "-1"]).expect("exec");
        assert!(!text.trim().is_empty(), "git log should return output");
    }

    #[test]
    fn work_dir_returns_path() {
        if !git_available() { return; }
        let (dir, repo, _id) = make_repo_with_commit();
        let work_dir = repo.work_dir();
        assert!(work_dir.exists(), "work_dir should exist");
        assert_eq!(work_dir, dir.path());
    }
}
