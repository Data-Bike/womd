//! Markdown profile selection (§3).

/// A Markdown profile determines which syntax extensions are active. Non-standard
/// extensions are NOT mixed into CommonMark/GFM; they come from plugins via the extension
/// API (§24).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MarkdownProfile {
    /// CommonMark 0.31.2 core profile.
    CommonMark,
    /// GitHub Flavored Markdown (CommonMark + tables, task lists, strikethrough,
    /// autolinks, tag filter).
    Gfm,
    /// A custom profile assembled from CommonMark + a set of registered extensions.
    Custom(String),
}

impl MarkdownProfile {
    pub fn is_gfm(&self) -> bool {
        matches!(self, Self::Gfm)
    }
}
