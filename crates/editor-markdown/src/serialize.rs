//! Serializer (ADR-003).
//!
//! Default path: emit `source[span]` for each block in document order. Because the parser
//! partitions `[0, len)` into contiguous block spans, this is byte-identical to the source
//! for an unedited document (Invariant 1). For nodes marked `dirty` (edited), the
//! serializer regenerates that node's text from its (modified) content + preserved trivia,
//! so only the edited node's bytes change (Invariant 2).

use editor_domain::ByteOffset;

use crate::ast::*;

/// Serialize a document back to bytes using `source` as the verbatim backing store.
///
/// For lazily-parsed documents (`parsed_offset < source.len()`) the blocks only
/// cover the parsed prefix; the unparsed tail has no dirty nodes by definition,
/// so it is emitted verbatim from `source` — serializing must never truncate.
pub fn serialize(doc: &Document, source: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(source.len());
    for block in &doc.blocks {
        serialize_block(block, source, &mut out);
    }
    // Emit the unparsed tail verbatim: `doc.span.end` tracks the end of the
    // last parsed block (== source.len() for fully-parsed documents).
    let tail_start = (doc.span.end.0 as usize).min(source.len());
    out.extend_from_slice(&source[tail_start..]);
    out
}

fn serialize_block(block: &Block, source: &[u8], out: &mut Vec<u8>) {
    let meta = block.meta();
    if meta.dirty {
        regenerate_block(block, source, out);
    } else {
        emit_span(meta.span, source, out);
    }
}

fn emit_span(span: SourceSpan, source: &[u8], out: &mut Vec<u8>) {
    // Clamp rather than slice raw: a span made stale by rebase_spans_after
    // (a large negative delta can push `end` below `start`, or both past
    // the buffer) must degrade to emitting less, never panic the caller.
    let st = (span.start.0 as usize).min(source.len());
    let e = (span.end.0 as usize).min(source.len()).max(st);
    out.extend_from_slice(&source[st..e]);
}

/// Re-emit the line ending that terminated the original span (`\n`, `\r\n`, or
/// nothing at EOF without a trailing newline). Block spans include their line
/// ending, so regenerated blocks must emit one too — otherwise the following
/// block's text is glued onto the same line.
fn reemit_line_ending(span: SourceSpan, source: &[u8], out: &mut Vec<u8>) {
    let end = (span.end.0 as usize).min(source.len());
    if end >= 2 && &source[end - 2..end] == b"\r\n" {
        out.extend_from_slice(b"\r\n");
    } else if end >= 1 && source[end - 1] == b'\n' {
        out.push(b'\n');
    }
}

/// Regenerate a dirty block from its (modified) content + preserved trivia.
///
/// `source` is the buffer this block's spans index into — for de-marked
/// children (block quote / list item content) that is the marker-stripped
/// buffer, NOT the original document.
fn regenerate_block(block: &Block, source: &[u8], out: &mut Vec<u8>) {
    match block {
        Block::Paragraph(p) => {
            out.extend_from_slice(serialize_inlines_src(&p.inlines, source).as_bytes());
            reemit_line_ending(p.meta.span, source, out);
        }
        Block::Heading(h) => {
            if h.style == HeadingStyle::Atx {
                for _ in 0..h.atx_open_hashes {
                    out.push(b'#');
                }
                out.push(b' ');
                out.extend_from_slice(serialize_inlines_src(&h.inlines, source).as_bytes());
                if h.atx_close_hashes > 0 {
                    out.push(b' ');
                    for _ in 0..h.atx_close_hashes {
                        out.push(b'#');
                    }
                }
                reemit_line_ending(h.meta.span, source, out);
            } else {
                // Setext: text line + underline of '=' (h1) or '-' (h2).
                out.extend_from_slice(serialize_inlines_src(&h.inlines, source).as_bytes());
                out.push(b'\n');
                let underline = if h.level == 1 { b'=' } else { b'-' };
                let ulen = if h.setext_underline_len > 0 {
                    h.setext_underline_len
                } else {
                    3
                };
                for _ in 0..ulen {
                    out.push(underline);
                }
                reemit_line_ending(h.meta.span, source, out);
            }
        }
        Block::List(l) => {
            regenerate_list(l, source, out);
        }
        Block::BlockQuote(bq) => {
            regenerate_block_quote(bq, source, out);
        }
        Block::CodeBlock(_)
        | Block::Table(_)
        | Block::HtmlBlock(_)
        | Block::LinkReferenceDefinition(_)
        | Block::UnknownBlock(_)
        | Block::ThematicBreak(_)
        | Block::BlankLine(_) => {
            // Fallback: emit verbatim. Regeneration for these node types is a follow-up;
            // edits to them currently go through the PieceTable byte-level path which is
            // already minimal-diff.
            emit_span(block.meta().span, source, out);
        }
    }
}

/// Split `raw` into lines including their terminators, treating `\n`, `\r\n`
/// and a bare `\r` all as line endings (the parser's `collect_lines`
/// semantics). `split_inclusive('\n')` would see a bare-CR document as one
/// line and de-mark only its first marker.
fn split_lines(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        if pos >= raw.len() {
            return None;
        }
        let end = match raw[pos..].iter().position(|&b| b == b'\n' || b == b'\r') {
            Some(i) => {
                let mut e = pos + i + 1;
                if raw[pos + i] == b'\r' && raw.get(pos + i + 1) == Some(&b'\n') {
                    e += 1;
                }
                e
            }
            None => raw.len(),
        };
        let line = &raw[pos..end];
        pos = end;
        Some(line)
    })
}

/// `str::lines()` splits on `\n` and `\r\n` but not bare `\r` — same fix as
/// `split_lines`, for already-decoded text.
fn iter_lines(text: &str) -> impl Iterator<Item = &str> {
    let bytes = text.as_bytes();
    let mut pos = 0;
    std::iter::from_fn(move || {
        if pos >= text.len() {
            return None;
        }
        let rel = bytes[pos..].iter().position(|&b| b == b'\n' || b == b'\r');
        let (line_end, next) = match rel {
            Some(i) => {
                let mut n = pos + i + 1;
                if bytes[pos + i] == b'\r' && bytes.get(pos + i + 1) == Some(&b'\n') {
                    n += 1;
                }
                (pos + i, n)
            }
            None => (text.len(), text.len()),
        };
        let line = &text[pos..line_end];
        pos = next;
        Some(line)
    })
}

/// Split a raw line (as yielded by `split_lines`) into content and
/// line ending, mirroring the parser's `content_end` semantics — a trailing
/// `\r` belongs to the ending.
fn split_line_ending(line: &[u8]) -> (&[u8], &[u8]) {
    if line.ends_with(b"\r\n") {
        line.split_at(line.len() - 2)
    } else if line.ends_with(b"\n") || line.ends_with(b"\r") {
        line.split_at(line.len() - 1)
    } else {
        (line, &[])
    }
}

/// Rebuild the marker-stripped buffer the parser produced for a block quote's
/// children: each line loses its leading spaces, the `>` marker, and one
/// optional space after it. Child block spans index into THIS buffer —
/// serializing clean children against the document would read wrong bytes.
fn de_mark_quote_source(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for line in split_lines(raw) {
        let (content, ending) = split_line_ending(line);
        let ind = content.iter().take_while(|&&b| b == b' ').count();
        let s = &content[ind.min(content.len())..];
        // Mirror the parser's de-marked build: `>`-marked lines lose marker +
        // one optional space; lazy continuation lines keep their indent —
        // it is content (`> a\n    ---` is paragraph text inside the quote).
        let s = if s.first() == Some(&b'>') {
            let after = &s[1..];
            if after.first() == Some(&b' ') {
                &after[1..]
            } else {
                after
            }
        } else {
            content
        };
        out.extend_from_slice(s);
        out.extend_from_slice(ending);
    }
    out
}

/// Rebuild the de-marked buffer the parser produced for a list item's
/// children: the first line loses indent + marker + whitespace after it plus
/// the task checkbox (`[ ] `/`[x] `, re-emitted from `item.task`);
/// continuation lines lose `indent + content_indent` bytes.
fn de_mark_item_source(raw: &[u8], ordered: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut strip = 0usize;
    for (k, line) in split_lines(raw).enumerate() {
        let (content, ending) = split_line_ending(line);
        if k == 0 {
            let ind = content.iter().take_while(|&&b| b == b' ').count();
            let s = &content[ind.min(content.len())..];
            let marker_len = if ordered {
                s.iter().take_while(|b| b.is_ascii_digit()).count() + 1
            } else {
                1
            };
            let after = &s[marker_len.min(s.len())..];
            let ws = after
                .iter()
                .take_while(|&&b| b == b' ' || b == b'\t')
                .count();
            strip = ind + marker_len + ws;
            let rest = &content[strip.min(content.len())..];
            if rest.starts_with(b"[ ] ") || rest.starts_with(b"[x] ") || rest.starts_with(b"[X] ") {
                strip += 4;
            }
        }
        // Continuation lines strip `strip`, but never more than the line's own
        // indent — lazy continuation lines (`- a\nlazy`) carry less indent and
        // must be kept verbatim, mirroring the parser's de-marked build.
        let n = if k == 0 {
            strip.min(content.len())
        } else {
            strip
                .min(content.iter().take_while(|&&b| b == b' ').count())
                .min(content.len())
        };
        out.extend_from_slice(&content[n..]);
        out.extend_from_slice(ending);
    }
    out
}

/// The de-marked child buffer for `item`, rebuilt from its span in `source`.
fn item_demarked_source(item: &ListItem, list: &List, source: &[u8]) -> Vec<u8> {
    let st = item.meta.span.start.0 as usize;
    let e = (item.meta.span.end.0 as usize).min(source.len());
    let raw = if st <= e { &source[st..e] } else { &[][..] };
    de_mark_item_source(raw, list.ordered)
}

/// Regenerate a dirty list block from its children + preserved structure (Invariant 2).
/// Each item's children are serialized recursively; the marker and task checkbox are
/// re-emitted from the item's metadata.
fn regenerate_list(list: &List, source: &[u8], out: &mut Vec<u8>) {
    for (i, item) in list.items.iter().enumerate() {
        // Marker: ordered lists emit `N.` or `N)`; unordered emit `-`/`*`/`+`.
        if list.ordered {
            let n = list.start + i as u32;
            out.extend_from_slice(n.to_string().as_bytes());
        }
        // `marker` preserves the original delimiter — `.` vs `)` for ordered
        // lists; hard-coding `.` would corrupt `1)`-style lists on regenerate.
        out.push(list.marker);
        out.push(b' ');
        // Task list checkbox.
        if let Some(task) = &item.task {
            match task {
                TaskState::Open => out.extend_from_slice(b"[ ] "),
                TaskState::Done => out.extend_from_slice(b"[x] "),
            }
        }
        // Children's spans index into the item's marker-stripped buffer, not
        // the document — rebuild it so clean children emit their own bytes.
        let de = item_demarked_source(item, list, source);
        // Children: the first child's first line goes on the marker line (no indent).
        // Subsequent lines and blocks are indented by 2 spaces.
        let mut first_child = true;
        for child in &item.children {
            if first_child {
                // Serialize first child without leading indent; its first line follows
                // the marker on the same line.
                let mut child_buf = Vec::new();
                serialize_block(child, &de, &mut child_buf);
                let text = String::from_utf8_lossy(&child_buf);
                let mut lines = iter_lines(&text);
                if let Some(first_line) = lines.next() {
                    out.extend_from_slice(first_line.as_bytes());
                    out.push(b'\n');
                }
                for line in lines {
                    // Blank lines carry no indent — `"  \n"` would introduce
                    // trailing whitespace the source never had.
                    if !line.is_empty() {
                        out.extend_from_slice(b"  ");
                    }
                    out.extend_from_slice(line.as_bytes());
                    out.push(b'\n');
                }
                first_child = false;
            } else {
                // Subsequent children: indent all lines by 2 spaces.
                let mut child_buf = Vec::new();
                serialize_block(child, &de, &mut child_buf);
                let text = String::from_utf8_lossy(&child_buf);
                for line in iter_lines(&text) {
                    if !line.is_empty() {
                        out.extend_from_slice(b"  ");
                    }
                    out.extend_from_slice(line.as_bytes());
                    out.push(b'\n');
                }
            }
        }
        if item.children.is_empty() {
            // Empty item: just the marker + newline.
            out.push(b'\n');
        }
    }
}

/// Regenerate a dirty block quote from its children (Invariant 2).
/// Each child block is serialized and prefixed with `> ` on each line.
fn regenerate_block_quote(bq: &BlockQuote, source: &[u8], out: &mut Vec<u8>) {
    // Children's spans index into the marker-stripped buffer, not `source`.
    let st = bq.meta.span.start.0 as usize;
    let e = (bq.meta.span.end.0 as usize).min(source.len());
    let raw = if st <= e { &source[st..e] } else { &[][..] };
    let de = de_mark_quote_source(raw);
    for child in &bq.children {
        let mut child_buf = Vec::new();
        serialize_block_with_indent(child, &de, &mut child_buf, 0);
        // Prefix each line with `> `.
        let text = String::from_utf8_lossy(&child_buf);
        for line in iter_lines(&text) {
            out.extend_from_slice(b"> ");
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
    }
}

/// Serialize a block with a leading indent on each line (for nested list content).
fn serialize_block_with_indent(block: &Block, source: &[u8], out: &mut Vec<u8>, indent: usize) {
    let meta = block.meta();
    if meta.dirty {
        // Recursively regenerate; for simplicity, collect into a temp buffer and indent.
        let mut tmp = Vec::new();
        regenerate_block(block, source, &mut tmp);
        let text = String::from_utf8_lossy(&tmp);
        let lines: Vec<&str> = iter_lines(&text).collect();
        let last_idx = lines.len().saturating_sub(1);
        for (i, line) in lines.iter().enumerate() {
            for _ in 0..indent {
                out.push(b' ');
            }
            out.extend_from_slice(line.as_bytes());
            // Add newline after each line except the last (unless the original ended with one).
            if i < last_idx {
                out.push(b'\n');
            } else if i == last_idx && matches!(tmp.last(), Some(&b'\n') | Some(&b'\r')) {
                out.push(b'\n');
            }
        }
    } else {
        // Emit verbatim, applying indent to each line. `get` instead of a
        // bare slice: a stale span (e.g. shifted past the buffer end by an
        // incremental-reparse splice) must not panic the serializer.
        let span = meta.span;
        let bytes = source
            .get(span.start.0 as usize..span.end.0 as usize)
            .unwrap_or(&[]);
        let text = String::from_utf8_lossy(bytes);
        for line in iter_lines(&text) {
            for _ in 0..indent {
                out.push(b' ');
            }
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
    }
}

/// Serialize inline nodes back to Markdown text.
pub fn serialize_inlines(inlines: &[Inline]) -> String {
    serialize_inlines_src(inlines, &[])
}

/// Like `serialize_inlines` but allows verbatim `source[span]` fallback for
/// node kinds that cannot be regenerated from their stored content
/// (`RawHtml`, `UnknownInline`). `source` must be the buffer the inline
/// spans index into — the de-marked buffer for quote/list children.
fn serialize_inlines_src(inlines: &[Inline], source: &[u8]) -> String {
    let mut s = String::new();
    for il in inlines {
        serialize_inline(il, source, &mut s);
    }
    s
}

/// Emit an inline node's source span verbatim when it is in bounds.
fn emit_inline_span(meta: &NodeMeta, source: &[u8], s: &mut String) -> bool {
    let st = meta.span.start.0 as usize;
    let e = meta.span.end.0 as usize;
    if st <= e && e <= source.len() {
        s.push_str(&String::from_utf8_lossy(&source[st..e]));
        true
    } else {
        false
    }
}

fn serialize_inline(il: &Inline, source: &[u8], s: &mut String) {
    match il {
        Inline::Text(_, t) => s.push_str(t),
        Inline::Emphasis(_, children, kind) => {
            let d = if *kind == EmphasisKind::Asterisk {
                "*"
            } else {
                "_"
            };
            s.push_str(d);
            s.push_str(&serialize_inlines_src(children, source));
            s.push_str(d);
        }
        Inline::Strong(_, children, kind) => {
            let d = if *kind == EmphasisKind::Asterisk {
                "**"
            } else {
                "__"
            };
            s.push_str(d);
            s.push_str(&serialize_inlines_src(children, source));
            s.push_str(d);
        }
        Inline::Strikethrough(_, children) => {
            s.push_str("~~");
            s.push_str(&serialize_inlines_src(children, source));
            s.push_str("~~");
        }
        Inline::CodeSpan(_, t, n) => {
            let fence: String = std::iter::repeat('`').take(*n as usize).collect();
            s.push_str(&fence);
            s.push_str(t);
            s.push_str(&fence);
        }
        Inline::MathSpan(_, t, display) => {
            if *display {
                s.push_str("\\[");
                s.push_str(t);
                s.push_str("\\]");
            } else {
                s.push_str("\\(");
                s.push_str(t);
                s.push_str("\\)");
            }
        }
        Inline::Link(l) => match l.style {
            LinkStyle::Inline => {
                s.push('[');
                s.push_str(&serialize_inlines_src(&l.inlines, source));
                s.push_str("](");
                s.push_str(&l.destination);
                if let Some(title) = &l.title {
                    s.push_str(" \"");
                    s.push_str(title);
                    s.push('"');
                }
                s.push(')');
            }
            LinkStyle::Reference => {
                s.push('[');
                s.push_str(&serialize_inlines_src(&l.inlines, source));
                s.push_str("][");
                s.push_str(l.reference.as_deref().unwrap_or(""));
                s.push(']');
            }
            LinkStyle::Collapsed => {
                s.push('[');
                s.push_str(&serialize_inlines_src(&l.inlines, source));
                s.push_str("][]");
            }
            LinkStyle::Shortcut => {
                s.push('[');
                s.push_str(&serialize_inlines_src(&l.inlines, source));
                s.push(']');
            }
        },
        Inline::Image(im) => {
            s.push_str("![");
            s.push_str(&im.alt);
            s.push_str("](");
            s.push_str(&im.destination);
            if let Some(title) = &im.title {
                s.push_str(" \"");
                s.push_str(title);
                s.push('"');
            }
            s.push(')');
        }
        Inline::Autolink(_, inner) => {
            s.push('<');
            s.push_str(inner);
            s.push('>');
        }
        Inline::HardBreak(_) => {
            s.push_str("  \n");
        }
        Inline::RawHtml(m) => {
            // These nodes have no stored content — emit their source span
            // verbatim when `source` is available, never placeholder text that
            // would corrupt the regenerated block.
            if !emit_inline_span(m, source, s) {
                s.push_str(&format!("[raw html @{}..{}]", m.span.start.0, m.span.end.0));
            }
        }
        Inline::UnknownInline(m) => {
            if !emit_inline_span(m, source, s) {
                s.push_str(&format!("[unknown @{}..{}]", m.span.start.0, m.span.end.0));
            }
        }
    }
}

/// Convenience: parse then serialize and assert byte-identical round-trip (Invariant 1).
#[cfg(test)]
pub fn roundtrip(source: &[u8], profile: editor_domain::MarkdownProfile) -> Vec<u8> {
    let doc = crate::parse(source, profile).expect("parse failed");
    serialize(&doc, source)
}

/// Toggle a task list item's checkbox in the source bytes and return the new source.
/// Changes only `[ ]`<->`[x]` within the item's span (§7, Invariant 2).
pub fn toggle_task_item(source: &[u8], item: &ListItem) -> Option<Vec<u8>> {
    let span = item.meta.span;
    let (st, e) = (span.start.0 as usize, span.end.0 as usize);
    if st > e || e > source.len() {
        return None; // stale span (item captured before a later edit)
    }
    let region = &source[st..e];
    // The checkbox sits on the item's first line, directly after the list
    // marker — searching the whole span would hit a literal `[ ]` inside
    // code spans or text of a non-task item.
    let first_line_end = region
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(region.len());
    let first_line = &region[..first_line_end];
    let marker_pos = first_line
        .windows(3)
        .position(|w| w == b"[ ]" || w == b"[x]" || w == b"[X]")
        .filter(|&pos| {
            // Everything before `[` must be indent + list marker + whitespace.
            let prefix = &first_line[..pos];
            let prefix = &prefix[prefix.iter().take_while(|&&b| b == b' ').count()..];
            let after_marker = if prefix.first() == Some(&b'-')
                || prefix.first() == Some(&b'*')
                || prefix.first() == Some(&b'+')
            {
                &prefix[1..]
            } else {
                let digits = prefix.iter().take_while(|b| b.is_ascii_digit()).count();
                if digits > 0
                    && (prefix.get(digits) == Some(&b'.') || prefix.get(digits) == Some(&b')'))
                {
                    &prefix[digits + 1..]
                } else {
                    return false;
                }
            };
            after_marker.iter().all(|&b| b == b' ' || b == b'\t')
        })?;
    let abs = st + marker_pos;
    let mut out = source.to_vec();
    let new = match &source[abs..abs + 3] {
        b"[ ]" => b"[x]",
        b"[x]" | b"[X]" => b"[ ]",
        _ => return None,
    };
    out[abs..abs + 3].copy_from_slice(new);
    Some(out)
}

/// Replace a text run within a paragraph: produces a new source with only the affected
/// byte range changed (Invariant 2). Returns the new source and the byte length delta.
pub fn replace_text_run(
    source: &[u8],
    para_span: SourceSpan,
    old: &str,
    new: &str,
) -> Option<(Vec<u8>, i64)> {
    if old.is_empty() {
        // `windows(0)` panics; an empty needle has no defined position.
        return None;
    }
    let (st, e) = (para_span.start.0 as usize, para_span.end.0 as usize);
    if st > e || e > source.len() {
        return None; // stale span
    }
    let region = &source[st..e];
    let pos = region
        .windows(old.len())
        .position(|w| w == old.as_bytes())?;
    let abs = para_span.start.0 as usize + pos;
    let mut out = source.to_vec();
    let old_bytes = old.as_bytes();
    out.splice(abs..abs + old_bytes.len(), new.as_bytes().iter().copied());
    let delta = new.len() as i64 - old.len() as i64;
    Some((out, delta))
}

/// Rebase all spans in a document by `delta` for spans starting at or after `from`.
/// Used after a byte-level edit to keep spans valid for unchanged nodes above the edit.
pub fn rebase_spans_after(doc: &mut Document, from: ByteOffset, delta: i64) {
    fn shift(span: &mut SourceSpan, from: u64, delta: i64) {
        if span.start.0 >= from {
            span.start = ByteOffset((span.start.0 as i64 + delta).max(0) as u64);
        }
        if span.end.0 >= from {
            span.end = ByteOffset((span.end.0 as i64 + delta).max(0) as u64);
        }
    }
    for b in &mut doc.blocks {
        let m = b.meta_mut();
        shift(&mut m.span, from.0, delta);
        // (Children spans rebasing for nested structures is a follow-up; the verbatim
        // serializer only uses top-level block spans for round-trip.)
    }
}

#[cfg(test)]
mod dirty_tests {
    use super::*;
    use crate::SourceSpan;
    use crate::ast::{Block, BlockQuote, Inline, List, ListItem, NodeMeta, Paragraph, TaskState};

    fn meta(span_end: u64) -> NodeMeta {
        NodeMeta {
            span: SourceSpan::new(ByteOffset(0), ByteOffset(span_end)),
            dirty: true,
        }
    }

    fn text_node(s: &str) -> Inline {
        Inline::Text(meta(s.len() as u64), s.to_string())
    }

    #[test]
    fn dirty_list_regenerates_markers() {
        let list = List {
            meta: meta(100),
            ordered: false,
            marker: b'-',
            start: 0,
            tight: true,
            items: vec![
                ListItem {
                    meta: meta(10),
                    task: None,
                    children: vec![Block::Paragraph(Paragraph {
                        meta: meta(5),
                        inlines: vec![text_node("First")],
                    })],
                },
                ListItem {
                    meta: meta(20),
                    task: Some(TaskState::Done),
                    children: vec![Block::Paragraph(Paragraph {
                        meta: meta(6),
                        inlines: vec![text_node("Second")],
                    })],
                },
            ],
        };
        let block = Block::List(list);
        let mut out = Vec::new();
        serialize_block(&block, b"", &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("- First"),
            "first item marker should be present: {text}"
        );
        assert!(
            text.contains("- [x] Second"),
            "second item with task should be present: {text}"
        );
    }

    #[test]
    fn dirty_ordered_list_regenerates_numbers() {
        let list = List {
            meta: meta(100),
            ordered: true,
            marker: b'.',
            start: 1,
            tight: true,
            items: vec![
                ListItem {
                    meta: meta(10),
                    task: None,
                    children: vec![Block::Paragraph(Paragraph {
                        meta: meta(3),
                        inlines: vec![text_node("One")],
                    })],
                },
                ListItem {
                    meta: meta(20),
                    task: None,
                    children: vec![Block::Paragraph(Paragraph {
                        meta: meta(3),
                        inlines: vec![text_node("Two")],
                    })],
                },
            ],
        };
        let block = Block::List(list);
        let mut out = Vec::new();
        serialize_block(&block, b"", &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("1. One"), "first ordered item: {text}");
        assert!(text.contains("2. Two"), "second ordered item: {text}");
    }

    /// `1)`-style ordered lists must regenerate with `)` — a hard-coded `.`
    /// would corrupt the source on dirty regeneration.
    #[test]
    fn dirty_ordered_list_preserves_paren_marker() {
        let list = List {
            meta: meta(100),
            ordered: true,
            marker: b')',
            start: 3,
            tight: true,
            items: vec![ListItem {
                meta: meta(10),
                task: None,
                children: vec![Block::Paragraph(Paragraph {
                    meta: meta(5),
                    inlines: vec![text_node("Item")],
                })],
            }],
        };
        let block = Block::List(list);
        let mut out = Vec::new();
        serialize_block(&block, b"", &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("3) Item"),
            "paren marker must be preserved: {text}"
        );
    }

    /// A header row consisting of a bare `|` produced an inverted cell span
    /// `(start > end)` which panicked downstream in `parse_inlines` /
    /// `emit_span`. Degenerate rows must clamp to an empty span instead.
    #[test]
    fn degenerate_table_row_does_not_panic() {
        let src = b"|\n---\n";
        let doc =
            crate::parse(src, editor_domain::MarkdownProfile::Gfm).expect("parse must not panic");
        let out = serialize(&doc, src);
        assert_eq!(out, src);
    }

    /// Dirty paragraph/heading regeneration must re-emit the block's line
    /// ending — otherwise the following block is glued onto the same line —
    /// and must preserve escape sequences verbatim (`\*`, not bare `*`).
    #[test]
    fn dirty_blocks_preserve_newlines_and_escapes() {
        let src = b"# Title\n\na \\* b\nlast\n";
        let mut doc = crate::parse(src, editor_domain::MarkdownProfile::Gfm).expect("parse");
        for b in &mut doc.blocks {
            b.meta_mut().dirty = true;
        }
        let out = serialize(&doc, src);
        assert_eq!(
            out, src,
            "dirty regeneration must keep line endings and escapes"
        );
    }

    /// `windows(0)` panics — an empty needle must be rejected up front.
    #[test]
    fn replace_text_run_empty_old_returns_none() {
        let src = b"hello\n";
        let span = SourceSpan::new(ByteOffset(0), ByteOffset(6));
        assert!(replace_text_run(src, span, "", "x").is_none());
    }

    #[test]
    fn dirty_block_quote_regenerates_prefix() {
        let bq = BlockQuote {
            meta: meta(100),
            children: vec![Block::Paragraph(Paragraph {
                meta: meta(6),
                inlines: vec![text_node("Quoted")],
            })],
        };
        let block = Block::BlockQuote(bq);
        let mut out = Vec::new();
        serialize_block(&block, b"", &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("> Quoted"),
            "block quote prefix should be present: {text}"
        );
    }

    /// `split_inclusive('\n')` sees a bare-CR document as ONE line — only the
    /// first marker would be stripped, corrupting every continuation line.
    #[test]
    fn de_mark_source_handles_bare_cr_lines() {
        assert_eq!(de_mark_quote_source(b"> a\r> b\r"), b"a\rb\r".to_vec());
        assert_eq!(
            de_mark_quote_source(b"> a\r\n> b\r\n"),
            b"a\r\nb\r\n".to_vec()
        );
        // List item: first line strips the "- " marker, continuation lines
        // strip the content indent.
        assert_eq!(
            de_mark_item_source(b"- a\r  b\r", false),
            b"a\rb\r".to_vec()
        );
        assert_eq!(
            de_mark_item_source(b"- a\r\n  b\r\n", false),
            b"a\r\nb\r\n".to_vec()
        );
    }

    /// A dirty quote in a bare-CR document must regenerate `> ` on every
    /// line — `str::lines()` doesn't split bare `\r` and would emit the whole
    /// quote as a single prefixed line with embedded CRs.
    #[test]
    fn dirty_quote_on_bare_cr_document_splits_lines() {
        let src = b"> a\rb\r";
        let mut doc = crate::parse(src, editor_domain::MarkdownProfile::Gfm).expect("parse");
        for b in &mut doc.blocks {
            b.meta_mut().dirty = true;
        }
        let out = serialize(&doc, src);
        let text = String::from_utf8_lossy(&out);
        let lines: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "expected two regenerated lines: {text:?}");
        assert!(lines.iter().all(|l| l.starts_with("> ")), "{text}");
        // No stray bare-CR may survive inside a regenerated line.
        assert!(
            !lines[0].contains('\r') && !lines[1].contains('\r'),
            "{text}"
        );
    }

    /// A span pushed out of bounds (e.g. `rebase_spans_after` with a large
    /// negative delta collapsing `end` below `start`, or a splice shifting
    /// the tail past the buffer) must degrade to emitting less — never
    /// panic `serialize` via a raw `source[start..end]` slice.
    #[test]
    fn stale_spans_do_not_panic() {
        let src = b"para one\n\npara two\n\npara three\n";
        let mut doc = crate::parse(src, editor_domain::MarkdownProfile::Gfm).expect("parse");
        // Simulate a stale span: invert the middle block and blow the last
        // block's span past the buffer end.
        let mid = doc.blocks[1].meta_mut();
        mid.span.start = editor_domain::ByteOffset(20);
        mid.span.end = editor_domain::ByteOffset(5); // end < start
        let last = doc.blocks[2].meta_mut();
        last.span.end = editor_domain::ByteOffset(u64::MAX);
        // Must not panic; emits whatever survives the clamps.
        let _ = serialize(&doc, src);
    }
}
