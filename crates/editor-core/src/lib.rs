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
    /// Undo-stack depth recorded by `mark_saved`. The document is dirty iff
    /// the current undo depth differs — so undoing back to the exact saved
    /// state clears the flag again instead of staying "modified" forever.
    saved_revision: usize,
}

impl DocumentBuffer {
    /// Open a document from raw bytes (the storage layer supplies these).
    pub fn open(bytes: Vec<u8>, meta: DocumentMeta, profile: MarkdownProfile) -> Result<Self, editor_domain::DocumentError> {
        let original: Arc<dyn editor_domain::ByteSource> = Arc::new(editor_domain::ArcByteSource::new(bytes));
        Self::open_from_buffer(original, meta, profile)
    }

    /// Open a document from a shared immutable buffer (zero-copy for mmap-backed files).
    /// The buffer is shared with the PieceTable as its immutable original (Invariant 1, 6).
    pub fn open_from_buffer(original: Arc<dyn editor_domain::ByteSource>, meta: DocumentMeta, profile: MarkdownProfile) -> Result<Self, editor_domain::DocumentError> {
        let table = PieceTable::from_original(Arc::clone(&original));
        let syntax = editor_markdown::parse_with(original.as_bytes(), profile.clone())?;
        Ok(Self {
            meta,
            profile,
            table,
            syntax,
            selection: SelectionModel::new(),
            undo: UndoManager::new(),
            dirty: false,
            saved_revision: 0,
        })
    }

    /// Open a large document lazily — creates a PieceTable with the full mmap
    /// (zero-copy, OS pages on demand) but parses only the first `chunk_size` bytes.
    /// Additional chunks are parsed on demand via `parse_next_chunk()`.
    /// This avoids parsing 100+ MB at once (Invariant 6: >RAM files).
    pub fn open_lazy(
        original: Arc<dyn editor_domain::ByteSource>,
        meta: DocumentMeta,
        profile: MarkdownProfile,
        chunk_size: usize,
    ) -> Result<Self, editor_domain::DocumentError> {
        let table = PieceTable::from_original(Arc::clone(&original));
        let total_len = original.len() as u64;
        let target_end = (chunk_size as u64).min(total_len);
        // Round the first chunk up to the next line boundary so we never
        // split a line between chunks (prevents corrupted block boundaries).
        let first_chunk_end = round_to_next_line_end(&table, target_end, total_len);
        // Parse only the first chunk. Blocks beyond this are loaded on demand.
        let syntax = editor_markdown::parse_range(
            original.as_bytes(),
            0,
            first_chunk_end,
            profile.clone(),
        )?;
        let syntax = editor_markdown::Document::from_blocks(syntax);
        Ok(Self {
            meta,
            profile,
            table,
            syntax,
            selection: SelectionModel::new(),
            undo: UndoManager::new(),
            dirty: false,
            saved_revision: 0,
        })
    }

    /// Parse the next chunk of the document starting at `offset`.
    /// Returns the byte offset where parsing stopped (end of chunk or end of file).
    /// The parsed blocks are merged into the syntax tree.
    pub fn parse_next_chunk(&mut self, offset: u64, chunk_size: usize) -> Result<u64, editor_domain::DocumentError> {
        let total_len = self.table.len();
        if offset >= total_len {
            return Ok(total_len);
        }
        // `offset + chunk_size` can wrap u64 for a degenerate chunk_size; a
        // wrapped end below `offset` would return a smaller offset and loop
        // the caller's "while parsed < total" iteration forever.
        let target_end = offset.saturating_add(chunk_size as u64).min(total_len);
        // Round up to the next line boundary so we never split a line across
        // chunks. Splitting a line produces partial blocks and breaks the
        // assumption in merge_blocks that new_blocks[0] starts at a logical
        // block boundary.
        let chunk_end = round_to_next_line_end(&self.table, target_end, total_len);
        // If the parsed frontier sits exactly at the end of an *unclosed*
        // fenced code block, the fence continues into this chunk — parsing it
        // as fresh Markdown would split the block into garbage nodes. Back the
        // window up to the block's start so the reparse sees the full fence;
        // merge_blocks then replaces the stale trailing block.
        let mut offset = offset;
        if let Some(last) = self.syntax.blocks.last() {
            let span = last.meta().span;
            let backs_up = match last {
                // An unclosed fence continues into this chunk — parsing it as
                // fresh Markdown would interpret the fence body as paragraphs.
                editor_markdown::Block::CodeBlock(cb) => {
                    cb.fenced && span.end.0 == offset && self.fence_unclosed(&span, cb)
                }
                // A block quote or list whose continuation lines start this
                // chunk would be split into two adjacent-but-separate blocks.
                // If the next content line is quote/list continuation, reparse
                // from the container's start so it merges into one node.
                editor_markdown::Block::BlockQuote(_) => {
                    span.end.0 == offset && self.chunk_continues_container(offset, chunk_end, b'>')
                }
                editor_markdown::Block::List(_) => {
                    span.end.0 == offset && self.chunk_continues_list(offset, chunk_end)
                }
                // A paragraph split mid-run: any next non-blank, non-marker
                // line is its lazy continuation — it must not start a second
                // paragraph.
                editor_markdown::Block::Paragraph(_) => {
                    span.end.0 == offset && self.chunk_continues_paragraph(offset, chunk_end)
                }
                _ => false,
            };
            if backs_up {
                offset = span.start.0;
            }
        }
        // Parse the *current* document bytes, not the immutable original: if the
        // user edited a lazy document before all chunks were parsed, the original
        // buffer is stale and would produce blocks that disagree with the piece
        // table (replace_block would then corrupt the document).
        let window = self.table.to_range(offset, chunk_end);
        let mut new_blocks = editor_markdown::parse_range(
            &window,
            0,
            window.len() as u64,
            self.profile.clone(),
        )?;
        // `parse_range` on a window slice returns 0-based spans; rebase to
        // absolute document offsets.
        for b in &mut new_blocks {
            editor_markdown::shift_block_spans(b, offset as i64);
        }
        // Merge new blocks into the syntax tree.
        self.syntax.merge_blocks(new_blocks);
        Ok(chunk_end)
    }

    /// Whether the fenced code block at `span` lacks a closing fence line —
    /// i.e. its last line is not a valid closer of the same fence character
    /// and at least `fence_len` characters long (CommonMark: the closer must
    /// be >= the opener and carry nothing but the fence characters and
    /// trailing whitespace).
    fn fence_unclosed(&self, span: &SourceSpan, cb: &editor_markdown::CodeBlock) -> bool {
        let bytes = self.table.to_range(span.start.0, span.end.0);
        // Last CONTENT line: a properly-closed fence ends with "```\n", so
        // the byte after the final '\n' is empty — strip trailing newlines
        // first, then take the line after the last remaining '\n'.
        let mut content: &[u8] = &bytes;
        while content.last() == Some(&b'\n') || content.last() == Some(&b'\r') {
            content = &content[..content.len() - 1];
        }
        let last_nl = content.iter().rposition(|&b| b == b'\n' || b == b'\r').map(|i| i + 1).unwrap_or(0);
        let last_line = &content[last_nl..];
        // A single-line block can't be closed: its last line IS the opener
        // (` "```\n" ` right at the chunk boundary) — the fence continues into
        // the next chunk. Treating the opener as a valid closer would leave
        // the fence body to be parsed as fresh Markdown.
        if last_nl == 0 {
            return true;
        }
        // CommonMark: a closing fence may be indented by at most 3 spaces.
        // A 4+-indented (or tab-indented) line is code content, not a closer —
        // the fence is still open and the chunk must back up.
        let leading_spaces = last_line.iter().take_while(|&&b| b == b' ').count();
        let after_indent = &last_line[leading_spaces..];
        if leading_spaces >= 4 || after_indent.first() == Some(&b'\t') {
            return true;
        }
        let trimmed: &[u8] = {
            let mut t = after_indent;
            while t.last().is_some_and(|b| b.is_ascii_whitespace()) {
                t = &t[..t.len() - 1];
            }
            t
        };
        let is_closer = !trimmed.is_empty()
            && trimmed.len() >= cb.fence_len as usize
            && trimmed.iter().all(|&b| b == cb.fence_char);
        !is_closer
    }

    /// True when the *first* line of `[offset, end)` continues a block quote:
    /// either another `>`-marked line or a lazy paragraph continuation — a
    /// non-blank, under-indented line that doesn't start a new block. Only
    /// the immediate next line can lazily continue (a blank ends the quote),
    /// so we must not skip blanks here.
    fn chunk_continues_container(&self, offset: u64, end: u64, marker: u8) -> bool {
        let bytes = self.table.to_range(offset, end.min(offset + 256));
        let Some(line) = logical_lines(&bytes).next() else {
            return false;
        };
        let mut l = line;
        let mut indent = 0;
        while l.first() == Some(&b' ') && indent < 4 {
            indent += 1;
            l = &l[1..];
        }
        if l.first() == Some(&marker) {
            return true;
        }
        if l.iter().all(|b| b.is_ascii_whitespace()) {
            return false; // a blank line always ends the quote
        }
        // A 4+-indented line can only join the quote as a lazy paragraph
        // continuation (indented code never interrupts a paragraph) — merge
        // and let the reparse decide.
        if indent >= 4 {
            return true;
        }
        // Lazy continuation mirrors the parser: any line that isn't a new
        // block start joins the quote's open paragraph. Whether the inner
        // state is actually a paragraph is re-decided by the reparse —
        // over-merging here is safe (the reparse is authoritative).
        !editor_markdown::parser::line_starts_new_block(l)
    }

    /// True when the next chunk's first non-blank line is plain text — a lazy
    /// paragraph continuation rather than a new block (heading, quote, list,
    /// fence, indented code, or a block start after 4+ spaces of indent).
    /// Over-matching here is safe: the reparse window is authoritative and
    /// will still emit the real second block if the line does start one.
    fn chunk_continues_paragraph(&self, offset: u64, end: u64) -> bool {
        let bytes = self.table.to_range(offset, end.min(offset + 256));
        let mut skipped_blank = false;
        for line in logical_lines(&bytes) {
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                skipped_blank = true;
                continue;
            }
            // A blank line always terminates the paragraph — whatever follows
            // is a new block.
            if skipped_blank {
                return false;
            }
            let mut l = line;
            let mut indent = 0;
            while l.first() == Some(&b' ') && indent < 4 {
                indent += 1;
                l = &l[1..];
            }
            if l.is_empty() {
                return false;
            }
            // Indented code cannot interrupt a paragraph: a >=4-indent line
            // directly after a paragraph line is a lazy continuation.
            if indent >= 4 {
                return true;
            }
            // Delegate to the parser's own block-start predicate — the
            // ad-hoc prefix list here used to split `#x` (no space after the
            // hash is NOT a heading — it's a lazy continuation), `>x`,
            // `-x`, `1.x`, HTML blocks, etc. differently from a full parse,
            // producing two paragraph blocks where one belongs.
            // `---`/`===`-runs are already excluded inside the predicate:
            // they may be setext underlines continuing the paragraph, so we
            // merge and let the reparse decide (thematic break vs H2).
            return !editor_markdown::parser::line_starts_new_block(l);
        }
        false
    }

    /// True when the first non-blank line of `[offset, end)` continues a list:
    /// a new list marker (`-`, `*`, `+`, `N.`/`N)`), an indented continuation
    /// (>=2 leading spaces), or — for the *first* line only — a lazy
    /// paragraph continuation (non-blank, under-indented, not a block start).
    /// A lazy line can only continue the item's open paragraph when it
    /// directly follows it, so blanks before it rule laziness out.
    fn chunk_continues_list(&self, offset: u64, end: u64) -> bool {
        let bytes = self.table.to_range(offset, end.min(offset + 256));
        let mut first_line = true;
        for line in logical_lines(&bytes) {
            // `first_line` must turn false after the *physical* first line —
            // a leading blank kills laziness for everything after it, so
            // reset before the blank-skip `continue`, not after it.
            let is_first = first_line;
            first_line = false;
            let mut l = line;
            let mut indent = 0;
            while l.first() == Some(&b' ') {
                indent += 1;
                l = &l[1..];
            }
            if l.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            if indent >= 2 {
                return true; // indented continuation of the last item
            }
            if matches!(l.first(), Some(b'-') | Some(b'*') | Some(b'+'))
                && l.get(1).is_some_and(|b| b.is_ascii_whitespace())
            {
                return true;
            }
            // Ordered marker: digits then '.' or ')' then whitespace.
            let digits = l.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 0
                && digits <= 9
                && matches!(l.get(digits), Some(b'.') | Some(b')'))
                && l.get(digits + 1).is_some_and(|b| b.is_ascii_whitespace())
            {
                return true;
            }
            // Lazy continuation of the last item's paragraph: only the very
            // first line of the chunk, and only when it doesn't start a new
            // block. Over-merging is safe — the reparse is authoritative.
            if is_first && !editor_markdown::parser::line_starts_new_block(l) {
                return true;
            }
            return false;
        }
        false
    }

    /// How many bytes of the document have been parsed so far.
    pub fn parsed_offset(&self) -> u64 {
        self.syntax.parsed_offset
    }

    /// Total document length in bytes (may be larger than parsed_offset for lazy docs).
    pub fn total_len(&self) -> u64 {
        self.table.len()
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
        // Capture removed bytes for each edit *as it is applied*, so the transaction
        // is invertible (undo). Edit ranges are interpreted in the document state
        // after all previous edits in the transaction (cumulative coordinates —
        // see apply_inverse), so the removed bytes must come from that same state,
        // not from the pre-transaction document.
        let single_edit = tx.edits.len() == 1;
        // Compute the net byte delta and the affected range before applying edits.
        let (affected_start, affected_end_pre, net_delta) = compute_edit_impact(&tx.edits);
        for i in 0..tx.edits.len() {
            // Bounds + UTF-8 boundary check against the CURRENT table state
            // (correct for both cumulative and last-to-first batches — each
            // edit's range is interpreted in the state left by the previous
            // edits). PieceTable::apply_edit clamps out-of-range cuts, which
            // would silently turn a stale-range edit into an append; a cut
            // inside a multi-byte char would corrupt the text. Reject both.
            let edit = &tx.edits[i];
            let doc_len = self.table.len();
            let valid = edit.range.start.0 <= edit.range.end.0
                && edit.range.end.0 <= doc_len
                && [edit.range.start.0, edit.range.end.0].iter().all(|&pos| {
                    pos >= doc_len
                        || !matches!(
                            self.table.to_range(pos, pos + 1).first(),
                            Some(&b) if (0x80..=0xBF).contains(&b)
                        )
                });
            if !valid {
                // Roll back the already-applied edits so a rejected
                // transaction never leaves a half-applied, un-undoable
                // change in the buffer.
                for prev in tx.edits[..i].iter().rev() {
                    let s = prev.range.start.0;
                    let e = s + prev.replacement.len() as u64;
                    self.table.replace(
                        ByteRange::new(ByteOffset(s), ByteOffset(e)),
                        &prev.removed,
                    );
                }
                return Err(editor_domain::DocumentError::InvalidEdit);
            }
            let edit = &mut tx.edits[i];
            edit.removed = self.table.extract_bytes(edit.range);
            self.table.replace(edit.range, &edit.replacement);
        }
        self.selection.set_primary(tx.selection_after);
        // Pushing while the redo stack is non-empty abandons the alternate
        // history branch — if the saved state lived there, it can never be
        // reached again, so the depth marker must stop matching.
        if !self.undo.redo.is_empty() {
            self.saved_revision = usize::MAX;
        }
        self.undo.push(tx);
        if single_edit {
            // Incremental reparse: find blocks overlapping the affected range, reparse
            // only that window from the post-edit bytes, splice in the new blocks,
            // and shift subsequent blocks by the net delta (ADR-003 §58).
            // On failure the piece table is already edited and the undo entry is
            // recorded — returning Err here would report "edit failed" while the
            // bytes actually changed and `syntax` no longer describes them.
            // Retry with a full reparse (a superset of the incremental input, so
            // it can only fail for reasons that also invalidate the document);
            // if that fails too, roll the table back so error == no change.
            if self
                .incremental_reparse(affected_start, affected_end_pre, net_delta)
                .is_err()
            {
                let bytes = self.table.to_bytes();
                match editor_markdown::parse_with(&bytes, self.profile.clone()) {
                    Ok(syn) => self.syntax = syn,
                    Err(e) => {
                        // Discard the undo entry outright — `undo.undo()`
                        // would push the rolled-back transaction onto the
                        // redo stack, offering to re-apply an edit that was
                        // never committed.
                        if let Some(rolled) = self.undo.undo.pop() {
                            for prev in rolled.edits.iter().rev() {
                                let s = prev.range.start.0;
                                let end = s + prev.replacement.len() as u64;
                                self.table.replace(
                                    ByteRange::new(ByteOffset(s), ByteOffset(end)),
                                    &prev.removed,
                                );
                            }
                            self.selection.set_primary(rolled.selection_before);
                        }
                        self.update_dirty();
                        return Err(e);
                    }
                }
            }
        } else if self.edits_beyond_parsed_frontier(affected_start, net_delta) {
            // Multi-edit transaction entirely inside the unparsed tail of a lazy
            // document: the parsed block tree is untouched, and the upcoming
            // `parse_next_chunk` calls read the edited bytes from the piece
            // table. Skipping the reparse avoids materializing a >RAM document.
        } else {
            // Multi-edit transactions mix coordinate frames (e.g. "last match
            // first" replace-all batches, or cumulative coordinates), so a single
            // contiguous affected window cannot be derived reliably. Fall back to
            // a full reparse — correctness over speed for the batch path.
            let bytes = self.table.to_bytes();
            self.syntax = editor_markdown::parse_with(&bytes, self.profile.clone())?;
        }
        self.update_dirty();
        Ok(())
    }

    /// True when the document is lazily parsed and the just-applied change
    /// (whose net delta is `net_delta`) started entirely beyond the parsed
    /// frontier. `pre_len` is the document length *before* the change.
    fn edits_beyond_parsed_frontier(&self, affected_start: u64, net_delta: i64) -> bool {
        let pre_len = (self.table.len() as i64 - net_delta).max(0) as u64;
        self.syntax.parsed_offset < pre_len && affected_start >= self.syntax.parsed_offset
    }

    /// Incremental reparse: reparse only the blocks overlapping `[affected_start,
    /// affected_end_pre)` (pre-edit coordinates); `net_delta` is the byte length
    /// difference (new - old) introduced by the just-applied change. Only the
    /// affected window is copied out of the piece table, so lazy (>RAM)
    /// documents are never materialized for a single edit.
    fn incremental_reparse(
        &mut self,
        affected_start: u64,
        affected_end_pre: u64,
        net_delta: i64,
    ) -> Result<(), editor_domain::DocumentError> {
        // Lazy documents: an edit entirely beyond the parsed frontier touches no
        // parsed block. The piece table already holds the new bytes and the next
        // `parse_next_chunk` will see them — nothing to reparse.
        if self.edits_beyond_parsed_frontier(affected_start, net_delta) {
            return Ok(());
        }
        // Find the first and last top-level blocks that overlap the affected
        // range. Bounds are inclusive so a point insert exactly on a block
        // boundary reparses the block whose leading bytes the insert touches.
        let blocks = &self.syntax.blocks;
        let mut first_idx = None;
        let mut last_idx = None;
        for (i, b) in blocks.iter().enumerate() {
            let span = b.meta().span;
            if span.end.0 >= affected_start && span.start.0 <= affected_end_pre {
                if first_idx.is_none() {
                    first_idx = Some(i);
                }
                last_idx = Some(i);
            }
        }
        // If no blocks overlap (e.g. an empty document or a gap we cannot
        // localize), fall back to a full reparse.
        let (Some(first), Some(last)) = (first_idx, last_idx) else {
            let bytes = self.table.to_bytes();
            self.syntax = editor_markdown::parse_with(&bytes, self.profile.clone())?;
            return Ok(());
        };
        // The reparse window starts at the first affected block. Its END is
        // not fixed: an edit can move block boundaries (deleting a fence
        // closer merges the code block with following paragraphs, deleting a
        // blank line merges two paragraphs). Extend the window block-by-block
        // until the reparse resynchronizes — i.e. the last newly parsed block
        // starts at or after the old last block's start, so its end (the
        // window end = an old block boundary) was reached naturally, not by
        // running out of input.
        let window_start = blocks[first].meta().span.start.0;
        let old_spans: Vec<SourceSpan> = blocks.iter().map(|b| b.meta().span).collect();
        let shift = |v: u64| (v as i64 + net_delta).max(0) as u64;
        let mut last = last;
        let new_blocks = loop {
            let window_end_pre = old_spans[last].end.0;
            let window_end_post = shift(window_end_pre).min(self.table.len());
            // Copy only the affected window out of the piece table.
            let window = self.table.to_range(window_start, window_end_post);
            let mut nb = editor_markdown::parse_range(
                &window,
                0,
                window.len() as u64,
                self.profile.clone(),
            )?;
            // Rebase window-local spans onto absolute document offsets.
            for b in &mut nb {
                editor_markdown::shift_block_spans(b, window_start as i64);
            }
            let crosses_boundary = nb
                .last()
                .map(|l| l.meta().span.start.0 < shift(old_spans[last].start.0))
                .unwrap_or(false);
            // The last new block started before the old block `last` did and
            // runs to the window's end — it may continue into the next block.
            if crosses_boundary && last + 1 < old_spans.len() {
                last += 1;
                continue;
            }
            break nb;
        };
        // Shift blocks after the window by net_delta — every document-coordinate
        // span, including nested ones (list item meta, table rows/cells, inlines),
        // via the shared helper. Blocks are a contiguous partition, so every block
        // after `last` starts at/after window_end_pre and shifts unconditionally.
        for b in &mut self.syntax.blocks[last + 1..] {
            editor_markdown::shift_block_spans(b, net_delta);
        }
        // Splice: replace blocks[first..=last] with new_blocks.
        self.syntax.blocks.splice(first..=last, new_blocks);
        // The parsed frontier is defined by coverage: it must equal the last
        // parsed block's end. A blind `parsed_offset += net_delta` overstates
        // the frontier when the reparse window produced shorter blocks (e.g.
        // the edit split the frontier block) — `parse_next_chunk` would then
        // resume past the coverage end and skip the gap bytes forever: no
        // block would ever cover them. Deriving the frontier from the last
        // block keeps `parse_next_chunk`'s `span.end == offset` continuation
        // checks and the gap-recovery property intact.
        let coverage_end = self
            .syntax
            .blocks
            .last()
            .map(|b| b.meta().span.end.0)
            .unwrap_or(0);
        self.syntax.parsed_offset = coverage_end;
        // Document span end follows the last parsed block (== total len for
        // fully-parsed documents, == parsed_offset for lazy ones).
        self.syntax.span.end = ByteOffset(coverage_end);
        Ok(())
    }

    /// Undo the last transaction by applying inverse edits in reverse order.
    pub fn undo(&mut self) -> Result<(), editor_domain::DocumentError> {
        let Some(tx) = self.undo.undo() else {
            return Ok(());
        };
        if let Err(e) = self.apply_inverse(&tx) {
            // The inverse edits already hit the table before reparse failed —
            // re-apply the forward edits so "undo failed" means "nothing
            // changed", then move the transaction back onto the undo stack.
            for edit in &tx.edits {
                self.table.replace(edit.range, &edit.replacement);
            }
            let _ = self.undo.redo.pop();
            self.undo.undo.push(tx);
            self.update_dirty();
            return Err(e);
        }
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
        let reparse = if tx.edits.len() == 1 {
            let e = &tx.edits[0];
            // Same contract as apply(): a failed window reparse retries with a
            // full reparse before giving up — the bytes are already committed.
            if self
                .incremental_reparse(e.range.start.0, e.range.end.0, e.delta())
                .is_ok()
            {
                Ok(())
            } else {
                let bytes = self.table.to_bytes();
                editor_markdown::parse_with(&bytes, self.profile.clone()).map(|syn| {
                    self.syntax = syn;
                })
            }
        } else {
            let (start, _end, delta) = compute_edit_impact(&tx.edits);
            if self.edits_beyond_parsed_frontier(start, delta) {
                // Same lazy-tail shortcut as in apply() — a full reparse here
                // would materialize a >RAM document just to redo a tail edit.
                Ok(())
            } else {
                let bytes = self.table.to_bytes();
                editor_markdown::parse_with(&bytes, self.profile.clone()).map(|syn| {
                    self.syntax = syn;
                })
            }
        };
        if let Err(e) = reparse {
            // Roll the table back (inverse edits) and put the transaction
            // back on the redo stack — a failed redo must not consume it.
            for edit in tx.edits.iter().rev() {
                let s = edit.range.start.0;
                let end = s + edit.replacement.len() as u64;
                self.table.replace(
                    ByteRange::new(ByteOffset(s), ByteOffset(end)),
                    &edit.removed,
                );
            }
            let _ = self.undo.undo.pop();
            self.undo.redo.push(tx);
            self.update_dirty();
            return Err(e);
        }
        self.selection.set_primary(tx.selection_after);
        self.update_dirty();
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
        let reparse = if tx.edits.len() == 1 {
            let e = &tx.edits[0];
            // The inverse change replaced [s, s+|replacement|) with `removed`.
            let start = e.range.start.0;
            let end_pre = start + e.replacement.len() as u64;
            let delta = e.removed.len() as i64 - e.replacement.len() as i64;
            self.incremental_reparse(start, end_pre, delta)
        } else {
            // Inverse delta is the negation of the original net delta; edits
            // wholly beyond the parsed frontier still touch no parsed block.
            let (start, _end, delta) = compute_edit_impact(&tx.edits);
            if self.edits_beyond_parsed_frontier(start, -delta) {
                // skip reparse — the parsed tree is untouched
                Ok(())
            } else {
                let bytes = self.table.to_bytes();
                editor_markdown::parse_with(&bytes, self.profile.clone()).map(|syn| {
                    self.syntax = syn;
                })
            }
        };
        // Fall back to a full reparse when the incremental window could not
        // be rebuilt — the table bytes are already reverted, so returning Err
        // here would leave `syntax` describing the pre-undo document.
        if reparse.is_err() {
            let bytes = self.table.to_bytes();
            self.syntax = editor_markdown::parse_with(&bytes, self.profile.clone())?;
        }
        self.update_dirty();
        Ok(())
    }

    /// Serialize the current document to bytes (source-preserving; §2, Invariant 1/2).
    pub fn serialize(&self) -> Vec<u8> {
        // The serializer uses the *current* bytes as the verbatim store so unchanged
        // regions are emitted byte-for-byte.
        let current = self.table.to_bytes();
        serialize(&self.syntax, &current)
    }

    /// Extract a byte range [start, end) from the current document without
    /// serializing the entire buffer. Used for virtualized rendering of large
    /// documents — only the requested block's bytes are copied.
    pub fn serialize_range(&self, start: u64, end: u64) -> Vec<u8> {
        self.table.to_range(start, end)
    }

    /// Replace a text run within a paragraph and return the resulting bytes (minimal diff).
    /// Convenience for the WYSIWYG "replace word" flow (§2 example).
    pub fn replace_text_run(
        &mut self,
        para_span: SourceSpan,
        old: &str,
        new: &str,
    ) -> Result<(), editor_domain::DocumentError> {
        // `windows(0)` panics — reject an empty needle explicitly.
        if old.is_empty() {
            return Err(editor_domain::DocumentError::InvalidEdit);
        }
        // A stale span (e.g. captured before a later edit) must not silently
        // corrupt the document — validate bounds against the table length.
        if para_span.start.0 > para_span.end.0 || para_span.end.0 > self.table.len() {
            return Err(editor_domain::DocumentError::InvalidEdit);
        }
        // Copy out only the paragraph span — never materialize a lazy (>RAM)
        // document just to find a needle inside one block (Invariant 6).
        let region = self.table.to_range(para_span.start.0, para_span.end.0);
        let Some(pos) = region.windows(old.len()).position(|w| w == old.as_bytes()) else {
            return Err(editor_domain::DocumentError::InvalidEdit);
        };
        let abs = para_span.start.0 + pos as u64;
        let edit = TextEdit::replace(
            ByteRange::new(ByteOffset(abs), ByteOffset(abs + old.len() as u64)),
            new.as_bytes(),
        );
        let before = self.selection.primary();
        let after = Selection::caret(ByteOffset(abs + new.len() as u64));
        self.apply(EditTransaction::single(edit, before, after))
    }

    /// Mark the buffer as saved (clears dirty flag; does NOT commit to Git — §62).
    pub fn mark_saved(&mut self) {
        self.saved_revision = self.undo.undo.len();
        self.dirty = false;
    }

    /// Recompute `dirty` from the undo position: the document differs from the
    /// saved state iff the undo stack is not at the saved revision.
    fn update_dirty(&mut self) {
        self.dirty = self.undo.undo.len() != self.saved_revision;
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

/// Iterate logical lines of a byte window where `\n`, `\r\n`, and bare `\r`
/// each count as a single line terminator. Splitting naively on both bytes
/// would emit a phantom empty line between the `\r` and `\n` of a CRLF pair,
/// which the blank-line checks in the chunk-continuation heuristics would
/// misread as a real blank line.
fn logical_lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|&b| b == b'\n').flat_map(|piece| {
        // A trailing '\r' belongs to a '\r\n' terminator; interior '\r's are
        // bare-CR line endings.
        let piece = piece.strip_suffix(b"\r").unwrap_or(piece);
        piece.split(|&b| b == b'\r')
    })
}

/// Round a target byte offset up to the next line ending (the byte after a
/// `\n`, or `total_len` if there is no later line start). This keeps chunk
/// boundaries on line boundaries so the parser never starts in the middle of
/// a line, which would produce split blocks and corrupt the block tree.
fn round_to_next_line_end(table: &PieceTable, target: u64, total_len: u64) -> u64 {
    if target >= total_len {
        return total_len;
    }
    let line_starts = table.line_starts();
    match line_starts.binary_search(&target) {
        // target is exactly a line start -> use it as the chunk end (the
        // previous line ends here, next chunk starts at a fresh line).
        Ok(i) => line_starts.get(i).copied().unwrap_or(total_len),
        // target is inside a line -> advance to the next line start.
        Err(i) => line_starts.get(i).copied().unwrap_or(total_len),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_domain::{ids::DocumentId, ArcByteSource, ByteSource};
    use editor_markdown::Block;
    use std::sync::Arc;

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

    /// Dirty tracking is revision-based: undoing all the way back to the
    /// saved state must clear the flag, not leave a phantom "modified".
    #[test]
    fn dirty_clears_when_undo_returns_to_saved_state() {
        let ins = |buf: &mut DocumentBuffer, s: &'static str, pos: u64| {
            buf.apply(EditTransaction::single(
                TextEdit::insert(ByteOffset(pos), s.as_bytes()),
                Selection::caret(ByteOffset(pos)),
                Selection::caret(ByteOffset(pos)),
            ))
            .unwrap();
        };
        let mut buf = open(b"ab\n");
        ins(&mut buf, "X", 1); // aXb
        ins(&mut buf, "Y", 1); // aYXb
        assert!(buf.is_dirty());
        buf.mark_saved();
        assert!(!buf.is_dirty());

        ins(&mut buf, "Z", 1); // aZYXb — dirty again
        assert!(buf.is_dirty());
        buf.undo().unwrap(); // back to saved bytes
        assert!(!buf.is_dirty(), "undo to the saved revision must clear dirty");
        buf.redo().unwrap();
        assert!(buf.is_dirty());
    }

    /// The false-clean trap of position-based dirty tracking: save at depth N,
    /// undo below N, then push new edits until the depth numerically returns
    /// to N. The content is different from the saved bytes, but the depth
    /// matches — so a diverged push must invalidate the saved revision.
    #[test]
    fn dirty_sticks_after_history_divergence() {
        let ins = |buf: &mut DocumentBuffer, s: &'static str, pos: u64| {
            buf.apply(EditTransaction::single(
                TextEdit::insert(ByteOffset(pos), s.as_bytes()),
                Selection::caret(ByteOffset(pos)),
                Selection::caret(ByteOffset(pos)),
            ))
            .unwrap();
        };
        let mut buf = open(b"ab\n");
        ins(&mut buf, "X", 1); // aXb  — depth 1
        ins(&mut buf, "Y", 1); // aYXb — depth 2
        buf.mark_saved();      // saved at depth 2, content "aYXb\n"
        buf.undo().unwrap();   // depth 1
        buf.undo().unwrap();   // depth 0, content "ab\n"
        // Diverge: two new pushes bring depth back to 2 == saved_revision.
        ins(&mut buf, "P", 1); // depth 1
        ins(&mut buf, "Q", 1); // depth 2 — same depth as saved, content differs
        assert_eq!(buf.serialize(), b"aQPb\n");
        assert!(
            buf.is_dirty(),
            "undo depth numerically equal to saved_revision after diverging \
             must NOT read as clean (content differs from the saved bytes)"
        );
        // And it must not "heal" by undoing back to identical depth states
        // that happen to share the saved content — conservative is safe.
        buf.undo().unwrap();
        buf.undo().unwrap();
        assert_eq!(buf.serialize(), b"ab\n");
        assert!(buf.is_dirty(), "post-divergence states are never at the saved revision");
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

    /// Multi-edit transaction where a later edit's range is in *cumulative*
    /// coordinates (the document state after the previous edits). `removed` must
    /// be captured per-edit from that same state — otherwise undo restores the
    /// wrong bytes.
    #[test]
    fn undo_multi_edit_transaction_with_replacements() {
        let mut buf = open(b"aaa bbb ccc ddd\n");
        // Edit 1: delete "aaa " → doc becomes "bbb ccc ddd\n".
        // Edit 2 (cumulative coords): replace "ccc" (now at [4,7)) with "XXX".
        let tx = EditTransaction::new(
            vec![
                TextEdit::delete(editor_domain::ByteRange::new(ByteOffset(0), ByteOffset(4))),
                TextEdit::replace(
                    editor_domain::ByteRange::new(ByteOffset(4), ByteOffset(7)),
                    b"XXX",
                ),
            ],
            Selection::caret(ByteOffset(0)),
            Selection::caret(ByteOffset(0)),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.serialize(), b"bbb XXX ddd\n");
        buf.undo().unwrap();
        assert_eq!(buf.serialize(), b"aaa bbb ccc ddd\n", "undo must restore exact original bytes");
        buf.redo().unwrap();
        assert_eq!(buf.serialize(), b"bbb XXX ddd\n");
    }

    /// Same shape but delete + insert (not just replace).
    #[test]
    fn undo_multi_edit_transaction_delete_then_delete() {
        let mut buf = open(b"ABCDE\n");
        // Delete [0,2) "AB" → "CDE\n"; then cumulative delete [0,2) "CD" → "E\n".
        let tx = EditTransaction::new(
            vec![
                TextEdit::delete(editor_domain::ByteRange::new(ByteOffset(0), ByteOffset(2))),
                TextEdit::delete(editor_domain::ByteRange::new(ByteOffset(0), ByteOffset(2))),
            ],
            Selection::caret(ByteOffset(0)),
            Selection::caret(ByteOffset(0)),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.serialize(), b"E\n");
        buf.undo().unwrap();
        assert_eq!(buf.serialize(), b"ABCDE\n");
    }

    /// After an edit before a list changes the byte length, the list's nested
    /// spans (item meta used by `toggle_task_item`) must be shifted to the new
    /// absolute positions — not just the top-level list span.
    #[test]
    fn incremental_reparse_shifts_nested_spans() {
        let src = b"# Title\n\n- [ ] task one\n- [ ] task two\n";
        let mut buf = open(src);
        // Insert 5 bytes inside the heading — shifts the whole list by +5.
        let tx = EditTransaction::single(
            TextEdit::insert(ByteOffset(2), b"XXXXX"),
            Selection::caret(ByteOffset(2)),
            Selection::caret(ByteOffset(7)),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.serialize(), b"# XXXXXTitle\n\n- [ ] task one\n- [ ] task two\n");
        // The list item span must point at the shifted source bytes.
        let list = buf.syntax().blocks.iter().find_map(|b| match b {
            Block::List(l) => Some(l),
            _ => None,
        }).expect("list");
        let out = buf.serialize();
        let item0 = &out[list.items[0].meta.span.start.0 as usize..list.items[0].meta.span.end.0 as usize];
        assert_eq!(item0, b"- [ ] task one\n");
        // Toggling must edit the checkbox at the *shifted* offset, not stale bytes.
        let toggled = editor_markdown::serialize::toggle_task_item(&out, &list.items[0])
            .expect("toggle");
        assert!(toggled.windows(15).any(|w| w == b"- [x] task one\n"));
    }

    /// A batch of same-range edits in original coordinates ordered last-first
    /// (the replace-all convention) must also invert exactly.
    #[test]
    fn undo_replace_all_style_transaction() {
        let mut buf = open(b"foo bar foo baz foo\n");
        // Replace all three "foo" (at 0, 8, 16) with "X", applied last-first.
        let tx = EditTransaction::new(
            [16usize, 8, 0]
                .iter()
                .map(|&s| {
                    TextEdit::replace(
                        editor_domain::ByteRange::new(ByteOffset(s as u64), ByteOffset(s as u64 + 3)),
                        b"X",
                    )
                })
                .collect(),
            Selection::caret(ByteOffset(0)),
            Selection::caret(ByteOffset(0)),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.serialize(), b"X bar X baz X\n");
        buf.undo().unwrap();
        assert_eq!(buf.serialize(), b"foo bar foo baz foo\n");
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

    #[test]
    fn lazy_open_parses_first_chunk_and_continues() {
        // Build a source with a block quote that spans past a small chunk boundary,
        // forcing the chunk boundary to fall inside the quote. With line-boundary
        // alignment, parsing should still be byte-identical.
        let src = b"# Title\n\nParagraph one.\n\n> quote line one\n> quote line two\n\nParagraph two.\n";
        let original: Arc<dyn ByteSource> = Arc::new(ArcByteSource::new(src.to_vec()));
        let meta = DocumentMeta {
            id: DocumentId::new("test"),
            has_bom: false,
            line_ending: editor_domain::LineEnding::Lf,
            trailing_newline: src.last() == Some(&b'\n'),
            encoding: editor_domain::Encoding::Utf8,
        };
        // Use a tiny chunk size that falls inside the block quote to exercise
        // the boundary alignment and merge_blocks logic.
        let mut buf = DocumentBuffer::open_lazy(original, meta, MarkdownProfile::Gfm, 24).unwrap();

        // First chunk should be parsed (and aligned to a line boundary).
        assert!(!buf.syntax().blocks.is_empty(), "first chunk should produce blocks");

        // Parse the next chunk(s) until the document is fully parsed.
        let mut parsed = buf.parsed_offset();
        let total = buf.total_len();
        let mut iterations = 0;
        while parsed < total && iterations < 10 {
            parsed = buf.parse_next_chunk(parsed, 24).unwrap();
            iterations += 1;
        }

        // Should be fully parsed and byte-identical.
        assert_eq!(parsed, total, "parser should reach total length: parsed={parsed}, total={total}");
        assert_eq!(buf.serialize(), src, "lazy chunked parse must be byte-identical");
    }

    /// A fenced code block spanning a chunk boundary must stay ONE block:
    /// parsing the next chunk as fresh Markdown would interpret the fence's
    /// body as paragraphs and split it. The frontier must back up to the
    /// unclosed block's start instead.
    #[test]
    fn lazy_fence_spanning_chunk_boundary_stays_one_block() {
        // Chunk boundary lands inside the fence body.
        let src = b"# T\n\n```rust\nlet a = 1;\nlet b = 2;\nlet c = 3;\n```\n\nTail.\n";
        let mut buf = open_lazy(src, 20); // 20B → first chunk ends mid-fence
        assert!(buf.parsed_offset() < buf.total_len());
        // The truncated first chunk produced a code block ending at the frontier.
        let mut parsed = buf.parsed_offset();
        while parsed < buf.total_len() {
            parsed = buf.parse_next_chunk(parsed, 20).unwrap();
        }
        // Exactly one code block covering the whole fence; no paragraph split
        // out of the fence body.
        let code_blocks = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::CodeBlock(_)))
            .count();
        assert_eq!(code_blocks, 1, "fence must survive the chunk boundary as one block");
        let paragraphs = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Paragraph(_)))
            .count();
        assert_eq!(paragraphs, 1, "only the tail paragraph — fence body must not produce paragraphs");
        assert_eq!(buf.serialize(), src);
    }

    /// A fence whose OPENER is the last line of the chunk (block span is a
    /// single line) is still unclosed — the body lives in the next chunk.
    /// Mistaking the opener line for a closer would parse the body as fresh
    /// Markdown paragraphs.
    #[test]
    fn lazy_fence_opener_alone_at_boundary_stays_unclosed() {
        // Chunk boundary lands right after the fence opener line.
        let src = b"para one\npara two\n```\nlet x = 1;\nlet y = 2;\n";
        let mut buf = open_lazy(src, 20); // rounds to line end at offset 22 — right after "```"
        let mut parsed = buf.parsed_offset();
        let mut iterations = 0;
        while parsed < buf.total_len() && iterations < 20 {
            parsed = buf.parse_next_chunk(parsed, 20).unwrap();
            iterations += 1;
        }
        assert_eq!(buf.serialize(), src);
        let code_blocks = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::CodeBlock(_)))
            .count();
        assert_eq!(code_blocks, 1, "fence body must not be re-parsed as Markdown");
        // No paragraph for the code body — only the leading paragraph.
        let paragraphs = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Paragraph(_)))
            .count();
        assert_eq!(paragraphs, 1);
    }

    /// A fence that DOES close before the chunk boundary must not trigger the
    /// back-up — the check is specifically for unclosed fences.
    #[test]
    fn lazy_closed_fence_before_boundary_does_not_back_up() {
        let src = b"```\nx\n```\n\npara one\n\npara two\n";
        let mut buf = open_lazy(src, 5);
        let mut parsed = buf.parsed_offset();
        let mut iterations = 0;
        while parsed < buf.total_len() && iterations < 20 {
            parsed = buf.parse_next_chunk(parsed, 5).unwrap();
            iterations += 1;
        }
        assert_eq!(buf.serialize(), src);
        let code_blocks = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::CodeBlock(_)))
            .count();
        assert_eq!(code_blocks, 1);
        // Both tail paragraphs parsed (not swallowed by a wrongly-extended fence).
        let paragraphs = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Paragraph(_)))
            .count();
        assert_eq!(paragraphs, 2);
    }

    /// A block quote continuing across the chunk boundary must stay ONE
    /// quote block, not split into two adjacent quotes.
    #[test]
    fn lazy_blockquote_spanning_boundary_stays_one_block() {
        let src = b"> line one\n> line two\n> line three\n\npara\n";
        let mut buf = open_lazy(src, 12); // boundary inside the quote
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 20 {
            parsed = buf.parse_next_chunk(parsed, 12).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let quotes = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::BlockQuote(_)))
            .count();
        assert_eq!(quotes, 1, "quote spanning the boundary must be one block");
    }

    /// A list continuing across the boundary stays one List.
    #[test]
    fn lazy_list_spanning_boundary_stays_one_block() {
        let src = b"- item one\n- item two\n- item three\n- item four\n";
        let mut buf = open_lazy(src, 13); // boundary inside the list
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 20 {
            parsed = buf.parse_next_chunk(parsed, 13).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let lists = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::List(_)))
            .count();
        assert_eq!(lists, 1, "list spanning the boundary must be one block");
        // And all four items survived.
        let items: usize = buf
            .syntax()
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::List(l) => Some(l.items.len()),
                _ => None,
            })
            .sum();
        assert_eq!(items, 4);
    }

    /// A quote that really ends before the boundary (blank line, then `>`)
    /// must produce TWO quotes — the back-up must not over-merge.
    #[test]
    fn lazy_blank_line_then_quote_does_not_overmerge() {
        let src = b"> first quote\n\n> second quote\n";
        let mut buf = open_lazy(src, 14);
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 20 {
            parsed = buf.parse_next_chunk(parsed, 14).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let quotes = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::BlockQuote(_)))
            .count();
        assert_eq!(quotes, 2, "quotes separated by a blank line stay separate");
    }

    /// A paragraph split mid-run must stay ONE paragraph — continuation lines
    /// in the next chunk are lazy paragraph lines, not a new paragraph.
    #[test]
    fn lazy_paragraph_spanning_boundary_stays_one_block() {
        let src = b"line one of para\nline two of para\nline three\n\nnext\n";
        let mut buf = open_lazy(src, 17);
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 20 {
            parsed = buf.parse_next_chunk(parsed, 17).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let paras = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Paragraph(_)))
            .count();
        assert_eq!(paras, 2, "boundary-split paragraph must merge into one");
    }

    /// `#x` (hash without a following space) is NOT a heading — it's a lazy
    /// paragraph continuation. A chunk boundary before it must still yield
    /// one paragraph, matching a full parse.
    #[test]
    fn lazy_hash_without_space_continues_paragraph() {
        let src = b"para text here\n#x not a heading\nmore text\n";
        let mut buf = open_lazy(src, 15); // boundary inside the paragraph run
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 20 {
            parsed = buf.parse_next_chunk(parsed, 15).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let paras = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Paragraph(_)))
            .count();
        assert_eq!(paras, 1, "`#x` must lazily continue the paragraph, not split it");
        assert_eq!(
            buf.syntax().blocks.iter().filter(|b| matches!(b, Block::Heading(_))).count(),
            0
        );
    }

    /// `para\n---` across a boundary is a setext H2, NOT a paragraph +
    /// thematic break. The merge back-up must fire so the reparse sees both.
    #[test]
    fn lazy_setext_underline_across_boundary_parses_as_heading() {
        let src = b"some title text\n---------------\n\nafter\n";
        let mut buf = open_lazy(src, 16);
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 20 {
            parsed = buf.parse_next_chunk(parsed, 16).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let headings = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Heading(_)))
            .count();
        assert_eq!(headings, 1, "setext underline must produce a heading");
    }

    /// A list item directly after the boundary interrupts the paragraph —
    /// no merge, stays a separate List.
    #[test]
    fn lazy_list_item_after_paragraph_stays_separate() {
        let src = b"para text here\n- item one\n- item two\n";
        let mut buf = open_lazy(src, 15);
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 20 {
            parsed = buf.parse_next_chunk(parsed, 15).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let (paras, lists) = buf.syntax().blocks.iter().fold((0, 0), |(p, l), b| {
            match b {
                Block::Paragraph(_) => (p + 1, l),
                Block::List(_) => (p, l + 1),
                _ => (p, l),
            }
        });
        assert_eq!((paras, lists), (1, 1));
    }

    /// `> a\nlazy` split at the chunk boundary: the lazy line belongs INSIDE
    /// the quote (CommonMark lazy continuation) — one quote block, not a
    /// quote plus a stray paragraph.
    #[test]
    fn lazy_chunk_boundary_quote_lazy_continuation() {
        let src = b"> first\nlazy\n> third\n";
        let mut buf = open_lazy(src, 8);
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 20 {
            parsed = buf.parse_next_chunk(parsed, 8).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let quotes = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::BlockQuote(_)))
            .count();
        assert_eq!(quotes, 1, "lazy continuation must merge into the quote");
    }

    /// `- a\nlazy` split at the boundary stays one list.
    #[test]
    fn lazy_chunk_boundary_list_lazy_continuation() {
        let src = b"- item\nlazy continuation\n- next\n";
        let mut buf = open_lazy(src, 7);
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 20 {
            parsed = buf.parse_next_chunk(parsed, 7).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let lists = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::List(_)))
            .count();
        assert_eq!(lists, 1, "lazy continuation must merge into the list");
        let top_paras = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Paragraph(_)))
            .count();
        assert_eq!(top_paras, 0);
    }

    /// A 4+-indented fence run is NOT a closer (CommonMark: closers allow ≤3
    /// spaces). `    ``` ` at the chunk boundary must keep the fence open —
    /// the next chunk is code content, not fresh Markdown.
    #[test]
    fn lazy_chunk_boundary_indented_fence_not_closer() {
        let src = b"```\ncode\n    ```\nmore\n```\n";
        let mut buf = open_lazy(src, 17); // boundary right after `    ```\n`
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 20 {
            parsed = buf.parse_next_chunk(parsed, 17).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let blocks = &buf.syntax().blocks;
        assert!(
            matches!(blocks.as_slice(), [Block::CodeBlock(_)]),
            "indented fence must not close the block: {} blocks",
            blocks.len()
        );
    }

    /// `> a\n    code` split at the boundary: the indented line is a lazy
    /// paragraph continuation inside the quote (indented code can't
    /// interrupt a paragraph) — one quote block, not quote + code.
    #[test]
    fn lazy_chunk_boundary_quote_indented_lazy_line() {
        let src = b"> a\n    code\n";
        let mut buf = open_lazy(src, 5);
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 20 {
            parsed = buf.parse_next_chunk(parsed, 5).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let blocks = &buf.syntax().blocks;
        assert!(
            matches!(blocks.as_slice(), [Block::BlockQuote(_)]),
            "indented lazy line must stay inside the quote: {} blocks",
            blocks.len()
        );
    }

    /// But a blank line before the boundary line kills laziness:
    /// `> a\n\nplain` across the boundary is quote + paragraph.
    #[test]
    fn lazy_chunk_boundary_blank_kills_laziness() {
        let src = b"> quote\n\nplain\n";
        let mut buf = open_lazy(src, 9);
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 20 {
            parsed = buf.parse_next_chunk(parsed, 9).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let (quotes, paras) = buf.syntax().blocks.iter().fold((0, 0), |(q, p), b| {
            match b {
                Block::BlockQuote(_) => (q + 1, p),
                Block::Paragraph(_) => (q, p + 1),
                _ => (q, p),
            }
        });
        assert_eq!((quotes, paras), (1, 1));
    }

    /// `logical_lines` must treat `\r\n` as ONE terminator (no phantom blank
    /// line between the bytes) and bare `\r` as a terminator too.
    #[test]
    fn logical_lines_handles_all_terminators() {
        let lines: Vec<&[u8]> = logical_lines(b"a\r\nb\r\nc").collect();
        assert_eq!(lines, vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]);
        let lines: Vec<&[u8]> = logical_lines(b"a\rb\rc").collect();
        assert_eq!(lines, vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]);
        let lines: Vec<&[u8]> = logical_lines(b"a\nb\nc").collect();
        assert_eq!(lines, vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]);
        // Mixed families, and a real blank line is still reported.
        let lines: Vec<&[u8]> = logical_lines(b"a\r\n\rb").collect();
        assert_eq!(lines, vec![b"a".as_slice(), b"".as_slice(), b"b".as_slice()]);
    }

    /// A bare-CR (classic Mac) document parsed lazily: the quote spanning the
    /// chunk boundary must stay ONE block and serialize byte-identically —
    /// the continuation heuristics must see logical CR lines, not one giant
    /// "line" per window.
    #[test]
    fn lazy_cr_document_quote_spanning_boundary() {
        let src = b"> line one\r> line two\r> line three\r\rpara\r";
        let mut buf = open_lazy(src, 12);
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 30 {
            parsed = buf.parse_next_chunk(parsed, 12).unwrap();
            it += 1;
        }
        assert_eq!(parsed, buf.total_len());
        assert_eq!(buf.serialize(), src);
        let quotes = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::BlockQuote(_)))
            .count();
        assert_eq!(quotes, 1, "CR quote spanning the boundary must be one block");
    }

    /// Same for a boundary-split paragraph in a bare-CR document.
    #[test]
    fn lazy_cr_document_paragraph_spanning_boundary() {
        let src = b"line one of para\rline two of para\rline three\r\rnext\r";
        let mut buf = open_lazy(src, 17);
        let mut parsed = buf.parsed_offset();
        let mut it = 0;
        while parsed < buf.total_len() && it < 30 {
            parsed = buf.parse_next_chunk(parsed, 17).unwrap();
            it += 1;
        }
        assert_eq!(buf.serialize(), src);
        let paras = buf
            .syntax()
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Paragraph(_)))
            .count();
        assert_eq!(paras, 2, "boundary-split CR paragraph must merge into one");
    }

    fn open_lazy(src: &[u8], chunk: usize) -> DocumentBuffer {
        let original: Arc<dyn ByteSource> = Arc::new(ArcByteSource::new(src.to_vec()));
        let meta = DocumentMeta {
            id: DocumentId::new("test"),
            has_bom: false,
            line_ending: editor_domain::LineEnding::Lf,
            trailing_newline: src.last() == Some(&b'\n'),
            encoding: editor_domain::Encoding::Utf8,
        };
        DocumentBuffer::open_lazy(original, meta, MarkdownProfile::Gfm, chunk).unwrap()
    }

    /// Editing a lazy document must not make `parse_next_chunk` re-read the
    /// *stale original* bytes — new chunks must reflect the current piece table.
    #[test]
    fn lazy_parse_next_chunk_reads_current_content_after_edit() {
        let src = b"# Title\n\npara one\n\npara two UNPARSED\n\ntail text\n";
        let mut buf = open_lazy(src, 24);
        assert!(buf.parsed_offset() < buf.total_len(), "doc must be lazy");

        // Edit entirely inside the unparsed region (past the first chunk).
        let at = src.windows(4).position(|w| w == b"tail").unwrap() as u64;
        assert!(at >= buf.parsed_offset(),
            "edit at {at} must be past the frontier {}", buf.parsed_offset());
        let tx = EditTransaction::single(
            TextEdit::replace(
                editor_domain::ByteRange::new(ByteOffset(at), ByteOffset(at + 4)),
                b"TAIL",
            ),
            Selection::caret(ByteOffset(at)),
            Selection::caret(ByteOffset(at)),
        );
        buf.apply(tx).unwrap();

        // Parsing the next chunk must produce blocks containing the edited
        // text, not the pre-edit original.
        let mut parsed = buf.parsed_offset();
        let total = buf.total_len();
        let mut iterations = 0;
        while parsed < total && iterations < 10 {
            parsed = buf.parse_next_chunk(parsed, 24).unwrap();
            iterations += 1;
        }
        assert_eq!(parsed, total);
        let out = buf.serialize();
        assert!(out.windows(9).any(|w| w == b"TAIL text"),
            "chunk parse must see edited bytes: {}", String::from_utf8_lossy(&out));
        assert!(!out.windows(9).any(|w| w == b"tail text"));
    }

    /// An insert inside the parsed region of a lazy document must shift
    /// `parsed_offset` by the delta, or the next chunk resumes at a stale
    /// offset and produces overlapping/garbage blocks.
    #[test]
    fn lazy_edit_shifts_parsed_offset() {
        let src = b"# Title\n\npara one\n\npara two here\n\ntail\n";
        let mut buf = open_lazy(src, 24);
        let frontier = buf.parsed_offset();
        assert!(frontier < buf.total_len());

        // Insert 10 bytes near the start (inside the parsed region).
        let tx = EditTransaction::single(
            TextEdit::insert(ByteOffset(2), b"0123456789"),
            Selection::caret(ByteOffset(2)),
            Selection::caret(ByteOffset(12)),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.parsed_offset(), frontier + 10,
            "parsed_offset must shift with the edit delta");

        // Chunks parsed afterwards must still be byte-identical.
        let mut parsed = buf.parsed_offset();
        let total = buf.total_len();
        let mut iterations = 0;
        while parsed < total && iterations < 10 {
            parsed = buf.parse_next_chunk(parsed, 24).unwrap();
            iterations += 1;
        }
        assert_eq!(parsed, total);
        let expected = b"# 0123456789Title\n\npara one\n\npara two here\n\ntail\n";
        assert_eq!(buf.serialize(), expected);
    }

    /// An insert entirely beyond the parsed frontier must not trigger a full
    /// reparse (which would materialize a >RAM document) and must leave the
    /// parsed tree untouched.
    #[test]
    fn lazy_edit_beyond_frontier_does_not_reparse() {
        let src = b"# Title\n\npara one\n\npara two here\n\ntail\n";
        let mut buf = open_lazy(src, 24);
        let frontier = buf.parsed_offset();
        assert!(frontier < buf.total_len());
        let block_count = buf.syntax().blocks.len();

        // Append at EOF — beyond the frontier.
        let end = buf.total_len();
        let tx = EditTransaction::single(
            TextEdit::insert(ByteOffset(end), b"appended\n"),
            Selection::caret(ByteOffset(end)),
            Selection::caret(ByteOffset(end + 8)),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.parsed_offset(), frontier,
            "frontier must not move for edits in the unparsed region");
        assert_eq!(buf.syntax().blocks.len(), block_count);
        // Chunk parsing then picks the appended text up naturally.
        let mut parsed = buf.parsed_offset();
        let total = buf.total_len();
        let mut iterations = 0;
        while parsed < total && iterations < 10 {
            parsed = buf.parse_next_chunk(parsed, 24).unwrap();
            iterations += 1;
        }
        assert_eq!(parsed, total);
        let out = buf.serialize();
        assert!(out.ends_with(b"tail\nappended\n"));
    }

    /// Insert exactly on a block boundary must still reparse a block (inclusive
    /// overlap), not silently skip the incremental path onto stale blocks.
    #[test]
    fn insert_at_block_boundary_reparses() {
        let src = b"Para one.\n\nPara two.\n";
        let mut buf = open(src);
        // Offset 11 is the start of the "Para two." block — an exact block boundary.
        let boundary = 11u64;
        let tx = EditTransaction::single(
            TextEdit::insert(ByteOffset(boundary), b"INS"),
            Selection::caret(ByteOffset(boundary)),
            Selection::caret(ByteOffset(boundary + 3)),
        );
        buf.apply(tx).unwrap();
        let out = buf.serialize();
        assert_eq!(out, b"Para one.\n\nINSPara two.\n");
        // All spans must be valid: every block's source slice must match.
        for b in buf.syntax().blocks.iter() {
            let s = b.meta().span;
            assert!(s.end.0 as usize <= out.len(), "span out of bounds: {s:?}");
        }
    }

    /// A multi-edit transaction applied in cumulative coordinates must undo to
    /// the exact original bytes — each edit's `removed` was captured from the
    /// document state it actually modified.
    #[test]
    fn multi_edit_transaction_undo_restores_original() {
        let src = b"aaa bbb ccc\n";
        let mut buf = open(src);
        // Two edits in cumulative coordinates: replace "aaa" -> "AA", then in
        // the resulting document replace "ccc" -> "CCCC" (its range shifted by -1).
        let edits = vec![
            TextEdit::replace(ByteRange::new(ByteOffset(0), ByteOffset(3)), b"AA"),
            TextEdit::replace(ByteRange::new(ByteOffset(7), ByteOffset(10)), b"CCCC"),
        ];
        let tx = EditTransaction::new(
            edits,
            Selection::caret(ByteOffset::ZERO),
            Selection::caret(ByteOffset::ZERO),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.serialize(), b"AA bbb CCCC\n");
        buf.undo().unwrap();
        assert_eq!(buf.serialize(), src, "undo must restore original bytes");
        buf.redo().unwrap();
        assert_eq!(buf.serialize(), b"AA bbb CCCC\n", "redo must reapply both edits");
    }

    /// A multi-edit transaction entirely beyond the parsed frontier must keep
    /// the lazy property through apply/undo/redo — a full `to_bytes()` +
    /// reparse on undo or redo would materialize a >RAM document.
    #[test]
    fn lazy_multi_edit_beyond_frontier_undo_redo_stay_lazy() {
        let src = b"# Title\n\npara one\n\npara two UNPARSED\n\ntail foo bar\n";
        let mut buf = open_lazy(src, 24);
        let frontier = buf.parsed_offset();
        assert!(frontier < buf.total_len(), "doc must be lazy");

        // Two replacements wholly inside the unparsed tail (last-first order,
        // the replace-all convention — ranges in the same coordinate frame).
        let tail = src.windows(4).position(|w| w == b"tail").unwrap() as u64;
        assert!(tail >= frontier);
        let tx = EditTransaction::new(
            vec![
                TextEdit::replace(
                    editor_domain::ByteRange::new(ByteOffset(tail + 9), ByteOffset(tail + 12)),
                    b"BAR",
                ),
                TextEdit::replace(
                    editor_domain::ByteRange::new(ByteOffset(tail + 5), ByteOffset(tail + 8)),
                    b"FOO",
                ),
            ],
            Selection::caret(ByteOffset(tail)),
            Selection::caret(ByteOffset(tail)),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.parsed_offset(), frontier, "parsed tree must be untouched");
        assert!(buf.serialize().ends_with(b"tail FOO BAR\n"));

        buf.undo().unwrap();
        assert_eq!(buf.parsed_offset(), frontier, "undo must stay lazy");
        assert!(buf.serialize().ends_with(b"tail foo bar\n"));

        buf.redo().unwrap();
        assert_eq!(buf.parsed_offset(), frontier, "redo must stay lazy");
        assert!(buf.serialize().ends_with(b"tail FOO BAR\n"));

        // Chunk parsing picks the redone bytes up naturally.
        let mut parsed = buf.parsed_offset();
        let total = buf.total_len();
        while parsed < total {
            parsed = buf.parse_next_chunk(parsed, 24).unwrap();
        }
        assert_eq!(parsed, total);
        assert!(buf.serialize().ends_with(b"tail FOO BAR\n"));
    }

    /// `windows(0)` would panic on an empty needle — the API must reject it.
    #[test]
    fn replace_text_run_empty_needle_is_rejected() {
        let mut buf = open(b"hello world\n");
        let span = SourceSpan::new(ByteOffset(0), ByteOffset(12));
        let res = buf.replace_text_run(span, "", "x");
        assert!(res.is_err(), "empty needle must error, not panic");
    }

    /// A span captured before later edits can exceed the current document —
    /// the API must reject it instead of panicking on the slice.
    #[test]
    fn replace_text_run_stale_span_is_rejected() {
        let mut buf = open(b"short\n");
        let stale = SourceSpan::new(ByteOffset(0), ByteOffset(10_000));
        assert!(buf.replace_text_run(stale, "x", "y").is_err());
        // Inverted span also rejected.
        let inverted = SourceSpan::new(ByteOffset(4), ByteOffset(1));
        assert!(buf.replace_text_run(inverted, "x", "y").is_err());
    }

    /// `parse_next_chunk` must saturate instead of wrapping: a huge offset or
    /// chunk size returns the document end immediately (no infinite caller loop).
    #[test]
    fn parse_next_chunk_saturates_on_huge_offsets() {
        let mut buf = open(b"tiny\n");
        let total = buf.total_len();
        assert_eq!(buf.parse_next_chunk(u64::MAX - 1, usize::MAX).unwrap(), total);
        assert_eq!(buf.parse_next_chunk(0, usize::MAX).unwrap(), total);
    }

    /// Lazy document: a single edit inside the already-parsed region must keep
    /// the parsed prefix correct AND let `parse_next_chunk` continue cleanly —
    /// the syntax tree must still serialize to the edited bytes end-to-end.
    #[test]
    fn lazy_edit_before_frontier_then_parse_rest() {
        // Build a document bigger than the lazy chunk.
        let mut src = Vec::new();
        src.extend_from_slice(b"# Head\n\n");
        for i in 0..40 {
            src.extend_from_slice(format!("paragraph line {i} with some text\n").as_bytes());
        }
        let original: Arc<dyn ByteSource> = Arc::new(ArcByteSource::new(src.clone()));
        let meta = DocumentMeta {
            id: DocumentId::new("lazy"),
            has_bom: false,
            line_ending: editor_domain::LineEnding::Lf,
            trailing_newline: true,
            encoding: editor_domain::Encoding::Utf8,
        };
        let mut buf = DocumentBuffer::open_lazy(original, meta, MarkdownProfile::Gfm, 64).unwrap();
        assert!(buf.parsed_offset() < buf.total_len(), "must start partially parsed");

        // Edit inside the parsed region (heading text).
        let pos = buf.parsed_offset() - 2;
        let tx = EditTransaction::single(
            TextEdit::insert(ByteOffset(pos), b"X"),
            Selection::caret(ByteOffset(pos)),
            Selection::caret(ByteOffset(pos + 1)),
        );
        buf.apply(tx).unwrap();

        // Parse to the end; the document must serialize to the edited source.
        let mut parsed = buf.parsed_offset();
        let total = buf.total_len();
        let mut iters = 0;
        while parsed < total && iters < 200 {
            parsed = buf.parse_next_chunk(parsed, 64).unwrap();
            iters += 1;
        }
        assert_eq!(parsed, total);
        let mut expected = src.clone();
        expected.insert(pos as usize, b'X');
        assert_eq!(buf.serialize(), expected);
    }

    /// Undo on a lazy document uses the inverse-edit path — the frontier and
    /// bytes must both be restored.
    #[test]
    fn lazy_undo_restores_frontier_and_bytes() {
        let src = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
        let original: Arc<dyn ByteSource> = Arc::new(ArcByteSource::new(src.to_vec()));
        let meta = DocumentMeta {
            id: DocumentId::new("lazy2"),
            has_bom: false,
            line_ending: editor_domain::LineEnding::Lf,
            trailing_newline: true,
            encoding: editor_domain::Encoding::Utf8,
        };
        let mut buf = DocumentBuffer::open_lazy(original, meta, MarkdownProfile::Gfm, 16).unwrap();
        let frontier_before = buf.parsed_offset();
        let tx = EditTransaction::single(
            TextEdit::insert(ByteOffset(0), b"zero\n"),
            Selection::caret(ByteOffset::ZERO),
            Selection::caret(ByteOffset(5)),
        );
        buf.apply(tx).unwrap();
        buf.undo().unwrap();
        assert_eq!(buf.parsed_offset(), frontier_before);
        // Serialize emits the parsed blocks + the unparsed tail verbatim —
        // a lazy document must round-trip fully, not truncate at the frontier.
        assert_eq!(buf.serialize(), src.as_slice());
    }

    /// A lazy document must serialize to the full byte content — the unparsed
    /// tail is emitted verbatim, never dropped.
    #[test]
    fn lazy_serialize_includes_unparsed_tail() {
        let src = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
        let original: Arc<dyn ByteSource> = Arc::new(ArcByteSource::new(src.to_vec()));
        let meta = DocumentMeta {
            id: DocumentId::new("lazy3"),
            has_bom: false,
            line_ending: editor_domain::LineEnding::Lf,
            trailing_newline: true,
            encoding: editor_domain::Encoding::Utf8,
        };
        let buf = DocumentBuffer::open_lazy(original, meta, MarkdownProfile::Gfm, 16).unwrap();
        assert!(buf.parsed_offset() < buf.total_len());
        assert_eq!(buf.serialize(), src.as_slice());
    }

    /// Deleting a fence CLOSER extends the code block over following blocks.
    /// The incremental reparse window must grow until it resynchronizes —
    /// otherwise the code block is truncated at the old boundary and the
    /// swallowed paragraph keeps a stale parse.
    #[test]
    fn incremental_reparse_extends_window_past_boundary() {
        let src = b"```\ncode\n```\n\npara\n";
        let mut buf = open(src);
        // Remove the "```\n" closer (bytes 9..13).
        let tx = EditTransaction::single(
            TextEdit::replace(
                ByteRange::new(ByteOffset(9), ByteOffset(13)),
                b"",
            ),
            Selection::caret(ByteOffset(9)),
            Selection::caret(ByteOffset(9)),
        );
        buf.apply(tx).unwrap();
        // Full-parse equivalence is the oracle: "```\ncode\n\npara\n" is one
        // unterminated fenced code block swallowing the paragraph.
        let oracle = editor_markdown::parse_with(b"```\ncode\n\npara\n", MarkdownProfile::Gfm).unwrap();
        let blocks = &buf.syntax().blocks;
        assert_eq!(blocks.len(), oracle.blocks.len(), "block count must match full reparse");
        for (a, b) in blocks.iter().zip(oracle.blocks.iter()) {
            assert_eq!(a.meta().span, b.meta().span, "span mismatch vs full reparse");
        }
        assert_eq!(buf.serialize(), b"```\ncode\n\npara\n");
    }

    /// Deleting the blank line between two paragraphs merges them — the
    /// reparse window must absorb the following block.
    #[test]
    fn incremental_reparse_merges_paragraphs() {
        let src = b"aaa\n\nbbb\n";
        let mut buf = open(src);
        // Remove the "\n" separator (byte 4) → "aaa\nbbb\n" is ONE paragraph.
        let tx = EditTransaction::single(
            TextEdit::replace(ByteRange::new(ByteOffset(4), ByteOffset(5)), b""),
            Selection::caret(ByteOffset(4)),
            Selection::caret(ByteOffset(4)),
        );
        buf.apply(tx).unwrap();
        let oracle = editor_markdown::parse_with(b"aaa\nbbb\n", MarkdownProfile::Gfm).unwrap();
        let blocks = &buf.syntax().blocks;
        assert_eq!(blocks.len(), oracle.blocks.len());
        for (a, b) in blocks.iter().zip(oracle.blocks.iter()) {
            assert_eq!(a.meta().span, b.meta().span);
        }
        assert_eq!(buf.serialize(), b"aaa\nbbb\n");
    }

    /// Inserting a blank line inside a paragraph SPLITS it into two — the
    /// forward blocks must shift by the delta even though the edit touched
    /// only one block.
    #[test]
    fn incremental_reparse_splits_paragraph() {
        let src = b"aaabbb\n\nrest\n";
        let mut buf = open(src);
        // Insert "\n\n" after "aaa" (byte 3) → two paragraphs + "rest".
        let tx = EditTransaction::single(
            TextEdit::insert(ByteOffset(3), b"\n\n"),
            Selection::caret(ByteOffset(3)),
            Selection::caret(ByteOffset(5)),
        );
        buf.apply(tx).unwrap();
        let oracle = editor_markdown::parse_with(b"aaa\n\nbbb\n\nrest\n", MarkdownProfile::Gfm).unwrap();
        let blocks = &buf.syntax().blocks;
        assert_eq!(blocks.len(), oracle.blocks.len());
        for (a, b) in blocks.iter().zip(oracle.blocks.iter()) {
            assert_eq!(a.meta().span, b.meta().span);
        }
        assert_eq!(buf.serialize(), b"aaa\n\nbbb\n\nrest\n");
    }

    /// An out-of-range edit must be rejected — not clamped into a silent
    /// append at EOF (PieceTable::apply_edit clamps; the buffer must not).
    #[test]
    fn apply_rejects_out_of_range_edit() {
        let mut buf = open(b"short\n");
        let tx = EditTransaction::single(
            TextEdit::insert(ByteOffset(10_000), b"X"),
            Selection::caret(ByteOffset::ZERO),
            Selection::caret(ByteOffset::ZERO),
        );
        assert!(buf.apply(tx).is_err());
        assert_eq!(buf.serialize(), b"short\n");
        assert!(!buf.is_dirty(), "a rejected edit must not dirty the buffer");
    }

    /// A cut that splits a multi-byte UTF-8 char must be rejected.
    #[test]
    fn apply_rejects_mid_char_cut() {
        // 'é' = 0xC3 0xA9 — cutting at byte 1 splits it.
        let mut buf = open("aéb".as_bytes());
        let tx = EditTransaction::single(
            TextEdit::replace(ByteRange::new(ByteOffset(1), ByteOffset(2)), b"X"),
            Selection::caret(ByteOffset::ZERO),
            Selection::caret(ByteOffset::ZERO),
        );
        assert!(buf.apply(tx).is_err());
        assert_eq!(buf.serialize(), "aéb".as_bytes());
        // A cut at the real boundary (0..1 = "a") works.
        let tx = EditTransaction::single(
            TextEdit::replace(ByteRange::new(ByteOffset(0), ByteOffset(1)), b"X"),
            Selection::caret(ByteOffset::ZERO),
            Selection::caret(ByteOffset::ZERO),
        );
        buf.apply(tx).unwrap();
        assert_eq!(buf.serialize(), "Xéb".as_bytes());
    }

    /// A multi-edit transaction that fails partway must roll back the edits
    /// already applied — no half-applied, un-undoable state.
    #[test]
    fn apply_rolls_back_partially_invalid_transaction() {
        let mut buf = open(b"abc\n");
        let good = TextEdit::insert(ByteOffset(0), b"X");
        let bad = TextEdit::insert(ByteOffset(999), b"Y");
        let tx = EditTransaction::new(
            vec![good, bad],
            Selection::caret(ByteOffset::ZERO),
            Selection::caret(ByteOffset::ZERO),
        );
        assert!(buf.apply(tx).is_err());
        // The first edit was valid but must have been rolled back.
        assert_eq!(buf.serialize(), b"abc\n");
    }
}
