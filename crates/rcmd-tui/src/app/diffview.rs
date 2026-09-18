//! Two files side by side, mc's Compare files and its `mcdiff`: the
//! rows are the two files paired up by the diff, a row missing one side
//! is a line only one of them has, and a changed row shows which words
//! changed. Whitespace, case and blank lines can be told not to count;
//! a hunk can be taken from one side into the other and the result
//! saved. The diff itself runs on a thread of its own, so a big pair of
//! files never holds up a keypress.

use super::*;
use rcmd_core::diff::{self, Options, Row};
use std::sync::mpsc::Receiver;

/// Past this size a file is not read into a diff: the rows for two of
/// them are more than anyone reads side by side.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// One of the two files.
pub struct DiffSide {
    pub title: String,
    pub lines: Vec<String>,
    /// Where it came from, and so where F2 writes it: `None` for a side
    /// that is no file - the HEAD version of one.
    source: Option<(Arc<dyn FsProvider>, PathBuf)>,
    charset: Option<&'static rcmd_core::charset::Encoding>,
    /// CRLF line ends, and a newline after the last line: what a save
    /// writes back, so a merge changes only the lines it merged.
    crlf: bool,
    trailing_newline: bool,
    /// Changed by a merge and not saved.
    pub modified: bool,
}

impl DiffSide {
    fn new(
        title: String,
        bytes: &[u8],
        charset: Option<&'static rcmd_core::charset::Encoding>,
    ) -> Self {
        let text = rcmd_core::charset::decode(bytes, charset);
        DiffSide {
            title,
            lines: text.lines().map(str::to_string).collect(),
            source: None,
            charset,
            crlf: text.contains("\r\n"),
            trailing_newline: text.ends_with('\n'),
            modified: false,
        }
    }

    fn save(&mut self) -> std::io::Result<()> {
        let Some((fs, path)) = &self.source else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "this side is not a file",
            ));
        };
        let writer = fs.writer().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::ReadOnlyFilesystem, "read-only")
        })?;
        let eol = if self.crlf { "\r\n" } else { "\n" };
        let mut text = self.lines.join(eol);
        if self.trailing_newline && !self.lines.is_empty() {
            text.push_str(eol);
        }
        let mut out = writer.open_write(path)?;
        std::io::Write::write_all(&mut out, &rcmd_core::charset::encode(&text, self.charset))?;
        std::io::Write::flush(&mut out)?;
        self.modified = false;
        Ok(())
    }
}

/// One file to put on a side of a diff.
pub struct DiffSource {
    pub fs: Arc<dyn FsProvider>,
    pub path: PathBuf,
    pub title: String,
    pub charset: Option<&'static rcmd_core::charset::Encoding>,
    pub size: u64,
}

/// A line typed at the bottom of the diff.
pub enum DiffPrompt {
    Search(TextField),
    /// A line number of the left file.
    Goto(String, usize),
}

pub struct DiffView {
    pub left: DiffSide,
    pub right: DiffSide,
    pub rows: Vec<Row>,
    /// Where the changes are, for "next difference".
    pub blocks: Vec<(usize, usize)>,
    /// The block the last step landed on: what F5 merges.
    pub current: Option<usize>,
    pub opts: Options,
    pub top: usize,
    /// Horizontal scroll, in characters, shared by both sides.
    pub col: usize,
    /// Rows on screen; updated on every draw, drives paging.
    pub height: usize,
    pub note: Option<String>,
    pub prompt: Option<DiffPrompt>,
    search: String,
    /// The rows being worked out on their thread.
    pending: Option<Receiver<Vec<Row>>>,
    /// A quit with unsaved merges was asked once: the second goes.
    quit_armed: bool,
}

impl DiffView {
    fn new(left: DiffSide, right: DiffSide) -> Self {
        let mut view = DiffView {
            left,
            right,
            rows: Vec::new(),
            blocks: Vec::new(),
            current: None,
            opts: Options::default(),
            top: 0,
            col: 0,
            height: 1,
            note: Some(" comparing… ".into()),
            prompt: None,
            search: String::new(),
            pending: None,
            quit_armed: false,
        };
        view.recompute();
        view
    }

    pub fn line(&self, row: usize, right: bool) -> Option<&str> {
        let row = self.rows.get(row)?;
        let (at, side) = match right {
            true => (row.right?, &self.right),
            false => (row.left?, &self.left),
        };
        side.lines.get(at).map(String::as_str)
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// What the options leave out, for the title: `-w -i -B` as diff
    /// spells them.
    pub fn flags(&self) -> String {
        let mut out = Vec::new();
        if self.opts.ignore_space {
            out.push("-w");
        }
        if self.opts.ignore_case {
            out.push("-i");
        }
        if self.opts.ignore_blank {
            out.push("-B");
        }
        out.join(" ")
    }

    /// Work the rows out again, on a thread: after a merge, or with
    /// the options changed.
    fn recompute(&mut self) {
        let (left, right, opts) = (self.left.lines.clone(), self.right.lines.clone(), self.opts);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(diff::rows_with(&left, &right, opts));
        });
        self.pending = Some(rx);
    }

    /// The rows, if they have arrived. True = they just did.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.pending else {
            return false;
        };
        let Ok(rows) = rx.try_recv() else {
            return false;
        };
        self.pending = None;
        let first = self.rows.is_empty();
        self.blocks = diff::blocks(&rows);
        self.rows = rows;
        self.current = self.current.filter(|&at| at < self.blocks.len());
        self.top = self.top.min(self.rows.len().saturating_sub(1));
        self.note = match self.blocks.len() {
            0 => Some(" the files are identical ".into()),
            n => Some(format!(" {n} difference(s) - n and p walk them ")),
        };
        // open on the first difference rather than on whatever the two
        // files happen to agree about at the top
        if first && let Some(&(start, _)) = self.blocks.first() {
            self.top = start.saturating_sub(2);
            self.current = Some(0);
        }
        true
    }

    fn scroll(&mut self, delta: isize) {
        let last = self.rows.len().saturating_sub(1) as isize;
        self.top = (self.top as isize + delta).clamp(0, last.max(0)) as usize;
    }

    /// The next (or previous) run of changed rows, put at the top with
    /// a couple of lines of context above it.
    fn jump(&mut self, forward: bool) {
        let here = self.top;
        let found = match forward {
            true => self.blocks.iter().position(|(start, _)| *start > here + 2),
            false => self.blocks.iter().rposition(|(start, _)| *start + 2 < here),
        };
        match found {
            Some(at) => {
                self.top = self.blocks[at].0.saturating_sub(2);
                self.current = Some(at);
            }
            None if self.blocks.is_empty() => self.note = Some(" the files are identical ".into()),
            None => self.note = Some(" no more differences that way ".into()),
        }
    }

    /// The block a merge acts on: the one stepped to, while it is on
    /// screen, and otherwise the first one that is.
    fn target_block(&self) -> Option<usize> {
        let visible = |&(start, end): &(usize, usize)| {
            end > self.top && start < self.top + self.height.max(1)
        };
        self.current
            .filter(|&at| self.blocks.get(at).is_some_and(visible))
            .or_else(|| self.blocks.iter().position(visible))
    }

    /// Take one side's version of the target block into the other.
    fn merge(&mut self, into_right: bool) {
        if self.pending.is_some() {
            return;
        }
        let Some(at) = self.target_block() else {
            self.note = Some(" no difference on screen to take ".into());
            return;
        };
        let (l, r) = diff::block_lines(&self.rows, self.blocks[at]);
        let (from, to, taken, into) = match into_right {
            true => (&self.left, &mut self.right, l, r),
            false => (&self.right, &mut self.left, r, l),
        };
        if to.source.is_none() {
            self.note = Some(" that side is not a file ".into());
            return;
        }
        let lines: Vec<String> = from.lines[taken].to_vec();
        to.lines.splice(into, lines);
        to.modified = true;
        self.current = Some(at);
        self.recompute();
    }

    fn search_next(&mut self) {
        let needle = self.search.to_lowercase();
        if needle.is_empty() {
            return;
        }
        let hit = |at: usize| {
            [false, true].into_iter().any(|right| {
                self.line(at, right)
                    .is_some_and(|text| text.to_lowercase().contains(&needle))
            })
        };
        let n = self.rows.len();
        // from the row after the one at the top, round past the end
        match (1..=n)
            .map(|step| (self.top + step) % n.max(1))
            .find(|&at| hit(at))
        {
            Some(at) => {
                if at <= self.top {
                    self.note = Some(" search wrapped round ".into());
                }
                self.top = at;
            }
            None => self.note = Some(format!(" \"{}\" is not there ", self.search)),
        }
    }

    fn goto(&mut self, line: usize) {
        let want = line.saturating_sub(1);
        match self
            .rows
            .iter()
            .position(|r| r.left.is_some_and(|l| l >= want))
        {
            Some(at) => self.top = at,
            None => self.top = self.rows.len().saturating_sub(1),
        }
    }
}

impl App {
    /// Compare files: the cursor files of the two panels, side by side.
    pub(super) fn open_diff(&mut self) {
        let mut sides = Vec::new();
        for side in [0, 1] {
            let panel = &self.panels[side];
            let Some(entry) = panel.selected().filter(|e| !e.is_parent()) else {
                self.status = Some(" both panels need a file under the cursor ".into());
                return;
            };
            if entry.is_dir() {
                self.status = Some(" compare files: that is a directory ".into());
                return;
            }
            sides.push(DiffSource {
                fs: panel.fs.clone(),
                path: panel.cwd.join(&entry.name),
                title: panel.display_path() + "/" + &panel.name_of(entry),
                charset: panel.charset,
                size: entry.size,
            });
        }
        let right = sides.pop().expect("two sides");
        let left = sides.pop().expect("two sides");
        self.open_diff_pair(left, right);
    }

    /// Two files side by side, wherever each is. False = they were not
    /// opened, and the status line says why.
    pub(super) fn open_diff_pair(&mut self, left: DiffSource, right: DiffSource) -> bool {
        let mut read = Vec::new();
        for side in [&left, &right] {
            match read_side(&side.fs, &side.path, side.size) {
                Ok(bytes) => read.push(bytes),
                Err(err) => {
                    self.status = Some(format!(" compare files: {err} "));
                    return false;
                }
            }
        }
        let (rb, lb) = (read.pop().expect("two"), read.pop().expect("two"));
        if diff::is_binary(&lb) || diff::is_binary(&rb) {
            self.status = Some(match lb == rb {
                true => " binary files, and identical ".into(),
                false => " binary files, and they differ - F3 shows them in hex ".into(),
            });
            return false;
        }
        let mut l = DiffSide::new(left.title, &lb, left.charset);
        l.source = Some((left.fs, left.path));
        let mut r = DiffSide::new(right.title, &rb, right.charset);
        r.source = Some((right.fs, right.path));
        self.open_screen(Screen::Diff(Box::new(DiffView::new(l, r))));
        true
    }

    /// The cursor file against what the last commit has of it.
    pub(super) fn open_diff_head(&mut self) {
        let panel = &self.panels[self.active];
        if !panel.is_local() {
            self.status = Some(" diff against HEAD needs a local file ".into());
            return;
        }
        let Some(entry) = panel.selected().filter(|e| !e.is_parent() && !e.is_dir()) else {
            self.status = Some(" diff against HEAD: a file under the cursor ".into());
            return;
        };
        let path = panel.cwd.join(&entry.name);
        let Some((head, rel)) = crate::git::head_blob(&path) else {
            self.status = Some(" not in the last commit of a git repository ".into());
            return;
        };
        let work = match read_side(&panel.fs, &path, entry.size) {
            Ok(bytes) => bytes,
            Err(err) => {
                self.status = Some(format!(" diff against HEAD: {err} "));
                return;
            }
        };
        if diff::is_binary(&head) || diff::is_binary(&work) {
            self.status = Some(match head == work {
                true => " binary, and as committed ".into(),
                false => " binary, and changed since the commit ".into(),
            });
            return;
        }
        let charset = panel.charset;
        let left = DiffSide::new(format!("HEAD:{rel}"), &head, charset);
        let mut right = DiffSide::new(crate::ui::abbrev_home(&path), &work, charset);
        right.source = Some((panel.fs.clone(), path));
        self.open_screen(Screen::Diff(Box::new(DiffView::new(left, right))));
    }

    /// Rows arriving for the diff on screen.
    pub(super) fn poll_diff(&mut self) -> bool {
        self.diff_mut().is_some_and(DiffView::poll)
    }

    pub(super) fn on_diff_key(&mut self, key: KeyEvent) {
        let Some(d) = self.diff_mut() else { return };
        if let Some(prompt) = d.prompt.as_mut() {
            match (prompt, key.code) {
                (_, KeyCode::Esc) => d.prompt = None,
                (DiffPrompt::Search(field), KeyCode::Enter) => {
                    d.search = field.value.clone();
                    let field = std::mem::replace(field, TextField::new(""));
                    d.prompt = None;
                    d.search_next();
                    self.remember(&field);
                }
                (DiffPrompt::Search(field), _) => {
                    field.key(key);
                }
                (DiffPrompt::Goto(value, _), KeyCode::Enter) => {
                    let line = value.trim().parse::<usize>().ok();
                    d.prompt = None;
                    match line {
                        Some(line) => d.goto(line),
                        None => d.note = Some(" a line number of the left file ".into()),
                    }
                }
                (DiffPrompt::Goto(value, cursor), code) => {
                    edit_line(value, cursor, code, key.modifiers);
                }
            }
            return;
        }
        let keep_quit = matches!(
            key.code,
            KeyCode::Esc | KeyCode::F(10) | KeyCode::F(3) | KeyCode::Char('q')
        );
        if !keep_quit {
            d.quit_armed = false;
        }
        d.note = None;
        let page = d.height.saturating_sub(1).max(1) as isize;
        let flip = |d: &mut DiffView, what: fn(&mut Options) -> &mut bool| {
            let flag = what(&mut d.opts);
            *flag = !*flag;
            d.recompute();
        };
        match key.code {
            KeyCode::Esc | KeyCode::F(10) | KeyCode::F(3) | KeyCode::Char('q') => {
                let unsaved = d.left.modified || d.right.modified;
                if unsaved && !d.quit_armed {
                    d.quit_armed = true;
                    d.note = Some(" merges not saved - F2 saves, q again drops them ".into());
                    return;
                }
                self.close_screen()
            }
            KeyCode::Up => d.scroll(-1),
            KeyCode::Down => d.scroll(1),
            KeyCode::PageUp => d.scroll(-page),
            KeyCode::PageDown => d.scroll(page),
            KeyCode::Home => d.top = 0,
            KeyCode::End => d.top = d.rows.len().saturating_sub(1),
            KeyCode::Left => d.col = d.col.saturating_sub(8),
            KeyCode::Right => d.col += 8,
            KeyCode::Char('n') | KeyCode::Tab | KeyCode::F(8) => d.jump(true),
            KeyCode::Char('p') | KeyCode::BackTab => d.jump(false),
            KeyCode::F(7) | KeyCode::Char('/') => {
                let field = TextField::new(d.search.clone()).with_history("diff-search");
                d.prompt = Some(DiffPrompt::Search(field));
            }
            KeyCode::F(17) => d.search_next(),
            KeyCode::Char(':' | 'g') => d.prompt = Some(DiffPrompt::Goto(String::new(), 0)),
            KeyCode::Char('w') => flip(d, |o| &mut o.ignore_space),
            KeyCode::Char('i') => flip(d, |o| &mut o.ignore_case),
            KeyCode::Char('b') => flip(d, |o| &mut o.ignore_blank),
            KeyCode::F(5) | KeyCode::Char('>') => d.merge(true),
            KeyCode::F(15) | KeyCode::Char('<') => d.merge(false),
            KeyCode::F(2) => {
                let mut saved = Vec::new();
                for side in [&mut d.left, &mut d.right] {
                    if !side.modified {
                        continue;
                    }
                    match side.save() {
                        Ok(()) => saved.push(side.title.clone()),
                        Err(err) => {
                            d.note = Some(format!(" {}: {err} ", side.title));
                            return;
                        }
                    }
                }
                d.note = Some(match saved.len() {
                    0 => " nothing to save ".into(),
                    _ => format!(" saved {} ", saved.join(", ")),
                });
                self.reload_after_diff_save();
            }
            _ => {}
        }
    }

    fn reload_after_diff_save(&mut self) {
        for panel in &mut self.panels {
            let _ = panel.refresh();
        }
    }
}

/// A side's bytes, if the file is small enough to diff.
fn read_side(fs: &Arc<dyn FsProvider>, path: &Path, size: u64) -> std::io::Result<Vec<u8>> {
    if size > MAX_BYTES {
        return Err(std::io::Error::other(format!(
            "{} is too big to diff (over {} MB)",
            path.display(),
            MAX_BYTES / (1024 * 1024)
        )));
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut fs.open_read(path)?, &mut bytes)?;
    Ok(bytes)
}
