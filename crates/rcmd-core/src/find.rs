//! Find file: walk a tree on a worker thread, stream matches back to the
//! UI as they are found. Matches carry their path relative to the search
//! root in `Entry::name`, ready for a panelized listing - and, when the
//! content was searched, the line each hit is on and what it says.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

use crate::entry::{self, Entry};

pub enum FindEvent {
    /// One result: a file whose name matched, or one hit inside it.
    /// A file with three hits is three of these.
    Match(Box<Found>),
    Done {
        matches: u64,
        scanned: u64,
    },
}

/// A result of a find.
#[derive(Clone, Debug)]
pub struct Found {
    /// Its name is the path relative to the search root - through the
    /// archive, for a member of one: `src.tar.gz/lib/x.c`.
    pub entry: Entry,
    /// Where in the file the content matched; `None` for a find by name.
    pub hit: Option<Hit>,
    /// For a member of an archive: the archive, relative to the root,
    /// and where the member is inside it.
    pub inside: Option<(PathBuf, PathBuf)>,
}

/// One line of a file the content was found on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// 1-based, as an editor counts.
    pub line: u64,
    /// The line, decoded leniently and cut to a preview: from where it
    /// starts, or from a little before the match when that is further
    /// along than a window is wide.
    pub text: String,
}

/// Hits reported for one file when every hit is wanted - a file that
/// matches on every line is a file, not a million results.
const MAX_HITS: usize = 1000;
/// How much of a hit's line the preview keeps.
const PREVIEW: usize = 200;

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
/// a skipped directory is not descended into. Sync because the walk
/// calls it from several threads at once.
pub type SkipFn = Box<dyn Fn(&Path) -> bool + Send + Sync>;

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
    /// mc's "First hit": one result per file, at the first line that
    /// matches. Off, every matching line is a result of its own.
    pub first_hit: bool,
    /// How deep to go: 1 is the start directory alone, which is mc's
    /// "Find recursively" switched off. `None` is all the way down.
    pub max_depth: Option<usize>,
    /// Directories never descended into: a bare name anywhere in the
    /// tree (`node_modules`), or a path from the start (`build/out`).
    pub ignore_dirs: Vec<String>,
    /// Look inside the archives the walk meets, the ones rcmd reads
    /// itself: the formats an external tool opens cost a process for
    /// every member, which is not a search.
    pub archives: bool,
    /// Report files, never directories - they are still walked into.
    pub files_only: bool,
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
            first_hit: true,
            max_depth: None,
            ignore_dirs: Vec::new(),
            archives: false,
            files_only: false,
        }
    }
}

/// Split mc's ignore-directories answer: `:`, `;`, `,` or blanks
/// between the entries, trailing slashes dropped.
pub fn parse_ignore_dirs(text: &str) -> Vec<String> {
    text.split(|c: char| matches!(c, ':' | ';' | ',') || c.is_whitespace())
        .map(|d| d.trim_end_matches('/'))
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether the directory at `rel` (relative to the search root) is one
/// of those the query ignores.
fn ignored(rel: &Path, ignore: &[String]) -> bool {
    ignore.iter().any(|dir| {
        if dir.contains('/') {
            rel == Path::new(dir.trim_start_matches("./"))
        } else {
            rel.file_name().is_some_and(|name| name == dir.as_str())
        }
    })
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
        // a size or an age is about files, and answering it takes a stat
        let criteria = matcher.has_criteria();
        let mut walk = jwalk::WalkDir::new(&root)
            // depth 0 is the search root itself, never a result
            .min_depth(1)
            .skip_hidden(query.skip_hidden)
            .follow_links(query.follow_links)
            // a pool of our own: the shared one may be busy, and jwalk
            // answers a busy pool by walking nothing at all
            .parallelism(jwalk::Parallelism::RayonNewPool(0));
        if let Some(depth) = query.max_depth {
            walk = walk.max_depth(depth.max(1));
        }
        let walk = match (skip, query.ignore_dirs.is_empty()) {
            (None, true) => walk,
            // dropping an entry here also prunes it: jwalk never
            // descends into what it was not handed back
            (skip, _) => {
                let (base, ignore) = (root.clone(), query.ignore_dirs.clone());
                walk.process_read_dir(move |_, _, _, children| {
                    children.retain(|child| match child {
                        Ok(entry) => {
                            let path = entry.path();
                            let dropped = entry.file_type.is_dir()
                                && path
                                    .strip_prefix(&base)
                                    .is_ok_and(|rel| ignored(rel, &ignore));
                            !dropped && !skip.as_ref().is_some_and(|skip| skip(&path))
                        }
                        Err(_) => true,
                    });
                })
            }
        };
        let mut send = |found: Found| -> bool {
            matches += 1;
            tx.send(FindEvent::Match(Box::new(found))).is_ok()
        };
        'walk: for entry in walk {
            if flag.load(Ordering::Relaxed) {
                break;
            }
            let Ok(entry) = entry else { continue };
            scanned += 1;
            if query.archives
                && entry.file_type.is_file()
                && crate::vfs::is_archive_name(&entry.file_name)
                && !is_tool_archive(&entry.file_name)
            {
                let path = entry.path();
                let rel = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
                let found = search_archive(&path, &rel, &matcher, seek.as_ref(), &query, &flag);
                scanned += found.scanned;
                for result in found.results {
                    if !send(result) {
                        flag.store(true, Ordering::Relaxed);
                        break 'walk;
                    }
                }
            }
            let name = entry.file_name.to_string_lossy();
            if query.files_only && entry.file_type.is_dir() {
                continue;
            }
            if !matcher.matches(&name) || (criteria && entry.file_type.is_dir()) {
                continue;
            }
            let path = entry.path();
            let stat = || -> Option<Entry> {
                let mut found = entry::stat(&path).ok()?;
                if criteria && !matcher.accepts(&found, &name) {
                    return None;
                }
                if let Ok(rel) = path.strip_prefix(&root) {
                    found.name = rel.as_os_str().to_os_string();
                }
                Some(found)
            };
            let Some(seek) = &seek else {
                if let Some(found) = stat()
                    && !send(Found {
                        entry: found,
                        hit: None,
                        inside: None,
                    })
                {
                    flag.store(true, Ordering::Relaxed);
                    break;
                }
                continue;
            };
            if !entry.file_type.is_file() {
                continue;
            }
            // a size or an age rules a file out before it is read; with
            // neither, most files hold no hit and the stat is saved
            let early = if criteria {
                match stat() {
                    Some(found) => Some(found),
                    None => continue,
                }
            } else {
                None
            };
            let hits = file_hits(&path, seek, query.first_hit);
            if hits.is_empty() {
                continue;
            }
            let Some(found) = early.or_else(stat) else {
                continue;
            };
            for hit in hits {
                let result = Found {
                    entry: found.clone(),
                    hit: Some(hit),
                    inside: None,
                };
                if !send(result) {
                    flag.store(true, Ordering::Relaxed);
                    break 'walk;
                }
            }
        }
        let _ = tx.send(FindEvent::Done { matches, scanned });
    });
    Ok(FindHandle {
        events: rx,
        cancel,
        thread: Some(thread),
    })
}

/// The archives an external tool opens: searched inside, each member
/// would be a process.
fn is_tool_archive(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy().to_lowercase();
    [".rar", ".7z", ".lha", ".lzh", ".arj", ".cab"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

/// The biggest member read into memory to be searched for content.
const MAX_MEMBER: u64 = 64 * 1024 * 1024;

/// What a search inside one archive came to.
struct InArchive {
    results: Vec<Found>,
    scanned: u64,
}

/// Walk an archive as the find walks a directory: its members by name,
/// and by content when there is content to look for. One that cannot
/// be opened is passed over - it is a file to the walk around it all
/// the same.
fn search_archive(
    path: &Path,
    rel: &Path,
    matcher: &crate::pattern::Matcher,
    seek: Option<&Seek>,
    query: &Query,
    cancel: &AtomicBool,
) -> InArchive {
    use crate::vfs::FsProvider;
    let mut out = InArchive {
        results: Vec::new(),
        scanned: 0,
    };
    let Ok(archive) = crate::archive::ArchiveFs::open(path) else {
        return out;
    };
    let mut dirs = vec![PathBuf::new()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = archive.read_dir(&dir) else {
            continue;
        };
        for mut member in entries {
            if cancel.load(Ordering::Relaxed) {
                return out;
            }
            out.scanned += 1;
            let inner = dir.join(&member.name);
            let name = member.name.to_string_lossy().into_owned();
            if query.skip_hidden && name.starts_with('.') {
                continue;
            }
            let is_dir = member.kind == crate::entry::EntryKind::Dir;
            if is_dir {
                dirs.push(inner.clone());
            }
            let criteria = matcher.has_criteria();
            if !matcher.matches(&name) || (criteria && (is_dir || !matcher.accepts(&member, &name)))
            {
                continue;
            }
            member.name = rel.join(&inner).into_os_string();
            let inside = Some((rel.to_path_buf(), inner.clone()));
            let Some(seek) = seek else {
                out.results.push(Found {
                    entry: member,
                    hit: None,
                    inside,
                });
                continue;
            };
            if member.kind != crate::entry::EntryKind::File || member.size > MAX_MEMBER {
                continue;
            }
            let mut bytes = Vec::new();
            let read = archive
                .open_read(&inner)
                .and_then(|r| r.take(MAX_MEMBER).read_to_end(&mut bytes));
            if read.is_err() {
                continue;
            }
            for hit in memory_hits(&bytes, seek, query.first_hit) {
                out.results.push(Found {
                    entry: member.clone(),
                    hit: Some(hit),
                    inside: inside.clone(),
                });
            }
        }
    }
    out
}

/// [`file_hits`] over bytes already in memory, a line at a time.
fn memory_hits(bytes: &[u8], seek: &Seek, first: bool) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (index, line) in bytes.split(|&b| b == b'\n').enumerate() {
        let line = &line[..line.len().min(MAX_LINE)];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let at = match seek {
            Seek::Lines(re) => re.find(&String::from_utf8_lossy(line)).map(|m| m.start()),
            Seek::Bytes { needles, fold } => {
                let folded;
                let hay = match fold {
                    true => {
                        folded = line.to_ascii_lowercase();
                        &folded[..]
                    }
                    false => line,
                };
                needles
                    .iter()
                    .filter_map(|needle| memchr::memmem::find(hay, needle))
                    .min()
            }
        };
        if let Some(at) = at {
            let text = String::from_utf8_lossy(line);
            // the match as a byte offset into the lossy text, which is
            // the line itself unless it was not UTF-8
            let at = at.min(text.len());
            hits.push(Hit {
                line: index as u64 + 1,
                text: preview(text.as_bytes(), 0, at),
            });
            if first || hits.len() >= MAX_HITS {
                break;
            }
        }
    }
    hits
}

/// Where this file holds what we are looking for: the first line, or
/// every line up to [`MAX_HITS`]. Empty = it does not.
fn file_hits(path: &Path, seek: &Seek, first: bool) -> Vec<Hit> {
    match seek {
        Seek::Bytes { needles, fold } => {
            let mut hits: Vec<Hit> = needles
                .iter()
                .flat_map(|needle| bytes_hits(path, needle, *fold, first))
                .collect();
            // several spellings of the word can land on one line
            hits.sort_by_key(|hit| hit.line);
            hits.dedup_by_key(|hit| hit.line);
            hits.truncate(if first { 1 } else { MAX_HITS });
            hits
        }
        Seek::Lines(re) => line_hits(path, re, first),
    }
}

/// Line by line, decoded leniently. A regular expression is anchored to
/// a line by definition - `.` does not cross one - so reading a line at
/// a time is both correct and bounded, whatever the file turns out to
/// be. Absurdly long lines (a binary with no newline in it) are cut.
fn line_hits(path: &Path, re: &regex::Regex, first: bool) -> Vec<Hit> {
    use std::io::{BufRead, BufReader};
    let mut hits = Vec::new();
    let Ok(file) = File::open(path) else {
        return hits;
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut number = 0u64;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return hits,
            Ok(_) => {}
        }
        number += 1;
        line.truncate(MAX_LINE);
        // the line separator is not part of the line: an anchored
        // pattern ending in $ must be able to reach the end of it
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        let text = String::from_utf8_lossy(&line);
        if let Some(m) = re.find(&text) {
            hits.push(Hit {
                line: number,
                text: preview(text.as_bytes(), 0, m.start()),
            });
            if first || hits.len() >= MAX_HITS {
                return hits;
            }
        }
    }
}

/// Longest line a content search will look at, so a binary file with no
/// newline in it cannot be read into memory whole.
const MAX_LINE: usize = 64 * 1024;

/// Chunked substring search; never loads the whole file. With `fold`
/// the haystack is lowercased as it goes and `needle` must already be
/// lowercase. Lines are counted only when there is a hit to number -
/// most files searched hold none, and they cost no more than before -
/// and the preview is read back from the file, in its own case.
fn bytes_hits(path: &Path, needle: &[u8], fold: bool, first: bool) -> Vec<Hit> {
    use std::os::unix::fs::FileExt;
    let mut hits = Vec::new();
    if needle.is_empty() {
        return hits;
    }
    let Ok(mut file) = File::open(path) else {
        return hits;
    };
    let finder = memchr::memmem::Finder::new(needle);
    let overlap = needle.len() - 1;
    let mut buf = vec![0u8; CHUNK + overlap];
    let mut carry = 0usize;
    // where buf[0] is in the file; how far the newlines are counted, and
    // how many there were; the last line a hit was reported on
    let (mut base, mut counted, mut lines, mut last) = (0u64, 0u64, 0u64, 0u64);
    loop {
        let n = match file.read(&mut buf[carry..]) {
            Ok(0) | Err(_) => return hits,
            Ok(n) => n,
        };
        let len = carry + n;
        if fold {
            buf[carry..len].make_ascii_lowercase();
        }
        let mut from = 0;
        while let Some(at) = finder.find(&buf[from..len]).map(|p| from + p) {
            let abs = base + at as u64;
            if counted < base {
                lines += count_lines(&file, counted, base);
                counted = base;
            }
            let rel = (counted - base) as usize;
            lines += memchr::memchr_iter(b'\n', &buf[rel..at]).count() as u64;
            counted = abs;
            if lines + 1 != last {
                last = lines + 1;
                // the preview's own read, in the file's own case
                let lead = abs.min(2 * PREVIEW as u64);
                let mut window = vec![0u8; 2 * PREVIEW + needle.len() + lead as usize];
                let got = file.read_at(&mut window, abs - lead).unwrap_or(0);
                window.truncate(got);
                let at_w = lead as usize;
                hits.push(Hit {
                    line: last,
                    text: preview(&window, line_start(&window[..at_w]), at_w),
                });
                if first || hits.len() >= MAX_HITS {
                    return hits;
                }
            }
            // the rest of this line has nothing new to report
            from = memchr::memchr(b'\n', &buf[at..len]).map_or(len, |e| at + e + 1);
            if from >= len {
                break;
            }
        }
        carry = overlap.min(len);
        let start = len - carry;
        base += start as u64;
        buf.copy_within(start..len, 0);
    }
}

/// The newlines in bytes `from..to` of the file, read with `pread` so
/// the scan's own position is left where it is.
fn count_lines(file: &File, mut from: u64, to: u64) -> u64 {
    use std::os::unix::fs::FileExt;
    let mut block = vec![0u8; CHUNK];
    let mut lines = 0;
    while from < to {
        let want = (to - from).min(CHUNK as u64) as usize;
        match file.read_at(&mut block[..want], from) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                lines += memchr::memchr_iter(b'\n', &block[..n]).count() as u64;
                from += n as u64;
            }
        }
    }
    lines
}

/// How much a content search reads at a time.
const CHUNK: usize = 64 * 1024;

/// Where the line holding `bytes`' end begins.
fn line_start(bytes: &[u8]) -> usize {
    memchr::memrchr(b'\n', bytes).map_or(0, |at| at + 1)
}

/// The line from `start` that holds the match at `at`, as a preview: a
/// match further along than half a window gets a lead-in of its own.
fn preview(bytes: &[u8], start: usize, at: usize) -> String {
    let end = memchr::memchr(b'\n', &bytes[at..]).map_or(bytes.len(), |e| at + e);
    let from = if at - start > PREVIEW / 2 {
        at - PREVIEW / 4
    } else {
        start
    };
    let text = String::from_utf8_lossy(&bytes[from..end]);
    let text = text.trim();
    let mut shown: String = text.chars().take(PREVIEW).collect();
    if from > start {
        shown.insert(0, '…');
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn collect(handle: FindHandle) -> (Vec<String>, u64) {
        let mut names = Vec::new();
        loop {
            match handle.events.recv().expect("find died without Done") {
                FindEvent::Match(found) => {
                    names.push(found.entry.name.to_string_lossy().into_owned())
                }
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

    /// A tree wide enough that the walk cannot be over before the test
    /// gets to cancel it.
    fn wide_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..40 {
            let sub = dir.path().join(format!("d{i:03}"));
            fs::create_dir_all(&sub).unwrap();
            for j in 0..40 {
                fs::write(sub.join(format!("f{j:03}.txt")), "x").unwrap();
            }
        }
        dir
    }

    /// Joins `thread`, or says so if it will not stop.
    fn joins_within(thread: thread::JoinHandle<()>, secs: u64) -> bool {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = thread.join();
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(secs))
            .is_ok()
    }

    #[test]
    fn cancel_stops_the_walk() {
        let t = wide_tree();
        let mut handle = spawn_find(t.path().to_path_buf(), named("*"), None).unwrap();
        handle.cancel();
        let thread = handle.thread.take().unwrap();
        // keep draining: a walker blocked on a full channel would look
        // like a hang that has nothing to do with cancelling
        let events = handle.events;
        thread::spawn(move || while events.recv().is_ok() {});
        assert!(joins_within(thread, 30), "cancelled walk never stopped");
    }

    #[test]
    fn dropping_the_receiver_stops_the_walk() {
        let t = wide_tree();
        let mut handle = spawn_find(t.path().to_path_buf(), named("*"), None).unwrap();
        let thread = handle.thread.take().unwrap();
        drop(handle.events); // the window closed on a search still running
        assert!(
            joins_within(thread, 30),
            "walk never stopped after its receiver went away"
        );
    }

    fn hits(handle: FindHandle) -> Vec<(String, u64, String)> {
        let mut out = Vec::new();
        loop {
            match handle.events.recv().expect("find died without Done") {
                FindEvent::Match(found) => {
                    let hit = found.hit.expect("a content find reports where");
                    out.push((
                        found.entry.name.to_string_lossy().into_owned(),
                        hit.line,
                        hit.text,
                    ));
                }
                FindEvent::Done { .. } => {
                    out.sort();
                    return out;
                }
            }
        }
    }

    #[test]
    fn a_hit_knows_its_line_and_what_it_says() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.txt"),
            "one\ntwo needle\nthree\nneedle again\n",
        )
        .unwrap();
        let first = hits(spawn_find(dir.path().to_path_buf(), containing("needle"), None).unwrap());
        assert_eq!(first, [("a.txt".into(), 2, "two needle".into())]);

        let mut every = containing("needle");
        every.first_hit = false;
        let all = hits(spawn_find(dir.path().to_path_buf(), every.clone(), None).unwrap());
        assert_eq!(
            all,
            [
                ("a.txt".into(), 2, "two needle".into()),
                ("a.txt".into(), 4, "needle again".into())
            ]
        );

        // a regular expression counts the same way
        every.content.as_mut().unwrap().regex = true;
        every.content.as_mut().unwrap().text = "ne+dle".into();
        let all = hits(spawn_find(dir.path().to_path_buf(), every, None).unwrap());
        assert_eq!(all.iter().map(|h| h.1).collect::<Vec<_>>(), [2, 4]);
    }

    #[test]
    fn lines_are_counted_across_chunks() {
        let dir = tempfile::tempdir().unwrap();
        // a thousand lines of a hundred bytes: the hit is past two chunks
        let mut text = String::new();
        for n in 1..=1000 {
            let body = if n == 900 { "NEEDLE" } else { "x" };
            text.push_str(&format!("{body:<99}\n"));
        }
        fs::write(dir.path().join("long.txt"), &text).unwrap();
        let found = hits(spawn_find(dir.path().to_path_buf(), containing("needle"), None).unwrap());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, 900);
        assert_eq!(
            found[0].2, "NEEDLE",
            "the preview keeps the file's own case"
        );
    }

    #[test]
    fn a_long_line_shows_the_match_not_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let line = format!("{}needle{}", "a".repeat(500), "b".repeat(500));
        fs::write(dir.path().join("wide.txt"), &line).unwrap();
        let found = hits(spawn_find(dir.path().to_path_buf(), containing("needle"), None).unwrap());
        assert!(found[0].2.starts_with('…'), "{}", found[0].2);
        assert!(found[0].2.contains("needle"));
    }

    #[test]
    fn depth_and_ignored_directories_prune_the_walk() {
        let t = tree();
        let mut shallow = named("*");
        shallow.max_depth = Some(1);
        let (names, _) = collect(spawn_find(t.path().to_path_buf(), shallow, None).unwrap());
        assert_eq!(names, ["notes.txt", "src"]);

        let mut ignoring = named("*.rs");
        ignoring.ignore_dirs = parse_ignore_dirs("deep");
        let (names, _) = collect(spawn_find(t.path().to_path_buf(), ignoring, None).unwrap());
        assert_eq!(names, ["src/main.rs"]);

        // a path is from the start, not any directory of that name
        let mut by_path = named("*.rs");
        by_path.ignore_dirs = parse_ignore_dirs("src/deep/ : nowhere");
        let (names, _) = collect(spawn_find(t.path().to_path_buf(), by_path, None).unwrap());
        assert_eq!(names, ["src/main.rs"]);
    }

    #[test]
    fn size_and_age_are_asked_of_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("small.txt"), "x").unwrap();
        fs::write(dir.path().join("sub/big.txt"), vec![b'x'; 4096]).unwrap();
        let mut big = named("*");
        big.name.size = ">1k".into();
        let (names, _) = collect(spawn_find(dir.path().to_path_buf(), big, None).unwrap());
        // the directory is not listed for a question only files answer
        assert_eq!(names, ["sub/big.txt"]);
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

    #[test]
    fn files_only_walks_into_directories_without_listing_them() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/deep.txt"), "x").unwrap();
        fs::write(dir.path().join("top.txt"), "x").unwrap();
        let query = Query {
            files_only: true,
            ..Query::default()
        };
        let (mut names, _) = collect(spawn_find(dir.path().to_path_buf(), query, None).unwrap());
        names.sort();
        assert_eq!(names, ["a/b/deep.txt", "top.txt"]);
    }

    #[test]
    fn a_find_can_look_inside_archives() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("bundle.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            File::create(&archive).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gz);
        for (name, body) in [
            ("src/needle.rs", "fn main() {}\n"),
            ("docs/readme.txt", "first line\nthe Haystack holds it\n"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, body.as_bytes()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        fs::write(dir.path().join("needle.rs"), "outside\n").unwrap();
        let run = |query: Query| -> Vec<Found> {
            let handle = spawn_find(dir.path().to_path_buf(), query, None).unwrap();
            let mut out = Vec::new();
            while let Ok(event) = handle.events.recv() {
                match event {
                    FindEvent::Match(found) => out.push(*found),
                    FindEvent::Done { .. } => break,
                }
            }
            out
        };
        let by_name = |archives| Query {
            name: crate::pattern::Pattern {
                text: "needle*".into(),
                shell: true,
                ..Query::default().name
            },
            archives,
            ..Query::default()
        };
        // off, the archive is a file like any other
        assert_eq!(run(by_name(false)).len(), 1);
        let mut found = run(by_name(true));
        found.sort_by(|a, b| a.entry.name.cmp(&b.entry.name));
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].entry.name, "bundle.tar.gz/src/needle.rs");
        assert_eq!(
            found[0].inside,
            Some((
                PathBuf::from("bundle.tar.gz"),
                PathBuf::from("src/needle.rs")
            ))
        );
        assert_eq!(found[1].inside, None);
        // and by what is in the members, case folded as asked
        let by_content = Query {
            content: Some(Content {
                text: "haystack".into(),
                regex: false,
                case_sensitive: false,
                whole_words: false,
                all_charsets: false,
            }),
            archives: true,
            ..Query::default()
        };
        let found = run(by_content);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].entry.name, "bundle.tar.gz/docs/readme.txt");
        assert_eq!(found[0].hit.as_ref().map(|h| h.line), Some(2));
        assert!(found[0].hit.as_ref().unwrap().text.contains("Haystack"));
    }
}
