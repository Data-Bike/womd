//! GitHub `RepositoryHostAdapter` (§34): uses the `gh` CLI for GitHub API operations
//! (PRs, branches, metadata). Falls back to `NotSupported` if `gh` is not installed.
//!
//! The `gh` CLI handles authentication via `gh auth login` (stored in the OS keychain),
//! so secrets never enter the editor process (§87). A future `reqwest`-backed impl can
//! replace this behind the trait without touching callers.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use editor_domain::ids::ProviderId;
use editor_plugin_api::{
    AdapterError, AuthSession, PullRequest, RemoteBranch, RemoteUrlTarget, RepositoryHostAdapter,
    RepositoryMetadata,
};

/// GitHub adapter backed by the `gh` CLI.
pub struct GithubAdapter {
    provider: ProviderId,
    work_dir: PathBuf,
    gh_bin: String,
}

impl GithubAdapter {
    pub fn new(work_dir: impl Into<PathBuf>) -> Self {
        Self {
            provider: ProviderId::from_str_unchecked("github"),
            work_dir: work_dir.into(),
            gh_bin: resolve_on_path("gh", "GH_BIN"),
        }
    }

    fn gh_available(&self) -> bool {
        run_with_timeout(&self.gh_bin, &self.work_dir, &["--version"], PROBE_TIMEOUT)
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn gh_text(&self, args: &[&str]) -> Result<String, AdapterError> {
        if !self.gh_available() {
            return Err(AdapterError::Other("gh CLI not installed".to_string()));
        }
        let out = run_with_timeout(&self.gh_bin, &self.work_dir, args, CMD_TIMEOUT)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            Err(AdapterError::Other(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ))
        }
    }

    fn git_text(&self, args: &[&str]) -> Result<String, AdapterError> {
        let git = resolve_on_path("git", "GIT_BIN");
        let out = run_with_timeout(&git, &self.work_dir, args, CMD_TIMEOUT)?;
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
/// a `gh.exe`/`git.exe` planted inside the repository (e.g. a malicious
/// checkout) would run instead of the real binary. An absolute path removes
/// the working directory from the search entirely (same hardening as
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

const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
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

/// `Command::output()` has no timeout — a wedged network call or a credential
/// prompt on a non-existent terminal hangs forever. Drain both pipes on
/// reader threads (a full pipe buffer blocks a healthy child, which then
/// looks like a hang), poll `try_wait`, and kill on deadline.
fn run_with_timeout(
    bin: &str,
    dir: &Path,
    args: &[&str],
    timeout: std::time::Duration,
) -> Result<std::process::Output, AdapterError> {
    run_with_timeout_capped(bin, dir, args, timeout, MAX_OUTPUT)
}

fn run_with_timeout_capped(
    bin: &str,
    dir: &Path,
    args: &[&str],
    timeout: std::time::Duration,
    cap: usize,
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
    let out_drain = spawn_pipe_drain(child.stdout.take().expect("piped"), cap);
    let err_drain = spawn_pipe_drain(child.stderr.take().expect("piped"), cap);
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

impl RepositoryHostAdapter for GithubAdapter {
    fn provider_id(&self) -> ProviderId {
        self.provider.clone()
    }

    fn authenticate(&self) -> Result<AuthSession, AdapterError> {
        if !self.gh_available() {
            return Err(AdapterError::Other("gh CLI not installed".to_string()));
        }
        // `gh auth status` exits 0 when logged in but writes its report to
        // STDERR — stdout is empty either way, so `gh_text` would always
        // report "not authenticated".
        let out = run_with_timeout(
            &self.gh_bin,
            &self.work_dir,
            &["auth", "status"],
            CMD_TIMEOUT,
        )?;
        let text = String::from_utf8_lossy(&out.stderr).to_string();
        if out.status.success() && (text.contains("Logged in") || text.contains("account ")) {
            Ok(AuthSession {
                provider: self.provider.clone(),
                user: extract_user(&text),
                token: String::new(),
            })
        } else {
            Err(AdapterError::Auth("not authenticated with gh".to_string()))
        }
    }

    fn repository_metadata(&self) -> Result<RepositoryMetadata, AdapterError> {
        let text = self.gh_text(&[
            "repo",
            "view",
            "--json",
            "nameWithOwner,defaultBranchRef,url",
        ])?;
        let full_name = extract_json_field(&text, "nameWithOwner");
        let default_branch = extract_json_field(&text, "defaultBranchRef");
        let html_url = extract_json_field(&text, "url");
        Ok(RepositoryMetadata {
            full_name,
            default_branch,
            html_url,
        })
    }

    fn pull_requests(&self) -> Result<Vec<PullRequest>, AdapterError> {
        let text = self.gh_text(&["pr", "list", "--json", "number,title,state,url"])?;
        let mut prs = Vec::new();
        // gh emits a compact JSON array on a single line — splitting on lines
        // only ever saw the first PR. Split into top-level objects instead.
        for obj in split_json_objects(&text) {
            if obj.contains("\"number\"") {
                let number = extract_json_int(obj, "number");
                let title = extract_json_field(obj, "title");
                let state = extract_json_field(obj, "state");
                let html_url = extract_json_field(obj, "url");
                prs.push(PullRequest {
                    number,
                    title,
                    state,
                    html_url,
                });
            }
        }
        Ok(prs)
    }

    fn create_pull_request(&self, title: &str, body: &str) -> Result<PullRequest, AdapterError> {
        let text = self.gh_text(&[
            "pr",
            "create",
            "--title",
            title,
            "--body",
            body,
            "--json",
            "number,url",
        ])?;
        let number = extract_json_int(&text, "number");
        let html_url = extract_json_field(&text, "url");
        Ok(PullRequest {
            number,
            title: title.to_string(),
            state: "open".to_string(),
            html_url,
        })
    }

    fn remote_branches(&self) -> Result<Vec<RemoteBranch>, AdapterError> {
        let git_text = self.git_text(&["branch", "-r", "--list"])?;
        let mut branches = Vec::new();
        for line in git_text.lines() {
            let name = line.trim().to_string();
            if !name.is_empty() && !name.contains(" -> ") {
                branches.push(RemoteBranch {
                    name,
                    sha: String::new(),
                });
            }
        }
        Ok(branches)
    }

    fn open_remote_url(&self, _target: RemoteUrlTarget) {}
}

/// Split a JSON array of objects into the object texts without a full JSON
/// parser: scans top-level `{...}` regions tracking string literals and escape
/// sequences so braces inside strings don't confuse the depth counter.
fn split_json_objects(json: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut in_str = false;
    let mut escape = false;
    for (i, c) in json.char_indices() {
        if in_str {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    out.push(&json[start..=i]);
                }
            }
            _ => {}
        }
    }
    out
}

/// Find the index of the closing quote of a JSON string value, honoring `\`
/// escapes (a bare `find('"')` stops early on an escaped `\"`).
fn json_string_end(rest: &str) -> Option<usize> {
    let mut escape = false;
    for (i, c) in rest.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if c == '\\' {
            escape = true;
            continue;
        }
        if c == '"' {
            return Some(i);
        }
    }
    None
}

/// Minimal JSON string unescape, in a single left-to-right pass.
/// Sequential `replace` calls are wrong: `\\n` (an escaped backslash
/// followed by a literal 'n') matches the `\n` rule and produces a newline
/// instead of `\n` text — the escapes must be consumed strictly in order.
fn json_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{C}'),
            Some('"') => out.push('"'),
            Some('/') => out.push('/'),
            Some('\\') => out.push('\\'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                let code = u32::from_str_radix(&hex, 16).unwrap_or(0xFFFD);
                // Surrogates can't form a `char`; lone/invalid escapes
                // degrade to U+FFFD rather than being dropped silently.
                out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
            }
            // Unknown escape: keep both bytes so nothing is silently eaten.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Find `needle` in `json` outside string literals. A plain `str::find` also
/// matches inside a value: a PR titled `x","state":"MERGED` would inject a
/// forged `"state":"MERGED` that the extractor then reads as a real field —
/// `gh pr list` carries remote user input, so keys must only be recognized
/// at structural positions.
fn find_outside_strings(json: &str, needle: &str) -> Option<usize> {
    let bytes = json.as_bytes();
    let n = needle.as_bytes();
    let mut in_str = false;
    let mut escape = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            // A key position is also a string start — try the needle first:
            // `"title":` both opens a string and is a key.
            if bytes[i..].starts_with(n) {
                return Some(i);
            }
            in_str = true;
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(n) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Extract a string field from simple JSON (no serde dep for the adapter skeleton).
fn extract_json_field(json: &str, field: &str) -> String {
    let needle = format!("\"{field}\":\"");
    if let Some(start) = find_outside_strings(json, &needle) {
        let rest = &json[start + needle.len()..];
        if let Some(end) = json_string_end(rest) {
            return json_unescape(&rest[..end]);
        }
    }
    let needle2 = format!("\"{field}\":{{\"name\":\"");
    if let Some(start) = find_outside_strings(json, &needle2) {
        let rest = &json[start + needle2.len()..];
        if let Some(end) = json_string_end(rest) {
            return json_unescape(&rest[..end]);
        }
    }
    String::new()
}

/// Extract an integer field from simple JSON.
fn extract_json_int(json: &str, field: &str) -> u64 {
    let needle = format!("\"{field}\":");
    if let Some(start) = find_outside_strings(json, &needle) {
        let rest = &json[start + needle.len()..];
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = num.parse() {
            return n;
        }
    }
    0
}

/// Extract the username from `gh auth status` output. gh versions report it
/// either as `account <user>` or `as <user> (…)` — try both.
fn extract_user(text: &str) -> String {
    for marker in ["account ", " as "] {
        if let Some(idx) = text.find(marker) {
            let rest = &text[idx + marker.len()..];
            let user: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
            if !user.is_empty() {
                return user;
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_field_simple() {
        let json = r#"{"nameWithOwner":"owner/repo","description":"test"}"#;
        assert_eq!(extract_json_field(json, "nameWithOwner"), "owner/repo");
    }

    #[test]
    fn extract_json_field_nested() {
        let json = r#"{"defaultBranchRef":{"name":"main"}}"#;
        assert_eq!(extract_json_field(json, "defaultBranchRef"), "main");
    }

    #[test]
    fn extract_json_int_field() {
        let json = r#"{"number":42,"title":"test"}"#;
        assert_eq!(extract_json_int(json, "number"), 42);
    }

    #[test]
    fn extract_json_missing_field() {
        let json = r#"{"other":"value"}"#;
        assert_eq!(extract_json_field(json, "missing"), "");
        assert_eq!(extract_json_int(json, "missing"), 0);
    }

    #[test]
    fn extract_user_from_auth_status() {
        let text = "Logged in to github.com account octocat";
        assert_eq!(extract_user(text), "octocat");
    }

    /// gh emits `pr list --json` as a compact array on ONE line — splitting
    /// must yield every object, not just the first line's.
    #[test]
    fn split_json_objects_compact_array() {
        let json =
            r#"[{"number":1,"title":"a"},{"number":2,"title":"b"},{"number":3,"title":"c"}]"#;
        let objs = split_json_objects(json);
        assert_eq!(objs.len(), 3);
        assert_eq!(extract_json_int(objs[2], "number"), 3);
    }

    /// Braces and brackets inside string values must not break the split.
    #[test]
    fn split_json_objects_braces_in_strings() {
        let json = r#"[{"number":1,"title":"fix {parser} [x]"},{"number":2,"title":"ok"}]"#;
        let objs = split_json_objects(json);
        assert_eq!(objs.len(), 2);
        assert_eq!(extract_json_field(objs[0], "title"), "fix {parser} [x]");
    }

    /// Escaped quotes inside values must not truncate the field.
    #[test]
    fn extract_json_field_escaped_quote() {
        let json = r#"{"title":"say \"hi\" now"}"#;
        assert_eq!(extract_json_field(json, "title"), "say \"hi\" now");
    }

    /// A `"field":` sequence inside a STRING VALUE is user data, not a key —
    /// `gh pr list` carries remote-controlled titles, so a forged
    /// `","state":"MERGED` in a title must not become the reported state.
    #[test]
    fn extract_resists_key_injection_through_string_values() {
        let obj = r#"{"number":7,"title":"x\",\"state\":\"FORGED","state":"OPEN","url":"u"}"#;
        // Forged key inside the title string must be skipped; the real
        // `"state"` after it is what we extract.
        assert_eq!(extract_json_field(obj, "state"), "OPEN");
        assert_eq!(extract_json_int(obj, "number"), 7);
        // And a title field still resolves normally.
        assert_eq!(extract_json_field(obj, "title"), "x\",\"state\":\"FORGED");
    }

    /// Nested-object form (`"defaultBranchRef":{"name":"main"}`) must also
    /// ignore key-like text inside earlier string values.
    #[test]
    fn extract_nested_key_not_matched_inside_string() {
        let obj = r#"{"title":"ref\":{\"name\":\"fake"},"defaultBranchRef":{"name":"main"}}"#;
        assert_eq!(extract_json_field(obj, "defaultBranchRef"), "main");
    }

    /// An escaped backslash followed by a literal 'n' must decode to `\n`
    /// TEXT, not a newline — sequential `replace` calls unescape twice.
    #[test]
    fn json_unescape_no_double_unescaping() {
        // Raw JSON content `a\\n` = 'a' + escaped backslash + 'n'.
        assert_eq!(json_unescape("a\\\\n"), "a\\n");
        // Raw JSON content `a\nb` = 'a' + escaped newline + 'b'.
        assert_eq!(json_unescape("a\\nb"), "a\nb");
        // `\\\\` = two escaped backslashes -> `\\` text, not re-unescaped.
        assert_eq!(json_unescape("x\\\\\\\\y"), "x\\\\y");
        // \uXXXX escape.
        assert_eq!(json_unescape("caf\\u00e9"), "caf\u{e9}");
        // Unknown escapes keep their backslash instead of being eaten.
        assert_eq!(json_unescape("a\\qb"), "a\\qb");
        // A lone trailing backslash survives.
        assert_eq!(json_unescape("tail\\"), "tail\\");
    }

    /// `extract_user` accepts both gh report formats.
    #[test]
    fn extract_user_both_formats() {
        assert_eq!(
            extract_user("  ✓ Logged in to github.com account octocat"),
            "octocat"
        );
        assert_eq!(
            extract_user("  ✓ Logged in to github.com as monalisa (oauth_token)"),
            "monalisa"
        );
        assert_eq!(extract_user("You are not logged in"), "");
    }

    #[test]
    fn split_json_objects_empty_and_malformed() {
        assert!(split_json_objects("[]").is_empty());
        assert!(split_json_objects("not json").is_empty());
    }

    /// `find_on_path` must return an ABSOLUTE path — a relative hit would
    /// resolve through the child's working directory on Windows
    /// (CreateProcessW searches it before PATH), re-opening the planted-
    /// `gh.exe`/`git.exe` hole this function exists to close.
    #[test]
    fn find_on_path_returns_absolute_hit() {
        let dir = std::env::temp_dir().join(format!(
            "womd-gh-path-{:016x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(windows)]
        let bin = dir.join("fake-tool.exe");
        #[cfg(not(windows))]
        let bin = dir.join("fake-tool");
        std::fs::write(&bin, b"x").unwrap();
        let found =
            find_on_path("fake-tool", [dir.clone()].into_iter()).expect("binary must be found");
        assert!(Path::new(&found).is_absolute(), "{found}");
        assert!(find_on_path("no-such-tool-xyz", [dir.clone()].into_iter()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A wedged subprocess (dead remote, credential prompt on a non-existent
    /// terminal) must be killed at the deadline, not hang the adapter forever.
    #[test]
    fn run_with_timeout_kills_hung_process() {
        // A process that stays alive without stdin or network: `ping` fails
        // instantly in sandboxes, `timeout` refuses redirected stdin, so use
        // `Start-Sleep` (Windows) / `sleep` (Unix).
        #[cfg(windows)]
        let (bin, args) = (
            "powershell",
            vec!["-NoProfile", "-Command", "Start-Sleep -Seconds 30"],
        );
        #[cfg(not(windows))]
        let (bin, args) = ("sh", vec!["-c", "sleep 30"]);
        let start = std::time::Instant::now();
        let err = run_with_timeout(
            bin,
            Path::new("."),
            &args,
            std::time::Duration::from_millis(400),
        )
        .expect_err("hung process must error");
        assert!(err.to_string().contains("timed out"), "{}", err);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "kill took too long: {:?}",
            start.elapsed()
        );
    }

    /// A healthy child producing more output than a pipe buffer (~64 KiB) must
    /// NOT be mistaken for a hung process — reader threads must drain the pipes
    /// while the parent polls.
    #[test]
    fn run_with_timeout_drains_large_output() {
        #[cfg(windows)]
        let (bin, args) = (
            "cmd",
            vec![
                "/c",
                "for /l %i in (1,1,5000) do @echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            ],
        );
        #[cfg(not(windows))]
        let (bin, args) = ("sh", vec!["-c", "yes | head -c 200000"]);
        let out = run_with_timeout(
            bin,
            Path::new("."),
            &args,
            std::time::Duration::from_secs(30),
        )
        .expect("healthy process with large output must complete");
        assert!(out.status.success());
        // >64 KiB — enough to fill an OS pipe buffer and deadlock a naive
        // spawn+try_wait loop that doesn't drain.
        assert!(out.stdout.len() > 65_536, "len={}", out.stdout.len());
    }

    /// Output beyond the cap must be bounded: the child blocks on a full
    /// pipe and is killed at the deadline — memory stays capped.
    #[test]
    fn output_beyond_cap_is_killed_not_buffered() {
        #[cfg(windows)]
        let (bin, args) = (
            "powershell",
            vec![
                "-NoProfile",
                "-Command",
                "1..200000 | ForEach-Object { 'x' * 200 }",
            ],
        );
        #[cfg(not(windows))]
        let (bin, args) = ("sh", vec!["-c", "yes | head -c 40000000"]);
        let start = std::time::Instant::now();
        let err = run_with_timeout_capped(
            bin,
            Path::new("."),
            &args,
            std::time::Duration::from_secs(2),
            4096, // tiny cap: writer blocks almost immediately
        )
        .expect_err("excess output must be killed");
        assert!(err.to_string().contains("timed out"), "{}", err);
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    /// A missing binary must produce an error, not a panic or a hang.
    #[test]
    fn run_with_timeout_missing_binary() {
        let err = run_with_timeout(
            "womd-definitely-not-a-real-binary-xyz",
            Path::new("."),
            &[],
            std::time::Duration::from_secs(1),
        )
        .expect_err("missing binary must error");
        assert!(!err.to_string().is_empty());
    }
}
