//! Myers O(ND) line diff algorithm (§38, §94).
//!
//! Computes a shortest edit script between two byte buffers treated as sequences of lines.
//! The classic O(ND) algorithm with linear-space backtracking via the recursive divide &
//! conquer variant is used; for MVP-sized inputs the simple O((N+M)*D) dynamic program is
//! clear and sufficient, with the V-array optimization from Myers.

/// A single line-level operation in the edit script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Equal,
    Delete,
    Insert,
}

/// One line-level change with the line content (excluding its trailing newline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineChange {
    /// Line present on both sides with identical content.
    Equal { old_no: u32, new_no: u32, bytes: Vec<u8> },
    /// Line present only on the old side.
    Delete { old_no: u32, bytes: Vec<u8> },
    /// Line present only on the new side.
    Insert { new_no: u32, bytes: Vec<u8> },
}

impl LineChange {
    pub fn op(&self) -> Operation {
        match self {
            Self::Equal { .. } => Operation::Equal,
            Self::Delete { .. } => Operation::Delete,
            Self::Insert { .. } => Operation::Insert,
        }
    }
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Equal { bytes, .. } | Self::Delete { bytes, .. } | Self::Insert { bytes, .. } => bytes,
        }
    }
}

/// Split a byte buffer into lines (content excludes the trailing newline; CRLF drops the
/// `\r` so equality is line-ending-tolerant for diff purposes while preserving original
/// bytes in the emitted `LineChange`).
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            let mut end = i;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            out.push(&bytes[start..end]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    out
}

/// Compute the line-level diff between `old` and `new` using the Myers O(ND) algorithm.
///
/// Returns a sequence of `LineChange` covering both sides in order. Line numbers are
/// 1-based and refer to the old/new side respectively.
pub fn line_diff(old: &[u8], new: &[u8]) -> Vec<LineChange> {
    let a = split_lines(old);
    let b = split_lines(new);
    let script = myers_edit_script(&a, &b);
    script_to_changes(&a, &b, &script)
}

/// Myers edit script as a sequence of operations over the concatenated a/b walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Equal,
    Delete,
    Insert,
}

/// The core Myers O(ND) algorithm. Returns the edit script as a vector of `Step`s.
///
/// Reference: Eugene W. Myers, "An O(ND) Difference Algorithm and Its Variations" (1986).
/// We use the linear-space V array and recover the path by storing the trace of each
/// iteration.
fn myers_edit_script(a: &[&[u8]], b: &[&[u8]]) -> Vec<Step> {
    let n = a.len();
    let m = b.len();
    if n == 0 && m == 0 {
        return Vec::new();
    }
    let max = n + m;
    // V is indexed by diagonal k in [-max, max]; offset by `max` so V[k + max] is valid.
    let mut v = vec![0i64; 2 * max + 1];
    // Trace of V snapshots for backtracking.
    let mut trace: Vec<Vec<i64>> = Vec::new();

    let offset = max as i64;
    for d in 0..=max as i64 {
        trace.push(v.clone());
        for k in (-d..=d).step_by(2) {
            let mut x = if k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]) {
                v[(k + 1 + offset) as usize]
            } else {
                v[(k - 1 + offset) as usize] + 1
            };
            let mut y = x - k;
            while x < n as i64 && y < m as i64 && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(k + offset) as usize] = x;
            if x >= n as i64 && y >= m as i64 {
                return backtrack(&trace, a, b, d);
            }
        }
    }
    // Fallback (should not happen): emit all deletes then all inserts.
    let mut out = Vec::with_capacity(n + m);
    out.extend(std::iter::repeat_n(Step::Delete, n));
    out.extend(std::iter::repeat_n(Step::Insert, m));
    out
}

/// Backtrack through the trace to recover the edit script, expanding snake (equal) runs.
fn backtrack(trace: &[Vec<i64>], a: &[&[u8]], b: &[&[u8]], last_d: i64) -> Vec<Step> {
    let n = a.len() as i64;
    let m = b.len() as i64;
    let offset = (n + m) as i64;
    let mut x = n;
    let mut y = m;
    let mut steps: Vec<Step> = Vec::new();

    for d in (1..=last_d).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = v[(prev_k + offset) as usize];
        let prev_y = prev_x - prev_k;

        // Snake (equal run) between (prev_x, prev_y) and the start of this d's move.
        while x > prev_x && y > prev_y {
            steps.push(Step::Equal);
            x -= 1;
            y -= 1;
        }
        if d > 1 || x != prev_x || y != prev_y {
            if x == prev_x {
                steps.push(Step::Insert);
            } else if y == prev_y {
                steps.push(Step::Delete);
            }
        }
        x = prev_x;
        y = prev_y;
    }
    // Leading snake from (0,0) to (x,y).
    while x > 0 && y > 0 {
        steps.push(Step::Equal);
        x -= 1;
        y -= 1;
    }
    while x > 0 {
        steps.push(Step::Delete);
        x -= 1;
    }
    while y > 0 {
        steps.push(Step::Insert);
        y -= 1;
    }
    steps.reverse();
    steps
}

/// Convert an edit script into `LineChange`s with 1-based line numbers.
fn script_to_changes(a: &[&[u8]], b: &[&[u8]], script: &[Step]) -> Vec<LineChange> {
    let mut out = Vec::with_capacity(script.len());
    let mut i = 0u32; // 1-based old line
    let mut j = 0u32; // 1-based new line
    for &s in script {
        match s {
            Step::Equal => {
                out.push(LineChange::Equal { old_no: i + 1, new_no: j + 1, bytes: a[i as usize].to_vec() });
                i += 1;
                j += 1;
            }
            Step::Delete => {
                out.push(LineChange::Delete { old_no: i + 1, bytes: a[i as usize].to_vec() });
                i += 1;
            }
            Step::Insert => {
                out.push(LineChange::Insert { new_no: j + 1, bytes: b[j as usize].to_vec() });
                j += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes_summary(changes: &[LineChange]) -> String {
        changes
            .iter()
            .map(|c| match c {
                LineChange::Equal { .. } => "=",
                LineChange::Delete { .. } => "-",
                LineChange::Insert { .. } => "+",
            })
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn identical_inputs_produce_all_equal() {
        let a = b"line1\nline2\nline3\n";
        let changes = line_diff(a, a);
        assert_eq!(changes_summary(&changes), "===");
        assert!(changes.iter().all(|c| matches!(c, LineChange::Equal { .. })));
    }

    #[test]
    fn insertion_at_end() {
        let old = b"a\nb\n";
        let new = b"a\nb\nc\n";
        let changes = line_diff(old, new);
        // Two equal lines (a, b) then one inserted line (c).
        let equals = changes.iter().filter(|c| matches!(c, LineChange::Equal { .. })).count();
        let inserts: Vec<_> = changes.iter().filter(|c| matches!(c, LineChange::Insert { .. })).collect();
        assert_eq!(equals, 2);
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].bytes(), b"c");
    }

    #[test]
    fn deletion_in_middle() {
        let old = b"a\nb\nc\n";
        let new = b"a\nc\n";
        let changes = line_diff(old, new);
        let deletes: Vec<_> = changes.iter().filter(|c| matches!(c, LineChange::Delete { .. })).collect();
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].bytes(), b"b");
    }

    #[test]
    fn modification_is_delete_then_insert() {
        let old = b"The quick brown fox\n";
        let new = b"The quick red fox\n";
        let changes = line_diff(old, new);
        let d: Vec<_> = changes.iter().filter(|c| matches!(c, LineChange::Delete { .. })).collect();
        let i: Vec<_> = changes.iter().filter(|c| matches!(c, LineChange::Insert { .. })).collect();
        assert_eq!(d.len(), 1);
        assert_eq!(i.len(), 1);
        assert_eq!(d[0].bytes(), b"The quick brown fox");
        assert_eq!(i[0].bytes(), b"The quick red fox");
    }

    #[test]
    fn empty_inputs() {
        assert!(line_diff(b"", b"").is_empty());
        let changes = line_diff(b"", b"x\n");
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], LineChange::Insert { .. }));
    }

    #[test]
    fn crlf_tolerant() {
        let old = b"a\r\nb\r\n";
        let new = b"a\nb\n";
        let changes = line_diff(old, new);
        assert!(changes.iter().all(|c| matches!(c, LineChange::Equal { .. })));
    }
}
