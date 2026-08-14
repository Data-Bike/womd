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

use editor_domain::ByteOffset;

use editor_domain::MarkdownProfile;

/// Parse with the given profile (§3).
pub fn parse_with(source: &[u8], profile: MarkdownProfile) -> Result<Document, editor_domain::DocumentError> {
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
    let start = offset as usize;
    let end = (start + len as usize).min(source.len());
    if start > source.len() {
        return Ok(Vec::new());
    }
    let window = &source[start..end];
    let blocks = parser::parse_block_sequence_export(window, 0, window.len() as u64, 0, &profile)?;
    // Rebase spans from window-local to absolute document offsets.
    let delta = offset as i64;
    let mut blocks = blocks;
    for b in &mut blocks {
        let m = b.meta_mut();
        m.span.start = ByteOffset((m.span.start.0 as i64 + delta).max(0) as u64);
        m.span.end = ByteOffset((m.span.end.0 as i64 + delta).max(0) as u64);
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
        assert_eq!(out, src, "round-trip mismatch for: {:?}", String::from_utf8_lossy(src));
    }

    fn roundtrip(src: &[u8], profile: MarkdownProfile) -> Vec<u8> {
        let doc = parse(src, profile).expect("parse failed");
        serialize(&doc, src)
    }

    #[test]
    fn roundtrip_paragraph() {
        assert_roundtrip(b"Hello world.\n");
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
                crate::ast::Inline::Emphasis(_, c, _) | crate::ast::Inline::Strong(_, c, _)
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

    // --- Nested block parsing (block quote & list children) ---

    #[test]
    fn roundtrip_block_quote_with_paragraph() {
        assert_roundtrip(b"> Hello world.\n");
    }

    #[test]
    fn roundtrip_block_quote_multiline() {
        assert_roundtrip(b"> Line one.\n> Line two.\n");
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
        let bq = doc.blocks.iter().find_map(|b| match b {
            crate::ast::Block::BlockQuote(bq) => Some(bq),
            _ => None,
        }).expect("block quote");
        assert!(!bq.children.is_empty(), "children should be parsed");
        // Should contain a heading and a paragraph.
        assert!(bq.children.iter().any(|b| matches!(b, crate::ast::Block::Heading(_))));
        assert!(bq.children.iter().any(|b| matches!(b, crate::ast::Block::Paragraph(_))));
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
        let list = doc.blocks.iter().find_map(|b| match b {
            crate::ast::Block::List(l) => Some(l),
            _ => None,
        }).expect("list");
        assert_eq!(list.items.len(), 2);
        // Each item should have children (a paragraph).
        assert!(list.items[0].children.iter().any(|b| matches!(b, crate::ast::Block::Paragraph(_))));
        assert!(list.items[1].children.iter().any(|b| matches!(b, crate::ast::Block::Paragraph(_))));
    }

    #[test]
    fn roundtrip_block_quote_with_list() {
        assert_roundtrip(b"> - Item in quote\n> - Another item\n");
    }

    #[test]
    fn block_quote_with_list_children() {
        let src = b"> - Item in quote\n> - Another item\n";
        let doc = parse(src, MarkdownProfile::Gfm).expect("parse");
        let bq = doc.blocks.iter().find_map(|b| match b {
            crate::ast::Block::BlockQuote(bq) => Some(bq),
            _ => None,
        }).expect("block quote");
        assert!(bq.children.iter().any(|b| matches!(b, crate::ast::Block::List(_))));
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
}
