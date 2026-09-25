//! Line diff for the internal viewer: Myers' algorithm, then the two
//! sides paired up into rows a screen can show side by side.
//!
//! Myers finds the shortest edit script - the fewest lines to delete
//! and insert - which is what makes a diff read like a description of
//! the change rather than a list of every line that moved. This is the
//! linear-space form (the "middle snake", divide and conquer): memory
//! grows with the files, not with the square of how much changed.
//!
//! Lines are compared by a key - the text, or the text with its blanks
//! or its case taken out - interned to a number, so the options that
//! ignore things are a matter of how the key is made, and comparing
//! two lines is comparing two integers.

use std::collections::HashMap;
use std::ops::Range;

/// One row of the side-by-side view: which line of each file it shows,
/// and whether the two are the same text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub left: Option<usize>,
    pub right: Option<usize>,
    /// Both sides the same as far as the options care - or a line the
    /// options say to ignore: the context between changes.
    pub same: bool,
}

/// What does not count as a difference, as `diff -w`, `-i` and `-B`
/// have it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// All whitespace: `a  =b` is `a = b`.
    pub ignore_space: bool,
    pub ignore_case: bool,
    /// Blank lines added or removed.
    pub ignore_blank: bool,
}

/// The furthest one pass of the middle-snake search goes before giving
/// up on the stretch in hand and calling all of it changed. Two files
/// that far apart are "all of it changed" to anyone reading them, and
/// the search costs this much times their length.
const MAX_D: usize = 4096;

/// Pair two files up line by line, every line counting.
pub fn rows(left: &[String], right: &[String]) -> Vec<Row> {
    rows_with(left, right, Options::default())
}

/// Pair two files up line by line. Equal runs align; a run of deletions
/// next to a run of insertions is shown as changed lines side by side,
/// which is what makes a small edit read as one line changing rather
/// than as one line leaving and another arriving.
pub fn rows_with(left: &[String], right: &[String], opts: Options) -> Vec<Row> {
    let mut ids: HashMap<String, u32> = HashMap::new();
    let mut key = |line: &str| -> u32 {
        let mut text: String = match opts.ignore_space {
            true => line.chars().filter(|c| !c.is_whitespace()).collect(),
            false => line.to_string(),
        };
        if opts.ignore_case {
            text = text.to_lowercase();
        }
        let next = ids.len() as u32;
        *ids.entry(text).or_insert(next)
    };
    let blank = |line: &str| line.trim().is_empty();
    // blank lines ignored: they are left out of the diff, and put back
    // afterwards as context, beside whatever they sat beside
    let keep = |lines: &[String]| -> Vec<usize> {
        (0..lines.len())
            .filter(|&i| !(opts.ignore_blank && blank(&lines[i])))
            .collect()
    };
    let (lkept, rkept) = (keep(left), keep(right));
    let a: Vec<u32> = lkept.iter().map(|&i| key(&left[i])).collect();
    let b: Vec<u32> = rkept.iter().map(|&i| key(&right[i])).collect();
    let mut script = Vec::new();
    compare(&a, &b, 0, 0, &mut script);
    let paired = pair(script);
    if lkept.len() == left.len() && rkept.len() == right.len() {
        return paired;
    }
    // map back to the real line numbers, with the blank lines between
    let mut out = Vec::with_capacity(left.len().max(right.len()));
    let (mut next_l, mut next_r) = (0usize, 0usize);
    let flush_blanks = |out: &mut Vec<Row>,
                        next_l: &mut usize,
                        next_r: &mut usize,
                        upto_l: usize,
                        upto_r: usize| {
        let lb: Vec<usize> = (*next_l..upto_l).collect();
        let rb: Vec<usize> = (*next_r..upto_r).collect();
        for i in 0..lb.len().max(rb.len()) {
            out.push(Row {
                left: lb.get(i).copied(),
                right: rb.get(i).copied(),
                same: true,
            });
        }
        *next_l = upto_l.max(*next_l);
        *next_r = upto_r.max(*next_r);
    };
    for row in paired {
        let l = row.left.map(|i| lkept[i]);
        let r = row.right.map(|i| rkept[i]);
        let (upto_l, upto_r) = (l.unwrap_or(next_l), r.unwrap_or(next_r));
        flush_blanks(&mut out, &mut next_l, &mut next_r, upto_l, upto_r);
        out.push(Row {
            left: l,
            right: r,
            same: row.same,
        });
        next_l = l.map_or(next_l, |i| i + 1);
        next_r = r.map_or(next_r, |i| i + 1);
    }
    flush_blanks(&mut out, &mut next_l, &mut next_r, left.len(), right.len());
    out
}

/// Deletions and insertions next to each other become changed rows,
/// side by side.
fn pair(script: Vec<Edit>) -> Vec<Row> {
    let mut rows = Vec::with_capacity(script.len());
    let mut dels: Vec<usize> = Vec::new();
    let mut ins: Vec<usize> = Vec::new();
    let flush = |rows: &mut Vec<Row>, dels: &mut Vec<usize>, ins: &mut Vec<usize>| {
        for i in 0..dels.len().max(ins.len()) {
            rows.push(Row {
                left: dels.get(i).copied(),
                right: ins.get(i).copied(),
                same: false,
            });
        }
        dels.clear();
        ins.clear();
    };
    for edit in script {
        match edit {
            Edit::Keep(a, b) => {
                flush(&mut rows, &mut dels, &mut ins);
                rows.push(Row {
                    left: Some(a),
                    right: Some(b),
                    same: true,
                });
            }
            Edit::Delete(a) => dels.push(a),
            Edit::Insert(b) => ins.push(b),
        }
    }
    flush(&mut rows, &mut dels, &mut ins);
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

/// The lines a block of rows covers on each side: what taking one
/// side's version of it means. A side with no lines in the block gets
/// the empty range where they would go.
pub fn block_lines(rows: &[Row], (start, end): (usize, usize)) -> (Range<usize>, Range<usize>) {
    let side = |pick: fn(&Row) -> Option<usize>| -> Range<usize> {
        let inside: Vec<usize> = rows[start..end].iter().filter_map(pick).collect();
        match (inside.first(), inside.last()) {
            (Some(&first), Some(&last)) => first..last + 1,
            _ => {
                let at = rows[..start]
                    .iter()
                    .rev()
                    .find_map(pick)
                    .map_or(0, |i| i + 1);
                at..at
            }
        }
    };
    (side(|r| r.left), side(|r| r.right))
}

/// What changed inside a pair of lines, word by word, as character
/// ranges on each side - where a changed row's highlight goes. A word
/// is a run of letters, digits and underscores; anything else is a word
/// of one character.
// a list of ranges that happens to hold one, not a range's contents
#[allow(clippy::single_range_in_vec_init)]
pub fn inline(left: &str, right: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    inline_with(left, right, Options::default())
}

/// [`inline`] under the same options as the rows: with `-w` whitespace
/// is no word at all, and with `-i` case is no difference - so what the
/// options call the same is not lit up inside a line that changed in
/// some other way.
#[allow(clippy::single_range_in_vec_init)]
pub fn inline_with(
    left: &str,
    right: &str,
    opts: Options,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let keep = |(text, _): &(&str, Range<usize>)| {
        !(opts.ignore_space && text.chars().all(char::is_whitespace))
    };
    let mut lt = tokens(left);
    let mut rt = tokens(right);
    lt.retain(keep);
    rt.retain(keep);
    // too long to be worth it: all of it changed
    if lt.len() + rt.len() > 4000 {
        return (
            vec![0..left.chars().count()],
            vec![0..right.chars().count()],
        );
    }
    let mut ids: HashMap<String, u32> = HashMap::new();
    let mut id = |t: &str| {
        let next = ids.len() as u32;
        let key = match opts.ignore_case {
            true => t.to_lowercase(),
            false => t.to_string(),
        };
        *ids.entry(key).or_insert(next)
    };
    let a: Vec<u32> = lt.iter().map(|&(t, _)| id(t)).collect();
    let b: Vec<u32> = rt.iter().map(|&(t, _)| id(t)).collect();
    let mut script = Vec::new();
    compare(&a, &b, 0, 0, &mut script);
    let mut out = (Vec::new(), Vec::new());
    let push = |ranges: &mut Vec<Range<usize>>, r: Range<usize>| match ranges.last_mut() {
        Some(last) if last.end == r.start => last.end = r.end,
        _ => ranges.push(r),
    };
    for edit in script {
        match edit {
            Edit::Keep(..) => {}
            Edit::Delete(i) => push(&mut out.0, lt[i].1.clone()),
            Edit::Insert(j) => push(&mut out.1, rt[j].1.clone()),
        }
    }
    out
}

/// A line cut into words and single characters, each with its
/// character range.
fn tokens(line: &str) -> Vec<(&str, Range<usize>)> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let mut chars = line.char_indices().enumerate().peekable();
    while let Some((ci, (bi, c))) = chars.next() {
        let (mut cend, mut bend) = (ci + 1, bi + c.len_utf8());
        if word(c) {
            while let Some(&(cj, (bj, d))) = chars.peek() {
                if !word(d) {
                    break;
                }
                cend = cj + 1;
                bend = bj + d.len_utf8();
                chars.next();
            }
        }
        out.push((&line[bi..bend], ci..cend));
    }
    out
}

/// Whether bytes look like something other than text: a NUL in the
/// first 8 KiB, which is `diff`'s and `git`'s test too.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

enum Edit {
    Keep(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// The edit script turning `a` into `b`, appended to `out`; `ao` and
/// `bo` are where these slices start in the whole files. The common
/// prefix and suffix come off first - which is what makes a one-line
/// change in a large file cost almost nothing - and the rest is split
/// at the middle snake and done in two halves.
fn compare(a: &[u32], b: &[u32], ao: usize, bo: usize, out: &mut Vec<Edit>) {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    out.extend((0..prefix).map(|i| Edit::Keep(ao + i, bo + i)));
    let (ma, mb) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let (mao, mbo) = (ao + prefix, bo + prefix);
    if ma.is_empty() {
        out.extend((0..mb.len()).map(|j| Edit::Insert(mbo + j)));
    } else if mb.is_empty() {
        out.extend((0..ma.len()).map(|i| Edit::Delete(mao + i)));
    } else {
        match middle_snake(ma, mb) {
            Some((x, y, u, v)) if (x, y) != (0, 0) || (u, v) != (ma.len(), mb.len()) => {
                compare(&ma[..x], &mb[..y], mao, mbo, out);
                out.extend((0..u - x).map(|i| Edit::Keep(mao + x + i, mbo + y + i)));
                compare(&ma[u..], &mb[v..], mao + u, mbo + v, out);
            }
            // too far apart to be worth proving line by line - or the
            // split would not make the problem any smaller
            _ => {
                out.extend((0..ma.len()).map(|i| Edit::Delete(mao + i)));
                out.extend((0..mb.len()).map(|j| Edit::Insert(mbo + j)));
            }
        }
    }
    let (sa, sb) = (ao + a.len() - suffix, bo + b.len() - suffix);
    out.extend((0..suffix).map(|i| Edit::Keep(sa + i, sb + i)));
}

/// Myers' middle snake: search from both ends at once until the two
/// searches meet, and return the diagonal run where they did, as
/// `(x, y)` to `(u, v)`. `None` past [`MAX_D`].
fn middle_snake(a: &[u32], b: &[u32]) -> Option<(usize, usize, usize, usize)> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let delta = n - m;
    let odd = delta & 1 != 0;
    let dmax = ((n + m + 1) / 2).min(MAX_D as isize);
    let off = (n + m + 2) as usize;
    let size = 2 * off + 1;
    let mut vf = vec![0isize; size];
    let mut vb = vec![0isize; size];
    let at = |k: isize| (off as isize + k) as usize;
    for d in 0..=dmax {
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && vf[at(k - 1)] < vf[at(k + 1)]) {
                vf[at(k + 1)]
            } else {
                vf[at(k - 1)] + 1
            };
            let mut y = x - k;
            let (x0, y0) = (x, y);
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            vf[at(k)] = x;
            let kr = delta - k;
            if odd && kr > -d && kr < d && x + vb[at(kr)] >= n {
                return Some((x0 as usize, y0 as usize, x as usize, y as usize));
            }
            k += 2;
        }
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && vb[at(k - 1)] < vb[at(k + 1)]) {
                vb[at(k + 1)]
            } else {
                vb[at(k - 1)] + 1
            };
            let mut y = x - k;
            let (x0, y0) = (x, y);
            while x < n && y < m && a[(n - 1 - x) as usize] == b[(m - 1 - y) as usize] {
                x += 1;
                y += 1;
            }
            vb[at(k)] = x;
            let kf = delta - k;
            if !odd && kf >= -d && kf <= d && x + vf[at(kf)] >= n {
                return Some((
                    (n - x) as usize,
                    (m - y) as usize,
                    (n - x0) as usize,
                    (m - y0) as usize,
                ));
            }
            k += 2;
        }
    }
    None
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

    /// Every line of both files shows exactly once, in order.
    fn well_formed(rows: &[Row], left: &[String], right: &[String]) {
        let l: Vec<usize> = rows.iter().filter_map(|r| r.left).collect();
        let r: Vec<usize> = rows.iter().filter_map(|r| r.right).collect();
        assert_eq!(l, (0..left.len()).collect::<Vec<_>>());
        assert_eq!(r, (0..right.len()).collect::<Vec<_>>());
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
    fn the_edit_script_is_the_shortest() {
        // the classic: abcabba -> cbabac keeps four lines, and every
        // line is accounted for once
        let (l, r) = (lines("a\nb\nc\na\nb\nb\na"), lines("c\nb\na\nb\na\nc"));
        let paired = rows(&l, &r);
        well_formed(&paired, &l, &r);
        let kept = paired.iter().filter(|r| r.same).count();
        assert_eq!(kept, 4, "{:?}", shape(&paired, &l, &r));
        // and scattered changes through a longer file
        let left: Vec<String> = (0..500).map(|i| format!("{}", i % 7)).collect();
        let right: Vec<String> = (0..480).map(|i| format!("{}", (i * 3) % 7)).collect();
        well_formed(&rows(&left, &right), &left, &right);
    }

    #[test]
    fn a_small_change_in_a_large_file_is_cheap() {
        let mut left: Vec<String> = (0..20_000).map(|i| format!("line {i}")).collect();
        let mut right = left.clone();
        right[10_000] = "changed".into();
        let paired = rows(&left, &right);
        assert_eq!(blocks(&paired), [(10_000, 10_001)]);
        assert_eq!(paired.len(), 20_000);
        left.clear();
        assert_eq!(rows(&left, &right).len(), 20_000);
        right.clear();
        assert!(rows(&left, &right).is_empty());
    }

    #[test]
    fn two_files_with_nothing_in_common_are_quick() {
        // the old trace kept a copy of V per edit: at this size that
        // was gigabytes; now it is two arrays and a cap on the search
        let left: Vec<String> = (0..60_000).map(|i| format!("l{i}")).collect();
        let right: Vec<String> = (0..60_000).map(|i| format!("r{i}")).collect();
        let started = std::time::Instant::now();
        let paired = rows(&left, &right);
        well_formed(&paired, &left, &right);
        assert!(paired.iter().all(|r| !r.same));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn options_ignore_what_they_say() {
        let (l, r) = (lines("int a=1;\nFOO\nx"), lines("int  a = 1;\nfoo\n\nx"));
        let plain = rows(&l, &r);
        assert_eq!(blocks(&plain).len(), 1);
        let all = Options {
            ignore_space: true,
            ignore_case: true,
            ignore_blank: true,
        };
        let paired = rows_with(&l, &r, all);
        well_formed(&paired, &l, &r);
        assert!(blocks(&paired).is_empty(), "{:?}", shape(&paired, &l, &r));
        // blank lines alone: the blank ones are context, the rest counts
        let blank = Options {
            ignore_blank: true,
            ..Options::default()
        };
        let (l, r) = (lines("a\nb"), lines("a\n\n\nb\nc"));
        let paired = rows_with(&l, &r, blank);
        well_formed(&paired, &l, &r);
        assert_eq!(shape(&paired, &l, &r), ["a|a", "-|", "-|", "b|b", "-|c*"]);
    }

    #[test]
    fn a_block_names_the_lines_each_side_has() {
        let (l, r) = (lines("a\nb\nc"), lines("a\nB\nX\nc"));
        let paired = rows(&l, &r);
        let block = blocks(&paired)[0];
        assert_eq!(block_lines(&paired, block), (1..2, 1..3));
        // a pure insertion: the left's empty range is where it would go
        let (l, r) = (lines("a\nc"), lines("a\nb\nc"));
        let paired = rows(&l, &r);
        assert_eq!(block_lines(&paired, blocks(&paired)[0]), (1..1, 1..2));
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn inside_a_line_the_changed_words_are_found() {
        let (l, r) = inline("let x = old_name(1);", "let x = new_name(1, 2);");
        assert_eq!(l, [8..16]);
        assert_eq!(r, [8..16, 18..21]);
        assert!(is_binary(b"ab\0cd"));
        assert!(!is_binary("plain text".as_bytes()));
    }

    #[test]
    fn the_inline_highlight_follows_the_options() {
        let (l, r) = ("let  a = B;", "let a = b; x");
        let (lw, rw) = inline(l, r);
        assert!(!lw.is_empty() && !rw.is_empty());
        let opts = Options {
            ignore_space: true,
            ignore_case: true,
            ..Options::default()
        };
        let (lw, rw) = inline_with(l, r, opts);
        // only the added word is a change: not the doubled space, not B
        assert!(lw.is_empty(), "{lw:?}");
        let lit: Vec<String> = rw
            .iter()
            .map(|w| r.chars().skip(w.start).take(w.len()).collect())
            .collect();
        assert_eq!(lit, ["x"]);
    }
}
