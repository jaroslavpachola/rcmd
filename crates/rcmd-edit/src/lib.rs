//! The internal editor core: a ropey text buffer with cursor, selection,
//! unlimited undo/redo, regex search and atomic save. TUI-free — the
//! frontend renders lines and maps keys; everything stateful lives here.
//!
//! Scope (per PLAN2 P4): no multi-cursor, no LSP, no splits. Files are
//! UTF-8 (lossy) with CRLF detected on load and restored on save.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use ropey::Rope;

#[cfg(feature = "syntax")]
mod highlight;
#[cfg(feature = "syntax")]
pub use highlight::Highlighter;

/// Stub so the frontend compiles identically without the feature.
#[cfg(not(feature = "syntax"))]
pub struct Highlighter;

#[cfg(not(feature = "syntax"))]
impl Highlighter {
    pub fn new(_path: &Path, _len_bytes: usize) -> Option<Highlighter> {
        None
    }

    pub fn invalidate_from(&mut self, _line: usize) {}

    pub fn range_spans(
        &mut self,
        _ed: &Editor,
        _start: usize,
        count: usize,
    ) -> Vec<Vec<(usize, usize, [u8; 3])>> {
        vec![Vec::new(); count]
    }
}

/// Cursor / selection position: line and column in characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct Match {
    pub pos: Pos,
    /// Length in characters (may be 0 for an empty regex match).
    pub len: usize,
}

/// One atomic buffer change: at `at`, `removed` was replaced by
/// `inserted`. Reverting swaps the two.
struct Edit {
    at: usize,
    removed: String,
    inserted: String,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Kind {
    Type,
    Backspace,
    Delete,
    Other,
}

struct EditGroup {
    id: u64,
    kind: Kind,
    edits: Vec<Edit>,
    before: Pos,
    after: Pos,
}

pub struct Editor {
    rope: Rope,
    pub path: PathBuf,
    crlf: bool,
    pub cursor: Pos,
    desired_col: usize,
    /// Selection anchor; `sticky` keeps it through plain movement (F3
    /// marking), otherwise unshifted movement drops it.
    anchor: Option<Pos>,
    sticky: bool,
    undo: Vec<EditGroup>,
    redo: Vec<EditGroup>,
    next_id: u64,
    /// Undo-group id the file was last saved at (0 = pristine).
    saved_id: u64,
    clipboard: String,
    pub search: String,
}

impl Editor {
    pub fn open(path: &Path) -> io::Result<Editor> {
        let bytes = std::fs::read(path)?;
        if bytes[..bytes.len().min(8192)].contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "binary file — use the F3 viewer",
            ));
        }
        let text = String::from_utf8_lossy(&bytes);
        let crlf = text.contains("\r\n");
        let rope = if crlf {
            Rope::from_str(&text.replace("\r\n", "\n"))
        } else {
            Rope::from_str(&text)
        };
        Ok(Editor::with_rope(rope, path.to_path_buf(), crlf))
    }

    /// New empty buffer for a file that does not exist yet.
    pub fn create(path: &Path) -> Editor {
        Editor::with_rope(Rope::new(), path.to_path_buf(), false)
    }

    fn with_rope(rope: Rope, path: PathBuf, crlf: bool) -> Editor {
        Editor {
            rope,
            path,
            crlf,
            cursor: Pos { line: 0, col: 0 },
            desired_col: 0,
            anchor: None,
            sticky: false,
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 1,
            saved_id: 0,
            clipboard: String::new(),
            search: String::new(),
        }
    }

    /// Atomic save: write to a temp file next to the target, keep the
    /// original permissions, rename over. Symlinks are followed.
    pub fn save(&mut self) -> io::Result<()> {
        let target = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        let dir = target.parent().unwrap_or_else(|| Path::new("."));
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        let tmp = dir.join(format!(".{name}.rcmd-{}", std::process::id()));
        let result = (|| -> io::Result<()> {
            let mut out = io::BufWriter::new(std::fs::File::create(&tmp)?);
            for chunk in self.rope.chunks() {
                if self.crlf {
                    let mut rest = chunk;
                    while let Some(i) = rest.find('\n') {
                        out.write_all(&rest.as_bytes()[..i])?;
                        out.write_all(b"\r\n")?;
                        rest = &rest[i + 1..];
                    }
                    out.write_all(rest.as_bytes())?;
                } else {
                    out.write_all(chunk.as_bytes())?;
                }
            }
            out.flush()?;
            if let Ok(meta) = std::fs::metadata(&target) {
                let _ = std::fs::set_permissions(&tmp, meta.permissions());
            }
            std::fs::rename(&tmp, &target)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        } else {
            self.saved_id = self.top_id();
        }
        result
    }

    fn top_id(&self) -> u64 {
        self.undo.last().map(|g| g.id).unwrap_or(0)
    }

    pub fn modified(&self) -> bool {
        self.top_id() != self.saved_id
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// Line content without the trailing newline.
    pub fn line(&self, idx: usize) -> String {
        if idx >= self.rope.len_lines() {
            return String::new();
        }
        let slice = self.rope.line(idx);
        let mut s = slice.to_string();
        if s.ends_with('\n') {
            s.pop();
        }
        s
    }

    pub fn line_len(&self, idx: usize) -> usize {
        if idx >= self.rope.len_lines() {
            return 0;
        }
        let slice = self.rope.line(idx);
        let n = slice.len_chars();
        if n > 0 && slice.char(n - 1) == '\n' {
            n - 1
        } else {
            n
        }
    }

    fn char_idx(&self, pos: Pos) -> usize {
        self.rope.line_to_char(pos.line) + pos.col.min(self.line_len(pos.line))
    }

    fn pos_at(&self, idx: usize) -> Pos {
        let line = self.rope.char_to_line(idx);
        Pos {
            line,
            col: idx - self.rope.line_to_char(line),
        }
    }

    // ----- selection ------------------------------------------------

    /// Ordered selection range in char indices, when non-empty.
    pub fn sel_range(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        let a = self.char_idx(anchor);
        let b = self.char_idx(self.cursor);
        if a == b {
            None
        } else {
            Some((a.min(b), a.max(b)))
        }
    }

    /// Selection clipped to one line, as char columns, for rendering.
    pub fn sel_on_line(&self, line: usize) -> Option<(usize, usize)> {
        let (a, b) = self.sel_range()?;
        let start = self.rope.line_to_char(line);
        let end = start + self.rope.line(line).len_chars();
        if b <= start || a >= end {
            return None;
        }
        Some((a.max(start) - start, b.min(end) - start))
    }

    pub fn has_selection(&self) -> bool {
        self.sel_range().is_some()
    }

    /// First and last line touched by the selection.
    pub fn sel_line_range(&self) -> Option<(usize, usize)> {
        let (a, b) = self.sel_range()?;
        Some((self.pos_at(a).line, self.pos_at(b).line))
    }

    /// F3-style mark: set the anchor (kept through plain movement) or
    /// clear it.
    pub fn toggle_mark(&mut self) {
        if self.anchor.is_some() {
            self.anchor = None;
            self.sticky = false;
        } else {
            self.anchor = Some(self.cursor);
            self.sticky = true;
        }
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
        self.sticky = false;
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(Pos { line: 0, col: 0 });
        self.sticky = true;
        self.cursor = self.pos_at(self.rope.len_chars());
        self.desired_col = self.cursor.col;
    }

    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.sel_range()?;
        Some(self.rope.slice(a..b).to_string())
    }

    // ----- movement -------------------------------------------------

    fn place(&mut self, pos: Pos, select: bool, keep_desired: bool) {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else if !self.sticky {
            self.anchor = None;
        }
        self.cursor = pos;
        if !keep_desired {
            self.desired_col = pos.col;
        }
    }

    pub fn move_left(&mut self, select: bool) {
        let pos = if self.cursor.col > 0 {
            Pos {
                line: self.cursor.line,
                col: self.cursor.col - 1,
            }
        } else if self.cursor.line > 0 {
            Pos {
                line: self.cursor.line - 1,
                col: self.line_len(self.cursor.line - 1),
            }
        } else {
            self.cursor
        };
        self.place(pos, select, false);
    }

    pub fn move_right(&mut self, select: bool) {
        let len = self.line_len(self.cursor.line);
        let pos = if self.cursor.col < len {
            Pos {
                line: self.cursor.line,
                col: self.cursor.col + 1,
            }
        } else if self.cursor.line + 1 < self.line_count() {
            Pos {
                line: self.cursor.line + 1,
                col: 0,
            }
        } else {
            self.cursor
        };
        self.place(pos, select, false);
    }

    pub fn move_vert(&mut self, delta: isize, select: bool) {
        let line = self
            .cursor
            .line
            .saturating_add_signed(delta)
            .min(self.line_count().saturating_sub(1));
        let col = self.desired_col.min(self.line_len(line));
        self.place(Pos { line, col }, select, true);
    }

    pub fn move_home(&mut self, select: bool) {
        // smart home: first non-blank, then column 0
        let text = self.line(self.cursor.line);
        let indent = text.chars().take_while(|c| c.is_whitespace()).count();
        let col = if self.cursor.col == indent { 0 } else { indent };
        self.place(
            Pos {
                line: self.cursor.line,
                col,
            },
            select,
            false,
        );
    }

    pub fn move_end(&mut self, select: bool) {
        self.place(
            Pos {
                line: self.cursor.line,
                col: self.line_len(self.cursor.line),
            },
            select,
            false,
        );
    }

    pub fn move_top(&mut self, select: bool) {
        self.place(Pos { line: 0, col: 0 }, select, false);
    }

    pub fn move_bottom(&mut self, select: bool) {
        let pos = self.pos_at(self.rope.len_chars());
        self.place(pos, select, false);
    }

    pub fn move_word(&mut self, forward: bool, select: bool) {
        let mut idx = self.char_idx(self.cursor);
        let len = self.rope.len_chars();
        let word = |c: char| c.is_alphanumeric() || c == '_';
        if forward {
            while idx < len && word(self.rope.char(idx)) {
                idx += 1;
            }
            while idx < len && !word(self.rope.char(idx)) {
                idx += 1;
            }
        } else {
            while idx > 0 && !word(self.rope.char(idx - 1)) {
                idx -= 1;
            }
            while idx > 0 && word(self.rope.char(idx - 1)) {
                idx -= 1;
            }
        }
        let pos = self.pos_at(idx);
        self.place(pos, select, false);
    }

    pub fn goto(&mut self, pos: Pos, select: bool) {
        let line = pos.line.min(self.line_count().saturating_sub(1));
        let col = pos.col.min(self.line_len(line));
        self.place(Pos { line, col }, select, false);
    }

    // ----- editing --------------------------------------------------

    /// The single mutation primitive: replace `remove_chars` characters
    /// at `at` with `insert`, recording one undoable edit.
    fn splice(&mut self, at: usize, remove_chars: usize, insert: &str, kind: Kind) {
        let before = self.cursor;
        let removed = self.rope.slice(at..at + remove_chars).to_string();
        self.rope.remove(at..at + remove_chars);
        self.rope.insert(at, insert);
        let after = self.pos_at(at + insert.chars().count());
        self.cursor = after;
        self.desired_col = after.col;
        self.anchor = None;
        self.sticky = false;
        self.redo.clear();

        // coalesce bursts of typing / deleting into one undo step
        if kind != Kind::Other
            && let Some(group) = self.undo.last_mut()
            && group.kind == kind
            && group.edits.len() == 1
        {
            let last = &mut group.edits[0];
            let merged = match kind {
                Kind::Type
                    if removed.is_empty()
                        && !insert.contains('\n')
                        && last.inserted.chars().count() < 64
                        && last.at + last.inserted.chars().count() == at =>
                {
                    last.inserted.push_str(insert);
                    true
                }
                Kind::Backspace
                    if insert.is_empty()
                        && !removed.contains('\n')
                        && last.inserted.is_empty()
                        && at + remove_chars == last.at =>
                {
                    last.at = at;
                    last.removed = format!("{removed}{}", last.removed);
                    true
                }
                Kind::Delete
                    if insert.is_empty()
                        && !removed.contains('\n')
                        && last.inserted.is_empty()
                        && at == last.at =>
                {
                    last.removed.push_str(&removed);
                    true
                }
                _ => false,
            };
            if merged {
                group.after = after;
                return;
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.undo.push(EditGroup {
            id,
            kind,
            edits: vec![Edit {
                at,
                removed,
                inserted: insert.to_string(),
            }],
            before,
            after,
        });
    }

    /// Type `text`, replacing the selection if there is one.
    pub fn insert(&mut self, text: &str) {
        let kind = if text.chars().count() == 1 && !text.contains('\n') {
            Kind::Type
        } else {
            Kind::Other
        };
        match self.sel_range() {
            Some((a, b)) => self.splice(a, b - a, text, Kind::Other),
            None => self.splice(self.char_idx(self.cursor), 0, text, kind),
        }
    }

    /// Enter: newline plus the current line's leading whitespace.
    pub fn newline(&mut self) {
        let indent: String = if self.sel_range().is_some() {
            String::new()
        } else {
            self.line(self.cursor.line)
                .chars()
                .take(self.cursor.col)
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect()
        };
        self.insert(&format!("\n{indent}"));
    }

    pub fn backspace(&mut self) {
        if let Some((a, b)) = self.sel_range() {
            self.splice(a, b - a, "", Kind::Other);
            return;
        }
        let at = self.char_idx(self.cursor);
        if at > 0 {
            self.splice(at - 1, 1, "", Kind::Backspace);
        }
    }

    pub fn delete_forward(&mut self) {
        if let Some((a, b)) = self.sel_range() {
            self.splice(a, b - a, "", Kind::Other);
            return;
        }
        let at = self.char_idx(self.cursor);
        if at < self.rope.len_chars() {
            self.splice(at, 1, "", Kind::Delete);
        }
    }

    /// F8: delete the selection, or the whole current line.
    pub fn delete_selection_or_line(&mut self) {
        if let Some((a, b)) = self.sel_range() {
            self.splice(a, b - a, "", Kind::Other);
            return;
        }
        let start = self.rope.line_to_char(self.cursor.line);
        let len = self.rope.line(self.cursor.line).len_chars();
        if len > 0 || start > 0 {
            // a lone last line without newline: eat the newline before it
            let (at, n) = if len == 0 {
                (start - 1, 1)
            } else {
                (start, len)
            };
            self.splice(at, n, "", Kind::Other);
        }
    }

    pub fn copy(&mut self) {
        if let Some(text) = self.selected_text() {
            self.clipboard = text;
        }
    }

    pub fn cut(&mut self) {
        if let Some((a, b)) = self.sel_range() {
            self.clipboard = self.rope.slice(a..b).to_string();
            self.splice(a, b - a, "", Kind::Other);
        }
    }

    pub fn paste(&mut self) {
        if self.clipboard.is_empty() {
            return;
        }
        let text = self.clipboard.clone();
        self.insert(&text);
    }

    pub fn has_clipboard(&self) -> bool {
        !self.clipboard.is_empty()
    }

    // ----- undo / redo ----------------------------------------------

    pub fn undo(&mut self) -> bool {
        let Some(group) = self.undo.pop() else {
            return false;
        };
        for edit in group.edits.iter().rev() {
            let end = edit.at + edit.inserted.chars().count();
            self.rope.remove(edit.at..end);
            self.rope.insert(edit.at, &edit.removed);
        }
        self.cursor = group.before;
        self.desired_col = self.cursor.col;
        self.anchor = None;
        self.sticky = false;
        self.redo.push(group);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(group) = self.redo.pop() else {
            return false;
        };
        for edit in &group.edits {
            let end = edit.at + edit.removed.chars().count();
            self.rope.remove(edit.at..end);
            self.rope.insert(edit.at, &edit.inserted);
        }
        self.cursor = group.after;
        self.desired_col = self.cursor.col;
        self.anchor = None;
        self.sticky = false;
        self.undo.push(group);
        true
    }

    // ----- search / replace ------------------------------------------

    /// Compile `pattern` with smartcase (case-insensitive unless it has
    /// an uppercase letter).
    pub fn compile(pattern: &str) -> Result<regex::Regex, regex::Error> {
        regex::RegexBuilder::new(pattern)
            .case_insensitive(!pattern.chars().any(char::is_uppercase))
            .build()
    }

    /// First match at or after `start`, wrapping around once.
    pub fn find_from(&self, start: Pos, re: &regex::Regex) -> Option<Match> {
        let lines = self.line_count();
        for step in 0..=lines {
            let line = (start.line + step) % lines;
            let text = self.line(line);
            let from_col = if step == 0 { start.col } else { 0 };
            let from_byte = text
                .char_indices()
                .nth(from_col)
                .map(|(b, _)| b)
                .unwrap_or(text.len());
            if let Some(m) = re.find(&text[from_byte..]) {
                let col = text[..from_byte + m.start()].chars().count();
                return Some(Match {
                    pos: Pos { line, col },
                    len: m.as_str().chars().count(),
                });
            }
            if step == lines {
                break;
            }
        }
        // the wrapped pass above skipped the head of the start line
        let text = self.line(start.line);
        let upto: usize = text
            .char_indices()
            .nth(start.col)
            .map(|(b, _)| b)
            .unwrap_or(text.len());
        re.find(&text[..upto]).map(|m| Match {
            pos: Pos {
                line: start.line,
                col: text[..m.start()].chars().count(),
            },
            len: m.as_str().chars().count(),
        })
    }

    /// Replace one found match with a literal string; cursor lands after
    /// the replacement.
    pub fn replace_match(&mut self, m: Match, replacement: &str) {
        let at = self.char_idx(m.pos);
        self.splice(at, m.len, replacement, Kind::Other);
    }

    /// Position just past a match — where the next search starts.
    pub fn after_match(&self, m: Match) -> Pos {
        let idx = self.char_idx(m.pos) + m.len.max(1);
        self.pos_at(idx.min(self.rope.len_chars()))
    }

    #[cfg(feature = "syntax")]
    pub(crate) fn line_with_nl(&self, idx: usize) -> String {
        self.rope.line(idx).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed(text: &str) -> Editor {
        Editor::with_rope(Rope::from_str(text), PathBuf::from("/t.txt"), false)
    }

    #[test]
    fn typing_and_undo_redo_roundtrip() {
        let mut e = ed("");
        for c in "hello".chars() {
            e.insert(&c.to_string());
        }
        assert_eq!(e.text(), "hello");
        assert!(e.modified());
        // one coalesced group: single undo removes the whole burst
        assert!(e.undo());
        assert_eq!(e.text(), "");
        assert!(!e.modified());
        assert!(e.redo());
        assert_eq!(e.text(), "hello");
        assert_eq!(e.cursor, Pos { line: 0, col: 5 });
    }

    #[test]
    fn newline_autoindents() {
        let mut e = ed("    fn x()");
        e.move_end(false);
        e.newline();
        assert_eq!(e.text(), "    fn x()\n    ");
        assert_eq!(e.cursor, Pos { line: 1, col: 4 });
    }

    #[test]
    fn backspace_coalesces_and_joins_lines() {
        let mut e = ed("ab\ncd");
        e.goto(Pos { line: 1, col: 0 }, false);
        e.backspace(); // join
        assert_eq!(e.text(), "abcd");
        e.move_end(false);
        e.backspace();
        e.backspace(); // coalesced pair
        assert_eq!(e.text(), "ab");
        e.undo();
        assert_eq!(e.text(), "abcd");
        e.undo();
        assert_eq!(e.text(), "ab\ncd");
    }

    #[test]
    fn selection_cut_paste() {
        let mut e = ed("one two three");
        e.goto(Pos { line: 0, col: 4 }, false);
        e.goto(Pos { line: 0, col: 7 }, true); // select "two"
        assert_eq!(e.selected_text().as_deref(), Some("two"));
        e.cut();
        assert_eq!(e.text(), "one  three");
        e.move_end(false);
        e.paste();
        assert_eq!(e.text(), "one  threetwo");
        // typing over a selection replaces it in one undo step
        e.goto(Pos { line: 0, col: 0 }, false);
        e.goto(Pos { line: 0, col: 3 }, true);
        e.insert("ONE");
        assert_eq!(e.text(), "ONE  threetwo");
        e.undo();
        assert_eq!(e.text(), "one  threetwo");
    }

    #[test]
    fn sticky_mark_survives_plain_movement() {
        let mut e = ed("abcdef");
        e.toggle_mark();
        e.move_right(false);
        e.move_right(false);
        assert_eq!(e.selected_text().as_deref(), Some("ab"));
        e.toggle_mark();
        assert!(!e.has_selection());
    }

    #[test]
    fn search_wraps_and_is_smartcase() {
        let e = ed("alpha\nBeta\ngamma\nbeta tail");
        let re = Editor::compile("beta").unwrap();
        let m = e.find_from(Pos { line: 2, col: 0 }, &re).unwrap();
        assert_eq!(m.pos, Pos { line: 3, col: 0 });
        // wraps to the Beta above the start
        let m = e.find_from(e.after_match(m), &re).unwrap();
        assert_eq!(m.pos, Pos { line: 1, col: 0 });
        // uppercase in the pattern turns case sensitivity on
        let re = Editor::compile("Beta").unwrap();
        let m = e.find_from(Pos { line: 2, col: 0 }, &re).unwrap();
        assert_eq!(m.pos, Pos { line: 1, col: 0 });
    }

    #[test]
    fn replace_match_moves_past_replacement() {
        let mut e = ed("aaa");
        let re = Editor::compile("a").unwrap();
        let mut from = Pos { line: 0, col: 0 };
        let mut n = 0;
        while n < 10 {
            let Some(m) = e.find_from(from, &re) else {
                break;
            };
            // stop once we wrapped past the end
            if m.pos < from {
                break;
            }
            e.replace_match(m, "bb");
            from = e.cursor;
            n += 1;
        }
        assert_eq!(e.text(), "bbbbbb");
        assert_eq!(n, 3);
    }

    #[test]
    fn delete_line_variants() {
        let mut e = ed("one\ntwo\nthree");
        e.goto(Pos { line: 1, col: 2 }, false);
        e.delete_selection_or_line();
        assert_eq!(e.text(), "one\nthree");
        e.undo();
        assert_eq!(e.text(), "one\ntwo\nthree");
    }

    #[test]
    fn crlf_preserved_on_save() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dos.txt");
        std::fs::write(&path, b"one\r\ntwo\r\n").unwrap();
        let mut e = Editor::open(&path).unwrap();
        assert_eq!(e.text(), "one\ntwo\n");
        e.move_bottom(false);
        e.insert("three");
        e.save().unwrap();
        assert!(!e.modified());
        assert_eq!(std::fs::read(&path).unwrap(), b"one\r\ntwo\r\nthree");
    }

    #[test]
    fn save_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.sh");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut e = Editor::open(&path).unwrap();
        e.move_bottom(false);
        e.insert("echo hi\n");
        e.save().unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn binary_files_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bin");
        std::fs::write(&path, b"ab\0cd").unwrap();
        assert!(Editor::open(&path).is_err());
    }

    #[test]
    fn modified_tracking_survives_undo_past_save() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("m.txt");
        std::fs::write(&path, "base").unwrap();
        let mut e = Editor::open(&path).unwrap();
        e.move_bottom(false);
        e.insert("1");
        e.save().unwrap();
        assert!(!e.modified());
        e.undo();
        assert!(e.modified()); // differs from what is on disk
        e.redo();
        assert!(!e.modified());
        e.undo();
        e.insert("2"); // new branch: same stack depth, different content
        assert!(e.modified());
    }

    #[test]
    fn word_movement() {
        let mut e = ed("foo bar_baz  qux");
        e.move_word(true, false);
        assert_eq!(e.cursor.col, 4);
        e.move_word(true, false);
        assert_eq!(e.cursor.col, 13);
        e.move_word(false, false);
        assert_eq!(e.cursor.col, 4);
    }

    #[test]
    fn vertical_movement_keeps_desired_column() {
        let mut e = ed("longer line\nab\nlonger again");
        e.goto(Pos { line: 0, col: 8 }, false);
        e.move_vert(1, false);
        assert_eq!(e.cursor, Pos { line: 1, col: 2 });
        e.move_vert(1, false);
        assert_eq!(e.cursor, Pos { line: 2, col: 8 });
    }

    #[test]
    fn select_all_and_line_helpers() {
        let mut e = ed("ab\ncd\n");
        e.select_all();
        assert_eq!(e.selected_text().as_deref(), Some("ab\ncd\n"));
        assert_eq!(e.line_count(), 3);
        assert_eq!(e.line(1), "cd");
        assert_eq!(e.line_len(1), 2);
        assert_eq!(e.sel_on_line(1), Some((0, 3))); // includes the newline
    }
}
