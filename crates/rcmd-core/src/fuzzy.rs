//! Fuzzy matching of paths, as a "go to file" box ranks them: the
//! letters typed must all be there, in order, and the fewer the gaps,
//! the more of them start a word, and the more of them are in the file
//! name rather than the directories above it, the better the match.
//!
//! Case follows the smart-case rule the quick search uses: a capital in
//! the pattern makes the whole match case-sensitive.

/// A match: how good it is, and which characters of the path made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub score: i64,
    /// Character indices into the path, ascending.
    pub positions: Vec<usize>,
}

const MATCHED: i64 = 16;
const WORD_START: i64 = 10;
/// The first character of a component, right after a `/`.
const COMPONENT_START: i64 = 12;
/// A letter right after the last one: more than a word start is worth,
/// or `f_o_o` would beat `foo` for "foo".
const RUN: i64 = 12;
const GAP: i64 = 1;
/// Every matched character in the file name rather than its directory.
const IN_NAME: i64 = 4;

/// How well `pattern` matches `path`, or `None` when its letters are
/// not all there in order. Blanks in the pattern are ignored, so
/// `src main` is `srcmain`.
pub fn fuzzy_match(pattern: &str, path: &str) -> Option<Match> {
    let pat: Vec<char> = pattern.chars().filter(|c| !c.is_whitespace()).collect();
    if pat.is_empty() {
        return Some(Match {
            score: 0,
            positions: Vec::new(),
        });
    }
    let sensitive = pat.iter().any(|c| c.is_uppercase());
    let hay: Vec<char> = path.chars().collect();
    let same = |a: char, b: char| a == b || (!sensitive && a.to_lowercase().eq(b.to_lowercase()));
    let name_at = hay.iter().rposition(|&c| c == '/').map_or(0, |i| i + 1);
    // the best of two tries: the tightest window in the whole path, and
    // one inside the file name alone, which is usually what was meant
    let whole = window(&pat, &hay, 0, &same);
    let named = window(&pat, &hay, name_at, &same);
    [whole, named]
        .into_iter()
        .flatten()
        .map(|positions| Match {
            score: score(&hay, &positions, name_at),
            positions,
        })
        .max_by_key(|m| m.score)
}

/// fzf's first algorithm: the earliest place the pattern ends, then
/// back from there to the latest place it can start, which gives the
/// shortest stretch holding every letter. Searching begins at `from`.
fn window(
    pat: &[char],
    hay: &[char],
    from: usize,
    same: &dyn Fn(char, char) -> bool,
) -> Option<Vec<usize>> {
    let mut at = from;
    let mut end = None;
    for &p in pat {
        let found = (at..hay.len()).find(|&i| same(hay[i], p))?;
        end = Some(found);
        at = found + 1;
    }
    let end = end?;
    let mut positions = vec![0; pat.len()];
    let mut at = end + 1;
    for (k, &p) in pat.iter().enumerate().rev() {
        let found = (from..at).rev().find(|&i| same(hay[i], p))?;
        positions[k] = found;
        at = found;
    }
    Some(positions)
}

fn score(hay: &[char], positions: &[usize], name_at: usize) -> i64 {
    let mut total = 0;
    for (k, &i) in positions.iter().enumerate() {
        total += MATCHED;
        let prev = i.checked_sub(1).map(|p| hay[p]);
        total += match prev {
            None | Some('/') => COMPONENT_START,
            Some(p) if !p.is_alphanumeric() => WORD_START,
            // camelCase: a capital after a small letter starts a word
            Some(p) if p.is_lowercase() && hay[i].is_uppercase() => WORD_START,
            _ => 0,
        };
        if k > 0 {
            let gap = (i - positions[k - 1] - 1) as i64;
            total += if gap == 0 { RUN } else { -GAP * gap.min(16) };
        }
        if i >= name_at {
            total += IN_NAME;
        }
    }
    // between two equal matches, the shorter path is the likelier one
    total - (hay.len() as i64) / 8
}

/// The candidates that match, best first: indices into `paths`. Ties
/// keep the order the candidates came in.
pub fn rank<'a>(pattern: &str, paths: impl IntoIterator<Item = &'a str>) -> Vec<usize> {
    let mut scored: Vec<(i64, usize)> = paths
        .into_iter()
        .enumerate()
        .filter_map(|(i, path)| fuzzy_match(pattern, path).map(|m| (m.score, i)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_in_order_or_nothing() {
        assert!(fuzzy_match("mrs", "src/main.rs").is_some());
        assert!(fuzzy_match("srm", "src/main.rs").is_some());
        assert!(fuzzy_match("zzz", "src/main.rs").is_none());
        // order matters
        assert!(fuzzy_match("sm", "main.rs").is_none());
    }

    #[test]
    fn the_file_name_beats_the_directories() {
        let paths = ["main/deep/other.txt", "src/deep/main.rs"];
        let ranked = rank("main", paths.iter().copied());
        assert_eq!(ranked, [1, 0]);
    }

    #[test]
    fn a_run_beats_scattered_letters() {
        let paths = ["a/f_o_o_bar.rs", "a/foobar.rs"];
        assert_eq!(rank("foo", paths.iter().copied()), [1, 0]);
    }

    #[test]
    fn word_starts_count() {
        // f and c start words in find_cmd, but sit mid-word in afcx
        let paths = ["xafcx.rs", "find_cmd.rs"];
        assert_eq!(rank("fc", paths.iter().copied()), [1, 0]);
    }

    #[test]
    fn a_capital_asks_for_case() {
        assert!(fuzzy_match("readme", "README.md").is_some());
        assert!(fuzzy_match("README", "readme.md").is_none());
    }

    #[test]
    fn positions_are_the_tightest_window() {
        let m = fuzzy_match("ab", "a-x-a-b").unwrap();
        assert_eq!(m.positions, [4, 6]);
    }

    #[test]
    fn blanks_are_ignored_and_empty_matches_everything() {
        assert!(fuzzy_match("src main", "src/main.rs").is_some());
        assert_eq!(rank("", ["b", "a"].iter().copied()), [0, 1]);
    }
}
