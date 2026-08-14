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
        if !ctx.inside_fenced_code && indent >= 4 {
            ctx.inside_indented_code = true;
        }
        if !ctx.inside_fenced_code {
            let stripped = &last_line[indent.min(last_line.len())..];
            if is_list_marker(stripped) {
                ctx.inside_list = true;
            }
        }
    }

    // Block quote depth: count `>` markers on the last non-blank prefix line.
    if let Some(last_line) = last_non_blank_line(prefix) {
        let stripped = &last_line[leading_spaces(last_line).min(last_line.len())..];
        if stripped.starts_with(b">") {
            ctx.block_quote_depth = stripped.iter().take_while(|&&b| b == b'>' || b == b' ').filter(|&&b| b == b'>').count() as u32;
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
    let mut last_open: Option<u8> = None;
    for line in prefix.split(|&b| b == b'\n') {
        let stripped = &line[leading_spaces(line).min(line.len())..];
        let fence = fence_char_and_len(stripped);
        if let Some((c, n)) = fence {
            if last_open == Some(c) {
                // A fence line of the same char with only trailing whitespace closes.
                let rest = &stripped[n..];
                if rest.iter().all(|&b| b == b' ' || b == b'\t' || b == b'\r') {
                    last_open = None;
                }
            } else if last_open.is_none() {
                last_open = Some(c);
            }
        }
    }
    last_open
}

/// If `s` begins with a fence run (``` or ~~~, length >= 3), return (char, length).
fn fence_char_and_len(s: &[u8]) -> Option<(u8, usize)> {
    let &c = s.first()?;
    if c != b'`' && c != b'~' {
        return None;
    }
    let n = s.iter().take_while(|&&b| b == c).count();
    if n >= 3 {
        Some((c, n))
    } else {
        None
    }
}

fn last_non_blank_line(bytes: &[u8]) -> Option<&[u8]> {
    bytes
        .rsplit(|&b| b == b'\n')
        .find(|line| !line.iter().all(|&b| b == b' ' || b == b'\t' || b == b'\r'))
}

fn leading_spaces(s: &[u8]) -> usize {
    s.iter().take_while(|&&b| b == b' ').count()
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
    matches!(s.get(digits), Some(b'.') | Some(b')'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_domain::ByteOffset;

    fn chunk(start: u64, bytes: &[u8]) -> ByteChunk {
        ByteChunk { start_offset: ByteOffset(start), bytes: bytes.to_vec(), leading_partial: true, trailing_partial: true }
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
}
