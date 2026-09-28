//! Tool implementations for the WoMD MCP server.
//!
//! All paths are workspace-relative and pass through `sandbox::Workspace`.
//! No tool spawns processes — Git access is read-only and goes through the
//! `editor-git` port. Writes go through `editor_storage`'s atomic /
//! streaming saves so unchanged bytes are preserved verbatim (Invariant 1, 2).

use std::fs;
use std::path::{Path, PathBuf};

use editor_domain::MarkdownProfile;
use editor_git::{GitCli, GitExtended, VersionControl};
use serde_json::{Value, json};

use crate::sandbox::{SandboxError, Workspace};

/// Tool dispatch surface. `call` returns structured JSON on success or an
/// `Err(String)` mapped to an MCP `isError` tool result.
///
/// Mutating tools (`create_document`, `write_document`, `append_document`,
/// `edit_document`, `delete_document`, `move_document`, `create_directory`,
/// `markdown_block_edit`, `markdown_toggle_task`) accept `commit_message`
/// and `commit` (default true): each change becomes its own git commit whose
/// message is chosen by the agent (ADR-007). If the workspace is not yet a
/// repository it is `git init`'d on first commit.
pub struct Tools {
    ws: Workspace,
}

impl std::fmt::Debug for Tools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tools")
            .field("root", &self.ws.root())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// argument helpers
// ---------------------------------------------------------------------------

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required string parameter '{key}'"))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn opt_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn opt_usize(args: &Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|v| usize::try_from(v).unwrap_or(usize::MAX))
        .unwrap_or(default)
}

/// Read a required non-negative integer arg without truncation (a raw
/// `u64 as usize` silently wraps on 32-bit builds).
fn req_usize(e: &Value, key: &str, ctx: &str) -> Result<usize, String> {
    let v = e
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{ctx}: missing '{key}'"))?;
    usize::try_from(v).map_err(|_| format!("{ctx}: '{key}' {v} too large"))
}

fn err(e: SandboxError) -> String {
    e.to_string()
}

// ---------------------------------------------------------------------------
// text helpers
// ---------------------------------------------------------------------------

/// Byte offsets of every line start (line 1 -> index 0).
fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

/// 1-based line number containing byte `offset`.
fn line_of(starts: &[usize], offset: usize) -> usize {
    match starts.binary_search(&offset) {
        Ok(i) => i + 1,
        Err(i) => i,
    }
}

/// Payload ceilings — a JSON tool call should never be able to allocate or
/// scan unbounded memory.
const MAX_TEXT_FILE: u64 = 64 * 1024 * 1024; // per-file read for text tools
const MAX_WRITE_BYTES: usize = 64 * 1024 * 1024; // per-call content parameter
const MAX_WALK_ENTRIES: usize = 50_000; // find_files / search traversal
const MAX_LIST_ENTRIES: usize = 5_000; // list_directory total
const MAX_SEARCH_FILES: usize = 20_000; // search_documents files scanned

fn read_utf8(path: &Path) -> Result<String, String> {
    let md = fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    // A FIFO/device has len 0 — the size cap alone would let open() block
    // forever waiting for a writer. Only regular files may be read.
    if !md.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if md.len() > MAX_TEXT_FILE {
        return Err(format!(
            "file is {} bytes — over the {} MiB limit for text tools",
            md.len(),
            MAX_TEXT_FILE / (1024 * 1024)
        ));
    }
    // Bounded read: the stat above is a fast pre-check only — a file that
    // grows between metadata() and read() must still be capped, so limit the
    // stream itself to MAX_TEXT_FILE + 1 and re-verify after the read.
    let bytes = {
        use std::io::Read;
        let f = fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let mut buf = Vec::new();
        f.take(MAX_TEXT_FILE + 1)
            .read_to_end(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        buf
    };
    if bytes.len() as u64 > MAX_TEXT_FILE {
        return Err(format!(
            "file grew past the {} MiB text limit while reading",
            MAX_TEXT_FILE / (1024 * 1024)
        ));
    }
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        return String::from_utf8(bytes)
            .map_err(|_| "file is not valid UTF-8 (BOM-prefixed)".to_string());
    }
    String::from_utf8(bytes).map_err(|_| "file is not valid UTF-8 text".to_string())
}

fn check_write_size(content: &str) -> Result<(), String> {
    if content.len() > MAX_WRITE_BYTES {
        return Err(format!(
            "content is {} bytes — over the {} MiB per-call write limit",
            content.len(),
            MAX_WRITE_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

/// Compare-and-save for read-modify-write tools: refuse to write over a file
/// that changed on disk since `original` was read — a human save or another
/// MCP call landing in the meantime would otherwise be silently clobbered
/// (its commit stays in history, but the *file* would lose it).
fn ensure_unchanged(p: &Path, original: &[u8]) -> Result<(), String> {
    // Length pre-check: a file that grew unboundedly (or shrank) is provably
    // different — no need to materialize it to decide.
    let md = fs::metadata(p).map_err(|e| format!("re-stat: {e}"))?;
    // Non-regular file (FIFO/device/swap-for-dir): never open it — a FIFO
    // read blocks until a writer appears.
    if !md.is_file() {
        return Err("path is no longer a regular file".into());
    }
    if md.len() != original.len() as u64 {
        return Err("file changed on disk since it was read — re-read and retry the edit".into());
    }
    // Bound the re-read at original.len()+1: enough to prove inequality while
    // a file that ballooned between stat and read can't be pulled fully into
    // memory just to be compared.
    let cur = {
        use std::io::Read;
        let f = fs::File::open(p).map_err(|e| format!("re-open: {e}"))?;
        let mut buf = Vec::new();
        f.take(original.len() as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(|e| format!("re-read: {e}"))?;
        buf
    };
    if cur != original {
        return Err("file changed on disk since it was read — re-read and retry the edit".into());
    }
    Ok(())
}

/// Minimal glob matcher over file *names*: `*` = any run, `?` = one char.
/// Iterative with last-star backtracking — O(p·n) worst case, no
/// exponential recursion on adversarial patterns like `*a*a*a*b`.
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star_p, mut star_n) = (usize::MAX, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = pi;
            star_n = ni;
            pi += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_n += 1;
            ni = star_n;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn is_probably_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

/// Collect workspace files under `root`. Iterative (deep trees can't overflow
/// the stack), never follows symlinks (a symlinked dir would escape the
/// sandbox for reads), skips read-denied names entirely, and stops at
/// `MAX_WALK_ENTRIES` — `true` means truncated.
fn walk(root: &Path, base: &Path, include_hidden: bool, out: &mut Vec<PathBuf>) -> bool {
    let mut stack = vec![root.to_path_buf()];
    // Canonical dirs already queued for descent. The containment check alone
    // lets a junction pointing INSIDE the root (e.g. `a/self -> a`) create a
    // cycle that floods the entry budget with duplicate paths — dedup by the
    // canonical target instead. Seed with the START dir's canonical form
    // (root may be a subdirectory of `base`), not just `base` itself.
    let mut seen: std::collections::HashSet<PathBuf> = [base.to_path_buf()]
        .into_iter()
        .chain(dir_canonical_within(base, root))
        .collect();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if out.len() >= MAX_WALK_ENTRIES {
                return true;
            }
            let name = e.file_name();
            let n = name.to_string_lossy();
            // `file_type` does NOT follow the link — symlinked files and dirs
            // are invisible to bulk traversal. Direct access still goes
            // through `Workspace::resolve` which canonicalizes them safely.
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() || Workspace::name_denied_for_read(&n) {
                continue;
            }
            if !include_hidden && n.starts_with('.') {
                continue;
            }
            let p = e.path();
            out.push(p.strip_prefix(base).unwrap_or(&p).to_path_buf());
            if ft.is_dir()
                && let Some(canon) = dir_canonical_within(base, &p)
                && seen.insert(canon)
            {
                stack.push(p);
            }
        }
    }
    false
}

/// Canonical form of `dir` when it canonically resolves inside `root`,
/// else `None`. `is_symlink()` misses Windows JUNCTIONS (reparse tag
/// MOUNT_POINT, not SYMLINK): a junction pointing outside the root looks
/// like a plain directory — descending would escape the sandbox
/// (`search_documents` would READ outside files). `root` is the canonical
/// verbatim-stripped workspace root — strip the `\\?\` prefix off `dir`'s
/// canonical form too or every descent fails.
fn dir_canonical_within(root: &Path, dir: &Path) -> Option<PathBuf> {
    let c = crate::sandbox::strip_verbatim(&dir.canonicalize().ok()?);
    c.starts_with(root).then_some(c)
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

impl Tools {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }

    /// Per-change commit contract: `commit` (default true) + required
    /// `commit_message` chosen by the agent. Returns `None` when the caller
    /// opted out with `commit:false`.
    fn commit_params<'a>(&self, args: &'a Value) -> Result<Option<&'a str>, String> {
        if opt_bool(args, "commit", true) {
            let msg = req_str(args, "commit_message")?;
            if msg.trim().is_empty() {
                return Err("'commit_message' must not be empty".into());
            }
            // The message lands in argv — control characters (NUL would make
            // the spawn fail deep inside editor-git) are rejected early with
            // a clear error. Newlines/tabs are legitimate (multi-line bodies).
            if msg
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                return Err("'commit_message' must not contain control characters".into());
            }
            // argv length is finite (CreateProcessW caps the command line at
            // 32767 chars) — a huge message would always fail the commit deep
            // in editor-git; reject it here with a clear reason instead.
            if msg.len() > 16 * 1024 {
                return Err("'commit_message' too large (max 16 KiB)".into());
            }
            Ok(Some(msg))
        } else {
            Ok(None)
        }
    }

    /// Commit exactly `paths` (absolute, sandboxed) as one git commit. If the
    /// workspace is not yet inside a repository it is initialized at the root.
    /// A commit failure is reported in the result, not as a tool error — the
    /// file change has already been applied.
    fn commit_changes(&self, paths: &[PathBuf], message: &str) -> Value {
        let mut initialized = false;
        // Hardened handles: the workspace may live inside an untrusted repo
        // whose hooks/fsmonitor/external-diff config must never run.
        let git = match GitCli::open_hardened(self.ws.root()) {
            Ok(g) => g,
            Err(_) => match GitCli::init_hardened(self.ws.root()) {
                Ok(g) => {
                    initialized = true;
                    g
                }
                Err(e) => {
                    return json!({
                        "committed": false,
                        "error": format!("cannot open or initialize a repository: {e}"),
                    });
                }
            },
        };
        // Canonicalize: `rev-parse --show-toplevel` may disagree with the
        // canonical workspace path in case or symlink form, which would
        // make strip_prefix fail and emit an absolute pathspec.
        let repo_root = GitExtended::repo_root(&git)
            .map(PathBuf::from)
            .map(|r| r.canonicalize().unwrap_or(r))
            .unwrap_or_else(|_| git.work_dir().to_path_buf());
        let rels: Vec<String> = paths
            .iter()
            .map(|p| {
                p.strip_prefix(&repo_root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| p.display().to_string())
            })
            .collect();
        let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
        let full_msg = format!(
            "{message}\n\nGenerated-By: womd-mcp/{}",
            crate::MCP_API_VERSION
        );
        match git.commit_paths(&refs, &full_msg) {
            Ok(Some(sha)) => json!({
                "committed": true,
                "sha": sha.0,
                "message": message,
                "repo_initialized": initialized,
            }),
            Ok(None) => json!({
                "committed": false,
                "reason": "nothing to commit for the given paths",
                "repo_initialized": initialized,
            }),
            Err(e) => json!({
                "committed": false,
                "error": e.to_string(),
                "repo_initialized": initialized,
            }),
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.ws
    }

    pub fn call(&self, name: &str, args: &Value) -> Result<Value, String> {
        match name {
            "list_directory" => self.list_directory(args),
            "read_document" => self.read_document(args),
            "document_info" => self.document_info(args),
            "create_document" => self.create_document(args),
            "write_document" => self.write_document(args),
            "append_document" => self.append_document(args),
            "edit_document" => self.edit_document(args),
            "delete_document" => self.delete_document(args),
            "move_document" => self.move_document(args),
            "create_directory" => self.create_directory(args),
            "search_documents" => self.search_documents(args),
            "find_files" => self.find_files(args),
            "markdown_outline" => self.markdown_outline(args),
            "markdown_block_edit" => self.markdown_block_edit(args),
            "markdown_toggle_task" => self.markdown_toggle_task(args),
            "git_status" => self.git_status(args),
            "git_diff" => self.git_diff(args),
            "git_log" => self.git_log(args),
            "api_manifest" => Ok(crate::manifest::manifest_json(
                &self.ws.root().display().to_string(),
            )),
            _ => Err(format!("unknown tool '{name}'")),
        }
    }

    // ------------------------------------------------------------------
    // filesystem
    // ------------------------------------------------------------------

    fn list_directory(&self, args: &Value) -> Result<Value, String> {
        let rel = opt_str(args, "path").unwrap_or(".");
        let depth = opt_usize(args, "depth", 1).min(6);
        let hidden = opt_bool(args, "include_hidden", false);
        let dir = self.ws.resolve_dir(rel).map_err(err)?;
        let mut entries = Vec::new();
        // Canonical ancestors of `dir` — junctions inside the root can point
        // at an ancestor (a/self -> a), which would otherwise duplicate the
        // subtree at every depth level. Not a global set: a dir legitimately
        // reachable from two branches must list its children both times.
        let mut ancestors: std::collections::HashSet<PathBuf> = dir
            .canonicalize()
            .map(|c| crate::sandbox::strip_verbatim(&c))
            .into_iter()
            .collect();
        let truncated = !self.list_into(&dir, rel, depth, hidden, &mut entries, &mut ancestors);
        entries.sort_by(|a, b| {
            let ka = (
                a["type"] != "directory",
                a["path"].as_str().unwrap_or("").to_string(),
            );
            let kb = (
                b["type"] != "directory",
                b["path"].as_str().unwrap_or("").to_string(),
            );
            ka.cmp(&kb)
        });
        Ok(json!({
            "root": rel, "entries": entries,
            "count": entries.len(), "truncated": truncated,
        }))
    }

    /// Returns false when the entry cap was hit. Symlinks are listed as
    /// `type:"symlink"` and never descended (they could point outside).
    fn list_into(
        &self,
        dir: &Path,
        rel: &str,
        depth: usize,
        hidden: bool,
        out: &mut Vec<Value>,
        ancestors: &mut std::collections::HashSet<PathBuf>,
    ) -> bool {
        let Ok(rd) = fs::read_dir(dir) else {
            return true;
        };
        for e in rd.flatten() {
            if out.len() >= MAX_LIST_ENTRIES {
                return false;
            }
            let name = e.file_name();
            let n = name.to_string_lossy();
            if Workspace::name_denied_for_read(&n) {
                continue;
            }
            if !hidden && n.starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            let child_rel = if rel == "." || rel.is_empty() {
                n.to_string()
            } else {
                format!("{rel}/{n}")
            };
            let is_dir = ft.is_dir();
            let ty = if ft.is_symlink() {
                "symlink"
            } else if is_dir {
                "directory"
            } else {
                "file"
            };
            out.push(json!({
                "name": n,
                "path": child_rel,
                "type": ty,
                // `e.metadata()` follows symlinks — it would leak the size of
                // a target outside the root. `symlink_metadata` does not.
                "size": if is_dir { Value::Null } else { json!(p.symlink_metadata().map(|m| m.len()).unwrap_or(0)) },
            }));
            // Junction-aware containment (same escape as `walk`): a
            // MOUNT_POINT reparse is not `is_symlink()` but resolves
            // outside the root — never descend into it. An in-root junction
            // that points at an ancestor also stays a leaf: descending
            // would re-list the same subtree at every depth level.
            if is_dir
                && depth > 0
                && let Some(canon) = dir_canonical_within(self.ws.root(), &p)
                && ancestors.insert(canon.clone())
            {
                let ok = self.list_into(&p, &child_rel, depth - 1, hidden, out, ancestors);
                ancestors.remove(&canon);
                if !ok {
                    return false;
                }
            }
        }
        true
    }

    fn read_document(&self, args: &Value) -> Result<Value, String> {
        let path = self.ws.resolve_file(req_str(args, "path")?).map_err(err)?;
        let text = read_utf8(&path)?;
        let start = opt_usize(args, "start_line", 1).max(1);
        let max = opt_usize(args, "max_lines", 400).min(20_000);
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let total = lines.len();
        // A start past EOF would otherwise produce an inverted response
        // (start_line > end_line) that a paginating client reads as data —
        // say it plainly, like the edit tools' out-of-range line errors.
        if total > 0 && start > total {
            return Err(format!(
                "start_line {start} out of range (total lines {total})"
            ));
        }
        let begin = (start - 1).min(total);
        let end = (begin + max).min(total);
        let mut numbered = String::new();
        for (i, l) in lines[begin..end].iter().enumerate() {
            numbered.push_str(&format!("{:>6}\t{}", begin + i + 1, l));
            if !l.ends_with('\n') {
                numbered.push('\n');
            }
        }
        Ok(json!({
            "path": self.ws.display_rel(&path),
            "total_lines": total,
            "start_line": begin + 1,
            "end_line": end,
            "truncated": end < total,
            "content": numbered,
        }))
    }

    fn document_info(&self, args: &Value) -> Result<Value, String> {
        let p = self.ws.resolve(req_str(args, "path")?).map_err(err)?;
        let md = fs::metadata(&p).map_err(|e| format!("stat: {e}"))?;
        if md.is_dir() {
            return Ok(json!({
                "path": self.ws.display_rel(&p),
                "type": "directory",
            }));
        }
        // Special files (FIFO, socket, device) must never be opened —
        // fs::read on a FIFO blocks forever. Report them as "other".
        if !md.is_file() {
            return Ok(json!({
                "path": self.ws.display_rel(&p),
                "type": "other",
                "size_bytes": md.len(),
            }));
        }
        let bytes = if md.len() <= MAX_TEXT_FILE {
            // Bound the read itself — the stat above races a file that grows
            // between metadata() and open().
            use std::io::Read;
            let f = fs::File::open(&p).map_err(|e| format!("read: {e}"))?;
            let mut buf = Vec::new();
            f.take(MAX_TEXT_FILE + 1)
                .read_to_end(&mut buf)
                .map_err(|e| format!("read: {e}"))?;
            buf
        } else {
            Vec::new()
        };
        let is_text = md.len() <= MAX_TEXT_FILE
            && !is_probably_binary(&bytes)
            && std::str::from_utf8(&bytes).is_ok();
        let lines = if is_text {
            bytes.iter().filter(|&&b| b == b'\n').count()
                + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"))
        } else {
            0
        };
        let modified = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        Ok(json!({
            "path": self.ws.display_rel(&p),
            "type": "file",
            "size_bytes": md.len(),
            "lines": lines,
            "is_text": is_text,
            "modified_epoch_secs": modified,
        }))
    }

    /// Raw file bytes for the `womd://file/` resource — unlike
    /// `read_document` this is not line-numbered and not capped at 20 000
    /// lines (still bound by the 64 MiB file limit inside `read_utf8`).
    pub fn read_document_raw(&self, rel: &str) -> Result<String, String> {
        let p = self.ws.resolve_file(rel).map_err(err)?;
        read_utf8(&p)
    }

    fn create_document(&self, args: &Value) -> Result<Value, String> {
        let commit_msg = self.commit_params(args)?;
        let rel = req_str(args, "path")?;
        let p = self.ws.resolve_write(rel).map_err(err)?;
        if p.exists() && !opt_bool(args, "overwrite", false) {
            return Err(format!(
                "'{rel}' already exists (pass overwrite:true to replace)"
            ));
        }
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        let content = opt_str(args, "content").unwrap_or("");
        check_write_size(content)?;
        editor_storage::atomic_save(&p, content.as_bytes()).map_err(|e| e.to_string())?;
        let mut out =
            json!({ "path": self.ws.display_rel(&p), "bytes": content.len(), "created": true });
        if let Some(m) = commit_msg {
            out["commit"] = self.commit_changes(&[p], m);
        }
        Ok(out)
    }

    fn write_document(&self, args: &Value) -> Result<Value, String> {
        let commit_msg = self.commit_params(args)?;
        let p = self.ws.resolve_write(req_str(args, "path")?).map_err(err)?;
        if p.is_dir() {
            return Err("path is a directory".into());
        }
        // Optional CAS precondition: the agent read the file, wants a full
        // overwrite only if nobody touched it since — same contract as the
        // built-in ensure_unchanged of the read-modify-write tools.
        if let Some(expected) = args.get("expected_content") {
            let expected = expected
                .as_str()
                .ok_or("'expected_content' must be a string")?;
            let cur = if p.is_file() {
                read_utf8(&p)?
            } else {
                return Err(format!(
                    "expected_content set but '{}' does not exist",
                    p.display()
                ));
            };
            if cur != expected {
                return Err(
                    "file changed on disk since your read (expected_content mismatch) — re-read and retry".into(),
                );
            }
            // Re-verify immediately before the save: the compare above and
            // atomic_save are not one operation, so shrink the TOCTOU window
            // to the last possible moment.
            ensure_unchanged(&p, expected.as_bytes())?;
        }
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        let content = req_str(args, "content")?;
        check_write_size(content)?;
        editor_storage::atomic_save(&p, content.as_bytes()).map_err(|e| e.to_string())?;
        let mut out = json!({ "path": self.ws.display_rel(&p), "bytes": content.len() });
        if let Some(m) = commit_msg {
            out["commit"] = self.commit_changes(&[p], m);
        }
        Ok(out)
    }

    fn append_document(&self, args: &Value) -> Result<Value, String> {
        let commit_msg = self.commit_params(args)?;
        let p = self.ws.resolve_write(req_str(args, "path")?).map_err(err)?;
        let content = req_str(args, "content")?;
        check_write_size(content)?;
        if !p.exists() {
            if !opt_bool(args, "create_if_missing", true) {
                return Err("file does not exist".into());
            }
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
            }
            editor_storage::atomic_save(&p, content.as_bytes()).map_err(|e| e.to_string())?;
            let mut out = json!({ "path": self.ws.display_rel(&p), "appended_bytes": content.len(), "created": true });
            if let Some(m) = commit_msg {
                out["commit"] = self.commit_changes(&[p], m);
            }
            return Ok(out);
        }
        // Size check BEFORE the read — a file larger than the limit must be
        // rejected without being pulled into memory at all. Also reject
        // non-regular files here: `p.exists()` is true for a FIFO, and
        // opening one would block until a writer shows up.
        let md = fs::metadata(&p).map_err(|e| format!("stat: {e}"))?;
        if !md.is_file() {
            return Err("path is not a regular file".into());
        }
        if md.len() + content.len() as u64 > MAX_TEXT_FILE {
            return Err(format!(
                "append would exceed the {} MiB file limit",
                MAX_TEXT_FILE / (1024 * 1024)
            ));
        }
        // Bound the read itself: the file can grow between the metadata
        // check above and open() — take() never loads more than MAX+1.
        let src = {
            use std::io::Read;
            let f = fs::File::open(&p).map_err(|e| format!("read: {e}"))?;
            let mut buf = Vec::new();
            f.take(MAX_TEXT_FILE + 1)
                .read_to_end(&mut buf)
                .map_err(|e| format!("read: {e}"))?;
            buf
        };
        if src.len() as u64 + content.len() as u64 > MAX_TEXT_FILE {
            return Err(format!(
                "append would exceed the {} MiB file limit",
                MAX_TEXT_FILE / (1024 * 1024)
            ));
        }
        ensure_unchanged(&p, &src)?;
        // Patch-based append: all original bytes copied verbatim (Invariant 1).
        let patches = [(
            editor_domain::ByteRange::new(
                editor_domain::ByteOffset(src.len() as u64),
                editor_domain::ByteOffset(src.len() as u64),
            ),
            content.as_bytes().to_vec(),
        )];
        editor_storage::streaming_save(&p, &src, &patches).map_err(|e| e.to_string())?;
        let mut out = json!({ "path": self.ws.display_rel(&p), "appended_bytes": content.len() });
        if let Some(m) = commit_msg {
            out["commit"] = self.commit_changes(&[p], m);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // structured edits
    // ------------------------------------------------------------------

    fn edit_document(&self, args: &Value) -> Result<Value, String> {
        let dry_run = opt_bool(args, "dry_run", false);
        // The schema declares minItems:1 — an empty batch is a no-op that
        // would still demand a commit_message and produce an empty commit.
        // Check before commit_params so the error names the real problem.
        let edits = args
            .get("edits")
            .and_then(Value::as_array)
            .ok_or("missing required array parameter 'edits'")?;
        if edits.is_empty() {
            return Err("'edits' must contain at least one edit".into());
        }
        // A dry run writes nothing and therefore must not demand a message.
        let commit_msg = if dry_run {
            None
        } else {
            self.commit_params(args)?
        };
        let rel = req_str(args, "path")?;
        let p = self.ws.resolve_file_write(rel).map_err(err)?;
        let original = read_utf8(&p)?;
        let mut text = original.clone();
        let mut applied = Vec::new();
        for (i, e) in edits.iter().enumerate() {
            let ty = e
                .get("type")
                .and_then(Value::as_str)
                .ok_or("edit missing 'type'")?;
            let before = text.len();
            match ty {
                "replace" => apply_replace(&mut text, e)?,
                "replace_lines" => apply_replace_lines(&mut text, e)?,
                "insert_lines" => apply_insert_lines(&mut text, e)?,
                "delete_lines" => apply_delete_lines(&mut text, e)?,
                "replace_bytes" => apply_replace_bytes(&mut text, e)?,
                other => return Err(format!("unknown edit type '{other}' (edit #{i})")),
            }
            applied.push(
                json!({ "index": i, "type": ty, "delta_bytes": text.len() as i64 - before as i64 }),
            );
            if text.len() as u64 > MAX_TEXT_FILE {
                return Err(format!(
                    "edits would exceed the {} MiB file limit",
                    MAX_TEXT_FILE / (1024 * 1024)
                ));
            }
        }
        if dry_run {
            const PREVIEW: usize = 64 * 1024;
            let preview = if text.len() > PREVIEW {
                // Slice at a char boundary — a raw &text[..PREVIEW] panics
                // when the cap lands inside a multibyte character.
                let mut cut = PREVIEW;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}…[truncated {} bytes]", &text[..cut], text.len() - cut)
            } else {
                text.clone()
            };
            return Ok(json!({
                "path": rel,
                "dry_run": true,
                "changed": text != original,
                "bytes_before": original.len(),
                "bytes_after": text.len(),
                "edits": applied,
                "preview": preview,
            }));
        }
        if text != original {
            ensure_unchanged(&p, original.as_bytes())?;
            editor_storage::atomic_save(&p, text.as_bytes()).map_err(|e| e.to_string())?;
        }
        let mut out = json!({
            "path": self.ws.display_rel(&p),
            "changed": text != original,
            "bytes_before": original.len(),
            "bytes_after": text.len(),
            "edits": applied,
        });
        if let Some(m) = commit_msg {
            out["commit"] = self.commit_changes(&[p], m);
        }
        Ok(out)
    }

    /// Resolve `rel` for operations that must act on the PATH ITSELF —
    /// delete and move-from. `resolve` canonicalizes the whole path, which
    /// turns a symlink leaf into its TARGET: `delete("link")` would then
    /// remove the target's content instead of unlinking the link. Here the
    /// parent resolves through the full policy (traversal, canonicalization,
    /// denied dirs — a symlinked parent escaping the root is still refused)
    /// and only the leaf name is checked and joined verbatim.
    fn resolve_leaf(&self, rel: &str) -> Result<PathBuf, String> {
        let rp = std::path::Path::new(rel);
        let name = rp
            .file_name()
            .ok_or_else(|| "path must name a file or directory".to_string())?;
        let name_str = name.to_string_lossy();
        if Workspace::name_denied_for_write(&name_str) {
            return Err(format!("'{name_str}': access denied"));
        }
        // Reject '.'/'..' leaves too — normalize_rel would have, but we
        // construct the candidate directly.
        if name_str == "." || name_str == ".." {
            return Err("path must name a file or directory".into());
        }
        let parent_rel = rp.parent().unwrap_or_else(|| std::path::Path::new(""));
        let parent = self.ws.resolve_write(parent_rel).map_err(err)?;
        Ok(parent.join(name))
    }

    fn delete_document(&self, args: &Value) -> Result<Value, String> {
        let commit_msg = self.commit_params(args)?;
        let rel = req_str(args, "path")?;
        let p = self.resolve_leaf(rel)?;
        if p == self.ws.root() {
            return Err("the workspace root cannot be deleted".into());
        }
        // symlink_metadata, not exists()/is_dir(): a symlink to a directory
        // is NOT a directory — remove_dir_all on it fails, and a dangling
        // symlink reports !exists() and becomes undeletable. The link itself
        // is always removed via remove_file.
        let Ok(md) = p.symlink_metadata() else {
            return Err(format!("'{rel}' does not exist"));
        };
        if md.file_type().is_symlink() || md.file_type().is_file() {
            fs::remove_file(&p).map_err(|e| format!("delete: {e}"))?;
        } else if md.file_type().is_dir() {
            if opt_bool(args, "recursive", false) {
                fs::remove_dir_all(&p).map_err(|e| format!("delete dir: {e}"))?;
            } else {
                fs::remove_dir(&p)
                    .map_err(|e| format!("delete dir (use recursive:true for non-empty): {e}"))?;
            }
        } else {
            return Err(format!("'{rel}' is not a regular file or directory"));
        }
        let mut out = json!({ "path": rel, "deleted": true });
        if let Some(m) = commit_msg {
            out["commit"] = self.commit_changes(&[p], m);
        }
        Ok(out)
    }

    fn move_document(&self, args: &Value) -> Result<Value, String> {
        let commit_msg = self.commit_params(args)?;
        let from_rel = req_str(args, "from")?;
        let to_rel = req_str(args, "to")?;
        // Both sides use the leaf-literal resolver: moving a symlink must
        // move the LINK, and renaming ONTO a symlink must replace the link —
        // never silently clobber its target.
        let from = self.resolve_leaf(from_rel)?;
        let to = self.resolve_leaf(to_rel)?;
        if from.symlink_metadata().is_err() {
            return Err(format!("'{from_rel}' does not exist"));
        }
        if from == self.ws.root() {
            return Err("the workspace root cannot be moved".into());
        }
        if from == to {
            return Err("'from' and 'to' are the same path".into());
        }
        // Moving a directory into its own subtree errors deep in rename —
        // report it clearly.
        if to.starts_with(&from) {
            return Err("cannot move a directory into itself".into());
        }
        let to_exists = to.symlink_metadata().is_ok();
        if to_exists && !opt_bool(args, "overwrite", false) {
            return Err(format!("'{to_rel}' already exists (pass overwrite:true)"));
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        if to_exists {
            // std::fs::rename refuses to replace an existing destination on
            // Windows — clear it first so `overwrite:true` works everywhere.
            // A non-empty directory still fails (POSIX semantics), which is
            // correct: overwrite must not destroy a tree's contents.
            let tmd = to.symlink_metadata().map_err(|e| format!("stat: {e}"))?;
            let r = if tmd.file_type().is_dir() {
                fs::remove_dir(&to)
            } else {
                fs::remove_file(&to)
            };
            r.map_err(|e| format!("overwrite '{to_rel}': {e}"))?;
        }
        fs::rename(&from, &to).map_err(|e| format!("rename: {e}"))?;
        let mut out = json!({ "from": from_rel, "to": to_rel, "moved": true });
        if let Some(m) = commit_msg {
            out["commit"] = self.commit_changes(&[from, to], m);
        }
        Ok(out)
    }

    fn create_directory(&self, args: &Value) -> Result<Value, String> {
        let commit_msg = self.commit_params(args)?;
        let p = self.ws.resolve_write(req_str(args, "path")?).map_err(err)?;
        fs::create_dir_all(&p).map_err(|e| format!("mkdir: {e}"))?;
        let mut out = json!({ "path": self.ws.display_rel(&p), "created": true });
        if let Some(m) = commit_msg {
            // Git does not track empty directories; the commit reports
            // `nothing to commit` unless the dir already contains files.
            out["commit"] = self.commit_changes(&[p], m);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // search
    // ------------------------------------------------------------------

    fn find_files(&self, args: &Value) -> Result<Value, String> {
        let rel = opt_str(args, "path").unwrap_or(".");
        let pattern = opt_str(args, "pattern").unwrap_or("*");
        if pattern.len() > 4 * 1024 {
            return Err("pattern too large (max 4 KiB)".into());
        }
        let max = opt_usize(args, "max_results", 200).min(2000);
        let dir = self.ws.resolve_dir(rel).map_err(err)?;
        let mut all = Vec::new();
        let walk_truncated = walk(&dir, self.ws.root(), true, &mut all);
        let mut matches: Vec<String> = all
            .iter()
            .filter_map(|p| {
                let name = p.file_name()?.to_string_lossy();
                glob_match(pattern, &name).then(|| p.display().to_string())
            })
            .collect();
        matches.sort();
        // `== max` is ambiguous: exactly `max` hits with nothing left is a
        // complete result, while `> max` is genuinely truncated.
        let over_cap = matches.len() > max;
        matches.truncate(max);
        Ok(json!({
            "pattern": pattern, "count": matches.len(), "files": matches,
            "truncated": walk_truncated || over_cap,
        }))
    }

    fn search_documents(&self, args: &Value) -> Result<Value, String> {
        let rel = opt_str(args, "path").unwrap_or(".");
        let query = req_str(args, "query")?;
        if query.len() > 64 * 1024 {
            return Err("query too large (max 64 KiB)".into());
        }
        let use_regex = opt_bool(args, "regex", false);
        let case = opt_bool(args, "case_sensitive", true);
        let file_pattern = opt_str(args, "file_pattern");
        if let Some(p) = file_pattern {
            if p.len() > 4 * 1024 {
                return Err("file_pattern too large (max 4 KiB)".into());
            }
        }
        let ctx = opt_usize(args, "context_lines", 0).min(5);
        let max = opt_usize(args, "max_results", 100).min(1000);
        let dir = self.ws.resolve_dir(rel).map_err(err)?;

        let re = if use_regex {
            Some(
                regex::RegexBuilder::new(query)
                    .case_insensitive(!case)
                    .build()
                    .map_err(|e| format!("invalid regex: {e}"))?,
            )
        } else {
            None
        };
        let needle = if case {
            query.to_string()
        } else {
            query.to_lowercase()
        };

        let mut files = Vec::new();
        let walk_truncated = walk(&dir, self.ws.root(), false, &mut files);
        let mut matches = Vec::new();
        let mut scanned = 0usize;
        let mut truncated = walk_truncated;
        'files: for rel_path in files {
            let abs = self.ws.root().join(&rel_path);
            // Cheap pre-checks before the expensive read: a >64 MiB file
            // must not be materialized just to be skipped.
            let Ok(fmd) = abs.metadata() else { continue };
            if !fmd.is_file() || fmd.len() > MAX_TEXT_FILE {
                continue;
            }
            let name = rel_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Some(pat) = file_pattern {
                if !glob_match(pat, &name) {
                    continue;
                }
            }
            scanned += 1;
            if scanned > MAX_SEARCH_FILES {
                truncated = true;
                break 'files;
            }
            // Bound the read itself — the metadata check above races a file
            // that grows between stat and open.
            let bytes = {
                use std::io::Read;
                let Ok(f) = fs::File::open(&abs) else {
                    continue;
                };
                let mut buf = Vec::new();
                if f.take(MAX_TEXT_FILE + 1).read_to_end(&mut buf).is_err() {
                    continue;
                }
                buf
            };
            if bytes.len() as u64 > MAX_TEXT_FILE {
                continue;
            }
            if is_probably_binary(&bytes) {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let hit = if let Some(re) = &re {
                    re.is_match(line)
                } else if case {
                    line.contains(&needle)
                } else {
                    line.to_lowercase().contains(&needle)
                };
                if hit {
                    // Check BEFORE pushing: `max_results: 0` must yield an
                    // empty list, not one match (push-then-check would let a
                    // single hit through any zero limit).
                    if matches.len() >= max {
                        truncated = true;
                        break 'files;
                    }
                    let mut m = json!({
                        "path": rel_path.display().to_string(),
                        "line": i + 1,
                        "text": line.trim_end(),
                    });
                    if ctx > 0 {
                        let lo = i.saturating_sub(ctx);
                        let hi = (i + ctx + 1).min(lines.len());
                        m["context"] = json!(
                            lines[lo..hi]
                                .iter()
                                .map(|l| l.trim_end())
                                .collect::<Vec<_>>()
                        );
                        m["context_start_line"] = json!(lo + 1);
                    }
                    matches.push(m);
                    if matches.len() >= max {
                        // More matches almost certainly remain — the client
                        // must know the list is incomplete, not just full.
                        truncated = true;
                        break 'files;
                    }
                }
            }
        }
        Ok(json!({
            "query": query, "count": matches.len(), "matches": matches,
            "files_scanned": scanned, "truncated": truncated,
        }))
    }

    // ------------------------------------------------------------------
    // markdown-aware tools
    // ------------------------------------------------------------------

    fn markdown_outline(&self, args: &Value) -> Result<Value, String> {
        let p = self.ws.resolve_file(req_str(args, "path")?).map_err(err)?;
        let text = read_utf8(&p)?;
        let doc = editor_markdown::parse(text.as_bytes(), MarkdownProfile::Gfm)
            .map_err(|e| format!("parse failed: {e:?}"))?;
        let starts = line_starts(&text);
        let blocks: Vec<Value> = doc
            .blocks
            .iter()
            .enumerate()
            .map(|(i, b)| block_json(i, b, &text, &starts))
            .collect();
        Ok(json!({
            "path": self.ws.display_rel(&p),
            "block_count": doc.blocks.len(),
            "blocks": blocks,
        }))
    }

    fn markdown_block_edit(&self, args: &Value) -> Result<Value, String> {
        let commit_msg = self.commit_params(args)?;
        let rel = req_str(args, "path")?;
        let p = self.ws.resolve_file_write(rel).map_err(err)?;
        let text = read_utf8(&p)?;
        let doc = editor_markdown::parse(text.as_bytes(), MarkdownProfile::Gfm)
            .map_err(|e| format!("parse failed: {e:?}"))?;
        let op = args
            .get("op")
            .ok_or("missing required object parameter 'op'")?;
        let ty = op
            .get("type")
            .and_then(Value::as_str)
            .ok_or("op missing 'type'")?;
        let n = doc.blocks.len();
        let span_of = |i: usize| -> Result<(u64, u64), String> {
            doc.blocks
                .get(i)
                .map(|b| (b.span().start.0, b.span().end.0))
                .ok_or_else(|| format!("block index {i} out of range (block_count={n})"))
        };
        let get_u = |k: &str| -> Result<usize, String> { req_usize(op, k, "op") };
        let (range, replacement) = match ty {
            "replace_block" => {
                let (s, e) = span_of(get_u("index")?)?;
                (s..e, ensure_block_newline(block_content(op)?, e, &text)?)
            }
            "replace_block_range" => {
                let start = get_u("start")?;
                let end = get_u("end")?;
                if end == 0 || end > n {
                    return Err(format!(
                        "end {end} out of range (block_count={n}, exclusive)"
                    ));
                }
                // start must precede end — otherwise `s..e` has start > end
                // and `end - start` underflows u64 (panic in debug builds).
                if start >= end {
                    return Err(format!(
                        "start {start} must be less than end {end} (block_count={n}, end exclusive)"
                    ));
                }
                let (s, _) = span_of(start)?;
                let (_, e) = span_of(end - 1)?;
                (s..e, ensure_block_newline(block_content(op)?, e, &text)?)
            }
            "delete_block" => {
                let (s, e) = span_of(get_u("index")?)?;
                (s..e, Vec::new())
            }
            "insert_block" => {
                let at = get_u("at")?;
                if at > n {
                    return Err(format!("'at' {at} out of range (block_count={n})"));
                }
                let mut content = block_content(op)?;
                if !content.ends_with(b"\n") {
                    content.push(b'\n');
                }
                let off = if at == n {
                    // Appending at EOF must not glue the block onto a partial
                    // last line — guarantee a newline separator.
                    if !text.is_empty() && !text.ends_with('\n') {
                        content.insert(0, b'\n');
                    }
                    text.len() as u64
                } else {
                    doc.blocks[at].span().start.0
                };
                (off..off, content)
            }
            other => return Err(format!("unknown op type '{other}'")),
        };
        // Same ceiling as the other writers: a result larger than MAX_TEXT_FILE
        // would make the file unreadable to every subsequent tool call.
        let result_len = text.len() as u64 - (range.end - range.start) + replacement.len() as u64;
        if result_len > MAX_TEXT_FILE {
            return Err(format!(
                "block edit would exceed the {} MiB file limit",
                MAX_TEXT_FILE / (1024 * 1024)
            ));
        }
        let patches = [(
            editor_domain::ByteRange::new(
                editor_domain::ByteOffset(range.start),
                editor_domain::ByteOffset(range.end),
            ),
            replacement,
        )];
        ensure_unchanged(&p, text.as_bytes())?;
        editor_storage::streaming_save(&p, text.as_bytes(), &patches).map_err(|e| e.to_string())?;
        let mut out = json!({
            "path": self.ws.display_rel(&p),
            "op": ty,
            "byte_range": [range.start, range.end],
            "saved": true,
        });
        if let Some(m) = commit_msg {
            out["commit"] = self.commit_changes(&[p], m);
        }
        Ok(out)
    }

    fn markdown_toggle_task(&self, args: &Value) -> Result<Value, String> {
        let commit_msg = self.commit_params(args)?;
        let rel = req_str(args, "path")?;
        let p = self.ws.resolve_file_write(rel).map_err(err)?;
        let text = read_utf8(&p)?;
        let line_no = req_usize(args, "line", "toggle_task")?;
        let starts = line_starts(&text);
        if line_no == 0 || line_no > starts.len() {
            return Err(format!(
                "line {line_no} out of range (total {})",
                starts.len()
            ));
        }
        let line_off = starts[line_no - 1] as u64;
        let doc = editor_markdown::parse(text.as_bytes(), MarkdownProfile::Gfm)
            .map_err(|e| format!("parse failed: {e:?}"))?;
        // Find the deepest task list item whose span contains this line.
        let mut best: Option<(u64, u64)> = None; // (span_start, span_len)
        find_task(&doc.blocks, line_off, &mut best);
        let (item_start, _len) =
            best.ok_or_else(|| format!("no task-list item ([ ]/[x]) contains line {line_no}"))?;
        let hay_end = (item_start + 256).min(text.len() as u64);
        let hay = &text.as_bytes()[item_start as usize..hay_end as usize];
        let idx = find_marker(hay).ok_or("task marker not found near item start")?;
        let abs = item_start as usize + idx;
        let cur = text.as_bytes()[abs + 1];
        let new_byte = if cur == b' ' { b'x' } else { b' ' };
        let patches = [(
            editor_domain::ByteRange::new(
                editor_domain::ByteOffset(abs as u64 + 1),
                editor_domain::ByteOffset(abs as u64 + 2),
            ),
            vec![new_byte],
        )];
        ensure_unchanged(&p, text.as_bytes())?;
        editor_storage::streaming_save(&p, text.as_bytes(), &patches).map_err(|e| e.to_string())?;
        let mut out = json!({
            "path": self.ws.display_rel(&p),
            "line": line_no,
            "now": if new_byte == b'x' { "done" } else { "open" },
        });
        if let Some(m) = commit_msg {
            out["commit"] = self.commit_changes(&[p], m);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // read-only git
    // ------------------------------------------------------------------

    /// Open the repository fresh each call — a mutating tool may have just
    /// `git init`'d the workspace, so a cached "not a repo" would be stale.
    fn git(&self) -> Result<GitCli, String> {
        GitCli::open_hardened(self.ws.root())
            .map_err(|_| "workspace root is not inside a git work tree".to_string())
    }

    /// Workspace prefix relative to the repository root ("" when the
    /// workspace IS the repo root). Git queries are scoped to this prefix so
    /// an agent can never see files outside the sandbox even when the
    /// workspace sits inside a larger repository.
    fn ws_prefix(&self, git: &GitCli) -> String {
        GitExtended::repo_root(git)
            .map(PathBuf::from)
            .ok()
            .map(|root| root.canonicalize().unwrap_or(root))
            .and_then(|root| {
                self.ws
                    .root()
                    .strip_prefix(&root)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .ok()
            })
            .unwrap_or_default()
    }

    /// Repo-root-relative path of an already-sandboxed absolute path —
    /// `:(top,literal)` pathspecs in `editor-git` anchor at the repo root,
    /// so working-dir-relative paths would misfire when the workspace is a
    /// subdirectory of a larger repository.
    fn repo_rel(&self, git: &GitCli, abs: &Path) -> String {
        GitExtended::repo_root(git)
            .map(PathBuf::from)
            .ok()
            .map(|root| root.canonicalize().unwrap_or(root))
            .and_then(|root| {
                abs.strip_prefix(&root)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .ok()
            })
            .unwrap_or_else(|| abs.display().to_string())
    }

    fn git_status(&self, _args: &Value) -> Result<Value, String> {
        let git = self.git()?;
        let st = git.status().map_err(|e| e.to_string())?;
        let prefix = self.ws_prefix(&git);
        let in_ws = |c: &&editor_git::FileChange| {
            prefix.is_empty()
                || c.path == prefix
                || c.path.starts_with(&format!("{prefix}/"))
                || c.old_path
                    .as_ref()
                    .map(|o| o == &prefix || o.starts_with(&format!("{prefix}/")))
                    .unwrap_or(false)
        };
        let map_change = |c: &editor_git::FileChange| {
            json!({
                "path": c.path,
                "status": format!("{:?}", c.status),
                "old_path": c.old_path,
            })
        };
        let staged: Vec<Value> = st.staged.iter().filter(in_ws).map(map_change).collect();
        let unstaged: Vec<Value> = st.changes.iter().filter(in_ws).map(map_change).collect();
        let untracked: Vec<Value> = st.untracked.iter().filter(in_ws).map(map_change).collect();
        let conflicted: Vec<Value> = st.conflicted.iter().filter(in_ws).map(map_change).collect();
        // `dirty` must describe the SCOPED lists — reporting repo-wide dirt
        // while the lists show nothing would be a lie the agent can't verify.
        let dirty = !(staged.is_empty()
            && unstaged.is_empty()
            && untracked.is_empty()
            && conflicted.is_empty());
        Ok(json!({
            "branch": st.head_branch,
            "dirty": dirty,
            "scope": if prefix.is_empty() { ".".to_string() } else { prefix.clone() },
            "staged": staged,
            "unstaged": unstaged,
            "untracked": untracked,
            "conflicted": conflicted,
        }))
    }

    fn git_diff(&self, args: &Value) -> Result<Value, String> {
        let git = self.git()?;
        if let Some(rel) = opt_str(args, "path") {
            // resolve (not resolve_file): a diff on a deleted-but-tracked
            // file is meaningful — the path only needs to be inside the
            // sandbox, not currently on disk.
            let abs = self.ws.resolve(rel).map_err(err)?;
            let rel_to_repo = self.repo_rel(&git, &abs);
            let raw = git.diff_file_raw(&rel_to_repo).map_err(|e| e.to_string())?;
            return Ok(json!({ "path": rel, "diff": raw }));
        }
        let staged = opt_bool(args, "staged", false);
        // `-- .` scopes the diff to the workspace subtree (work_dir == root).
        let raw = if staged {
            // `--cached` without a commit-ish diffs the index against the
            // EMPTY tree on an unborn HEAD — the old fallback (`diff -- .`)
            // silently returned the *unstaged* diff under `staged:true`.
            git.exec_text(&[
                "diff",
                "--no-ext-diff",
                "--no-color",
                "--cached",
                "HEAD",
                "--",
                ".",
            ])
            .or_else(|_| {
                git.exec_text(&["diff", "--no-ext-diff", "--no-color", "--cached", "--", "."])
            })
        } else {
            git.exec_text(&["diff", "--no-ext-diff", "--no-color", "HEAD", "--", "."])
                .or_else(|_| git.exec_text(&["diff", "--no-ext-diff", "--no-color", "--", "."]))
        }
        .map_err(|e| e.to_string())?;
        Ok(json!({ "staged": staged, "diff": raw }))
    }

    fn git_log(&self, args: &Value) -> Result<Value, String> {
        let git = self.git()?;
        let max = opt_usize(args, "max_count", 20).min(200);
        let entries = if let Some(rel) = opt_str(args, "path") {
            // Same as git_diff — history of a deleted file is legitimate.
            let abs = self.ws.resolve(rel).map_err(err)?;
            let rel_to_repo = self.repo_rel(&git, &abs);
            editor_git::GitExtended::log_for_file(&git, &rel_to_repo, max)
        } else {
            // Scoped to the workspace subtree: `-- <prefix>` limits the log
            // to commits touching paths under it.
            let prefix = self.ws_prefix(&git);
            if prefix.is_empty() {
                editor_git::GitExtended::log(&git, max)
            } else {
                editor_git::GitExtended::log_for_file(&git, &prefix, max)
            }
        }
        .map_err(|e| e.to_string())?;
        let list: Vec<Value> = entries
            .iter()
            .map(|e| {
                json!({
                    "sha": e.sha, "short_sha": e.short_sha, "author": e.author,
                    "date": e.date, "message": e.message,
                })
            })
            .collect();
        Ok(json!({ "count": list.len(), "commits": list }))
    }
}

// ---------------------------------------------------------------------------
// edit op implementations
// ---------------------------------------------------------------------------

fn apply_replace(text: &mut String, e: &Value) -> Result<(), String> {
    let search = e
        .get("search")
        .and_then(Value::as_str)
        .ok_or("replace: missing 'search'")?;
    let replacement = e.get("replace").and_then(Value::as_str).unwrap_or("");
    if search.is_empty() {
        return Err("replace: 'search' must not be empty".into());
    }
    let count = text.matches(search).count();
    if let Some(expected) = e.get("expected_count").and_then(Value::as_u64) {
        if count as u64 != expected {
            return Err(format!(
                "replace: expected {expected} occurrence(s), found {count} — refusing"
            ));
        }
    }
    if count == 0 {
        return Err("replace: 'search' text not found".into());
    }
    match e
        .get("occurrence")
        .and_then(Value::as_str)
        .unwrap_or("first")
    {
        "all" => *text = text.replace(search, replacement),
        "first" => *text = text.replacen(search, replacement, 1),
        "last" => {
            let idx = text.rfind(search).unwrap();
            text.replace_range(idx..idx + search.len(), replacement);
        }
        other => return Err(format!("replace: invalid occurrence '{other}'")),
    }
    Ok(())
}

fn apply_replace_lines(text: &mut String, e: &Value) -> Result<(), String> {
    let (s, e_line) = line_range(text, e, false)?;
    let mut content = e
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // If the replaced region isn't at EOF, the new content must end with a
    // line boundary or it would glue onto the following line.
    if e_line < text.len() && !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    text.replace_range(s..e_line, &content);
    Ok(())
}

fn apply_insert_lines(text: &mut String, e: &Value) -> Result<(), String> {
    let line = req_usize(e, "line", "insert_lines")?;
    let mut content = e
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let starts = line_starts(text);
    if line == 0 || line > starts.len() + 1 {
        return Err(format!(
            "insert_lines: line {line} out of range (1..={})",
            starts.len() + 1
        ));
    }
    let at = if line == starts.len() + 1 {
        text.len()
    } else {
        starts[line - 1]
    };
    if at > 0 && !text[..at].ends_with('\n') {
        content.insert(0, '\n');
    }
    if at < text.len() && !content.ends_with('\n') {
        content.push('\n');
    }
    text.insert_str(at, &content);
    Ok(())
}

fn apply_delete_lines(text: &mut String, e: &Value) -> Result<(), String> {
    let (s, end) = line_range(text, e, true)?;
    text.replace_range(s..end, "");
    Ok(())
}

fn line_range(text: &str, e: &Value, _del: bool) -> Result<(usize, usize), String> {
    let s = req_usize(e, "start_line", "lines")?;
    let el = req_usize(e, "end_line", "lines")?;
    let starts = line_starts(text);
    let total = starts.len();
    if s == 0 || el == 0 || s > total || el > total || s > el {
        return Err(format!(
            "invalid line range {s}..={el} (total lines {total})"
        ));
    }
    let start_b = starts[s - 1];
    let end_b = if el == total { text.len() } else { starts[el] };
    Ok((start_b, end_b))
}

fn apply_replace_bytes(text: &mut String, e: &Value) -> Result<(), String> {
    let s = req_usize(e, "start", "replace_bytes")?;
    let en = req_usize(e, "end", "replace_bytes")?;
    let content = e.get("content").and_then(Value::as_str).unwrap_or("");
    if s > en || en > text.len() || !text.is_char_boundary(s) || !text.is_char_boundary(en) {
        return Err(format!("replace_bytes: invalid range {s}..{en}"));
    }
    text.replace_range(s..en, content);
    Ok(())
}

// ---------------------------------------------------------------------------
// markdown helpers
// ---------------------------------------------------------------------------

fn block_content(op: &Value) -> Result<Vec<u8>, String> {
    Ok(op
        .get("content")
        .and_then(Value::as_str)
        .ok_or("op missing string 'content'")?
        .as_bytes()
        .to_vec())
}

/// A replacement that isn't at EOF must end on a line boundary — otherwise
/// the following block glues onto the replacement's last line and the
/// markdown is corrupted (e.g. replacing a paragraph mid-document with
/// "x" produces "x## Next heading"). Empty content is a delete — allowed.
fn ensure_block_newline(mut content: Vec<u8>, end: u64, text: &str) -> Result<Vec<u8>, String> {
    if !content.is_empty()
        && (end as usize) < text.len()
        && !content.ends_with(b"\n")
        && !text.as_bytes()[(end as usize)..].starts_with(b"\n")
    {
        content.push(b'\n');
    }
    Ok(content)
}

fn block_kind_name(b: &editor_markdown::Block) -> &'static str {
    use editor_markdown::Block::*;
    match b {
        BlankLine(_) => "blank_line",
        Paragraph(_) => "paragraph",
        Heading(_) => "heading",
        ThematicBreak(_) => "thematic_break",
        BlockQuote(_) => "block_quote",
        List(_) => "list",
        CodeBlock(_) => "code_block",
        Table(_) => "table",
        HtmlBlock(_) => "html_block",
        LinkReferenceDefinition(_) => "link_reference_definition",
        UnknownBlock(_) => "unknown",
    }
}

fn block_json(i: usize, b: &editor_markdown::Block, src: &str, starts: &[usize]) -> Value {
    let sp = b.span();
    let (s, e) = (sp.start.0 as usize, sp.end.0 as usize);
    let mut o = json!({
        "index": i,
        "kind": block_kind_name(b),
        "byte_span": [sp.start.0, sp.end.0],
        "start_line": line_of(starts, s),
        "end_line": line_of(starts, e.saturating_sub(1).max(s)),
    });
    use editor_markdown::Block::*;
    match b {
        Heading(h) => {
            // Defensive: a parser span landing on a non-boundary index would
            // panic and kill the server — clamp instead.
            let raw = src.get(s..e).unwrap_or("");
            let text_part = raw
                .trim()
                .trim_start_matches('#')
                .trim()
                .trim_end_matches('#')
                .trim_end();
            o["level"] = json!(h.level);
            o["text"] = json!(text_part);
        }
        List(l) => {
            o["ordered"] = json!(l.ordered);
            o["items"] = json!(l.items.len());
            if l.ordered {
                o["start"] = json!(l.start);
            }
        }
        CodeBlock(c) => {
            o["fenced"] = json!(c.fenced);
            o["info_string"] = json!(c.info_string);
        }
        Table(t) => {
            o["rows"] = json!(t.rows.len());
        }
        BlockQuote(q) => {
            o["children"] = json!(q.children.len());
        }
        _ => {}
    }
    o
}

fn find_task(blocks: &[editor_markdown::Block], line_off: u64, best: &mut Option<(u64, u64)>) {
    // Iterative, not recursive: list children nest as deep as the parser
    // allows — an explicit stack keeps the server safe even if that cap
    // ever changes.
    let mut stack: Vec<&editor_markdown::Block> = blocks.iter().collect();
    while let Some(b) = stack.pop() {
        if let editor_markdown::Block::List(l) = b {
            for item in &l.items {
                let sp = item.meta.span;
                if item.task.is_some() && sp.start.0 <= line_off && line_off < sp.end.0 {
                    let len = sp.len();
                    if best.map(|(_, l)| len < l).unwrap_or(true) {
                        *best = Some((sp.start.0, len));
                    }
                }
                stack.extend(item.children.iter());
            }
        }
    }
}

fn find_marker(hay: &[u8]) -> Option<usize> {
    hay.windows(3)
        .position(|w| w == b"[ ]" || w == b"[x]" || w == b"[X]")
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup() -> (tempfile::TempDir, Tools) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, Tools::new(ws))
    }

    fn call(t: &Tools, name: &str, args: Value) -> Result<Value, String> {
        t.call(name, &args)
    }

    #[test]
    fn create_read_edit_roundtrip() {
        let (_d, t) = setup();
        call(&t, "create_document", json!({"path": "notes/a.md", "content": "# Hi\n\nline one\nline two\n", "commit": false})).unwrap();
        let r = call(&t, "read_document", json!({"path": "notes/a.md"})).unwrap();
        assert!(r["content"].as_str().unwrap().contains("line one"));
        let r = call(
            &t,
            "edit_document",
            json!({
                "path": "notes/a.md",
                "commit": false,
                "edits": [{"type": "replace", "search": "one", "replace": "1"}]
            }),
        )
        .unwrap();
        assert_eq!(r["changed"], true);
        let r = call(&t, "read_document", json!({"path": "notes/a.md"})).unwrap();
        assert!(r["content"].as_str().unwrap().contains("line 1"));
    }

    /// A start_line past EOF must be a clear error, not an inverted
    /// start_line > end_line response a paginating client reads as data.
    #[test]
    fn read_document_start_past_eof_errors() {
        let (_d, t) = setup();
        call(
            &t,
            "create_document",
            json!({"path": "s.md", "content": "one\ntwo\n", "commit": false}),
        )
        .unwrap();
        let err = call(
            &t,
            "read_document",
            json!({"path": "s.md", "start_line": 99}),
        )
        .expect_err("start past EOF must error");
        assert!(err.contains("out of range"), "{err}");
        // In-range pagination still works, and the last page is truthful.
        let r = call(
            &t,
            "read_document",
            json!({"path": "s.md", "start_line": 2}),
        )
        .unwrap();
        assert_eq!(r["start_line"], 2);
        assert_eq!(r["end_line"], 2);
        assert_eq!(r["truncated"], false);
    }

    #[test]
    fn create_refuses_overwrite_by_default() {
        let (_d, t) = setup();
        call(
            &t,
            "create_document",
            json!({"path": "a.md", "content": "x", "commit": false}),
        )
        .unwrap();
        assert!(
            call(
                &t,
                "create_document",
                json!({"path": "a.md", "content": "y", "commit": false})
            )
            .is_err()
        );
        call(
            &t,
            "create_document",
            json!({"path": "a.md", "content": "y", "overwrite": true, "commit": false}),
        )
        .unwrap();
    }

    #[test]
    fn edit_replace_expected_count() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({"path": "a.txt", "content": "x x x", "commit": false}),
        )
        .unwrap();
        assert!(call(&t, "edit_document", json!({
            "path": "a.txt", "commit": false,
            "edits": [{"type": "replace", "search": "x", "replace": "y", "expected_count": 2}]
        })).is_err());
        call(
            &t,
            "edit_document",
            json!({
                "path": "a.txt", "commit": false,
                "edits": [{"type": "replace", "search": "x", "replace": "y", "occurrence": "all"}]
            }),
        )
        .unwrap();
        assert_eq!(read_utf8(&t.ws.root().join("a.txt")).unwrap(), "y y y");
    }

    #[test]
    fn edit_line_ops() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({"path": "l.txt", "content": "a\nb\nc\n", "commit": false}),
        )
        .unwrap();
        call(
            &t,
            "edit_document",
            json!({
                "path": "l.txt", "commit": false,
                "edits": [{"type": "insert_lines", "line": 2, "content": "X"}]
            }),
        )
        .unwrap();
        assert_eq!(
            read_utf8(&t.ws.root().join("l.txt")).unwrap(),
            "a\nX\nb\nc\n"
        );
        call(
            &t,
            "edit_document",
            json!({
                "path": "l.txt", "commit": false,
                "edits": [{"type": "delete_lines", "start_line": 3, "end_line": 3}]
            }),
        )
        .unwrap();
        assert_eq!(read_utf8(&t.ws.root().join("l.txt")).unwrap(), "a\nX\nc\n");
        call(&t, "edit_document", json!({
            "path": "l.txt", "commit": false,
            "edits": [{"type": "replace_lines", "start_line": 1, "end_line": 2, "content": "p\nq"}]
        })).unwrap();
        assert_eq!(read_utf8(&t.ws.root().join("l.txt")).unwrap(), "p\nq\nc\n");
    }

    #[test]
    fn dry_run_does_not_write() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({"path": "d.txt", "content": "keep", "commit": false}),
        )
        .unwrap();
        let r = call(
            &t,
            "edit_document",
            json!({
                "path": "d.txt", "dry_run": true,
                "edits": [{"type": "replace", "search": "keep", "replace": "changed"}]
            }),
        )
        .unwrap();
        assert!(r["preview"].as_str().unwrap().contains("changed"));
        assert_eq!(read_utf8(&t.ws.root().join("d.txt")).unwrap(), "keep");
    }

    #[test]
    fn move_and_delete() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({"path": "m.txt", "content": "x", "commit": false}),
        )
        .unwrap();
        call(
            &t,
            "move_document",
            json!({"from": "m.txt", "to": "sub/m2.txt", "commit": false}),
        )
        .unwrap();
        assert!(t.ws.root().join("sub/m2.txt").exists());
        call(
            &t,
            "delete_document",
            json!({"path": "sub", "recursive": true, "commit": false}),
        )
        .unwrap();
        assert!(!t.ws.root().join("sub").exists());
    }

    #[test]
    fn search_finds_literal_and_glob() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({"path": "s/a.md", "content": "hello world\n", "commit": false}),
        )
        .unwrap();
        call(
            &t,
            "write_document",
            json!({"path": "s/b.txt", "content": "hello txt\n", "commit": false}),
        )
        .unwrap();
        let r = call(
            &t,
            "search_documents",
            json!({"query": "hello", "file_pattern": "*.md"}),
        )
        .unwrap();
        assert_eq!(r["count"], 1);
        let r = call(&t, "find_files", json!({"pattern": "*.txt"})).unwrap();
        assert_eq!(r["count"], 1);
    }

    /// `max_results: 0` must return zero matches — the cap check runs BEFORE
    /// the match is pushed, not after (a push-then-check would leak one hit
    /// past a zero limit).
    #[test]
    fn search_max_results_zero_returns_empty() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({"path": "a.md", "content": "needle here\n", "commit": false}),
        )
        .unwrap();
        let r = call(
            &t,
            "search_documents",
            json!({"query": "needle", "max_results": 0}),
        )
        .unwrap();
        assert_eq!(r["count"], 0);
        assert!(r["matches"].as_array().unwrap().is_empty());
        assert_eq!(r["truncated"], true);
    }

    /// The schema declares `edits: minItems 1` — an empty edit array is a
    /// no-op that must be rejected, not silently succeed (and it must not
    /// demand a commit_message for a change that cannot exist).
    #[test]
    fn edit_document_rejects_empty_edits() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({"path": "a.md", "content": "x\n", "commit": false}),
        )
        .unwrap();
        // Without commit:false an empty batch must still fail on the
        // edits check rather than demanding a message.
        let r = call(&t, "edit_document", json!({"path": "a.md", "edits": []}));
        let msg = r.expect_err("empty edits must fail");
        assert!(msg.contains("'edits'"), "got: {msg}");
        // And with commit:false too.
        let r = call(
            &t,
            "edit_document",
            json!({"path": "a.md", "edits": [], "commit": false}),
        );
        assert!(r.is_err());
    }

    #[test]
    fn markdown_outline_and_block_edit() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({
                "path": "doc.md",
                "content": "# Title\n\npara one\n\n## Sub\n\npara two\n",
                "commit": false
            }),
        )
        .unwrap();
        let r = call(&t, "markdown_outline", json!({"path": "doc.md"})).unwrap();
        let blocks = r["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["kind"], "heading");
        assert_eq!(blocks[0]["text"], "Title");
        // Replace the paragraph block containing "para one".
        let idx = blocks
            .iter()
            .position(|b| b["kind"] == "paragraph")
            .unwrap();
        call(
            &t,
            "markdown_block_edit",
            json!({
                "path": "doc.md", "commit": false,
                "op": {"type": "replace_block", "index": idx, "content": "para UNO\n"}
            }),
        )
        .unwrap();
        let text = read_utf8(&t.ws.root().join("doc.md")).unwrap();
        assert!(text.contains("para UNO"));
        assert!(text.contains("## Sub"));
        assert!(!text.contains("para one"));
    }

    #[test]
    fn insert_block_appends() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({"path": "i.md", "content": "# T\n", "commit": false}),
        )
        .unwrap();
        let r = call(&t, "markdown_outline", json!({"path": "i.md"})).unwrap();
        let n = r["block_count"].as_u64().unwrap();
        call(
            &t,
            "markdown_block_edit",
            json!({
                "path": "i.md", "commit": false,
                "op": {"type": "insert_block", "at": n, "content": "\nappended paragraph\n"}
            }),
        )
        .unwrap();
        let text = read_utf8(&t.ws.root().join("i.md")).unwrap();
        assert!(text.contains("appended paragraph"));
    }

    /// Inserting a block at the very end of a file whose last line has no
    /// trailing newline must not glue the new block onto that line.
    #[test]
    fn insert_block_at_eof_separates_partial_last_line() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({
                "path": "p.md", "content": "last line without newline", "commit": false
            }),
        )
        .unwrap();
        let r = call(&t, "markdown_outline", json!({"path": "p.md"})).unwrap();
        let n = r["block_count"].as_u64().unwrap();
        call(
            &t,
            "markdown_block_edit",
            json!({
                "path": "p.md", "commit": false,
                "op": {"type": "insert_block", "at": n, "content": "new block\n"}
            }),
        )
        .unwrap();
        let text = read_utf8(&t.ws.root().join("p.md")).unwrap();
        assert!(
            text.contains("without newline\nnew block"),
            "block must start on its own line, got: {}",
            &text[text.len().saturating_sub(60)..]
        );
    }

    /// Replacing a mid-document block with content that lacks a trailing
    /// newline must not glue the next block onto the replacement's last
    /// line — that corrupts the markdown ("x## Next heading").
    #[test]
    fn replace_block_keeps_following_block_on_its_own_line() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({
                "path": "g.md", "content": "# Head\n\nfirst para\n\nsecond para\n",
                "commit": false
            }),
        )
        .unwrap();
        call(
            &t,
            "markdown_block_edit",
            json!({
                "path": "g.md", "commit": false,
                // No trailing \n in the replacement. The paragraph's span
                // includes the blank line after it, so [2]=first para+blank.
                "op": {"type": "replace_block", "index": 2, "content": "REPLACED"}
            }),
        )
        .unwrap();
        let text = read_utf8(&t.ws.root().join("g.md")).unwrap();
        assert!(
            text.contains("REPLACED\nsecond para"),
            "following block must stay on its own line, got: {text}"
        );
        assert!(!text.contains("REPLACEDsecond"), "glued output: {text}");
    }

    /// `replace_block_range` with start >= end produced a range whose
    /// `end - start` underflowed u64 — a panic in debug builds, and in
    /// release a wrapped-around giant `result_len` that only accidentally
    /// errored via the size cap. Reject it up front.
    #[test]
    fn replace_block_range_rejects_inverted_range() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({
                "path": "r.md", "content": "# A\n\np1\n\n## B\n\np2\n", "commit": false
            }),
        )
        .unwrap();
        for (start, end) in [(3usize, 2usize), (2usize, 2usize)] {
            let e = call(
                &t,
                "markdown_block_edit",
                json!({
                    "path": "r.md", "commit": false,
                    "op": {"type": "replace_block_range", "start": start, "end": end, "content": "x\n"}
                }),
            )
            .unwrap_err();
            assert!(
                e.contains("must be less than end"),
                "start={start} end={end}: {e}"
            );
        }
        // File untouched.
        let text = read_utf8(&t.ws.root().join("r.md")).unwrap();
        assert!(text.contains("## B"));
        // A valid range still works.
        call(
            &t,
            "markdown_block_edit",
            json!({
                "path": "r.md", "commit": false,
                "op": {"type": "replace_block_range", "start": 0, "end": 2, "content": "# Merged\n\n"}
            }),
        )
        .unwrap();
        let text = read_utf8(&t.ws.root().join("r.md")).unwrap();
        assert!(text.contains("# Merged"));
        assert!(text.contains("p2"));
    }

    #[test]
    fn toggle_task_flips_marker() {
        let (_d, t) = setup();
        call(
            &t,
            "write_document",
            json!({
                "path": "tasks.md",
                "content": "- [ ] open task\n- [x] done task\n",
                "commit": false
            }),
        )
        .unwrap();
        call(
            &t,
            "markdown_toggle_task",
            json!({"path": "tasks.md", "line": 1, "commit": false}),
        )
        .unwrap();
        let text = read_utf8(&t.ws.root().join("tasks.md")).unwrap();
        assert!(text.starts_with("- [x] open task"));
        call(
            &t,
            "markdown_toggle_task",
            json!({"path": "tasks.md", "line": 2, "commit": false}),
        )
        .unwrap();
        let text = read_utf8(&t.ws.root().join("tasks.md")).unwrap();
        assert!(text.contains("- [ ] done task"));
    }

    #[test]
    fn traversal_blocked_in_tools() {
        let (_d, t) = setup();
        assert!(call(&t, "read_document", json!({"path": "../x"})).is_err());
        assert!(
            call(
                &t,
                "write_document",
                json!({"path": "..\\x", "content": "y", "commit": false})
            )
            .is_err()
        );
        assert!(call(&t, "read_document", json!({"path": "/etc/hosts"})).is_err());
    }

    #[test]
    fn manifest_lists_all_dispatched_tools() {
        let (_d, t) = setup();
        for spec in crate::manifest::tool_specs() {
            // Dispatch must recognize every advertised name (arg errors are
            // fine — 'unknown tool' is not).
            let res = t.call(spec.name, &json!({}));
            if let Err(msg) = res {
                assert!(
                    !msg.contains("unknown tool"),
                    "{} not dispatched",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn api_manifest_reports_version() {
        let (_d, t) = setup();
        let r = call(&t, "api_manifest", json!({})).unwrap();
        assert_eq!(r["api_version"], crate::MCP_API_VERSION);
        assert!(r["capabilities"]["tools"].as_array().unwrap().len() >= 15);
        // Mutating tools advertise the commit contract.
        let tools = r["capabilities"]["tools"].as_array().unwrap();
        let write = tools
            .iter()
            .find(|t| t["name"] == "write_document")
            .unwrap();
        assert!(write["inputSchema"]["properties"]["commit_message"].is_object());
        // latest revision is expected_content (3.1.0), not the 3.0.0 baseline
        assert_eq!(write["revised_in"], "3.1.0");
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn commit_message_required_by_default() {
        let (_d, t) = setup();
        // Default commit:true without a message must refuse BEFORE writing.
        assert!(
            call(
                &t,
                "write_document",
                json!({"path": "x.md", "content": "x"})
            )
            .is_err()
        );
        assert!(!t.ws.root().join("x.md").exists());
        call(
            &t,
            "write_document",
            json!({"path": "x.md", "content": "x", "commit": false}),
        )
        .unwrap();
    }

    #[test]
    fn commit_message_length_capped() {
        let (_d, t) = setup();
        // argv caps the Windows command line at ~32k chars — a bigger
        // message would die deep inside editor-git; refuse it up front.
        let huge = "m".repeat(17 * 1024);
        let r = call(
            &t,
            "write_document",
            json!({"path": "x.md", "content": "x", "commit_message": huge}),
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("too large"));
        // A 16 KiB message is accepted (commit may still fail without git,
        // but the arg itself is legal).
        let ok = "m".repeat(16 * 1024);
        let r = call(
            &t,
            "write_document",
            json!({"path": "y.md", "content": "x", "commit_message": ok}),
        );
        assert!(r.is_ok(), "{:?}", r.err());
    }

    #[test]
    fn each_change_creates_agent_named_commit() {
        if !git_available() {
            return;
        }
        let (_d, t) = setup();
        let r = call(
            &t,
            "create_document",
            json!({
                "path": "a.md", "content": "one\n", "commit_message": "add a.md draft"
            }),
        )
        .unwrap();
        assert_eq!(r["commit"]["committed"], true);
        assert_eq!(r["commit"]["repo_initialized"], true);
        let r = call(
            &t,
            "edit_document",
            json!({
                "path": "a.md",
                "edits": [{"type": "replace", "search": "one", "replace": "two"}],
                "commit_message": "polish wording"
            }),
        )
        .unwrap();
        assert_eq!(r["commit"]["committed"], true);
        let git = GitCli::open(t.ws.root()).unwrap();
        let log = GitExtended::log(&git, 10).unwrap();
        assert_eq!(log.len(), 2);
        assert!(log.iter().any(|e| e.message.contains("polish wording")));
        assert!(log.iter().any(|e| e.message.contains("add a.md draft")));
    }

    #[test]
    fn git_diff_staged_on_unborn_head_shows_index() {
        if !git_available() {
            return;
        }
        let (_d, t) = setup();
        let git = GitCli::init_hardened(t.ws.root()).unwrap();
        // Write directly (no commit) and stage it — repo still has no HEAD.
        fs::write(t.ws.root().join("s.md"), "staged content\n").unwrap();
        git.exec_text(&["add", "--", "s.md"]).unwrap();
        // staged:true must show index-vs-empty-tree, not silently fall back
        // to the unstaged diff (which would be empty for a staged file).
        let r = call(&t, "git_diff", json!({"staged": true})).unwrap();
        let d = r["diff"].as_str().unwrap();
        assert!(d.contains("s.md"), "staged diff missing file: {d}");
        assert!(
            d.contains("staged content"),
            "staged diff missing blob: {d}"
        );
    }

    #[test]
    fn move_and_delete_are_committed() {
        if !git_available() {
            return;
        }
        let (_d, t) = setup();
        call(
            &t,
            "create_document",
            json!({
                "path": "d.md", "content": "x\n", "commit_message": "add d"
            }),
        )
        .unwrap();
        let r = call(
            &t,
            "move_document",
            json!({
                "from": "d.md", "to": "e.md", "commit_message": "rename d to e"
            }),
        )
        .unwrap();
        assert_eq!(r["commit"]["committed"], true);
        let r = call(
            &t,
            "delete_document",
            json!({
                "path": "e.md", "commit_message": "remove e"
            }),
        )
        .unwrap();
        assert_eq!(r["commit"]["committed"], true);
        let git = GitCli::open(t.ws.root()).unwrap();
        let log = GitExtended::log(&git, 10).unwrap();
        assert_eq!(log.len(), 3);
    }

    // ------------------------------------------------------------------
    // API 3.0.0: write policy, compare-and-save, traversal hardening
    // ------------------------------------------------------------------

    #[test]
    fn executable_writes_are_denied_by_tools() {
        let (_d, t) = setup();
        for (tool, args) in [
            (
                "create_document",
                json!({"path": "evil.exe", "content": "MZ", "commit": false}),
            ),
            (
                "write_document",
                json!({"path": "evil.ps1", "content": "x", "commit": false}),
            ),
            (
                "write_document",
                json!({"path": "build.rs", "content": "fn main(){}", "commit": false}),
            ),
            (
                "write_document",
                json!({"path": ".env", "content": "SECRET=1", "commit": false}),
            ),
            (
                "write_document",
                json!({"path": ".vscode/settings.json", "content": "{}", "commit": false}),
            ),
            (
                "write_document",
                json!({"path": ".gitattributes", "content": "* filter=x", "commit": false}),
            ),
            (
                "write_document",
                json!({"path": ".gitmodules", "content": "[submodule]", "commit": false}),
            ),
            ("create_directory", json!({"path": ".ssh", "commit": false})),
            (
                "move_document",
                json!({"from": "a.md", "to": "x.bat", "commit": false}),
            ),
        ] {
            assert!(call(&t, tool, args).is_err(), "{tool} should deny");
        }
        assert!(!t.ws.root().join("evil.exe").exists());
        assert!(!t.ws.root().join(".env").exists());
    }

    #[test]
    fn reads_of_denied_names_fail() {
        let (d, t) = setup();
        std::fs::write(d.path().join(".env"), b"SECRET=1").unwrap();
        std::fs::write(d.path().join("id_rsa"), b"PRIVATE").unwrap();
        assert!(call(&t, "read_document", json!({"path": ".env"})).is_err());
        assert!(call(&t, "read_document", json!({"path": "id_rsa"})).is_err());
        // …and they never show up in listings either.
        let r = call(
            &t,
            "list_directory",
            json!({"path": ".", "include_hidden": true}),
        )
        .unwrap();
        let names: Vec<&str> = r["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["name"].as_str())
            .collect();
        assert!(!names.contains(&".env") && !names.contains(&"id_rsa"));
    }

    /// A human save (or another writer) landing between the tool's read and
    /// its save must abort the call — never silently clobber the file.
    #[test]
    fn edit_refuses_to_clobber_external_change() {
        let (d, t) = setup();
        let f = d.path().join("doc.md");
        std::fs::write(&f, b"base content\n").unwrap();
        // Simulate "the file changed under us" by racing a write between the
        // internal read and save: easiest deterministic check — call the CAS
        // helper directly via an edit whose original no longer matches.
        // Public path: append_document validates the same invariant.
        let r = call(
            &t,
            "append_document",
            json!({
                "path": "doc.md", "content": "more\n", "commit": false
            }),
        );
        assert!(r.is_ok());
        // Now corrupt-then-edit: write a file, then edit it normally works.
        std::fs::write(&f, b"line1\nline2\n").unwrap();
        let r = call(
            &t,
            "edit_document",
            json!({
                "path": "doc.md",
                "edits": [{"type": "replace", "search": "line1", "replace": "LINE1"}],
                "commit": false
            }),
        );
        assert!(r.is_ok());
        assert_eq!(read_utf8(&f).unwrap(), "LINE1\nline2\n");
        // Direct CAS unit check.
        assert!(ensure_unchanged(&f, b"LINE1\nline2\n").is_ok());
        assert!(ensure_unchanged(&f, b"stale").is_err());
    }

    /// Non-regular files (FIFOs, devices, dirs) must never be opened for
    /// reading — a FIFO has len 0, passes the size cap, and open() then
    /// blocks forever waiting for a writer. append/read/CAS all reject via
    /// the metadata check before touching the file.
    #[test]
    fn non_regular_files_are_rejected_before_open() {
        let (d, t) = setup();
        std::fs::create_dir(d.path().join("d.md")).unwrap();
        // append_document on an existing DIRECTORY must fail cleanly.
        let r = call(
            &t,
            "append_document",
            json!({"path": "d.md", "content": "x", "commit": false}),
        );
        assert!(r.is_err());
        assert!(
            r.unwrap_err().contains("not a regular file"),
            "expected regular-file rejection"
        );
        // The CAS helper on a directory never opens it either.
        assert!(ensure_unchanged(&d.path().join("d.md"), b"").is_err());
    }

    /// Bulk traversal never follows symlinks — an in-workspace link to an
    /// outside directory must not leak outside filenames or contents.
    #[cfg(unix)]
    #[test]
    fn walk_and_search_ignore_symlinked_dirs() {
        let (d, t) = setup();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), b"needle-content").unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("escape")).unwrap();
        std::fs::write(d.path().join("inside.md"), b"needle-content").unwrap();

        let r = call(&t, "find_files", json!({"pattern": "*.md"})).unwrap();
        let files: Vec<&str> = r["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f.as_str())
            .collect();
        assert!(
            files.iter().all(|f| !f.contains("escape")),
            "symlinked dir must not be walked: {files:?}"
        );

        let r = call(&t, "search_documents", json!({"query": "needle-content"})).unwrap();
        let hits: Vec<&str> = r["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["path"].as_str())
            .collect();
        assert_eq!(hits, vec!["inside.md"]);

        // The symlink itself appears in listings as type "symlink", unopened.
        let r = call(&t, "list_directory", json!({"path": "."})).unwrap();
        let e = r["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "escape")
            .unwrap();
        assert_eq!(e["type"], "symlink");
    }

    /// Windows JUNCTIONS (reparse tag MOUNT_POINT) are NOT `is_symlink()` —
    /// they look like real directories to `file_type()`. A junction pointing
    /// outside the root must still never be descended: `search_documents`
    /// would otherwise READ files outside the sandbox.
    #[cfg(windows)]
    #[test]
    fn walk_and_search_ignore_junction_dirs() {
        let (d, t) = setup();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), b"needle-content").unwrap();
        let link = d.path().join("escape");
        // `mklink /j` needs no elevation, unlike symlink creation.
        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/j"])
            .arg(&link)
            .arg(outside.path())
            .output();
        match out {
            Ok(o) if o.status.success() => {}
            _ => return, // mklink unavailable — nothing to verify here
        }
        // Sanity: the reparse point is exactly the case `is_symlink()` misses.
        if link
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(true)
        {
            return; // toolchain reports it as a symlink — covered elsewhere
        }
        std::fs::write(d.path().join("inside.md"), b"needle-content").unwrap();

        let r = call(&t, "find_files", json!({"pattern": "*.md"})).unwrap();
        let files: Vec<&str> = r["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f.as_str())
            .collect();
        assert!(
            files.iter().all(|f| !f.contains("escape")),
            "junction dir must not be walked: {files:?}"
        );

        let r = call(&t, "search_documents", json!({"query": "needle-content"})).unwrap();
        let hits: Vec<&str> = r["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["path"].as_str())
            .collect();
        assert_eq!(hits, vec!["inside.md"]);
    }

    /// A junction pointing INSIDE the root passes the containment check but
    /// forms a cycle (`sub/loop -> sub`). Traversal must dedup by canonical
    /// target — otherwise the walk floods the entry budget with duplicates
    /// of the same subtree and `truncated` hides real files.
    #[cfg(windows)]
    #[test]
    fn walk_dedups_in_root_junction_cycles() {
        let (d, t) = setup();
        let sub = d.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("doc.md"), b"needle").unwrap();
        std::fs::write(d.path().join("top.md"), b"needle").unwrap();
        let link = sub.join("loop");
        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/j"])
            .arg(&link)
            .arg(&sub)
            .output();
        match out {
            Ok(o) if o.status.success() => {}
            _ => return, // mklink unavailable — nothing to verify here
        }
        if link
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(true)
        {
            return; // reported as symlink — the cycle never forms
        }

        // find_files must not emit duplicated `sub/loop/loop/...` paths.
        let r = call(&t, "find_files", json!({"pattern": "*.md"})).unwrap();
        assert_eq!(
            r["truncated"], false,
            "junction cycle must not flood the entry budget"
        );
        let files: Vec<&str> = r["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f.as_str())
            .collect();
        assert!(
            files.iter().all(|f| !f.contains("loop")),
            "no paths through the junction cycle: {files:?}"
        );
        assert!(files.contains(&"top.md") && files.contains(&"sub/doc.md"));

        // Same from a SUBDIRECTORY start: `find_files`/`search_documents` on
        // "sub" begin the walk inside `sub` — the junction `sub/loop` points
        // back at the start dir itself, which must already be in `seen`
        // (seeded), or the whole subtree is listed a second time.
        let r = call(&t, "find_files", json!({"pattern": "*.md", "path": "sub"})).unwrap();
        let files: Vec<&str> = r["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f.as_str())
            .collect();
        assert_eq!(
            files,
            vec!["doc.md"],
            "starting inside a junction-cycled dir must not duplicate: {files:?}"
        );

        // list_directory at depth must not nest the subtree repeatedly.
        let r = call(&t, "list_directory", json!({"path": ".", "depth": 6})).unwrap();
        let listed: Vec<&str> = r["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["path"].as_str())
            .collect();
        assert!(
            listed.iter().all(|p| !p.contains("loop/")),
            "no descent through the junction: {listed:?}"
        );
    }

    #[test]
    fn glob_match_is_linear_not_exponential() {
        // Would hang the old recursive matcher.
        let pat = "*a*a*a*a*a*a*a*b";
        let name = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaac";
        assert!(!glob_match(pat, name));
        assert!(glob_match("*.md", "notes.md"));
        assert!(glob_match("doc-?.txt", "doc-7.txt"));
        assert!(!glob_match("doc-?.txt", "doc-77.txt"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*z", "az"));
        assert!(glob_match("a*z", "a middle z"));
    }

    /// dry_run truncates the preview at the 64 KiB cap — the cut must land on
    /// a char boundary even when a multibyte character straddles it.
    /// Regression: a naive `&text[..PREVIEW]` panicked there.
    #[test]
    fn dry_run_preview_never_splits_a_char() {
        let (d, t) = setup();
        // '€' is 3 bytes starting at byte 65535, straddling the 65536 cap.
        let text = format!("{}{}{}", "a".repeat(65535), "€", "b".repeat(100));
        std::fs::write(d.path().join("big.md"), &text).unwrap();
        let r = call(
            &t,
            "edit_document",
            json!({
                "path": "big.md",
                "edits": [{"type": "replace", "search": "b", "replace": "c", "occurrence": "all"}],
                "dry_run": true
            }),
        );
        let r = r.expect("dry_run on multibyte boundary must not panic/err");
        let preview = r["preview"].as_str().unwrap();
        assert!(preview.contains("truncated"));
        // File untouched — dry run.
        assert_eq!(read_utf8(&d.path().join("big.md")).unwrap(), text);
    }

    /// `expected_content` (API 3.1.0) makes write_document compare-and-save:
    /// a mismatched precondition or a missing file refuses, never clobbers.
    #[test]
    fn write_document_expected_content_is_cas() {
        let (d, t) = setup();
        std::fs::write(d.path().join("w.md"), b"current\n").unwrap();
        assert!(
            call(
                &t,
                "write_document",
                json!({
                    "path": "w.md", "content": "new", "expected_content": "stale", "commit": false
                })
            )
            .is_err()
        );
        assert_eq!(read_utf8(&d.path().join("w.md")).unwrap(), "current\n");

        call(
            &t,
            "write_document",
            json!({
                "path": "w.md", "content": "new\n", "expected_content": "current\n", "commit": false
            }),
        )
        .unwrap();
        assert_eq!(read_utf8(&d.path().join("w.md")).unwrap(), "new\n");

        // Precondition on a non-existent file must not create it silently.
        assert!(
            call(
                &t,
                "write_document",
                json!({
                    "path": "nope.md", "content": "x", "expected_content": "", "commit": false
                })
            )
            .is_err()
        );
    }

    #[test]
    fn move_document_guards_are_clear() {
        let (d, t) = setup();
        std::fs::create_dir_all(d.path().join("a")).unwrap();
        std::fs::write(d.path().join("a/f.md"), b"x").unwrap();
        assert!(
            call(
                &t,
                "move_document",
                json!({
                    "from": "a/f.md", "to": "a/f.md", "commit": false
                })
            )
            .is_err()
        );
        // A directory can never be moved into its own subtree.
        assert!(
            call(
                &t,
                "move_document",
                json!({
                    "from": "a", "to": "a/b", "commit": false
                })
            )
            .is_err()
        );
        assert!(d.path().join("a/f.md").exists());
    }

    /// Special files (FIFOs…) are reported as "other" — opening one would
    /// block the server thread forever.
    #[cfg(unix)]
    #[test]
    fn document_info_reports_special_files_without_reading() {
        let (d, t) = setup();
        let fifo = d.path().join("pipe");
        let ok = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return; // mkfifo unavailable — nothing to assert
        }
        let r = call(&t, "document_info", json!({"path": "pipe"})).unwrap();
        assert_eq!(r["type"], "other");
    }

    /// delete_document must unlink symlinks — including dangling ones and
    /// links to directories — without ever touching the target's content.
    #[cfg(unix)]
    #[test]
    fn delete_document_unlinks_symlinks_safely() {
        let (d, t) = setup();
        let target = d.path().join("target-dir");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.md"), b"keep").unwrap();
        std::os::unix::fs::symlink(&target, d.path().join("link")).unwrap();
        std::os::unix::fs::symlink(d.path().join("nowhere"), d.path().join("dangling")).unwrap();

        call(
            &t,
            "delete_document",
            json!({"path": "link", "commit": false}),
        )
        .unwrap();
        assert!(
            target.join("keep.md").exists(),
            "target content must survive"
        );
        assert!(d.path().join("link").symlink_metadata().is_err());

        call(
            &t,
            "delete_document",
            json!({"path": "dangling", "commit": false}),
        )
        .unwrap();
        assert!(d.path().join("dangling").symlink_metadata().is_err());
    }

    /// `overwrite:true` must actually work on Windows — std::fs::rename
    /// refuses to replace an existing destination there.
    #[test]
    fn move_overwrite_replaces_existing_file() {
        let (d, t) = setup();
        std::fs::write(d.path().join("a.md"), "new").unwrap();
        std::fs::write(d.path().join("b.md"), "old").unwrap();
        // Without overwrite → refusal.
        assert!(
            call(
                &t,
                "move_document",
                json!({
                    "from": "a.md", "to": "b.md", "commit": false
                })
            )
            .is_err()
        );
        call(
            &t,
            "move_document",
            json!({
                "from": "a.md", "to": "b.md", "overwrite": true, "commit": false
            }),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("b.md")).unwrap(),
            "new"
        );
        assert!(!d.path().join("a.md").exists());
        // Moving a file onto a NON-EMPTY directory must fail, not empty it.
        let dir = d.path().join("full");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("keep.txt"), "keep").unwrap();
        std::fs::write(d.path().join("c.md"), "c").unwrap();
        assert!(
            call(
                &t,
                "move_document",
                json!({
                    "from": "c.md", "to": "full", "overwrite": true, "commit": false
                })
            )
            .is_err()
        );
        assert!(dir.join("keep.txt").exists(), "non-empty dir must survive");
    }

    /// move_document must move the LINK, never the target — and renaming
    /// onto a symlink must unlink it, never clobber the target.
    #[cfg(unix)]
    #[test]
    fn move_document_operates_on_links_not_targets() {
        let (d, t) = setup();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("real.md"), "real").unwrap();
        std::os::unix::fs::symlink(outside.path().join("real.md"), d.path().join("link")).unwrap();

        // Moving a dangling/valid symlink moves the link itself.
        call(
            &t,
            "move_document",
            json!({
                "from": "link", "to": "moved-link", "commit": false
            }),
        )
        .unwrap();
        assert!(outside.path().join("real.md").exists(), "target untouched");
        let md = d.path().join("moved-link").symlink_metadata().unwrap();
        assert!(
            md.file_type().is_symlink(),
            "the link moved, not the target"
        );

        // Renaming onto a symlink unlinks it — the target must not be overwritten.
        std::os::unix::fs::symlink(outside.path().join("real.md"), d.path().join("dest")).unwrap();
        std::fs::write(d.path().join("src.md"), "src").unwrap();
        call(
            &t,
            "move_document",
            json!({
                "from": "src.md", "to": "dest", "overwrite": true, "commit": false
            }),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.path().join("real.md")).unwrap(),
            "real",
            "target must not be clobbered by the rename"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("dest")).unwrap(),
            "src"
        );
    }

    /// Writers must refuse results over MAX_TEXT_FILE — otherwise the file
    /// becomes unreadable to every tool (all reads enforce the same limit)
    /// and the agent can no longer touch its own document.
    #[test]
    fn writers_refuse_results_over_the_file_limit() {
        let (d, t) = setup();
        // One byte under the limit so the file itself is still readable.
        let big = vec![b'x'; (MAX_TEXT_FILE - 1) as usize];
        std::fs::write(d.path().join("big.txt"), &big).unwrap();

        let r = call(
            &t,
            "append_document",
            json!({
                "path": "big.txt", "content": "ab", "commit": false
            }),
        );
        let e = r.unwrap_err();
        assert!(e.contains("limit"), "append err: {e}");

        let r = call(
            &t,
            "edit_document",
            json!({
                "path": "big.txt", "commit": false,
                "edits": [{ "type": "insert_lines", "line": 1, "content": "ab" }]
            }),
        );
        let e = r.unwrap_err();
        assert!(e.contains("limit"), "edit err: {e}");

        let r = call(
            &t,
            "markdown_block_edit",
            json!({
                "path": "big.txt", "commit": false,
                "op": { "type": "insert_block", "at": 0, "content": "ab\n" }
            }),
        );
        let e = r.unwrap_err();
        assert!(e.contains("limit"), "block_edit err: {e}");

        // Untouched — the CAS guard must not have written anything.
        assert_eq!(
            std::fs::metadata(d.path().join("big.txt")).unwrap().len(),
            MAX_TEXT_FILE - 1
        );
    }
}
