//! Chunked file access for the viewer: lines are indexed lazily as the
//! user scrolls, so opening a multi-GB file is instant and memory use
//! stays bounded.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Lines longer than this are broken into virtual lines, which bounds
/// per-line memory and keeps binary files scrollable in text mode.
pub const MAX_LINE: usize = 4096;
const SCAN_CHUNK: usize = 64 * 1024;

/// How the pattern is read. mc's viewer offers the same three.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchKind {
    /// A literal substring.
    #[default]
    Normal,
    /// A regular expression.
    Regex,
    /// A run of bytes written as hex ("7f 45 4c 46" or "7f454c46"),
    /// which is the only way to look for something that is not text.
    Hex,
}

/// What to look for and how, as the viewer's search dialog asks it.
#[derive(Clone, Debug, Default)]
pub struct Search {
    pub pattern: String,
    pub kind: SearchKind,
    pub case_sensitive: bool,
    /// Match only where the pattern stands as a word of its own.
    pub whole_word: bool,
    pub backwards: bool,
}

/// What a "goto" input asks for. One field takes all three because
/// which one you mean is written into the number: a bare number is a
/// line, `0x…` or a trailing `b` is a byte offset, a trailing `%` is a
/// share of the file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Goto {
    /// 1-based, as the status line counts.
    Line(usize),
    Offset(u64),
    /// 0..=100.
    Percent(f64),
}

/// Read a goto input. `None` for anything that is not one of the three.
pub fn parse_goto(input: &str) -> Option<Goto> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(number) = text.strip_suffix('%') {
        let percent: f64 = number.trim().parse().ok()?;
        return (0.0..=100.0)
            .contains(&percent)
            .then_some(Goto::Percent(percent));
    }
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok().map(Goto::Offset);
    }
    if let Some(number) = text.strip_suffix(['b', 'B']) {
        return number.trim().parse().ok().map(Goto::Offset);
    }
    text.parse().ok().map(Goto::Line)
}

/// A compiled search: either a regular expression or a byte sequence.
enum Matcher {
    Text(regex::Regex),
    Bytes(Vec<u8>),
}

impl Matcher {
    fn matches(&self, line: &str) -> bool {
        match self {
            Matcher::Text(re) => re.is_match(line),
            // a byte search never reaches here: find() takes it apart
            Matcher::Bytes(_) => false,
        }
    }
}

impl Search {
    /// Turn the dialog's answers into something that can be run against
    /// a line. Errors are the user's regex, so they are worth showing.
    fn compile(&self) -> Result<Matcher, String> {
        if self.kind == SearchKind::Hex {
            return parse_hex(&self.pattern).map(Matcher::Bytes);
        }
        let body = match self.kind {
            SearchKind::Regex => self.pattern.clone(),
            _ => regex::escape(&self.pattern),
        };
        let build = |pattern: &str| {
            regex::RegexBuilder::new(pattern)
                .case_insensitive(!self.case_sensitive)
                .build()
        };
        // compile what was typed first, so a mistake in it is reported
        // against the pattern the user wrote rather than against the
        // wrapper below
        build(&body).map_err(|err| err.to_string())?;
        // \b would not do: it anchors on the pattern's own edges, and a
        // pattern starting with punctuation has no word boundary there
        let pattern = if self.whole_word {
            format!(r"(?:^|\W)(?:{body})(?:$|\W)")
        } else {
            body
        };
        build(&pattern)
            .map(Matcher::Text)
            .map_err(|err| err.to_string())
    }
}

impl Search {
    /// Where this search matches inside one line, as character index
    /// ranges, for painting the hits. A hexadecimal search matches
    /// bytes rather than characters, so it highlights nothing - the
    /// line the viewer jumped to is the answer it has.
    pub fn ranges(&self, line: &str) -> Vec<(usize, usize)> {
        let Ok(Matcher::Text(re)) = self.compile() else {
            return Vec::new();
        };
        // byte offsets from the regex, character offsets to the caller,
        // because a column on screen is a character
        let mut chars: Vec<usize> = line.char_indices().map(|(at, _)| at).collect();
        chars.push(line.len());
        let index_of = |byte: usize| chars.partition_point(|at| *at < byte);
        re.find_iter(line)
            .map(|m| (index_of(m.start()), index_of(m.end())))
            .filter(|(from, to)| to > from)
            .collect()
    }
}

/// "7f 45 4c 46", "7f454c46" or "0x7f 0x45" - all the same four bytes.
fn parse_hex(text: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = text
        .split_whitespace()
        .map(|token| token.trim_start_matches("0x").trim_start_matches("0X"))
        .collect();
    if cleaned.is_empty() {
        return Err("no bytes in the pattern".into());
    }
    if !cleaned.len().is_multiple_of(2) {
        return Err("a hexadecimal pattern needs whole bytes".into());
    }
    cleaned
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).unwrap_or(""), 16).map_err(|_| {
                format!(
                    "'{}' is not a hexadecimal byte",
                    String::from_utf8_lossy(pair)
                )
            })
        })
        .collect()
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

pub struct FileView {
    file: File,
    pub size: u64,
    /// Start offsets of every line discovered so far; `offsets[0] == 0`.
    offsets: Vec<u64>,
    /// All bytes below this offset have been scanned for line breaks.
    indexed_to: u64,
    /// Length of the still-unterminated line at the scan frontier.
    cur_len: usize,
    fully_indexed: bool,
}

impl FileView {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(FileView {
            file,
            size,
            offsets: vec![0],
            indexed_to: 0,
            cur_len: 0,
            fully_indexed: size == 0,
        })
    }

    pub fn known_lines(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_fully_indexed(&self) -> bool {
        self.fully_indexed
    }

    pub fn offset_of_line(&self, idx: usize) -> Option<u64> {
        self.offsets.get(idx).copied()
    }

    pub fn total_lines(&mut self) -> io::Result<usize> {
        self.ensure_lines(usize::MAX)?;
        Ok(self.offsets.len())
    }

    /// Scan forward until at least `min` line starts are known or EOF.
    pub fn ensure_lines(&mut self, min: usize) -> io::Result<()> {
        if self.offsets.len() >= min || self.fully_indexed {
            return Ok(());
        }
        let mut buf = vec![0u8; SCAN_CHUNK];
        while self.offsets.len() < min && !self.fully_indexed {
            let n = self.file.read_at(&mut buf, self.indexed_to)?;
            if n == 0 {
                self.fully_indexed = true;
                return Ok(());
            }
            for (i, &byte) in buf[..n].iter().enumerate() {
                let next = self.indexed_to + i as u64 + 1;
                let break_here = if byte == b'\n' {
                    self.cur_len = 0;
                    true
                } else {
                    self.cur_len += 1;
                    if self.cur_len >= MAX_LINE {
                        self.cur_len = 0;
                        true
                    } else {
                        false
                    }
                };
                if break_here && next < self.size {
                    self.offsets.push(next);
                }
            }
            self.indexed_to += n as u64;
            if self.indexed_to >= self.size {
                self.fully_indexed = true;
            }
        }
        Ok(())
    }

    /// Follow mode: pick up external changes to the file. Returns true
    /// when the size changed. Growth resumes indexing at the frontier;
    /// shrinking (truncate/rotate) rebuilds the index from scratch.
    pub fn refresh(&mut self) -> io::Result<bool> {
        let size = self.file.metadata()?.len();
        if size == self.size {
            return Ok(false);
        }
        if size < self.size {
            self.offsets = vec![0];
            self.indexed_to = 0;
            self.cur_len = 0;
            self.size = size;
            self.fully_indexed = size == 0;
            return Ok(true);
        }
        // A break exactly at the old EOF never pushed the next line's
        // start (that would have been a phantom line); now that data
        // follows it, register it.
        if self.cur_len == 0 && self.indexed_to > 0 && self.offsets.last() != Some(&self.indexed_to)
        {
            self.offsets.push(self.indexed_to);
        }
        self.size = size;
        self.fully_indexed = false;
        Ok(true)
    }

    /// Read one line (lossy UTF-8, trailing newline/CR stripped).
    /// None once past the last line.
    pub fn line(&mut self, idx: usize) -> io::Result<Option<String>> {
        self.ensure_lines(idx + 2)?;
        let Some(&start) = self.offsets.get(idx) else {
            return Ok(None);
        };
        let end = self.offsets.get(idx + 1).copied().unwrap_or(self.size);
        let len = end.saturating_sub(start).min(MAX_LINE as u64 + 1) as usize;
        let mut buf = vec![0u8; len];
        let n = self.file.read_at(&mut buf, start)?;
        buf.truncate(n);
        if buf.last() == Some(&b'\n') {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }

    /// Case-insensitive substring search, scanning forward from `start`.
    /// The plain form, kept for callers that want no options at all.
    pub fn search_from(&mut self, start: usize, needle: &str) -> io::Result<Option<usize>> {
        let search = Search {
            pattern: needle.to_string(),
            ..Search::default()
        };
        self.find(start, &search)
    }

    /// Find the line matching `search`, starting at `from` and moving
    /// the way the search says. Returns the line index, which is what
    /// the viewer scrolls to - a hexadecimal search finds an offset and
    /// this reports the line holding it.
    pub fn find(&mut self, from: usize, search: &Search) -> io::Result<Option<usize>> {
        let matcher = search.compile().map_err(io::Error::other)?;
        if let Matcher::Bytes(needle) = &matcher {
            let start = self.offset_of_line(from).unwrap_or(0);
            return match self.find_bytes(start, needle, search.backwards)? {
                Some(offset) => self.line_at_offset(offset).map(Some),
                None => Ok(None),
            };
        }
        if search.backwards {
            for idx in (0..=from).rev() {
                if let Some(line) = self.line(idx)?
                    && matcher.matches(&line)
                {
                    return Ok(Some(idx));
                }
            }
            return Ok(None);
        }
        let mut idx = from;
        while let Some(line) = self.line(idx)? {
            if matcher.matches(&line) {
                return Ok(Some(idx));
            }
            idx += 1;
        }
        Ok(None)
    }

    /// Find a byte sequence, which is what a hexadecimal search is for:
    /// the bytes it names may not be text at all.
    pub fn find_bytes(&self, from: u64, needle: &[u8], backwards: bool) -> io::Result<Option<u64>> {
        if needle.is_empty() || needle.len() as u64 > self.size {
            return Ok(None);
        }
        let overlap = needle.len() as u64 - 1;
        if backwards {
            let mut end = from.min(self.size);
            while end > 0 {
                let start = end.saturating_sub(SCAN_CHUNK as u64);
                let buf = self.read_at(start, (end - start + overlap) as usize)?;
                if let Some(at) = rfind(&buf, needle) {
                    let hit = start + at as u64;
                    if hit < from {
                        return Ok(Some(hit));
                    }
                }
                end = start;
            }
            return Ok(None);
        }
        let mut at = from;
        while at < self.size {
            let buf = self.read_at(at, SCAN_CHUNK + overlap as usize)?;
            if buf.len() < needle.len() {
                break;
            }
            if let Some(found) = find_sub(&buf, needle) {
                return Ok(Some(at + found as u64));
            }
            at += SCAN_CHUNK as u64;
        }
        Ok(None)
    }

    /// The line a goto input names. A line number is 1-based on the
    /// way in and 0-based on the way out, because that is the gap
    /// between what a status line counts and what an index is.
    pub fn goto_line(&mut self, goto: Goto) -> io::Result<usize> {
        let offset = match goto {
            Goto::Line(line) => return Ok(line.saturating_sub(1)),
            Goto::Offset(offset) => offset.min(self.size),
            Goto::Percent(percent) => ((self.size as f64) * percent / 100.0).round() as u64,
        };
        self.line_at_offset(offset.min(self.size))
    }

    /// Which line holds this byte offset.
    pub fn line_at_offset(&mut self, offset: u64) -> io::Result<usize> {
        // index far enough that the offset is inside what is known
        while !self.fully_indexed && self.indexed_to <= offset {
            let before = self.offsets.len();
            self.ensure_lines(self.offsets.len() + 4096)?;
            if self.offsets.len() == before {
                break;
            }
        }
        Ok(match self.offsets.binary_search(&offset) {
            Ok(idx) => idx,
            Err(idx) => idx.saturating_sub(1),
        })
    }

    /// Raw bytes for the hex view.
    pub fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let n = FileExt::read_at(&self.file, &mut buf, offset)?;
        buf.truncate(n);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn view_of(content: &[u8]) -> (tempfile::TempDir, FileView) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(content)
            .unwrap();
        let view = FileView::open(&path).unwrap();
        (dir, view)
    }

    #[test]
    fn indexes_lines_across_chunk_boundaries() {
        // 20k lines of "line NNNNN" ≈ 220 KB, several scan chunks
        let content: String = (0..20_000).map(|i| format!("line {i:05}\n")).collect();
        let (_dir, mut v) = view_of(content.as_bytes());
        assert_eq!(v.line(0).unwrap().unwrap(), "line 00000");
        assert_eq!(v.line(19_999).unwrap().unwrap(), "line 19999");
        assert_eq!(v.line(20_000).unwrap(), None); // no phantom after trailing \n
        assert_eq!(v.total_lines().unwrap(), 20_000);
    }

    #[test]
    fn long_lines_get_virtual_breaks() {
        let content = vec![b'x'; MAX_LINE * 2 + 100];
        let (_dir, mut v) = view_of(&content);
        assert_eq!(v.line(0).unwrap().unwrap().len(), MAX_LINE);
        assert_eq!(v.line(1).unwrap().unwrap().len(), MAX_LINE);
        assert_eq!(v.line(2).unwrap().unwrap().len(), 100);
        assert_eq!(v.line(3).unwrap(), None);
    }

    #[test]
    fn crlf_and_lossy_decoding() {
        let (_dir, mut v) = view_of(b"dos line\r\nnext\nbad \xff byte\n");
        assert_eq!(v.line(0).unwrap().unwrap(), "dos line");
        assert_eq!(v.line(1).unwrap().unwrap(), "next");
        assert_eq!(v.line(2).unwrap().unwrap(), "bad \u{FFFD} byte");
    }

    #[test]
    fn empty_file_is_one_blank_line() {
        let (_dir, mut v) = view_of(b"");
        assert_eq!(v.line(0).unwrap().unwrap(), "");
        assert_eq!(v.line(1).unwrap(), None);
    }

    #[test]
    fn search_is_case_insensitive_and_positional() {
        let (_dir, mut v) = view_of(b"alpha\nBRAVO\ncharlie\nbravo again\n");
        assert_eq!(v.search_from(0, "bravo").unwrap(), Some(1));
        assert_eq!(v.search_from(2, "bravo").unwrap(), Some(3));
        assert_eq!(v.search_from(4, "bravo").unwrap(), None);
        assert_eq!(v.search_from(0, "zulu").unwrap(), None);
    }

    #[test]
    fn refresh_follows_appends_and_truncation() {
        use std::fs::OpenOptions;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let mut v = FileView::open(&path).unwrap();
        assert_eq!(v.total_lines().unwrap(), 2); // fully indexed
        assert!(!v.refresh().unwrap());

        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"three\nfour\n").unwrap();
        assert!(v.refresh().unwrap());
        assert_eq!(v.total_lines().unwrap(), 4);
        assert_eq!(v.line(2).unwrap().unwrap(), "three");
        assert_eq!(v.line(3).unwrap().unwrap(), "four");

        // growth of an unterminated last line
        f.write_all(b"fi").unwrap();
        assert!(v.refresh().unwrap());
        assert_eq!(v.line(4).unwrap().unwrap(), "fi");
        f.write_all(b"ve\n").unwrap();
        assert!(v.refresh().unwrap());
        assert_eq!(v.total_lines().unwrap(), 5);
        assert_eq!(v.line(4).unwrap().unwrap(), "five");

        // rotation: smaller file rebuilds the index
        std::fs::write(&path, "fresh\n").unwrap();
        assert!(v.refresh().unwrap());
        assert_eq!(v.total_lines().unwrap(), 1);
        assert_eq!(v.line(0).unwrap().unwrap(), "fresh");
    }

    #[test]
    fn read_at_returns_raw_bytes() {
        let (_dir, v) = view_of(b"0123456789");
        assert_eq!(v.read_at(2, 4).unwrap(), b"2345");
        assert_eq!(v.read_at(8, 16).unwrap(), b"89"); // truncated at EOF
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    fn view(content: &[u8]) -> (tempfile::TempDir, FileView) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, content).unwrap();
        let file = FileView::open(&path).unwrap();
        (dir, file)
    }

    fn search(pattern: &str) -> Search {
        Search {
            pattern: pattern.to_string(),
            ..Search::default()
        }
    }

    const TEXT: &[u8] = b"first line\nsecond LINE here\nthe third\nlining up\nlast line\n";

    #[test]
    fn a_plain_search_ignores_case_until_told_not_to() {
        let (_dir, mut f) = view(TEXT);
        assert_eq!(f.find(0, &search("line")).unwrap(), Some(0));
        assert_eq!(f.find(1, &search("line")).unwrap(), Some(1));

        let mut cased = search("LINE");
        cased.case_sensitive = true;
        assert_eq!(f.find(0, &cased).unwrap(), Some(1));
    }

    #[test]
    fn whole_words_does_not_match_inside_one() {
        let (_dir, mut f) = view(TEXT);
        let mut whole = search("line");
        whole.whole_word = true;
        // "lining" holds the letters but is not the word
        assert_eq!(f.find(3, &whole).unwrap(), Some(4));
        // and without the option it matches "lining up"
        assert_eq!(f.find(3, &search("lin")).unwrap(), Some(3));
    }

    #[test]
    fn a_pattern_is_literal_unless_it_is_a_regex() {
        let (_dir, mut f) = view(b"a.c\nabc\n");
        // "a.c" as a literal matches only the first line
        assert_eq!(f.find(0, &search("a.c")).unwrap(), Some(0));
        assert_eq!(f.find(1, &search("a.c")).unwrap(), None);

        let mut re = search("a.c");
        re.kind = SearchKind::Regex;
        assert_eq!(f.find(1, &re).unwrap(), Some(1));
    }

    #[test]
    fn backwards_walks_the_other_way_and_stops_at_the_top() {
        let (_dir, mut f) = view(TEXT);
        let mut back = search("line");
        back.backwards = true;
        assert_eq!(f.find(4, &back).unwrap(), Some(4));
        assert_eq!(f.find(3, &back).unwrap(), Some(1));
        assert_eq!(f.find(0, &back).unwrap(), Some(0));
        let mut missing = search("nowhere");
        missing.backwards = true;
        assert_eq!(f.find(4, &missing).unwrap(), None);
    }

    #[test]
    fn a_bad_regex_is_reported_rather_than_ignored() {
        let (_dir, mut f) = view(TEXT);
        let mut re = search("a(b");
        re.kind = SearchKind::Regex;
        let err = f.find(0, &re).unwrap_err().to_string();
        assert!(err.contains("unclosed"), "{err}");
        // the message quotes what was typed, not the whole-word
        // wrapper rcmd would have put around it
        assert!(err.contains("a(b"), "{err}");
        assert!(!err.contains(r"(?:^|\W)"), "{err}");

        re.whole_word = true;
        let err = f.find(0, &re).unwrap_err().to_string();
        assert!(!err.contains(r"(?:^|\W)"), "{err}");
    }

    #[test]
    fn a_hexadecimal_search_finds_bytes_that_are_not_text() {
        let mut content = b"header\n".to_vec();
        content.extend_from_slice(&[0x7f, 0x45, 0x4c, 0x46, 0x00, 0xff]);
        content.extend_from_slice(b"\ntrailer\n");
        let (_dir, mut f) = view(&content);

        let mut hex = search("7f 45 4c 46");
        hex.kind = SearchKind::Hex;
        // the ELF magic is on the second line
        assert_eq!(f.find(0, &hex).unwrap(), Some(1));

        // the spellings are interchangeable
        for spelling in ["7f454c46", "0x7f 0x45 0x4c 0x46"] {
            let mut hex = search(spelling);
            hex.kind = SearchKind::Hex;
            assert_eq!(f.find(0, &hex).unwrap(), Some(1), "{spelling}");
        }

        let mut absent = search("de ad be ef");
        absent.kind = SearchKind::Hex;
        assert_eq!(f.find(0, &absent).unwrap(), None);
    }

    #[test]
    fn a_hexadecimal_pattern_has_to_be_hexadecimal() {
        let (_dir, mut f) = view(TEXT);
        for bad in ["", "7f4", "zz"] {
            let mut hex = search(bad);
            hex.kind = SearchKind::Hex;
            assert!(f.find(0, &hex).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_byte_search_crosses_the_chunk_boundary_it_scans_in() {
        // the needle straddles the seam between two scan chunks, which
        // is the one place a chunked search can lose a match
        let mut content = vec![b'.'; SCAN_CHUNK - 2];
        content.extend_from_slice(b"NEEDLE");
        content.extend_from_slice(&[b'.'; 100]);
        let (_dir, f) = view(&content);
        assert_eq!(
            f.find_bytes(0, b"NEEDLE", false).unwrap(),
            Some(SCAN_CHUNK as u64 - 2)
        );
        assert_eq!(
            f.find_bytes(content.len() as u64, b"NEEDLE", true).unwrap(),
            Some(SCAN_CHUNK as u64 - 2)
        );
    }

    #[test]
    fn an_offset_maps_back_to_the_line_holding_it() {
        let (_dir, mut f) = view(TEXT);
        assert_eq!(f.line_at_offset(0).unwrap(), 0);
        assert_eq!(f.line_at_offset(3).unwrap(), 0);
        assert_eq!(f.line_at_offset(11).unwrap(), 1);
        assert_eq!(f.line_at_offset(TEXT.len() as u64 - 1).unwrap(), 4);
    }
}

#[cfg(test)]
mod goto_tests {
    use super::*;

    #[test]
    fn a_bare_number_is_a_line() {
        assert_eq!(parse_goto("42"), Some(Goto::Line(42)));
        assert_eq!(parse_goto("  7 "), Some(Goto::Line(7)));
    }

    #[test]
    fn hex_and_a_trailing_b_are_both_offsets() {
        assert_eq!(parse_goto("0x7b"), Some(Goto::Offset(123)));
        assert_eq!(parse_goto("0X7B"), Some(Goto::Offset(123)));
        assert_eq!(parse_goto("123b"), Some(Goto::Offset(123)));
        assert_eq!(parse_goto("123B"), Some(Goto::Offset(123)));
    }

    #[test]
    fn a_trailing_percent_is_a_share_of_the_file() {
        assert_eq!(parse_goto("50%"), Some(Goto::Percent(50.0)));
        assert_eq!(parse_goto("0%"), Some(Goto::Percent(0.0)));
        assert_eq!(parse_goto("100%"), Some(Goto::Percent(100.0)));
        assert_eq!(parse_goto("12.5%"), Some(Goto::Percent(12.5)));
        // a share of a file cannot be more than the file
        assert_eq!(parse_goto("101%"), None);
        assert_eq!(parse_goto("-1%"), None);
    }

    #[test]
    fn nonsense_is_refused_rather_than_taken_as_line_zero() {
        for bad in ["", "  ", "abc", "0xzz", "12x", "%"] {
            assert_eq!(parse_goto(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_three_forms_land_where_they_say() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        // ten lines of ten bytes each
        let body: String = (0..10).map(|n| format!("line {n}!!!\n")).collect();
        std::fs::write(&path, &body).unwrap();
        let mut f = FileView::open(&path).unwrap();
        assert_eq!(f.size, 100);

        assert_eq!(f.goto_line(Goto::Line(1)).unwrap(), 0);
        assert_eq!(f.goto_line(Goto::Line(5)).unwrap(), 4);
        // line 0 and line 1 are the same place: there is no line zero
        assert_eq!(f.goto_line(Goto::Line(0)).unwrap(), 0);

        assert_eq!(f.goto_line(Goto::Offset(0)).unwrap(), 0);
        assert_eq!(f.goto_line(Goto::Offset(25)).unwrap(), 2);
        // past the end lands at the end rather than failing
        assert_eq!(f.goto_line(Goto::Offset(9_999)).unwrap(), 9);

        assert_eq!(f.goto_line(Goto::Percent(0.0)).unwrap(), 0);
        assert_eq!(f.goto_line(Goto::Percent(50.0)).unwrap(), 5);
        assert_eq!(f.goto_line(Goto::Percent(100.0)).unwrap(), 9);
    }
}
