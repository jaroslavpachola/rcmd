//! mc's user-menu conditions: the little language in front of a `menu`
//! entry that decides whether the entry is offered at all.
//!
//! ```text
//! + f *.tar.gz | f *.tgz     # only for tarballs
//! + t d & !t t               # a directory, and nothing marked
//! ```
//!
//! A term is an optional `!`, a letter saying what to look at, and a
//! pattern: `f`/`F` the cursor file here or in the other panel, `d`/`D`
//! the directory, `t`/`T` the file's type. Terms are joined by `|` and
//! `&`, evaluated left to right the way mc evaluates them - there is no
//! precedence to get wrong, and mc's own menu files are written for
//! that.
//!
//! Patterns are globs. mc's `shell_patterns=0` files say theirs in
//! regex instead; those are converted on import rather than understood
//! here, so what a condition means never depends on a line at the top
//! of some other file.

use crate::glob::glob_match;

/// What the panels look like right now, as the conditions see them.
#[derive(Default)]
pub struct Context<'a> {
    pub file: &'a str,
    pub dir: &'a str,
    pub kind: FileKind,
    pub tagged: bool,
    pub other_file: &'a str,
    pub other_dir: &'a str,
    pub other_kind: FileKind,
    pub other_tagged: bool,
}

/// What `t` can be asked about. rcmd's listings know files, directories
/// and links; a socket or a device answers "no" rather than pretending.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    #[default]
    None,
    File,
    Dir,
    Link,
    LinkDir,
    Executable,
}

impl FileKind {
    fn is(self, letter: char) -> bool {
        match letter {
            // "not a directory" - mc's n
            'n' => !matches!(self, FileKind::Dir | FileKind::LinkDir | FileKind::None),
            'r' => matches!(self, FileKind::File | FileKind::Executable),
            'd' => matches!(self, FileKind::Dir | FileKind::LinkDir),
            'l' => matches!(self, FileKind::Link | FileKind::LinkDir),
            'x' => matches!(self, FileKind::Executable),
            // c b f s: rcmd's listings do not carry them
            _ => false,
        }
    }
}

/// Whether `condition` holds. An empty condition always does - that is
/// an entry with no `+` line in front of it.
pub fn matches(condition: &str, cx: &Context) -> bool {
    let mut result: Option<bool> = None;
    for (joiner, term) in terms(condition) {
        let value = term_matches(term, cx);
        result = Some(match (result, joiner) {
            (None, _) => value,
            (Some(so_far), true) => so_far && value,
            (Some(so_far), false) => so_far || value,
        });
    }
    result.unwrap_or(true)
}

/// Split into terms, each with the joiner that came before it (true =
/// `&`, false = `|`; the first term's joiner is not used).
fn terms(condition: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let mut and = true;
    let mut start = 0;
    for (at, c) in condition.char_indices() {
        if c == '|' || c == '&' {
            let term = condition[start..at].trim();
            if !term.is_empty() {
                out.push((and, term));
            }
            and = c == '&';
            start = at + c.len_utf8();
        }
    }
    let term = condition[start..].trim();
    if !term.is_empty() {
        out.push((and, term));
    }
    out
}

fn term_matches(term: &str, cx: &Context) -> bool {
    let (negate, term) = match term.strip_prefix('!') {
        Some(rest) => (true, rest.trim()),
        None => (false, term),
    };
    let mut chars = term.chars();
    let Some(letter) = chars.next() else {
        return false;
    };
    let pattern = chars.as_str().trim();
    let value = match letter {
        'f' => glob_match(pattern, cx.file),
        'F' => glob_match(pattern, cx.other_file),
        'd' => glob_match(pattern, cx.dir),
        'D' => glob_match(pattern, cx.other_dir),
        't' => types_match(pattern, cx.kind, cx.tagged),
        'T' => types_match(pattern, cx.other_kind, cx.other_tagged),
        // mc's `x` is "this program exists and is executable"; it takes
        // a path rather than a pattern
        'x' => std::path::Path::new(pattern).is_file(),
        _ => false,
    };
    value != negate
}

/// `t rd` holds when the file is any one of the letters - mc's types
/// are a set, not a sequence. `t` itself means "something is marked".
fn types_match(letters: &str, kind: FileKind, tagged: bool) -> bool {
    letters.chars().any(|letter| match letter {
        't' => tagged,
        other => kind.is(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx() -> Context<'static> {
        Context {
            file: "archive.tar.gz",
            dir: "/home/you/downloads",
            kind: FileKind::File,
            tagged: false,
            other_file: "notes",
            other_dir: "/tmp",
            other_kind: FileKind::Dir,
            other_tagged: true,
        }
    }

    #[test]
    fn no_condition_is_always_true() {
        assert!(matches("", &cx()));
        assert!(matches("   ", &cx()));
    }

    #[test]
    fn a_pattern_looks_at_the_right_panel() {
        assert!(matches("f *.tar.gz", &cx()));
        assert!(!matches("f *.zip", &cx()));
        assert!(matches("F notes", &cx()));
        assert!(matches("d */downloads", &cx()));
        assert!(matches("D /tmp", &cx()));
    }

    #[test]
    fn types_and_marks() {
        assert!(matches("t r", &cx())); // a regular file
        assert!(matches("t n", &cx())); // ...which is not a directory
        assert!(!matches("t d", &cx()));
        assert!(!matches("t t", &cx())); // nothing marked here
        assert!(matches("T t", &cx())); // ...but something is over there
        assert!(matches("T d", &cx()));
        // a set of letters: any one of them will do
        assert!(matches("t dr", &cx()));
    }

    #[test]
    fn negation_and_joiners_read_left_to_right() {
        assert!(matches("!f *.zip", &cx()));
        assert!(matches("f *.tar.gz | f *.tgz", &cx()));
        assert!(matches("f *.tgz | f *.tar.gz", &cx()));
        assert!(matches("f *.tar.gz & t r", &cx()));
        assert!(!matches("f *.tar.gz & t d", &cx()));
        // left to right, no precedence: (false | true) & true
        assert!(matches("f *.zip | f *.tar.gz & t r", &cx()));
        // ...and the same terms the other way round: (false & true) | true
        assert!(matches("f *.zip & t r | f *.tar.gz", &cx()));
    }

    #[test]
    fn nonsense_is_false_rather_than_a_panic() {
        assert!(!matches("z whatever", &cx()));
        assert!(!matches("!", &cx()));
    }
}
