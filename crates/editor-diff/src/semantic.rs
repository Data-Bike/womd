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
    diff_documents(&old_doc, &new_doc)
}

fn diff_documents(old: &Document, new: &Document) -> Vec<SemanticChange> {
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
                if !bytes_equal(ob, new_blocks[j]) {
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

fn bytes_equal(a: &Block, b: &Block) -> bool {
    a.span() == b.span() || a == b
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
}
