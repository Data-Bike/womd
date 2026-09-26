//! Lossless/source-preserving Markdown AST (ADR-003).
//!
//! Every node stores its exact `SourceSpan` (byte range) into the original source. The
//! serializer emits `source[span]` verbatim for unchanged nodes, so round-tripping an
//! unedited document is byte-identical (Invariant 1) and edits produce minimal diffs
//! (Invariant 2). Unknown extension syntax becomes `UnknownBlock`/`UnknownInline` and is
//! emitted verbatim (Invariant 5).

use editor_domain::{ByteOffset, ByteRange};

/// Half-open byte span `[start, end)` into the original source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    pub start: ByteOffset,
    pub end: ByteOffset,
}

impl SourceSpan {
    pub fn new(start: ByteOffset, end: ByteOffset) -> Self {
        Self { start, end }
    }
    pub fn range(&self) -> ByteRange {
        ByteRange::new(self.start, self.end)
    }
    pub fn len(&self) -> u64 {
        self.end.0.saturating_sub(self.start.0)
    }
}

/// A document: a list of blocks whose spans partition `[0, len)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub span: SourceSpan,
    pub blocks: Vec<Block>,
    /// How far parsing has progressed (for lazy/chunked parsing of large files).
    /// Blocks with `span.end <= parsed_offset` are fully parsed; bytes beyond
    /// `parsed_offset` have not yet been parsed. For fully-parsed documents,
    /// `parsed_offset == span.end.0`.
    pub parsed_offset: u64,
}

impl Document {
    /// Create a document from a pre-parsed list of blocks.
    /// The document span covers `[0, last_block_end)`.
    /// `parsed_offset` is set to the end of the last block.
    pub fn from_blocks(blocks: Vec<Block>) -> Self {
        let end = blocks.last().map(|b| b.meta().span.end.0).unwrap_or(0);
        #[cfg(debug_assertions)]
        {
            let mut prev_end = 0u64;
            for b in &blocks {
                let span = b.meta().span;
                assert!(span.start.0 == prev_end,
                    "from_blocks: non-contiguous blocks at offset {}, expected {}",
                    span.start.0, prev_end);
                assert!(span.end.0 >= span.start.0,
                    "from_blocks: negative span {:?}", span);
                prev_end = span.end.0;
            }
        }
        Self {
            span: SourceSpan::new(ByteOffset(0), ByteOffset(end)),
            blocks,
            parsed_offset: end,
        }
    }

    /// Merge newly-parsed blocks from a subsequent chunk into this document.
    /// New blocks are appended; `parsed_offset` advances to the end of the
    /// last new block. Blocks that overlap with existing blocks (due to
    /// chunk boundary re-alignment) replace the overlapping tail.
    pub fn merge_blocks(&mut self, new_blocks: Vec<Block>) {
        if new_blocks.is_empty() {
            return;
        }
        let new_start = new_blocks[0].meta().span.start.0;

        // Defensive: if the new chunk starts after the last kept block, there
        // is a gap. This should not happen with contiguous chunk parsing, but
        // log a clear invariant violation in debug builds.
        #[cfg(debug_assertions)]
        if let Some(last) = self.blocks.last() {
            assert!(
                last.meta().span.end.0 >= new_start,
                "merge_blocks: gap between existing blocks (end {}) and new chunk (start {})",
                last.meta().span.end.0, new_start
            );
        }

        // Remove existing blocks that overlap with the new chunk.
        // Keep blocks whose span ends at or before new_start.
        let split_point = self
            .blocks
            .iter()
            .position(|b| b.meta().span.end.0 > new_start)
            .unwrap_or(self.blocks.len());
        self.blocks.truncate(split_point);
        // Append new blocks.
        let new_end = new_blocks.last().map(|b| b.meta().span.end.0).unwrap_or(0);
        self.blocks.extend(new_blocks);
        self.parsed_offset = new_end;
        // Update document span.
        self.span.end = ByteOffset(new_end);
    }
}

/// Common fields for every node: source span + dirty flag.
///
/// `dirty` is false on parse. When a user edit modifies a node's content, the editor marks
/// it dirty; the serializer then regenerates that node from its (modified) content +
/// preserved trivia instead of emitting `source[span]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeMeta {
    pub span: SourceSpan,
    pub dirty: bool,
}

/// Block-level nodes (§5–16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Blank line(s) between blocks — semantically significant trivia (§23).
    BlankLine(NodeMeta),
    Paragraph(Paragraph),
    Heading(Heading),
    ThematicBreak(ThematicBreak),
    BlockQuote(BlockQuote),
    List(List),
    CodeBlock(CodeBlock),
    /// GFM table (§16).
    Table(Table),
    HtmlBlock(HtmlBlock),
    /// Link reference definition (§12). May be hidden in WYSIWYG.
    LinkReferenceDefinition(LinkReferenceDefinition),
    /// Extension syntax with no matching plugin — preserved verbatim (Invariant 5).
    UnknownBlock(UnknownBlock),
}

impl Block {
    pub fn meta(&self) -> NodeMeta {
        match self {
            Self::BlankLine(m) | Self::UnknownBlock(UnknownBlock { meta: m, .. }) => *m,
            Self::Paragraph(p) => p.meta,
            Self::Heading(h) => h.meta,
            Self::ThematicBreak(t) => t.meta,
            Self::BlockQuote(b) => b.meta,
            Self::List(l) => l.meta,
            Self::CodeBlock(c) => c.meta,
            Self::Table(t) => t.meta,
            Self::HtmlBlock(h) => h.meta,
            Self::LinkReferenceDefinition(d) => d.meta,
        }
    }
    pub fn span(&self) -> SourceSpan {
        self.meta().span
    }
    pub fn meta_mut(&mut self) -> &mut NodeMeta {
        match self {
            Self::BlankLine(m) | Self::UnknownBlock(UnknownBlock { meta: m, .. }) => m,
            Self::Paragraph(p) => &mut p.meta,
            Self::Heading(h) => &mut h.meta,
            Self::ThematicBreak(t) => &mut t.meta,
            Self::BlockQuote(b) => &mut b.meta,
            Self::List(l) => &mut l.meta,
            Self::CodeBlock(c) => &mut c.meta,
            Self::Table(t) => &mut t.meta,
            Self::HtmlBlock(h) => &mut h.meta,
            Self::LinkReferenceDefinition(d) => &mut d.meta,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paragraph {
    pub meta: NodeMeta,
    /// Parsed inline children (span-partition the paragraph content).
    pub inlines: Vec<Inline>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub meta: NodeMeta,
    pub level: u8,
    /// ATX vs Setext (§5.2–5.3).
    pub style: HeadingStyle,
    /// For ATX: number of `#` markers and whether closing `#`s were present.
    pub atx_open_hashes: u8,
    pub atx_close_hashes: u8,
    /// For Setext: number of underline characters (= or -) in the original source.
    pub setext_underline_len: u8,
    pub inlines: Vec<Inline>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadingStyle {
    Atx,
    Setext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThematicBreak {
    pub meta: NodeMeta,
    /// The marker char used: `-`, `*`, or `_` (§5.4).
    pub marker: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockQuote {
    pub meta: NodeMeta,
    /// Nested blocks (the `> ` markers are part of the source span, preserved verbatim).
    pub children: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List {
    pub meta: NodeMeta,
    pub ordered: bool,
    /// Marker char: `-`, `*`, `+` for unordered; `.` or `)` for ordered (§6.1–6.2).
    pub marker: u8,
    /// Starting number for ordered lists (preserved, e.g. `42.`) (§6.2).
    pub start: u32,
    /// Tight vs loose (§6.4).
    pub tight: bool,
    pub items: Vec<ListItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    pub meta: NodeMeta,
    /// GFM task list state, if `[ ]`/`[x]` present (§7).
    pub task: Option<TaskState>,
    /// Content blocks of the item (§6.5).
    pub children: Vec<Block>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Open,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    pub meta: NodeMeta,
    pub fenced: bool,
    /// Fence char `` ` `` or `~` and fence length (§8.3).
    pub fence_char: u8,
    pub fence_len: u8,
    /// Info string (language) for fenced blocks.
    pub info_string: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub meta: NodeMeta,
    pub alignments: Vec<TableAlign>,
    pub rows: Vec<TableRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAlign {
    Left,
    Center,
    Right,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    pub meta: NodeMeta,
    pub header: bool,
    pub cells: Vec<TableCell>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCell {
    pub meta: NodeMeta,
    pub inlines: Vec<Inline>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlBlock {
    pub meta: NodeMeta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkReferenceDefinition {
    pub meta: NodeMeta,
    pub label: String,
    pub destination: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownBlock {
    pub meta: NodeMeta,
    /// The extension id that would claim this syntax, if known.
    pub extension_id: Option<String>,
}

/// Inline-level nodes (§8.1, §9–15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    /// Raw text run.
    Text(NodeMeta, String),
    /// Emphasis `*x*` / `_x_` (§9).
    Emphasis(NodeMeta, Vec<Inline>, EmphasisKind),
    /// Strong `**x**` / `__x__` (§9).
    Strong(NodeMeta, Vec<Inline>, EmphasisKind),
    /// Strikethrough `~~x~~` (GFM, §10).
    Strikethrough(NodeMeta, Vec<Inline>),
    /// Inline code span (§8.1). Preserves delimiter length.
    CodeSpan(NodeMeta, String, u8),
    /// LaTeX math: \( ... \) for inline, \[ ... \] for display.
    MathSpan(NodeMeta, String, bool),
    /// Link (§11).
    Link(Link),
    /// Image (§13).
    Image(Image),
    /// Autolink `<url>` (§14).
    Autolink(NodeMeta, String),
    /// Hard line break: trailing spaces or backslash (§19).
    HardBreak(NodeMeta),
    /// Raw HTML inline (§21).
    RawHtml(NodeMeta),
    /// Unknown inline extension syntax (Invariant 5).
    UnknownInline(NodeMeta),
}

impl Inline {
    pub fn meta(&self) -> NodeMeta {
        match self {
            Self::Text(m, _) | Self::CodeSpan(m, _, _) | Self::MathSpan(m, _, _)
            | Self::Autolink(m, _) | Self::HardBreak(m) | Self::RawHtml(m) | Self::UnknownInline(m) => *m,
            Self::Emphasis(m, _, _) | Self::Strong(m, _, _) | Self::Strikethrough(m, _) => *m,
            Self::Link(l) => l.meta,
            Self::Image(i) => i.meta,
        }
    }
    pub fn span(&self) -> SourceSpan {
        self.meta().span
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmphasisKind {
    Asterisk,
    Underscore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub meta: NodeMeta,
    pub inlines: Vec<Inline>,
    pub style: LinkStyle,
    pub destination: String,
    pub title: Option<String>,
    /// For reference styles: the reference id/label.
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStyle {
    Inline,
    Reference,
    Collapsed,
    Shortcut,
}

// ---------------------------------------------------------------------------
// Span shifting (incremental reparse support, ADR-003 §58)
// ---------------------------------------------------------------------------

fn shift_offset(off: &mut ByteOffset, delta: i64) {
    *off = ByteOffset((off.0 as i64 + delta).max(0) as u64);
}

fn shift_span(span: &mut SourceSpan, delta: i64) {
    shift_offset(&mut span.start, delta);
    shift_offset(&mut span.end, delta);
}

fn shift_meta(meta: &mut NodeMeta, delta: i64) {
    shift_span(&mut meta.span, delta);
}

/// Shift every *document-coordinate* `SourceSpan` in a top-level block by `delta`.
///
/// Used by `parse_range` to rebase window-relative spans onto absolute document
/// offsets and by `DocumentBuffer`'s incremental reparse to shift trailing blocks
/// after an edit.
///
/// Spans that live in a *de-marked* coordinate space are intentionally left
/// untouched: `BlockQuote::children` and `ListItem::children` are parsed against a
/// marker-stripped buffer, so their offsets are not document coordinates. Only the
/// container's own spans (block meta, list item meta) are document coordinates.
pub fn shift_block_spans(block: &mut Block, delta: i64) {
    match block {
        Block::BlankLine(m)
        | Block::ThematicBreak(ThematicBreak { meta: m, .. })
        | Block::CodeBlock(CodeBlock { meta: m, .. })
        | Block::HtmlBlock(HtmlBlock { meta: m })
        | Block::LinkReferenceDefinition(LinkReferenceDefinition { meta: m, .. })
        | Block::UnknownBlock(UnknownBlock { meta: m, .. }) => shift_meta(m, delta),
        Block::Paragraph(p) => {
            shift_meta(&mut p.meta, delta);
            for i in &mut p.inlines {
                shift_inline_spans(i, delta);
            }
        }
        Block::Heading(h) => {
            shift_meta(&mut h.meta, delta);
            for i in &mut h.inlines {
                shift_inline_spans(i, delta);
            }
        }
        Block::BlockQuote(bq) => {
            // Children spans are de-marked coordinates — shift the container only.
            shift_meta(&mut bq.meta, delta);
        }
        Block::List(l) => {
            shift_meta(&mut l.meta, delta);
            for item in &mut l.items {
                // Item span is a document coordinate; item children are de-marked.
                shift_meta(&mut item.meta, delta);
            }
        }
        Block::Table(t) => {
            shift_meta(&mut t.meta, delta);
            for row in &mut t.rows {
                shift_meta(&mut row.meta, delta);
                for cell in &mut row.cells {
                    shift_meta(&mut cell.meta, delta);
                    for i in &mut cell.inlines {
                        shift_inline_spans(i, delta);
                    }
                }
            }
        }
    }
}

/// Shift an inline node's document-coordinate span (and any nested inline
/// children) by `delta`.
pub fn shift_inline_spans(inline: &mut Inline, delta: i64) {
    match inline {
        Inline::Text(m, _)
        | Inline::CodeSpan(m, _, _)
        | Inline::MathSpan(m, _, _)
        | Inline::Autolink(m, _)
        | Inline::HardBreak(m)
        | Inline::RawHtml(m)
        | Inline::UnknownInline(m) => shift_meta(m, delta),
        Inline::Emphasis(m, children, _)
        | Inline::Strong(m, children, _)
        | Inline::Strikethrough(m, children) => {
            shift_meta(m, delta);
            for c in children {
                shift_inline_spans(c, delta);
            }
        }
        Inline::Link(l) => {
            shift_meta(&mut l.meta, delta);
            for c in &mut l.inlines {
                shift_inline_spans(c, delta);
            }
        }
        Inline::Image(i) => shift_meta(&mut i.meta, delta),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub meta: NodeMeta,
    pub alt: String,
    pub style: LinkStyle,
    pub destination: String,
    pub title: Option<String>,
    pub reference: Option<String>,
}
