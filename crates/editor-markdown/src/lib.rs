//! Lossless/source-preserving CommonMark 0.31.2 + GFM Markdown parser, AST, and
//! serializer (ADR-003).
//!
//! * Parsing produces a `Document` whose block spans partition `[0, len)`, so serializing
//!   an unedited document is byte-identical (Invariant 1).
//! * Edits mark affected nodes `dirty`; the serializer regenerates only those nodes,
//!   keeping diffs minimal (Invariant 2).
//! * Unknown extension syntax becomes `UnknownBlock`/`UnknownInline` and is preserved
//!   verbatim (Invariant 5).
//! * Non-standard extensions plug in via `MarkdownExtension` (§24), never mixed into
//!   CommonMark/GFM (§3).

#![forbid(unsafe_code)]

pub mod ast;
pub mod parser;
pub mod serialize;

pub use ast::*;
pub use parser::parse;
pub use serialize::{serialize, serialize_inlines};

use editor_domain::MarkdownProfile;

/// Parse with the given profile (§3).
pub fn parse_with(
    source: &[u8],
    profile: MarkdownProfile,
) -> Result<Document, editor_domain::DocumentError> {
    parser::parse(source, profile)
}

/// Parse a sub-range `[offset, offset+len)` of `source` and return blocks with spans
/// rebased to absolute document offsets (i.e. `offset` maps to `ByteOffset(offset)`).
/// Used by incremental reparse (ADR-003 §58) to reparse only the affected window.
pub fn parse_range(
    source: &[u8],
    offset: u64,
    len: u64,
    profile: MarkdownProfile,
) -> Result<Vec<ast::Block>, editor_domain::DocumentError> {
    let source_len = source.len() as u64;
    // Clamp offset to source length to avoid 32-bit wraparound or out-of-bounds.
    let start = offset.min(source_len) as usize;
    // `offset + len` may overflow if either is near u64::MAX. Use saturating add.
    let end = offset.saturating_add(len).min(source_len) as usize;
    if start >= source.len() {
        return Ok(Vec::new());
    }
    let window = &source[start..end];
    let mut blocks =
        parser::parse_block_sequence_export(window, 0, window.len() as u64, 0, &profile)?;
    // Rebase every document-coordinate span (block meta, inlines, table
    // internals, list item meta) from window-local to absolute document offsets.
    // De-marked children (block quote / list item content) keep their
    // container-relative coordinates.
    let delta = offset as i64;
    for b in &mut blocks {
        ast::shift_block_spans(b, delta);
    }
    Ok(blocks)
}

/// Extension point for non-standard Markdown syntax (§24). Implementations live in plugins.
pub trait MarkdownExtension {
    /// Stable extension id.
    fn id(&self) -> &editor_domain::ids::ExtensionId;
    /// Parse priority relative to other extensions (higher runs first).
    fn precedence(&self) -> i32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_domain::MarkdownProfile;

    fn assert_roundtrip(src: &[u8]) {
        let out = roundtrip(src, MarkdownProfile::Gfm);
        assert_eq!(
            out,
            src,
            "round-trip mismatch for: {:?}",
            String::from_utf8_lossy(src)
        );
    }

    fn roundtrip(src: &[u8], profile: MarkdownProfile) -> Vec<u8> {
        let doc = parse(src, profile).expect("parse failed");
        serialize(&doc, src)
    }

    /// Extract text for a byte span from the source.
    fn span_text(span: SourceSpan, src: &[u8]) -> &[u8] {
        &src[span.start.0 as usize..span.end.0 as usize]
    }

    #[test]
    fn roundtrip_paragraph() {
        assert_roundtrip(b"Hello world.\n");
    }

    /// Pathological nesting (`> > > ...` tens of thousands deep, and nested
    /// list items) must not overflow the stack — beyond MAX_CONTAINER_DEPTH
    /// children parse to empty but the parent span keeps round-trip intact.
    #[test]
    fn pathological_nesting_does_not_crash_and_round_trips() {
        let mut deep_quote = Vec::new();
        for _ in 0..50_000 {
            deep_quote.extend_from_slice(b"> ");
        }
        deep_quote.extend_from_slice(b"x\n");
        assert_roundtrip(&deep_quote);

        let mut deep_list = Vec::new();
        for _ in 0..50_000 {
            deep_list.extend_from_slice(b"  - ");
        }
        deep_list.extend_from_slice(b"x\n");
        assert_roundtrip(&deep_list);
    }

    /// Same hazard for inline nesting: nested emphasis and bracketed link
    /// text recurse `parse_inlines` per level — the depth cap must hold and
    /// round-trip must stay byte-identical.
    #[test]
    fn pathological_inline_nesting_round_trips() {
        // `*a *b *c ... x` — each `*x` opens emphasis that swallows the rest.
        // Sizes are enough to blow the 64-deep cap and the unguarded stack
        // (~4k frames), without making the debug-build test quadratic-slow.
        let mut deep_em = Vec::new();
        for _ in 0..5_000 {
            deep_em.extend_from_slice(b"*a ");
        }
        for _ in 0..5_000 {
            deep_em.extend_from_slice(b"*");
        }
        deep_em.extend_from_slice(b"\n");
        assert_roundtrip(&deep_em);

        // Deeply nested brackets inside link text.
        let mut deep_link = Vec::new();
        for _ in 0..5_000 {
            deep_link.extend_from_slice(b"[a ");
        }
        for _ in 0..5_000 {
            deep_link.extend_from_slice(b"]");
        }
        deep_link.extend_from_slice(b"(x)\n");
        assert_roundtrip(&deep_link);
    }

    #[test]
    fn roundtrip_atx_headings() {
        assert_roundtrip(b"# H1\n## H2\n### H3\n");
    }

    #[test]
    fn roundtrip_atx_with_closing_hashes() {
        assert_roundtrip(b"# H1 #\n## H2  ##\n");
    }

    #[test]
    fn roundtrip_setext() {
        assert_roundtrip(b"Heading 1\n=========\n\nHeading 2\n---------\n");
    }

    #[test]
    fn roundtrip_thematic_break() {
        assert_roundtrip(b"---\n\n***\n\n___\n");
    }

    #[test]
    fn roundtrip_block_quote() {
        assert_roundtrip(b"> quote line one\n> quote line two\n");
    }

    #[test]
    fn roundtrip_unordered_lists() {
        assert_roundtrip(b"- a\n- b\n* c\n+ d\n");
    }

    #[test]
    fn roundtrip_ordered_lists() {
        assert_roundtrip(b"1. one\n2. two\n1) one\n2) two\n");
    }

    #[test]
    fn roundtrip_ordered_start_value() {
        assert_roundtrip(b"42. forty-two\n43. forty-three\n");
    }

    #[test]
    fn roundtrip_task_list() {
        assert_roundtrip(b"- [ ] todo\n- [x] done\n- [X] also done\n");
    }

    #[test]
    fn roundtrip_fenced_code_backtick() {
        assert_roundtrip(b"```rust\nfn main() {}\n```\n");
    }

    #[test]
    fn roundtrip_fenced_code_tilde() {
        assert_roundtrip(b"~~~rust\nfn main() {}\n~~~\n");
    }

    #[test]
    fn roundtrip_indented_code() {
        assert_roundtrip(b"    let x = 1;\n    let y = 2;\n");
    }

    #[test]
    fn roundtrip_emphasis() {
        assert_roundtrip(b"*italic* and _italic_ and **bold** and __bold__.\n");
    }

    #[test]
    fn roundtrip_code_span() {
        assert_roundtrip(b"Use `code` here.\n");
    }

    #[test]
    fn roundtrip_links() {
        assert_roundtrip(b"[text](https://example.com)\n[text](https://example.com \"Title\")\n");
    }

    #[test]
    fn roundtrip_image() {
        assert_roundtrip(b"![alt](image.png)\n![alt](image.png \"Title\")\n");
    }

    #[test]
    fn roundtrip_autolink() {
        assert_roundtrip(b"<https://example.com>\n<name@example.com>\n");
    }

    #[test]
    fn roundtrip_strikethrough() {
        assert_roundtrip(b"~~deleted~~ text.\n");
    }

    #[test]
    fn roundtrip_table() {
        assert_roundtrip(b"| Name | Value |\n| --- | ---: |\n| Foo | 100 |\n");
    }

    /// Table cell inlines must reference the correct byte ranges in the source.
    /// Regression test for a bug where split_table_cells returned offsets relative
    /// to the line slice, but they were used as absolute offsets in the document.
    #[test]
    fn table_cell_offsets_are_absolute() {
        let src = b"# Course Syllabus\n\n## Section\n\n| Item | Specification |\n| --- | --- |\n| Credit value | 1 ECTS |\n| Total workload | 28 hours |\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let table = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                crate::ast::Block::Table(t) => Some(t),
                _ => None,
            })
            .expect("no table found");

        // Header row should have cells "Item" and "Specification".
        let header = table.rows.iter().find(|r| r.header).expect("no header row");
        assert_eq!(header.cells.len(), 2);
        let h0_text = std::str::from_utf8(span_text(header.cells[0].meta.span, src)).unwrap();
        assert_eq!(h0_text.trim(), "Item");
        let h1_text = std::str::from_utf8(span_text(header.cells[1].meta.span, src)).unwrap();
        assert_eq!(h1_text.trim(), "Specification");

        // First data row: "Credit value" | "1 ECTS".
        let r0 = table.rows.iter().find(|r| !r.header).expect("no data row");
        assert_eq!(r0.cells.len(), 2);
        let c0_text = std::str::from_utf8(span_text(r0.cells[0].meta.span, src)).unwrap();
        assert_eq!(c0_text.trim(), "Credit value");
        let c1_text = std::str::from_utf8(span_text(r0.cells[1].meta.span, src)).unwrap();
        assert_eq!(c1_text.trim(), "1 ECTS");
    }

    /// Table with leading indentation — cell offsets must still be absolute.
    #[test]
    fn table_cell_offsets_with_indent() {
        let src = b"| A | B |\n| - | - |\n| 1 | 2 |\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let table = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                crate::ast::Block::Table(t) => Some(t),
                _ => None,
            })
            .expect("no table found");
        let header = table.rows.iter().find(|r| r.header).expect("no header row");
        let h0_text = std::str::from_utf8(span_text(header.cells[0].meta.span, src)).unwrap();
        assert_eq!(h0_text.trim(), "A");
    }

    #[test]
    fn roundtrip_link_reference_def() {
        assert_roundtrip(b"[id]: https://example.com \"Title\"\n");
    }

    #[test]
    fn roundtrip_blank_lines_preserved() {
        assert_roundtrip(b"para one\n\n\n\npara two\n");
    }

    #[test]
    fn roundtrip_complex_document() {
        let src = b"# Title\n\nPara with **bold** and *italic* and `code`.\n\n- item one\n- item two\n\n> a quote\n\n```rust\nfn main() {}\n```\n\n| A | B |\n| - | - |\n| 1 | 2 |\n";
        assert_roundtrip(src);
    }

    #[test]
    fn minimal_diff_text_replace() {
        let src = b"# Test\n\nHello beautiful world.\n";
        let (out, delta) = serialize::replace_text_run(
            src,
            SourceSpan::new(editor_domain::ByteOffset(8), editor_domain::ByteOffset(31)),
            "beautiful",
            "great",
        )
        .expect("replace failed");
        assert_eq!(out, b"# Test\n\nHello great world.\n");
        assert_eq!(delta, -4);
        // Bytes before the edit are unchanged.
        assert_eq!(&out[..8], &src[..8]);
    }

    #[test]
    fn task_toggle_changes_only_checkbox() {
        let src = b"- [ ] todo\n- [x] done\n";
        let doc = parse(src, MarkdownProfile::Gfm).unwrap();
        let item = doc.blocks.iter().find_map(|b| match b {
            Block::List(l) => l.items.first(),
            _ => None,
        });
        let item = item.expect("list item");
        let toggled = serialize::toggle_task_item(src, item).expect("toggle failed");
        assert_eq!(toggled, b"- [x] todo\n- [x] done\n");
        // Only bytes 2..5 changed.
        assert_eq!(&toggled[..2], &src[..2]);
        assert_eq!(&toggled[5..], &src[5..]);
    }

    /// A literal `[ ]` inside code spans or plain text of a NON-task item
    /// must not be toggled — the checkbox lives right after the list marker.
    #[test]
    fn task_toggle_ignores_literal_brackets() {
        let src = b"- text `[ ]` more\n- plain [ ] text\n";
        let doc = parse(src, MarkdownProfile::Gfm).unwrap();
        let list = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::List(l) => Some(l),
                _ => None,
            })
            .expect("list");
        for item in &list.items {
            assert!(item.task.is_none(), "non-task item");
            assert!(
                serialize::toggle_task_item(src, item).is_none(),
                "literal [ ] in code/text must not toggle"
            );
        }
    }

    /// Ordered task items and `[X]` toggle correctly.
    #[test]
    fn task_toggle_ordered_and_uppercase() {
        let src = b"1. [X] done\n2) [ ] todo\n";
        let doc = parse(src, MarkdownProfile::Gfm).unwrap();
        let items: Vec<_> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::List(l) => Some(l.items.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        let first = serialize::toggle_task_item(src, &items[0]).expect("toggle");
        assert_eq!(first, b"1. [ ] done\n2) [ ] todo\n");
        // Toggling the second item on the ORIGINAL source.
        let second = serialize::toggle_task_item(src, &items[1]).expect("toggle");
        assert_eq!(second, b"1. [X] done\n2) [x] todo\n");
    }

    /// A stale item span (from before an earlier edit) must return None, not panic.
    #[test]
    fn task_toggle_stale_span_is_none() {
        let src = b"- [ ] todo\n";
        let doc = parse(src, MarkdownProfile::Gfm).unwrap();
        let mut item = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::List(l) => Some(l.items[0].clone()),
                _ => None,
            })
            .expect("item");
        item.meta.span = SourceSpan::new(
            editor_domain::ByteOffset(0),
            editor_domain::ByteOffset(10_000),
        );
        assert!(serialize::toggle_task_item(src, &item).is_none());
    }

    /// `replace_text_run` on a stale paragraph span returns None, not panic.
    #[test]
    fn replace_text_run_stale_span_returns_none() {
        let src = b"short text\n";
        let stale = SourceSpan::new(
            editor_domain::ByteOffset(0),
            editor_domain::ByteOffset(10_000),
        );
        assert!(serialize::replace_text_run(src, stale, "x", "y").is_none());
        let inverted = SourceSpan::new(editor_domain::ByteOffset(8), editor_domain::ByteOffset(2));
        assert!(serialize::replace_text_run(src, inverted, "x", "y").is_none());
    }

    // --- Two-pass link reference resolution (§100) ---

    fn find_first_link(doc: &crate::ast::Document) -> Option<&crate::ast::Link> {
        for b in &doc.blocks {
            let inlines = match b {
                crate::ast::Block::Paragraph(p) => &p.inlines,
                crate::ast::Block::Heading(h) => &h.inlines,
                _ => continue,
            };
            if let Some(l) = find_link_in_inlines(inlines) {
                return Some(l);
            }
        }
        None
    }

    fn find_link_in_inlines(inlines: &[crate::ast::Inline]) -> Option<&crate::ast::Link> {
        for il in inlines {
            match il {
                crate::ast::Inline::Link(l) => return Some(l),
                crate::ast::Inline::Emphasis(_, c, _)
                | crate::ast::Inline::Strong(_, c, _)
                | crate::ast::Inline::Strikethrough(_, c) => {
                    if let Some(l) = find_link_in_inlines(c) {
                        return Some(l);
                    }
                }
                _ => {}
            }
        }
        None
    }

    #[test]
    fn reference_link_resolves_against_definition() {
        let src = b"[text][label]\n\n[label]: http://example.com\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let link = find_first_link(&doc).expect("link");
        assert_eq!(link.style, crate::ast::LinkStyle::Reference);
        assert_eq!(link.destination, "http://example.com");
        assert_eq!(link.reference.as_deref(), Some("label"));
    }

    #[test]
    fn shortcut_link_resolves() {
        let src = b"[text]\n\n[text]: http://example.com \"Title\"\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let link = find_first_link(&doc).expect("link");
        assert_eq!(link.style, crate::ast::LinkStyle::Shortcut);
        assert_eq!(link.destination, "http://example.com");
        assert_eq!(link.title.as_deref(), Some("Title"));
    }

    #[test]
    fn collapsed_link_resolves() {
        let src = b"[text][]\n\n[text]: http://example.com\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let link = find_first_link(&doc).expect("link");
        assert_eq!(link.style, crate::ast::LinkStyle::Collapsed);
        assert_eq!(link.destination, "http://example.com");
    }

    #[test]
    fn reference_link_resolves_case_insensitive() {
        let src = b"[Text][LABEL]\n\n[label]: http://example.com\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let link = find_first_link(&doc).expect("link");
        assert_eq!(link.destination, "http://example.com");
    }

    #[test]
    fn reference_definition_after_link_roundtrips() {
        let src = b"[text][label]\n\n[label]: http://example.com\n";
        assert_roundtrip(src);
    }

    #[test]
    fn unresolved_reference_link_still_roundtrips() {
        let src = b"[text][unknown]\n";
        assert_roundtrip(src);
    }

    /// CommonMark: when a label is defined twice, the FIRST definition wins;
    /// later duplicates are ignored.
    #[test]
    fn duplicate_label_first_definition_wins() {
        let src = b"[text][x]\n\n[x]: http://first.com\n[x]: http://second.com\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let link = find_first_link(&doc).expect("link");
        assert_eq!(link.destination, "http://first.com");
        // Normalization makes "X"/" x " the same label too.
        let src2 = b"[text][x]\n\n[X]: http://first.com\n[x]: http://second.com\n";
        let doc2 = parse(src2, MarkdownProfile::Gfm).expect("parse");
        let link2 = find_first_link(&doc2).expect("link");
        assert_eq!(link2.destination, "http://first.com");
    }

    /// Angle-bracket destinations may contain spaces — `[x]: <a b c>` must
    /// keep `a b c` as the destination, not split on the first space.
    #[test]
    fn reference_def_angle_destination_with_spaces() {
        let src = b"[t][x]\n\n[x]: <http://a b/c> \"Title\"\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let link = find_first_link(&doc).expect("link");
        assert_eq!(link.destination, "http://a b/c");
        assert_eq!(link.title.as_deref(), Some("Title"));
        // Unclosed angle bracket degrades to token parse, still parses.
        let src2 = b"[t][x]\n\n[x]: <http://a\n";
        let doc2 = parse(src2, MarkdownProfile::Gfm).expect("parse");
        let link2 = find_first_link(&doc2).expect("link");
        assert!(!link2.destination.is_empty());
    }

    /// `[a](<u v> "t")` — an angle-bracket destination inside an inline link
    /// may contain spaces; the title still parses after the closing `>`.
    #[test]
    fn inline_link_angle_destination_with_spaces() {
        let src = b"[a](<http://u v/x> \"t\")\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let link = find_first_link(&doc).expect("link");
        assert_eq!(link.destination, "http://u v/x");
        assert_eq!(link.title.as_deref(), Some("t"));
        assert_roundtrip(src);
    }

    /// `\)` inside an inline link destination is an escape, not the link's
    /// closing paren — `[a](x\)y)` must parse as one link, not stop early.
    #[test]
    fn inline_link_escaped_paren_in_destination() {
        let src = b"[a](http://x.com/a\\)b)\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let link = find_first_link(&doc).expect("link");
        assert_eq!(link.destination, "http://x.com/a\\)b");
        assert_roundtrip(src);
    }

    /// `[]:` is not a link reference definition — an empty label makes it a
    /// plain paragraph per CommonMark.
    #[test]
    fn empty_label_def_is_paragraph() {
        let src = b"[]: http://x.com\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        assert!(
            doc.blocks
                .iter()
                .all(|b| !matches!(b, crate::ast::Block::LinkReferenceDefinition(_))),
            "empty-label line must not produce a LinkReferenceDefinition"
        );
        assert_roundtrip(src);
    }

    // --- Nested block parsing (block quote & list children) ---

    #[test]
    fn roundtrip_block_quote_with_paragraph() {
        assert_roundtrip(b"> Hello world.\n");
    }

    #[test]
    fn roundtrip_block_quote_multiline() {
        assert_roundtrip(b"> Line one.\n> Line two.\n");
    }

    /// CommonMark lazy continuation: a `>`-less line right after quote text
    /// continues the quote's paragraph — it is NOT a new block outside.
    #[test]
    fn block_quote_lazy_continuation_joins_paragraph() {
        let src = b"> line one\nlazy line\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let quotes = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, crate::ast::Block::BlockQuote(_)))
            .count();
        assert_eq!(quotes, 1, "lazy line must stay inside the quote");
        // The quote's span must cover the lazy line too.
        let bq = doc
            .blocks
            .iter()
            .find(|b| matches!(b, crate::ast::Block::BlockQuote(_)))
            .expect("quote");
        assert_eq!(bq.span().end.0, src.len() as u64);
        assert_roundtrip(src);
    }

    /// `> a\n---` — the `---` is a lazy setext underline (h2 inside the
    /// quote), not a thematic break outside it.
    #[test]
    fn block_quote_lazy_setext_underline_becomes_heading() {
        let src = b"> a\n---\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let bq = doc
            .blocks
            .iter()
            .find(|b| matches!(b, crate::ast::Block::BlockQuote(_)))
            .expect("quote");
        if let crate::ast::Block::BlockQuote(q) = bq {
            assert!(
                q.children
                    .iter()
                    .any(|c| matches!(c, crate::ast::Block::Heading(h) if h.level == 2)),
                "lazy --- must produce an h2 inside the quote, got {:?}",
                q.children
            );
        }
        assert_roundtrip(src);
    }

    /// Lines that START a new block cannot lazy-continue a quote:
    /// `> a\n- b` = quote{a} + list{b}.
    #[test]
    fn block_quote_lazy_continuation_stops_at_block_start() {
        let src = b"> a\n- b\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let (quotes, lists) = doc.blocks.iter().fold((0, 0), |(q, l), b| match b {
            crate::ast::Block::BlockQuote(_) => (q + 1, l),
            crate::ast::Block::List(_) => (q, l + 1),
            _ => (q, l),
        });
        assert_eq!((quotes, lists), (1, 1));
        assert_roundtrip(src);
    }

    /// Indented code can't interrupt a paragraph — `> a\n    code` lazily
    /// continues the quote's paragraph, it is NOT code outside the quote.
    #[test]
    fn block_quote_indented_lazy_continuation() {
        let src = b"> a\n    code\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let bq = doc
            .blocks
            .iter()
            .find(|b| matches!(b, crate::ast::Block::BlockQuote(_)))
            .expect("quote");
        assert_eq!(
            bq.span().end.0,
            src.len() as u64,
            "indented lazy line must stay inside"
        );
        assert_roundtrip(src);
    }

    /// An indented `---`/`#`/`>` line after a paragraph is continuation text,
    /// not a block start — indented code never interrupts a paragraph, and a
    /// setext underline may have at most 3 leading spaces.
    #[test]
    fn indented_line_continues_paragraph() {
        // `    ---` under a paragraph: NOT a setext underline.
        let src = b"a\n    ---\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        assert!(
            matches!(doc.blocks.as_slice(), [Block::Paragraph(_)]),
            "4-indented --- must be paragraph text: {:?}",
            doc.blocks
                .iter()
                .map(|b| std::mem::discriminant(b))
                .collect::<Vec<_>>()
        );
        assert_roundtrip(src);

        // `    # h` under a paragraph: NOT a heading.
        let src = b"a\n    # h\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        assert!(matches!(doc.blocks.as_slice(), [Block::Paragraph(_)]));
        assert_roundtrip(src);

        // But an underline indented ≤3 still forms a setext heading.
        let src = b"a\n   ---\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        assert!(matches!(doc.blocks.as_slice(), [Block::Heading(_)]));
        assert_roundtrip(src);
    }

    /// A lazy continuation line inside a block quote keeps its indent —
    /// `> a\n    ---` is paragraph text inside the quote, not an h2.
    #[test]
    fn block_quote_indented_lazy_line_keeps_indent() {
        let src = b"> a\n    ---\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let bq = doc
            .blocks
            .iter()
            .find(|b| matches!(b, crate::ast::Block::BlockQuote(_)))
            .expect("quote");
        assert_eq!(bq.span().end.0, src.len() as u64);
        let crate::ast::Block::BlockQuote(q) = bq else {
            unreachable!()
        };
        // The de-marked children must be a single paragraph (`a` + indented
        // `---` continuation), not para + heading.
        assert!(
            matches!(q.children.as_slice(), [crate::ast::Block::Paragraph(_)]),
            "indented lazy line must stay paragraph text: {} children",
            q.children.len()
        );
        assert_roundtrip(src);
    }

    /// A blank line still ends the quote — `> a\n\nplain` = quote + para.
    #[test]
    fn block_quote_blank_line_still_breaks() {
        let src = b"> a\n\nplain\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let (quotes, paras) = doc.blocks.iter().fold((0, 0), |(q, p), b| match b {
            crate::ast::Block::BlockQuote(_) => (q + 1, p),
            crate::ast::Block::Paragraph(_) => (q, p + 1),
            _ => (q, p),
        });
        assert_eq!((quotes, paras), (1, 1));
        assert_roundtrip(src);
    }

    /// A closing fence indented 4+ spaces is code content, not a closer —
    /// the fence runs to EOF instead of ending early.
    #[test]
    fn fence_closer_indented_four_spaces_is_not_a_closer() {
        let src = b"```\n    ```\nstill code\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let cbs = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, crate::ast::Block::CodeBlock(_)))
            .count();
        assert_eq!(cbs, 1, "4-indent ``` must not close the fence");
        let cb = doc
            .blocks
            .iter()
            .find(|b| matches!(b, crate::ast::Block::CodeBlock(_)))
            .expect("code block");
        assert_eq!(cb.span().end.0, src.len() as u64);
        assert_roundtrip(src);
        // But <=3 spaces still closes.
        let src2 = b"```\n   ```\nafter\n";
        let doc2 = parse(src2, MarkdownProfile::Gfm).expect("parse");
        assert!(
            doc2.blocks
                .iter()
                .any(|b| matches!(b, crate::ast::Block::Paragraph(_))),
            "3-indent closer must end the fence"
        );
    }

    /// `- a\nlazy` — the unmarked line lazily continues the item's
    /// paragraph; it must not end the list nor become a top-level paragraph.
    #[test]
    fn list_lazy_continuation_joins_item() {
        let src = b"- a\nlazy\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let lists = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, crate::ast::Block::List(_)))
            .count();
        assert_eq!(lists, 1);
        let top_paras = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, crate::ast::Block::Paragraph(_)))
            .count();
        assert_eq!(top_paras, 0, "lazy line must not leave the list");
        assert_roundtrip(src);
    }

    /// A blank line kills laziness: `- a\n\nx` = list{a} + paragraph{x}.
    #[test]
    fn list_lazy_continuation_stops_after_blank() {
        let src = b"- a\n\nx\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let (lists, paras) = doc.blocks.iter().fold((0, 0), |(l, p), b| match b {
            crate::ast::Block::List(_) => (l + 1, p),
            crate::ast::Block::Paragraph(_) => (l, p + 1),
            _ => (l, p),
        });
        assert_eq!((lists, paras), (1, 1));
        assert_roundtrip(src);
    }

    /// A fence inside an item doesn't swallow a following unmarked line:
    /// `- ``` `\n  code\nx` = item{fence{code}} + paragraph{x}.
    #[test]
    fn list_fence_does_not_lazy_continue() {
        let src = b"- ```\n  code\nx\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        assert!(
            doc.blocks
                .iter()
                .any(|b| matches!(b, crate::ast::Block::Paragraph(_))),
            "unmarked line after a fenced item must be a paragraph outside"
        );
        assert_roundtrip(src);
    }

    /// Fenced code inside a quote does NOT lazy-continue — an unmarked line
    /// after `> ``` ` must not be swallowed into the fence.
    #[test]
    fn block_quote_fence_does_not_lazy_continue() {
        let src = b"> ```\n> code\noutside\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let bq = doc
            .blocks
            .iter()
            .find(|b| matches!(b, crate::ast::Block::BlockQuote(_)))
            .expect("quote");
        // The quote must end before `outside` — that line belongs to a new
        // paragraph, not the quote's inner fence.
        assert!(bq.span().end.0 < src.len() as u64);
        assert!(
            doc.blocks
                .iter()
                .any(|b| matches!(b, crate::ast::Block::Paragraph(_))),
            "unmarked line after a quoted fence must be a paragraph outside"
        );
        assert_roundtrip(src);
    }

    #[test]
    fn roundtrip_block_quote_with_heading() {
        assert_roundtrip(b"> # Quoted heading\n");
    }

    #[test]
    fn roundtrip_nested_block_quotes() {
        assert_roundtrip(b"> > Deep quote.\n");
    }

    #[test]
    fn block_quote_children_are_parsed() {
        let src = b"> # Heading in quote\n> Paragraph in quote.\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let bq = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                crate::ast::Block::BlockQuote(bq) => Some(bq),
                _ => None,
            })
            .expect("block quote");
        assert!(!bq.children.is_empty(), "children should be parsed");
        // Should contain a heading and a paragraph.
        assert!(
            bq.children
                .iter()
                .any(|b| matches!(b, crate::ast::Block::Heading(_)))
        );
        assert!(
            bq.children
                .iter()
                .any(|b| matches!(b, crate::ast::Block::Paragraph(_)))
        );
    }

    #[test]
    fn roundtrip_list_with_items() {
        assert_roundtrip(b"- First\n- Second\n- Third\n");
    }

    #[test]
    fn roundtrip_ordered_list() {
        assert_roundtrip(b"1. First\n2. Second\n");
    }

    #[test]
    fn roundtrip_task_list_nested() {
        assert_roundtrip(b"- [ ] Todo\n- [x] Done\n");
    }

    #[test]
    fn roundtrip_list_with_nested_content() {
        assert_roundtrip(b"- Item with **bold**\n");
    }

    #[test]
    fn list_item_children_are_parsed() {
        let src = b"- First item\n- Second item\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let list = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                crate::ast::Block::List(l) => Some(l),
                _ => None,
            })
            .expect("list");
        assert_eq!(list.items.len(), 2);
        // Each item should have children (a paragraph).
        assert!(
            list.items[0]
                .children
                .iter()
                .any(|b| matches!(b, crate::ast::Block::Paragraph(_)))
        );
        assert!(
            list.items[1]
                .children
                .iter()
                .any(|b| matches!(b, crate::ast::Block::Paragraph(_)))
        );
    }

    #[test]
    fn roundtrip_block_quote_with_list() {
        assert_roundtrip(b"> - Item in quote\n> - Another item\n");
    }

    #[test]
    fn block_quote_with_list_children() {
        let src = b"> - Item in quote\n> - Another item\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let bq = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                crate::ast::Block::BlockQuote(bq) => Some(bq),
                _ => None,
            })
            .expect("block quote");
        assert!(
            bq.children
                .iter()
                .any(|b| matches!(b, crate::ast::Block::List(_)))
        );
    }

    // --- Parser edge cases ---

    #[test]
    fn roundtrip_setext_multiline_heading() {
        // Setext heading with multi-line text.
        assert_roundtrip(b"A heading\non two lines\n===\n");
    }

    #[test]
    fn roundtrip_nested_unordered_list() {
        // Nested list with 2-space indent.
        assert_roundtrip(b"- Outer\n  - Inner\n");
    }

    #[test]
    fn roundtrip_list_with_blank_line_between() {
        // Loose list (blank line between items).
        assert_roundtrip(b"- First\n\n- Second\n");
    }

    #[test]
    fn roundtrip_html_block() {
        assert_roundtrip(b"<div>\n  <p>Hello</p>\n</div>\n");
    }

    #[test]
    fn roundtrip_inline_html() {
        // Inline HTML within a paragraph.
        assert_roundtrip(b"Text with <b>bold</b> html.\n");
    }

    #[test]
    fn roundtrip_hard_break() {
        // Hard break via two trailing spaces + newline.
        assert_roundtrip(b"Line one.  \nLine two.\n");
    }

    #[test]
    fn roundtrip_multiple_block_quotes() {
        // Two separate block quotes separated by blank line.
        assert_roundtrip(b"> First quote.\n\n> Second quote.\n");
    }

    #[test]
    fn roundtrip_code_span_double_backtick() {
        // Code span with double backtick to allow single backtick inside.
        assert_roundtrip(b"``code with ` inside``\n");
    }

    #[test]
    fn roundtrip_emphasis_nested() {
        assert_roundtrip(b"**bold _italic_ inside**\n");
    }

    #[test]
    fn roundtrip_thematic_break_variants() {
        assert_roundtrip(b"---\n");
        assert_roundtrip(b"***\n");
        assert_roundtrip(b"___\n");
    }

    #[test]
    fn roundtrip_empty_document() {
        assert_roundtrip(b"");
    }

    #[test]
    fn roundtrip_only_newline() {
        assert_roundtrip(b"\n");
    }

    #[test]
    fn roundtrip_multiple_blank_lines() {
        // Multiple blank lines are preserved (lossless).
        assert_roundtrip(b"# Title\n\n\n\nParagraph.\n");
    }

    #[test]
    fn roundtrip_code_block_no_language() {
        assert_roundtrip(b"```\nplain code\n```\n");
    }

    #[test]
    fn roundtrip_nested_strong_emphasis() {
        assert_roundtrip(b"***bold italic***\n");
    }

    #[test]
    fn roundtrip_link_with_title() {
        assert_roundtrip(b"[text](http://example.com \"Title\")\n");
    }

    #[test]
    fn roundtrip_image_without_title() {
        assert_roundtrip(b"![alt](http://example.com/img.png)\n");
    }

    #[test]
    fn roundtrip_autolink_email() {
        // Autolink with email.
        assert_roundtrip(b"<user@example.com>\n");
    }

    #[test]
    fn roundtrip_mixed_lists() {
        // Unordered followed by ordered (separate lists).
        assert_roundtrip(b"- Item A\n\n1. First\n2. Second\n");
    }

    /// `parse_range` at a nonzero offset must rebase *all* document-coordinate
    /// spans — nested inline spans, table row/cell spans, and list item spans —
    /// not just the top-level block meta. De-marked children (block quote and
    /// list item content blocks) must stay in their container-relative space.
    #[test]
    fn parse_range_rebases_nested_spans() {
        let prefix = b"# Intro\n\n";
        let body = b"| A | B |\n| - | - |\n| *x* | `y` |\n\nPara with **bold** text.\n\n- item one\n- item two\n";
        let mut src = prefix.to_vec();
        src.extend_from_slice(body);
        let offset = prefix.len() as u64;
        let blocks = parse_range(&src, offset, body.len() as u64, MarkdownProfile::Gfm)
            .expect("parse_range");

        // Find the table and check cell + inline spans are absolute.
        let table = blocks
            .iter()
            .find_map(|b| match b {
                Block::Table(t) => Some(t),
                _ => None,
            })
            .expect("table block");
        let data_row = table.rows.iter().find(|r| !r.header).expect("data row");
        let cell_text = span_text(data_row.cells[0].meta.span, &src);
        assert_eq!(String::from_utf8_lossy(cell_text).trim(), "*x*");
        // The emphasis inline inside the cell must also be rebased.
        let em = data_row.cells[0]
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Emphasis(m, _, _) => Some(*m),
                _ => None,
            })
            .expect("emphasis inline");
        assert_eq!(span_text(em.span, &src), b"*x*");
        let code = data_row.cells[1]
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::CodeSpan(m, _, _) => Some(*m),
                _ => None,
            })
            .expect("code span");
        assert_eq!(span_text(code.span, &src), b"`y`");

        // Paragraph inlines (Strong) must be rebased.
        let para = blocks
            .iter()
            .find_map(|b| match b {
                Block::Paragraph(p) => Some(p),
                _ => None,
            })
            .expect("paragraph");
        let strong = para
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Strong(m, _, _) => Some(*m),
                _ => None,
            })
            .expect("strong inline");
        assert_eq!(span_text(strong.span, &src), b"**bold**");

        // List item meta spans are document coordinates and must be rebased.
        let list = blocks
            .iter()
            .find_map(|b| match b {
                Block::List(l) => Some(l),
                _ => None,
            })
            .expect("list");
        let item0 = span_text(list.items[0].meta.span, &src);
        assert_eq!(item0, b"- item one\n");
    }

    /// Every document-coordinate span must be within the parsed window.
    #[test]
    fn parse_range_spans_stay_within_window() {
        let prefix = b"prefix\n\n";
        let body = b"- [ ] task\n- plain\n";
        let mut src = prefix.to_vec();
        src.extend_from_slice(body);
        let offset = prefix.len() as u64;
        let blocks = parse_range(&src, offset, body.len() as u64, MarkdownProfile::Gfm)
            .expect("parse_range");
        let list = blocks
            .iter()
            .find_map(|b| match b {
                Block::List(l) => Some(l),
                _ => None,
            })
            .expect("list");
        for item in &list.items {
            assert!(item.meta.span.start.0 >= offset, "item span not rebased");
            assert!(item.meta.span.end.0 <= offset + body.len() as u64);
        }
    }

    #[test]
    fn chunked_parse_preserves_full_coverage() {
        // Simulates large-file chunking: parse the first half, then parse
        // a second chunk and merge. The merged block spans must still
        // contiguously cover [0, total_len) so serialization is byte-identical.
        let src = b"# Section 1\n\nParagraph one.\nStill paragraph one.\n\n> block quote\n> continues\n\nParagraph two.\n";
        let total_len = src.len() as u64;
        // Pick a chunk boundary that falls inside the block quote to exercise
        // the overlap/replace path in Document::merge_blocks.
        let split = src.iter().position(|&b| b == b'>').unwrap() as u64 + 4;
        let first = parse_range(src, 0, split, MarkdownProfile::Gfm).expect("first chunk");
        let mut doc = Document::from_blocks(first);

        let second =
            parse_range(src, split, total_len - split, MarkdownProfile::Gfm).expect("second chunk");
        doc.merge_blocks(second);

        // Spans must be contiguous and cover the full document.
        assert!(!doc.blocks.is_empty(), "expected blocks after merge");
        assert_eq!(
            doc.blocks[0].meta().span.start.0,
            0,
            "first block must start at 0"
        );
        let last_end = doc.blocks.last().unwrap().meta().span.end.0;
        assert_eq!(last_end, total_len, "last block must end at total_len");
        for i in 1..doc.blocks.len() {
            let prev_end = doc.blocks[i - 1].meta().span.end.0;
            let next_start = doc.blocks[i].meta().span.start.0;
            assert_eq!(
                prev_end, next_start,
                "blocks must be contiguous at index {i}"
            );
        }

        // Round-trip must still be byte-identical (no lost/duplicated bytes).
        let out = serialize(&doc, src);
        assert_eq!(out, src, "round-trip mismatch after chunked merge");
    }

    #[test]
    fn roundtrip_inline_math() {
        let src = b"This is \\(ROA_{it}\\) inline.\n";
        let out = roundtrip(src, MarkdownProfile::Gfm);
        assert_eq!(out, src);
    }

    #[test]
    fn roundtrip_display_math() {
        let src = b"\\[\nROA_{it}=\\beta_1 Leverage_{it}+\\varepsilon_{it}.\n\\]\n";
        let out = roundtrip(src, MarkdownProfile::Gfm);
        assert_eq!(out, src);
    }

    #[test]
    fn math_underscores_not_emphasis() {
        let src = b"This is \\(ROA_{it}\\) inline.\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let para = &doc.blocks[0];
        if let Block::Paragraph(Paragraph { inlines, .. }) = para {
            let mut found_math = false;
            for il in inlines {
                if let Inline::MathSpan(_, content, false) = il {
                    assert_eq!(content, "ROA_{it}");
                    found_math = true;
                }
            }
            assert!(found_math, "expected inline math span");
        } else {
            panic!("expected a paragraph");
        }
    }

    /// The task checkbox `[ ] `/`[x] ` is re-emitted from `item.task` — it must
    /// not also remain in the children's text (double checkbox on render and
    /// on dirty regenerate).
    #[test]
    fn task_item_children_exclude_checkbox() {
        let src = b"- [ ] todo item\n- [x] done item\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let Block::List(l) = &doc.blocks[0] else {
            panic!("expected list")
        };
        assert_eq!(l.items[0].task, Some(TaskState::Open));
        assert_eq!(l.items[1].task, Some(TaskState::Done));
        let first_text = match &l.items[0].children[0] {
            Block::Paragraph(p) => crate::serialize::serialize_inlines(&p.inlines),
            _ => panic!("expected paragraph child"),
        };
        assert_eq!(first_text, "todo item");
        let second_text = match &l.items[1].children[0] {
            Block::Paragraph(p) => crate::serialize::serialize_inlines(&p.inlines),
            _ => panic!("expected paragraph child"),
        };
        assert_eq!(second_text, "done item");
    }

    /// Dirty block quote regeneration: clean children must emit THEIR bytes
    /// (de-marked coordinates), not coincident document bytes.
    #[test]
    fn dirty_block_quote_regen_uses_demarked_source() {
        let src = b"para text that makes the doc long\n\n> hello world\n> second line\n";
        let mut doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let quote = doc
            .blocks
            .iter_mut()
            .find_map(|b| match b {
                Block::BlockQuote(bq) => Some(bq),
                _ => None,
            })
            .expect("quote block");
        quote.meta.dirty = true;
        let out = serialize(&doc, src);
        assert_eq!(
            String::from_utf8_lossy(&out),
            "para text that makes the doc long\n\n> hello world\n> second line\n"
        );
    }

    /// Dirty list regeneration: item children must resolve against the item's
    /// de-marked buffer, and the task checkbox must appear exactly once.
    #[test]
    fn dirty_list_regen_uses_demarked_source_and_single_checkbox() {
        let src = b"para text that makes the doc long\n\n- [ ] first item\n- second item\n";
        let mut doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let list = doc
            .blocks
            .iter_mut()
            .find_map(|b| match b {
                Block::List(l) => Some(l),
                _ => None,
            })
            .expect("list block");
        list.meta.dirty = true;
        let out = serialize(&doc, src);
        assert_eq!(
            String::from_utf8_lossy(&out),
            "para text that makes the doc long\n\n- [ ] first item\n- second item\n"
        );
    }

    /// Ordered list dirty regen keeps the `)` delimiter and numbering.
    #[test]
    fn dirty_ordered_list_regen_preserves_marker() {
        let src = b"3) alpha\n4) beta\n";
        let mut doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let list = doc
            .blocks
            .iter_mut()
            .find_map(|b| match b {
                Block::List(l) => Some(l),
                _ => None,
            })
            .expect("list block");
        list.meta.dirty = true;
        let out = serialize(&doc, src);
        assert_eq!(String::from_utf8_lossy(&out), "3) alpha\n4) beta\n");
    }

    /// Dirty regen of a list whose item has a lazy continuation line: the
    /// rebuilt de-marked buffer must keep the unindented `lazy` line verbatim
    /// (strip = min(marker width, line indent)), so the child's span reads the
    /// right bytes — before the fix the fixed strip ate `la` of `lazy`.
    #[test]
    fn dirty_list_regen_lazy_continuation_line() {
        let src = b"- a\nlazy\n";
        let mut doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let list = doc
            .blocks
            .iter_mut()
            .find_map(|b| match b {
                Block::List(l) => Some(l),
                _ => None,
            })
            .expect("list block");
        // The item must contain a single paragraph covering `a\nlazy`.
        let item = &list.items[0];
        assert_eq!(item.children.len(), 1);
        list.meta.dirty = true;
        let out = serialize(&doc, src);
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("lazy"),
            "lazy line must survive regen: {text:?}"
        );
        assert!(
            text.contains("- a"),
            "marker+text must survive regen: {text:?}"
        );
    }

    /// An unresolved reference-style link is literal text, not a link with an
    /// empty destination (CommonMark; also prevents `[t]()` on regen).
    #[test]
    fn unresolved_reference_is_literal_text() {
        let src = b"see [nope][missing] here\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let Block::Paragraph(p) = &doc.blocks[0] else {
            panic!("expected paragraph")
        };
        assert!(
            p.inlines.iter().all(|i| !matches!(i, Inline::Link(_))),
            "unresolved reference must not produce a Link node"
        );
    }

    /// A closing fence LONGER than the opening run still closes the block
    /// (CommonMark: the closer needs at least the opener's length).
    #[test]
    fn longer_closing_fence_terminates_block() {
        let src = b"```rust\ncode\n`````\nafter\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let code = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::CodeBlock(cb) => Some(cb),
                _ => None,
            })
            .expect("code block");
        // The 5-backtick line closes the block; "after" must be a new block.
        assert_eq!(code.meta.span.end.0 as usize, src.len() - 6);
        let last = doc.blocks.last().unwrap();
        assert!(
            matches!(last, Block::Paragraph(_)),
            "trailing line must be a paragraph"
        );
    }

    /// Nested inline spans (depth >= 2: emphasis inside strong, emphasis inside
    /// a link) must be rebased to document coordinates, not left relative to
    /// the inner parse region.
    #[test]
    fn nested_inline_spans_are_document_absolute() {
        let src = b"para **bold *it* text** and [`c` link](http://x)\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let Block::Paragraph(p) = &doc.blocks[0] else {
            panic!("expected paragraph")
        };
        let strong = p
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Strong(m, c, _) => Some((*m, c)),
                _ => None,
            })
            .expect("strong");
        assert_eq!(span_text(strong.0.span, src), b"**bold *it* text**");
        let em = strong
            .1
            .iter()
            .find_map(|i| match i {
                Inline::Emphasis(m, _, _) => Some(*m),
                _ => None,
            })
            .expect("nested emphasis");
        assert_eq!(
            span_text(em.span, src),
            b"*it*",
            "nested emphasis span must be document-absolute"
        );

        let link = p
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Link(l) => Some(l),
                _ => None,
            })
            .expect("link");
        let code = link
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::CodeSpan(m, _, _) => Some(*m),
                _ => None,
            })
            .expect("code inside link");
        assert_eq!(
            span_text(code.span, src),
            b"`c`",
            "link child span must be document-absolute"
        );
    }

    /// CRLF line endings must round-trip byte-identically — spans include the
    /// `\r\n` verbatim, so no block loses or gains bytes.
    #[test]
    fn roundtrip_crlf_document() {
        assert_roundtrip(
            b"# Title\r\n\r\nParagraph with **bold**.\r\n\r\n- item\r\n- [ ] task\r\n",
        );
    }

    /// Bare `\r` (classic Mac line ending) is a line terminator too — a
    /// CR-only file must split into real blocks, not collapse into one
    /// giant line. Round-trip still byte-identical.
    #[test]
    fn bare_cr_splits_blocks() {
        let doc = parse(b"# Title\r\r- item\r- two", MarkdownProfile::Gfm).expect("parse");
        // heading, blank-ish separation, list — not one fused paragraph
        assert!(
            doc.blocks.len() >= 2,
            "bare-CR doc parsed as a single block: {:?}",
            doc.blocks.len()
        );
        assert_roundtrip(b"# Title\r\r- item\r- two");
        // Mixed families keep working.
        assert_roundtrip(b"a\rb\r\nc\nd");
    }

    /// Deeper nesting (emphasis inside strong inside strikethrough, code inside
    /// emphasis) — every level's span must be document-absolute after rebasing.
    #[test]
    fn deeply_nested_inline_spans_are_document_absolute() {
        let src = b"p **a ~~b *c* d~~ e**\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let Block::Paragraph(p) = &doc.blocks[0] else {
            panic!("expected paragraph")
        };
        let strong = p
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Strong(m, c, _) => Some((*m, c)),
                _ => None,
            })
            .expect("strong");
        assert_eq!(span_text(strong.0.span, src), b"**a ~~b *c* d~~ e**");
        let strike = strong
            .1
            .iter()
            .find_map(|i| match i {
                Inline::Strikethrough(m, c) => Some((*m, c)),
                _ => None,
            })
            .expect("strikethrough");
        assert_eq!(span_text(strike.0.span, src), b"~~b *c* d~~");
        let em = strike
            .1
            .iter()
            .find_map(|i| match i {
                Inline::Emphasis(m, _, _) => Some(*m),
                _ => None,
            })
            .expect("emphasis at depth 3");
        assert_eq!(
            span_text(em.span, src),
            b"*c*",
            "depth-3 inline must be doc-absolute"
        );
    }

    /// Same-marker nested emphasis `*a *b* c*`: the outer closer must pair
    /// with the LAST run (after the inner `*b*` closes), not the first inner
    /// run — otherwise the tree flattens wrongly.
    #[test]
    fn nested_same_marker_emphasis_pairs_correctly() {
        let src = b"p *a *b* c*\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let Block::Paragraph(p) = &doc.blocks[0] else {
            panic!("expected paragraph")
        };
        let em = p
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Emphasis(m, c, _) => Some((*m, c)),
                _ => None,
            })
            .expect("outer emphasis");
        assert_eq!(span_text(em.0.span, src), b"*a *b* c*");
        let inner =
            em.1.iter()
                .find_map(|i| match i {
                    Inline::Emphasis(m, _, _) => Some(*m),
                    _ => None,
                })
                .expect("inner emphasis");
        assert_eq!(span_text(inner.span, src), b"*b*");
    }

    /// `**bold *em***` — the `***` run must close BOTH the inner `*` and the
    /// outer `**` (strong containing emphasis, not literal text).
    #[test]
    fn cascading_closer_run_pairs_inner_and_outer() {
        let src = b"p **bold *em***\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let Block::Paragraph(p) = &doc.blocks[0] else {
            panic!("expected paragraph")
        };
        let strong = p
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Strong(m, c, _) => Some((*m, c)),
                _ => None,
            })
            .expect("strong");
        assert_eq!(span_text(strong.0.span, src), b"**bold *em***");
        let em = strong
            .1
            .iter()
            .find_map(|i| match i {
                Inline::Emphasis(m, _, _) => Some(*m),
                _ => None,
            })
            .expect("nested emphasis");
        assert_eq!(span_text(em.span, src), b"*em*");
    }

    /// Delimiter flanking rules (CommonMark §emphasis):
    /// - `_` inside a word is literal (`snake_case` is not emphasis)
    /// - a `*` followed by whitespace cannot open (`a * b`)
    /// - `_a_b` has no valid closer (intraword `_` can't close)
    /// - `a*b*c` IS valid (`*` may be intraword)
    #[test]
    fn emphasis_flanking_rules() {
        for src in [&b"snake_case_name\n"[..], b"a * b * tail\n", b"_a_b\n"] {
            let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
            let Block::Paragraph(p) = &doc.blocks[0] else {
                panic!("expected paragraph")
            };
            assert!(
                p.inlines
                    .iter()
                    .all(|i| !matches!(i, Inline::Emphasis(..) | Inline::Strong(..))),
                "no emphasis expected in {:?}",
                String::from_utf8_lossy(src)
            );
        }
        // In `a * b *c* tail` the lone `*` is literal but `*c*` IS emphasis —
        // only one pair forms.
        let doc = parse(b"a * b *c* tail\n", MarkdownProfile::Gfm).expect("parse failed");
        let Block::Paragraph(p) = &doc.blocks[0] else {
            panic!("expected paragraph")
        };
        let ems: Vec<_> = p
            .inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Emphasis(m, _, _) => Some(*m),
                _ => None,
            })
            .collect();
        assert_eq!(ems.len(), 1);
        assert_eq!(span_text(ems[0].span, b"a * b *c* tail\n"), b"*c*");
        let doc = parse(b"a*b*c\n", MarkdownProfile::Gfm).expect("parse failed");
        let Block::Paragraph(p) = &doc.blocks[0] else {
            panic!("expected paragraph")
        };
        let em = p
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Emphasis(m, _, _) => Some(*m),
                _ => None,
            })
            .expect("intraword `*` emphasis");
        assert_eq!(span_text(em.span, b"a*b*c\n"), b"*b*");
    }

    /// `_foo_bar_` emphasizes "foo_bar" — the intraword `_` is not a delimiter.
    #[test]
    fn underscore_intraword_inside_emphasis() {
        let src = b"_foo_bar_\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let Block::Paragraph(p) = &doc.blocks[0] else {
            panic!("expected paragraph")
        };
        let em = p
            .inlines
            .iter()
            .find_map(|i| match i {
                Inline::Emphasis(m, _, _) => Some(*m),
                _ => None,
            })
            .expect("emphasis");
        assert_eq!(span_text(em.span, src), b"_foo_bar_");
    }

    /// An unresolved shortcut reference `[nope]` (no `[]`/label) must stay
    /// literal text — CommonMark requires a matching definition.
    #[test]
    fn unresolved_shortcut_is_literal_text() {
        let src = b"see [nope] here\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let Block::Paragraph(p) = &doc.blocks[0] else {
            panic!("expected paragraph")
        };
        assert!(
            p.inlines.iter().all(|i| !matches!(i, Inline::Link(_))),
            "unresolved shortcut must not produce a Link node"
        );
        assert_roundtrip(src);
    }

    /// An unresolved collapsed reference `[nope][]` must also stay literal.
    #[test]
    fn unresolved_collapsed_is_literal_text() {
        let src = b"see [nope][] here\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let Block::Paragraph(p) = &doc.blocks[0] else {
            panic!("expected paragraph")
        };
        assert!(
            p.inlines.iter().all(|i| !matches!(i, Inline::Link(_))),
            "unresolved collapsed ref must not produce a Link node"
        );
    }

    /// A fence that never closes consumes the rest of the document and still
    /// round-trips (CommonMark: unterminated fence runs to EOF).
    #[test]
    fn unterminated_fence_consumes_to_eof() {
        let src = b"```rust\ncode\nmore code\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        assert_eq!(doc.blocks.len(), 1);
        assert!(matches!(doc.blocks[0], Block::CodeBlock(_)));
        assert_eq!(doc.blocks[0].meta().span.end.0 as usize, src.len());
        assert_roundtrip(src);
    }

    /// Tilde fences: a longer closing run still terminates (same rule as backticks).
    #[test]
    fn tilde_fence_longer_closer_terminates() {
        let src = b"~~~x\ncode\n~~~~~\nafter\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let code = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::CodeBlock(cb) => Some(cb),
                _ => None,
            })
            .expect("code block");
        assert_eq!(code.fence_char, b'~');
        assert_eq!(code.meta.span.end.0 as usize, src.len() - 6);
        assert!(matches!(doc.blocks.last().unwrap(), Block::Paragraph(_)));
    }

    /// Seven or more `#` is a paragraph, not a heading (CommonMark: 1–6 only).
    #[test]
    fn seven_hashes_is_paragraph() {
        let src = b"####### not a heading\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        assert!(
            matches!(doc.blocks[0], Block::Paragraph(_)),
            "7+ hashes must be a paragraph"
        );
        assert_roundtrip(src);
    }

    /// `#` must be followed by space/EOL — `#x` is a paragraph, `# ` is an
    /// empty h1; both round-trip.
    #[test]
    fn atx_heading_space_requirement() {
        let src = b"#x not heading\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        assert!(matches!(doc.blocks[0], Block::Paragraph(_)));
        assert_roundtrip(b"# \n#x\n");
    }

    /// Block-quote child spans live in the de-marked buffer's coordinate
    /// space (0-based, `> ` stripped) — they must NOT index the document.
    #[test]
    fn block_quote_children_spans_are_demarked() {
        let src = b"> hello world\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let bq = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::BlockQuote(bq) => Some(bq),
                _ => None,
            })
            .expect("quote");
        let child = &bq.children[0];
        // The child's span indexes the de-marked buffer ("hello world\n"),
        // so it starts at 0 even though the document text is at offset 2.
        assert_eq!(
            child.meta().span.start.0,
            0,
            "child span must be de-marked-relative"
        );
    }

    /// List-item child spans are likewise de-marked-relative.
    #[test]
    fn list_item_children_spans_are_demarked() {
        let src = b"- hello world\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let list = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::List(l) => Some(l),
                _ => None,
            })
            .expect("list");
        let child = &list.items[0].children[0];
        assert_eq!(
            child.meta().span.start.0,
            0,
            "item child span must be de-marked-relative"
        );
    }

    /// Dirty paragraph regen must preserve the backslash of an escaped
    /// punctuation char — dropping it would change `\*x\*` into emphasis.
    #[test]
    fn dirty_paragraph_regen_keeps_escapes() {
        let src = b"see \\*not emphasis\\* here\n";
        let mut doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let Block::Paragraph(p) = &mut doc.blocks[0] else {
            panic!("expected paragraph")
        };
        p.meta.dirty = true;
        let out = serialize(&doc, src);
        assert_eq!(out, src, "escaped punctuation must survive dirty regen");
    }

    /// Nested quote inside a list item: dirty regen of the whole list must
    /// reproduce the nested content exactly.
    #[test]
    fn dirty_list_regen_nested_quote() {
        let src = b"- item\n\n  > quoted\n  > lines\n";
        let mut doc = parse(src, MarkdownProfile::Gfm).expect("parse failed");
        let list = doc
            .blocks
            .iter_mut()
            .find_map(|b| match b {
                Block::List(l) => Some(l),
                _ => None,
            })
            .expect("list block");
        list.meta.dirty = true;
        let out = serialize(&doc, src);
        assert_eq!(String::from_utf8_lossy(&out), String::from_utf8_lossy(src));
    }
}
