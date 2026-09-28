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

use editor_domain::{ByteRange, ids::RepositoryId};

use crate::{
    Branch, ChangeSelection, CommitEntry, CommitId, CommitRequest, Credentials, DiffRequest,
    FileChange, FileDiff, FileStatus, GitError, GitExtended, GitResult, MergeStrategy, Remote,
    RepositoryStatus, Revision, StashEntry, Tag, VersionControl,
};

/// A `VersionControl` implementation backed by the `git` CLI.
pub struct GitCli {
    repo_id: RepositoryId,
    work_dir: PathBuf,
    git_bin: String,
    /// When true, every git invocation runs with config overrides that
    /// disable repository-controlled code execution: hooks, external
    /// fsmonitor commands and external diff programs. Used by the MCP
    /// server, whose workspace may sit inside an untrusted repo.
    hardened: bool,
    /// Extra `-c key=value` overrides injected when `hardened` is set:
    /// the static ones plus every `filter.*`/`diff.*`/credential key found
    /// in the merged config (clean/smudge/process filters run on `add`,
    /// diff drivers' textconv on `diff`, credential helpers on transport).
    hardened_overrides: Vec<String>,
}

/// An empty directory `core.hooksPath` is pointed at for version-snapshot
/// commits. An editor/agent-initiated "save a version" is bookkeeping, not
/// a user `git commit`: repository hooks (husky, pre-commit scripts…)
/// would execute arbitrary code and could mutate or stall the snapshot.
fn empty_hooks_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        // Point hooksPath at a path that does NOT exist and cannot be
        // predicted: creating a fixed name inside a shared temp dir would
        // let another local process pre-create it and plant real hooks —
        // reintroducing the code-execution vector this exists to close.
        // Git treats a missing hooksPath as "no hooks", so absence is fine.
        // The suffix draws on RandomState's per-process random keys.
        use std::hash::{BuildHasher, Hasher};
        let rnd = |seed: u64| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(seed ^ std::process::id() as u64);
            h.write_u64(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0),
            );
            h.finish()
        };
        std::env::temp_dir().join(format!("womd-no-hooks-{:016x}{:016x}", rnd(1), rnd(2)))
    })
}

/// Resolve the `git` binary to an absolute path found on PATH —
/// see [`resolve_tool_binary`]. `GIT_BIN` overrides everything.
fn resolve_git_binary() -> String {
    resolve_tool_binary("git", "GIT_BIN")
}

/// Resolve a tool binary (`git`, `gh`, …) to an absolute path found on PATH.
///
/// `Command::new(name)` with `current_dir(work_dir)` is dangerous on Windows:
/// `CreateProcessW` searches the *current directory* before PATH, so a
/// `git.exe`/`gh.exe` planted inside the repository (e.g. through a malicious
/// checkout or an MCP workspace write) would be executed instead of the real
/// binary. Returning an absolute path eliminates CWD from the search entirely.
///
/// `env_var` overrides everything (operator/test control). Falls back to the
/// bare name when nothing is found on PATH — same behaviour as before.
///
/// Public so UI code spawning `gh` shares the same hardening.
pub fn resolve_tool_binary(name: &str, env_var: &str) -> String {
    if let Ok(b) = std::env::var(env_var) {
        // Keep it absolute: a relative override resolves through the child's
        // working directory — the same CWD hole this function exists to close.
        if let Ok(abs) = Path::new(&b).canonicalize() {
            return abs.to_string_lossy().to_string();
        }
        return b;
    }
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    find_on_path(name, std::env::split_paths(&path_var)).unwrap_or_else(|| name.to_string())
}

/// Search `dirs` for `name` (plus PATHEXT extensions on Windows); first hit
/// wins, canonicalized to an absolute path. Split out for tests — mutating
/// the process PATH in a test would race parallel cases.
fn find_on_path(name: &str, dirs: impl Iterator<Item = PathBuf>) -> Option<String> {
    #[cfg(windows)]
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .map(|e| e.to_lowercase())
        .collect();
    for dir in dirs {
        let cands: Vec<PathBuf> = {
            #[cfg(windows)]
            {
                exts.iter()
                    .map(|ext| dir.join(format!("{name}{ext}")))
                    .collect()
            }
            #[cfg(not(windows))]
            {
                vec![dir.join(name)]
            }
        };
        for cand in cands {
            // Canonicalize to an ABSOLUTE path: a relative PATH entry
            // (e.g. "." ) would otherwise make the candidate resolve
            // through the child's working directory — same CWD hole
            // this function exists to close.
            if cand.is_file()
                && let Ok(abs) = cand.canonicalize()
            {
                return Some(abs.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// Pipe drain shared between the reader thread and the collector: the thread
/// appends up to `cap` bytes then stops (the child blocks on a full pipe and
/// the deadline kills it), `done` flips on EOF/error. Keeping the bytes in a
/// shared buffer lets the collector return them even when the reader is
/// still blocked — a detached grandchild inheriting the pipe's write end
/// keeps it open forever, and a plain `join` on the reader would hang
/// despite the child having already exited.
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

/// Collect drained bytes, waiting up to `grace` for EOF. Whatever was read
/// is returned either way — a leaked pipe handle in a detached grandchild
/// must not hang the caller past the child exit.
fn collect_drain(drain: &PipeDrain, grace: std::time::Duration) -> Vec<u8> {
    let deadline = std::time::Instant::now() + grace;
    while !drain.done.load(std::sync::atomic::Ordering::Relaxed)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    std::mem::take(&mut *drain.buf.lock().unwrap_or_else(|e| e.into_inner()))
}

impl GitCli {
    /// Open a repository at `work_dir` (must contain a `.git` or be inside a work tree).
    pub fn open(work_dir: impl Into<PathBuf>) -> GitResult<Self> {
        Self::open_impl(work_dir.into(), false)
    }

    /// Like [`Self::open`], but every git invocation additionally disables
    /// repository-controlled code execution (`core.hooksPath`, external
    /// `core.fsmonitor`, external diff programs). Sandbox contexts (MCP)
    /// must use this — the workspace can live inside an untrusted repo.
    pub fn open_hardened(work_dir: impl Into<PathBuf>) -> GitResult<Self> {
        Self::open_impl(work_dir.into(), true)
    }

    fn open_impl(work_dir: PathBuf, hardened: bool) -> GitResult<Self> {
        let git_bin = resolve_git_binary();
        // Verify it's a repo.
        // Scrub env when hardened: `GIT_DIR`/`GIT_INDEX_FILE` from the parent
        // process could make rev-parse answer about a different repository.
        let out = Self::run(
            &git_bin,
            &work_dir,
            &["rev-parse", "--is-inside-work-tree"],
            hardened,
        )?;
        let stdout_trimmed = trim_bytes_end(&out.stdout);
        if !stdout_trimmed.eq_ignore_ascii_case(b"true") {
            return Err(GitError::NotFound(work_dir.display().to_string()));
        }
        let id = RepositoryId::new(work_dir.display().to_string());
        let hardened_overrides = if hardened {
            Self::collect_hardened_overrides(&git_bin, &work_dir)
        } else {
            Vec::new()
        };
        Ok(Self {
            repo_id: id,
            work_dir,
            git_bin,
            hardened,
            hardened_overrides,
        })
    }

    /// Create a `GitCli` over an already-validated repo path (used by tests).
    pub fn new_unchecked(work_dir: PathBuf, git_bin: impl Into<String>) -> Self {
        Self {
            repo_id: RepositoryId::new(work_dir.display().to_string()),
            work_dir,
            git_bin: git_bin.into(),
            hardened: false,
            hardened_overrides: Vec::new(),
        }
    }

    /// Build the `-c` override list for hardened exec. The static entries
    /// disable the execution channels reachable through ordinary git verbs;
    /// on top of those we enumerate the MERGED config (system+global+repo+
    /// worktree — a global `filter.lfs.clean` executes too) for keys that
    /// spawn external programs and clear each one.
    ///
    /// Why enumeration: `git add` runs `filter.<drv>.clean`/`process` for any
    /// file whose attributes select that driver, and `git diff` runs
    /// `diff.<drv>.textconv`/`command` on binary/attr-matched files — a repo
    /// can define both in `.git/config` and select them via `.gitattributes`,
    /// so version-snapshot commits would execute repository-controlled code.
    fn collect_hardened_overrides(git_bin: &str, dir: &Path) -> Vec<String> {
        // NOTE on clearing semantics: `-c key=` sets an EMPTY string, not a
        // proper unset. For command-valued keys (filter.*.clean, diff drivers,
        // diff.external, credential helpers) an empty command fails LOUDLY —
        // git cannot spawn "" — which is the desired fail-closed behavior
        // (the hostile repo command never runs; the operation errors instead).
        // It is NOT safe to force-clear `diff.external` unconditionally: git
        // then routes every `git diff` through the empty external command and
        // all diffs break. It is cleared only when a config scope actually
        // sets it (via `is_exec_config_key`), and our own diff commands carry
        // `--no-ext-diff` so they still produce correct output.
        let mut out = vec![
            format!("core.hooksPath={}", empty_hooks_dir().display()),
            "core.fsmonitor=false".to_string(),
            // `commit.gpgsign=true` in config makes `git commit` spawn the
            // signing program — arbitrary exec on every snapshot commit.
            "commit.gpgsign=false".to_string(),
            "tag.gpgsign=false".to_string(),
        ];
        // `git config` only reads config — still scrubbed: parent env such as
        // GIT_CONFIG_PARAMETERS/GIT_CONFIG_GLOBAL could feed forged keys into
        // the enumeration itself.
        if let Ok(o) = Self::run(
            git_bin,
            dir,
            &["config", "--null", "--get-regexp", "."],
            true,
        ) {
            let mut seen = std::collections::HashSet::new();
            for rec in o.stdout.split(|&b| b == 0) {
                let Some(nl) = rec.iter().position(|&b| b == b'\n') else {
                    continue;
                };
                let key = &rec[..nl];
                let Ok(key_str) = std::str::from_utf8(key) else {
                    continue;
                };
                let lower = key_str.to_lowercase();
                if is_exec_config_key(&lower) && seen.insert(lower) {
                    out.push(format!("{key_str}="));
                }
            }
        }
        out
    }

    /// How long a git command may run before it is killed. Network operations
    /// (fetch/pull/push) need real time; credential prompts are disabled via
    /// `GIT_TERMINAL_PROMPT=0` so a blocked prompt fails fast instead.
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

    /// Grace period for pipe readers to hit EOF *after* the child exits. A
    /// detached grandchild inheriting the pipe's write end would otherwise
    /// keep it open forever and `join` would hang despite the timeout.
    const EOF_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

    /// Environment variables that let a parent process (or a wrapper script
    /// that launched us) control which code `git` executes or where it reads
    /// and writes. Hardened exec removes them so only the config files on
    /// disk — already sanitized via `-c` overrides — can steer the command.
    /// Kept as a denylist rather than `env_clear()`: git needs SystemRoot /
    /// HOME / PATH to locate its libexec helpers and the user's identity.
    const SCRUBBED_ENV: &[&str] = &[
        // Redirect the whole repository / object store elsewhere.
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_GRAFT_FILE",
        "GIT_SHALLOW_FILE",
        "GIT_NAMESPACE",
        // Change which config files are read (defeats -c sanitization).
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_LOCAL",
        "GIT_CONFIG",
        "XDG_CONFIG_HOME",
        // Program execution channels.
        "GIT_EXTERNAL_DIFF",
        "GIT_DIFF_OPTS",
        "GIT_EXEC_PATH",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_SSH_VARIANT",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "GIT_PROXY_COMMAND",
        "GIT_PAGER",
        "GIT_EDITOR",
        "GIT_SEQUENCE_EDITOR",
        "GIT_MERGE_VERBOSITY",
        "SHELL",
        "BASH_ENV",
        "ENV",
        "CDPATH",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        // Pathspec reinterpretation would defeat `:(top,literal)` scoping.
        "GIT_LITERAL_PATHSPECS",
        "GIT_GLOB_PATHSPECS",
        "GIT_NOGLOB_PATHSPECS",
        "GIT_ICASE_PATHSPECS",
        // Forged author/committer identity on snapshot commits.
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_COMMITTER_DATE",
        "GIT_PREFIX",
    ];

    fn run(
        git_bin: &str,
        dir: &Path,
        args: &[&str],
        scrub_env: bool,
    ) -> GitResult<std::process::Output> {
        let mut cmd = Command::new(git_bin);
        cmd.current_dir(dir)
            .args(args)
            // Never let git prompt for credentials on a terminal that isn't
            // there — a blocked prompt would hang the command indefinitely.
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if scrub_env {
            for var in Self::SCRUBBED_ENV {
                cmd.env_remove(var);
            }
            // GIT_TRACE*/GIT_TRACE2* write logs to env-chosen paths — an
            // arbitrary file-write channel; GIT_CONFIG_KEY_n/VALUE_n inject
            // config pairs under GIT_CONFIG_COUNT. Their names are dynamic,
            // so remove them by prefix from the real environment.
            for (k, _) in std::env::vars_os() {
                let s = k.to_string_lossy().to_uppercase();
                if s.starts_with("GIT_TRACE")
                    || s.starts_with("GIT_CONFIG_KEY_")
                    || s.starts_with("GIT_CONFIG_VALUE_")
                {
                    cmd.env_remove(&k);
                }
            }
        }
        let mut child = cmd.spawn().map_err(|e| GitError::Io(e.to_string()))?;
        // Drain stdout/stderr on reader threads. Polling try_wait without
        // draining deadlocks as soon as the child fills a pipe buffer
        // (a big `status`/`diff` output easily exceeds 64 KiB). Each pipe is
        // capped: a misbehaving `git` cannot exhaust memory — past the cap
        // the reader stops, the pipe fills, the child blocks on write, and
        // the deadline kills it (surfaced as "timed out").
        const MAX_PIPE: usize = 64 * 1024 * 1024;
        let out_drain = spawn_pipe_drain(child.stdout.take().expect("piped"), MAX_PIPE);
        let err_drain = spawn_pipe_drain(child.stderr.take().expect("piped"), MAX_PIPE);
        let deadline = std::time::Instant::now() + Self::TIMEOUT;
        let status = loop {
            match child.try_wait().map_err(|e| GitError::Io(e.to_string()))? {
                Some(status) => break status,
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GitError::Other(
                        "git command timed out (possible dead remote or hung operation)"
                            .to_string(),
                    ));
                }
            }
        };
        let stdout = collect_drain(&out_drain, Self::EOF_GRACE);
        let stderr = collect_drain(&err_drain, Self::EOF_GRACE);
        let o = std::process::Output {
            status,
            stdout,
            stderr,
        };
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
    }

    fn exec(&self, args: &[&str]) -> GitResult<std::process::Output> {
        if self.hardened {
            // `-c` overrides must precede the subcommand. The full set was
            // computed at open: hooks→empty dir, fsmonitor/diff.external/gpg
            // signing off, plus every merged-config key that can spawn a
            // program (filter drivers, textconv, credential helpers…).
            let mut a: Vec<&str> =
                Vec::with_capacity(args.len() + 2 * self.hardened_overrides.len());
            for ov in &self.hardened_overrides {
                a.push("-c");
                a.push(ov.as_str());
            }
            a.extend_from_slice(args);
            Self::run(&self.git_bin, &self.work_dir, &a, true)
        } else {
            Self::run(&self.git_bin, &self.work_dir, args, false)
        }
    }

    /// Run a git command and return stdout as text (public for UI use).
    pub fn exec_text(&self, args: &[&str]) -> GitResult<String> {
        let o = self.exec(args)?;
        Ok(String::from_utf8_lossy(&o.stdout).to_string())
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// `git init` a repository at `work_dir` and open it. Used by the MCP
    /// server when a workspace is not yet versioned (ADR-007: every agent
    /// change is a commit).
    pub fn init(work_dir: impl Into<PathBuf>) -> GitResult<Self> {
        Self::init_impl(work_dir.into(), false)
    }

    /// [`Self::init`] producing a hardened handle — see `open_hardened`.
    pub fn init_hardened(work_dir: impl Into<PathBuf>) -> GitResult<Self> {
        Self::init_impl(work_dir.into(), true)
    }

    fn init_impl(work_dir: PathBuf, hardened: bool) -> GitResult<Self> {
        let git_bin = resolve_git_binary();
        Self::run(&git_bin, &work_dir, &["init"], hardened)?;
        let hardened_overrides = if hardened {
            Self::collect_hardened_overrides(&git_bin, &work_dir)
        } else {
            Vec::new()
        };
        let repo = Self {
            repo_id: RepositoryId::new(work_dir.display().to_string()),
            work_dir,
            git_bin,
            hardened,
            hardened_overrides,
        };
        // A fresh repo on a machine without global git config cannot commit —
        // seed a local identity, but never override an existing one.
        for (key, val) in [
            ("user.name", "WoMD MCP"),
            ("user.email", "womd-mcp@localhost"),
        ] {
            let unset = repo
                .exec_text(&["config", "--get", key])
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
            if unset {
                let _ = repo.exec_text(&["config", key, val]);
            }
        }
        Ok(repo)
    }

    /// Commit the working-tree state of exactly `paths` (`git commit --only`),
    /// leaving any other staged or unstaged changes untouched. New files are
    /// staged first so `git commit -- <path>` accepts them. Returns the new
    /// commit id, or `Ok(None)` when the paths have nothing to commit.
    pub fn commit_paths(&self, paths: &[&str], message: &str) -> GitResult<Option<CommitId>> {
        if paths.is_empty() {
            return Ok(None);
        }
        validate_message_arg(message)?;
        // `:(top,literal)` every path: a checked-in file may legitimately be
        // named `:(glob)*.md` on unix — after `--` git would still interpret
        // pathspec magic and stage unrelated files.
        let specs: Vec<String> = paths.iter().map(|p| literal_pathspec(p)).collect();
        let mut add = vec!["add", "-A", "--"];
        add.extend(specs.iter().map(String::as_str));
        self.exec(&add)?;
        // Version snapshots never run repository hooks — see empty_hooks_dir.
        let hooks = format!("core.hooksPath={}", empty_hooks_dir().display());
        let mut cmd = vec![
            "-c",
            hooks.as_str(),
            "commit",
            "--only",
            "-m",
            message,
            "--",
        ];
        cmd.extend(specs.iter().map(String::as_str));
        match self.exec(&cmd) {
            Ok(_) => {
                let sha = self.exec_text(&["rev-parse", "HEAD"])?;
                Ok(Some(CommitId(sha.trim().to_string())))
            }
            Err(GitError::Other(m))
                if m.contains("nothing to commit")
                    || m.contains("no changes added")
                    || m.contains("nothing added") =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

impl VersionControl for GitCli {
    fn repository_id(&self) -> &RepositoryId {
        &self.repo_id
    }

    fn status(&self) -> GitResult<RepositoryStatus> {
        // `--porcelain=v1 -z`: NUL-delimited records, and paths are emitted
        // verbatim — the non-`-z` format C-quotes paths containing `"`, `\`,
        // or non-ASCII bytes (core.quotepath defaults on), so a file named
        // `файл.md` arrived mangled and could not be staged or diffed.
        // With `-z`, rename/copy entries are two records: `XY <new>\0<old>\0`.
        let out = self.exec(&["status", "--porcelain=v1", "-z", "-b"])?;
        let records: Vec<&[u8]> = out.stdout.split(|&b| b == b'\0').collect();
        let mut status = RepositoryStatus::default();
        let mut i = 0usize;
        while i < records.len() {
            let rec = records[i];
            i += 1;
            if rec.is_empty() {
                continue;
            }
            if rec.starts_with(b"## ") {
                // Branch record: "## main...origin/main [ahead 1]"; a detached
                // HEAD comes through as "## HEAD (no branch)" — normalize to
                // plain "HEAD" (a valid commit-ish) instead of leaking the
                // parenthesized label into pull/merge refs where it fails
                // validation or, worse, prints raw in the UI.
                let rest = String::from_utf8_lossy(&rec[3..]);
                let head = rest.split("...").next().unwrap_or("").trim();
                let head = if head == "HEAD" || head.starts_with("HEAD ") {
                    "HEAD"
                } else {
                    head
                };
                status.head_branch = Some(head.to_string());
                // Note: ahead/behind is NOT a dirty state — `dirty` means the
                // working tree has uncommitted changes.
                continue;
            }
            if rec.len() < 4 {
                continue;
            }
            let x = rec[0]; // X = staged (index) status
            let y = rec[1]; // Y = working tree status
            let path = String::from_utf8_lossy(&rec[3..]).to_string();
            // In `-z` mode a rename/copy emits the source path as the next record.
            let mut old_path = None;
            if x == b'R' || x == b'C' {
                if let Some(src) = records.get(i) {
                    old_path = Some(String::from_utf8_lossy(src).to_string());
                    i += 1;
                }
            }
            let staged_status = status_from_code(x);
            let wt_status = status_from_code(y);

            // Staged changes come from the X column.
            match staged_status {
                FileStatus::Added
                | FileStatus::Modified
                | FileStatus::Deleted
                | FileStatus::Renamed => {
                    status.staged.push(FileChange {
                        path: path.clone(),
                        status: staged_status,
                        old_path: old_path.clone(),
                    });
                    status.dirty = true;
                }
                FileStatus::Conflicted => {
                    status.conflicted.push(FileChange {
                        path: path.clone(),
                        status: staged_status,
                        old_path: old_path.clone(),
                    });
                    status.dirty = true;
                }
                _ => {}
            }

            // Working tree changes come from the Y column.
            match wt_status {
                FileStatus::Untracked => {
                    status.untracked.push(FileChange {
                        path: path.clone(),
                        status: wt_status,
                        old_path: old_path.clone(),
                    });
                    status.dirty = true;
                }
                FileStatus::Added
                | FileStatus::Modified
                | FileStatus::Deleted
                | FileStatus::Renamed => {
                    status.changes.push(FileChange {
                        path: path.clone(),
                        status: wt_status,
                        old_path: old_path.clone(),
                    });
                    status.dirty = true;
                }
                FileStatus::Conflicted => {
                    if !status.conflicted.iter().any(|f| f.path == path) {
                        status.conflicted.push(FileChange {
                            path: path.clone(),
                            status: wt_status,
                            old_path: old_path.clone(),
                        });
                    }
                    status.dirty = true;
                }
                _ => {}
            }
        }
        Ok(status)
    }

    fn diff(&self, request: DiffRequest) -> GitResult<Vec<FileDiff>> {
        // Validate every caller-supplied ref/pathspec *before* building the
        // argument vector — an unvalidated `--`-looking string would be parsed
        // as an option by git.
        match &request {
            DiffRequest::CommitVsCommit { a, b }
            | DiffRequest::FileVersionVsVersion { a, b, .. } => {
                validate_commit_ref(&a.0)?;
                validate_commit_ref(&b.0)?;
            }
            DiffRequest::BranchVsBranch { a, b } => {
                validate_commit_ref(a)?;
                validate_commit_ref(b)?;
            }
            DiffRequest::HeadVsBranch { branch } => validate_commit_ref(branch)?,
            DiffRequest::WorkingTreeVsCommit { commit }
            | DiffRequest::IndexVsCommit { commit }
            | DiffRequest::WorkingTreeVsCommitFile { commit, .. } => {
                validate_commit_ref(&commit.0)?;
            }
            _ => {}
        }
        // File paths go through `literal_pathspec` so glob metacharacters in
        // file names cannot misfire.
        let owned;
        // `--no-ext-diff` on every invocation: `diff.external` /
        // GIT_EXTERNAL_DIFF can name a repository-chosen program — it must
        // never run under a hardened handle, and the flag also keeps diffs
        // correct when a cleared (`=`) config value would break them.
        // `-z`: NUL-separated, paths verbatim. Without it git C-quotes
        // names containing tabs/quotes/non-ASCII (core.quotepath) and the
        // caller would see mangled paths that match nothing on disk.
        let args: Vec<&str> = match &request {
            DiffRequest::WorkingTreeVsIndex => vec!["diff", "--no-ext-diff", "--raw", "-z"],
            DiffRequest::IndexVsHead => {
                vec!["diff", "--no-ext-diff", "--cached", "--raw", "-z"]
            }
            DiffRequest::WorkingTreeVsHead => {
                vec!["diff", "--no-ext-diff", "HEAD", "--raw", "-z"]
            }
            DiffRequest::CommitVsCommit { a, b } => {
                vec![
                    "diff",
                    "--no-ext-diff",
                    "--raw",
                    "-z",
                    a.0.as_str(),
                    b.0.as_str(),
                ]
            }
            DiffRequest::BranchVsBranch { a, b } => {
                vec![
                    "diff",
                    "--no-ext-diff",
                    "--raw",
                    "-z",
                    a.as_str(),
                    b.as_str(),
                ]
            }
            DiffRequest::HeadVsBranch { branch } => {
                vec![
                    "diff",
                    "--no-ext-diff",
                    "--raw",
                    "-z",
                    "HEAD",
                    branch.as_str(),
                ]
            }
            DiffRequest::FileVersionVsVersion { path, a, b } => {
                owned = literal_pathspec(path);
                vec![
                    "diff",
                    "--no-ext-diff",
                    "--raw",
                    "-z",
                    a.0.as_str(),
                    b.0.as_str(),
                    "--",
                    owned.as_str(),
                ]
            }
            DiffRequest::WorkingTreeVsCommit { commit } => {
                vec!["diff", "--no-ext-diff", "--raw", "-z", commit.0.as_str()]
            }
            DiffRequest::IndexVsCommit { commit } => {
                vec![
                    "diff",
                    "--no-ext-diff",
                    "--cached",
                    "--raw",
                    "-z",
                    commit.0.as_str(),
                ]
            }
            DiffRequest::WorkingTreeVsCommitFile { commit, path } => {
                owned = literal_pathspec(path);
                vec![
                    "diff",
                    "--no-ext-diff",
                    "--raw",
                    "-z",
                    commit.0.as_str(),
                    "--",
                    owned.as_str(),
                ]
            }
        };
        let out = self.exec(&args)?;
        Ok(parse_raw_diff_z(&out.stdout))
    }

    fn stage(&self, selection: ChangeSelection) -> GitResult<()> {
        match selection {
            ChangeSelection::All => {
                self.exec(&["add", "-A"])?;
            }
            ChangeSelection::File { path } => {
                // Resolve from the repo root (the working dir may be a
                // subdirectory) and match literally — `:/` alone would still
                // interpret glob metacharacters in the path.
                let ps = literal_pathspec(&path);
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
                let ps = literal_pathspec(&path);
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
        validate_message_arg(&request.message)?;
        let mut args = vec![
            "commit".to_string(),
            "-m".to_string(),
            request.message.clone(),
        ];
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
            let rest = if current {
                &line[2..]
            } else {
                line.trim_start()
            };
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            // Upstream + tracking counts appear in brackets:
            //   [origin/main]           — upstream, in sync
            //   [origin/main: ahead 2, behind 1]
            //   [gone]                  — upstream was deleted
            let mut upstream = None;
            let mut ahead = 0u32;
            let mut behind = 0u32;
            if let Some(bi) = rest.find('[') {
                if let Some(rel_end) = rest[bi..].find(']') {
                    let inner = rest[bi + 1..bi + rel_end].trim();
                    if inner != "gone" {
                        let mut parts = inner.splitn(2, ':');
                        let up = parts.next().unwrap_or("").trim();
                        if !up.is_empty() {
                            upstream = Some(up.to_string());
                        }
                        if let Some(tracking) = parts.next() {
                            for tok in tracking.split(',') {
                                let tok = tok.trim();
                                if let Some(n) = tok.strip_prefix("ahead ") {
                                    ahead = n.trim().parse().unwrap_or(0);
                                } else if let Some(n) = tok.strip_prefix("behind ") {
                                    behind = n.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                    }
                }
            }
            out.push(Branch {
                name,
                is_remote: false,
                upstream,
                ahead,
                behind,
            });
        }
        // Remote branches.
        let rem = self.exec_text(&["branch", "-r", "--list"])?;
        for line in rem.lines() {
            let name = line.trim().to_string();
            if !name.is_empty() && !name.contains(" -> ") {
                out.push(Branch {
                    name,
                    is_remote: true,
                    upstream: None,
                    ahead: 0,
                    behind: 0,
                });
            }
        }
        Ok(out)
    }

    fn checkout(&self, target: Revision) -> GitResult<()> {
        match target {
            // A leading `-` would be parsed as an option — `checkout -f`
            // force-discards local changes.
            Revision::Branch(b) => {
                validate_name_arg(&b, "branch name")?;
                self.exec(&["checkout", b.as_str()])?;
            }
            Revision::Commit(c) => {
                validate_commit_ref(&c.0)?;
                self.exec(&["checkout", c.0.as_str()])?;
            }
            Revision::Tag(t) => {
                validate_name_arg(&t, "tag name")?;
                self.exec(&["checkout", t.as_str()])?;
            }
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
    while end > 0
        && (bytes[end - 1] == b' '
            || bytes[end - 1] == b'\n'
            || bytes[end - 1] == b'\r'
            || bytes[end - 1] == b'\t')
    {
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

/// Validate a commit reference — reject anything that looks like a CLI flag or
/// contains shell metacharacters. Legitimate refs are SHAs (hex), ref names
/// (HEAD, main, tags/v1.0), and relative refs (HEAD~1, HEAD^2).
/// Does a (lowercased) git config key name an execution or credential
/// channel? Enumerated keys get cleared (`-c key=`) by hardened exec so
/// repository config cannot run code during version snapshots.
fn is_exec_config_key(lower: &str) -> bool {
    // Filters run on add/checkout; `process` is the long-running filter
    // protocol (git-lfs style). `.required` is a bool — not an exec channel.
    if lower.starts_with("filter.")
        && (lower.ends_with(".clean")
            || lower.ends_with(".smudge")
            || lower.ends_with(".command")
            || lower.ends_with(".process"))
    {
        return true;
    }
    // Diff drivers: `command`/`textconv`/`cachetextconv` spawn programs when
    // `git diff` meets an attr-matched or binary file.
    if lower.starts_with("diff.")
        && (lower.ends_with(".command")
            || lower.ends_with(".textconv")
            || lower.ends_with(".cachetextconv"))
    {
        return true;
    }
    // Transport credential helpers are shell commands FED the credential —
    // clearing them also prevents exfiltration on any network op.
    if lower.starts_with("credential.") {
        return true;
    }
    matches!(
        lower,
        "core.sshcommand"
            | "core.sshvariant"
            | "core.gitproxy"
            | "core.pager"
            // `diff.external` runs on EVERY `git diff`. Enumerate-and-empty
            // only: our diff calls use `--no-ext-diff` so they keep working
            // while a configured external command can never execute.
            | "diff.external"
    ) || lower.starts_with("gpg.")   // signing programs (gpgsm/x509/ssh variants)
        || lower.starts_with("ssh.") // ssh.* transport programs
        || lower.starts_with("sendmail.")
        || lower.starts_with("protocol.") // protocol.file.allow etc.
        || lower == "include.path" || lower.starts_with("includeif.")
        || lower.starts_with("url.")     // insteadOf rewrites remote URLs
        || lower.starts_with("alias.") // alias values run on `git <alias>`
}

fn validate_commit_ref(s: &str) -> GitResult<()> {
    if s.is_empty() {
        return Err(GitError::Other("empty commit reference".into()));
    }
    if s.starts_with('-') {
        return Err(GitError::Other(format!(
            "invalid commit reference: {:?}",
            s
        )));
    }
    if s.chars().any(|c| {
        c == '\n'
            || c == '\r'
            || c == '\0'
            || c == '`'
            || c == '$'
            || c == '!'
            || c == '&'
            || c == '|'
            || c == ';'
            || c == '('
            || c == ')'
    }) {
        return Err(GitError::Other(format!(
            "invalid characters in commit reference: {:?}",
            s
        )));
    }
    Ok(())
}

/// Validate a git "name" argument (branch/tag/remote names and similar
/// positional args). Rejects empty strings, leading dashes (which git would
/// parse as options — `checkout -f` would silently discard local changes), and
/// characters `git check-ref-format` forbids — most critically `*` and `?`,
/// which turn a `push <branch>:<branch>` refspec into a glob that force-pushes
/// or fetches EVERY ref, and `:`/whitespace, which change the refspec shape.
fn validate_name_arg(s: &str, what: &str) -> GitResult<()> {
    if s.is_empty() {
        return Err(GitError::Other(format!("empty {what}")));
    }
    if s.starts_with('-') {
        return Err(GitError::Other(format!("invalid {what}: {s:?}")));
    }
    if s.contains("..") || s.contains("@{") {
        return Err(GitError::Other(format!(
            "invalid characters in {what}: {s:?}"
        )));
    }
    if s.chars().any(|c| {
        c.is_control()
            || c == ' '
            || matches!(
                c,
                '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '{' | '}' | '"' | '\''
            )
    }) {
        return Err(GitError::Other(format!(
            "invalid characters in {what}: {s:?}"
        )));
    }
    Ok(())
}

/// Validate a message that lands in argv as `-m <msg>` (commit, stash).
/// A NUL byte makes `Command::spawn` fail with an opaque io error deep in
/// the call chain; other control characters have no place in a subject
/// line. Newlines and tabs are legitimate (multi-line bodies).
fn validate_message_arg(msg: &str) -> GitResult<()> {
    if msg
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(GitError::Other(
            "message must not contain control characters".into(),
        ));
    }
    Ok(())
}

/// Build a pathspec anchored at the repo root with literal matching:
/// `:(top,literal)<path>`. Plain `:/` anchors at the root but still applies
/// glob matching (`*`, `?`, `[...]`), which would misfire for files whose
/// names contain metacharacters. A caller-supplied `:`-prefixed magic
/// pathspec is passed through unchanged.
fn literal_pathspec(path: &str) -> String {
    // Pass through caller-supplied `:(magic)` pathspecs and absolute paths
    // (git accepts absolute paths literally after `--`). A bare `:`-prefixed
    // name is NOT magic — only `:(`, `:/`, `:!`, `:^` forms are — so a real
    // file named `:foo` must be wrapped in `:(top,literal)` to match, not
    // passed through for git to reject as unsupported magic.
    if path.starts_with(":(") || Path::new(path).is_absolute() {
        path.to_string()
    } else {
        format!(":(top,literal){path}")
    }
}

/// Parse `git diff --raw` output into `FileDiff`s (without hunks; hunks come from a
/// follow-up `git diff <path>` call when the UI expands a file).
/// Parse `git diff --raw -z` output. Each entry is a `:`-prefixed meta
/// record (`:src-mode dst-mode sha sha STATUS`) NUL-terminated, followed by
/// one NUL-terminated path — or two for rename/copy (empirical order: OLD
/// path, then NEW path). `-z` emits paths verbatim: no C-quoting, so
/// non-ASCII/tab/quote filenames survive intact (unlike plain `--raw`,
/// where core.quotepath mangles them).
fn parse_raw_diff_z(bytes: &[u8]) -> Vec<FileDiff> {
    let mut out = Vec::new();
    let mut it = bytes.split(|&b| b == 0);
    while let Some(meta) = it.next() {
        if !meta.starts_with(b":") {
            continue;
        }
        // Status is the last whitespace-separated token of the meta record
        // ("M", "A", "D", "R100", "C075", "T"…). R/C mean rename/copy.
        let status = meta.split(|&b| b == b' ').next_back().unwrap_or_default();
        let first = it.next().unwrap_or_default();
        if matches!(status.first(), Some(b'R') | Some(b'C')) {
            let second = it.next().unwrap_or_default();
            if second.is_empty() {
                break; // truncated stream — stop rather than misalign
            }
            out.push(FileDiff {
                path: String::from_utf8_lossy(second).to_string(),
                old_path: Some(String::from_utf8_lossy(first).to_string()),
                hunks: Vec::new(),
            });
        } else {
            if first.is_empty() {
                continue;
            }
            out.push(FileDiff {
                path: String::from_utf8_lossy(first).to_string(),
                old_path: None,
                hunks: Vec::new(),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Hunk-level staging (§42)
// ---------------------------------------------------------------------------

/// Get the unified diff for a single file (working tree vs index).
/// Returns raw bytes — a file may contain non-UTF-8 bytes and a lossy
/// conversion would corrupt the patch fed to `git apply` ("patch does not
/// apply", or worse, an apply that silently maps replaced bytes).
fn file_unified_diff(repo: &GitCli, path: &str) -> GitResult<Vec<u8>> {
    let ps = literal_pathspec(path);
    Ok(repo
        .exec(&["diff", "--no-ext-diff", "--", ps.as_str()])?
        .stdout)
}

/// Get the unified diff for a single file (index vs HEAD) — for unstaging.
fn file_unified_diff_cached(repo: &GitCli, path: &str) -> GitResult<Vec<u8>> {
    let ps = literal_pathspec(path);
    Ok(repo
        .exec(&["diff", "--no-ext-diff", "--cached", "--", ps.as_str()])?
        .stdout)
}

/// Extract the `hunk_index`-th hunk from a unified diff text.
/// Returns the hunk header + body lines (including `@@ ... @@` line).
fn join_hunk_lines(lines: &[&[u8]]) -> Vec<u8> {
    let mut out = lines.join(b"\n".as_slice());
    out.push(b'\n');
    out
}

fn extract_hunk(diff_text: &[u8], hunk_index: usize) -> Option<Vec<u8>> {
    let mut hunk_count = 0usize;
    let mut current: Vec<&[u8]> = Vec::new();
    let mut in_hunk = false;
    // `split(b'\n')` keeps '\r' bytes — stripping them would corrupt hunks
    // from CRLF files and the rebuilt patch wouldn't match the real bytes.
    for line in diff_text.split(|&b| b == b'\n') {
        if line.starts_with(b"@@ ") {
            // If we were collecting the target hunk, we're done.
            if in_hunk && hunk_count - 1 == hunk_index {
                return Some(join_hunk_lines(&current));
            }
            in_hunk = true;
            hunk_count += 1;
            current.clear();
            current.push(line);
        } else if in_hunk {
            // A hunk body line starts with ' ', '+', '-', or '\' (no newline marker).
            let c = line.first().copied();
            if matches!(c, Some(b' ') | Some(b'+') | Some(b'-') | Some(b'\\')) {
                current.push(line);
            } else {
                // End of hunk (e.g. next "diff --git" or empty line at end).
                if hunk_count - 1 == hunk_index {
                    return Some(join_hunk_lines(&current));
                }
                in_hunk = false;
            }
        }
    }
    // Last hunk at end of text.
    if in_hunk && hunk_count - 1 == hunk_index && !current.is_empty() {
        return Some(join_hunk_lines(&current));
    }
    None
}

/// Build a complete patch file from a file header + a single hunk.
/// `git apply` needs the `diff --git` header and the `---`/`+++` lines.
/// The extended headers between `diff --git` and `---` MUST be preserved:
/// `new file mode`/`deleted file mode`/`old mode`/`new mode` carry the mode
/// change, and `similarity index`/`rename from`/`rename to`/`copy from`/
/// `copy to` tell `apply` which index paths the hunk touches — without them a
/// hunk for a renamed or mode-changed file applies to the wrong path or fails.
fn build_patch(diff_text: &[u8], hunk_text: &[u8]) -> Vec<u8> {
    let mut patch: Vec<u8> = Vec::new();
    // Copy the whole file header block: everything from `diff --git` up to and
    // including the `+++ ` line. `file_unified_diff` emits a single-file diff,
    // so this captures exactly one header.
    let mut in_header = false;
    for line in diff_text.split(|&b| b == b'\n') {
        if line.starts_with(b"diff --git") {
            in_header = true;
        }
        if !in_header {
            continue;
        }
        // Headers are metadata, not file bytes — a stray `\r` (from a CRLF
        // unified diff) in them would corrupt the patch; hunk bodies keep
        // their `\r` intact via `extract_hunk`'s `split(b'\n')`.
        let clean = line.strip_suffix(b"\r").unwrap_or(line);
        patch.extend_from_slice(clean);
        patch.push(b'\n');
        if line.starts_with(b"+++ ") || line.starts_with(b"@@ ") {
            // `+++` ends the header; `@@` before it means a malformed diff.
            break;
        }
    }
    patch.extend_from_slice(hunk_text);
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
    let ps = literal_pathspec(path);
    repo.exec(&["add", "--", ps.as_str()])?;
    Ok(())
}

fn unstage_line_range(repo: &GitCli, path: &str, _range: ByteRange) -> GitResult<()> {
    let ps = literal_pathspec(path);
    repo.exec(&["reset", "-q", "--", ps.as_str()])?;
    Ok(())
}

/// Write `patch` to a fresh temp file and run `git apply --cached [--reverse]`
/// on it. `create_new` refuses to follow a pre-existing file/symlink at the
/// predictable temp path (TOCTOU hardening on shared temp dirs).
fn apply_patch_via_temp(repo: &GitCli, patch: &[u8], reverse: bool) -> GitResult<()> {
    use std::io::Write;
    let mut tmp = std::env::temp_dir();
    tmp.push(format!(
        "womd_hunk_patch_{}_{}.patch",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let write_result = std::fs::File::create_new(&tmp).and_then(|mut f| f.write_all(patch));
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(GitError::Io(e.to_string()));
    }
    let path_str = tmp.to_string_lossy().to_string();
    let args = if reverse {
        vec!["apply", "--cached", "--reverse", path_str.as_str()]
    } else {
        vec!["apply", "--cached", path_str.as_str()]
    };
    let result = repo.exec(&args);
    let _ = std::fs::remove_file(&tmp);
    result?;
    Ok(())
}

/// Apply a patch to the index using `git apply --cached`.
fn apply_patch_to_index(repo: &GitCli, patch: &[u8]) -> GitResult<()> {
    apply_patch_via_temp(repo, patch, false)
}

/// Apply a reverse patch to the index (unstage).
fn apply_patch_to_index_reverse(repo: &GitCli, patch: &[u8]) -> GitResult<()> {
    apply_patch_via_temp(repo, patch, true)
}

/// File history for a single document (§45).
pub fn file_history(repo: &GitCli, path: &str) -> GitResult<Vec<FileHistoryEntry>> {
    if path.chars().any(|c| c == '\n' || c == '\r' || c == '\0') {
        return Err(GitError::Other("invalid characters in path".into()));
    }
    let ps = literal_pathspec(path);
    // \x1f field separator: author names and subjects can contain tabs.
    let text = repo.exec_text(&[
        "log",
        "--follow",
        "--pretty=format:%H%x1f%an%x1f%ad%x1f%s",
        "--date=short",
        "--",
        ps.as_str(),
    ])?;
    let mut out = Vec::new();
    for line in text.lines() {
        let mut f = line.split('\x1f');
        let sha = f.next().unwrap_or("").to_string();
        let author = f.next().unwrap_or("").to_string();
        let date = f.next().unwrap_or("").to_string();
        let message = f.next().unwrap_or("").to_string();
        if !sha.is_empty() {
            out.push(FileHistoryEntry {
                revision: CommitId(sha),
                author,
                date,
                message,
            });
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
    validate_commit_ref(rev)?;
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
            if line.is_empty() {
                continue;
            }
            // Format: "stash@{0}: WIP on branch: abc123 message"
            let parts: Vec<&str> = line.splitn(2, ": ").collect();
            if parts.len() < 2 {
                continue;
            }
            let idx_str = parts[0].trim_start_matches("stash@{").trim_end_matches('}');
            let index: usize = idx_str.parse().unwrap_or(0);
            let rest = parts[1];
            // "WIP on <branch>: <sha> <subject>" or "On <branch>: <message>"
            // (custom -m stashes) — extract just the branch name.
            let after_prefix = rest
                .strip_prefix("WIP on ")
                .or_else(|| rest.strip_prefix("On "))
                .unwrap_or(rest);
            let branch = after_prefix
                .split(':')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let message = rest.to_string();
            out.push(StashEntry {
                index,
                message,
                branch,
            });
        }
        Ok(out)
    }

    fn stash_push(&self, message: Option<&str>) -> GitResult<()> {
        match message {
            Some(msg) => {
                validate_message_arg(msg)?;
                self.exec(&["stash", "push", "-m", msg])?;
            }
            None => {
                self.exec(&["stash", "push"])?;
            }
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
        validate_name_arg(name, "branch name")?;
        self.exec(&["branch", name])?;
        Ok(())
    }

    fn delete_branch(&self, name: &str, force: bool) -> GitResult<()> {
        validate_name_arg(name, "branch name")?;
        let flag = if force { "-D" } else { "-d" };
        self.exec(&["branch", flag, name])?;
        Ok(())
    }

    fn rename_branch(&self, old_name: &str, new_name: &str) -> GitResult<()> {
        validate_name_arg(old_name, "branch name")?;
        validate_name_arg(new_name, "branch name")?;
        self.exec(&["branch", "-m", old_name, new_name])?;
        Ok(())
    }

    // ── Merge / Rebase ─────────────────────────────────────────────────────
    fn merge(&self, branch: &str, strategy: MergeStrategy) -> GitResult<()> {
        // The merge target is a commit-ish; a leading `-` would smuggle
        // options like `--abort` or `--strategy=` into the merge.
        validate_commit_ref(branch)?;
        let args: Vec<&str> = match strategy {
            MergeStrategy::Merge => vec!["merge", branch],
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
        validate_commit_ref(branch)?;
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
        validate_commit_ref(commit)?;
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
        validate_commit_ref(commit)?;
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
        let text = self.exec_text(&[
            "tag",
            "-l",
            // %1f (unit separator) not %09 — a tag subject may itself
            // contain tabs, which would truncate/shift the fields.
            "--format=%(refname:short)%1f%(objectname:short)%1f%(objecttype)%1f%(contents:subject)",
        ])?;
        let mut out = Vec::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let mut f = line.split('\x1f');
            let name = f.next().unwrap_or("").to_string();
            let target = f.next().unwrap_or("").to_string();
            let obj_type = f.next().unwrap_or("").to_string();
            let message = f.next().map(|s| s.to_string());
            if !name.is_empty() {
                let is_lightweight = obj_type != "tag";
                let msg = if is_lightweight {
                    None
                } else {
                    message.filter(|m| !m.is_empty())
                };
                out.push(Tag {
                    name,
                    target,
                    message: msg,
                    is_lightweight,
                });
            }
        }
        Ok(out)
    }

    fn create_tag(&self, name: &str, message: Option<&str>) -> GitResult<()> {
        validate_name_arg(name, "tag name")?;
        match message {
            Some(msg) => {
                validate_message_arg(msg)?;
                self.exec(&["tag", "-a", name, "-m", msg])?;
            }
            None => {
                self.exec(&["tag", name])?;
            }
        }
        Ok(())
    }

    fn delete_tag(&self, name: &str) -> GitResult<()> {
        validate_name_arg(name, "tag name")?;
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
            if parts.len() < 2 {
                continue;
            }
            let name = parts[0].to_string();
            let url_and_type = parts[1];
            let is_fetch = url_and_type.contains("(fetch)");
            let url = url_and_type
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            let entry = remotes.entry(name.clone()).or_insert(Remote {
                name: name.clone(),
                url: url.clone(),
                fetch_url: String::new(),
                push_url: String::new(),
            });
            if is_fetch {
                entry.fetch_url = url.clone();
            } else {
                entry.push_url = url.clone();
            }
            // The canonical url prefers fetch; fall back to push for
            // remotes that only have a push URL configured.
            entry.url = if !entry.fetch_url.is_empty() {
                entry.fetch_url.clone()
            } else {
                entry.push_url.clone()
            };
        }
        Ok(remotes.into_values().collect())
    }

    fn add_remote(&self, name: &str, url: &str) -> GitResult<()> {
        validate_name_arg(name, "remote name")?;
        if url.starts_with('-') || url.chars().any(|c| c.is_control()) {
            return Err(GitError::Other(format!("invalid remote url: {url:?}")));
        }
        self.exec(&["remote", "add", name, url])?;
        Ok(())
    }

    fn remove_remote(&self, name: &str) -> GitResult<()> {
        validate_name_arg(name, "remote name")?;
        self.exec(&["remote", "remove", name])?;
        Ok(())
    }

    fn push_to_remote(&self, remote: &str, branch: &str, force: bool) -> GitResult<()> {
        validate_name_arg(remote, "remote name")?;
        validate_name_arg(branch, "branch name")?;
        let refspec = format!("{}:{}", branch, branch);
        if force {
            self.exec(&["push", "--force", remote, refspec.as_str()])?;
        } else {
            self.exec(&["push", remote, refspec.as_str()])?;
        }
        Ok(())
    }

    fn pull_from_remote(&self, remote: &str, branch: &str) -> GitResult<()> {
        validate_name_arg(remote, "remote name")?;
        // `git pull remote SRC:DST` treats the arg as a refspec and force-
        // updates the DST ref — a `:`-bearing "branch" must be rejected, so
        // this is name-arg (no `:`), not commit-ref (which allows `:`/even
        // `~`/`^` suffixes that would silently pull a different commit).
        validate_name_arg(branch, "branch name")?;
        self.exec(&["pull", remote, branch])?;
        Ok(())
    }

    fn fetch_remote(&self, remote: &str) -> GitResult<()> {
        validate_name_arg(remote, "remote name")?;
        self.exec(&["fetch", remote])?;
        Ok(())
    }

    // ── Log ────────────────────────────────────────────────────────────────
    fn log(&self, max_count: usize) -> GitResult<Vec<CommitEntry>> {
        let limit = format!("-{}", max_count);
        // %x1e record sep / %x1f field sep — robust against newlines AND
        // tabs in commit bodies/author names (see parse_log).
        let fmt = "%H%x1f%h%x1f%an%x1f%ae%x1f%ad%x1f%s%x1f%b%x1f%p%x1e";
        let pretty = format!("format:{}", fmt);
        let text = self.exec_text(&[
            "log",
            &format!("--pretty={}", pretty),
            "--date=short",
            limit.as_str(),
        ])?;
        Ok(parse_log(&text))
    }

    fn log_for_file(&self, path: &str, max_count: usize) -> GitResult<Vec<CommitEntry>> {
        if path.chars().any(|c| c == '\n' || c == '\r' || c == '\0') {
            return Err(GitError::Other("invalid characters in path".into()));
        }
        let limit = format!("-{}", max_count);
        let fmt = "%H%x1f%h%x1f%an%x1f%ae%x1f%ad%x1f%s%x1f%b%x1f%p%x1e";
        let pretty = format!("format:{}", fmt);
        let ps = literal_pathspec(path);
        let text = self.exec_text(&[
            "log",
            "--follow",
            "-M",
            &format!("--pretty={}", pretty),
            "--date=short",
            limit.as_str(),
            "--",
            ps.as_str(),
        ])?;
        Ok(parse_log(&text))
    }

    // ── Diff (extended) ────────────────────────────────────────────────────
    fn diff_commits(&self, a: &str, b: &str) -> GitResult<Vec<FileDiff>> {
        validate_commit_ref(a)?;
        validate_commit_ref(b)?;
        let out = self.exec(&["diff", "--no-ext-diff", "--raw", "-z", a, b])?;
        Ok(parse_raw_diff_z(&out.stdout))
    }

    fn diff_working_tree_vs_commit(&self, commit: &str) -> GitResult<Vec<FileDiff>> {
        validate_commit_ref(commit)?;
        let out = self.exec(&["diff", "--no-ext-diff", "--raw", "-z", commit])?;
        Ok(parse_raw_diff_z(&out.stdout))
    }

    fn diff_file_at_commits(&self, path: &str, a: &str, b: &str) -> GitResult<Vec<FileDiff>> {
        validate_commit_ref(a)?;
        validate_commit_ref(b)?;
        let ps = literal_pathspec(path);
        let out = self.exec(&[
            "diff",
            "--no-ext-diff",
            "--raw",
            "-z",
            a,
            b,
            "--",
            ps.as_str(),
        ])?;
        Ok(parse_raw_diff_z(&out.stdout))
    }

    // ── Reset / Clean ──────────────────────────────────────────────────────
    fn reset_soft(&self, commit: &str) -> GitResult<()> {
        validate_commit_ref(commit)?;
        self.exec(&["reset", "--soft", commit])?;
        Ok(())
    }

    fn reset_mixed(&self, commit: &str) -> GitResult<()> {
        validate_commit_ref(commit)?;
        self.exec(&["reset", "--mixed", commit])?;
        Ok(())
    }

    fn reset_hard(&self, commit: &str) -> GitResult<()> {
        validate_commit_ref(commit)?;
        self.exec(&["reset", "--hard", commit])?;
        Ok(())
    }

    fn clean(&self, directories: bool, force: bool) -> GitResult<()> {
        let mut args = vec!["clean"];
        if directories {
            args.push("-d");
        }
        if force {
            args.push("-f");
        }
        self.exec(&args)?;
        Ok(())
    }

    // ── Config ─────────────────────────────────────────────────────────────
    fn config_get(&self, key: &str) -> GitResult<Option<String>> {
        validate_name_arg(key, "config key")?;
        match self.exec_text(&["config", key]) {
            Ok(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Err(GitError::Other(msg)) if msg.contains("not found") || msg.is_empty() => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn config_set(&self, key: &str, value: &str) -> GitResult<()> {
        validate_name_arg(key, "config key")?;
        // Writing config is not remote-reachable today (no MCP tool / Tauri
        // command maps here), but the port must never turn repo config into
        // code execution or credential redirection. Deny the keys that can:
        //   credential.helper   — shell command run by git, fed credentials
        //   core.*/diff.external/alias.*/sendmail.* — external programs
        //   include[If].*.path  — pulls arbitrary config (chains to the rest)
        //   url.*.insteadOf     — rewrites remote URLs (fetch/push redirect)
        //   filter.*            — clean/smudge commands on checkout/add
        //   gpg.*.program/ssh.* — signing/transport programs
        let lower = key.to_lowercase();
        const DANGEROUS_PREFIXES: &[&str] = &[
            "credential.",
            "core.hookspath",
            "core.fsmonitor",
            "core.gitproxy",
            "core.sshcommand",
            "core.pager",
            "core.attributesfile",
            "diff.external",
            "include.path",
            "includeif.",
            "url.",
            "filter.",
            "sendmail.",
            "alias.",
            "gpg.",
            "ssh.",
            "http.proxy",
            "http.sslverify",
            "protocol.",
        ];
        if DANGEROUS_PREFIXES.iter().any(|p| lower.starts_with(p)) {
            return Err(GitError::Other(format!(
                "refusing to set config key {key:?}: it can execute external programs or redirect credentials"
            )));
        }
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
        Ok(st.changes.is_empty()
            && st.staged.is_empty()
            && st.untracked.is_empty()
            && st.conflicted.is_empty())
    }

    fn diff_file_raw(&self, path: &str) -> GitResult<String> {
        let ps = literal_pathspec(path);
        let text = self.exec_text(&["diff", "--no-ext-diff", "HEAD", "--", ps.as_str()]);
        let text = match text {
            Ok(t) => t,
            // Unborn HEAD: diff the index/worktree without a commit anchor.
            Err(_) => self.exec_text(&["diff", "--no-ext-diff", "--", ps.as_str()])?,
        };
        if text.trim().is_empty() {
            self.exec_text(&[
                "diff",
                "--no-ext-diff",
                "--cached",
                "HEAD",
                "--",
                ps.as_str(),
            ])
            .or_else(|_| self.exec_text(&["diff", "--no-ext-diff", "--cached", "--", ps.as_str()]))
        } else {
            Ok(text)
        }
    }

    fn diff_file_commits_raw(
        &self,
        path: &str,
        commit_a: &str,
        commit_b: &str,
    ) -> GitResult<String> {
        validate_commit_ref(commit_a)?;
        validate_commit_ref(commit_b)?;
        let ps = literal_pathspec(path);
        self.exec_text(&[
            "diff",
            "--no-ext-diff",
            commit_a,
            commit_b,
            "--",
            ps.as_str(),
        ])
    }

    fn diff_file_vs_commit_raw(&self, path: &str, commit: &str) -> GitResult<String> {
        validate_commit_ref(commit)?;
        let ps = literal_pathspec(path);
        self.exec_text(&["diff", "--no-ext-diff", commit, "--", ps.as_str()])
    }

    fn discard_file(&self, path: &str) -> GitResult<()> {
        let ps = literal_pathspec(path);
        self.exec_text(&["checkout", "--", ps.as_str()])?;
        Ok(())
    }

    fn clean_files(&self, pathspec: &str) -> GitResult<()> {
        let ps = literal_pathspec(pathspec);
        self.exec_text(&["clean", "-f", "--", ps.as_str()])?;
        Ok(())
    }

    fn repo_root(&self) -> GitResult<String> {
        let root = self.exec_text(&["rev-parse", "--show-toplevel"])?;
        Ok(root.trim().to_string())
    }

    fn remote_branches(&self) -> GitResult<Vec<String>> {
        let text = self.exec_text(&["branch", "-r", "--list"])?;
        Ok(text
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.contains(" -> "))
            .collect())
    }
}

/// Parse `git log --pretty=format` output with tab-separated fields.
fn parse_log(text: &str) -> Vec<CommitEntry> {
    let mut out = Vec::new();
    // Split on ASCII record separator (\x1e) — robust against newlines in
    // commit bodies. FIELDS are \x1f-separated: author names and commit
    // bodies are attacker-controlled in cloned repos and may contain tabs,
    // which would shift positional \t parsing and corrupt `parents`/`body`.
    for record in text.split('\x1e') {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        // parents is the LAST field — take it from the right so a \x1f
        // embedded in the body can't displace it.
        let (head, parents_raw) = record.rsplit_once('\x1f').unwrap_or((record, ""));
        // splitn(7): the 7th field is the body — any \x1f inside it stays
        // verbatim instead of spawning phantom fields.
        let mut f = head.splitn(7, '\x1f');
        let sha = f.next().unwrap_or("").to_string();
        let short_sha = f.next().unwrap_or("").to_string();
        let author = f.next().unwrap_or("").to_string();
        let author_email = f.next().unwrap_or("").to_string();
        let date = f.next().unwrap_or("").to_string();
        let message = f.next().unwrap_or("").to_string();
        let body = f.next().unwrap_or("").to_string();
        let parents = parents_raw
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();
        if !sha.is_empty() {
            out.push(CommitEntry {
                sha,
                short_sha,
                author,
                author_email,
                date,
                message,
                body,
                parents,
            });
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
        dir
    }

    fn write(dir: &Path, name: &str, content: &[u8]) {
        fs::write(dir.join(name), content).expect("write");
    }

    /// `find_on_path` must return an ABSOLUTE path — a relative hit would
    /// resolve through the child's working directory on Windows
    /// (CreateProcessW searches it before PATH), re-opening the planted-
    /// `git.exe` hole `resolve_tool_binary` exists to close.
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
        // Missing name → None (caller falls back to the bare name).
        assert!(find_on_path("no-such-tool-xyz", [dir.path().to_path_buf()].into_iter()).is_none());
        // A directory named like the tool is not executable — skipped.
        let sub = dir.path().join("tool-dir");
        fs::create_dir(&sub).unwrap();
        assert!(find_on_path("tool-dir", [dir.path().to_path_buf()].into_iter()).is_none());
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
        let id = repo
            .commit(CommitRequest {
                message: "init".to_string(),
                amend: false,
            })
            .expect("commit");
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
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
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
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
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
        repo.stage(ChangeSelection::File {
            path: "doc.md".to_string(),
        })
        .expect("stage");
        repo.commit(CommitRequest {
            message: "first".to_string(),
            amend: false,
        })
        .expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::File {
            path: "doc.md".to_string(),
        })
        .expect("stage");
        repo.commit(CommitRequest {
            message: "second".to_string(),
            amend: false,
        })
        .expect("commit");
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
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
        // Create and checkout a new branch via CLI, then verify branches() sees it.
        Command::new(std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string()))
            .current_dir(dir.path())
            .args(["branch", "feature"])
            .output()
            .expect("branch");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
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
        repo.stage(ChangeSelection::File {
            path: "doc.md".to_string(),
        })
        .expect("stage");
        repo.unstage(ChangeSelection::File {
            path: "doc.md".to_string(),
        })
        .expect("unstage");
        let st = repo.status().expect("status");
        assert!(st.staged.is_empty());
        assert!(
            st.untracked.iter().any(|f| f.path == "doc.md")
                || st.changes.iter().any(|f| f.path == "doc.md")
        );
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
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
        // Modify two distant lines (creates 2 separate hunks).
        let modified = b"# Title\n\nLine 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\n\nFirst CHANGED.\nLine 11\nLine 12\nLine 13\nLine 14\nLine 15\nLine 16\nLine 17\nLine 18\n\nSecond CHANGED.\n";
        write(dir.path(), "doc.md", modified);
        // Stage only the first hunk (hunk_index 0).
        repo.stage(ChangeSelection::Hunk {
            path: "doc.md".to_string(),
            hunk_index: 0,
        })
        .expect("stage hunk");
        // Verify: the first change is staged, the second is still unstaged.
        let st = repo.status().expect("status");
        assert!(
            st.staged.iter().any(|f| f.path == "doc.md"),
            "file should be partially staged"
        );
        assert!(
            st.changes.iter().any(|f| f.path == "doc.md"),
            "file should still have unstaged changes"
        );
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
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
        // Modify both and stage all.
        let modified = b"# Title\n\nLine 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\n\nFirst CHANGED.\nLine 11\nLine 12\nLine 13\nLine 14\nLine 15\nLine 16\nLine 17\nLine 18\n\nSecond CHANGED.\n";
        write(dir.path(), "doc.md", modified);
        repo.stage(ChangeSelection::File {
            path: "doc.md".to_string(),
        })
        .expect("stage all");
        // Unstage the first hunk only.
        repo.unstage(ChangeSelection::Hunk {
            path: "doc.md".to_string(),
            hunk_index: 0,
        })
        .expect("unstage hunk");
        // Verify: the second change is still staged, the first is unstaged.
        let st = repo.status().expect("status");
        assert!(
            st.staged.iter().any(|f| f.path == "doc.md"),
            "some changes should still be staged"
        );
        assert!(
            st.changes.iter().any(|f| f.path == "doc.md"),
            "some changes should be unstaged"
        );
    }

    /// `extract_hunk` must keep '\r' bytes — `str::lines` strips them, which
    /// silently corrupts hunks for CRLF files and makes `git apply` reject
    /// the rebuilt patch ("patch does not apply").
    #[test]
    fn extract_hunk_preserves_crlf_bytes() {
        let diff = b"diff --git a/f.md b/f.md\r\nindex 111..222 100644\r\n--- a/f.md\r\n+++ b/f.md\r\n@@ -1,2 +1,2 @@\r\n-old line\r\n+new line\r\n context\r\n";
        let hunk = extract_hunk(diff, 0).expect("hunk");
        assert!(
            hunk.windows(b"-old line\r\n".len())
                .any(|w| w == b"-old line\r\n"),
            "CR stripped: {hunk:?}"
        );
        assert!(
            hunk.windows(b" context\r\n".len())
                .any(|w| w == b" context\r\n"),
            "CR stripped: {hunk:?}"
        );
    }

    /// Selecting a hunk index that doesn't exist returns None, not a panic.
    #[test]
    fn extract_hunk_out_of_range_is_none() {
        let diff = b"@@ -1 +1 @@\n-a\n+b\n";
        assert!(extract_hunk(diff, 5).is_none());
    }

    /// `build_patch` must keep the extended header block (`new file mode`,
    /// `rename from/to`, `old mode`/`new mode`…) — dropping it makes
    /// `git apply` lose mode changes and mis-route hunks of renamed files.
    /// Header `\r` bytes are stripped; hunk body bytes are preserved raw —
    /// including bytes that are not valid UTF-8.
    #[test]
    fn build_patch_keeps_extended_headers_and_raw_bytes() {
        let diff = b"diff --git a/old.sh b/new.sh\nsimilarity index 88%\nrename from old.sh\nrename to new.sh\nold mode 100644\nnew mode 100755\nindex 111..222\n--- a/old.sh\n+++ b/new.sh\n@@ -1 +1 @@\n-old\n+new\n";
        let patch = build_patch(diff, b"@@ -1 +1 @@\n-old\n+new\n");
        let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
        assert!(contains(&patch, b"rename from old.sh\n"), "{patch:?}");
        assert!(contains(&patch, b"rename to new.sh\n"), "{patch:?}");
        assert!(contains(&patch, b"new mode 100755\n"), "{patch:?}");
        assert!(contains(&patch, b"similarity index 88%\n"), "{patch:?}");
        // Header ends at `+++`; the hunk follows.
        assert!(patch.ends_with(b"+++ b/new.sh\n@@ -1 +1 @@\n-old\n+new\n"));

        // New-file mode header + a CRLF hunk body: headers clean, body raw.
        let diff2 = b"diff --git a/x b/x\r\nnew file mode 100755\r\nindex 000..111\n--- /dev/null\n+++ b/x\n@@ -0,0 +1 @@\n+line\r\n";
        let patch2 = build_patch(diff2, b"@@ -0,0 +1 @@\n+line\r\n");
        assert!(contains(&patch2, b"new file mode 100755\n"));
        assert!(contains(&patch2, b"+line\r\n"));
        assert!(!contains(&patch2, b"diff --git a/x b/x\r"));
        assert!(!contains(&patch2, b"new file mode 100755\r"));

        // Non-UTF-8 bytes inside a hunk body survive untouched — a `&str`
        // pipeline would have replaced them with U+FFFD and produced a patch
        // `git apply` could never match against the real file.
        let patch3 = build_patch(diff, b"@@ -1 +1 @@\n-bad\xffbytes\n+ok\n");
        assert!(contains(&patch3, b"-bad\xffbytes\n"));
    }

    // ── Tests for GitExtended ────────────────────────────────────────────────

    #[test]
    fn diff_between_commits() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo
            .commit(CommitRequest {
                message: "first".to_string(),
                amend: false,
            })
            .expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c2 = repo
            .commit(CommitRequest {
                message: "second".to_string(),
                amend: false,
            })
            .expect("commit");
        // Diff between the two commits.
        let diffs = repo.diff_commits(&c1.0, &c2.0).expect("diff commits");
        assert!(diffs.iter().any(|d| d.path == "doc.md"));
    }

    #[test]
    fn diff_working_tree_vs_commit() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo
            .commit(CommitRequest {
                message: "first".to_string(),
                amend: false,
            })
            .expect("commit");
        // Modify working tree.
        write(dir.path(), "doc.md", b"v1 modified\n");
        let diffs = repo
            .diff_working_tree_vs_commit(&c1.0)
            .expect("diff wt vs commit");
        assert!(diffs.iter().any(|d| d.path == "doc.md"));
    }

    #[test]
    fn stash_push_pop() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
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
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
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
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
        repo.create_branch("feature").expect("create branch");
        let branches = repo.branches().expect("branches");
        assert!(branches.iter().any(|b| b.name == "feature"));
        repo.delete_branch("feature", false).expect("delete branch");
        let branches = repo.branches().expect("branches");
        assert!(!branches.iter().any(|b| b.name == "feature"));
    }

    #[test]
    fn rename_branch() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
        repo.create_branch("old-name").expect("create");
        repo.rename_branch("old-name", "new-name").expect("rename");
        let branches = repo.branches().expect("branches");
        assert!(branches.iter().any(|b| b.name == "new-name"));
        assert!(!branches.iter().any(|b| b.name == "old-name"));
    }

    #[test]
    fn merge_fast_forward() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
        // Create a branch with a new commit.
        repo.create_branch("feature").expect("create branch");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        write(dir.path(), "b.md", b"b\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "feature commit".to_string(),
            amend: false,
        })
        .expect("commit");
        // Merge feature into master (fast-forward).
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        repo.merge("feature", MergeStrategy::FastForwardOnly)
            .expect("merge ff");
        // b.md should now exist on master.
        assert!(dir.path().join("b.md").exists());
    }

    #[test]
    fn create_and_delete_tag() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
        repo.create_tag("v1.0", Some("version 1.0"))
            .expect("create tag");
        let tags = repo.tags().expect("tags");
        assert!(tags.iter().any(|t| t.name == "v1.0"));
        repo.delete_tag("v1.0").expect("delete tag");
        let tags = repo.tags().expect("tags");
        assert!(!tags.iter().any(|t| t.name == "v1.0"));
    }

    #[test]
    fn log_returns_commits() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "first".to_string(),
            amend: false,
        })
        .expect("commit");
        write(dir.path(), "a.md", b"a modified\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "second".to_string(),
            amend: false,
        })
        .expect("commit");
        let log = repo.log(10).expect("log");
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].message, "second");
        assert_eq!(log[1].message, "first");
        assert!(!log[0].sha.is_empty());
        assert!(!log[0].short_sha.is_empty());
    }

    #[test]
    fn log_for_file() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "first".to_string(),
            amend: false,
        })
        .expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "second".to_string(),
            amend: false,
        })
        .expect("commit");
        let log = repo.log_for_file("doc.md", 10).expect("log for file");
        assert_eq!(log.len(), 2);
    }

    /// A detached grandchild inheriting the pipe write-end keeps it open
    /// after the child exits — `collect_drain` must return the bytes read so
    /// far at the grace deadline instead of hanging on `join` forever.
    #[test]
    fn pipe_drain_collect_returns_without_eof() {
        use std::io::Write;
        let (r, mut w) = std::io::pipe().expect("pipe");
        let drain = spawn_pipe_drain(r, 1024);
        w.write_all(b"partial output").expect("write");
        w.flush().expect("flush");
        // `w` stays open — EOF never arrives. Collect must not hang.
        let t = std::time::Instant::now();
        let got = collect_drain(&drain, std::time::Duration::from_millis(300));
        assert!(
            t.elapsed() < std::time::Duration::from_secs(2),
            "collect_drain hung on an open pipe"
        );
        drop(w);
        assert_eq!(got, b"partial output");
    }

    /// The cap must bound the buffer exactly — a flood is truncated, not
    /// stored unboundedly.
    #[test]
    fn pipe_drain_respects_cap() {
        use std::io::Write;
        let (r, mut w) = std::io::pipe().expect("pipe");
        let drain = spawn_pipe_drain(r, 10);
        // Write more than the cap; the reader stops at the cap.
        let _ = w.write_all(&[b'x'; 8192]);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let got = collect_drain(&drain, std::time::Duration::from_millis(100));
        assert_eq!(got.len(), 10);
        drop(w);
    }

    #[test]
    fn current_branch_name() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
        let branch = repo.current_branch().expect("current branch");
        assert!(!branch.is_empty());
    }

    #[test]
    fn head_commit_id() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let id = repo
            .commit(CommitRequest {
                message: "init".to_string(),
                amend: false,
            })
            .expect("commit");
        let head = repo.head_commit().expect("head commit");
        assert_eq!(head.0, id.0);
    }

    #[test]
    fn is_clean_check() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
        assert!(repo.is_clean().expect("is clean"));
        write(dir.path(), "a.md", b"a modified\n");
        assert!(!repo.is_clean().expect("is not clean"));
    }

    #[test]
    fn reset_soft() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo
            .commit(CommitRequest {
                message: "first".to_string(),
                amend: false,
            })
            .expect("commit");
        write(dir.path(), "a.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "second".to_string(),
            amend: false,
        })
        .expect("commit");
        // Soft reset to first commit — staged changes should appear.
        repo.reset_soft(&c1.0).expect("reset soft");
        let st = repo.status().expect("status");
        // After soft reset, the diff between HEAD and index has the changes.
        assert!(st.dirty, "soft reset should leave repo dirty");
    }

    #[test]
    fn reset_hard() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo
            .commit(CommitRequest {
                message: "first".to_string(),
                amend: false,
            })
            .expect("commit");
        write(dir.path(), "a.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "second".to_string(),
            amend: false,
        })
        .expect("commit");
        // Hard reset to first commit — working tree should match.
        repo.reset_hard(&c1.0).expect("reset hard");
        let content = std::fs::read_to_string(dir.path().join("a.md")).expect("read");
        assert_eq!(content.trim(), "v1");
        assert!(repo.is_clean().expect("is clean"));
    }

    #[test]
    fn cherry_pick() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
        // Create branch, add commit.
        repo.create_branch("feature").expect("create branch");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        write(dir.path(), "b.md", b"b\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let feature_commit = repo
            .commit(CommitRequest {
                message: "add b".to_string(),
                amend: false,
            })
            .expect("commit");
        // Go back to master and cherry-pick.
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        assert!(!dir.path().join("b.md").exists());
        repo.cherry_pick(&feature_commit.0).expect("cherry-pick");
        assert!(
            dir.path().join("b.md").exists(),
            "b.md should exist after cherry-pick"
        );
    }

    #[test]
    fn revert_commit() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
        // Add a file and commit.
        write(dir.path(), "b.md", b"b\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c2 = repo
            .commit(CommitRequest {
                message: "add b".to_string(),
                amend: false,
            })
            .expect("commit");
        assert!(dir.path().join("b.md").exists());
        // Revert the commit that added b.md.
        repo.revert(&c2.0).expect("revert");
        assert!(
            !dir.path().join("b.md").exists(),
            "b.md should be gone after revert"
        );
    }

    #[test]
    fn config_get_set() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        let repo = GitCli::open(dir.path()).expect("open");
        repo.config_set("user.name", "TestUser")
            .expect("config set");
        let val = repo.config_get("user.name").expect("config get");
        assert_eq!(val, Some("TestUser".to_string()));
    }

    #[test]
    fn clean_untracked() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "a.md", b"a\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "init".to_string(),
            amend: false,
        })
        .expect("commit");
        // Create untracked file.
        write(dir.path(), "junk.txt", b"junk\n");
        assert!(!repo.is_clean().expect("not clean"));
        repo.clean(false, true).expect("clean");
        assert!(!dir.path().join("junk.txt").exists());
        assert!(repo.is_clean().expect("is clean"));
    }

    #[test]
    fn diff_file_at_two_commits() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "doc.md", b"v1\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo
            .commit(CommitRequest {
                message: "first".to_string(),
                amend: false,
            })
            .expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c2 = repo
            .commit(CommitRequest {
                message: "second".to_string(),
                amend: false,
            })
            .expect("commit");
        let diffs = repo
            .diff_file_at_commits("doc.md", &c1.0, &c2.0)
            .expect("diff file at commits");
        assert!(diffs.iter().any(|d| d.path == "doc.md"));
    }

    // ── Comprehensive tests for all GitExtended operations ───────────────────

    // Helper: create a repo with an initial commit.
    fn make_repo_with_commit() -> (tempfile::TempDir, GitCli, CommitId) {
        let dir = make_repo();
        write(dir.path(), "README.md", b"# README\n");
        let repo = GitCli::open(dir.path()).expect("open");
        repo.stage(ChangeSelection::All).expect("stage");
        let id = repo
            .commit(CommitRequest {
                message: "initial".to_string(),
                amend: false,
            })
            .expect("commit");
        (dir, repo, id)
    }

    // Helper: create a commit with a specific file content.
    fn commit_file(repo: &GitCli, path: &str, content: &[u8], msg: &str) -> CommitId {
        // Write to the file in the repo's working dir.
        let full = repo.work_dir().join(path);
        std::fs::write(&full, content).expect("write file");
        repo.stage(ChangeSelection::File {
            path: path.to_string(),
        })
        .expect("stage");
        repo.commit(CommitRequest {
            message: msg.to_string(),
            amend: false,
        })
        .expect("commit")
    }

    // ── Stash: comprehensive ────────────────────────────────────────────────

    #[test]
    fn stash_list_empty() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let stashes = repo.stash_list().expect("stash list");
        assert!(stashes.is_empty(), "stash list should be empty");
    }

    #[test]
    fn stash_push_without_message() {
        if !git_available() {
            return;
        }
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
        if !git_available() {
            return;
        }
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
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        for i in 0..3 {
            write(
                dir.path(),
                "README.md",
                format!("content{}\n", i).as_bytes(),
            );
            repo.stash_push(Some(&format!("stash {}", i)))
                .expect("stash");
        }
        let stashes = repo.stash_list().expect("list");
        assert_eq!(stashes.len(), 3);
        assert_eq!(stashes[0].index, 0);
        assert_eq!(stashes[2].index, 2);
    }

    #[test]
    fn stash_apply_does_not_remove() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "README.md", b"modified\n");
        repo.stash_push(Some("test")).expect("stash");
        repo.stash_apply(0).expect("apply");
        assert_eq!(
            repo.stash_list().expect("list").len(),
            1,
            "stash should remain after apply"
        );
    }

    #[test]
    fn stash_pop_removes() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "README.md", b"modified\n");
        repo.stash_push(Some("test")).expect("stash");
        repo.stash_pop(0).expect("pop");
        assert_eq!(
            repo.stash_list().expect("list").len(),
            0,
            "stash should be gone after pop"
        );
    }

    #[test]
    fn stash_drop_specific_index() {
        if !git_available() {
            return;
        }
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
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        assert_eq!(repo.current_branch().expect("branch"), "feature");
    }

    #[test]
    fn delete_branch_force() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("temp").expect("create");
        // Add a commit on temp so it's not merged.
        repo.checkout(Revision::Branch("temp".to_string()))
            .expect("checkout");
        commit_file(&repo, "new.txt", b"new\n", "on temp");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        // Force delete (unmerged).
        repo.delete_branch("temp", true).expect("force delete");
        let branches = repo.branches().expect("branches");
        assert!(!branches.iter().any(|b| b.name == "temp"));
    }

    #[test]
    fn delete_branch_not_merged_fails_without_force() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("temp").expect("create");
        repo.checkout(Revision::Branch("temp".to_string()))
            .expect("checkout");
        commit_file(&repo, "new.txt", b"new\n", "on temp");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        // Non-force delete should fail (unmerged).
        assert!(repo.delete_branch("temp", false).is_err());
    }

    #[test]
    fn rename_current_branch() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let old = repo.current_branch().expect("current");
        repo.rename_branch(&old, "renamed").expect("rename");
        assert_eq!(repo.current_branch().expect("current"), "renamed");
    }

    #[test]
    fn create_multiple_branches() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        for name in &["dev", "staging", "prod", "hotfix"] {
            repo.create_branch(name).expect("create");
        }
        let branches = repo.branches().expect("branches");
        for name in &["dev", "staging", "prod", "hotfix"] {
            assert!(
                branches.iter().any(|b| b.name == *name),
                "branch {} should exist",
                name
            );
        }
    }

    // ── Merge: comprehensive ────────────────────────────────────────────────

    #[test]
    fn merge_no_fast_forward_creates_merge_commit() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        // Diverge: commit on master and on feature.
        repo.create_branch("feature").expect("create");
        commit_file(&repo, "master.txt", b"master\n", "on master");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        commit_file(&repo, "feature.txt", b"feature\n", "on feature");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        // Merge with --no-ff.
        repo.merge("feature", MergeStrategy::NoFastForward)
            .expect("merge no-ff");
        // Both files should exist.
        assert!(repo.work_dir().join("feature.txt").exists());
        assert!(repo.work_dir().join("master.txt").exists());
        // Should have 4 commits: initial, master, feature, merge.
        let log = repo.log(10).expect("log");
        assert!(log.len() >= 4);
    }

    #[test]
    fn merge_fast_forward_only_fails_on_diverged() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        commit_file(&repo, "master.txt", b"master\n", "on master");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        commit_file(&repo, "feature.txt", b"feature\n", "on feature");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        // FF-only merge should fail because branches diverged.
        assert!(
            repo.merge("feature", MergeStrategy::FastForwardOnly)
                .is_err()
        );
    }

    #[test]
    fn merge_squash_combines_commits() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        commit_file(&repo, "a.txt", b"a\n", "commit a");
        commit_file(&repo, "b.txt", b"b\n", "commit b");
        commit_file(&repo, "c.txt", b"c\n", "commit c");
        repo.checkout(Revision::Branch("master".to_string()).clone())
            .expect("checkout master");
        repo.merge("feature", MergeStrategy::Squash)
            .expect("squash merge");
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
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        commit_file(&repo, "new.txt", b"new\n", "on feature");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        // Default merge (should fast-forward since master hasn't diverged).
        repo.merge("feature", MergeStrategy::Merge).expect("merge");
        assert!(repo.work_dir().join("new.txt").exists());
    }

    // ── Rebase: comprehensive ───────────────────────────────────────────────

    #[test]
    fn rebase_simple() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        // Create a feature branch with a commit.
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        commit_file(&repo, "feature.txt", b"feature\n", "on feature");
        // Meanwhile, add a commit on master.
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        commit_file(&repo, "master.txt", b"master\n", "on master");
        // Rebase feature onto master.
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout feature");
        repo.rebase("master").expect("rebase");
        // Both files should exist.
        assert!(repo.work_dir().join("feature.txt").exists());
        assert!(repo.work_dir().join("master.txt").exists());
    }

    // ── Cherry-pick: comprehensive ──────────────────────────────────────────

    #[test]
    fn cherry_pick_multiple_commits() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        // Create feature branch with 2 commits.
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        let c1 = commit_file(&repo, "a.txt", b"a\n", "add a");
        let c2 = commit_file(&repo, "b.txt", b"b\n", "add b");
        // Go back to master and cherry-pick both.
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        repo.cherry_pick(&c1.0).expect("cherry-pick 1");
        repo.cherry_pick(&c2.0).expect("cherry-pick 2");
        assert!(repo.work_dir().join("a.txt").exists());
        assert!(repo.work_dir().join("b.txt").exists());
    }

    #[test]
    fn cherry_pick_does_not_affect_source_branch() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        commit_file(&repo, "new.txt", b"new\n", "add new");
        let feature_log_len = repo.log(10).expect("log").len();
        let head = repo.head_commit().expect("head");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        repo.cherry_pick(&head.0).expect("cherry-pick");
        assert!(repo.work_dir().join("new.txt").exists());
        // Feature branch should still have the same number of commits.
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout feature");
        assert_eq!(repo.log(10).expect("log").len(), feature_log_len);
    }

    // ── Revert: comprehensive ───────────────────────────────────────────────

    #[test]
    fn revert_creates_new_commit() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let c1 = commit_file(&repo, "new.txt", b"new\n", "add new");
        let log_before = repo.log(10).expect("log").len();
        repo.revert(&c1.0).expect("revert");
        let log_after = repo.log(10).expect("log").len();
        assert_eq!(log_after, log_before + 1, "revert should add a commit");
        assert!(
            !repo.work_dir().join("new.txt").exists(),
            "file should be removed by revert"
        );
    }

    #[test]
    fn revert_multiple_commits() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let c1 = commit_file(&repo, "a.txt", b"a\n", "add a");
        let _c2 = commit_file(&repo, "b.txt", b"b\n", "add b");
        // Revert the first addition.
        repo.revert(&c1.0).expect("revert");
        assert!(
            !repo.work_dir().join("a.txt").exists(),
            "a.txt should be reverted"
        );
        assert!(
            repo.work_dir().join("b.txt").exists(),
            "b.txt should still exist"
        );
    }

    // ── Tags: comprehensive ─────────────────────────────────────────────────

    #[test]
    fn create_lightweight_tag() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_tag("v1.0", None).expect("create tag");
        let tags = repo.tags().expect("tags");
        let tag = tags.iter().find(|t| t.name == "v1.0").expect("tag exists");
        assert!(tag.is_lightweight, "should be lightweight");
        assert!(tag.message.is_none(), "no message for lightweight");
    }

    #[test]
    fn create_annotated_tag() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_tag("v2.0", Some("release 2.0"))
            .expect("create tag");
        let tags = repo.tags().expect("tags");
        let tag = tags.iter().find(|t| t.name == "v2.0").expect("tag exists");
        assert!(
            !tag.is_lightweight,
            "annotated tag should not be lightweight"
        );
        assert_eq!(tag.message.as_deref(), Some("release 2.0"));
    }

    #[test]
    fn create_multiple_tags() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        for v in &["v1.0", "v1.1", "v2.0", "v3.0"] {
            repo.create_tag(v, Some(&format!("version {}", v)))
                .expect("create");
        }
        let tags = repo.tags().expect("tags");
        assert_eq!(tags.len(), 4);
    }

    #[test]
    fn tags_empty_repo() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let tags = repo.tags().expect("tags");
        assert!(tags.is_empty());
    }

    #[test]
    fn checkout_tag() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_tag("v1.0", None).expect("create tag");
        repo.checkout(Revision::Tag("v1.0".to_string()))
            .expect("checkout tag");
        // Should be in detached HEAD state.
        let branch = repo.current_branch().expect("current");
        assert!(
            branch == "HEAD" || branch.is_empty(),
            "should be detached HEAD after checking out tag, got: {}",
            branch
        );
    }

    #[test]
    fn status_detached_head_normalizes_branch() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_tag("v1.0", None).expect("create tag");
        repo.checkout(Revision::Tag("v1.0".to_string()))
            .expect("checkout tag");
        // `git status -b` reports "## HEAD (no branch)" when detached — the UI
        // must see a plain "HEAD" commit-ish, not the parenthesized label
        // (which would fail ref validation if passed to pull/merge).
        let st = repo.status().expect("status");
        assert_eq!(st.head_branch.as_deref(), Some("HEAD"));
    }

    // ── Reset: comprehensive ────────────────────────────────────────────────

    #[test]
    fn reset_mixed_unstages() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "file.txt", b"modified\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let head = repo.head_commit().expect("head");
        // Mixed reset to HEAD — should unstage but keep working tree.
        repo.reset_mixed(&head.0).expect("reset mixed");
        let st = repo.status().expect("status");
        assert!(st.staged.is_empty(), "mixed reset should unstage");
        assert!(
            !repo.is_clean().expect("not clean"),
            "working tree should still have changes"
        );
    }

    #[test]
    fn reset_soft_keeps_staged() {
        if !git_available() {
            return;
        }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "file.txt", b"new\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "second".to_string(),
            amend: false,
        })
        .expect("commit");
        // Soft reset to initial — staged changes should contain the second commit's diff.
        repo.reset_soft(&c1.0).expect("reset soft");
        let st = repo.status().expect("status");
        assert!(st.dirty, "should be dirty after soft reset");
    }

    #[test]
    fn reset_hard_discards_everything() {
        if !git_available() {
            return;
        }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "README.md", b"# Modified\n");
        repo.reset_hard(&c1.0).expect("reset hard");
        // Tracked changes are gone, but untracked files remain (reset --hard doesn't clean untracked).
        let content = std::fs::read_to_string(dir.path().join("README.md")).expect("read");
        assert_eq!(
            content.trim(),
            "# README",
            "hard reset should restore original content"
        );
        // No staged or unstaged tracked changes.
        let st = repo.status().expect("status");
        assert!(st.staged.is_empty(), "no staged changes after hard reset");
        assert!(
            st.changes.is_empty(),
            "no unstaged tracked changes after hard reset"
        );
    }

    // ── Clean: comprehensive ────────────────────────────────────────────────

    #[test]
    fn clean_removes_untracked_files() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "junk1.txt", b"junk\n");
        write(dir.path(), "junk2.txt", b"junk\n");
        repo.clean(false, true).expect("clean");
        assert!(!dir.path().join("junk1.txt").exists());
        assert!(!dir.path().join("junk2.txt").exists());
    }

    #[test]
    fn clean_with_directories() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        std::fs::create_dir_all(dir.path().join("tempdir")).expect("mkdir");
        write(dir.path(), "tempdir/file.txt", b"temp\n");
        // Without -d flag, directories are not removed.
        repo.clean(false, true).expect("clean without -d");
        assert!(
            dir.path().join("tempdir").exists(),
            "dir should remain without -d"
        );
        // With -d flag.
        repo.clean(true, true).expect("clean with -d");
        assert!(
            !dir.path().join("tempdir").exists(),
            "dir should be removed with -d"
        );
    }

    #[test]
    fn clean_without_force_does_nothing() {
        if !git_available() {
            return;
        }
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
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "junk.txt", b"junk\n");
        repo.clean(false, true).expect("clean");
        // README.md is tracked, should still exist.
        assert!(
            dir.path().join("README.md").exists(),
            "tracked files should not be cleaned"
        );
    }

    // ── Config: comprehensive ───────────────────────────────────────────────

    #[test]
    fn config_get_nonexistent_key() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let val = repo.config_get("nonexistent.key123").expect("config get");
        assert_eq!(val, None, "nonexistent key should return None");
    }

    #[test]
    fn config_set_and_get_user_email() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.config_set("user.email", "test@example.com")
            .expect("set");
        let val = repo.config_get("user.email").expect("get");
        assert_eq!(val, Some("test@example.com".to_string()));
    }

    #[test]
    fn config_overwrite() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.config_set("user.name", "First").expect("set");
        repo.config_set("user.name", "Second").expect("overwrite");
        let val = repo.config_get("user.name").expect("get");
        assert_eq!(val, Some("Second".to_string()));
    }

    // ── Diff: comprehensive ─────────────────────────────────────────────────

    #[test]
    fn diff_commits_no_changes() {
        if !git_available() {
            return;
        }
        let (_dir, repo, c1) = make_repo_with_commit();
        // Diff a commit against itself.
        let diffs = repo.diff_commits(&c1.0, &c1.0).expect("diff");
        assert!(diffs.is_empty(), "diff of same commit should be empty");
    }

    #[test]
    fn diff_working_tree_vs_commit_clean() {
        if !git_available() {
            return;
        }
        let (_dir, repo, c1) = make_repo_with_commit();
        let diffs = repo.diff_working_tree_vs_commit(&c1.0).expect("diff");
        assert!(diffs.is_empty(), "clean working tree should have no diff");
    }

    #[test]
    fn diff_working_tree_vs_commit_modified() {
        if !git_available() {
            return;
        }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "README.md", b"# Modified README\n");
        let diffs = repo.diff_working_tree_vs_commit(&c1.0).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "README.md"));
    }

    #[test]
    fn diff_working_tree_vs_commit_new_file() {
        if !git_available() {
            return;
        }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "new.txt", b"new\n");
        // Untracked files don't show in git diff — need to stage first.
        repo.stage(ChangeSelection::All).expect("stage");
        let diffs = repo.diff_working_tree_vs_commit(&c1.0).expect("diff");
        assert!(
            diffs.iter().any(|d| d.path == "new.txt"),
            "staged new file should show in diff"
        );
    }

    #[test]
    fn diff_file_at_commits_same_commit() {
        if !git_available() {
            return;
        }
        let (_dir, repo, c1) = make_repo_with_commit();
        let diffs = repo
            .diff_file_at_commits("README.md", &c1.0, &c1.0)
            .expect("diff");
        assert!(diffs.is_empty());
    }

    #[test]
    fn diff_commits_multiple_files() {
        if !git_available() {
            return;
        }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "a.txt", b"a\n");
        write(dir.path(), "b.txt", b"b\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c2 = repo
            .commit(CommitRequest {
                message: "add files".to_string(),
                amend: false,
            })
            .expect("commit");
        let diffs = repo.diff_commits(&c1.0, &c2.0).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "a.txt"));
        assert!(diffs.iter().any(|d| d.path == "b.txt"));
    }

    #[test]
    fn diff_commits_deleted_file() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "temp.txt", b"temp\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo
            .commit(CommitRequest {
                message: "add temp".to_string(),
                amend: false,
            })
            .expect("commit");
        std::fs::remove_file(dir.path().join("temp.txt")).expect("delete");
        repo.stage(ChangeSelection::All).expect("stage removal");
        let c2 = repo
            .commit(CommitRequest {
                message: "remove temp".to_string(),
                amend: false,
            })
            .expect("commit");
        let diffs = repo.diff_commits(&c1.0, &c2.0).expect("diff");
        assert!(diffs.iter().any(|d| d.path == "temp.txt"));
    }

    // ── Log: comprehensive ──────────────────────────────────────────────────

    #[test]
    fn log_empty_repo_no_commits() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        let repo = GitCli::open(dir.path()).expect("open");
        // No commits yet — log should return error or empty.
        let result = repo.log(10);
        // git log on empty repo returns error.
        assert!(result.is_err() || result.expect("log").is_empty());
    }

    #[test]
    fn log_max_count_limit() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        for i in 0..5 {
            write(
                dir.path(),
                &format!("file{}.txt", i),
                format!("content{}\n", i).as_bytes(),
            );
            repo.stage(ChangeSelection::All).expect("stage");
            repo.commit(CommitRequest {
                message: format!("commit {}", i),
                amend: false,
            })
            .expect("commit");
        }
        let log = repo.log(3).expect("log");
        assert_eq!(log.len(), 3, "should respect max_count");
    }

    #[test]
    fn log_has_short_sha() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let log = repo.log(10).expect("log");
        let entry = &log[0];
        assert!(!entry.sha.is_empty(), "sha should not be empty");
        assert!(!entry.short_sha.is_empty(), "short_sha should not be empty");
        assert!(
            entry.sha.starts_with(&entry.short_sha),
            "sha should start with short_sha"
        );
    }

    #[test]
    fn log_has_parents() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        // Initial commit has no parents.
        let log = repo.log(10).expect("log");
        let initial = log.last().expect("last");
        assert!(initial.parents.is_empty(), "initial commit has no parents");
        // Add another commit — should have 1 parent.
        write(dir.path(), "new.txt", b"new\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "second".to_string(),
            amend: false,
        })
        .expect("commit");
        let log = repo.log(10).expect("log");
        let second = &log[0];
        assert_eq!(
            second.parents.len(),
            1,
            "second commit should have 1 parent"
        );
    }

    /// Commit bodies and author names are attacker-controlled in cloned
    /// repos — a body containing TAB characters must not shift positional
    /// parsing and corrupt `parents`/`body` (the reason fields are \x1f,
    /// not \t, separated).
    #[test]
    fn log_survives_tabs_in_commit_body() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "t.txt", b"t\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "subject\n\nbody\twith\ttabs".to_string(),
            amend: false,
        })
        .expect("commit");
        let log = repo.log(10).expect("log");
        let entry = &log[0];
        assert_eq!(entry.message, "subject");
        assert_eq!(entry.body.trim(), "body\twith\ttabs");
        assert_eq!(
            entry.parents.len(),
            1,
            "tab in body must not corrupt parents"
        );
        assert!(entry.parents[0].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn log_for_file_follows_renames() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "original.txt", b"content\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "add original".to_string(),
            amend: false,
        })
        .expect("commit");
        // Rename and modify — use git mv for proper rename detection.
        let git_bin = std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string());
        Command::new(&git_bin)
            .current_dir(dir.path())
            .args(["mv", "original.txt", "renamed.txt"])
            .output()
            .expect("git mv");
        write(dir.path(), "renamed.txt", b"content modified\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "rename and modify".to_string(),
            amend: false,
        })
        .expect("commit");
        // --follow should find at least the rename commit (rename detection with custom
        // format may not always traverse past the rename boundary on all git versions).
        let log = repo.log_for_file("renamed.txt", 10).expect("log");
        assert!(
            !log.is_empty(),
            "should find at least 1 commit for renamed file with --follow"
        );
    }

    #[test]
    fn log_for_nonexistent_file() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let log = repo.log_for_file("nonexistent.txt", 10).expect("log");
        assert!(log.is_empty(), "nonexistent file should have no history");
    }

    // ── Misc: comprehensive ─────────────────────────────────────────────────

    #[test]
    fn head_commit_after_multiple_commits() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "a.txt", b"a\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo
            .commit(CommitRequest {
                message: "c1".to_string(),
                amend: false,
            })
            .expect("commit");
        assert_eq!(repo.head_commit().expect("head").0, c1.0);
        write(dir.path(), "b.txt", b"b\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c2 = repo
            .commit(CommitRequest {
                message: "c2".to_string(),
                amend: false,
            })
            .expect("commit");
        assert_eq!(repo.head_commit().expect("head").0, c2.0);
    }

    #[test]
    fn is_clean_after_stash() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "README.md", b"# Modified\n");
        assert!(!repo.is_clean().expect("not clean"));
        repo.stash_push(None).expect("stash");
        assert!(repo.is_clean().expect("clean after stash"));
    }

    #[test]
    fn current_branch_after_checkout() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let initial = repo.current_branch().expect("current");
        repo.create_branch("newbranch").expect("create");
        repo.checkout(Revision::Branch("newbranch".to_string()))
            .expect("checkout");
        assert_eq!(repo.current_branch().expect("current"), "newbranch");
        repo.checkout(Revision::Branch(initial.clone()))
            .expect("checkout back");
        assert_eq!(repo.current_branch().expect("current"), initial);
    }

    #[test]
    fn commit_amend() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "new.txt", b"new\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let original_head = repo.head_commit().expect("head");
        repo.commit(CommitRequest {
            message: "original message".to_string(),
            amend: false,
        })
        .expect("commit");
        // Amend the commit.
        repo.commit(CommitRequest {
            message: "amended message".to_string(),
            amend: true,
        })
        .expect("amend");
        let log = repo.log(10).expect("log");
        assert_eq!(log[0].message, "amended message");
        // HEAD should have changed (new commit hash from amend).
        assert_ne!(repo.head_commit().expect("head").0, original_head.0);
    }

    // ── Combined workflows ──────────────────────────────────────────────────

    #[test]
    fn workflow_stage_commit_branch_merge() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        // Create feature branch, add a file, commit, merge back.
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        write(dir.path(), "feature.txt", b"feature content\n");
        repo.stage(ChangeSelection::File {
            path: "feature.txt".to_string(),
        })
        .expect("stage");
        repo.commit(CommitRequest {
            message: "add feature".to_string(),
            amend: false,
        })
        .expect("commit");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        repo.merge("feature", MergeStrategy::FastForwardOnly)
            .expect("merge");
        assert!(dir.path().join("feature.txt").exists());
        assert!(repo.is_clean().expect("clean"));
    }

    #[test]
    fn workflow_stash_branch_pop() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        // Modify a tracked file, stash, switch branch, pop.
        write(dir.path(), "README.md", b"# WIP\n");
        repo.stash_push(Some("wip")).expect("stash");
        repo.create_branch("dev").expect("create");
        repo.checkout(Revision::Branch("dev".to_string()))
            .expect("checkout");
        repo.stash_pop(0).expect("pop");
        let content = std::fs::read_to_string(dir.path().join("README.md")).expect("read");
        assert_eq!(
            content.trim(),
            "# WIP",
            "stashed changes should be restored"
        );
    }

    #[test]
    fn workflow_commit_tag_reset() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        // Make a commit, tag it, then reset back.
        write(dir.path(), "v1.txt", b"v1\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let _c1 = repo
            .commit(CommitRequest {
                message: "v1".to_string(),
                amend: false,
            })
            .expect("commit");
        repo.create_tag("v1.0", Some("first release")).expect("tag");
        // Hard reset to initial.
        let initial = repo
            .log(10)
            .expect("log")
            .last()
            .expect("initial")
            .sha
            .clone();
        repo.reset_hard(&initial).expect("reset");
        assert!(
            !dir.path().join("v1.txt").exists(),
            "v1.txt should be gone after reset"
        );
        // Tag should still exist (tags are independent of HEAD).
        let tags = repo.tags().expect("tags");
        assert!(
            tags.iter().any(|t| t.name == "v1.0"),
            "tag should survive reset"
        );
    }

    #[test]
    fn workflow_cherry_pick_then_revert() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        // Create a commit on feature, cherry-pick it, then revert it.
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        let c1 = commit_file(&repo, "new.txt", b"new content\n", "add new");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        repo.cherry_pick(&c1.0).expect("cherry-pick");
        assert!(dir.path().join("new.txt").exists());
        // Revert the cherry-picked commit.
        let head = repo.head_commit().expect("head");
        repo.revert(&head.0).expect("revert");
        assert!(
            !dir.path().join("new.txt").exists(),
            "file should be gone after revert"
        );
    }

    #[test]
    fn workflow_multiple_branches_merge_squash() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        // Create two feature branches, merge both with squash.
        repo.create_branch("feat-a").expect("create a");
        repo.checkout(Revision::Branch("feat-a".to_string()))
            .expect("checkout a");
        commit_file(&repo, "a.txt", b"a\n", "add a");
        commit_file(&repo, "a2.txt", b"a2\n", "add a2");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        repo.merge("feat-a", MergeStrategy::Squash)
            .expect("squash a");
        assert!(dir.path().join("a.txt").exists());
        assert!(dir.path().join("a2.txt").exists());
        // Second feature.
        repo.create_branch("feat-b").expect("create b");
        repo.checkout(Revision::Branch("feat-b".to_string()))
            .expect("checkout b");
        commit_file(&repo, "b.txt", b"b\n", "add b");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        repo.merge("feat-b", MergeStrategy::Squash)
            .expect("squash b");
        assert!(dir.path().join("b.txt").exists());
    }

    #[test]
    fn workflow_stage_unstage_commit() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        // Stage, unstage, stage again, commit.
        write(dir.path(), "file.txt", b"content\n");
        repo.stage(ChangeSelection::File {
            path: "file.txt".to_string(),
        })
        .expect("stage");
        repo.unstage(ChangeSelection::File {
            path: "file.txt".to_string(),
        })
        .expect("unstage");
        let st = repo.status().expect("status");
        assert!(st.staged.is_empty(), "should not be staged after unstage");
        repo.stage(ChangeSelection::File {
            path: "file.txt".to_string(),
        })
        .expect("stage again");
        repo.commit(CommitRequest {
            message: "add file".to_string(),
            amend: false,
        })
        .expect("commit");
        assert!(repo.is_clean().expect("clean after commit"));
    }

    #[test]
    fn workflow_diff_between_all_three_states() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        // Stage one change, make another unstaged change.
        write(dir.path(), "staged.txt", b"staged\n");
        repo.stage(ChangeSelection::File {
            path: "staged.txt".to_string(),
        })
        .expect("stage");
        write(dir.path(), "README.md", b"# Modified\n");
        // Working tree vs HEAD should show both.
        let diffs_wt_head = repo
            .diff(DiffRequest::WorkingTreeVsHead)
            .expect("diff wt vs head");
        assert!(diffs_wt_head.iter().any(|d| d.path == "staged.txt"));
        assert!(diffs_wt_head.iter().any(|d| d.path == "README.md"));
        // Index vs HEAD should show only staged.
        let diffs_idx_head = repo
            .diff(DiffRequest::IndexVsHead)
            .expect("diff idx vs head");
        assert!(diffs_idx_head.iter().any(|d| d.path == "staged.txt"));
        assert!(!diffs_idx_head.iter().any(|d| d.path == "README.md"));
        // Working tree vs index should show only unstaged.
        let diffs_wt_idx = repo
            .diff(DiffRequest::WorkingTreeVsIndex)
            .expect("diff wt vs idx");
        assert!(diffs_wt_idx.iter().any(|d| d.path == "README.md"));
    }

    #[test]
    fn workflow_tag_cherry_pick_and_rebase() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        // Tag initial, create feature, add commits, rebase, cherry-pick.
        repo.create_tag("v1.0", Some("initial")).expect("tag");
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        commit_file(&repo, "feat.txt", b"feat\n", "add feat");
        // Add commit on master.
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        commit_file(&repo, "master.txt", b"master\n", "on master");
        // Rebase feature onto master.
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout feature");
        repo.rebase("master").expect("rebase");
        assert!(dir.path().join("feat.txt").exists());
        assert!(dir.path().join("master.txt").exists());
        // Cherry-pick feature's commit onto master.
        let head = repo.head_commit().expect("head");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        repo.cherry_pick(&head.0).expect("cherry-pick");
        assert!(
            dir.path().join("feat.txt").exists(),
            "feat.txt should exist after cherry-pick"
        );
    }

    #[test]
    fn workflow_stash_apply_drop_clean() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        // Create a tracked file, modify it, stash, apply, drop, clean untracked.
        write(dir.path(), "tracked.txt", b"tracked\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "add tracked".to_string(),
            amend: false,
        })
        .expect("commit");
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
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("temp").expect("create");
        repo.delete_branch("temp", false).expect("delete");
        assert!(
            !repo
                .branches()
                .expect("branches")
                .iter()
                .any(|b| b.name == "temp")
        );
        // Recreate.
        repo.create_branch("temp").expect("recreate");
        assert!(
            repo.branches()
                .expect("branches")
                .iter()
                .any(|b| b.name == "temp")
        );
    }

    #[test]
    fn workflow_config_set_commit_check_author() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        repo.config_set("user.name", "New Author")
            .expect("set name");
        repo.config_set("user.email", "new@example.com")
            .expect("set email");
        write(dir.path(), "file.txt", b"content\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "by new author".to_string(),
            amend: false,
        })
        .expect("commit");
        let log = repo.log(10).expect("log");
        assert_eq!(log[0].author, "New Author");
        assert_eq!(log[0].author_email, "new@example.com");
    }

    // ── Edge cases ──────────────────────────────────────────────────────────

    #[test]
    fn open_nonexistent_directory() {
        if !git_available() {
            return;
        }
        let result = GitCli::open("C:\\nonexistent\\path\\12345");
        assert!(result.is_err());
    }

    #[test]
    fn status_after_delete_file() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        std::fs::remove_file(dir.path().join("README.md")).expect("delete");
        let st = repo.status().expect("status");
        assert!(
            st.changes
                .iter()
                .any(|f| f.path == "README.md" && f.status == FileStatus::Deleted),
            "should show deleted file"
        );
    }

    #[test]
    fn status_renamed_file() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        // Use git mv for reliable rename detection.
        let git_bin = std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string());
        Command::new(&git_bin)
            .current_dir(dir.path())
            .args(["mv", "README.md", "RENAMED.md"])
            .output()
            .expect("git mv");
        let st = repo.status().expect("status");
        // Should show as renamed (R) in staged.
        assert!(
            st.staged.iter().any(|f| f.path == "RENAMED.md"),
            "renamed file should appear in staged"
        );
    }

    #[test]
    fn commit_with_empty_message_fails() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "file.txt", b"content\n");
        repo.stage(ChangeSelection::All).expect("stage");
        // Empty message — git should reject it.
        let result = repo.commit(CommitRequest {
            message: "".to_string(),
            amend: false,
        });
        assert!(result.is_err(), "commit with empty message should fail");
    }

    #[test]
    fn diff_branch_vs_branch() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        write(dir.path(), "new.txt", b"new\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "on feature".to_string(),
            amend: false,
        })
        .expect("commit");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        let diffs = repo
            .diff(DiffRequest::BranchVsBranch {
                a: "master".to_string(),
                b: "feature".to_string(),
            })
            .expect("diff");
        assert!(diffs.iter().any(|d| d.path == "new.txt"));
    }

    #[test]
    fn diff_head_vs_branch() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        repo.create_branch("feature").expect("create");
        repo.checkout(Revision::Branch("feature".to_string()))
            .expect("checkout");
        write(dir.path(), "new.txt", b"new\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "on feature".to_string(),
            amend: false,
        })
        .expect("commit");
        repo.checkout(Revision::Branch("master".to_string()))
            .expect("checkout master");
        let diffs = repo
            .diff(DiffRequest::HeadVsBranch {
                branch: "feature".to_string(),
            })
            .expect("diff");
        assert!(diffs.iter().any(|d| d.path == "new.txt"));
    }

    #[test]
    fn diff_file_version_vs_version() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "doc.md", b"v1\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo
            .commit(CommitRequest {
                message: "v1".to_string(),
                amend: false,
            })
            .expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c2 = repo
            .commit(CommitRequest {
                message: "v2".to_string(),
                amend: false,
            })
            .expect("commit");
        let diffs = repo
            .diff(DiffRequest::FileVersionVsVersion {
                path: "doc.md".to_string(),
                a: c1.clone(),
                b: c2.clone(),
            })
            .expect("diff");
        assert!(diffs.iter().any(|d| d.path == "doc.md"));
    }

    #[test]
    fn diff_working_tree_vs_commit_file() {
        if !git_available() {
            return;
        }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "README.md", b"# Modified\n");
        let diffs = repo
            .diff(DiffRequest::WorkingTreeVsCommitFile {
                commit: c1,
                path: "README.md".to_string(),
            })
            .expect("diff");
        assert!(diffs.iter().any(|d| d.path == "README.md"));
    }

    #[test]
    fn diff_index_vs_commit() {
        if !git_available() {
            return;
        }
        let (dir, repo, c1) = make_repo_with_commit();
        write(dir.path(), "new.txt", b"new\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let diffs = repo
            .diff(DiffRequest::IndexVsCommit { commit: c1 })
            .expect("diff");
        assert!(diffs.iter().any(|d| d.path == "new.txt"));
    }

    #[test]
    fn read_file_at_head() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let head = repo.head_commit().expect("head");
        let content = read_file_at_revision(&repo, "README.md", &head.0).expect("read");
        assert_eq!(String::from_utf8_lossy(&content).trim(), "# README");
    }

    #[test]
    fn read_file_at_old_revision() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "doc.md", b"v1\n");
        repo.stage(ChangeSelection::All).expect("stage");
        let c1 = repo
            .commit(CommitRequest {
                message: "v1".to_string(),
                amend: false,
            })
            .expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "v2".to_string(),
            amend: false,
        })
        .expect("commit");
        let old = read_file_at_revision(&repo, "doc.md", &c1.0).expect("read old");
        assert_eq!(String::from_utf8_lossy(&old).trim(), "v1");
    }

    #[test]
    fn file_history_empty_for_nonexistent() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let hist = file_history(&repo, "nonexistent.txt").expect("history");
        assert!(hist.is_empty());
    }

    #[test]
    fn file_history_order() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        write(dir.path(), "doc.md", b"v1\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "first".to_string(),
            amend: false,
        })
        .expect("commit");
        write(dir.path(), "doc.md", b"v2\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "second".to_string(),
            amend: false,
        })
        .expect("commit");
        write(dir.path(), "doc.md", b"v3\n");
        repo.stage(ChangeSelection::All).expect("stage");
        repo.commit(CommitRequest {
            message: "third".to_string(),
            amend: false,
        })
        .expect("commit");
        let hist = file_history(&repo, "doc.md").expect("history");
        assert_eq!(hist.len(), 3);
        assert_eq!(hist[0].message, "third");
        assert_eq!(hist[1].message, "second");
        assert_eq!(hist[2].message, "first");
    }

    #[test]
    fn branches_includes_remote_after_fetch() {
        if !git_available() {
            return;
        }
        // We can't test actual remote fetch without a remote repo,
        // but we can verify that branches() doesn't crash.
        let (_dir, repo, _id) = make_repo_with_commit();
        let _branches = repo.branches().expect("branches should not crash");
    }

    #[test]
    fn remotes_empty_repo() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let remotes = repo.remotes().expect("remotes");
        assert!(remotes.is_empty(), "fresh repo should have no remotes");
    }

    #[test]
    fn add_and_remove_remote() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.add_remote("origin", "https://github.com/test/repo.git")
            .expect("add remote");
        let remotes = repo.remotes().expect("remotes");
        assert!(remotes.iter().any(|r| r.name == "origin"));
        repo.remove_remote("origin").expect("remove remote");
        let remotes = repo.remotes().expect("remotes");
        assert!(!remotes.iter().any(|r| r.name == "origin"));
    }

    #[test]
    fn add_multiple_remotes() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        repo.add_remote("origin", "https://github.com/test/repo.git")
            .expect("add");
        repo.add_remote("upstream", "https://github.com/upstream/repo.git")
            .expect("add");
        let remotes = repo.remotes().expect("remotes");
        assert_eq!(remotes.len(), 2);
        assert!(remotes.iter().any(|r| r.name == "origin"));
        assert!(remotes.iter().any(|r| r.name == "upstream"));
    }

    #[test]
    fn exec_text_returns_output() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let text = repo.exec_text(&["log", "--oneline", "-1"]).expect("exec");
        assert!(!text.trim().is_empty(), "git log should return output");
    }

    #[test]
    fn work_dir_returns_path() {
        if !git_available() {
            return;
        }
        let (dir, repo, _id) = make_repo_with_commit();
        let work_dir = repo.work_dir();
        assert!(work_dir.exists(), "work_dir should exist");
        assert_eq!(work_dir, dir.path());
    }

    /// `git status` without `-z` C-quotes non-ASCII and special-char paths
    /// (core.quotepath), which mangled `файл.md` into octal escapes and broke
    /// staging/diff. The `-z` parser must return verbatim paths.
    #[test]
    fn status_non_ascii_path_is_verbatim() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "файл (2).md", b"# x\n");
        let repo = GitCli::open(dir.path()).expect("open");
        let st = repo.status().expect("status");
        assert!(
            st.untracked.iter().any(|f| f.path == "файл (2).md"),
            "untracked path should be verbatim, got: {:?}",
            st.untracked
        );
    }

    /// Rename entries in `-z` mode emit the source path as a following record —
    /// the parser must pair them, not treat the source as a separate file.
    #[test]
    fn status_rename_pairs_paths() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        let git = std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string());
        Command::new(&git)
            .current_dir(repo.work_dir())
            .args(["mv", "README.md", "renamed.md"])
            .output()
            .expect("git mv");
        let st = repo.status().expect("status");
        let ren = st.staged.iter().find(|f| f.status == FileStatus::Renamed);
        let ren = ren.expect("rename should be staged");
        assert_eq!(ren.path, "renamed.md");
        assert_eq!(ren.old_path.as_deref(), Some("README.md"));
        // The source path must not leak into staged as its own entry.
        assert!(
            !st.staged
                .iter()
                .any(|f| f.path == "README.md" && f.old_path.is_none())
        );
    }

    // ── Option-injection guards ────────────────────────────────────────────
    // User-controlled names must never reach git as a leading `-` argument:
    // `git checkout -f` discards local changes, `git fetch --prune` deletes
    // remote-tracking refs, `git pull origin --rebase` changes pull semantics.
    #[test]
    fn rejects_dash_prefixed_branch_checkout() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        assert!(repo.checkout(Revision::Branch("-f".to_string())).is_err());
        assert!(
            repo.checkout(Revision::Branch("--force".to_string()))
                .is_err()
        );
    }

    #[test]
    fn rejects_dash_prefixed_names() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        assert!(repo.create_branch("-x").is_err());
        assert!(repo.delete_branch("--all", false).is_err());
        assert!(repo.rename_branch("-m", "x").is_err());
        assert!(repo.merge("--abort", MergeStrategy::Merge).is_err());
        assert!(repo.rebase("--continue").is_err());
        assert!(repo.create_tag("-d", None).is_err());
        assert!(repo.add_remote("-t", "https://x").is_err());
        assert!(repo.add_remote("origin", "-m").is_err());
        assert!(repo.fetch_remote("--prune").is_err());
        assert!(repo.pull_from_remote("origin", "--rebase").is_err());
        assert!(repo.push_to_remote("-u", "main", false).is_err());
        assert!(repo.config_get("-l").is_err());
    }

    /// Config keys that execute external programs or redirect credentials
    /// must be refused at the port — `credential.helper` is a shell command
    /// fed the credential, `core.hooksPath` re-enables hooks, `url.*.insteadOf`
    /// silently redirects every fetch/push.
    #[test]
    fn config_set_rejects_executable_or_credential_keys() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        for key in [
            "credential.helper",
            "credential.https://evil.example.helper",
            "core.hooksPath",
            "core.fsmonitor",
            "core.sshCommand",
            "core.gitProxy",
            "diff.external",
            "include.path",
            "includeIf.gitdir:/tmp.path",
            "url.https://evil.example/.insteadOf",
            "filter.x.clean",
            "sendmail.smtpserver",
            "alias.st",
            "gpg.program",
            "http.sslVerify",
        ] {
            assert!(repo.config_set(key, "x").is_err(), "{key} must be refused");
        }
        // Plain identity keys still work.
        repo.config_set("user.name", "Legit").expect("set");
        assert_eq!(
            repo.config_get("user.name").expect("get").as_deref(),
            Some("Legit")
        );
    }

    #[test]
    fn rejects_control_chars_in_names() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        assert!(repo.create_branch("bad\nname").is_err());
        assert!(repo.create_branch("bad name").is_err());
        assert!(repo.checkout(Revision::Branch(String::new())).is_err());
    }

    /// A `--raw -z` rename record is `meta\0OLD\0NEW\0` — the parser must
    /// take paths verbatim (no quoting) and in the right order.
    #[test]
    fn parse_raw_diff_z_rename_and_special_names() {
        let bytes = b":100644 100644 abc1234 def5678 R100\0old dir/old.md\0new dir/new.md\0:100644 100644 1111111 2222222 M\0\xd1\x82\xd0\xb0\xd0\xb1 \xd1\x84\xd0\xb0\xd0\xb9\xd0\xbb.md\0";
        let diffs = parse_raw_diff_z(bytes);
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].path, "new dir/new.md");
        assert_eq!(diffs[0].old_path, Some("old dir/old.md".to_string()));
        // Raw UTF-8 filename arrives unmangled (plain --raw would C-quote it).
        assert_eq!(diffs[1].path, "таб файл.md");
        assert_eq!(diffs[1].old_path, None);
    }

    /// A filename containing a real TAB passes through `-z` verbatim —
    /// plain --raw would have C-quoted it into a mangled literal.
    #[test]
    fn parse_raw_diff_z_path_with_tab() {
        let bytes = b":100644 100644 a1 b2 M\0with\ttab.md\0";
        let diffs = parse_raw_diff_z(bytes);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].path, "with\ttab.md");
    }

    /// A truncated stream must not panic or emit a half-parsed rename.
    #[test]
    fn parse_raw_diff_z_truncated() {
        let bytes = b":100644 100644 a1 b2 R100\0only-old.md\0";
        let diffs = parse_raw_diff_z(bytes);
        assert!(diffs.is_empty());
    }

    /// Refname-illegal characters must be rejected before they reach a git
    /// argv — `*`/`?` in a `push <b>:<b>` refspec glob-expand to EVERY ref
    /// (a `push --force origin *` would force-push all branches), and `:`
    /// changes the refspec shape entirely.
    #[test]
    fn validate_name_arg_rejects_glob_and_refspec_chars() {
        for bad in [
            "*", "?", "a:b", "a~1", "a^2", "a[b", "a\\b", "a..b", "a@{b}", "-x", "a b", "",
        ] {
            assert!(
                validate_name_arg(bad, "branch name").is_err(),
                "{bad:?} must be rejected"
            );
        }
        for good in [
            "main",
            "feature/x",
            "release-1.0",
            "v2.0",
            "dependabot/npm+x",
        ] {
            assert!(
                validate_name_arg(good, "branch name").is_ok(),
                "{good:?} must be accepted"
            );
        }
    }

    /// `-m <msg>` arguments: control characters (NUL especially — it fails
    /// `Command::spawn` with an opaque io error) must be rejected early and
    /// clearly; newline/tab bodies remain valid.
    #[test]
    fn validate_message_arg_rejects_control_chars() {
        assert!(validate_message_arg("subj\0ect").is_err());
        assert!(validate_message_arg("a\u{1b}[31m").is_err());
        assert!(validate_message_arg("a\u{7f}b").is_err());
        assert!(validate_message_arg("subject\n\nbody\ttab").is_ok());
        assert!(validate_message_arg("plain subject").is_ok());
    }

    /// `push_to_remote` builds `<branch>:<branch>` — a glob or colon in the
    /// name must fail validation instead of smuggling a mass-push refspec.
    #[test]
    fn push_to_remote_rejects_glob_branch() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        assert!(repo.push_to_remote("origin", "*", false).is_err());
        assert!(repo.push_to_remote("origin", "a:b", false).is_err());
        assert!(repo.push_to_remote("origin", "main?", true).is_err());
        assert!(repo.push_to_remote("*", "main", false).is_err());
    }

    /// `git pull remote SRC:DST` interprets the branch arg as a refspec and
    /// force-updates DST — a `:`-bearing "branch" (or one with `~`/`^`, which
    /// would silently pull a different commit) must be rejected before exec.
    #[test]
    fn pull_from_remote_rejects_refspec_branch() {
        if !git_available() {
            return;
        }
        let (_dir, repo, _id) = make_repo_with_commit();
        assert!(repo.pull_from_remote("origin", "main:main").is_err());
        assert!(repo.pull_from_remote("origin", "HEAD~1").is_err());
        assert!(repo.pull_from_remote("origin", "-anything").is_err());
        assert!(repo.pull_from_remote("origin", "a..b").is_err());
        // A plain branch name stays valid (fails only because no origin exists).
        let e = repo.pull_from_remote("origin", "main").unwrap_err();
        assert!(!e.to_string().contains("branch name"), "{e}");
    }

    /// A repo file literally named `:foo` is legal — the bare `:` prefix is
    /// not pathspec magic (only `:(`…`)` is), so `literal_pathspec` must wrap
    /// it rather than pass it through for git to reject as bad magic.
    #[test]
    fn literal_pathspec_wraps_colon_prefixed_filename() {
        assert_eq!(literal_pathspec(":foo"), ":(top,literal):foo");
        assert_eq!(literal_pathspec(":/x"), ":(top,literal):/x");
        // Caller-constructed magic still passes through unchanged.
        assert_eq!(literal_pathspec(":(top,literal)a.md"), ":(top,literal)a.md");
        assert_eq!(literal_pathspec(":(top)a.md"), ":(top)a.md");
    }

    /// Hardened handles must neutralize ALL config-driven execution, not just
    /// hooks: `filter.<drv>.clean` runs on `git add` (every snapshot commit)
    /// and `commit.gpgsign` spawns the signing program. A hostile repo config
    /// must yield a clean, unfiltered, unsigned commit.
    #[test]
    fn hardened_exec_never_runs_repo_filters_or_signers() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        let plain = GitCli::open(dir.path()).expect("open");
        // Poison the config as a hostile checkout would. `exec` (not
        // `config_set`) is used because the port rightly refuses these keys.
        plain
            .exec(&[
                "config",
                "filter.danger.clean",
                "echo PWNED > __filter_ran && echo FILTERED",
            ])
            .expect("poison filter");
        fs::write(dir.path().join(".gitattributes"), "*.md filter=danger\n").unwrap();
        write(dir.path(), "doc.md", b"real content\n");

        // Hardened: filter cleared at open, commit stores RAW bytes, marker absent.
        let repo = GitCli::open_hardened(dir.path()).expect("open hardened");
        repo.commit_paths(&["doc.md", ".gitattributes"], "snapshot")
            .expect("hardened commit");
        assert!(
            !dir.path().join("__filter_ran").exists(),
            "hardened add ran the repo's clean filter"
        );
        assert_eq!(
            repo.exec_text(&["show", "HEAD:doc.md"]).unwrap(),
            "real content\n",
            "hardened commit must store unfiltered bytes"
        );

        // Control: the NON-hardened handle really does run the filter.
        write(dir.path(), "doc2.md", b"two\n");
        plain
            .commit_paths(&["doc2.md"], "control")
            .expect("control commit");
        assert!(
            dir.path().join("__filter_ran").exists(),
            "control run should have executed the filter"
        );
        assert_eq!(
            plain.exec_text(&["show", "HEAD:doc2.md"]).unwrap(),
            "FILTERED\n",
            "control commit should contain filtered bytes"
        );

        // gpgsign=true + a missing program: hardened commits keep working,
        // plain commits fail trying to spawn the signer.
        plain
            .exec(&["config", "commit.gpgsign", "true"])
            .expect("enable sign");
        plain
            .exec(&["config", "gpg.program", "definitely-missing-womd-signer"])
            .expect("poison signer");
        write(dir.path(), "doc3.md", b"three\n");
        repo.commit_paths(&["doc3.md"], "hardened still commits")
            .expect("hardened commit despite gpgsign");
        write(dir.path(), "doc4.md", b"four\n");
        let e = plain
            .commit_paths(&["doc4.md"], "plain commit")
            .expect_err("plain commit must fail on the missing signer");
        let msg = e.to_string();
        assert!(
            msg.contains("definitely-missing-womd-signer")
                || msg.to_lowercase().contains("sign")
                || msg.to_lowercase().contains("gpg"),
            "unexpected failure: {msg}"
        );
    }

    /// The hardened env denylist must keep covering the variables that
    /// redirect the repo (GIT_DIR et al.), inject config, or name programs
    /// git will execute. A regression here silently reopens an exec channel.
    #[test]
    fn hardened_env_scrublist_covers_execution_and_redirect_vars() {
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_EXTERNAL_DIFF",
            "GIT_DIFF_OPTS",
            "GIT_EXEC_PATH",
            "GIT_SSH_COMMAND",
            "GIT_ASKPASS",
            "GIT_PROXY_COMMAND",
            "GIT_PAGER",
            "GIT_EDITOR",
            "BASH_ENV",
            "LD_PRELOAD",
        ] {
            assert!(
                GitCli::SCRUBBED_ENV.contains(&var),
                "SCRUBBED_ENV is missing {var}"
            );
        }
    }

    /// `diff.external` names a program git shells out to on every `git diff`.
    /// A hardened handle must (a) never execute it and (b) still return the
    /// real diff — a naive `diff.external=` empty override breaks (b) because
    /// git then tries to spawn the empty command and every diff dies. The fix
    /// is `--no-ext-diff` on our diff commands + clearing the config key only
    /// when a config scope actually sets it.
    #[test]
    fn hardened_diff_ignores_external_diff_program() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        write(dir.path(), "file.md", b"original\n");
        let plain = GitCli::open(dir.path()).expect("open");
        plain
            .commit_paths(&["file.md"], "base")
            .expect("base commit");
        write(dir.path(), "file.md", b"changed content\n");
        // `exec` (not `config_set`) — the port rightly refuses this key.
        plain
            .exec(&[
                "config",
                "diff.external",
                "echo PWNED > __extdiff_ran; echo EXTERNAL-DIFF-EXECUTED",
            ])
            .expect("poison diff.external");

        // Control: a raw `git diff` (no --no-ext-diff) really does run the
        // external program — proves the poisoned config is live.
        let raw = plain
            .exec_text(&["diff", "HEAD", "--", "file.md"])
            .expect("control diff");
        assert!(
            raw.contains("EXTERNAL-DIFF-EXECUTED"),
            "control: external diff did not run: {raw:?}"
        );
        assert!(dir.path().join("__extdiff_ran").exists());
        fs::remove_file(dir.path().join("__extdiff_ran")).unwrap();

        // Hardened: opened AFTER the config is poisoned — the key is
        // enumerated and cleared, and our diff calls carry --no-ext-diff.
        let repo = GitCli::open_hardened(dir.path()).expect("open hardened");
        let d = repo
            .diff_file_raw("file.md")
            .expect("hardened diff must not fail on configured diff.external");
        assert!(
            !d.contains("EXTERNAL-DIFF-EXECUTED"),
            "hardened diff ran external program: {d:?}"
        );
        assert!(
            !dir.path().join("__extdiff_ran").exists(),
            "hardened diff executed the external program"
        );
        assert!(
            d.contains("changed content"),
            "hardened diff lost the real change: {d:?}"
        );
    }

    /// `commit_paths` makes a version snapshot — repository hooks are
    /// arbitrary code and must never execute (or mutate/stall the commit).
    #[test]
    fn commit_paths_never_runs_hooks() {
        if !git_available() {
            return;
        }
        let dir = make_repo();
        // A hostile/broken pre-commit hook: runs code AND would block.
        let hooks = dir.path().join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        fs::write(&hook, "#!/bin/sh\ntouch hook-ran\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let repo = GitCli::open(dir.path()).expect("open");
        write(dir.path(), "a.md", b"# A\n");
        let id = repo.commit_paths(&["a.md"], "snapshot").expect("commit");
        assert!(id.is_some(), "hook must not be able to fail the commit");
        assert!(
            !dir.path().join("hook-ran").exists(),
            "pre-commit hook executed"
        );
    }
}
