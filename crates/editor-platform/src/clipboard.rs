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
        Self {
            inner: std::sync::Mutex::new(String::new()),
        }
    }
}

impl Default for InMemoryClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard for InMemoryClipboard {
    fn set_text(&self, text: &str) -> Result<(), PlatformError> {
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = text.to_string();
        Ok(())
    }
    fn get_text(&self) -> Result<String, PlatformError> {
        Ok(self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone())
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
        // The write runs on a thread: `write_all` on a full pipe blocks when
        // the child is wedged, and the deadline below only applies *after*
        // the write returns — a >64 KiB clipboard would hang the editor
        // forever without this. Killing the child closes the pipe, so the
        // writer always unblocks and the join after `kill` cannot hang.
        fn feed_stdin(
            child: &mut std::process::Child,
            text: &str,
        ) -> Result<std::process::ExitStatus, PlatformError> {
            let bytes = text.as_bytes().to_vec();
            let stdin = child.stdin.take();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let r = if let Some(mut s) = stdin {
                    s.write_all(&bytes)
                } else {
                    Ok(())
                };
                let _ = tx.send(r);
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let status = loop {
                match child
                    .try_wait()
                    .map_err(|e| PlatformError::Os(e.to_string()))?
                {
                    Some(s) => break s,
                    None if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    None => {
                        let _ = child.kill();
                        let _ = child.wait();
                        // Killing the child closes its pipe end so the
                        // writer unblocks — unless a detached grandchild
                        // inherited the read end. Never join without a
                        // deadline.
                        let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
                        return Err(PlatformError::Os("clipboard command timed out".to_string()));
                    }
                }
            };
            // The child exited — its stdin end is closed, but a grandchild
            // may still hold it open: collect the write result with a grace
            // deadline instead of joining unconditionally.
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap_or(Ok(()))
                .map_err(|e| PlatformError::Os(e.to_string()))?;
            if !status.success() {
                return Err(PlatformError::Os(format!(
                    "clipboard command exited with {status}"
                )));
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
        #[cfg(windows)]
        {
            // PowerShell appends a line ending to its stdout — strip exactly
            // one trailing EOL (the clipboard's own trailing newline is
            // indistinguishable; keeping it would be wrong more often).
            let out = grab_stdout(
                "powershell",
                &["-NoProfile", "-Command", "Get-Clipboard -Raw"],
            )?;
            let mut text = String::from_utf8_lossy(&out).to_string();
            if text.ends_with("\r\n") {
                text.truncate(text.len() - 2);
            } else if text.ends_with('\n') {
                text.pop();
            }
            Ok(text)
        }
        #[cfg(target_os = "macos")]
        {
            let out = grab_stdout("pbpaste", &[])?;
            Ok(String::from_utf8_lossy(&out).to_string())
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            match grab_stdout("xclip", &["-selection", "clipboard", "-o"]) {
                Ok(out) => Ok(String::from_utf8_lossy(&out).to_string()),
                Err(first) => {
                    // xsel fallback (many minimal X environments ship it
                    // instead of xclip).
                    let out =
                        grab_stdout("xsel", &["--clipboard", "--output"]).map_err(|_| first)?;
                    Ok(String::from_utf8_lossy(&out).to_string())
                }
            }
        }
    }
}

/// Spawn `bin args`, drain stdout with a 64 MiB cap, and enforce a 10 s
/// deadline — `Command::output()` has neither: a wedged clipboard tool
/// (xclip waiting on a selection owner, a hung PowerShell) would block the
/// editor forever, and endless output would exhaust memory.
#[allow(dead_code)] // compiled out on platforms with no system clipboard use
fn grab_stdout(bin: &str, args: &[&str]) -> Result<Vec<u8>, PlatformError> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| PlatformError::Os(e.to_string()))?;
    let out_pipe = child.stdout.take().expect("piped");
    const MAX_PIPE: u64 = 64 * 1024 * 1024;
    // The reader reports through a channel: `join` has no deadline, and a
    // detached grandchild inheriting the pipe's write end keeps it open
    // forever — a wedged powershell/xclip child would hang the editor even
    // after the child itself was reaped.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out_pipe.take(MAX_PIPE).read_to_end(&mut v);
        let _ = tx.send(v);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        match child
            .try_wait()
            .map_err(|e| PlatformError::Os(e.to_string()))?
        {
            Some(s) => break s,
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
                return Err(PlatformError::Os(format!("{bin} timed out")));
            }
        }
    };
    let stdout = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap_or_default();
    if !status.success() {
        return Err(PlatformError::Os(format!("{bin} exited with {status}")));
    }
    Ok(stdout)
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

    /// `grab_stdout` collects the child's stdout through a channel instead of
    /// joining the reader thread: a detached grandchild holding the pipe's
    /// write end would make `join` hang forever even after the child exits.
    /// A missing binary must simply error (spawn failure) rather than hang.
    #[test]
    fn grab_stdout_missing_binary_errors_not_hangs() {
        let err = grab_stdout("womd-no-such-clipboard-bin", &[]);
        assert!(err.is_err());
    }

    /// A child that exits successfully yields its stdout bytes.
    #[test]
    fn grab_stdout_collects_output() {
        // `git --version` is available in every dev environment this
        // workspace builds in (editor-git's own tests rely on it) and writes
        // a short line to stdout.
        let has_git = std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_git {
            return;
        }
        let out = grab_stdout("git", &["--version"]).expect("git --version");
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("git version"), "got: {text}");
    }
}
