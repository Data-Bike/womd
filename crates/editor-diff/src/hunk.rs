//! Hunk grouping (§40): collapse runs of equal lines into context, group changes into
//! hunks with surrounding context, and provide prev/next-change navigation.

use crate::myers::LineChange;

/// Configuration for hunk grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HunkConfig {
    /// Number of context lines to keep around each change run.
    pub context: u32,
}

impl HunkConfig {
    /// Default context of 3 lines (matches `git diff`).
    pub const DEFAULT: Self = Self { context: 3 };
}

/// A hunk: a contiguous group of `LineChange`s with surrounding context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// Old-side starting line number (1-based).
    pub old_start: u32,
    /// New-side starting line number (1-based).
    pub new_start: u32,
    pub lines: Vec<LineChange>,
}

impl Hunk {
    /// First changed line index within `lines`, if any.
    pub fn first_change_index(&self) -> Option<usize> {
        self.lines
            .iter()
            .position(|c| !matches!(c, LineChange::Equal { .. }))
    }
    /// Last changed line index within `lines`, if any.
    pub fn last_change_index(&self) -> Option<usize> {
        self.lines
            .iter()
            .rposition(|c| !matches!(c, LineChange::Equal { .. }))
    }
}

/// Group a flat list of `LineChange`s into hunks with `config.context` lines of context
/// around each change run. Adjacent change runs whose context windows overlap are merged
/// into a single hunk (§40).
pub fn group_into_hunks(changes: &[LineChange], config: HunkConfig) -> Vec<Hunk> {
    if changes.is_empty() {
        return Vec::new();
    }
    // Indices of changed lines.
    let change_indices: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, c)| !matches!(c, LineChange::Equal { .. }))
        .map(|(i, _)| i)
        .collect();
    if change_indices.is_empty() {
        return Vec::new();
    }

    let ctx = config.context as usize;
    // Build windows [start, end] for each change index, then merge overlapping/adjacent.
    let mut windows: Vec<(usize, usize)> = change_indices
        .iter()
        .map(|&i| (i.saturating_sub(ctx), (i + ctx + 1).min(changes.len())))
        .collect();
    windows.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for w in windows {
        match merged.last_mut() {
            Some(last) if w.0 <= last.1 => last.1 = last.1.max(w.1),
            _ => merged.push(w),
        }
    }

    // Compute old/new start line numbers for each window.
    merged
        .into_iter()
        .map(|(start, end)| {
            let (old_start, new_start) = line_numbers_at(changes, start, end);
            Hunk {
                old_start,
                new_start,
                lines: changes[start..end].to_vec(),
            }
        })
        .collect()
}

/// Compute the `@@ -old_start +new_start @@` header numbers for the hunk window
/// `changes[start..end]`. When the window contains no lines of one side (a pure
/// insert or pure delete hunk), git reports the number of lines already passed
/// on that side — e.g. `-0,0` for an insertion at the top of the file.
fn line_numbers_at(changes: &[LineChange], start: usize, end: usize) -> (u32, u32) {
    let mut old = 0u32;
    let mut new = 0u32;
    for c in &changes[..start] {
        match c {
            LineChange::Equal { .. } => {
                old += 1;
                new += 1;
            }
            LineChange::Delete { .. } => old += 1,
            LineChange::Insert { .. } => new += 1,
        }
    }
    let has_old = changes[start..end]
        .iter()
        .any(|c| !matches!(c, LineChange::Insert { .. }));
    let has_new = changes[start..end]
        .iter()
        .any(|c| !matches!(c, LineChange::Delete { .. }));
    (
        if has_old { old + 1 } else { old },
        if has_new { new + 1 } else { new },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::myers::line_diff;

    #[test]
    fn single_change_produces_one_hunk_with_context() {
        let old = b"a\nb\nc\nd\ne\nf\ng\n";
        let new = b"a\nb\nX\nd\ne\nf\ng\n";
        let changes = line_diff(old, new);
        let hunks = group_into_hunks(&changes, HunkConfig { context: 1 });
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        // Context 1 around the changed line: b, X, d.
        assert!(h.lines.len() >= 3);
        assert!(
            h.lines
                .iter()
                .any(|c| matches!(c, LineChange::Insert { bytes, .. } if bytes == b"X"))
        );
    }

    #[test]
    fn far_apart_changes_produce_two_hunks() {
        let old = b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let new = b"1\n2\nX\n4\n5\n6\n7\n8\nY\n10\n";
        let changes = line_diff(old, new);
        let hunks = group_into_hunks(&changes, HunkConfig { context: 1 });
        assert_eq!(hunks.len(), 2);
    }

    #[test]
    fn adjacent_changes_merge_into_one_hunk() {
        let old = b"a\nb\nc\nd\n";
        let new = b"X\nY\nc\nd\n";
        let changes = line_diff(old, new);
        let hunks = group_into_hunks(&changes, HunkConfig { context: 1 });
        assert_eq!(hunks.len(), 1);
    }

    #[test]
    fn no_changes_produce_no_hunks() {
        let changes = line_diff(b"a\nb\n", b"a\nb\n");
        assert!(group_into_hunks(&changes, HunkConfig::DEFAULT).is_empty());
    }

    /// A pure-insert hunk has no old-side lines: git reports the old position
    /// as the number of lines already passed (`-1,0` for an insert after line
    /// 1), not `old+1` which would point inside the hunk that doesn't exist.
    #[test]
    fn pure_insert_hunk_reports_passed_line_count() {
        let old = b"a\nb\n";
        let new = b"a\nx\nb\n";
        let changes = line_diff(old, new);
        let hunks = group_into_hunks(&changes, HunkConfig { context: 0 });
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.old_start, 1, "insert after old line 1 -> -1,0");
        assert_eq!(h.new_start, 2);
    }

    /// A pure-insert hunk at the very top of the file reports `-0,0` (zero
    /// old-side lines passed) — matching `git diff` output for top insertions.
    #[test]
    fn pure_insert_hunk_at_top_reports_zero_old_start() {
        let old = b"a\nb\n";
        let new = b"x\na\nb\n";
        let changes = line_diff(old, new);
        let hunks = group_into_hunks(&changes, HunkConfig { context: 0 });
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 0, "top insert -> -0,0 like git");
        assert_eq!(hunks[0].new_start, 1);
    }

    /// A pure-delete hunk at EOF reports `+N,0` with N = new lines passed.
    #[test]
    fn pure_delete_hunk_at_eof_reports_new_passed_count() {
        let old = b"a\nb\nc\n";
        let new = b"a\nb\n";
        let changes = line_diff(old, new);
        let hunks = group_into_hunks(&changes, HunkConfig { context: 0 });
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 3);
        assert_eq!(hunks[0].new_start, 2, "deleted last line -> +2,0");
    }

    #[test]
    fn first_and_last_change_index() {
        let old = b"a\nb\nc\nd\n";
        let new = b"X\nb\nY\nd\n";
        let changes = line_diff(old, new);
        let hunks = group_into_hunks(&changes, HunkConfig { context: 1 });
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert!(h.first_change_index().is_some());
        assert!(h.last_change_index().is_some());
        assert!(h.first_change_index().unwrap() <= h.last_change_index().unwrap());
    }
}
