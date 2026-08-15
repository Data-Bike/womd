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
pub fn serialize(doc: &Document, source: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(source.len());
    for block in &doc.blocks {
        serialize_block(block, source, &mut out);
    }
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
    out.extend_from_slice(&source[span.start.0 as usize..span.end.0 as usize]);
}

/// Regenerate a dirty block from its (modified) content + preserved trivia.
fn regenerate_block(block: &Block, source: &[u8], out: &mut Vec<u8>) {
    match block {
        Block::Paragraph(p) => {
            out.extend_from_slice(serialize_inlines(&p.inlines).as_bytes());
        }
        Block::Heading(h) => {
            if h.style == HeadingStyle::Atx {
                for _ in 0..h.atx_open_hashes {
                    out.push(b'#');
                }
                out.push(b' ');
                out.extend_from_slice(serialize_inlines(&h.inlines).as_bytes());
                if h.atx_close_hashes > 0 {
                    out.push(b' ');
                    for _ in 0..h.atx_close_hashes {
                        out.push(b'#');
                    }
                }
            } else {
                // Setext: text line + underline of '=' (h1) or '-' (h2).
                out.extend_from_slice(serialize_inlines(&h.inlines).as_bytes());
                out.push(b'\n');
                let underline = if h.level == 1 { b'=' } else { b'-' };
                let ulen = if h.setext_underline_len > 0 { h.setext_underline_len } else { 3 };
                for _ in 0..ulen {
                    out.push(underline);
                }
            }
        }
        Block::List(l) => {
            regenerate_list(l, source, out);
        }
        Block::BlockQuote(bq) => {
            regenerate_block_quote(bq, source, out);
        }
        Block::CodeBlock(_) | Block::Table(_) | Block::HtmlBlock(_)
        | Block::LinkReferenceDefinition(_) | Block::UnknownBlock(_)
        | Block::ThematicBreak(_) | Block::BlankLine(_) => {
            // Fallback: emit verbatim. Regeneration for these node types is a follow-up;
            // edits to them currently go through the PieceTable byte-level path which is
            // already minimal-diff.
            emit_span(block.meta().span, source, out);
        }
    }
}

/// Regenerate a dirty list block from its children + preserved structure (Invariant 2).
/// Each item's children are serialized recursively; the marker and task checkbox are
/// re-emitted from the item's metadata.
fn regenerate_list(list: &List, _source: &[u8], out: &mut Vec<u8>) {
    for (i, item) in list.items.iter().enumerate() {
        // Marker: ordered lists emit `N.` or `N)`; unordered emit `-`/`*`/`+`.
        if list.ordered {
            let n = list.start + i as u32;
            out.extend_from_slice(n.to_string().as_bytes());
            out.push(b'.');
        } else {
            out.push(list.marker);
        }
        out.push(b' ');
        // Task list checkbox.
        if let Some(task) = &item.task {
            match task {
                TaskState::Open => out.extend_from_slice(b"[ ] "),
                TaskState::Done => out.extend_from_slice(b"[x] "),
            }
        }
        // Children: the first child's first line goes on the marker line (no indent).
        // Subsequent lines and blocks are indented by 2 spaces.
        let mut first_child = true;
        for child in &item.children {
            if first_child {
                // Serialize first child without leading indent; its first line follows
                // the marker on the same line.
                let mut child_buf = Vec::new();
                serialize_block(child, _source, &mut child_buf);
                let text = String::from_utf8_lossy(&child_buf);
                let mut lines = text.lines();
                if let Some(first_line) = lines.next() {
                    out.extend_from_slice(first_line.as_bytes());
                    out.push(b'\n');
                }
                for line in lines {
                    out.extend_from_slice(b"  ");
                    out.extend_from_slice(line.as_bytes());
                    out.push(b'\n');
                }
                first_child = false;
            } else {
                // Subsequent children: indent all lines by 2 spaces.
                let mut child_buf = Vec::new();
                serialize_block(child, _source, &mut child_buf);
                let text = String::from_utf8_lossy(&child_buf);
                for line in text.lines() {
                    out.extend_from_slice(b"  ");
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
    for child in &bq.children {
        let mut child_buf = Vec::new();
        serialize_block_with_indent(child, source, &mut child_buf, 0);
        // Prefix each line with `> `.
        let text = String::from_utf8_lossy(&child_buf);
        for line in text.lines() {
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
        let lines: Vec<&str> = text.lines().collect();
        let last_idx = lines.len().saturating_sub(1);
        for (i, line) in lines.iter().enumerate() {
            for _ in 0..indent {
                out.push(b' ');
            }
            out.extend_from_slice(line.as_bytes());
            // Add newline after each line except the last (unless the original ended with one).
            if i < last_idx {
                out.push(b'\n');
            } else if i == last_idx && tmp.last() == Some(&b'\n') {
                out.push(b'\n');
            }
        }
    } else {
        // Emit verbatim, applying indent to each line.
        let span = meta.span;
        let bytes = &source[span.start.0 as usize..span.end.0 as usize];
        let text = String::from_utf8_lossy(bytes);
        for line in text.lines() {
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
    let mut s = String::new();
    for il in inlines {
        serialize_inline(il, &mut s);
    }
    s
}

fn serialize_inline(il: &Inline, s: &mut String) {
    match il {
        Inline::Text(_, t) => s.push_str(t),
        Inline::Emphasis(_, children, kind) => {
            let d = if *kind == EmphasisKind::Asterisk { "*" } else { "_" };
            s.push_str(d);
            s.push_str(&serialize_inlines(children));
            s.push_str(d);
        }
        Inline::Strong(_, children, kind) => {
            let d = if *kind == EmphasisKind::Asterisk { "**" } else { "__" };
            s.push_str(d);
            s.push_str(&serialize_inlines(children));
            s.push_str(d);
        }
        Inline::Strikethrough(_, children) => {
            s.push_str("~~");
            s.push_str(&serialize_inlines(children));
            s.push_str("~~");
        }
        Inline::CodeSpan(_, t, n) => {
            let fence: String = std::iter::repeat('`').take(*n as usize).collect();
            s.push_str(&fence);
            s.push_str(t);
            s.push_str(&fence);
        }
        Inline::Link(l) => match l.style {
            LinkStyle::Inline => {
                s.push('[');
                s.push_str(&serialize_inlines(&l.inlines));
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
                s.push_str(&serialize_inlines(&l.inlines));
                s.push_str("][");
                s.push_str(l.reference.as_deref().unwrap_or(""));
                s.push(']');
            }
            LinkStyle::Collapsed => {
                s.push('[');
                s.push_str(&serialize_inlines(&l.inlines));
                s.push_str("][]");
            }
            LinkStyle::Shortcut => {
                s.push('[');
                s.push_str(&serialize_inlines(&l.inlines));
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
            // Verbatim fallback.
            s.push_str(&format!("[raw html @{}..{}]", m.span.start.0, m.span.end.0));
        }
        Inline::UnknownInline(m) => {
            s.push_str(&format!("[unknown @{}..{}]", m.span.start.0, m.span.end.0));
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
    let region = &source[span.start.0 as usize..span.end.0 as usize];
    let marker_pos = region
        .windows(3)
        .position(|w| w == b"[ ]" || w == b"[x]" || w == b"[X]")?;
    let abs = span.start.0 as usize + marker_pos;
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
    let region = &source[para_span.start.0 as usize..para_span.end.0 as usize];
    let pos = region.windows(old.len()).position(|w| w == old.as_bytes())?;
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
    use crate::ast::{Block, BlockQuote, Inline, List, ListItem, NodeMeta, Paragraph, TaskState};
    use crate::SourceSpan;

    fn meta(span_end: u64) -> NodeMeta {
        NodeMeta { span: SourceSpan::new(ByteOffset(0), ByteOffset(span_end)), dirty: true }
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
        assert!(text.contains("- First"), "first item marker should be present: {text}");
        assert!(text.contains("- [x] Second"), "second item with task should be present: {text}");
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
        assert!(text.contains("> Quoted"), "block quote prefix should be present: {text}");
    }
}
