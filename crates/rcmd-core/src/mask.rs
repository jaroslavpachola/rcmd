//! MC's copy/rename masks: `*.tar.gz` in, `*.tgz` out.
//!
//! A source mask is a shell pattern whose wildcards double as capture
//! groups - `*` and `?`, numbered left to right. The target mask spends
//! them: `*` is the first group, `\1`..`\9` any of them, `\0` the whole
//! name, and `\u \l \U \L \E` change the case of what follows. Anything
//! else is literal, and `\` quotes itself and `*`.
//!
//! Greedy, like the regex mc compiles the pattern into: `*.*` against
//! `a.b.c` captures `a.b` and `c`, not `a` and `b.c`.
//!
//! The regex form behind mc's "use shell patterns" switch is not here.
//! rcmd already does regex renaming with capture groups in the bulk
//! rename editor (F9, File, Bulk rename), which is a better place for
//! it than a one-line field in a dialog.

/// A compiled source mask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mask {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Literal(String),
    /// `*` - any run of characters, greedy.
    Star,
    /// `?` - exactly one character.
    Any,
}

impl Mask {
    /// Compile a shell pattern. `\` quotes the next character, so `\*`
    /// is a literal asterisk.
    pub fn new(pattern: &str) -> Mask {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut chars = pattern.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        literal.push(next);
                    }
                }
                '*' | '?' => {
                    if !literal.is_empty() {
                        parts.push(Part::Literal(std::mem::take(&mut literal)));
                    }
                    parts.push(if c == '*' { Part::Star } else { Part::Any });
                }
                other => literal.push(other),
            }
        }
        if !literal.is_empty() {
            parts.push(Part::Literal(literal));
        }
        Mask { parts }
    }

    /// True when the mask is just `*` - it matches everything and
    /// captures the whole name, so nothing needs filtering.
    pub fn is_catch_all(&self) -> bool {
        self.parts == [Part::Star]
    }

    /// What the wildcards captured, in order, or `None` if the name does
    /// not match at all.
    pub fn captures(&self, name: &str) -> Option<Vec<String>> {
        let chars: Vec<char> = name.chars().collect();
        let mut caps = Vec::new();
        walk(&self.parts, &chars, &mut caps).then_some(caps)
    }

    pub fn matches(&self, name: &str) -> bool {
        self.captures(name).is_some()
    }
}

/// Greedy backtracking match, collecting what each wildcard consumed.
fn walk(parts: &[Part], name: &[char], caps: &mut Vec<String>) -> bool {
    let Some((part, rest)) = parts.split_first() else {
        return name.is_empty();
    };
    match part {
        Part::Literal(text) => {
            let literal: Vec<char> = text.chars().collect();
            name.starts_with(&literal[..]) && walk(rest, &name[literal.len()..], caps)
        }
        Part::Any => {
            if name.is_empty() {
                return false;
            }
            caps.push(name[0].to_string());
            if walk(rest, &name[1..], caps) {
                true
            } else {
                caps.pop();
                false
            }
        }
        // longest first, so `*.*` on "a.b.c" captures "a.b" and "c" -
        // the same split the regex mc builds would produce
        Part::Star => {
            for take in (0..=name.len()).rev() {
                caps.push(name[..take].iter().collect());
                if walk(rest, &name[take..], caps) {
                    return true;
                }
                caps.pop();
            }
            false
        }
    }
}

/// Case folding asked for by `\u \l \U \L \E`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    None,
    Upper,
    Lower,
}

/// Spend the captures on a target mask. `whole` is the untouched source
/// name, which `\0` asks for.
pub fn expand(target: &str, caps: &[String], whole: &str) -> String {
    let mut out = String::new();
    let mut run = Case::None; // \U or \L, until \E
    let mut next = Case::None; // \u or \l, for one character
    let mut chars = target.chars().peekable();
    let push = |out: &mut String, text: &str, run: Case, next: &mut Case| {
        for c in text.chars() {
            let case = if *next != Case::None {
                std::mem::replace(next, Case::None)
            } else {
                run
            };
            match case {
                Case::Upper => out.extend(c.to_uppercase()),
                Case::Lower => out.extend(c.to_lowercase()),
                Case::None => out.push(c),
            }
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '*' => push(
                &mut out,
                caps.first().map_or("", String::as_str),
                run,
                &mut next,
            ),
            '\\' => match chars.next() {
                Some('0') => push(&mut out, whole, run, &mut next),
                Some(d @ '1'..='9') => {
                    let index = d as usize - '1' as usize;
                    let text = caps.get(index).map_or("", String::as_str);
                    push(&mut out, text, run, &mut next);
                }
                Some('u') => next = Case::Upper,
                Some('l') => next = Case::Lower,
                Some('U') => run = Case::Upper,
                Some('L') => run = Case::Lower,
                Some('E') => run = Case::None,
                Some(other) => push(&mut out, &other.to_string(), run, &mut next),
                None => {}
            },
            other => push(&mut out, &other.to_string(), run, &mut next),
        }
    }
    out
}

/// Whether a destination's last component is a target mask rather than
/// a plain name - an unquoted `*` or a `\N` back-reference.
pub fn is_target_mask(text: &str) -> bool {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('0'..='9') => return true,
                _ => continue,
            },
            '*' => return true,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcs_own_examples() {
        // *.tar.gz -> *.tgz turns foo.tar.gz into foo.tgz
        let mask = Mask::new("*.tar.gz");
        let caps = mask.captures("foo.tar.gz").unwrap();
        assert_eq!(caps, vec!["foo"]);
        assert_eq!(expand("*.tgz", &caps, "foo.tar.gz"), "foo.tgz");

        // *.* with \2.\1 swaps basename and extension
        let mask = Mask::new("*.*");
        let caps = mask.captures("file.c").unwrap();
        assert_eq!(caps, vec!["file", "c"]);
        assert_eq!(expand("\\2.\\1", &caps, "file.c"), "c.file");
    }

    #[test]
    fn stars_are_greedy_like_the_regex_mc_builds() {
        let caps = Mask::new("*.*").captures("a.b.c").unwrap();
        assert_eq!(caps, vec!["a.b", "c"]);
    }

    #[test]
    fn a_name_that_does_not_match_captures_nothing() {
        assert!(Mask::new("*.tar.gz").captures("notes.txt").is_none());
        assert!(!Mask::new("*.tar.gz").matches("notes.txt"));
    }

    #[test]
    fn question_marks_capture_one_character_each() {
        let caps = Mask::new("??-*.log").captures("ab-server.log").unwrap();
        assert_eq!(caps, vec!["a", "b", "server"]);
        assert_eq!(
            expand("\\3-\\1\\2.txt", &caps, "ab-server.log"),
            "server-ab.txt"
        );
    }

    #[test]
    fn backslash_zero_is_the_whole_name() {
        let caps = Mask::new("*").captures("report.txt").unwrap();
        assert_eq!(
            expand("copy-of-\\0", &caps, "report.txt"),
            "copy-of-report.txt"
        );
    }

    #[test]
    fn case_conversions_follow_mcs_rules() {
        let caps = Mask::new("*").captures("hello world").unwrap();
        // \L\u* : initial capital, the rest lowered
        assert_eq!(expand("\\L\\u*", &caps, "hello world"), "Hello world");
        assert_eq!(expand("\\U*", &caps, "hello world"), "HELLO WORLD");
        // \E ends a run
        assert_eq!(
            expand("\\U*\\E.bak", &caps, "hello world"),
            "HELLO WORLD.bak"
        );
    }

    #[test]
    fn backslash_quotes_a_wildcard() {
        assert!(Mask::new("\\*.txt").matches("*.txt"));
        assert!(!Mask::new("\\*.txt").matches("notes.txt"));
        let caps = Mask::new("*").captures("x").unwrap();
        assert_eq!(expand("a\\*b", &caps, "x"), "a*b");
    }

    #[test]
    fn a_catch_all_mask_is_recognised() {
        assert!(Mask::new("*").is_catch_all());
        assert!(!Mask::new("*.txt").is_catch_all());
    }

    #[test]
    fn a_destination_only_carries_a_mask_when_it_has_wildcards() {
        assert!(is_target_mask("*.tgz"));
        assert!(is_target_mask("\\2.\\1"));
        assert!(!is_target_mask("plain.txt"));
        assert!(!is_target_mask("literal\\*star"));
    }
}
