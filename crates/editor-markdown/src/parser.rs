//! Lossless Markdown parser (ADR-003).
//!
//! Block parser partitions the document into blocks whose spans exactly cover `[0, len)`
//! (including blank-line trivia), which makes byte-identical round-trip a structural
//! property of the serializer. Inline parser partitions paragraph/heading/cell content into
//! inline nodes for navigation and minimal-diff editing.

use editor_domain::{errors::DocumentError, ByteOffset, MarkdownProfile};

use crate::ast::*;

/// Parse a full document from source bytes under the given profile.
pub fn parse(source: &[u8], profile: MarkdownProfile) -> Result<Document, DocumentError> {
    let len = source.len() as u64;
    // Two-pass: first collect link reference definitions so that reference/collapsed/
    // shortcut links resolve against definitions appearing later in the document (§100).
    let preliminary = parse_block_sequence(source, 0, len, 0, &profile, &Refs::default())?;
    let refs = collect_references(&preliminary);
    let blocks = parse_block_sequence(source, 0, len, 0, &profile, &refs)?;
    Ok(Document { span: SourceSpan::new(ByteOffset(0), ByteOffset(len)), blocks, parsed_offset: len })
}

// ---------------------------------------------------------------------------
// Line helpers
// ---------------------------------------------------------------------------

/// A line: `[start, end)` includes its trailing newline; `content` excludes line ending.
#[derive(Clone, Copy, Debug)]
struct Line {
    start: u64,
    end: u64,
    /// Offset just before the line-ending (`\n` or `\r\n` or EOF).
    content_end: u64,
}

fn collect_lines(bytes: &[u8], start: u64, end: u64) -> Vec<Line> {
    let mut out = Vec::new();
    let mut i = start;
    while i < end {
        let line_start = i;
        // Find the next line terminator. A line ends at `\n`, `\r\n`, or a
        // bare `\r` (CommonMark treats CR as a line ending too — a classic-Mac
        // file must not collapse into a single giant line).
        let mut j = i;
        while j < end && bytes[j as usize] != b'\n' && bytes[j as usize] != b'\r' {
            j += 1;
        }
        let content_end = j;
        let line_end = if j < end {
            if bytes[j as usize] == b'\r' && j + 1 < end && bytes[(j + 1) as usize] == b'\n' {
                j + 2 // CRLF
            } else {
                j + 1 // LF or bare CR
            }
        } else {
            j
        };
        out.push(Line { start: line_start, end: line_end, content_end });
        i = line_end;
    }
    out
}

fn line_content<'a>(bytes: &'a [u8], line: Line) -> &'a [u8] {
    &bytes[line.start as usize..line.content_end as usize]
}

/// Count leading spaces (0..=3 meaningful for most blocks; 4+ => indented code).
fn leading_indent(bytes: &[u8]) -> usize {
    bytes.iter().take_while(|&&b| b == b' ').count()
}

fn is_blank(content: &[u8]) -> bool {
    content.iter().all(|&b| b == b' ' || b == b'\t')
}

// ---------------------------------------------------------------------------
// Byte trim helpers (the `str::trim*` methods are not available on `[u8]`).
// ---------------------------------------------------------------------------

fn trim_start_bytes(s: &[u8]) -> &[u8] {
    let i = s.iter().position(|&b| b != b' ' && b != b'\t').unwrap_or(s.len());
    &s[i..]
}

fn trim_end_bytes(s: &[u8]) -> &[u8] {
    let i = s.iter().rposition(|&b| b != b' ' && b != b'\t').map_or(0, |p| p + 1);
    &s[..i]
}

fn trim_bytes(s: &[u8]) -> &[u8] {
    trim_end_bytes(trim_start_bytes(s))
}

fn trim_matches_bytes(s: &[u8], matcher: impl Fn(u8) -> bool) -> &[u8] {
    let i = s.iter().position(|&b| !matcher(b)).unwrap_or(s.len());
    let j = s[i..].iter().rposition(|&b| !matcher(b)).map_or(i, |p| i + p + 1);
    &s[i..j]
}

// ---------------------------------------------------------------------------
// Block parser
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
/// Public wrapper for `parse_block_sequence` (used by incremental reparse, §58).
pub fn parse_block_sequence_export(
    bytes: &[u8],
    start: u64,
    end: u64,
    base_indent: u32,
    profile: &MarkdownProfile,
) -> Result<Vec<Block>, DocumentError> {
    parse_block_sequence(bytes, start, end, base_indent, profile, &Refs::default())
}

fn parse_block_sequence(
    bytes: &[u8],
    start: u64,
    end: u64,
    base_indent: u32,
    profile: &MarkdownProfile,
    refs: &Refs,
) -> Result<Vec<Block>, DocumentError> {
    let lines = collect_lines(bytes, start, end);
    let mut blocks = Vec::new();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx];
        let content = line_content(bytes, line);
        let indent = leading_indent(content) as u32;
        let stripped = &content[(indent as usize).min(content.len())..];

        // Blank line -> BlankLine trivia (consume consecutive blanks).
        if is_blank(content) {
            let mut j = idx;
            while j < lines.len() && is_blank(line_content(bytes, lines[j])) {
                j += 1;
            }
            let span = SourceSpan::new(ByteOffset(lines[idx].start), ByteOffset(lines[j - 1].end));
            blocks.push(Block::BlankLine(NodeMeta { span, dirty: false }));
            idx = j;
            continue;
        }

        // Indented code block (4+ spaces, not inside list item content area).
        if indent >= 4 + base_indent && stripped.first() != Some(&b'>') {
            let (blk, next) = parse_indented_code(bytes, &lines, idx, base_indent);
            blocks.push(blk);
            idx = next;
            continue;
        }

        // ATX heading.
        if let Some(h) = try_atx_heading(bytes, line, stripped, refs) {
            blocks.push(Block::Heading(h));
            idx += 1;
            continue;
        }

        // Thematic break.
        if is_thematic_break(stripped) {
            let span = SourceSpan::new(ByteOffset(line.start), ByteOffset(line.end));
            let marker = thematic_marker(stripped).unwrap_or(b'-');
            blocks.push(Block::ThematicBreak(ThematicBreak { meta: NodeMeta { span, dirty: false }, marker }));
            idx += 1;
            continue;
        }

        // Fenced code block.
        if let Some((fence_char, fence_len, info)) = parse_fence_open(stripped) {
            let (blk, next) = parse_fenced_code(bytes, &lines, idx, fence_char, fence_len, info);
            blocks.push(blk);
            idx = next;
            continue;
        }

        // Block quote.
        if stripped.first() == Some(&b'>') {
            let (blk, next) = parse_block_quote(bytes, &lines, idx, profile, refs);
            blocks.push(blk);
            idx = next;
            continue;
        }

        // GFM table.
        if profile.is_gfm() && stripped.contains(&b'|') && idx + 1 < lines.len() {
            if let Some((table, next)) = try_parse_table(bytes, &lines, idx, refs) {
                blocks.push(Block::Table(table));
                idx = next;
                continue;
            }
        }

        // Link reference definition.
        if let Some((def, consumed)) = try_link_reference_def(bytes, &lines, idx) {
            blocks.push(Block::LinkReferenceDefinition(def));
            idx += consumed;
            continue;
        }

        // List.
        if let Some(marker) = list_marker(stripped) {
            let (list, next) = parse_list(bytes, &lines, idx, marker, profile, refs);
            blocks.push(Block::List(list));
            idx = next;
            continue;
        }

        // HTML block (simplified: line starts with `<` and a known tag name).
        if stripped.starts_with(b"<") && looks_like_html_block(stripped) {
            let (blk, next) = parse_html_block(bytes, &lines, idx);
            blocks.push(blk);
            idx = next;
            continue;
        }

        // Paragraph (with possible setext heading underline).
        let (blk, next) = parse_paragraph(bytes, &lines, idx, refs);
        blocks.push(blk);
        idx = next;
    }
    Ok(blocks)
}

// ---------------------------------------------------------------------------
// Specific block parsers
// ---------------------------------------------------------------------------

fn try_atx_heading(bytes: &[u8], line: Line, stripped: &[u8], refs: &Refs) -> Option<Heading> {
    let mut hashes = 0usize;
    while stripped.get(hashes) == Some(&b'#') {
        hashes += 1;
    }
    if hashes == 0 || hashes > 6 {
        return None;
    }
    // Must be followed by space or end-of-line.
    if let Some(&c) = stripped.get(hashes) {
        if c != b' ' && c != b'\t' {
            return None;
        }
    }
    // Heading content sits after the opening hashes + following whitespace and
    // before the optional closing-hash run. `content` is a subslice of
    // `stripped`, so the inline parser must be bounded to exactly that region —
    // otherwise the parsed Text nodes absorb the space after `#` and the
    // closing `##` run, and dirty regeneration would emit them twice.
    let after_hashes = &stripped[hashes..];
    let rest = trim_start_bytes(after_hashes);
    let mut close = 0usize;
    let mut content = trim_end_bytes(rest);
    {
        // Count a trailing run of '#' separated from content by space/tab.
        let mut t = content.len();
        while t > 0 && content[t - 1] == b'#' {
            t -= 1;
        }
        if t < content.len() {
            if t == 0 {
                // All hashes, no content.
                close = content.len();
                content = &content[..0];
            } else if content[t - 1] == b' ' || content[t - 1] == b'\t' {
                close = content.len() - t;
                content = trim_end_bytes(&content[..t - 1]);
            }
        }
    }
    let ind = leading_indent(line_content(bytes, line));
    // Offset of `content` inside `stripped` (rest is a suffix of after_hashes,
    // content a prefix of rest).
    let content_rel = hashes + (after_hashes.len() - rest.len());
    let content_start = line.start + ind as u64 + content_rel as u64;
    let content_end = content_start + content.len() as u64;
    let inlines = parse_inlines(bytes, content_start, content_end, refs);
    let span = SourceSpan::new(ByteOffset(line.start), ByteOffset(line.end));
    Some(Heading {
        meta: NodeMeta { span, dirty: false },
        level: hashes as u8,
        style: HeadingStyle::Atx,
        atx_open_hashes: hashes as u8,
        atx_close_hashes: close.min(u8::MAX as usize) as u8,
        setext_underline_len: 0,
        inlines,
    })
}

fn is_thematic_break(s: &[u8]) -> bool {
    let chars: Vec<u8> = s.iter().filter(|&&b| b != b' ' && b != b'\t').copied().collect();
    if chars.is_empty() {
        return false;
    }
    let m = chars[0];
    if m != b'-' && m != b'*' && m != b'_' {
        return false;
    }
    chars.iter().all(|&c| c == m) && chars.len() >= 3
}

fn thematic_marker(s: &[u8]) -> Option<u8> {
    let m = s.iter().find(|&&b| b != b' ' && b != b'\t').copied()?;
    if m == b'-' || m == b'*' || m == b'_' {
        Some(m)
    } else {
        None
    }
}

fn parse_fence_open(s: &[u8]) -> Option<(u8, u8, String)> {
    let &c = s.first()?;
    if c != b'`' && c != b'~' {
        return None;
    }
    let mut n = 0usize;
    while s.get(n) == Some(&c) {
        n += 1;
    }
    if n < 3 {
        return None;
    }
    // Backtick fences must not contain backticks in info string; we don't enforce here.
    let info = String::from_utf8_lossy(trim_start_bytes(&s[n..])).to_string();
    Some((c, n as u8, info))
}

fn parse_fenced_code(
    bytes: &[u8],
    lines: &[Line],
    idx: usize,
    fence_char: u8,
    fence_len: u8,
    info: String,
) -> (Block, usize) {
    let start = lines[idx].start;
    let mut j = idx + 1;
    while j < lines.len() {
        let c = line_content(bytes, lines[j]);
        let ind = leading_indent(c);
        let s = &c[ind.min(c.len())..];
        // Closing fence: a run of >= fence_len identical markers, indented at
        // most 3 spaces, followed only by whitespace. The run length must be
        // measured first — slicing at `fence_len` would include the extra
        // markers of a longer run and reject a valid closer (CommonMark: a
        // longer run still closes). A 4+ space indent is code content, not a
        // closer.
        let run = s.iter().take_while(|&&b| b == fence_char).count();
        if ind <= 3
            && run >= fence_len as usize
            && s[run..].iter().all(|&b| b == b' ' || b == b'\t')
        {
            break;
        }
        j += 1;
    }
    let end_line = if j < lines.len() {
        lines[j].end
    } else if !lines.is_empty() {
        lines[lines.len() - 1].end
    } else {
        start
    };
    let span = SourceSpan::new(ByteOffset(start), ByteOffset(end_line));
    let next = if j < lines.len() { j + 1 } else { lines.len() };
    (
        Block::CodeBlock(CodeBlock {
            meta: NodeMeta { span, dirty: false },
            fenced: true,
            fence_char,
            fence_len,
            info_string: info,
        }),
        next,
    )
}

fn parse_indented_code(bytes: &[u8], lines: &[Line], idx: usize, base_indent: u32) -> (Block, usize) {
    let start = lines[idx].start;
    let mut j = idx;
    while j < lines.len() {
        let c = line_content(bytes, lines[j]);
        if is_blank(c) {
            // blank lines may belong to code block if followed by more indented lines
            // (simplified: keep consuming while next non-blank is indented)
            let mut k = j + 1;
            while k < lines.len() && is_blank(line_content(bytes, lines[k])) {
                k += 1;
            }
            if k < lines.len() && (leading_indent(line_content(bytes, lines[k])) as u32) >= 4 + base_indent {
                j = k;
                continue;
            }
            break;
        }
        if (leading_indent(c) as u32) < 4 + base_indent {
            break;
        }
        j += 1;
    }
    let end = if j > idx { lines[j - 1].end } else { lines[idx].end };
    let span = SourceSpan::new(ByteOffset(start), ByteOffset(end));
    (Block::CodeBlock(CodeBlock { meta: NodeMeta { span, dirty: false }, fenced: false, fence_char: 0, fence_len: 0, info_string: String::new() }), j)
}

/// ATX-heading-like: 1–6 `#` followed by space/tab/EOL. `#x` is NOT a
/// heading — CommonMark requires whitespace after the run.
fn is_atx_like(s: &[u8]) -> bool {
    let mut i = 0usize;
    while i < s.len() && i < 6 && s[i] == b'#' {
        i += 1;
    }
    i > 0 && (i == s.len() || s[i] == b' ' || s[i] == b'\t')
}

/// Does `s` start a block that interrupts a paragraph — i.e. can a
/// `>`-less / unindented line never be a lazy continuation? Setext
/// underlines are deliberately NOT here: they are paragraph continuations.
/// Exported for editor-core's lazy-chunk boundary detection, which applies
/// the same rule when deciding whether a line continues the previous block.
pub fn line_starts_new_block(s: &[u8]) -> bool {
    parse_fence_open(s).is_some()
        || is_atx_like(s)
        || list_marker(s).is_some()
        || (is_thematic_break(s) && !is_setext_underline(s))
        || (s.starts_with(b"<") && looks_like_html_block(s))
}

/// Whether the de-marked content line `s` leaves the deepest open block as a
/// paragraph — i.e. whether a following unmarked line could lazily continue
/// it. Containers (`>`, list markers) defer the answer to their inner
/// content; leaf blocks (fences, headings, thematic breaks, HTML, table
/// delimiter rows, setext underlines) close it.
fn deepest_is_paragraph(s: &[u8], depth: u32) -> bool {
    if depth > 8 {
        return false;
    }
    let ind = leading_indent(s);
    let c = &s[ind.min(s.len())..];
    if c.is_empty() {
        return false;
    }
    // Indented code is a leaf block — its lines can't lazily continue a
    // paragraph that came before them.
    if ind >= 4 {
        return false;
    }
    if c.first() == Some(&b'>') {
        let inner = &c[1..];
        let inner = if inner.first() == Some(&b' ') { &inner[1..] } else { inner };
        return deepest_is_paragraph(inner, depth + 1);
    }
    if let Some(mk) = list_marker(c) {
        let ml = if mk.ordered { count_digits(c) + 1 } else { 1 };
        let inner = &c[ml.min(c.len())..];
        let ws = inner.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
        let inner = &inner[ws.min(inner.len())..];
        let inner = if inner.starts_with(b"[ ] ") || inner.starts_with(b"[x] ") || inner.starts_with(b"[X] ") {
            &inner[4.min(inner.len())..]
        } else {
            inner
        };
        return deepest_is_paragraph(inner, depth + 1);
    }
    if is_atx_like(c) {
        return false;
    }
    if parse_fence_open(c).is_some()
        || is_thematic_break(c)
        || is_setext_underline(c)
        || parse_delim_row(c).is_some()
        || c.first() == Some(&b'|')
        || (c.starts_with(b"<") && looks_like_html_block(c))
    {
        return false;
    }
    true
}

/// Advance a running fence state over one de-marked content line. Returns
/// true when the line is INSIDE (or closes/opens) a fenced code block —
/// i.e. it is never paragraph content regardless of how it looks. An open
/// fence swallows every following line until a matching closer.
fn track_fence(state: &mut Option<(u8, usize)>, line: &[u8]) -> bool {
    if let Some((ch, len)) = *state {
        let ind = leading_indent(line);
        let t = trim_bytes(line);
        // Closer: <=3 indent, only fence chars of the opener's kind, >= len.
        if ind <= 3 && t.len() >= len && t.iter().all(|&b| b == ch) {
            *state = None;
        }
        return true;
    }
    if let Some((ch, len, _info)) = parse_fence_open(line) {
        *state = Some((ch, usize::from(len)));
        return true;
    }
    false
}

fn parse_block_quote(bytes: &[u8], lines: &[Line], idx: usize, profile: &MarkdownProfile, refs: &Refs) -> (Block, usize) {
    let start = lines[idx].start;
    let mut j = idx;
    let mut inner_end = start;
    // `para_open`: the deepest de-marked block is still a paragraph — only
    // then can an unmarked line lazily continue the quote. `fence` tracks an
    // open fenced code block inside the de-marked content — its body looks
    // like paragraph text but can never be lazily continued.
    let mut para_open = false;
    let mut fence: Option<(u8, usize)> = None;
    while j < lines.len() {
        let c = line_content(bytes, lines[j]);
        let ind = leading_indent(c);
        let s = &c[ind.min(c.len())..];
        if s.first() != Some(&b'>') {
            if is_blank(c) {
                break;
            }
            // A 4+-indented unmarked line can only join the quote as a lazy
            // paragraph continuation (indented code never interrupts a
            // paragraph); without an open paragraph it ends the quote and is
            // indented code outside. Under-indented lines join only an open
            // paragraph and only when they don't start a new block (setext
            // underlines DO continue — `> a\n---` is an h2 inside).
            if ind >= 4 {
                if !para_open {
                    break;
                }
            } else if !para_open || line_starts_new_block(s) {
                break;
            }
            if ind < 4 {
                para_open = !track_fence(&mut fence, s) && deepest_is_paragraph(s, 0);
            }
            inner_end = lines[j].end;
            j += 1;
            continue;
        }
        // `>`-marked line: update para_open from its de-marked content. A
        // blank de-marked line (`>` alone) closes the inner paragraph;
        // inside an open fence every line is fence content; a 4+-indented
        // line leaves para_open unchanged (para continuation or code leaf).
        let after_gt = &s[1..];
        let after_sp = if after_gt.first() == Some(&b' ') { &after_gt[1..] } else { after_gt };
        if track_fence(&mut fence, after_sp) || is_blank(after_sp) {
            para_open = false;
        } else if leading_indent(after_sp) < 4 {
            para_open = deepest_is_paragraph(after_sp, 0);
        }
        inner_end = lines[j].end;
        j += 1;
    }
    let span = SourceSpan::new(ByteOffset(start), ByteOffset(inner_end));
    // Build a de-marked buffer: strip leading `>` and one optional space per quote line.
    // Children spans are relative to this de-marked buffer (offset 0 = first byte after
    // stripping). The serializer emits the parent span verbatim for non-dirty nodes, so
    // round-trip is byte-identical regardless of children span offsets.
    let mut de_marked = Vec::new();
    for k in idx..j {
        let c = line_content(bytes, lines[k]);
        let ind = leading_indent(c);
        let s = &c[ind.min(c.len())..];
        let out_line = if s.first() == Some(&b'>') {
            // Strip leading `>` and one optional space after it.
            let after_gt = &s[1..];
            if after_gt.first() == Some(&b' ') { &after_gt[1..] } else { after_gt }
        } else {
            // Lazy continuation line: keep verbatim including its indent —
            // the indent is content (`> a\n    ---` is paragraph text inside
            // the quote; stripping it would fabricate a setext underline).
            c
        };
        de_marked.extend_from_slice(out_line);
        // Preserve line ending from the original line.
        de_marked.extend_from_slice(&bytes[lines[k].content_end as usize..lines[k].end as usize]);
    }
    let children = if de_marked.is_empty() {
        Vec::new()
    } else {
        // Recursively parse the de-marked content. Errors are non-fatal: empty children
        // preserve round-trip via the parent span.
        parse_block_sequence(&de_marked, 0, de_marked.len() as u64, 0, profile, refs).unwrap_or_default()
    };
    (Block::BlockQuote(BlockQuote { meta: NodeMeta { span, dirty: false }, children }), j)
}

fn list_marker(s: &[u8]) -> Option<ListMarker> {
    if s.is_empty() {
        return None;
    }
    let c = s[0];
    if c == b'-' || c == b'*' || c == b'+' {
        // must be followed by space or end
        if s.len() == 1 || s[1] == b' ' || s[1] == b'\t' {
            return Some(ListMarker { ordered: false, marker: c, start: 1 });
        }
        return None;
    }
    // ordered
    let mut digits = 0usize;
    while s.get(digits).map_or(false, |b| b.is_ascii_digit()) {
        digits += 1;
    }
    if digits == 0 || digits > 9 {
        return None;
    }
    let after = s.get(digits)?;
    if after != &b'.' && after != &b')' {
        return None;
    }
    if s.len() > digits + 1 && s[digits + 1] != b' ' && s[digits + 1] != b'\t' {
        return None;
    }
    let start: u32 = String::from_utf8_lossy(&s[..digits]).parse().unwrap_or(1);
    Some(ListMarker { ordered: true, marker: *after, start })
}

#[derive(Clone, Copy)]
struct ListMarker {
    ordered: bool,
    marker: u8,
    start: u32,
}

fn parse_list(bytes: &[u8], lines: &[Line], idx: usize, m: ListMarker, profile: &MarkdownProfile, refs: &Refs) -> (List, usize) {
    let start = lines[idx].start;
    let mut j = idx;
    let mut items: Vec<ListItem> = Vec::new();
    let mut tight = true;
    while j < lines.len() {
        let c = line_content(bytes, lines[j]);
        if is_blank(c) {
            // peek ahead: if next non-blank is a list marker of same kind, loose
            let mut k = j + 1;
            while k < lines.len() && is_blank(line_content(bytes, lines[k])) {
                k += 1;
            }
            if k < lines.len() {
                let nc = line_content(bytes, lines[k]);
                let ns = &nc[leading_indent(nc).min(nc.len())..];
                if let Some(nm) = list_marker(ns) {
                    if nm.ordered == m.ordered && nm.marker == m.marker {
                        tight = false;
                    }
                }
            }
            j += 1;
            continue;
        }
        let ind = leading_indent(c);
        let s = &c[ind.min(c.len())..];
        if let Some(nm) = list_marker(s) {
            if nm.ordered != m.ordered || nm.marker != m.marker {
                break;
            }
            // item start
            let item_start = lines[j].start;
            // determine content start after marker
            let marker_len = if nm.ordered { count_digits(s) + 1 } else { 1 };
            let after_marker = &s[marker_len.min(s.len())..];
            let content_indent = marker_len + after_marker.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
            // The first line's content (after marker + checkbox) decides the
            // initial lazy-continuation state: only an open paragraph can be
            // lazily continued by an under-indented line.
            let first_content = trim_start_bytes(after_marker);
            let first_content = if task_like(first_content) {
                trim_start_bytes(&first_content[4.min(first_content.len())..])
            } else {
                first_content
            };
            let mut fence: Option<(u8, usize)> = None;
            let mut para_open = !track_fence(&mut fence, first_content)
                && !is_blank(first_content)
                && deepest_is_paragraph(first_content, 0);
            let mut prev_blank = false;
            let mut k = j + 1;
            while k < lines.len() {
                let nc = line_content(bytes, lines[k]);
                if is_blank(nc) {
                    prev_blank = true;
                    k += 1;
                    continue;
                }
                let nind = leading_indent(nc);
                if nind < content_indent {
                    // could be a new marker of same list or end
                    let ns2 = &nc[nind.min(nc.len())..];
                    if let Some(nm2) = list_marker(ns2) {
                        if nm2.ordered == m.ordered && nm2.marker == m.marker {
                            break;
                        }
                    }
                    // Lazy continuation: `- a\nlazy` keeps `lazy` inside the
                    // item's paragraph — but only an OPEN paragraph can be
                    // continued (a fence/table/etc. swallows nothing), the
                    // line must directly follow content (no blank between),
                    // and it must not start a new block itself. A 4+-indented
                    // line can never start a block while under-indented for
                    // the item — it's always paragraph continuation.
                    if !prev_blank && para_open && (nind >= 4 || !line_starts_new_block(ns2)) {
                        if nind < 4 {
                            para_open = !track_fence(&mut fence, ns2)
                                && deepest_is_paragraph(ns2, 0);
                        }
                        prev_blank = false;
                        k += 1;
                        continue;
                    }
                    break;
                }
                // Indented continuation line: track para_open from its
                // de-marked content (list items may contain blocks of their
                // own — after a leaf block a later unmarked line can no
                // longer lazily continue).
                let strip = (ind as usize) + content_indent;
                let dc = &nc[strip.min(nc.len())..];
                para_open = !track_fence(&mut fence, dc)
                    && !is_blank(dc)
                    && deepest_is_paragraph(dc, 0);
                prev_blank = false;
                k += 1;
            }
            let item_end_line = if k > j { k - 1 } else { j };
            // trim trailing blank lines from item span
            let mut last = item_end_line;
            while last > j && is_blank(line_content(bytes, lines[last])) {
                last -= 1;
            }
            let item_span = SourceSpan::new(ByteOffset(item_start), ByteOffset(lines[last].end));
            // task list?
            let after = &s[marker_len.min(s.len())..];
            let after_trim = trim_start_bytes(after);
            let task = if after_trim.starts_with(b"[ ] ") {
                Some(TaskState::Open)
            } else if after_trim.starts_with(b"[x] ") || after_trim.starts_with(b"[X] ") {
                Some(TaskState::Done)
            } else {
                None
            };
            // Build a de-marked buffer for the item's children: strip the marker + content
            // indent from the first line, and strip `content_indent` from continuation lines.
            // Children spans are relative to this de-marked buffer. The serializer emits the
            // item span verbatim for non-dirty nodes, so round-trip is byte-identical.
            let mut de_marked = Vec::new();
            for line_idx in j..=last {
                let lc = line_content(bytes, lines[line_idx]);
                if line_idx == j {
                    // First line: skip marker + trailing spaces after marker.
                    let mut skip = (ind as usize) + marker_len + after_marker.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
                    // A task checkbox is re-emitted from `item.task` — keep it out
                    // of the children's buffer or it would render/serialize twice.
                    if task.is_some() {
                        skip += 4; // "[ ] " / "[x] " / "[X] "
                    }
                    de_marked.extend_from_slice(&lc[skip.min(lc.len())..]);
                } else {
                    // Continuation: strip content_indent, but never more than
                    // the line's own indent — lazy continuation lines carry
                    // less indent and must be kept verbatim, not truncated.
                    let strip = ((ind as usize) + content_indent).min(leading_indent(lc));
                    de_marked.extend_from_slice(&lc[strip.min(lc.len())..]);
                }
                de_marked.extend_from_slice(&bytes[lines[line_idx].content_end as usize..lines[line_idx].end as usize]);
            }
            let children = if de_marked.is_empty() {
                Vec::new()
            } else {
                parse_block_sequence(&de_marked, 0, de_marked.len() as u64, 0, profile, refs).unwrap_or_default()
            };
            items.push(ListItem { meta: NodeMeta { span: item_span, dirty: false }, task, children });
            j = k;
        } else {
            // not a marker; if indented enough it's continuation, else stop
            if ind >= 2 {
                j += 1;
                continue;
            }
            break;
        }
    }
    let end = if j > idx { lines[j - 1].end } else { lines[idx].end };
    // trim trailing blanks for list span
    let span = SourceSpan::new(ByteOffset(start), ByteOffset(end));
    (List { meta: NodeMeta { span, dirty: false }, ordered: m.ordered, marker: m.marker, start: m.start, tight, items }, j)
}

fn count_digits(s: &[u8]) -> usize {
    s.iter().take_while(|b| b.is_ascii_digit()).count()
}

/// Task-list checkbox prefix: `[ ] `, `[x] `, or `[X] `.
fn task_like(s: &[u8]) -> bool {
    s.starts_with(b"[ ] ") || s.starts_with(b"[x] ") || s.starts_with(b"[X] ")
}

fn try_link_reference_def(bytes: &[u8], lines: &[Line], idx: usize) -> Option<(LinkReferenceDefinition, usize)> {
    let c = line_content(bytes, lines[idx]);
    let s = &c[leading_indent(c).min(c.len())..];
    if !s.starts_with(b"[") {
        return None;
    }
    let close = s.iter().position(|&b| b == b']')?;
    if s.get(close + 1) != Some(&b':') {
        return None;
    }
    let label = String::from_utf8_lossy(&s[1..close]).trim().to_string();
    if label.is_empty() {
        // CommonMark requires a non-empty label; `[]:` is a paragraph.
        return None;
    }
    let rest = trim_start_bytes(&s[close + 2..]);
    // destination: either `<...>` (may contain spaces) or a bare token that
    // runs to the next whitespace.
    let (destination, dest_end) = if rest.first() == Some(&b'<') {
        match rest.iter().position(|&b| b == b'>') {
            Some(gt) => (String::from_utf8_lossy(&rest[1..gt]).to_string(), gt + 1),
            // Unclosed `<` — fall back to token parsing so the definition
            // still round-trips rather than being dropped.
            None => (
                String::from_utf8_lossy(rest).to_string(),
                rest.len(),
            ),
        }
    } else {
        let end = rest
            .iter()
            .position(|&b| b == b' ' || b == b'\t')
            .unwrap_or(rest.len());
        (String::from_utf8_lossy(&rest[..end]).to_string(), end)
    };
    let title = if dest_end < rest.len() {
        let t = trim_bytes(&rest[dest_end..]);
        if t.is_empty() {
            None
        } else {
            // Strip surrounding quotes if present (CommonMark allows "..." or '...').
            let stripped = if t.len() >= 2 && ((t[0] == b'"' && t[t.len() - 1] == b'"') || (t[0] == b'\'' && t[t.len() - 1] == b'\'')) {
                &t[1..t.len() - 1]
            } else {
                t
            };
            Some(String::from_utf8_lossy(stripped).to_string())
        }
    } else {
        None
    };
    let span = SourceSpan::new(ByteOffset(lines[idx].start), ByteOffset(lines[idx].end));
    Some((
        LinkReferenceDefinition { meta: NodeMeta { span, dirty: false }, label, destination, title },
        1,
    ))
}

fn looks_like_html_block(s: &[u8]) -> bool {
    // Simplified: starts with `<` followed by a letter, `/`, `!`, or `?`.
    match s.get(1) {
        Some(b) => b.is_ascii_alphabetic() || *b == b'/' || *b == b'!' || *b == b'?',
        None => false,
    }
}

fn parse_html_block(bytes: &[u8], lines: &[Line], idx: usize) -> (Block, usize) {
    let start = lines[idx].start;
    // Simplified: consume until blank line.
    let mut j = idx + 1;
    while j < lines.len() && !is_blank(line_content(bytes, lines[j])) {
        j += 1;
    }
    let end = lines[j - 1].end;
    let span = SourceSpan::new(ByteOffset(start), ByteOffset(end));
    (Block::HtmlBlock(HtmlBlock { meta: NodeMeta { span, dirty: false } }), j)
}

fn parse_paragraph(bytes: &[u8], lines: &[Line], idx: usize, refs: &Refs) -> (Block, usize) {
    let start = lines[idx].start;
    let mut j = idx;
    let mut setext_level: Option<u8> = None;
    let mut setext_underline_len: u8 = 3;
    loop {
        let c = line_content(bytes, lines[j]);
        if is_blank(c) {
            break;
        }
        let ind = leading_indent(c);
        let s = &c[ind.min(c.len())..];
        // Setext underline? Up to 3 leading spaces allowed — a 4+-indented
        // `---`/`===` line is paragraph continuation text (indented code
        // can't interrupt a paragraph), not an underline.
        if j > idx && ind < 4 && is_setext_underline(s) {
            setext_level = Some(if s[0] == b'=' { 1 } else { 2 });
            setext_underline_len = s.iter().filter(|&&b| b == b'=' || b == b'-').count().min(u8::MAX as usize) as u8;
            j += 1;
            break;
        }
        // Stop if a new block starts (fence, atx, thematic, block quote, list,
        // html). Block starts need indent < 4 too — `    ---`/`    # h`
        // continue the paragraph rather than starting a block.
        if j > idx && ind < 4 {
            if is_thematic_break(s) || parse_fence_open(s).is_some() || try_atx_heading(bytes, lines[j], s, refs).is_some() || s.first() == Some(&b'>') || list_marker(s).is_some() || (s.starts_with(b"<") && looks_like_html_block(s)) {
                break;
            }
        }
        j += 1;
        if j >= lines.len() {
            break;
        }
    }
    let last = if j > idx { j - 1 } else { idx };
    let content_end = if setext_level.is_some() { lines[last - 1].content_end } else { lines[last].content_end };
    let para_end = lines[j - 1].end;
    let span = SourceSpan::new(ByteOffset(start), ByteOffset(para_end));
    let inlines = parse_inlines(bytes, start, content_end, refs);
    if let Some(level) = setext_level {
        return (
            Block::Heading(Heading {
                meta: NodeMeta { span, dirty: false },
                level,
                style: HeadingStyle::Setext,
                atx_open_hashes: 0,
                atx_close_hashes: 0,
                setext_underline_len,
                inlines,
            }),
            j,
        );
    }
    (Block::Paragraph(Paragraph { meta: NodeMeta { span, dirty: false }, inlines }), j)
}

fn is_setext_underline(s: &[u8]) -> bool {
    let t = trim_end_bytes(s);
    if t.is_empty() {
        return false;
    }
    let c = t[0];
    if c != b'=' && c != b'-' {
        return false;
    }
    t.iter().all(|&b| b == c) && t.len() >= 1
}

// ---------------------------------------------------------------------------
// GFM table
// ---------------------------------------------------------------------------

fn try_parse_table(bytes: &[u8], lines: &[Line], idx: usize, refs: &Refs) -> Option<(Table, usize)> {
    let header_line = line_content(bytes, lines[idx]);
    let hindent = leading_indent(header_line).min(header_line.len());
    let hs = &header_line[hindent..];
    // Base offset of hs within bytes (for converting relative cell offsets to absolute).
    let hbase = lines[idx].start as usize + hindent;
    let delim_line = line_content(bytes, lines[idx + 1]);
    let ds = &delim_line[leading_indent(delim_line).min(delim_line.len())..];
    if !ds.contains(&b'|') && !ds.contains(&b':') && !ds.contains(&b'-') {
        return None;
    }
    let aligns = parse_delim_row(ds)?;
    let header_cells = split_table_cells(hs);
    if aligns.len() != header_cells.len() && !header_cells.is_empty() {
        // GFM allows mismatch but require at least a valid delimiter
    }
    let mut rows = Vec::new();
    let hspan = SourceSpan::new(ByteOffset(lines[idx].start), ByteOffset(lines[idx].end));
    let hcells: Vec<TableCell> = header_cells
        .iter()
        .map(|(s, e)| {
            let abs_s = *s + hbase as u64;
            let abs_e = *e + hbase as u64;
            TableCell {
                meta: NodeMeta { span: SourceSpan::new(ByteOffset(abs_s), ByteOffset(abs_e)), dirty: false },
                inlines: parse_inlines(bytes, abs_s, abs_e, refs),
            }
        })
        .collect();
    rows.push(TableRow { meta: NodeMeta { span: hspan, dirty: false }, header: true, cells: hcells });
    let mut j = idx + 2;
    while j < lines.len() {
        let c = line_content(bytes, lines[j]);
        if is_blank(c) {
            break;
        }
        let cindent = leading_indent(c).min(c.len());
        let cs = &c[cindent..];
        if !cs.contains(&b'|') {
            break;
        }
        // Base offset of cs within bytes.
        let cbase = lines[j].start as usize + cindent;
        let cells = split_table_cells(cs);
        let rspan = SourceSpan::new(ByteOffset(lines[j].start), ByteOffset(lines[j].end));
        let rcells: Vec<TableCell> = cells
            .iter()
            .map(|(s, e)| {
                let abs_s = *s + cbase as u64;
                let abs_e = *e + cbase as u64;
                TableCell {
                    meta: NodeMeta { span: SourceSpan::new(ByteOffset(abs_s), ByteOffset(abs_e)), dirty: false },
                    inlines: parse_inlines(bytes, abs_s, abs_e, refs),
                }
            })
            .collect();
        rows.push(TableRow { meta: NodeMeta { span: rspan, dirty: false }, header: false, cells: rcells });
        j += 1;
    }
    let end = if j > idx + 1 { lines[j - 1].end } else { lines[idx + 1].end };
    let span = SourceSpan::new(ByteOffset(lines[idx].start), ByteOffset(end));
    Some((Table { meta: NodeMeta { span, dirty: false }, alignments: aligns, rows }, j))
}

fn parse_delim_row(s: &[u8]) -> Option<Vec<TableAlign>> {
    let cells: Vec<&[u8]> = s.split(|&b| b == b'|').collect();
    let mut aligns = Vec::new();
    for cell in cells {
        let t = trim_matches_bytes(cell, |b: u8| b == b' ' || b == b'\t');
        if t.is_empty() {
            continue;
        }
        if !t.iter().all(|&b| b == b'-' || b == b':') || !t.contains(&b'-') {
            return None;
        }
        let left = t.starts_with(b":");
        let right = t.ends_with(b":");
        aligns.push(match (left, right) {
            (true, true) => TableAlign::Center,
            (true, false) => TableAlign::Left,
            (false, true) => TableAlign::Right,
            (false, false) => TableAlign::None,
        });
    }
    if aligns.is_empty() {
        None
    } else {
        Some(aligns)
    }
}

/// Split a table row into (start,end) byte offsets of cell contents (relative to `s`).
/// Callers add the document offset of `s` to convert to absolute offsets.
fn split_table_cells(s: &[u8]) -> Vec<(u64, u64)> {
    // Trim trailing whitespace/CR so a trailing pipe is detected correctly.
    let s = trim_end_bytes(s);
    let mut cells = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    // Skip an optional leading pipe.
    if s.first() == Some(&b'|') {
        start = 1;
        i = 1;
    }
    // If there is a trailing pipe, the last cell ends before it; otherwise the last
    // cell runs to the end of the line.
    let trailing_pipe = s.last() == Some(&b'|');
    let end_limit = if trailing_pipe { s.len().saturating_sub(1) } else { s.len() };
    while i < end_limit {
        let b = s[i];
        if b == b'\\' && i + 1 < s.len() {
            i += 2;
            continue;
        }
        if b == b'|' {
            cells.push((start as u64, i as u64));
            start = i + 1;
        }
        i += 1;
    }
    // Push the final cell only if it has content or there was a previous cell.
    // (Avoids a spurious empty cell after a trailing pipe.)
    if start < end_limit || cells.is_empty() {
        // Clamp: for a degenerate row like "|" the leading pipe consumed
        // `start` past `end_limit` — an inverted span would panic downstream
        // when used as a `bytes[start..end]` slice.
        cells.push((start.min(end_limit) as u64, end_limit as u64));
    }
    cells
}

// ---------------------------------------------------------------------------
// Inline parser (simplified, span-partitioning)
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Refs {
    // label -> (destination, title)
    map: std::collections::HashMap<String, (String, Option<String>)>,
}

/// Parse inline content in `bytes[start..end)` into a list of inline nodes whose spans
/// partition the range. For the foundation this handles the common cases; full CommonMark
/// delimiter-run nuance is a follow-up (ADR-003).
fn parse_inlines(bytes: &[u8], start: u64, end: u64, refs: &Refs) -> Vec<Inline> {
    let region = &bytes[start as usize..end as usize];
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut text_start = 0usize;
    let abs = |off: usize| ByteOffset(start + off as u64);

    let flush_text = |out: &mut Vec<Inline>, ts: usize, te: usize| {
        if te > ts {
            let span = SourceSpan::new(abs(ts), abs(te));
            let s = String::from_utf8_lossy(&region[ts..te]).to_string();
            out.push(Inline::Text(NodeMeta { span, dirty: false }, s));
        }
    };

    while i < region.len() {
        let b = region[i];

        // LaTeX math: \( ... \) and \[ ... \].
        if b == b'\\' && i + 1 < region.len() {
            let open_br = region[i + 1];
            if open_br == b'(' || open_br == b'[' {
                let close_br = if open_br == b'(' { b')' } else { b']' };
                if let Some(close) = find_math_close(&region[i + 2..], close_br) {
                    flush_text(&mut out, text_start, i);
                    let math_start = i + 2;
                    let math_end = i + 2 + close;
                    let span = SourceSpan::new(abs(i), abs(math_end + 2));
                    let math = String::from_utf8_lossy(&region[math_start..math_end]).to_string();
                    let display = open_br == b'[';
                    out.push(Inline::MathSpan(NodeMeta { span, dirty: false }, math, display));
                    i = math_end + 2;
                    text_start = i;
                    continue;
                }
            }
        }

        // Escape.
        if b == b'\\' && i + 1 < region.len() && is_ascii_punct(region[i + 1]) {
            flush_text(&mut out, text_start, i);
            // Text nodes store raw source bytes — keep the backslash so dirty
            // regeneration emits `\*`, not a bare `*` (which would silently
            // change emphasis semantics).
            let span = SourceSpan::new(abs(i), abs(i + 2));
            out.push(Inline::Text(NodeMeta { span, dirty: false }, String::from_utf8_lossy(&region[i..i + 2]).to_string()));
            i += 2;
            text_start = i;
            continue;
        }

        // Hard line break via backslash before newline (§19).
        if b == b'\\' && i + 1 < region.len() && region[i + 1] == b'\n' {
            flush_text(&mut out, text_start, i);
            let span = SourceSpan::new(abs(i), abs(i + 2));
            out.push(Inline::HardBreak(NodeMeta { span, dirty: false }));
            i += 2;
            text_start = i;
            continue;
        }

        // Hard line break: two+ trailing spaces before newline.
        if b == b' ' {
            // count trailing spaces
            let mut k = i;
            while k < region.len() && region[k] == b' ' {
                k += 1;
            }
            if k > i + 1 && k < region.len() && region[k] == b'\n' {
                flush_text(&mut out, text_start, i);
                let span = SourceSpan::new(abs(i), abs(k + 1));
                out.push(Inline::HardBreak(NodeMeta { span, dirty: false }));
                i = k + 1;
                text_start = i;
                continue;
            }
        }

        // Inline code span.
        if b == b'`' {
            let mut n = 0usize;
            while region.get(i + n) == Some(&b'`') {
                n += 1;
            }
            let after = &region[i + n..];
            if let Some(cl) = find_code_close(after, n) {
                let code_end = i + n + cl + n;
                flush_text(&mut out, text_start, i);
                let span = SourceSpan::new(abs(i), abs(code_end));
                let inner = String::from_utf8_lossy(&region[i + n..i + n + cl]).to_string();
                out.push(Inline::CodeSpan(NodeMeta { span, dirty: false }, inner, n as u8));
                i = code_end;
                text_start = i;
                continue;
            }
        }

        // Emphasis / strong with `*` or `_`.
        if b == b'*' || b == b'_' {
            let mut n = 0usize;
            while region.get(i + n) == Some(&b) {
                n += 1;
            }
            // Left-flanking: a delimiter run can't open emphasis when the byte
            // after the run is whitespace (`a * b` keeps a literal `*`).
            let next_ok = region.get(i + n).map_or(false, |&nb| !nb.is_ascii_whitespace());
            // `_` cannot open inside a word (`snake_case` stays literal);
            // `*` may open intraword (`a*b*` is valid emphasis).
            let prev_ok = i == 0 || region[i - 1].is_ascii_whitespace() || is_ascii_punct(region[i - 1]);
            let can_open = next_ok && (b == b'*' || prev_ok);
            if can_open {
                if let Some(em) = try_emphasis(&region, i, n, b, start, refs) {
                    flush_text(&mut out, text_start, i);
                    out.push(em.node);
                    i = em.end;
                    text_start = i;
                    continue;
                }
            }
        }

        // GFM strikethrough.
        if b == b'~' && region.get(i + 1) == Some(&b'~') {
            if let Some(close) = find_str(&region[i + 2..], b"~~") {
                flush_text(&mut out, text_start, i);
                let inner_start = i + 2;
                let inner_end = i + 2 + close;
                let span = SourceSpan::new(abs(i), abs(inner_end + 2));
                let children = parse_inlines(bytes, start + inner_start as u64, start + inner_end as u64, refs);
                out.push(Inline::Strikethrough(NodeMeta { span, dirty: false }, children));
                i = inner_end + 2;
                text_start = i;
                continue;
            }
        }

        // Image.
        if b == b'!' && region.get(i + 1) == Some(&b'[') {
            if let Some(img) = try_image(&region, i, start, refs) {
                flush_text(&mut out, text_start, i);
                out.push(img.node);
                i = img.end;
                text_start = i;
                continue;
            }
        }

        // Link.
        if b == b'[' {
            if let Some(link) = try_link(&region, i, start, refs) {
                flush_text(&mut out, text_start, i);
                out.push(link.node);
                i = link.end;
                text_start = i;
                continue;
            }
        }

        // Autolink.
        if b == b'<' {
            if let Some(end_off) = find_autolink_end(&region[i..]) {
                flush_text(&mut out, text_start, i);
                let span = SourceSpan::new(abs(i), abs(i + end_off));
                let inner = String::from_utf8_lossy(&region[i + 1..i + end_off - 1]).to_string();
                out.push(Inline::Autolink(NodeMeta { span, dirty: false }, inner));
                i += end_off;
                text_start = i;
                continue;
            }
        }

        i += 1;
    }
    flush_text(&mut out, text_start, region.len());
    out
}

fn is_ascii_punct(b: u8) -> bool {
    matches!(b, b'!'..=b'/' | b':'..=b'@' | b'['..=b'`' | b'{'..=b'~')
}

fn find_code_close(region: &[u8], n: usize) -> Option<usize> {
    let mut i = 0usize;
    while i < region.len() {
        if region[i] == b'`' {
            let mut k = 0usize;
            while region.get(i + k) == Some(&b'`') {
                k += 1;
            }
            if k == n {
                return Some(i);
            }
            i += k;
        } else {
            i += 1;
        }
    }
    None
}

fn find_str(region: &[u8], needle: &[u8]) -> Option<usize> {
    region.windows(needle.len()).position(|w| w == needle)
}

/// Find the first un-escaped `\)` or `\]` closing a LaTeX math span.
fn find_math_close(region: &[u8], close_br: u8) -> Option<usize> {
    let mut i = 0usize;
    while i + 1 < region.len() {
        if region[i] == b'\\' && region[i + 1] == close_br {
            return Some(i);
        }
        i += 1;
    }
    None
}

struct Parsed {
    node: Inline,
    end: usize,
}

/// Scan `after` (the region following an opening delimiter run) for a run of
/// `ch` that may close it. A closer must be right-flanking (preceded by a
/// non-whitespace byte) — otherwise `a *b c*` would close the opener on a run
/// that is actually a later opener. Runs that could open a NESTED emphasis
/// are tracked as `depth`, so `*a *b* c*` pairs its outer delimiters instead
/// of closing on the inner run. `_` delimiters additionally cannot be
/// intraword: a closing `_` must be followed by whitespace, punctuation, or
/// the end of the region (`_foo_bar` is not emphasis).
fn find_delim_close(after: &[u8], ch: u8, need: usize) -> Option<usize> {
    let strict = ch == b'_';
    // Pending inner opener runs (lengths). A closer run first satisfies the
    // most recent inner opener(s); any leftover length may then close us —
    // e.g. in `**bold *em***` the `***` run gives 1 char to the inner `*`
    // close and the remaining `**` to the outer strong.
    let mut stack: Vec<usize> = Vec::new();
    let mut p = 0usize;
    while p < after.len() {
        if after[p] != ch {
            p += 1;
            continue;
        }
        let mut k = 0usize;
        while after.get(p + k) == Some(&ch) {
            k += 1;
        }
        // p == 0 means the run directly follows the opening run — the byte
        // before it is the opener's last delimiter (non-whitespace).
        let prev_non_ws = p == 0 || !after[p - 1].is_ascii_whitespace();
        let next = after.get(p + k).copied();
        let left_flank = next.map_or(false, |b| !b.is_ascii_whitespace());
        let closer_ok = prev_non_ws
            && (!strict || next.map_or(true, |b| b.is_ascii_whitespace() || is_ascii_punct(b)));
        if closer_ok {
            let mut rem = k;
            while let Some(&top) = stack.last() {
                if top <= rem {
                    rem -= top;
                    stack.pop();
                } else {
                    break;
                }
            }
            if rem >= need && stack.is_empty() {
                // The inner closers consumed `k - rem` leading chars of the
                // run; the outer close starts at the remaining `rem` chars.
                return Some(p + (k - rem));
            }
            p += k;
            continue;
        }
        // `_` openers can't be intraword either (roughly: the byte before the
        // run must be whitespace or punctuation).
        let open_ok = !strict
            || p == 0
            || after[p - 1].is_ascii_whitespace()
            || is_ascii_punct(after[p - 1]);
        if left_flank && open_ok {
            stack.push(k);
        }
        p += k;
    }
    None
}

fn try_emphasis(region: &[u8], i: usize, n: usize, ch: u8, base: u64, refs: &Refs) -> Option<Parsed> {
    // Strong if n >= 2 and a closing run of >=2 exists; else emphasis if n >= 1.
    let kind = if ch == b'*' { EmphasisKind::Asterisk } else { EmphasisKind::Underscore };
    let after = &region[i + n..];
    // Try strong (consume 2 delimiters).
    if n >= 2 {
        if let Some(close) = find_delim_close(after, ch, 2) {
            let inner_start = i + 2;
            let inner_end = i + 2 + close;
            let span = SourceSpan::new(ByteOffset(base + i as u64), ByteOffset(base + (inner_end + 2) as u64));
            let children = parse_inlines(region, inner_start as u64, inner_end as u64, refs);
            // Adjust children spans to absolute using base offset of region start (0 here).
            let children = rebase_inlines(children, base as i64);
            return Some(Parsed { node: Inline::Strong(NodeMeta { span, dirty: false }, children, kind), end: inner_end + 2 });
        }
    }
    // Emphasis with 1 delimiter.
    if let Some(close) = find_delim_close(after, ch, 1) {
        let inner_start = i + 1;
        let inner_end = i + 1 + close;
        let span = SourceSpan::new(ByteOffset(base + i as u64), ByteOffset(base + (inner_end + 1) as u64));
        let children = parse_inlines(region, inner_start as u64, inner_end as u64, refs);
        let children = rebase_inlines(children, base as i64);
        return Some(Parsed { node: Inline::Emphasis(NodeMeta { span, dirty: false }, children, kind), end: inner_end + 1 });
    }
    None
}

/// `parse_inlines` was called on a sub-slice `region[inner_start..inner_end]` which is
/// actually a sub-slice of `region` (we passed `region` as `bytes` and offsets relative to
/// `region`). The returned spans are relative to `region` start (offset 0 = region start).
/// We need them absolute in the document, so add `base` (the document offset of `region[0]`).
fn rebase_inlines(inlines: Vec<Inline>, delta: i64) -> Vec<Inline> {
    inlines.into_iter().map(|il| rebase_inline(il, delta)).collect()
}

fn rebase_inline(mut il: Inline, delta: i64) -> Inline {
    fn shift(m: &mut NodeMeta, delta: i64) {
        m.span.start = ByteOffset((m.span.start.0 as i64 + delta).max(0) as u64);
        m.span.end = ByteOffset((m.span.end.0 as i64 + delta).max(0) as u64);
    }
    match &mut il {
        Inline::Text(m, _) | Inline::CodeSpan(m, _, _) | Inline::MathSpan(m, _, _)
        | Inline::Autolink(m, _) | Inline::HardBreak(m) | Inline::RawHtml(m) | Inline::UnknownInline(m) => {
            shift(m, delta);
        }
        Inline::Emphasis(m, children, _) | Inline::Strong(m, children, _)
        | Inline::Strikethrough(m, children) => {
            shift(m, delta);
            *children = rebase_inlines(core::mem::take(children), delta);
        }
        Inline::Link(l) => {
            shift(&mut l.meta, delta);
            l.inlines = rebase_inlines(core::mem::take(&mut l.inlines), delta);
        }
        Inline::Image(im) => {
            shift(&mut im.meta, delta);
        }
    }
    il
}

fn try_link(region: &[u8], i: usize, base: u64, refs: &Refs) -> Option<Parsed> {
    // Find matching `]`.
    let mut depth = 1i32;
    let mut j = i + 1;
    while j < region.len() && depth > 0 {
        if region[j] == b'\\' && j + 1 < region.len() {
            j += 2;
            continue;
        }
        if region[j] == b'[' {
            depth += 1;
        } else if region[j] == b']' {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
        j += 1;
    }
    if depth != 0 {
        return None;
    }
    let text_end = j; // position of `]`
    let after = &region[text_end + 1..];
    // Inline `( ... )` — track parenthesis depth to handle URLs with nested
    // parens; `\(` and `\)` are escapes and must not affect the depth count.
    if after.first() == Some(&b'(') {
        let mut depth = 1usize;
        let mut close = 0usize;
        let mut k = 1usize;
        while k < after.len() {
            let b = after[k];
            if b == b'\\' && k + 1 < after.len() {
                k += 2;
                continue;
            }
            if b == b'(' {
                depth += 1;
            } else if b == b')' {
                depth -= 1;
                if depth == 0 { close = k; break; }
            }
            k += 1;
        }
        if close == 0 { return None; }
        let inner = &after[1..close];
        let (dest, title) = split_link_dest(inner);
        let span_end = text_end + 1 + close + 1;
        let span = SourceSpan::new(ByteOffset(base + i as u64), ByteOffset(base + span_end as u64));
        let children = parse_inlines(region, (i + 1) as u64, text_end as u64, refs);
        let children = rebase_inlines(children, base as i64);
        return Some(Parsed {
            node: Inline::Link(Link {
                meta: NodeMeta { span, dirty: false },
                inlines: children,
                style: LinkStyle::Inline,
                destination: dest,
                title,
                reference: None,
            }),
            end: span_end,
        });
    }
    // Reference / shortcut / collapsed.
    let (label, style, ref_consumed) = if after.first() == Some(&b'[') {
        // [text][id] or [text][]
        let close2 = after.iter().position(|&b| b == b']')?;
        let id_bytes = &after[1..close2];
        let label = if id_bytes.is_empty() {
            String::from_utf8_lossy(&region[i + 1..text_end]).trim().to_string()
        } else {
            String::from_utf8_lossy(id_bytes).trim().to_string()
        };
        let style = if id_bytes.is_empty() { LinkStyle::Collapsed } else { LinkStyle::Reference };
        (label, style, close2 + 1)
    } else {
        // shortcut: [text]
        let label = String::from_utf8_lossy(&region[i + 1..text_end]).trim().to_string();
        (label, LinkStyle::Shortcut, 0usize)
    };
    let normalized = normalize_label(&label);
    // CommonMark: a reference/collapsed/shortcut link is only a link when a
    // matching reference definition exists; otherwise the brackets are
    // literal text. (Also keeps a dirty regenerate from emitting `[t]()` for
    // an undefined label.)
    let (destination, title) = refs.map.get(&normalized).cloned()?;
    let span_end = text_end + 1 + ref_consumed;
    let span = SourceSpan::new(ByteOffset(base + i as u64), ByteOffset(base + span_end as u64));
    let children = parse_inlines(region, (i + 1) as u64, text_end as u64, refs);
    let children = rebase_inlines(children, base as i64);
    Some(Parsed {
        node: Inline::Link(Link {
            meta: NodeMeta { span, dirty: false },
            inlines: children,
            style,
            destination,
            title,
            reference: Some(label),
        }),
        end: span_end,
    })
}

fn try_image(region: &[u8], i: usize, base: u64, refs: &Refs) -> Option<Parsed> {
    // `![alt](dest)` or reference.
    let link = try_link(region, i + 1, base, refs)?;
    let mut span = link.node.span();
    span.start = ByteOffset(base + i as u64);
    let end = link.end;
    let alt = match &link.node {
        Inline::Link(l) => collect_text(&l.inlines),
        _ => String::new(),
    };
    let (style, destination, title, reference) = match &link.node {
        Inline::Link(l) => (l.style, l.destination.clone(), l.title.clone(), l.reference.clone()),
        _ => return None,
    };
    Some(Parsed {
        node: Inline::Image(Image {
            meta: NodeMeta { span, dirty: false },
            alt,
            style,
            destination,
            title,
            reference,
        }),
        end,
    })
}

fn collect_text(inlines: &[Inline]) -> String {
    let mut s = String::new();
    for il in inlines {
        match il {
            Inline::Text(_, t) => s.push_str(t),
            Inline::Emphasis(_, c, _) | Inline::Strong(_, c, _) | Inline::Strikethrough(_, c) => {
                s.push_str(&collect_text(c));
            }
            Inline::CodeSpan(_, t, _) => s.push_str(t),
            Inline::MathSpan(_, t, _) => s.push_str(t),
            Inline::Link(l) => s.push_str(&collect_text(&l.inlines)),
            _ => {}
        }
    }
    s
}

fn split_link_dest(inner: &[u8]) -> (String, Option<String>) {
    // A `<...>` destination may legally contain spaces — take everything up
    // to the closing `>` as the destination instead of splitting at the
    // first space (CommonMark: `[a](<u v> t)` = dest `u v`, title `t`).
    let mut split = inner.len();
    if inner.first() == Some(&b'<') {
        if let Some(gt) = inner.iter().position(|&b| b == b'>') {
            let dest = String::from_utf8_lossy(&inner[1..gt]).to_string();
            let title_raw = trim_bytes(&inner[gt + 1..]);
            let title = if title_raw.is_empty() {
                None
            } else {
                let t = trim_matches_bytes(title_raw, |b: u8| b == b'"' || b == b'\'');
                Some(String::from_utf8_lossy(t).to_string())
            };
            return (dest, title);
        }
        // Unclosed `<` — fall through to whitespace splitting.
    }
    // destination ends at first space; rest is title.
    let mut in_quotes = false;
    for (i, &b) in inner.iter().enumerate() {
        if b == b'"' {
            in_quotes = !in_quotes;
        }
        if (b == b' ' || b == b'\t') && !in_quotes {
            split = i;
            break;
        }
    }
    let dest = String::from_utf8_lossy(&inner[..split]).to_string();
    let title_raw = trim_bytes(&inner[split..]);
    let title = if title_raw.is_empty() {
        None
    } else {
        let t = trim_matches_bytes(title_raw, |b: u8| b == b'"' || b == b'\'');
        Some(String::from_utf8_lossy(t).to_string())
    };
    (dest, title)
}

fn find_autolink_end(region: &[u8]) -> Option<usize> {
    // `<...>` with no spaces and a `>`; must contain a `:` or `@` for url/email.
    if region.first() != Some(&b'<') {
        return None;
    }
    let close = region.iter().position(|&b| b == b'>')?;
    let inner = &region[1..close];
    if inner.iter().any(|&b| b == b' ' || b == b'\t' || b == b'<') {
        return None;
    }
    if !inner.contains(&b':') && !inner.contains(&b'@') {
        return None;
    }
    Some(close + 1)
}

/// First pass: collect link reference definitions from the whole document so that
/// reference/collapsed/shortcut links can resolve against definitions appearing later
/// (§100). Labels are normalized per CommonMark: trimmed, case-folded, internal whitespace
/// collapsed to single spaces.
fn collect_references(blocks: &[Block]) -> Refs {
    let mut refs = Refs::default();
    for b in blocks {
        if let Block::LinkReferenceDefinition(d) = b {
            let normalized = normalize_label(&d.label);
            // CommonMark: the FIRST definition of a label wins — later
            // duplicates are ignored, not overwritten.
            refs.map
                .entry(normalized)
                .or_insert((d.destination.clone(), d.title.clone()));
        }
    }
    refs
}

/// Normalize a reference label per CommonMark: trim, case-fold to lowercase, collapse
/// internal whitespace runs to a single space.
fn normalize_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_space = true; // trim leading
    for c in label.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.extend(c.to_lowercase());
            prev_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}
