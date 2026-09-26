//! Semantic Markdown diff (§43): an additional, document-structured view of changes built
//! on top of `editor-markdown`. Raw Git diff remains the source of truth (Invariant 10);
//! this module provides a higher-level representation for the UI ("Section X: paragraph
//! modified", "table row added", …).

use editor_domain::MarkdownProfile;
use editor_markdown::{parse, Block, Document};

/// A structural unit that can be reported as changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticUnit {
    Heading { level: u8, text: String },
    Paragraph,
    ListItem,
    Table,
    TableRow,
    TableCell,
    CodeBlock,
    Link,
    Image,
    BlockQuote,
    ThematicBreak,
    Other,
}

impl SemanticUnit {
    fn from_block(b: &Block) -> Self {
        match b {
            Block::Heading(h) => Self::Heading {
                level: h.level,
                text: editor_markdown::serialize_inlines(&h.inlines),
            },
            Block::Paragraph(_) => Self::Paragraph,
            Block::List(_) => Self::ListItem,
            Block::Table(_) => Self::Table,
            Block::CodeBlock(_) => Self::CodeBlock,
            Block::BlockQuote(_) => Self::BlockQuote,
            Block::ThematicBreak(_) => Self::ThematicBreak,
            Block::HtmlBlock(_) | Block::LinkReferenceDefinition(_) | Block::BlankLine(_) | Block::UnknownBlock(_) => Self::Other,
        }
    }
    fn label(&self) -> String {
        match self {
            Self::Heading { level, text } => format!("H{level} \"{text}\""),
            Self::Paragraph => "Paragraph".to_string(),
            Self::ListItem => "List item".to_string(),
            Self::Table => "Table".to_string(),
            Self::TableRow => "Table row".to_string(),
            Self::TableCell => "Table cell".to_string(),
            Self::CodeBlock => "Code block".to_string(),
            Self::Link => "Link".to_string(),
            Self::Image => "Image".to_string(),
            Self::BlockQuote => "Block quote".to_string(),
            Self::ThematicBreak => "Thematic break".to_string(),
            Self::Other => "Other".to_string(),
        }
    }
}

/// One semantic change between two document versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticChange {
    Added { unit: SemanticUnit },
    Removed { unit: SemanticUnit },
    Modified { unit: SemanticUnit },
}

impl SemanticChange {
    pub fn describe(&self) -> String {
        match self {
            Self::Added { unit } => format!("{} added", unit.label()),
            Self::Removed { unit } => format!("{} removed", unit.label()),
            Self::Modified { unit } => format!("{} modified", unit.label()),
        }
    }
}

/// Compute a semantic diff between two Markdown sources by comparing block spans.
///
/// Blocks are matched by (unit kind, heading text) where applicable; otherwise by order.
/// This is intentionally a simple, robust mapping — it does not replace the raw Git diff
/// (§43, Invariant 10).
pub fn semantic_diff(old: &[u8], new: &[u8], profile: MarkdownProfile) -> Vec<SemanticChange> {
    let old_doc = match parse(old, profile.clone()) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let new_doc = match parse(new, profile) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    diff_documents(&old_doc, &new_doc, old, new)
}

fn diff_documents(old: &Document, new: &Document, old_src: &[u8], new_src: &[u8]) -> Vec<SemanticChange> {
    // Skip blank-line trivia; compare meaningful blocks.
    let old_blocks: Vec<&Block> = old.blocks.iter().filter(|b| !matches!(b, Block::BlankLine(_))).collect();
    let new_blocks: Vec<&Block> = new.blocks.iter().filter(|b| !matches!(b, Block::BlankLine(_))).collect();

    // Greedy positional match with kind/text equality; remaining unmatched are added/removed.
    let mut used_new = vec![false; new_blocks.len()];
    let mut changes = Vec::new();

    for ob in &old_blocks {
        let ou = SemanticUnit::from_block(ob);
        let matched = new_blocks.iter().enumerate().find(|(j, nb)| {
            !used_new[*j] && SemanticUnit::from_block(nb) == ou
        });
        match matched {
            Some((j, _)) => {
                used_new[j] = true;
                // Same kind/text: check byte content for "modified".
                if !bytes_equal(ob, new_blocks[j], old_src, new_src) {
                    changes.push(SemanticChange::Modified { unit: ou });
                }
            }
            None => changes.push(SemanticChange::Removed { unit: ou }),
        }
    }
    for (j, nb) in new_blocks.iter().enumerate() {
        if !used_new[j] {
            changes.push(SemanticChange::Added { unit: SemanticUnit::from_block(nb) });
        }
    }
    changes
}

/// Compare two blocks by source bytes. Structural comparison can't work:
/// `CodeBlock`, `HtmlBlock`, and `UnknownBlock` don't store their content in
/// the AST (it's only in `source[span]`), so two code blocks with different
/// bodies would wrongly compare equal and the modification would be missed.
/// Byte comparison through each block's own source span also correctly reports
/// identical content at different offsets as unchanged.
fn bytes_equal(a: &Block, b: &Block, a_src: &[u8], b_src: &[u8]) -> bool {
    fn span_bytes<'s>(src: &'s [u8], span: editor_markdown::SourceSpan) -> &'s [u8] {
        let start = (span.start.0 as usize).min(src.len());
        let end = (span.end.0 as usize).min(src.len()).max(start);
        &src[start..end]
    }
    span_bytes(a_src, a.span()) == span_bytes(b_src, b.span())
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_domain::MarkdownProfile;

    #[test]
    fn added_paragraph() {
        let old = b"# Title\n";
        let new = b"# Title\n\nNew paragraph.\n";
        let changes = semantic_diff(old, new, MarkdownProfile::Gfm);
        assert!(changes.iter().any(|c| matches!(c, SemanticChange::Added { unit: SemanticUnit::Paragraph })));
    }

    #[test]
    fn removed_heading() {
        let old = b"# Title\n\n## Subtitle\n";
        let new = b"# Title\n";
        let changes = semantic_diff(old, new, MarkdownProfile::Gfm);
        assert!(changes.iter().any(|c| matches!(c, SemanticChange::Removed { .. })));
    }

    #[test]
    fn modified_paragraph() {
        let old = b"Hello world.\n";
        let new = b"Hello universe.\n";
        let changes = semantic_diff(old, new, MarkdownProfile::Gfm);
        assert!(changes.iter().any(|c| matches!(c, SemanticChange::Modified { unit: SemanticUnit::Paragraph })));
    }

    #[test]
    fn identical_documents_have_no_changes() {
        let src = b"# Title\n\nPara.\n\n- a\n- b\n";
        assert!(semantic_diff(src, src, MarkdownProfile::Gfm).is_empty());
    }

    #[test]
    fn describe_is_human_readable() {
        let old = b"# Title\n";
        let new = b"# Title\n\nNew para.\n";
        let changes = semantic_diff(old, new, MarkdownProfile::Gfm);
        let desc = changes.iter().map(|c| c.describe()).collect::<Vec<_>>().join("; ");
        assert!(desc.contains("Paragraph added"));
    }

    /// A same-length edit produces identical spans on both sides — the old
    /// `bytes_equal` treated equal spans as equal content and missed the
    /// modification entirely.
    #[test]
    fn same_length_modification_is_detected() {
        let old = b"abc\n";
        let new = b"xyz\n";
        let changes = semantic_diff(old, new, MarkdownProfile::Gfm);
        assert!(changes.iter().any(|c| matches!(
            c,
            SemanticChange::Modified { unit: SemanticUnit::Paragraph }
        )), "expected a Modified paragraph, got {changes:?}");
    }

    /// A block whose content is identical but whose span moved (because an
    /// earlier edit shifted it) must compare equal — position is not content.
    #[test]
    fn moved_but_identical_block_is_not_modified() {
        // Direct unit test of the comparison helper: two parses of the same
        // paragraph at different offsets.
        let doc_a = parse(b"shifted text\n", MarkdownProfile::Gfm).unwrap();
        let doc_b = parse(b"x y z\n\nshifted text\n", MarkdownProfile::Gfm).unwrap();
        // Both are single-line; take each last paragraph.
        let a = &doc_a.blocks[0];
        let b = doc_b.blocks.iter().rev().find(|x| matches!(x, Block::Paragraph(_))).unwrap();
        assert_ne!(a.span(), b.span(), "spans must differ for the test to be meaningful");
        assert!(
            super::bytes_equal(a, b, b"shifted text\n", b"x y z\n\nshifted text\n"),
            "identical content at different offsets must compare equal"
        );
    }

    /// CodeBlock doesn't store its body in the AST — a content-only change
    /// inside a fenced block must still be detected as Modified.
    #[test]
    fn modified_code_block_body_is_detected() {
        let old = b"```rust\nlet a = 1;\n```\n";
        let new = b"```rust\nlet a = 2;\n```\n";
        let changes = semantic_diff(old, new, MarkdownProfile::Gfm);
        assert!(changes.iter().any(|c| matches!(
            c,
            SemanticChange::Modified { unit: SemanticUnit::CodeBlock }
        )), "expected a Modified code block, got {changes:?}");
    }

    /// HtmlBlock likewise stores no content — body changes must be detected.
    #[test]
    fn modified_html_block_body_is_detected() {
        let old = b"<div>\n  <p>old</p>\n</div>\n";
        let new = b"<div>\n  <p>new</p>\n</div>\n";
        let changes = semantic_diff(old, new, MarkdownProfile::Gfm);
        assert!(changes.iter().any(|c| matches!(c, SemanticChange::Modified { .. })),
            "expected a Modified change, got {changes:?}");
    }

    /// Same fence but different code -> Modified; identical block moved by an
    /// earlier edit -> unchanged (regression coverage for both directions).
    #[test]
    fn code_block_change_vs_move() {
        let old = b"para\n\n```\nbody\n```\n";
        let moved = b"para edit\n\n```\nbody\n```\n";
        let modified = b"para\n\n```\nchanged\n```\n";
        let moved_changes = semantic_diff(old, moved, MarkdownProfile::Gfm);
        assert!(moved_changes.iter().all(|c| !matches!(
            c,
            SemanticChange::Modified { unit: SemanticUnit::CodeBlock }
        )), "moved identical code block must not be Modified: {moved_changes:?}");
        let mod_changes = semantic_diff(old, modified, MarkdownProfile::Gfm);
        assert!(mod_changes.iter().any(|c| matches!(
            c,
            SemanticChange::Modified { unit: SemanticUnit::CodeBlock }
        )), "expected Modified code block: {mod_changes:?}");
    }
}
