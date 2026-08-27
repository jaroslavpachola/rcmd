//! What mc's select, unselect and filter dialogs ask for: a pattern and
//! the three answers that change what it means. One type for all three,
//! because in mc they are the same dialog with a different title.
//!
//! A shell pattern is Far's *mask list* rather than mc's single glob:
//! `*.c,*.h` is either of them and `*.c,*.h|*_test.*` is either of them
//! with a second list taken back out. mc's plain `*.rs` is the one-mask
//! case of it and keeps meaning what it meant. A regular expression is
//! left alone, where `|` is the alternation it has always been.

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
}

impl Default for Pattern {
    fn default() -> Self {
        Pattern {
            text: "*".into(),
            shell: true,
            case_sensitive: true,
            files_only: true,
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
        Ok(())
    }
}

/// A compiled [`Pattern`], ready to run against a name.
pub enum Matcher {
    /// A mask list: a name is in when any of `include` matches it and
    /// none of `exclude` does. Both sides are lowercased already when
    /// the match is case-insensitive, and `fold` says so.
    Masks {
        /// Empty means everything, which is what `|*.o` asks for.
        include: Vec<String>,
        exclude: Vec<String>,
        fold: bool,
    },
    Regex(regex::Regex),
}

/// Split a mask list into its two halves. The first `|` is the one that
/// divides them; commas separate the masks on each side, whitespace
/// around a mask is not part of it, and an empty mask is dropped rather
/// than left to match nothing.
fn parse_masks(text: &str, fold: bool) -> (Vec<String>, Vec<String>) {
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
    (list(included), list(excluded))
}

impl Pattern {
    /// Compile once per listing rather than once per entry. The error is
    /// the user's regular expression, so it is worth showing.
    pub fn compile(&self) -> Result<Matcher, String> {
        if self.shell {
            let fold = !self.case_sensitive;
            let (include, exclude) = parse_masks(&self.text, fold);
            return Ok(Matcher::Masks {
                include,
                exclude,
                fold,
            });
        }
        regex::RegexBuilder::new(&self.text)
            .case_insensitive(!self.case_sensitive)
            .build()
            .map(Matcher::Regex)
            .map_err(|err| err.to_string())
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
        // every mask lets everything through, and nothing is taken back
        // out again
        let (include, exclude) = parse_masks(&self.text, false);
        exclude.is_empty() && include.iter().all(|mask| mask.chars().all(|c| c == '*'))
    }
}

impl Matcher {
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Matcher::Masks {
                include,
                exclude,
                fold,
            } => {
                // the masks were lowercased at compile time; the name
                // has to be lowered here, there being nowhere earlier
                // to do it
                let name = match fold {
                    true => std::borrow::Cow::Owned(name.to_lowercase()),
                    false => std::borrow::Cow::Borrowed(name),
                };
                let matched =
                    include.is_empty() || include.iter().any(|mask| glob_match(mask, &name));
                matched && !exclude.iter().any(|mask| glob_match(mask, &name))
            }
            Matcher::Regex(re) => re.is_match(name),
        }
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
            files_only: true,
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
    fn masks_fold_case_on_both_sides() {
        let m = shell("*.C,*.H|*_TEST.*", false);
        assert!(m.matches("main.c"));
        assert!(!m.matches("main_test.c"));
        assert!(!shell("*.C", true).matches("main.c"));
    }

    #[test]
    fn a_regular_expression_keeps_its_alternation() {
        // the mask list's `|` is the shell switch's alone: in a regex
        // it is the alternation it has always been, and reading it as
        // an exclusion would quietly invert what the user asked for
        let re = Pattern {
            text: "foo|bar".into(),
            shell: false,
            case_sensitive: true,
            files_only: true,
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
            case_sensitive: true,
            files_only: true,
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
