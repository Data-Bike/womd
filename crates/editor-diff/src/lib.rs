//! Editor diff: line/intraline diff, hunks, stage-hunk model, semantic Markdown diff.
//!
//! Raw Git diff remains the source of truth for version control (Invariant 10). The
//! algorithms here operate on byte buffers and are used both to render Git diffs in the UI
//! (§38–42) and to provide an additional semantic Markdown diff view (§43–44). They are
//! pure Rust and fully testable without a Git repository (§94).

#![forbid(unsafe_code)]

mod myers;
mod intraline;
mod hunk;
mod semantic;

pub use hunk::{group_into_hunks, Hunk, HunkConfig};
pub use intraline::{intraline_diff, IntralineChange};
pub use myers::{line_diff, LineChange, Operation};
pub use semantic::{semantic_diff, SemanticChange, SemanticUnit};

use editor_domain::ByteOffset;

/// A half-open byte range `[start, end)`.
pub type ByteRange = editor_domain::ByteRange;

/// Re-export of `ByteOffset` for diff consumers.
pub type DiffOffset = ByteOffset;
