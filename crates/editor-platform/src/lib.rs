//! Platform abstractions (§74, §86–88, §97). No business logic; only OS-specific services
//! behind traits. Implementations are platform-specific and feature-gated.
//!
//! This crate provides:
//! * `SecureStorage` — OS keychain where available; in-memory fallback for tests.
//! * `UrlOpener` — open a URL via the OS default handler (§86).
//! * `Clipboard` — clipboard get/set (§77).
//! * `FileWatcher` — file change notification (§64); polling-based fallback.
//! * `HighDpiInfo` — screen scale factor for rendering (§97).

#![forbid(unsafe_code)]

mod clipboard;
mod watcher;
mod secret;
mod url_opener;

pub use clipboard::{Clipboard, InMemoryClipboard, SystemClipboard};
pub use watcher::{FileWatcher, PollingFileWatcher, WatchEvent};
pub use secret::{SecureStorage, InMemorySecureStorage};
pub use url_opener::{UrlOpener, SystemUrlOpener};

/// Typed platform error (§89).
#[derive(Debug)]
pub enum PlatformError {
    NotSupported,
    Os(String),
    NotFound(String),
}

impl core::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&match self {
            Self::NotSupported => "not supported on this platform".to_string(),
            Self::Os(s) => format!("os error: {s}"),
            Self::NotFound(s) => format!("not found: {s}"),
        })
    }
}
impl std::error::Error for PlatformError {}

/// Screen scale factor for high-DPI rendering (§97).
pub trait HighDpiInfo {
    fn scale_factor(&self) -> f64;
}

/// A no-op high-DPI info source (scale = 1.0), used in headless/test environments.
pub struct DefaultDpiInfo;

impl HighDpiInfo for DefaultDpiInfo {
    fn scale_factor(&self) -> f64 {
        1.0
    }
}
