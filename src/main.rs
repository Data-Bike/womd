//! WoMD application entry point.
//!
//! The UI shell (Tauri 2 + custom editing surface, ADR-001) lands in the UI vertical slice.
//! This binary currently exercises the editor core to verify the foundation builds and the
//! source-preservation invariants hold end-to-end.

#![forbid(unsafe_code)]

use editor_core::{DocumentBuffer, EditTransaction, TextEdit};
use editor_domain::{
    ByteOffset, DocumentMeta, Encoding, LineEnding, MarkdownProfile, Selection, ids::DocumentId,
};

fn main() {
    let src = b"# WoMD\n\nA source-preserving Markdown editor.\n\n- [ ] Build foundation\n- [x] Write ADRs\n";

    let meta = DocumentMeta {
        id: DocumentId::new("welcome"),
        has_bom: false,
        line_ending: LineEnding::Lf,
        trailing_newline: true,
        encoding: Encoding::Utf8,
    };

    let mut buf = DocumentBuffer::open(src.to_vec(), meta, MarkdownProfile::Gfm)
        .expect("document must parse");

    // Invariant 1: open -> serialize is byte-identical when no edits.
    assert_eq!(buf.serialize(), src);

    // Insert text via a transaction (Invariant 7 path: local, no Git/network).
    let edit = TextEdit::insert(ByteOffset(6), b" (Word-like Markdown)");
    let tx = EditTransaction::single(
        edit,
        Selection::caret(ByteOffset(6)),
        Selection::caret(ByteOffset(26)),
    );
    buf.apply(tx).expect("apply");

    // Undo restores the original.
    buf.undo().expect("undo");
    assert_eq!(buf.serialize(), src);

    println!("WoMD foundation OK: invariants verified.");
}
