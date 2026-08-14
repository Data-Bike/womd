//! Typed domain errors (§89). No stringly-typed errors.

/// Document-level error.
#[derive(Debug)]
pub enum DocumentError {
    /// Byte range is outside the document bounds.
    OutOfRange,
    /// Operation attempted on a non-existent node id.
    NodeNotFound,
    /// Edit would violate source preservation (e.g. splitting a UTF-8 character).
    InvalidEdit,
    /// Parse failure with a message and byte offset.
    Parse { message: String, offset: u64 },
}

impl core::fmt::Display for DocumentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&match self {
            Self::OutOfRange => "byte range out of range".to_string(),
            Self::NodeNotFound => "node not found".to_string(),
            Self::InvalidEdit => "invalid edit".to_string(),
            Self::Parse { message, offset } => format!("parse error at {offset}: {message}"),
        })
    }
}
impl std::error::Error for DocumentError {}

/// Storage-level error (re-exported shape; the canonical impl lives in `editor-storage`).
#[derive(Debug)]
pub enum StorageError {
    NotFound,
    Io(String),
    OutOfRange,
    Cancelled,
}

impl core::fmt::Display for StorageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&match self {
            Self::NotFound => "not found".to_string(),
            Self::Io(s) => format!("io error: {s}"),
            Self::OutOfRange => "byte range out of range".to_string(),
            Self::Cancelled => "operation cancelled".to_string(),
        })
    }
}
impl std::error::Error for StorageError {}
