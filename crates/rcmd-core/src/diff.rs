//! Line diff for the internal viewer: Myers' algorithm, then the two
//! sides paired up into rows a screen can show side by side.
//!
//! Myers finds the shortest edit script - the fewest lines to delete
//! and insert - which is what makes a diff read like a description of
//! the change rather than a list of every line that moved.

/// One row of the side-by-side view: which line of each file it shows,
/// and whether the two are the same text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub left: Option<usize>,
    pub right: Option<usize>,
    /// Both sides present and equal: the context between changes.
    pub same: bool,
}

/// Beyond this many differing lines the diff is not worth computing:
/// two files with nothing in common produce an edit script as long as
/// both of them put together, and the answer ("all of it changed") is
/// one the reader can already see.
const MAX_EDITS: usize = 20_000;

/// Pair two files up line by line. Equal runs align; a run of deletions
/// next to a run of insertions is shown as changed lines side by side,
/// which is what makes a small edit read as one line changing rather
/// than as one line leaving and another arriving.
pub fn rows(left: &[String], right: &[String]) -> Vec<Row> {
    let script = myers(left, right);
    let mut rows = Vec::with_capacity(script.len());
    let mut pending_left: Vec<usize> = Vec::new();
    let mut pending_right: Vec<usize> = Vec::new();
    let flush = |rows: &mut Vec<Row>, dels: &mut Vec<usize>, ins: &mut Vec<usize>| {
        let paired = dels.len().min(ins.len());
        for i in 0..paired {
            rows.push(Row {
                left: Some(dels[i]),
                right: Some(ins[i]),
                same: false,
            });
        }
        for &at in &dels[paired..] {
            rows.push(Row {
                left: Some(at),
                right: None,
                same: false,
            });
        }
        for &at in &ins[paired..] {
            rows.push(Row {
                left: None,
                right: Some(at),
                same: false,
            });
        }
        dels.clear();
        ins.clear();
    };
    for edit in script {
        match edit {
            Edit::Keep(a, b) => {
                flush(&mut rows, &mut pending_left, &mut pending_right);
                rows.push(Row {
                    left: Some(a),
                    right: Some(b),
                    same: true,
                });
            }
            Edit::Delete(a) => pending_left.push(a),
            Edit::Insert(b) => pending_right.push(b),
        }
    }
    flush(&mut rows, &mut pending_left, &mut pending_right);
    rows
}

/// Where the changes are, as (first row, row past the last) pairs -
/// what "next difference" steps through.
pub fn blocks(rows: &[Row]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if row.same {
            continue;
        }
        match out.last_mut() {
            Some((_, end)) if *end == i => *end = i + 1,
            _ => out.push((i, i + 1)),
        }
    }
    out
}

enum Edit {
    Keep(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// Myers' greedy algorithm, keeping one V array per edit distance so
/// the path can be walked back afterwards. The common prefix and
/// suffix are taken off first, which is what makes a one-line change
/// in a large file cost almost nothing.
fn myers(left: &[String], right: &[String]) -> Vec<Edit> {
    let (n, m) = (left.len(), right.len());
    let prefix = left
        .iter()
        .zip(right.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = left[prefix..]
        .iter()
        .rev()
        .zip(right[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (a, b) = (&left[prefix..n - suffix], &right[prefix..m - suffix]);
    let mut script: Vec<Edit> = (0..prefix).map(|i| Edit::Keep(i, i)).collect();
    script.extend(middle(a, b, prefix));
    script.extend((0..suffix).map(|i| Edit::Keep(n - suffix + i, m - suffix + i)));
    script
}

fn middle(a: &[String], b: &[String], offset: usize) -> Vec<Edit> {
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return (0..m).map(|j| Edit::Insert(offset + j)).collect();
    }
    if m == 0 {
        return (0..n).map(|i| Edit::Delete(offset + i)).collect();
    }
    let max = (n + m).min(MAX_EDITS);
    let mut v = vec![0isize; 2 * max + 3];
    let k0 = max as isize + 1; // index of k = 0
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for d in 0..=max as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let idx = (k0 + k) as usize;
            // take the step that has got furthest: down (an insert) or
            // right (a delete)
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while (x as usize) < n && (y as usize) < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x as usize >= n && y as usize >= m {
                return backtrack(&trace, a, b, offset, k0);
            }
            k += 2;
        }
    }
    // beyond MAX_EDITS: say the whole of both sides changed rather
    // than spend the rest of the day proving it line by line
    let mut out: Vec<Edit> = (0..n).map(|i| Edit::Delete(offset + i)).collect();
    out.extend((0..m).map(|j| Edit::Insert(offset + j)));
    out
}

/// Walk the recorded V arrays back from the end to the start, which
/// gives the edits in reverse; they are flipped before returning.
fn backtrack(
    trace: &[Vec<isize>],
    a: &[String],
    b: &[String],
    offset: usize,
    k0: isize,
) -> Vec<Edit> {
    let mut out = Vec::new();
    let (mut x, mut y) = (a.len() as isize, b.len() as isize);
    for (d, v) in trace.iter().enumerate().rev() {
        let d = d as isize;
        let k = x - y;
        let idx = (k0 + k) as usize;
        let down = k == -d || (k != d && v[idx - 1] < v[idx + 1]);
        let prev_k = if down { k + 1 } else { k - 1 };
        let prev_x = v[(k0 + prev_k) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            out.push(Edit::Keep(offset + x as usize, offset + y as usize));
        }
        if d > 0 {
            if down {
                y -= 1;
                out.push(Edit::Insert(offset + y as usize));
            } else {
                x -= 1;
                out.push(Edit::Delete(offset + x as usize));
            }
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    fn shape(rows: &[Row], left: &[String], right: &[String]) -> Vec<String> {
        rows.iter()
            .map(|r| {
                let l = r.left.map(|i| left[i].as_str()).unwrap_or("-");
                let rr = r.right.map(|i| right[i].as_str()).unwrap_or("-");
                format!("{l}|{rr}{}", if r.same { "" } else { "*" })
            })
            .collect()
    }

    #[test]
    fn a_changed_line_shows_as_one_row() {
        let (l, r) = (lines("a\nb\nc"), lines("a\nB\nc"));
        assert_eq!(shape(&rows(&l, &r), &l, &r), ["a|a", "b|B*", "c|c"]);
    }

    #[test]
    fn insertions_and_deletions_keep_their_side() {
        let (l, r) = (lines("a\nc"), lines("a\nb\nc"));
        assert_eq!(shape(&rows(&l, &r), &l, &r), ["a|a", "-|b*", "c|c"]);
        let (l, r) = (lines("a\nb\nc"), lines("a\nc"));
        assert_eq!(shape(&rows(&l, &r), &l, &r), ["a|a", "b|-*", "c|c"]);
    }

    #[test]
    fn identical_files_are_all_context() {
        let (l, r) = (lines("one\ntwo"), lines("one\ntwo"));
        let paired = rows(&l, &r);
        assert!(paired.iter().all(|row| row.same));
        assert!(blocks(&paired).is_empty());
    }

    #[test]
    fn blocks_group_runs_of_changed_rows() {
        let (l, r) = (lines("a\nb\nc\nd\ne"), lines("a\nB\nC\nd\nE"));
        let paired = rows(&l, &r);
        assert_eq!(blocks(&paired), [(1, 3), (4, 5)]);
    }

    #[test]
    fn a_small_change_in_a_large_file_is_cheap() {
        // 20k identical lines with one changed in the middle: the
        // prefix and suffix are trimmed, so this is not 20k edits
        let mut left: Vec<String> = (0..20_000).map(|i| format!("line {i}")).collect();
        let mut right = left.clone();
        right[10_000] = "changed".into();
        let paired = rows(&left, &right);
        assert_eq!(blocks(&paired), [(10_000, 10_001)]);
        assert_eq!(paired.len(), 20_000);
        // ...and one file being empty is not a special case
        left.clear();
        assert_eq!(rows(&left, &right).len(), 20_000);
        right.clear();
        assert!(rows(&left, &right).is_empty());
    }
}
