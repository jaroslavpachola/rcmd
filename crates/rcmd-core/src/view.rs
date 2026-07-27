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
    pub fn search_from(&mut self, start: usize, needle: &str) -> io::Result<Option<usize>> {
        let needle = needle.to_lowercase();
        let mut idx = start;
        loop {
            match self.line(idx)? {
                None => return Ok(None),
                Some(line) => {
                    if line.to_lowercase().contains(&needle) {
                        return Ok(Some(idx));
                    }
                }
            }
            idx += 1;
        }
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
