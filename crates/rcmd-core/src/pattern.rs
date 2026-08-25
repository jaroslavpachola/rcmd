//! What mc's select, unselect and filter dialogs ask for: a pattern and
//! the three answers that change what it means. One type for all three,
//! because in mc they are the same dialog with a different title.

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
    /// The glob, lowercased already when the match is case-insensitive.
    Glob(String, bool),
    Regex(regex::Regex),
}

impl Pattern {
    /// Compile once per listing rather than once per entry. The error is
    /// the user's regular expression, so it is worth showing.
    pub fn compile(&self) -> Result<Matcher, String> {
        if self.shell {
            return Ok(match self.case_sensitive {
                true => Matcher::Glob(self.text.clone(), true),
                false => Matcher::Glob(self.text.to_lowercase(), false),
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
        self.text.is_empty() || (self.shell && self.text.chars().all(|c| c == '*'))
    }
}

impl Matcher {
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Matcher::Glob(pattern, true) => glob_match(pattern, name),
            // the pattern was lowercased at compile time; the name has
            // to be lowered here, there being nowhere earlier to do it
            Matcher::Glob(pattern, false) => glob_match(pattern, &name.to_lowercase()),
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
