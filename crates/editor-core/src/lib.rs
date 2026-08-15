//! Editor core: TextEdit, EditTransaction, UndoManager, SelectionModel, and the
//! `DocumentBuffer` facade that composes the source-preserving text buffer with the
//! lossless syntax model and selection (§59, §60, §61).
//!
//! The UI never mutates strings directly; all changes go through `TextEdit`/commands.

#![forbid(unsafe_code)]

use std::sync::Arc;

use editor_domain::{ByteOffset, ByteRange, DocumentMeta, MarkdownProfile, Selection};
use editor_markdown::{serialize, Document, SourceSpan};
use editor_text::PieceTable;

/// A single atomic byte-range replacement (§60). `removed` is filled in by
/// `DocumentBuffer::apply` with the bytes that were replaced, so the edit can be inverted
/// for undo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: ByteRange,
    pub replacement: Vec<u8>,
    /// Bytes removed by this edit (captured at apply time; empty until applied).
    pub removed: Vec<u8>,
}

impl TextEdit {
    pub fn replace(range: ByteRange, replacement: impl Into<Vec<u8>>) -> Self {
        Self { range, replacement: replacement.into(), removed: Vec::new() }
    }
    pub fn insert(at: ByteOffset, text: impl Into<Vec<u8>>) -> Self {
        Self { range: ByteRange::empty(at), replacement: text.into(), removed: Vec::new() }
    }
    pub fn delete(range: ByteRange) -> Self {
        Self { range, replacement: Vec::new(), removed: Vec::new() }
    }
    /// Net byte length delta of this edit.
    pub fn delta(&self) -> i64 {
        self.replacement.len() as i64 - (self.range.end.0 - self.range.start.0) as i64
    }
}

/// A transaction groups one or more `TextEdit`s into a single undo step (§61).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditTransaction {
    pub edits: Vec<TextEdit>,
    pub selection_before: Selection,
    pub selection_after: Selection,
}

impl EditTransaction {
    pub fn new(edits: Vec<TextEdit>, before: Selection, after: Selection) -> Self {
        Self { edits, selection_before: before, selection_after: after }
    }
    pub fn single(edit: TextEdit, before: Selection, after: Selection) -> Self {
        Self::new(vec![edit], before, after)
    }
}

/// Undo/redo stack (§61).
pub struct UndoManager {
    undo: Vec<EditTransaction>,
    redo: Vec<EditTransaction>,
}

impl UndoManager {
    pub fn new() -> Self {
        Self { undo: Vec::new(), redo: Vec::new() }
    }
    pub fn push(&mut self, tx: EditTransaction) {
        self.undo.push(tx);
        self.redo.clear();
    }
    pub fn undo(&mut self) -> Option<EditTransaction> {
        let tx = self.undo.pop()?;
        self.redo.push(tx.clone());
        Some(tx)
    }
    pub fn redo(&mut self) -> Option<EditTransaction> {
        let tx = self.redo.pop()?;
        self.undo.push(tx.clone());
        Some(tx)
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

impl Default for UndoManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Selection model (§59).
#[derive(Debug, Clone)]
pub struct SelectionModel {
    pub selections: Vec<Selection>,
}

impl SelectionModel {
    pub fn new() -> Self {
        Self { selections: vec![Selection::caret(ByteOffset::ZERO)] }
    }
    pub fn primary(&self) -> Selection {
        *self.selections.last().unwrap_or(&Selection::caret(ByteOffset::ZERO))
    }
    pub fn set_primary(&mut self, sel: Selection) {
        self.selections = vec![sel];
    }
}

impl Default for SelectionModel {
    fn default() -> Self {
        Self::new()
    }
}

/// The document buffer facade: text + syntax + selection + undo (§59).
pub struct DocumentBuffer {
    pub meta: DocumentMeta,
    pub profile: MarkdownProfile,
    table: PieceTable,
    syntax: Document,
    selection: SelectionModel,
    undo: UndoManager,
    dirty: bool,
}

impl DocumentBuffer {
    /// Open a document from raw bytes (the storage layer supplies these).
    pub fn open(bytes: Vec<u8>, meta: DocumentMeta, profile: MarkdownProfile) -> Result<Self, editor_domain::DocumentError> {
        let original: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
        let table = PieceTable::from_original(Arc::clone(&original));
        let syntax = editor_markdown::parse_with(&original, profile.clone())?;
        Ok(Self {
            meta,
            profile,
            table,
            syntax,
            selection: SelectionModel::new(),
            undo: UndoManager::new(),
            dirty: false,
        })
    }

    pub fn len(&self) -> u64 {
        self.table.len()
    }
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    pub fn text(&self) -> &PieceTable {
        &self.table
    }
    pub fn syntax(&self) -> &Document {
        &self.syntax
    }
    pub fn selection(&self) -> &SelectionModel {
        &self.selection
    }
    pub fn selection_mut(&mut self) -> &mut SelectionModel {
        &mut self.selection
    }
    pub fn undo_manager(&self) -> &UndoManager {
        &self.undo
    }

    /// Apply a transaction: edit the piece table, reparse affected region (incremental
    /// reparse per ADR-003 §58 — only the blocks overlapping the edit are reparsed),
    /// update selection, record undo.
    pub fn apply(&mut self, mut tx: EditTransaction) -> Result<(), editor_domain::DocumentError> {
        // Capture removed bytes for each edit so the transaction is invertible (undo).
        for edit in &mut tx.edits {
            edit.removed = self.table.extract_bytes(edit.range);
        }
        // Compute the net byte delta and the affected range before applying edits.
        let (affected_start, affected_end_pre, net_delta) = compute_edit_impact(&tx.edits);
        for edit in &tx.edits {
            self.table.replace(edit.range, &edit.replacement);
        }
        self.selection.set_primary(tx.selection_after);
        self.undo.push(tx);
        // Incremental reparse: find blocks overlapping the affected range, reparse only
        // that window from the post-edit bytes, splice in the new blocks, and shift
        // subsequent blocks by the net delta (ADR-003 §58).
        let bytes = self.table.to_bytes();
        self.incremental_reparse(&bytes, affected_start, affected_end_pre, net_delta)?;
        self.dirty = true;
        Ok(())
    }

    /// Incremental reparse: reparse only the blocks overlapping `[affected_start,
    /// affected_end_pre)` from the pre-edit document. The post-edit bytes are `bytes`;
    /// `net_delta` is the byte length difference (new - old) introduced by the edit.
    fn incremental_reparse(
        &mut self,
        bytes: &[u8],
        affected_start: u64,
        affected_end_pre: u64,
        net_delta: i64,
    ) -> Result<(), editor_domain::DocumentError> {
        // Find the first and last top-level blocks that overlap the affected range.
        let blocks = &self.syntax.blocks;
        let mut first_idx = None;
        let mut last_idx = None;
        for (i, b) in blocks.iter().enumerate() {
            let span = b.meta().span;
            if span.end.0 > affected_start && span.start.0 < affected_end_pre {
                if first_idx.is_none() {
                    first_idx = Some(i);
                }
                last_idx = Some(i);
            }
        }
        // If no blocks overlap (e.g. edit at EOF), fall back to full reparse.
        let (Some(first), Some(last)) = (first_idx, last_idx) else {
            self.syntax = editor_markdown::parse_with(bytes, self.profile.clone())?;
            return Ok(());
        };
        // The window to reparse: from the first affected block's start to the last
        // affected block's end (in post-edit coordinates, i.e. shifted by net_delta for
        // blocks after the edit point — but since we're taking the first block's start
        // which is before the edit, and the last block's end which is after the edit, we
        // need to adjust the end by net_delta).
        let window_start = blocks[first].meta().span.start.0;
        let window_end_pre = blocks[last].meta().span.end.0;
        let window_end_post = ((window_end_pre as i64 + net_delta).max(0) as u64).min(bytes.len() as u64);
        let window_len = window_end_post.saturating_sub(window_start);
        // Reparse the window.
        let new_blocks = editor_markdown::parse_range(bytes, window_start, window_len, self.profile.clone())?;
        // Shift blocks after the window by net_delta. The threshold is the pre-edit end of
        // the last affected block — blocks after this point need their spans adjusted to
        // post-edit coordinates.
        let shift_from = window_end_pre;
        for b in &mut self.syntax.blocks[last + 1..] {
            let m = b.meta_mut();
            if m.span.start.0 >= shift_from {
                m.span.start = ByteOffset((m.span.start.0 as i64 + net_delta).max(0) as u64);
            }
            if m.span.end.0 >= shift_from {
                m.span.end = ByteOffset((m.span.end.0 as i64 + net_delta).max(0) as u64);
            }
        }
        // Splice: replace blocks[first..=last] with new_blocks.
        self.syntax.blocks.splice(first..=last, new_blocks);
        // Update document span end.
        self.syntax.span.end = ByteOffset(bytes.len() as u64);
        Ok(())
    }

    /// Undo the last transaction by applying inverse edits in reverse order.
    pub fn undo(&mut self) -> Result<(), editor_domain::DocumentError> {
        let Some(tx) = self.undo.undo() else {
            return Ok(());
        };
        self.apply_inverse(&tx)?;
        self.selection.set_primary(tx.selection_before);
        Ok(())
    }

    /// Redo the last undone transaction.
    pub fn redo(&mut self) -> Result<(), editor_domain::DocumentError> {
        let Some(tx) = self.undo.redo() else {
            return Ok(());
        };
        // Re-apply original edits (removed bytes already captured).
        for edit in &tx.edits {
            self.table.replace(edit.range, &edit.replacement);
        }
        self.selection.set_primary(tx.selection_after);
        let bytes = self.table.to_bytes();
        self.syntax = editor_markdown::parse_with(&bytes, self.profile.clone())?;
        self.dirty = true;
        Ok(())
    }

    fn apply_inverse(&mut self, tx: &EditTransaction) -> Result<(), editor_domain::DocumentError> {
        // Undo edits in reverse order. Each edit's range was in cumulative coordinates
        // (relative to the document state after all previous edits in the transaction).
        // After undoing edit N, the document is in the state after edits 1..N-1, which
        // is exactly the state edit N-1's range refers to — so no delta adjustment needed.
        for edit in tx.edits.iter().rev() {
            let inserted_start = edit.range.start.0;
            let inserted_end = inserted_start + edit.replacement.len() as u64;
            self.table.replace(
                ByteRange::new(ByteOffset(inserted_start), ByteOffset(inserted_end)),
                &edit.removed,
            );
        }
        let bytes = self.table.to_bytes();
        self.syntax = editor_markdown::parse_with(&bytes, self.profile.clone())?;
        self.dirty = true;
        Ok(())
    }

    /// Serialize the current document to bytes (source-preserving; §2, Invariant 1/2).
    pub fn serialize(&self) -> Vec<u8> {
        // The serializer uses the *current* bytes as the verbatim store so unchanged
        // regions are emitted byte-for-byte.
        let current = self.table.to_bytes();
        serialize(&self.syntax, &current)
    }

    /// Replace a text run within a paragraph and return the resulting bytes (minimal diff).
    /// Convenience for the WYSIWYG "replace word" flow (§2 example).
    pub fn replace_text_run(
        &mut self,
        para_span: SourceSpan,
        old: &str,
        new: &str,
    ) -> Result<(), editor_domain::DocumentError> {
        let current = self.table.to_bytes();
        let Some((new_bytes, _delta)) = editor_markdown::serialize::replace_text_run(&current, para_span, old, new)
        else {
            return Err(editor_domain::DocumentError::InvalidEdit);
        };
        // Apply as a single edit covering the old run.
        let region = &current[para_span.start.0 as usize..para_span.end.0 as usize];
        let pos = region.windows(old.len()).position(|w| w == old.as_bytes());
        let Some(pos) = pos else {
            return Err(editor_domain::DocumentError::InvalidEdit);
        };
        let abs = para_span.start.0 + pos as u64;
        let edit = TextEdit::replace(
            ByteRange::new(ByteOffset(abs), ByteOffset(abs + old.len() as u64)),
            new.as_bytes(),
        );
        let before = self.selection.primary();
        let after = Selection::caret(ByteOffset(abs + new.len() as u64));
        self.apply(EditTransaction::single(edit, before, after))?;
        let _ = new_bytes;
        Ok(())
    }

    /// Mark the buffer as saved (clears dirty flag; does NOT commit to Git — §62).
    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }
}

/// Compute the affected byte range and net delta from a list of edits (pre-edit
/// coordinates). Returns `(min_start, max_end, net_delta)`.
fn compute_edit_impact(edits: &[TextEdit]) -> (u64, u64, i64) {
    let mut min_start = u64::MAX;
    let mut max_end = 0u64;
    let mut net_delta: i64 = 0;
    for edit in edits {
        min_start = min_start.min(edit.range.start.0);
        max_end = max_end.max(edit.range.end.0);
        net_delta += edit.delta();
    }
    if min_start == u64::MAX {
        min_start = 0;
    }
    (min_start, max_end, net_delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_domain::ids::DocumentId;
    use editor_markdown::Block;

    fn open(src: &[u8]) -> DocumentBuffer {
        let meta = DocumentMeta {
            id: DocumentId::new("test"),
            has_bom: false,
            line_ending: editor_domain::LineEnding::Lf,
            trailing_newline: src.last() == Some(&b'\n'),
            encoding: editor_domain::Encoding::Utf8,
        };
        DocumentBuffer::open(src.to_vec(), meta, MarkdownProfile::Gfm).unwrap()
    }

    #[test]
    fn open_save_roundtrip_is_byte_identical() {
        let src = b"# Title\n\nHello **beautiful** world.\n\n- a\n- b\n";
        let buf = open(src);
        assert_eq!(buf.serialize(), src);
        assert!(!buf.is_dirty());
    }

    #[test]
    fn small_edit_produces_small_diff() {
        let src = b"# Test\n\nHello beautiful world.\n";
        let mut buf = open(src);
        // Find the paragraph span.
        let para = buf.syntax().blocks.iter().find_map(|b| match b {
            Block::Paragraph(p) => Some(p.meta.span),
            _ => None,
        }).expect("paragraph");
        buf.replace_text_run(para, "beautiful", "great").unwrap();
        let out = buf.serialize();
        assert_eq!(out, b"# Test\n\nHello great world.\n");
        // Diff vs original is exactly one line.
        let changed_lines = out
            .split(|&b| b == b'\n')
            .zip(src.split(|&b| b == b'\n'))
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(changed_lines, 1);
        assert!(buf.is_dirty());
    }

    #[test]
    fn insert_text_via_transaction() {
        let mut buf = open(b"Hello\n");
        let edit = TextEdit::insert(ByteOffset(5), b", world");
        let tx = EditTransaction::single(
            edit,
            Selection::caret(ByteOffset(5)),
            Selection::caret(ByteOffset(12)),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.serialize(), b"Hello, world\n");
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut buf = open(b"Hello\n");
        let tx = EditTransaction::single(
            TextEdit::insert(ByteOffset(5), b", world"),
            Selection::caret(ByteOffset(5)),
            Selection::caret(ByteOffset(12)),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.serialize(), b"Hello, world\n");
        buf.undo().unwrap();
        assert_eq!(buf.serialize(), b"Hello\n");
        buf.redo().unwrap();
        assert_eq!(buf.serialize(), b"Hello, world\n");
    }

    #[test]
    fn undo_replace_restores_original() {
        let mut buf = open(b"abc def ghi\n");
        let tx = EditTransaction::single(
            TextEdit::replace(
                editor_domain::ByteRange::new(ByteOffset(4), ByteOffset(7)),
                b"xyz",
            ),
            Selection::caret(ByteOffset(4)),
            Selection::caret(ByteOffset(7)),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.serialize(), b"abc xyz ghi\n");
        buf.undo().unwrap();
        assert_eq!(buf.serialize(), b"abc def ghi\n");
    }

    #[test]
    fn undo_multi_edit_transaction() {
        // Two sequential inserts in one transaction: insert "X" at pos 1, then "Y" at pos 3
        // (pos 3 is in the document AFTER the first insert, i.e. cumulative coordinates).
        let mut buf = open(b"abc\n");
        let tx = EditTransaction::new(
            vec![
                TextEdit::insert(ByteOffset(1), b"X"),
                TextEdit::insert(ByteOffset(3), b"Y"),
            ],
            Selection::caret(ByteOffset(1)),
            Selection::caret(ByteOffset(4)),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.serialize(), b"aXbYc\n");
        // Undo should restore the original.
        buf.undo().unwrap();
        assert_eq!(buf.serialize(), b"abc\n");
        // Redo should re-apply both edits.
        buf.redo().unwrap();
        assert_eq!(buf.serialize(), b"aXbYc\n");
    }

    #[test]
    fn incremental_reparse_preserves_blocks_outside_edit() {
        let src = b"# Title\n\nFirst paragraph.\n\nSecond paragraph.\n";
        let mut buf = open(src);
        // Edit the first paragraph only.
        let para_span = buf.syntax().blocks.iter().find_map(|b| match b {
            Block::Paragraph(p) => {
                let text = String::from_utf8_lossy(&src[p.meta.span.start.0 as usize..p.meta.span.end.0 as usize]);
                if text.contains("First") { Some(p.meta.span) } else { None }
            }
            _ => None,
        }).expect("first paragraph");
        buf.replace_text_run(para_span, "First", "Changed").unwrap();
        let out = buf.serialize();
        assert_eq!(out, b"# Title\n\nChanged paragraph.\n\nSecond paragraph.\n");
        // The second paragraph block should still exist and have a valid span.
        let has_second = buf.syntax().blocks.iter().any(|b| match b {
            Block::Paragraph(p) => {
                let text = String::from_utf8_lossy(&out[p.meta.span.start.0 as usize..p.meta.span.end.0 as usize]);
                text.contains("Second paragraph.")
            }
            _ => false,
        });
        assert!(has_second, "second paragraph should be preserved by incremental reparse");
    }

    #[test]
    fn incremental_reparse_block_count_grows_on_split() {
        let src = b"# Title\n\nSome text.\n";
        let mut buf = open(src);
        // Insert a new paragraph after the existing one (one blank line separator).
        let insert_text = b"\nNew paragraph.\n";
        let tx = EditTransaction::single(
            TextEdit::insert(ByteOffset(src.len() as u64), insert_text),
            Selection::caret(ByteOffset(src.len() as u64)),
            Selection::caret(ByteOffset(src.len() as u64 + insert_text.len() as u64)),
        );
        buf.apply(tx).unwrap();
        let out = buf.serialize();
        assert_eq!(out, b"# Title\n\nSome text.\n\nNew paragraph.\n");
        // Should have more blocks than before (heading + blank + para + blank + para).
        assert!(buf.syntax().blocks.len() >= 4, "block count should grow: got {}", buf.syntax().blocks.len());
    }
}
