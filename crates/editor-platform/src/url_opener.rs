//! URL opener (§86). Opens a URL via the OS default handler. Must not execute arbitrary
//! commands — only well-formed `http`/`https`/`mailto` URLs are accepted (§86, security).

use crate::PlatformError;

/// URL opener trait (§86).
pub trait UrlOpener {
    fn open(&self, url: &str) -> Result<(), PlatformError>;
}

/// System URL opener. Uses the platform's default handler.
pub struct SystemUrlOpener;

impl UrlOpener for SystemUrlOpener {
    fn open(&self, url: &str) -> Result<(), PlatformError> {
        // Security: only allow http, https, and mailto schemes (§86).
        if !is_safe_url(url) {
            return Err(PlatformError::Os(format!(
                "refusing to open URL with unsafe scheme: {url}"
            )));
        }
        #[cfg(windows)]
        {
            std::process::Command::new("rundll32")
                .args(["url.dll,FileProtocolHandler", url])
                .spawn()
                .map_err(|e| PlatformError::Os(e.to_string()))?;
            Ok(())
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open")
                .arg(url)
                .spawn()
                .map_err(|e| PlatformError::Os(e.to_string()))?;
            Ok(())
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            std::process::Command::new("xdg-open")
                .arg(url)
                .spawn()
                .map_err(|e| PlatformError::Os(e.to_string()))?;
            Ok(())
        }
    }
}

/// Check that a URL uses a safe scheme (http, https, mailto) and has a non-empty host
/// for http(s). Prevents command injection via `file://`, `javascript:`, etc. (§86).
fn is_safe_url(url: &str) -> bool {
    // Control characters (incl. NUL, \r, \n) must never reach the OS
    // handler's argv — they can mangle command lines on some platforms.
    if url.chars().any(|c| c.is_control()) {
        return false;
    }
    let lower = url.to_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        // Must have a host after the scheme.
        let after_scheme = lower.split("://").nth(1).unwrap_or("");
        !after_scheme.is_empty()
    } else if lower.starts_with("mailto:") {
        // mailto: must have something after the scheme.
        url.len() > "mailto:".len()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_urls_accepted() {
        assert!(is_safe_url("https://example.com"));
        assert!(is_safe_url("http://localhost:8080/path"));
        assert!(is_safe_url("mailto:user@example.com"));
    }

    #[test]
    fn unsafe_urls_rejected() {
        assert!(!is_safe_url("file:///etc/passwd"));
        assert!(!is_safe_url("javascript:alert(1)"));
        assert!(!is_safe_url("data:text/html,<script>"));
        assert!(!is_safe_url("ftp://example.com"));
        assert!(!is_safe_url(""));
        assert!(!is_safe_url("http://"));
        // Empty mailto payload and whitespace-prefixed schemes are rejected,
        // not sanitized — the caller sees a refusal, not a mangled URL.
        assert!(!is_safe_url("mailto:"));
        assert!(!is_safe_url(" https://example.com"));
        assert!(!is_safe_url("\thttps://example.com"));
        // Control characters inside otherwise-valid URLs are rejected.
        assert!(!is_safe_url("https://example.com/\nfile:///etc"));
        assert!(!is_safe_url("mailto:a@b.com\u{0}x"));
        assert!(!is_safe_url("http://exa\rmple.com"));
    }

    #[test]
    fn case_insensitive_scheme() {
        assert!(is_safe_url("HTTPS://Example.COM"));
        assert!(is_safe_url("Http://localhost"));
    }
}
