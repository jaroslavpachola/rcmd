//! Command-line path completion (R3): files and directories only, no
//! command completion. The TUI hands over the word under the cursor
//! (still shell-escaped); we answer with the completed word and the
//! candidate names when several match.

use std::path::{Path, PathBuf};

/// The outcome of a completion attempt on one word.
pub struct Completed {
    /// Replacement for the whole word, shell-escaped like the input.
    pub word: String,
    /// All matching names (sorted); length > 1 means "ambiguous, the
    /// word only advanced to the common prefix".
    pub matches: Vec<String>,
}

/// Byte offset where the word under the cursor starts: after the last
/// space that is not backslash-escaped.
pub fn word_start(line: &str) -> usize {
    let mut start = 0;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            ' ' => start = i + ch.len_utf8(),
            _ => {}
        }
    }
    start
}

/// Complete `word` (shell-escaped, possibly `~`-prefixed) against the
/// filesystem, relative paths resolved from `cwd`. `None` = no match.
pub fn complete_word(cwd: &Path, word: &str) -> Option<Completed> {
    let raw = unescape(word);
    // split into the directory part (kept verbatim) and the name prefix
    let (dir_text, prefix) = match raw.rfind('/') {
        Some(i) => (&raw[..=i], &raw[i + 1..]),
        None => ("", raw.as_str()),
    };
    let dir = resolve(cwd, dir_text);
    let mut matches: Vec<(String, bool)> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir())
                || (e.file_type().is_ok_and(|t| t.is_symlink()) && e.path().is_dir());
            name.starts_with(prefix).then_some((name, is_dir))
        })
        .collect();
    if matches.is_empty() {
        return None;
    }
    matches.sort();
    let stem = common_prefix(matches.iter().map(|(n, _)| n.as_str()));
    let mut word = format!("{dir_text}{}", escape(&stem));
    if matches.len() == 1 {
        word.push(if matches[0].1 { '/' } else { ' ' });
    }
    Some(Completed {
        word,
        matches: matches.into_iter().map(|(n, _)| n).collect(),
    })
}

fn resolve(cwd: &Path, dir_text: &str) -> PathBuf {
    if let Some(rest) = dir_text.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    if dir_text.is_empty() {
        cwd.to_path_buf()
    } else if Path::new(dir_text).is_absolute() {
        PathBuf::from(dir_text)
    } else {
        cwd.join(dir_text)
    }
}

fn common_prefix<'a>(mut names: impl Iterator<Item = &'a str>) -> String {
    let mut prefix = names.next().unwrap_or("").to_string();
    for name in names {
        let shared = prefix
            .char_indices()
            .find(|&(i, c)| name.get(i..).and_then(|s| s.chars().next()) != Some(c))
            .map_or(prefix.len(), |(i, _)| i);
        prefix.truncate(shared);
    }
    prefix
}

/// Characters the shell would interpret; completion escapes them so the
/// inserted name survives as one argument.
const SPECIAL: &str = " \t!\"#$&'()*;<>?[\\]^`{|}~";

fn escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if SPECIAL.contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn unescape(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut escaped = false;
    for ch in word.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn playground() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("subdir/inner.txt"), "").unwrap();
        fs::write(dir.path().join("sample.txt"), "").unwrap();
        fs::write(dir.path().join("sample.rs"), "").unwrap();
        fs::write(dir.path().join("with space.txt"), "").unwrap();
        dir
    }

    #[test]
    fn word_start_honours_escapes() {
        assert_eq!(word_start(""), 0);
        assert_eq!(word_start("cat file"), 4);
        assert_eq!(word_start("cat with\\ space"), 4);
        assert_eq!(word_start("cat a b"), 6);
    }

    #[test]
    fn unique_match_completes_fully() {
        let dir = playground();
        let c = complete_word(dir.path(), "subd").unwrap();
        assert_eq!(c.word, "subdir/");
        assert_eq!(c.matches, ["subdir"]);
        let c = complete_word(dir.path(), "subdir/in").unwrap();
        assert_eq!(c.word, "subdir/inner.txt ");
    }

    #[test]
    fn ambiguous_match_stops_at_common_prefix() {
        let dir = playground();
        let c = complete_word(dir.path(), "sam").unwrap();
        assert_eq!(c.word, "sample.");
        assert_eq!(c.matches, ["sample.rs", "sample.txt"]);
    }

    #[test]
    fn spaces_round_trip_escaped() {
        let dir = playground();
        let c = complete_word(dir.path(), "wit").unwrap();
        assert_eq!(c.word, "with\\ space.txt ");
        let c = complete_word(dir.path(), "with\\ sp").unwrap();
        assert_eq!(c.word, "with\\ space.txt ");
    }

    #[test]
    fn absolute_and_missing() {
        let dir = playground();
        let abs = format!("{}/sam", dir.path().display());
        let c = complete_word(Path::new("/nowhere"), &abs).unwrap();
        assert!(c.word.ends_with("sample."));
        assert!(complete_word(dir.path(), "nosuch").is_none());
    }
}
