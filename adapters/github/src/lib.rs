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
            gh_bin: std::env::var("GH_BIN").unwrap_or_else(|_| "gh".to_string()),
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
            Err(AdapterError::Other(String::from_utf8_lossy(&out.stderr).trim().to_string()))
        }
    }

    fn git_text(&self, args: &[&str]) -> Result<String, AdapterError> {
        let git = std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string());
        let out = run_with_timeout(&git, &self.work_dir, args, CMD_TIMEOUT)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            Err(AdapterError::Other(String::from_utf8_lossy(&out.stderr).trim().to_string()))
        }
    }
}

const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
        let out = run_with_timeout(&self.gh_bin, &self.work_dir, &["auth", "status"], CMD_TIMEOUT)?;
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
            "repo", "view", "--json", "nameWithOwner,defaultBranchRef,url",
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
                prs.push(PullRequest { number, title, state, html_url });
            }
        }
        Ok(prs)
    }

    fn create_pull_request(&self, title: &str, body: &str) -> Result<PullRequest, AdapterError> {
        let text = self.gh_text(&["pr", "create", "--title", title, "--body", body, "--json", "number,url"])?;
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
                branches.push(RemoteBranch { name, sha: String::new() });
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

/// Extract a string field from simple JSON (no serde dep for the adapter skeleton).
fn extract_json_field(json: &str, field: &str) -> String {
    let needle = format!("\"{field}\":\"");
    if let Some(start) = json.find(&needle) {
        let rest = &json[start + needle.len()..];
        if let Some(end) = json_string_end(rest) {
            return json_unescape(&rest[..end]);
        }
    }
    let needle2 = format!("\"{field}\":{{\"name\":\"");
    if let Some(start) = json.find(&needle2) {
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
    if let Some(start) = json.find(&needle) {
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
        let json = r#"[{"number":1,"title":"a"},{"number":2,"title":"b"},{"number":3,"title":"c"}]"#;
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
        assert_eq!(extract_user("  ✓ Logged in to github.com account octocat"), "octocat");
        assert_eq!(extract_user("  ✓ Logged in to github.com as monalisa (oauth_token)"), "monalisa");
        assert_eq!(extract_user("You are not logged in"), "");
    }

    #[test]
    fn split_json_objects_empty_and_malformed() {
        assert!(split_json_objects("[]").is_empty());
        assert!(split_json_objects("not json").is_empty());
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
        let err = run_with_timeout(bin, Path::new("."), &args, std::time::Duration::from_millis(400))
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
            vec!["/c", "for /l %i in (1,1,5000) do @echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"],
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
