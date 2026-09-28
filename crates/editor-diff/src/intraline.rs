//! Intraline diff (§39): highlight the exact changed regions within a modified line pair,
//! rather than colouring the whole line. Uses a **token-level LCS** (words and whitespace
//! runs as atomic tokens) so that a one-word change inside a long paragraph only marks
//! that word — matching the §39 example (`brown`/`red` highlighted as whole words, not
//! fragmented by shared characters).

use editor_domain::{ByteOffset, ByteRange};

/// A single intraline change: a byte range on the old side paired with the corresponding
/// range on the new side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntralineChange {
    /// Changed byte range on the old line (0-based within the line).
    pub old_range: ByteRange,
    /// Changed byte range on the new line (0-based within the line).
    pub new_range: ByteRange,
}

/// A token: a maximal run of "word" characters or "non-word" characters, with its byte
/// span. Words are `[A-Za-z0-9_]+` plus any non-ASCII (Unicode) codepoint; everything else
/// forms whitespace/punctuation tokens. This keeps word-level edits atomic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Token {
    is_word: bool,
    start: usize,
    end: usize,
}

fn tokenize(s: &[u8]) -> Vec<Token> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < s.len() {
        let start = i;
        let is_word = is_word_byte(s[i]);
        while i < s.len() && is_word_byte(s[i]) == is_word {
            i += 1;
        }
        out.push(Token {
            is_word,
            start,
            end: i,
        });
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 // treat any non-ASCII (UTF-8 cont) as word-ish
}

/// Compute intraline changes between two line contents using a token-level LCS.
pub fn intraline_diff(old: &[u8], new: &[u8]) -> Vec<IntralineChange> {
    if old == new {
        return Vec::new();
    }
    // Bounded fallback for pathological lines. The LCS DP table is n*m usize
    // cells — capping each side at MAX_TOKENS alone still allows ~512 MB of
    // table, so the product is capped as well (~32 MB).
    const MAX_TOKENS: usize = 8192;
    const MAX_LCS_CELLS: usize = 4_000_000;
    let old_toks = tokenize(old);
    let new_toks = tokenize(new);
    if old_toks.len() > MAX_TOKENS
        || new_toks.len() > MAX_TOKENS
        || old_toks.len().saturating_mul(new_toks.len()) > MAX_LCS_CELLS
    {
        return vec![IntralineChange {
            old_range: ByteRange::new(ByteOffset(0), ByteOffset(old.len() as u64)),
            new_range: ByteRange::new(ByteOffset(0), ByteOffset(new.len() as u64)),
        }];
    }

    let lcs = token_lcs(&old_toks, &new_toks, old, new);
    // Walk the LCS to find non-matching runs and merge adjacent changed regions.
    let mut changes = Vec::new();
    let mut oi = 0usize;
    let mut ni = 0usize;
    let mut li = 0usize;
    let mut cur: Option<(usize, usize, usize, usize)> = None; // (old_start, old_end, new_start, new_end)

    while oi < old_toks.len() || ni < new_toks.len() {
        let old_match = oi < old_toks.len() && li < lcs.len() && old_toks[oi].start == lcs[li].0;
        let new_match = ni < new_toks.len() && li < lcs.len() && new_toks[ni].start == lcs[li].1;

        if old_match && new_match {
            // Match: flush any pending change.
            if let Some((os, oe, ns, ne)) = cur.take() {
                changes.push(IntralineChange {
                    old_range: ByteRange::new(ByteOffset(os as u64), ByteOffset(oe as u64)),
                    new_range: ByteRange::new(ByteOffset(ns as u64), ByteOffset(ne as u64)),
                });
            }
            oi += 1;
            ni += 1;
            li += 1;
        } else {
            // Mismatch: extend the pending change on the non-matching side(s).
            let old_pos = if oi < old_toks.len() {
                old_toks[oi].start
            } else {
                old.len()
            };
            let new_pos = if ni < new_toks.len() {
                new_toks[ni].start
            } else {
                new.len()
            };
            let (os, ns) = match cur {
                Some((os, _, ns, _)) => (os, ns),
                None => (old_pos, new_pos),
            };
            if oi < old_toks.len() && !old_match {
                oi += 1;
            }
            if ni < new_toks.len() && !new_match {
                ni += 1;
            }
            let oe = if oi < old_toks.len() {
                old_toks[oi].start
            } else {
                old.len()
            };
            let ne = if ni < new_toks.len() {
                new_toks[ni].start
            } else {
                new.len()
            };
            cur = Some((os, oe, ns, ne));
        }
    }
    if let Some((os, oe, ns, ne)) = cur.take() {
        changes.push(IntralineChange {
            old_range: ByteRange::new(ByteOffset(os as u64), ByteOffset(oe as u64)),
            new_range: ByteRange::new(ByteOffset(ns as u64), ByteOffset(ne as u64)),
        });
    }
    changes
}

/// Token-level LCS, returning matched (old_byte_start, new_byte_start) pairs.
fn token_lcs(a: &[Token], b: &[Token], a_bytes: &[u8], b_bytes: &[u8]) -> Vec<(usize, usize)> {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            let a_text = &a_bytes[a[i - 1].start..a[i - 1].end];
            let b_text = &b_bytes[b[j - 1].start..b[j - 1].end];
            dp[i][j] = if a_text == b_text {
                dp[i - 1][j - 1] + 1
            } else {
                dp[i - 1][j].max(dp[i][j - 1])
            };
        }
    }
    let mut out = Vec::with_capacity(dp[n][m]);
    let mut i = n;
    let mut j = m;
    while i > 0 && j > 0 {
        let a_text = &a_bytes[a[i - 1].start..a[i - 1].end];
        let b_text = &b_bytes[b[j - 1].start..b[j - 1].end];
        if a_text == b_text {
            out.push((a[i - 1].start, b[j - 1].start));
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_lines_have_no_changes() {
        assert!(intraline_diff(b"hello world", b"hello world").is_empty());
    }

    #[test]
    fn single_word_change_is_localized() {
        let old = b"The quick brown fox";
        let new = b"The quick red fox";
        let changes = intraline_diff(old, new);
        assert_eq!(changes.len(), 1, "expected one change, got {changes:?}");
        let c = &changes[0];
        assert_eq!(
            &old[c.old_range.start.0 as usize..c.old_range.end.0 as usize],
            b"brown"
        );
        assert_eq!(
            &new[c.new_range.start.0 as usize..c.new_range.end.0 as usize],
            b"red"
        );
    }

    #[test]
    fn insertion_only_change() {
        let old = b"hello world";
        let new = b"hello cruel world";
        let changes = intraline_diff(old, new);
        assert_eq!(changes.len(), 1, "got {changes:?}");
        let c = &changes[0];
        assert_eq!(c.old_range.len(), 0);
        // Inserted region is "cruel " (word + trailing space) or " cruel" — accept either
        // as long as it contains "cruel".
        let inserted = &new[c.new_range.start.0 as usize..c.new_range.end.0 as usize];
        assert!(
            inserted.windows(5).any(|w| w == b"cruel"),
            "inserted={:?}",
            inserted
        );
    }

    #[test]
    fn completely_different_lines_are_one_change() {
        let old = b"abc";
        let new = b"xyz";
        let changes = intraline_diff(old, new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].old_range.len(), 3);
        assert_eq!(changes[0].new_range.len(), 3);
    }

    #[test]
    fn long_line_falls_back_to_whole_line() {
        let old = vec![b'a'; 20000];
        let new = vec![b'b'; 20000];
        let changes = intraline_diff(&old, &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].old_range.len(), 20000);
        assert_eq!(changes[0].new_range.len(), 20000);
    }
}
