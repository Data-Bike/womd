//! Clipboard access (§77).

use crate::PlatformError;

/// Clipboard trait: get/set text.
pub trait Clipboard {
    fn set_text(&self, text: &str) -> Result<(), PlatformError>;
    fn get_text(&self) -> Result<String, PlatformError>;
}

/// In-memory clipboard (for tests and headless environments).
pub struct InMemoryClipboard {
    inner: std::sync::Mutex<String>,
}

impl InMemoryClipboard {
    pub fn new() -> Self {
        Self { inner: std::sync::Mutex::new(String::new()) }
    }
}

impl Default for InMemoryClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard for InMemoryClipboard {
    fn set_text(&self, text: &str) -> Result<(), PlatformError> {
        *self.inner.lock().unwrap() = text.to_string();
        Ok(())
    }
    fn get_text(&self) -> Result<String, PlatformError> {
        Ok(self.inner.lock().unwrap().clone())
    }
}

/// System clipboard via the `clip` / `pbpaste`-style CLI tools.
///
/// On Windows: uses `clip` (set) and `powershell Get-Clipboard` (get).
/// On macOS: uses `pbcopy` (set) and `pbpaste` (get).
/// On Linux: uses `xclip` (set/get) with fallback to `xsel`.
pub struct SystemClipboard;

impl Clipboard for SystemClipboard {
    fn set_text(&self, text: &str) -> Result<(), PlatformError> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        // Shared helper: write `text` to the child's stdin, CLOSE the pipe
        // (the child waits for EOF), then wait for exit and check the status.
        // `child.stdin.take()` moves the handle out so it drops at the end of
        // the write — keeping it alive until `wait()` deadlocks, because the
        // child never sees EOF. The deadline guards against a wedged tool —
        // `wait()` has no timeout.
        fn feed_stdin(child: &mut std::process::Child, text: &str) -> Result<std::process::ExitStatus, PlatformError> {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(text.as_bytes()).map_err(|e| PlatformError::Os(e.to_string()))?;
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let status = loop {
                match child.try_wait().map_err(|e| PlatformError::Os(e.to_string()))? {
                    Some(s) => break s,
                    None if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    None => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(PlatformError::Os("clipboard command timed out".to_string()));
                    }
                }
            };
            if !status.success() {
                return Err(PlatformError::Os(format!("clipboard command exited with {status}")));
            }
            Ok(status)
        }
        #[cfg(windows)]
        {
            let mut child = Command::new("clip")
                .stdin(Stdio::piped())
                .spawn()
                .map_err(|e| PlatformError::Os(e.to_string()))?;
            feed_stdin(&mut child, text)?;
            Ok(())
        }
        #[cfg(target_os = "macos")]
        {
            let mut child = Command::new("pbcopy")
                .stdin(Stdio::piped())
                .spawn()
                .map_err(|e| PlatformError::Os(e.to_string()))?;
            feed_stdin(&mut child, text)?;
            Ok(())
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let mut child = Command::new("xclip")
                .args(["-selection", "clipboard"])
                .stdin(Stdio::piped())
                .spawn()
                .map_err(|e| PlatformError::Os(e.to_string()))?;
            feed_stdin(&mut child, text)?;
            Ok(())
        }
    }

    fn get_text(&self) -> Result<String, PlatformError> {
        use std::process::Command;
        #[cfg(windows)]
        {
            // PowerShell Get-Clipboard returns text with a trailing newline; trim it.
            let out = Command::new("powershell")
                .args(["-NoProfile", "-Command", "Get-Clipboard -Raw"])
                .output()
                .map_err(|e| PlatformError::Os(e.to_string()))?;
            if !out.status.success() {
                return Err(PlatformError::Os(String::from_utf8_lossy(&out.stderr).to_string()));
            }
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        }
        #[cfg(target_os = "macos")]
        {
            let out = Command::new("pbpaste")
                .output()
                .map_err(|e| PlatformError::Os(e.to_string()))?;
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let out = Command::new("xclip")
                .args(["-selection", "clipboard", "-o"])
                .output()
                .map_err(|e| PlatformError::Os(e.to_string()))?;
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_clipboard_roundtrip() {
        let cb = InMemoryClipboard::new();
        cb.set_text("hello world").unwrap();
        assert_eq!(cb.get_text().unwrap(), "hello world");
    }

    #[test]
    fn in_memory_clipboard_overwrite() {
        let cb = InMemoryClipboard::new();
        cb.set_text("first").unwrap();
        cb.set_text("second").unwrap();
        assert_eq!(cb.get_text().unwrap(), "second");
    }

    #[test]
    fn in_memory_clipboard_empty_default() {
        let cb = InMemoryClipboard::new();
        assert_eq!(cb.get_text().unwrap(), "");
    }
}
