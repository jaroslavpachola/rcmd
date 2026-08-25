//! Find file: walk a tree on a worker thread, stream matches back to the
//! UI as they are found. Matches carry their path relative to the search
//! root in `Entry::name`, ready for a panelized listing.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use crate::entry::{self, Entry};

pub enum FindEvent {
    Match(Box<Entry>),
    Done { matches: u64, scanned: u64 },
}

pub struct FindHandle {
    pub events: Receiver<FindEvent>,
    cancel: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
}

impl FindHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// "Skip this path?" - supplied by the caller (e.g. a gitignore check);
/// a skipped directory is not descended into.
pub type SkipFn = Box<dyn Fn(&Path) -> bool + Send>;

/// What to look for: the name, optionally what is inside, and the
/// answers mc's Find File dialog puts beside them.
#[derive(Clone, Debug)]
pub struct Query {
    /// Matched against the file's name, not its path.
    pub name: crate::pattern::Pattern,
    pub content: Option<Content>,
    /// Leave dotfiles and dot-directories out.
    pub skip_hidden: bool,
    /// Descend through symlinked directories. Off by default: a link
    /// pointing at its own ancestor is a walk that never ends.
    pub follow_links: bool,
}

/// The "containing text" half, and what the text means.
#[derive(Clone, Debug)]
pub struct Content {
    pub text: String,
    /// The text is a regular expression, matched line by line.
    pub regex: bool,
    pub case_sensitive: bool,
    /// Match the word and not the letters inside a longer one.
    pub whole_words: bool,
    /// Look for the text in every codepage rcmd knows, not only UTF-8:
    /// a file written on a KOI8-R machine holds different bytes for the
    /// same word. Free for an ASCII search, where every codepage spells
    /// it the same way and the duplicates collapse.
    pub all_charsets: bool,
}

impl Default for Query {
    fn default() -> Self {
        Query {
            name: crate::pattern::Pattern {
                files_only: false,
                ..crate::pattern::Pattern::default()
            },
            content: None,
            skip_hidden: false,
            follow_links: false,
        }
    }
}

/// A compiled [`Content`]: either bytes to scan for, or a regular
/// expression to run over each line.
enum Seek {
    /// One or more byte strings, any of which counts as a hit; the
    /// haystack is lowercased first when `fold` is set.
    Bytes {
        needles: Vec<Vec<u8>>,
        fold: bool,
    },
    Lines(regex::Regex),
}

impl Content {
    fn compile(&self) -> Result<Seek, String> {
        if self.regex || self.whole_words {
            let body = match self.regex {
                true => self.text.clone(),
                false => regex::escape(&self.text),
            };
            // compile what was typed first, so a mistake in it is
            // reported against the pattern the user wrote rather than
            // against the wrapper below
            let build = |pattern: &str| {
                regex::RegexBuilder::new(pattern)
                    .case_insensitive(!self.case_sensitive)
                    .build()
            };
            build(&body).map_err(|err| err.to_string())?;
            let pattern = match self.whole_words {
                // \b would anchor on the pattern's own edges, and a
                // pattern starting with punctuation has no boundary there
                true => format!(r"(?:^|\W)(?:{body})(?:$|\W)"),
                false => body,
            };
            return build(&pattern)
                .map(Seek::Lines)
                .map_err(|err| err.to_string());
        }
        let fold = !self.case_sensitive;
        // The haystack is folded a byte at a time, which only touches
        // ASCII - so outside it the word has to be looked for as it was
        // typed as well as lowered, or "Привет" would never match the
        // file it is written in.
        let mut forms = vec![self.text.clone()];
        if fold {
            let lowered = self.text.to_lowercase();
            forms = match self.text.is_ascii() {
                // ASCII is what the fold reaches, so the lowered word
                // covers every spelling of it and costs one scan
                true => vec![lowered],
                false if lowered == self.text => forms,
                false => vec![self.text.clone(), lowered],
            };
        }
        let mut needles: Vec<Vec<u8>> = Vec::new();
        let push = |bytes: Vec<u8>, needles: &mut Vec<Vec<u8>>| {
            if !bytes.is_empty() && !needles.contains(&bytes) {
                needles.push(bytes);
            }
        };
        for form in &forms {
            push(form.as_bytes().to_vec(), &mut needles);
            if self.all_charsets {
                for (label, _) in crate::charset::CHARSETS {
                    if let Some(enc) = crate::charset::by_label(label) {
                        push(crate::charset::encode(form, Some(enc)), &mut needles);
                    }
                }
            }
        }
        Ok(Seek::Bytes { needles, fold })
    }
}

/// Walk `root` on a worker thread, streaming matches back as they are
/// found. The error is a bad regular expression, which belongs to the
/// user and is worth showing.
pub fn spawn_find(root: PathBuf, query: Query, skip: Option<SkipFn>) -> Result<FindHandle, String> {
    let matcher = query.name.compile()?;
    let seek = query.content.as_ref().map(Content::compile).transpose()?;
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let thread = thread::spawn(move || {
        let mut matches = 0u64;
        let mut scanned = 0u64;
        let mut ctx = Walk {
            root: root.clone(),
            query,
            matcher,
            seek,
            skip,
            tx,
            cancel: flag,
        };
        ctx.walk(&root.clone(), &mut matches, &mut scanned);
        let _ = ctx.tx.send(FindEvent::Done { matches, scanned });
    });
    Ok(FindHandle {
        events: rx,
        cancel,
        thread: Some(thread),
    })
}

/// Everything the walk carries; it recurses, and eight parameters that
/// never change is a lot to hand down each time.
struct Walk {
    root: PathBuf,
    query: Query,
    matcher: crate::pattern::Matcher,
    seek: Option<Seek>,
    skip: Option<SkipFn>,
    tx: Sender<FindEvent>,
    cancel: Arc<AtomicBool>,
}

impl Walk {
    fn walk(&mut self, dir: &Path, matches: &mut u64, scanned: &mut u64) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return; // unreadable dirs are silently skipped, like find's -readable
        };
        for dent in read.flatten() {
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            let path = dent.path();
            if self.skip.as_deref().is_some_and(|f| f(&path)) {
                continue;
            }
            let name = dent.file_name();
            if self.query.skip_hidden && name.to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(meta) = dent.metadata() else { continue }; // lstat
            *scanned += 1;
            let is_dir = match meta.is_symlink() {
                // a symlink is followed only when asked, and then it is
                // its target that decides whether this is a directory
                true => {
                    self.query.follow_links && std::fs::metadata(&path).is_ok_and(|m| m.is_dir())
                }
                false => meta.is_dir(),
            };
            if self.matcher.matches(&name.to_string_lossy()) {
                let hit = match &self.seek {
                    None => true,
                    Some(seek) => meta.is_file() && file_matches(&path, seek),
                };
                if hit && let Ok(mut entry) = entry::stat(&path) {
                    if let Ok(rel) = path.strip_prefix(&self.root) {
                        entry.name = rel.as_os_str().to_os_string();
                    }
                    *matches += 1;
                    if self.tx.send(FindEvent::Match(Box::new(entry))).is_err() {
                        self.cancel.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
            if is_dir {
                self.walk(&path, matches, scanned);
            }
        }
    }
}

/// Does this file hold what we are looking for?
fn file_matches(path: &Path, seek: &Seek) -> bool {
    match seek {
        Seek::Bytes { needles, fold } => needles
            .iter()
            .any(|needle| file_contains(path, needle, *fold)),
        Seek::Lines(re) => file_lines_match(path, re),
    }
}

/// Line by line, decoded leniently. A regular expression is anchored to
/// a line by definition - `.` does not cross one - so reading a line at
/// a time is both correct and bounded, whatever the file turns out to
/// be. Absurdly long lines (a binary with no newline in it) are cut.
fn file_lines_match(path: &Path, re: &regex::Regex) -> bool {
    use std::io::{BufRead, BufReader};
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {}
        }
        line.truncate(MAX_LINE);
        // the line separator is not part of the line: an anchored
        // pattern ending in $ must be able to reach the end of it
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if re.is_match(&String::from_utf8_lossy(&line)) {
            return true;
        }
    }
}

/// Longest line a content search will look at, so a binary file with no
/// newline in it cannot be read into memory whole.
const MAX_LINE: usize = 64 * 1024;

/// Chunked substring search; never loads the whole file. With `fold`
/// the haystack is lowercased as it goes and `needle` must already be
/// lowercase.
fn file_contains(path: &Path, needle: &[u8], fold: bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let overlap = needle.len() - 1;
    let mut buf = vec![0u8; 64 * 1024 + overlap];
    let mut carry = 0usize;
    loop {
        let n = match file.read(&mut buf[carry..]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        let hay = &buf[..carry + n];
        let found = match fold {
            true => {
                let lower: Vec<u8> = hay.iter().map(|b| b.to_ascii_lowercase()).collect();
                lower.windows(needle.len()).any(|w| w == needle)
            }
            false => hay.windows(needle.len()).any(|w| w == needle),
        };
        if found {
            return true;
        }
        carry = overlap.min(hay.len());
        let start = hay.len() - carry;
        buf.copy_within(start..start + carry, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn collect(handle: FindHandle) -> (Vec<String>, u64) {
        let mut names = Vec::new();
        loop {
            match handle.events.recv().expect("find died without Done") {
                FindEvent::Match(entry) => names.push(entry.name.to_string_lossy().into_owned()),
                FindEvent::Done { matches, .. } => {
                    names.sort();
                    return (names, matches);
                }
            }
        }
    }

    fn named(pattern: &str) -> Query {
        Query {
            name: crate::pattern::Pattern {
                text: pattern.into(),
                files_only: false,
                ..crate::pattern::Pattern::default()
            },
            ..Query::default()
        }
    }

    fn containing(text: &str) -> Query {
        Query {
            content: Some(Content {
                text: text.into(),
                regex: false,
                case_sensitive: false,
                whole_words: false,
                all_charsets: false,
            }),
            ..named("*")
        }
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() { magic() }").unwrap();
        fs::write(dir.path().join("src/deep/util.rs"), "pub fn util() {}").unwrap();
        fs::write(dir.path().join("notes.txt"), "the MAGIC word").unwrap();
        dir
    }

    #[test]
    fn finds_by_name_glob_with_relative_paths() {
        let t = tree();
        let (names, matches) =
            collect(spawn_find(t.path().to_path_buf(), named("*.rs"), None).unwrap());
        assert_eq!(names, ["src/deep/util.rs", "src/main.rs"]);
        assert_eq!(matches, 2);
    }

    #[test]
    fn content_filter_is_case_insensitive_unless_asked() {
        let t = tree();
        let (names, _) =
            collect(spawn_find(t.path().to_path_buf(), containing("Magic"), None).unwrap());
        assert_eq!(names, ["notes.txt", "src/main.rs"]);

        let mut cased = containing("Magic");
        cased.content.as_mut().unwrap().case_sensitive = true;
        let (names, _) = collect(spawn_find(t.path().to_path_buf(), cased, None).unwrap());
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn whole_words_and_regular_expressions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "the magic word\n").unwrap();
        fs::write(dir.path().join("b.txt"), "magically\n").unwrap();
        let mut words = containing("magic");
        words.content.as_mut().unwrap().whole_words = true;
        let (names, _) = collect(spawn_find(dir.path().to_path_buf(), words, None).unwrap());
        assert_eq!(names, ["a.txt"], "magically is not the word magic");

        let mut re = containing(r"^magic\w+$");
        re.content.as_mut().unwrap().regex = true;
        let (names, _) = collect(spawn_find(dir.path().to_path_buf(), re, None).unwrap());
        assert_eq!(names, ["b.txt"]);

        // and a broken one never starts a walk
        let mut bad = containing("(");
        bad.content.as_mut().unwrap().regex = true;
        assert!(spawn_find(dir.path().to_path_buf(), bad, None).is_err());
    }

    #[test]
    fn all_charsets_finds_the_word_as_another_machine_spelled_it() {
        let dir = tempfile::tempdir().unwrap();
        let koi = crate::charset::by_label("KOI8-R (Russian)").unwrap();
        fs::write(
            dir.path().join("koi.txt"),
            crate::charset::encode("Привет мир", Some(koi)),
        )
        .unwrap();
        // as UTF-8 those bytes are not the word at all
        let (names, _) =
            collect(spawn_find(dir.path().to_path_buf(), containing("Привет"), None).unwrap());
        assert!(names.is_empty());
        let mut every = containing("Привет");
        every.content.as_mut().unwrap().all_charsets = true;
        let (names, _) = collect(spawn_find(dir.path().to_path_buf(), every, None).unwrap());
        assert_eq!(names, ["koi.txt"]);
    }

    #[test]
    fn hidden_files_are_skipped_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/config"), "x").unwrap();
        fs::write(dir.path().join("plain.txt"), "x").unwrap();
        let mut q = named("*");
        q.skip_hidden = true;
        let (names, _) = collect(spawn_find(dir.path().to_path_buf(), q, None).unwrap());
        assert_eq!(names, ["plain.txt"]);
    }

    #[test]
    fn symlinked_directories_are_walked_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("real")).unwrap();
        fs::write(dir.path().join("real/inside.txt"), "x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        let (names, _) =
            collect(spawn_find(dir.path().to_path_buf(), named("inside.txt"), None).unwrap());
        assert_eq!(names, ["real/inside.txt"]);
        let mut follow = named("inside.txt");
        follow.follow_links = true;
        let (names, _) = collect(spawn_find(dir.path().to_path_buf(), follow, None).unwrap());
        assert_eq!(names, ["link/inside.txt", "real/inside.txt"]);
    }

    #[test]
    fn skip_prunes_files_and_whole_trees() {
        let t = tree();
        let (names, _) = collect(
            spawn_find(
                t.path().to_path_buf(),
                named("*"),
                Some(Box::new(|p: &Path| {
                    p.file_name()
                        .is_some_and(|n| n == "deep" || n == "notes.txt")
                })),
            )
            .unwrap(),
        );
        assert_eq!(names, ["src", "src/main.rs"]);
    }

    #[test]
    fn content_straddling_chunk_boundary_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let mut data = vec![b'x'; 64 * 1024 - 3];
        data.extend_from_slice(b"needle");
        fs::write(dir.path().join("big.bin"), &data).unwrap();
        let (names, _) =
            collect(spawn_find(dir.path().to_path_buf(), containing("needle"), None).unwrap());
        assert_eq!(names, ["big.bin"]);
    }
}
