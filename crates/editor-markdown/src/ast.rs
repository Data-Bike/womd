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
        self.end.0 - self.start.0
    }
}

/// A document: a list of blocks whose spans partition `[0, len)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub span: SourceSpan,
    pub blocks: Vec<Block>,
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
            Self::Text(m, _) | Self::CodeSpan(m, _, _) | Self::Autolink(m, _)
            | Self::HardBreak(m) | Self::RawHtml(m) | Self::UnknownInline(m) => *m,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub meta: NodeMeta,
    pub alt: String,
    pub style: LinkStyle,
    pub destination: String,
    pub title: Option<String>,
    pub reference: Option<String>,
}
