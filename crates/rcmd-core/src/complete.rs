//! Completion of the word under the cursor, as a shell does it: a path
//! anywhere, a command from `$PATH` as the first word of a command
//! line, `$NAME` from the environment, `~user` from the password file.
//! The TUI hands over the line up to the cursor (still shell-escaped);
//! we answer with the completed word and, when several match, every
//! candidate with the word it would become.

use std::path::{Path, PathBuf};

/// The outcome of a completion attempt on one word.
pub struct Completed {
    /// Replacement for the whole word, shell-escaped like the input.
    pub word: String,
    /// All matching names (sorted); length > 1 means "ambiguous, the
    /// word only advanced to the common prefix".
    pub matches: Vec<String>,
    /// For each match, the whole word it completes to - what picking it
    /// from a list puts on the line.
    pub options: Vec<String>,
}

/// Complete the last word of `head`, the line up to the cursor. `cwd`
/// resolves relative paths; `commands` says whether the line is a
/// command, whose first word is looked up on `$PATH` rather than in
/// the directory.
pub fn complete(cwd: &Path, head: &str, commands: bool) -> Option<Completed> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    complete_in(cwd, head, commands, &path)
}

/// ...with the command search path given rather than read.
fn complete_in(
    cwd: &Path,
    head: &str,
    commands: bool,
    search: &std::ffi::OsStr,
) -> Option<Completed> {
    let start = word_start(head);
    let word = &head[start..];
    if let Some(name) = word.strip_prefix('$') {
        let names = std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .filter(|k| k.starts_with(name));
        return finish(names, |n| format!("${n}"), "");
    }
    if let Some(user) = word.strip_prefix('~')
        && !user.contains('/')
    {
        return finish(
            users().into_iter().filter(|u| u.starts_with(user)),
            |u| format!("~{u}"),
            "/",
        );
    }
    if commands && head[..start].trim().is_empty() && !word.is_empty() && !word.contains('/') {
        let prefix = unescape(word);
        return finish(
            path_commands(search)
                .into_iter()
                .filter(|c| c.starts_with(&prefix)),
            escape,
            " ",
        );
    }
    complete_word(cwd, word)
}

/// The candidates, sorted and deduplicated, as a completion: the word
/// advanced to what they share, and `done` added once only one is left.
fn finish(
    names: impl Iterator<Item = String>,
    spell: impl Fn(&str) -> String,
    done: &str,
) -> Option<Completed> {
    let mut matches: Vec<String> = names.collect();
    matches.sort();
    matches.dedup();
    if matches.is_empty() {
        return None;
    }
    let stem = common_prefix(matches.iter().map(String::as_str));
    let mut word = spell(&stem);
    if matches.len() == 1 {
        word.push_str(done);
    }
    let options = matches
        .iter()
        .map(|m| format!("{}{done}", spell(m)))
        .collect();
    Some(Completed {
        word,
        matches,
        options,
    })
}

/// Every executable on `$PATH`, by name.
fn path_commands(search: &std::ffi::OsStr) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(search)
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            // metadata follows the link: /usr/bin is mostly symlinks
            e.path()
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// The login names the password file knows.
fn users() -> Vec<String> {
    std::fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split(':').next())
        .filter(|name| !name.is_empty() && !name.starts_with('#'))
        .map(str::to_string)
        .collect()
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
    let end = |is_dir: bool| if is_dir { '/' } else { ' ' };
    if matches.len() == 1 {
        word.push(end(matches[0].1));
    }
    let options = matches
        .iter()
        .map(|(name, is_dir)| format!("{dir_text}{}{}", escape(name), end(*is_dir)))
        .collect();
    Some(Completed {
        word,
        matches: matches.into_iter().map(|(n, _)| n).collect(),
        options,
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
    fn every_candidate_knows_the_word_it_makes() {
        let dir = playground();
        let c = complete_word(dir.path(), "s").unwrap();
        assert_eq!(c.options, ["sample.rs ", "sample.txt ", "subdir/"]);
    }

    #[test]
    fn the_first_word_of_a_command_is_a_command() {
        let dir = playground();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for name in ["rcmd-test-tool", "rcmd-test-other"] {
            let path = bin.join(name);
            fs::write(&path, "#!/bin/sh\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // not executable: not a command
        fs::write(bin.join("rcmd-test-data"), "").unwrap();
        let path = bin.as_os_str();
        let c = complete_in(dir.path(), "rcmd-test-t", true, path).unwrap();
        assert_eq!(c.word, "rcmd-test-tool ");
        let c = complete_in(dir.path(), "rcmd-test-", true, path).unwrap();
        assert_eq!(c.matches, ["rcmd-test-other", "rcmd-test-tool"]);
        // the second word is a path again, and so is anything in a field
        let c = complete_in(dir.path(), "rcmd-test-tool sam", true, path).unwrap();
        assert_eq!(c.word, "sample.");
        assert!(complete_in(dir.path(), "rcmd-test-t", false, path).is_none());
    }

    #[test]
    fn variables_and_home_directories() {
        let dir = playground();
        // SAFETY: a name no other test reads
        unsafe { std::env::set_var("RCMD_COMPLETE_TEST_VAR", "1") };
        let c = complete(dir.path(), "echo $RCMD_COMPLETE_TEST_V", true).unwrap();
        assert_eq!(c.word, "$RCMD_COMPLETE_TEST_VAR");
        let c = complete(dir.path(), "cd ~roo", true).unwrap();
        assert_eq!(c.word, "~root/");
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
