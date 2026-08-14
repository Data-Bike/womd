//! GitHub `RepositoryHostAdapter` (§34): uses the `gh` CLI for GitHub API operations
//! (PRs, branches, metadata). Falls back to `NotSupported` if `gh` is not installed.
//!
//! The `gh` CLI handles authentication via `gh auth login` (stored in the OS keychain),
//! so secrets never enter the editor process (§87). A future `reqwest`-backed impl can
//! replace this behind the trait without touching callers.

#![forbid(unsafe_code)]

use std::path::PathBuf;
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
        Command::new(&self.gh_bin)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn gh_text(&self, args: &[&str]) -> Result<String, AdapterError> {
        if !self.gh_available() {
            return Err(AdapterError::Other("gh CLI not installed".to_string()));
        }
        let out = Command::new(&self.gh_bin)
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

    fn git_text(&self, args: &[&str]) -> Result<String, AdapterError> {
        let git = std::env::var("GIT_BIN").unwrap_or_else(|_| "git".to_string());
        let out = Command::new(&git)
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

impl RepositoryHostAdapter for GithubAdapter {
    fn provider_id(&self) -> ProviderId {
        self.provider.clone()
    }

    fn authenticate(&self) -> Result<AuthSession, AdapterError> {
        let text = self.gh_text(&["auth", "status"])?;
        let user = extract_user(&text);
        if text.contains("Logged in") {
            Ok(AuthSession {
                provider: self.provider.clone(),
                user,
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
        for line in text.lines() {
            if line.contains("\"number\"") {
                let number = extract_json_int(line, "number");
                let title = extract_json_field(line, "title");
                let state = extract_json_field(line, "state");
                let html_url = extract_json_field(line, "url");
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

/// Extract a string field from simple JSON (no serde dep for the adapter skeleton).
fn extract_json_field(json: &str, field: &str) -> String {
    let needle = format!("\"{field}\":\"");
    if let Some(start) = json.find(&needle) {
        let rest = &json[start + needle.len()..];
        if let Some(end) = rest.find('"') {
            return rest[..end].to_string();
        }
    }
    let needle2 = format!("\"{field}\":{{\"name\":\"");
    if let Some(start) = json.find(&needle2) {
        let rest = &json[start + needle2.len()..];
        if let Some(end) = rest.find('"') {
            return rest[..end].to_string();
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

/// Extract the username from `gh auth status` output.
fn extract_user(text: &str) -> String {
    if let Some(idx) = text.find("account ") {
        let rest = &text[idx + 8..];
        let user: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        return user;
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
}
