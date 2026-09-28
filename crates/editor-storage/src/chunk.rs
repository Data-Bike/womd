//! Chunk parser context (§54): find Markdown parse synchronization points so a windowed
//! parse never assumes the first byte of a chunk is the document start.
//!
//! Given a `ByteChunk` plus surrounding prefix/suffix context, this module determines the
//! enclosing block state at the window's start (open fenced code? open list? block quote
//! depth? open HTML block? link reference table?) so the parser can interpret the window
//! correctly. For the foundation we provide line-based sync-point detection for the most
//! common enclosing structures (fenced code, indented code, block quote); full block-state
//! tracking lands with the incremental parser.

use crate::ByteChunk;

/// Enclosing block state inferred from prefix context.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChunkContext {
    /// True if the window begins inside an open fenced code block.
    pub inside_fenced_code: bool,
    /// The fence char (`` ` `` or `~`) if inside a fenced code block.
    pub fence_char: u8,
    /// True if the window begins inside an indented code block.
    pub inside_indented_code: bool,
    /// Block quote `>` depth at the window start (0 = not in a quote).
    pub block_quote_depth: u32,
    /// True if the window begins inside a list item.
    pub inside_list: bool,
}

/// Analyze prefix context to determine the block state at the start of `chunk`.
///
/// `prefix` is the bytes immediately preceding `chunk` (typically a small overlap window,
/// §53). The analysis is conservative: it scans backward for an unclosed fence and forward
/// through the prefix for block-quote markers and indentation.
pub fn analyze_context(prefix: &[u8], chunk: &ByteChunk) -> ChunkContext {
    let mut ctx = ChunkContext::default();

    // Fenced code: scan the prefix for an opening fence that is not yet closed.
    if let Some(open) = last_unclosed_fence(prefix) {
        ctx.inside_fenced_code = true;
        ctx.fence_char = open;
    }

    // Indented code / list: examine the last non-blank line of the prefix.
    if let Some(last_line) = last_non_blank_line(prefix) {
        let indent = leading_spaces(last_line);
        if !ctx.inside_fenced_code && indent >= 4 && is_inside_indented_code(prefix) {
            ctx.inside_indented_code = true;
        }
        if !ctx.inside_fenced_code {
            let stripped = &last_line[indent.min(last_line.len())..];
            if is_list_marker(stripped) {
                ctx.inside_list = true;
            } else if indent >= 2 {
                // An indented continuation line is inside a list item when a
                // marker earlier in the same non-blank run has a content
                // column the line reaches ("- a\n  continuation").
                ctx.inside_list = continues_list_item(prefix, indent);
            }
        }
    }

    // Block quote depth: count `>` markers on the last non-blank prefix line.
    if let Some(last_line) = last_non_blank_line(prefix) {
        let stripped = &last_line[leading_spaces(last_line).min(last_line.len())..];
        if stripped.starts_with(b">") {
            ctx.block_quote_depth = stripped
                .iter()
                .take_while(|&&b| b == b'>' || b == b' ')
                .filter(|&&b| b == b'>')
                .count() as u32;
        }
    }

    // If the chunk itself starts with a fence-closing line, the enclosing fenced-code
    // state would end at that line; we leave that to the parser. The context here
    // describes the state *entering* the chunk.
    let _ = chunk;
    ctx
}

/// Find the last opening fence in `prefix` that has no matching close. Returns the fence
/// char if any.
fn last_unclosed_fence(prefix: &[u8]) -> Option<u8> {
    // (char, run length) — a closing run must be AT LEAST as long as the
    // opener (CommonMark: a shorter run is just code content).
    let mut last_open: Option<(u8, usize)> = None;
    for line in prefix.split(|&b| b == b'\n') {
        // A fence may be indented by at most 3 spaces; 4+ means indented code.
        if leading_spaces(line) > 3 {
            continue;
        }
        let stripped = &line[leading_spaces(line).min(line.len())..];
        let fence = fence_char_and_len(stripped);
        if let Some((c, n)) = fence {
            match last_open {
                Some((open_c, open_n)) if open_c == c && n >= open_n => {
                    // A fence line of the same char with only trailing
                    // whitespace and a run >= the opener's length closes.
                    let rest = &stripped[n..];
                    if rest.iter().all(|&b| b == b' ' || b == b'\t' || b == b'\r') {
                        last_open = None;
                    }
                }
                Some(_) => {
                    // A fence line of a different char — or a same-char run
                    // shorter than the opener — is code content, not a close.
                }
                None => last_open = Some((c, n)),
            }
        }
    }
    last_open.map(|(c, _)| c)
}

/// If `s` begins with a fence run (``` or ~~~, length >= 3), return (char, length).
fn fence_char_and_len(s: &[u8]) -> Option<(u8, usize)> {
    let &c = s.first()?;
    if c != b'`' && c != b'~' {
        return None;
    }
    let n = s.iter().take_while(|&&b| b == c).count();
    if n >= 3 { Some((c, n)) } else { None }
}

fn last_non_blank_line(bytes: &[u8]) -> Option<&[u8]> {
    bytes
        .rsplit(|&b| b == b'\n')
        .find(|line| !line.iter().all(|&b| b == b' ' || b == b'\t' || b == b'\r'))
}

fn leading_spaces(s: &[u8]) -> usize {
    s.iter().take_while(|&&b| b == b' ').count()
}

/// True when the indented line at the end of `prefix` is part of an indented
/// CODE block — i.e. the run of `>=4`-indented lines ending the prefix was
/// preceded by a blank line or the prefix start. A non-blank line directly
/// before the run means paragraph continuation, which is NOT indented code
/// (indented code cannot interrupt a paragraph, CommonMark §5.4).
fn is_inside_indented_code(prefix: &[u8]) -> bool {
    let mut seen_indented = false;
    for line in prefix.rsplit(|&b| b == b'\n') {
        let blank = line.iter().all(|&b| b == b' ' || b == b'\t' || b == b'\r');
        if blank {
            if seen_indented {
                return true; // blank separator before the indented run
            }
            continue;
        }
        if leading_spaces(line) >= 4 {
            seen_indented = true;
            continue;
        }
        return false; // non-blank, non-indented line -> paragraph continuation
    }
    seen_indented
}

fn is_list_marker(s: &[u8]) -> bool {
    if s.is_empty() {
        return false;
    }
    let c = s[0];
    if c == b'-' || c == b'*' || c == b'+' {
        return s.len() == 1 || s[1] == b' ' || s[1] == b'\t';
    }
    let digits = s.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 9 {
        return false;
    }
    if !matches!(s.get(digits), Some(b'.') | Some(b')')) {
        return false;
    }
    // The marker must be followed by whitespace or end-of-line —
    // "1.x" is not a list item (CommonMark §6.2).
    match s.get(digits + 1) {
        None => true,
        Some(b' ') | Some(b'\t') => true,
        _ => false,
    }
}

/// Content column of a list marker line: the byte offset where the item's
/// content starts (`- x` → marker indent + 2, `123. x` → indent + digits + 1
/// delimiter + 1 space). Continuation lines indented to at least this column
/// belong to the item.
fn list_marker_content_col(line: &[u8]) -> Option<usize> {
    let indent = leading_spaces(line);
    let s = &line[indent.min(line.len())..];
    if s.is_empty() {
        return None;
    }
    let c = s[0];
    if c == b'-' || c == b'*' || c == b'+' {
        // Marker + one space (or EOL — then content is treated as indent+2).
        return if s.len() == 1 || s[1] == b' ' || s[1] == b'\t' {
            Some(indent + 2)
        } else {
            None
        };
    }
    let digits = s.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 9 || !matches!(s.get(digits), Some(b'.') | Some(b')')) {
        return None;
    }
    match s.get(digits + 1) {
        None => Some(indent + digits + 2),
        Some(b' ') | Some(b'\t') => Some(indent + digits + 2),
        _ => None,
    }
}

/// True when a line indented `line_indent` columns continues a list item:
/// scan the contiguous non-blank run ending the prefix backwards; the first
/// list marker found wins, and the line is inside the item when its indent
/// reaches the marker's content column. A blank line or a line that is
/// itself a top-level construct ends the search conservatively.
fn continues_list_item(prefix: &[u8], line_indent: usize) -> bool {
    // Phase 0: skip trailing blank segments (a prefix ending in '\n' yields
    // an empty first rsplit entry). Phase 1: the last non-blank line is the
    // one being classified — skip it, then scan earlier lines for a marker.
    let mut skipping = true;
    for line in prefix.rsplit(|&b| b == b'\n') {
        let blank = line.iter().all(|&b| b == b' ' || b == b'\t' || b == b'\r');
        if skipping {
            if blank {
                continue;
            }
            skipping = false;
            continue;
        }
        if blank {
            return false; // blank line ends the contiguous run
        }
        if let Some(col) = list_marker_content_col(line) {
            return line_indent >= col;
        }
        // A non-marker line: could be an earlier continuation of the same
        // item (e.g. "- a\n  b\n  c") — keep scanning for the marker.
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_domain::ByteOffset;

    fn chunk(start: u64, bytes: &[u8]) -> ByteChunk {
        ByteChunk {
            start_offset: ByteOffset(start),
            bytes: bytes.to_vec(),
            leading_partial: true,
            trailing_partial: true,
        }
    }

    #[test]
    fn detects_open_fenced_code() {
        let prefix = b"intro\n```rust\nfn a() {}\n";
        let ctx = analyze_context(prefix, &chunk(20, b"fn b() {}\n```\n"));
        assert!(ctx.inside_fenced_code);
        assert_eq!(ctx.fence_char, b'`');
    }

    #[test]
    fn closed_fence_is_not_enclosing() {
        let prefix = b"```rust\nfn a() {}\n```\n";
        let ctx = analyze_context(prefix, &chunk(20, b"after\n"));
        assert!(!ctx.inside_fenced_code);
    }

    #[test]
    fn detects_block_quote_depth() {
        let prefix = b"> quote\n> more\n";
        let ctx = analyze_context(prefix, &chunk(14, b"continued\n"));
        assert_eq!(ctx.block_quote_depth, 1);
    }

    #[test]
    fn detects_indented_code() {
        let prefix = b"    let x = 1;\n";
        let ctx = analyze_context(prefix, &chunk(15, b"    let y = 2;\n"));
        assert!(ctx.inside_indented_code);
    }

    #[test]
    fn detects_list() {
        let prefix = b"- item one\n";
        let ctx = analyze_context(prefix, &chunk(11, b"  continued\n"));
        assert!(ctx.inside_list);
    }

    #[test]
    fn plain_text_has_no_enclosing_state() {
        let prefix = b"just a paragraph\n";
        let ctx = analyze_context(prefix, &chunk(17, b"more text\n"));
        assert_eq!(ctx, ChunkContext::default());
    }

    /// A backtick line indented by 4+ spaces is indented code, not a fence —
    /// it must not be counted as an opening fence (CommonMark §5.8).
    #[test]
    fn deeply_indented_fence_is_indented_code_not_fence() {
        let prefix = b"para\n    ```rust\n    fn a() {}\n";
        let ctx = analyze_context(prefix, &chunk(20, b"next\n"));
        assert!(!ctx.inside_fenced_code);
    }

    /// A fence indented up to 3 spaces is still a fence.
    #[test]
    fn slightly_indented_fence_counts() {
        let prefix = b"para\n   ```rust\n   code\n";
        let ctx = analyze_context(prefix, &chunk(20, b"more\n"));
        assert!(ctx.inside_fenced_code);
    }

    /// A closing run SHORTER than the opener is code content, not a close —
    ///// the block remains open (CommonMark: closer must be >= opener length).
    #[test]
    fn shorter_run_does_not_close_fence() {
        let prefix = b"`````\ncode\n```\nmore code\n";
        let ctx = analyze_context(prefix, &chunk(20, b"still inside\n"));
        assert!(
            ctx.inside_fenced_code,
            "3-tick line cannot close a 5-tick fence"
        );
    }

    /// A closing run LONGER than the opener still closes the block.
    #[test]
    fn longer_run_closes_fence() {
        let prefix = b"```\ncode\n`````\nafter\n";
        let ctx = analyze_context(prefix, &chunk(20, b"next\n"));
        assert!(!ctx.inside_fenced_code);
    }

    /// `1.x` — ordered marker not followed by whitespace — is not a list.
    #[test]
    fn ordered_marker_needs_whitespace() {
        let prefix = b"1.x not a list\n";
        let ctx = analyze_context(prefix, &chunk(15, b"next\n"));
        assert!(!ctx.inside_list);
        // But "1." at EOL and "1. " are valid markers.
        assert!(analyze_context(b"1. item\n", &chunk(8, b"x\n")).inside_list);
        assert!(analyze_context(b"1.\n", &chunk(3, b"x\n")).inside_list);
    }

    /// `para\n    continuation` — a `>=4`-indented line right after paragraph
    /// text is a paragraph continuation, NOT an indented code block (code
    /// cannot interrupt a paragraph).
    #[test]
    fn indented_line_after_paragraph_is_not_code() {
        let prefix = b"para text\n    continuation of para\n";
        let ctx = analyze_context(prefix, &chunk(35, b"next\n"));
        assert!(
            !ctx.inside_indented_code,
            "paragraph continuation is not code"
        );
    }

    /// The same indented line after a BLANK separator IS indented code.
    #[test]
    fn indented_line_after_blank_is_code() {
        let prefix = b"para text\n\n    indented code\n";
        let ctx = analyze_context(prefix, &chunk(30, b"    more code\n"));
        assert!(ctx.inside_indented_code);
    }

    /// Paragraph continuation, then a blank, then real indented code —
    /// the last indented line must be detected as code even though an
    /// earlier indented line was continuation.
    #[test]
    fn indented_code_after_mixed_continuation() {
        let prefix = b"para\n    cont\n\n    real code\n";
        let ctx = analyze_context(prefix, &chunk(30, b"    more\n"));
        assert!(ctx.inside_indented_code);
    }

    /// A different-char fence inside an open block is content, not a close or
    /// a new open.
    #[test]
    fn different_char_fence_inside_block_is_content() {
        let prefix = b"```\ncode\n~~~\nmore\n";
        let ctx = analyze_context(prefix, &chunk(15, b"next\n"));
        assert!(ctx.inside_fenced_code);
        assert_eq!(ctx.fence_char, b'`');
    }

    /// A same-char fence WITH an info string inside an open block cannot be a
    /// closer (closers take no info) — it's content.
    #[test]
    fn fence_with_info_inside_block_is_content() {
        let prefix = b"```\ncode\n```rust\nmore\n";
        let ctx = analyze_context(prefix, &chunk(15, b"next\n"));
        assert!(
            ctx.inside_fenced_code,
            "a ```rust line cannot close a fence"
        );
    }

    /// An indented continuation line inside a list item is still inside the
    /// list — a chunk boundary there must not reset the list context.
    #[test]
    fn indented_continuation_inside_list() {
        assert!(analyze_context(b"- item\n  continued\n", &chunk(18, b"x\n")).inside_list);
        // Multi-line continuation.
        assert!(analyze_context(b"- a\n  b\n  c\n", &chunk(10, b"x\n")).inside_list);
        // Ordered marker content column: "12. " → indent 0 + 2 digits + 2.
        assert!(analyze_context(b"12. item\n    cont\n", &chunk(15, b"x\n")).inside_list);
    }

    /// A continuation line that does NOT reach the marker's content column is
    /// outside the list item.
    #[test]
    fn shallow_continuation_not_inside_list() {
        // "  x" reaches only column 2 under "1234. " (content col 6).
        assert!(!analyze_context(b"1234. item\n  x\n", &chunk(12, b"y\n")).inside_list);
    }

    /// A blank line ends the item run conservatively — even though a deeply
    /// indented line could still belong to the item, the safe answer is
    /// "not inside".
    #[test]
    fn blank_line_breaks_list_context() {
        assert!(!analyze_context(b"- item\n\n  indented\n", &chunk(15, b"x\n")).inside_list);
    }

    /// Continuation lines with no marker anywhere in the prefix are not a
    /// list (e.g. plain indented code already handled, or prose).
    #[test]
    fn indented_line_without_marker_is_not_list() {
        assert!(!analyze_context(b"para\n    code\n", &chunk(12, b"x\n")).inside_list);
    }
}
