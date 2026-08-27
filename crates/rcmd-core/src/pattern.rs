//! What mc's select, unselect and filter dialogs ask for: a pattern and
//! the three answers that change what it means. One type for all three,
//! because in mc they are the same dialog with a different title.
//!
//! A shell pattern is Far's *mask list* rather than mc's single glob:
//! `*.c,*.h` is either of them and `*.c,*.h|*_test.*` is either of them
//! with a second list taken back out. mc's plain `*.rs` is the one-mask
//! case of it and keeps meaning what it meant. A regular expression is
//! left alone, where `|` is the alternation it has always been.

use std::time::{Duration, SystemTime};

use crate::entry::Entry;
use crate::glob::glob_match;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    pub text: String,
    /// Shell wildcards (`*`, `?`). Off = a regular expression, which is
    /// what mc offers behind the same switch.
    pub shell: bool,
    pub case_sensitive: bool,
    /// Leave directories alone. A filter that hid them would strand you
    /// in a directory with no way down, which is why mc has the switch
    /// on by default and so does rcmd.
    pub files_only: bool,
    /// How big, as `>1M`, `<=100k`, `1M-2G`. Empty is no limit. mc asks
    /// nothing of the sort; DOS Navigator's select dialog does, and
    /// "everything over a hundred megabytes" is otherwise a trip
    /// through find and panelize.
    pub size: String,
    /// How recently it was touched, as an age: `30m`, `24h`, `7d`,
    /// `2w`. Empty is no limit.
    pub newer: String,
}

impl Default for Pattern {
    fn default() -> Self {
        Pattern {
            text: "*".into(),
            shell: true,
            case_sensitive: true,
            files_only: true,
            size: String::new(),
            newer: String::new(),
        }
    }
}

impl std::fmt::Display for Pattern {
    /// How a panel says what it is filtering by, in one line: the
    /// pattern, and the answers that are not the usual ones.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.text)?;
        if !self.shell {
            write!(f, " (regex)")?;
        }
        if !self.case_sensitive {
            write!(f, " (any case)")?;
        }
        if !self.files_only {
            write!(f, " (dirs too)")?;
        }
        if !self.size.trim().is_empty() {
            write!(f, " (size {})", self.size.trim())?;
        }
        if !self.newer.trim().is_empty() {
            write!(f, " (newer than {})", self.newer.trim())?;
        }
        Ok(())
    }
}

/// A compiled [`Pattern`], ready to run against a name - and, where
/// the dialog asked for them, against the entry's size and age.
pub struct Matcher {
    name: NameMatcher,
    size: Option<SizeRange>,
    /// Anything older than this is out.
    newer: Option<SystemTime>,
}

/// Inclusive byte bounds, either end open.
type SizeRange = (Option<u64>, Option<u64>);

enum NameMatcher {
    Masks(Masks),
    Regex(regex::Regex),
}

/// `>1M`, `>=1M`, `<100k`, `<=100k`, `1M-2G`. The suffixes are the
/// panel's own: k, M, G, T, each 1024 of the one below. Empty means no
/// limit; anything else is the user's typing and worth quoting back.
fn parse_size(text: &str) -> Result<Option<SizeRange>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let bytes = |part: &str| -> Result<u64, String> {
        let part = part.trim();
        let (digits, scale) = match part.chars().last() {
            Some('k' | 'K') => (&part[..part.len() - 1], 1024u64),
            Some('m' | 'M') => (&part[..part.len() - 1], 1024 * 1024),
            Some('g' | 'G') => (&part[..part.len() - 1], 1024 * 1024 * 1024),
            Some('t' | 'T') => (&part[..part.len() - 1], 1024u64.pow(4)),
            _ => (part, 1),
        };
        digits
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("size: {part} is not a number of bytes"))
            .map(|n| n.saturating_mul(scale))
    };
    if let Some(rest) = text.strip_prefix(">=") {
        return Ok(Some((Some(bytes(rest)?), None)));
    }
    if let Some(rest) = text.strip_prefix("<=") {
        return Ok(Some((None, Some(bytes(rest)?))));
    }
    if let Some(rest) = text.strip_prefix('>') {
        return Ok(Some((Some(bytes(rest)?.saturating_add(1)), None)));
    }
    if let Some(rest) = text.strip_prefix('<') {
        let max = bytes(rest)?;
        return Ok(Some((None, Some(max.saturating_sub(1)))));
    }
    // a range, and the one place a '-' is not part of a number
    if let Some((from, to)) = text.split_once('-') {
        return Ok(Some((Some(bytes(from)?), Some(bytes(to)?))));
    }
    Err(format!("size: {text} - say >1M, <=100k or 1M-2G"))
}

/// An age: `30m`, `24h`, `7d`, `2w`. Turned into the instant a file has
/// to be newer than, which is what the entries carry.
fn parse_newer(text: &str, now: SystemTime) -> Result<Option<SystemTime>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let (digits, unit) = text.split_at(text.len() - 1);
    let seconds: u64 = match unit {
        "m" | "M" => 60,
        "h" | "H" => 3600,
        "d" | "D" => 86_400,
        "w" | "W" => 604_800,
        _ => return Err(format!("newer than: {text} - say 30m, 24h, 7d or 2w")),
    };
    let count: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("newer than: {text} - say 30m, 24h, 7d or 2w"))?;
    Ok(Some(
        now.checked_sub(Duration::from_secs(count.saturating_mul(seconds)))
            .unwrap_or(SystemTime::UNIX_EPOCH),
    ))
}

/// A compiled mask list: a name is in when any of `include` matches it
/// and none of `exclude` does. Far's language, and the one every glob
/// rcmd is given by hand is read in - the select and filter dialogs,
/// the find dialog's name, `[[highlight]]` and `[[open]]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Masks {
    /// Empty means everything, which is what `|*.o` asks for.
    include: Vec<String>,
    exclude: Vec<String>,
    /// The masks are lowercased already; the name has to be lowered on
    /// the way in.
    fold: bool,
}

/// Put several mask lists together into one: every include from every
/// list, and every exclude from every list taken back out. Two named
/// filters switched on at once is exactly that question - show me what
/// either of them shows, minus what either of them hides.
pub fn join_masks<'a>(lists: impl IntoIterator<Item = &'a str>) -> String {
    let (mut include, mut exclude) = (Vec::new(), Vec::new());
    // one list that includes everything makes the union everything,
    // however narrow the others are: `|*.o` shows all but the objects,
    // and adding "and also the .c files" to it takes nothing away
    let mut everything = false;
    for list in lists {
        let (left, right) = match list.split_once('|') {
            Some((left, right)) => (left, right),
            None => (list, ""),
        };
        everything |= left.split(',').all(|mask| mask.trim().is_empty());
        for (part, into) in [(left, &mut include), (right, &mut exclude)] {
            for mask in part.split(',').map(str::trim).filter(|m| !m.is_empty()) {
                if !into.contains(&mask.to_string()) {
                    into.push(mask.to_string());
                }
            }
        }
    }
    if everything {
        include.clear();
    }
    match exclude.is_empty() {
        true => include.join(","),
        false => format!("{}|{}", include.join(","), exclude.join(",")),
    }
}

impl Masks {
    /// Split a mask list into its two halves. The first `|` is the one
    /// that divides them; commas separate the masks on each side,
    /// whitespace around a mask is not part of it, and an empty mask is
    /// dropped rather than left to match nothing.
    pub fn parse(text: &str, fold: bool) -> Self {
        let (included, excluded) = match text.split_once('|') {
            Some((left, right)) => (left, right),
            None => (text, ""),
        };
        let list = |part: &str| -> Vec<String> {
            part.split(',')
                .map(str::trim)
                .filter(|mask| !mask.is_empty())
                .map(|mask| match fold {
                    true => mask.to_lowercase(),
                    false => mask.to_string(),
                })
                .collect()
        };
        Masks {
            include: list(included),
            exclude: list(excluded),
            fold,
        }
    }

    pub fn matches(&self, name: &str) -> bool {
        let name = match self.fold {
            true => std::borrow::Cow::Owned(name.to_lowercase()),
            false => std::borrow::Cow::Borrowed(name),
        };
        let matched =
            self.include.is_empty() || self.include.iter().any(|mask| glob_match(mask, &name));
        matched && !self.exclude.iter().any(|mask| glob_match(mask, &name))
    }

    /// Everything is in and nothing is taken back out, which is how a
    /// filter is cleared.
    pub fn is_open(&self) -> bool {
        self.exclude.is_empty()
            && self
                .include
                .iter()
                .all(|mask| mask.chars().all(|c| c == '*'))
    }
}

impl Pattern {
    /// Compile once per listing rather than once per entry. The error is
    /// the user's regular expression, so it is worth showing.
    pub fn compile(&self) -> Result<Matcher, String> {
        let name = if self.shell {
            NameMatcher::Masks(Masks::parse(&self.text, !self.case_sensitive))
        } else {
            NameMatcher::Regex(
                regex::RegexBuilder::new(&self.text)
                    .case_insensitive(!self.case_sensitive)
                    .build()
                    .map_err(|err| err.to_string())?,
            )
        };
        Ok(Matcher {
            name,
            size: parse_size(&self.size)?,
            newer: parse_newer(&self.newer, SystemTime::now())?,
        })
    }

    /// Whether this pattern lets everything through, which is how a
    /// filter is cleared.
    pub fn is_open(&self) -> bool {
        if self.text.is_empty() {
            return true;
        }
        if !self.shell {
            return false;
        }
        Masks::parse(&self.text, false).is_open()
    }
}

impl Matcher {
    /// The name alone, for the callers that have nothing else - find
    /// walks paths, not listings.
    pub fn matches(&self, name: &str) -> bool {
        match &self.name {
            NameMatcher::Masks(masks) => masks.matches(name),
            NameMatcher::Regex(re) => re.is_match(name),
        }
    }

    /// The name and everything else the dialog asked about. A
    /// directory is never held to a size or an age: the number in the
    /// listing is not its own, and the two criteria are about files.
    pub fn accepts(&self, entry: &Entry, name: &str) -> bool {
        if !self.matches(name) {
            return false;
        }
        if entry.is_dir() {
            return true;
        }
        if let Some((min, max)) = self.size
            && (min.is_some_and(|min| entry.size < min) || max.is_some_and(|max| entry.size > max))
        {
            return false;
        }
        if let Some(newer) = self.newer
            && entry.mtime.is_none_or(|mtime| mtime < newer)
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(text: &str, case: bool) -> Matcher {
        Pattern {
            text: text.into(),
            shell: true,
            case_sensitive: case,
            ..Pattern::default()
        }
        .compile()
        .unwrap()
    }

    #[test]
    fn shell_patterns_and_case() {
        assert!(shell("*.RS", true).matches("main.RS"));
        assert!(!shell("*.RS", true).matches("main.rs"));
        assert!(shell("*.RS", false).matches("main.rs"));
        assert!(shell("*.rs", false).matches("MAIN.RS"));
    }

    #[test]
    fn a_mask_list_is_any_of_its_masks() {
        let m = shell("*.c,*.h", true);
        assert!(m.matches("main.c"));
        assert!(m.matches("main.h"));
        assert!(!m.matches("main.rs"));
        // the space after a comma belongs to the typing, not the mask
        let spaced = shell("*.c, *.h", true);
        assert!(spaced.matches("main.h"));
        // and one mask is still one mask, which is what mc asks for
        assert!(shell("*.rs", true).matches("main.rs"));
    }

    #[test]
    fn what_follows_the_bar_is_taken_back_out() {
        let m = shell("*.c,*.h|*_test.*", true);
        assert!(m.matches("parser.c"));
        assert!(!m.matches("parser_test.c"));
        // nothing on the left is everything on the left
        let all_but = shell("|*.o,*.d", true);
        assert!(all_but.matches("main.c"));
        assert!(!all_but.matches("main.o"));
        // an exclusion beats an include that also matched
        assert!(!shell("*|*", true).matches("anything"));
    }

    #[test]
    fn several_mask_lists_join_into_one() {
        assert_eq!(join_masks(["*.c,*.h", "*.rs"]), "*.c,*.h,*.rs");
        // the exclusions add up too, and land on one side
        assert_eq!(
            join_masks(["*.c|*_test.*", "*.rs|*.tmp"]),
            "*.c,*.rs|*_test.*,*.tmp"
        );
        // a mask named twice is one mask
        assert_eq!(join_masks(["*.c", "*.c,*.h"]), "*.c,*.h");
        assert_eq!(join_masks(std::iter::empty()), "");
        // "everything except" joined with a narrow list is still
        // everything except: a filter that shows more cannot be made to
        // show less by switching a second one on beside it
        let joined = join_masks(["|*.o", "*.c"]);
        assert_eq!(joined, "|*.o");
        let m = shell(&joined, true);
        assert!(m.matches("main.c") && m.matches("notes.txt"));
        assert!(!m.matches("main.o"));
    }

    #[test]
    fn masks_fold_case_on_both_sides() {
        let m = shell("*.C,*.H|*_TEST.*", false);
        assert!(m.matches("main.c"));
        assert!(!m.matches("main_test.c"));
        assert!(!shell("*.C", true).matches("main.c"));
    }

    #[test]
    fn size_takes_the_forms_it_offers() {
        let with = |size: &str| Pattern {
            size: size.into(),
            ..Pattern::default()
        };
        let file = |bytes: u64| Entry {
            name: "f".into(),
            kind: crate::entry::EntryKind::File,
            size: bytes,
            ..Entry::parent()
        };
        let m = with(">1M").compile().unwrap();
        assert!(m.accepts(&file(2 * 1024 * 1024), "f"));
        assert!(
            !m.accepts(&file(1024 * 1024), "f"),
            "exactly 1M is not over it"
        );
        let m = with(">=1M").compile().unwrap();
        assert!(m.accepts(&file(1024 * 1024), "f"));
        let m = with("<100k").compile().unwrap();
        assert!(m.accepts(&file(1000), "f"));
        assert!(!m.accepts(&file(102_400), "f"));
        let m = with("1k-2k").compile().unwrap();
        assert!(m.accepts(&file(1024), "f") && m.accepts(&file(2048), "f"));
        assert!(!m.accepts(&file(2049), "f"));
        // a directory's size is not its own, so it is never held to one
        assert!(m.accepts(&Entry::parent(), ".."));
        // and nonsense is quoted back rather than matching nothing
        assert!(with("about a gig").compile().is_err());
        assert!(with(">x").compile().is_err());
    }

    #[test]
    fn newer_than_is_an_age() {
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        let week_ago = std::time::SystemTime::now() - Duration::from_secs(7 * 86_400);
        let file = |mtime| Entry {
            name: "f".into(),
            kind: crate::entry::EntryKind::File,
            mtime: Some(mtime),
            ..Entry::parent()
        };
        let m = Pattern {
            newer: "24h".into(),
            ..Pattern::default()
        }
        .compile()
        .unwrap();
        assert!(m.accepts(&file(hour_ago), "f"));
        assert!(!m.accepts(&file(week_ago), "f"));
        // a file whose time nobody knows is not "recent"
        assert!(!m.accepts(
            &Entry {
                name: "f".into(),
                kind: crate::entry::EntryKind::File,
                mtime: None,
                ..Entry::parent()
            },
            "f"
        ));
        assert!(
            Pattern {
                newer: "yesterday".into(),
                ..Pattern::default()
            }
            .compile()
            .is_err()
        );
    }

    #[test]
    fn a_regular_expression_keeps_its_alternation() {
        // the mask list's `|` is the shell switch's alone: in a regex
        // it is the alternation it has always been, and reading it as
        // an exclusion would quietly invert what the user asked for
        let re = Pattern {
            text: "foo|bar".into(),
            shell: false,
            ..Pattern::default()
        }
        .compile()
        .unwrap();
        assert!(re.matches("foo"));
        assert!(re.matches("bar"));
    }

    #[test]
    fn regular_expressions_are_the_other_switch() {
        let re = Pattern {
            text: r"^\d+\.txt$".into(),
            shell: false,
            ..Pattern::default()
        };
        let m = re.compile().unwrap();
        assert!(m.matches("12.txt"));
        assert!(!m.matches("a12.txt"));
        // as a shell pattern the same text is a literal that matches
        // nothing, which is exactly why the switch exists
        let mut asglob = re.clone();
        asglob.shell = true;
        assert!(!asglob.compile().unwrap().matches("12.txt"));
        // and a broken regex is reported rather than swallowed
        let bad = Pattern {
            text: "(".into(),
            shell: false,
            ..Pattern::default()
        };
        assert!(bad.compile().is_err());
    }

    #[test]
    fn an_open_pattern_is_how_a_filter_is_cleared() {
        assert!(Pattern::default().is_open());
        assert!(
            Pattern {
                text: "**".into(),
                ..Pattern::default()
            }
            .is_open()
        );
        assert!(
            !Pattern {
                text: "*.rs".into(),
                ..Pattern::default()
            }
            .is_open()
        );
        // every mask lets everything through
        assert!(
            Pattern {
                text: "*,*".into(),
                ..Pattern::default()
            }
            .is_open()
        );
        // but an exclusion is a filter however open the other side is
        assert!(
            !Pattern {
                text: "|*.o".into(),
                ..Pattern::default()
            }
            .is_open()
        );
        // a regular expression of "*" is not "everything": it is a
        // broken pattern, and pretending otherwise would clear the
        // filter for someone who mistyped one
        assert!(
            !Pattern {
                text: "*".into(),
                shell: false,
                ..Pattern::default()
            }
            .is_open()
        );
    }
}
