//! Editor diff: line/intraline diff, hunks, stage-hunk model, semantic Markdown diff.
//!
//! Raw Git diff remains the source of truth for version control (Invariant 10). The
//! algorithms here operate on byte buffers and are used both to render Git diffs in the UI
//! (§38–42) and to provide an additional semantic Markdown diff view (§43–44). They are
//! pure Rust and fully testable without a Git repository (§94).

#![forbid(unsafe_code)]

mod hunk;
mod intraline;
mod myers;
mod semantic;

pub use hunk::{Hunk, HunkConfig, group_into_hunks};
pub use intraline::{IntralineChange, intraline_diff};
pub use myers::{LineChange, Operation, line_diff};
pub use semantic::{SemanticChange, SemanticUnit, semantic_diff};

use editor_domain::ByteOffset;

/// A half-open byte range `[start, end)`.
pub type ByteRange = editor_domain::ByteRange;

/// Re-export of `ByteOffset` for diff consumers.
pub type DiffOffset = ByteOffset;
