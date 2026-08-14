//! Strongly-typed identifiers. Newtypes prevent mixing ids across domains.

use core::fmt;

/// A document identifier (opaque).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocumentId(pub String);

impl DocumentId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl fmt::Display for DocumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A repository identifier (opaque).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepositoryId(pub String);

impl RepositoryId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// A plugin identifier (e.g. `com.example.mermaid`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginId(pub String);

impl PluginId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A Markdown extension identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtensionId(pub String);

impl ExtensionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// A repository hosting provider identifier (e.g. `github`, `gitlab`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Construct without validation. Used by adapters that know their constant id.
    pub fn from_str_unchecked(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
