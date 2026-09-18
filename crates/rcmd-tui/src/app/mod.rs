use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

pub use crate::field::TextField;
use crate::field::{byte_index, edit_line};
use anyhow::{Context, Result};
use notify::Watcher as _;
use ratatui::DefaultTerminal;
use ratatui::crossterm::cursor;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::TableState;
use rcmd_core::entry;
use rcmd_core::find::{self, FindEvent, FindHandle};
use rcmd_core::fish;
use rcmd_core::fsops::{self, FileFacts, JobEvent, JobHandle, Rename, Reply, TransferOpts};
use rcmd_core::ftp::{self, FtpUrl};
use rcmd_core::mask::{self, Mask};
use rcmd_core::panel::{ListMode, LoadKind, Panel, SortKey};
use rcmd_core::remote::{self, ConnectEvent, ConnectHandle, ConnectReply};
use rcmd_core::sftp::{self, SftpUrl};
use rcmd_core::tree::Tree;

use crate::format::{self, Field, Format, Item};
use rcmd_core::vfs::{FsProvider, LocalFs, RemoteFs};
use rcmd_core::view::{FileView, Search, SearchKind};

use crate::config::{Config, HotEntry, UserCommand};
use crate::keymap::Keymap;
use crate::subshell::Subshell;
use crate::{config, git, keymap, state, ui};

/// How long an idle rcmd goes without repainting: long enough that the
/// terminal is left alone, short enough that a state change nobody
/// flagged still turns up.
const IDLE_FRAME: Duration = Duration::from_secs(2);

/// Fallback for `esc_timeout_ms`: how long a lone Esc waits for its
/// follow-up key before acting as a plain Escape (MC's meta prefix).
/// Short, so "Esc clears the command line" feels immediate; raise it in
/// the config when typing Esc-1..0 for F1..F10 by hand.
pub const ESC_TIMEOUT_MS: u64 = 250;

/// Command lines kept across sessions (in the state file).
const HISTORY_CAP: usize = 100;

/// The material mc's macros are made of: the cursor file, the marked
/// files and the directory, for this panel and the other one, plus
/// whatever is in the clipboard file. All shell-quoted already - a
/// filename is not a word.
pub struct Macros {
    pub here: (String, String, String),
    pub there: (String, String, String),
    pub clip: String,
}

/// Expand a command template against `m`. Returns what it got to and
/// which side's marks the template spent (`%u` / `%U`), in this-panel /
/// other-panel order.
pub fn expand_template(template: &str, m: &Macros) -> (Expanded, [bool; 2]) {
    let (file, tagged, dir) = &m.here;
    let (other_file, other_tagged, other_dir) = &m.there;
    // mc's "selected": the marked files if there are any, and the one
    // under the cursor if there are not
    let selected = |tagged: &String, file: &String| match tagged.is_empty() {
        true => file.clone(),
        false => tagged.clone(),
    };
    let mut untag = [false, false];
    let mut out = String::new();
    let mut rest = template.char_indices();
    while let Some((_, c)) = rest.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match rest.next() {
            Some((_, 'f')) => out.push_str(file),
            Some((_, 'F')) => out.push_str(other_file),
            Some((_, 'd')) => out.push_str(dir),
            Some((_, 'D')) => out.push_str(other_dir),
            Some((_, 't')) => out.push_str(tagged),
            Some((_, 'T')) => out.push_str(other_tagged),
            Some((_, 'u')) => {
                out.push_str(tagged);
                untag[0] = true;
            }
            Some((_, 'U')) => {
                out.push_str(other_tagged);
                untag[1] = true;
            }
            Some((_, 's')) => out.push_str(&selected(tagged, file)),
            Some((_, 'S')) => out.push_str(&selected(other_tagged, other_file)),
            Some((_, 'q')) => out.push_str(&m.clip),
            Some((_, '%')) => out.push('%'),
            // %{question} asks, and the answer goes in unquoted - it is
            // how mc passes options, not filenames
            Some((at, '{')) => {
                let tail = &template[at + 1..];
                if let Some(end) = tail.find('}') {
                    return (
                        Expanded::Ask {
                            question: tail[..end].to_string(),
                            before: out,
                            rest: tail[end + 1..].to_string(),
                        },
                        untag,
                    );
                }
                out.push_str("%{");
            }
            Some((_, other)) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    (Expanded::Done(out), untag)
}

impl InputAction {
    /// The name of the history ring this field walks. Fields that ask
    /// the same kind of question share one - a destination is a
    /// destination whether F5 or F6 asked for it. `None` = a question
    /// too specific to be worth remembering (a `%{...}` prompt asks
    /// something different every time).
    pub fn history(&self) -> Option<&'static str> {
        Some(match self {
            InputAction::CopyTo { .. } | InputAction::MoveTo { .. } => "destination",
            InputAction::Mkdir => "mkdir",
            InputAction::Apply => "apply",
            InputAction::Checksum { .. } => "destination",
            InputAction::Pack { .. } => "pack",
            InputAction::SftpConnect => "connect",
            InputAction::EditNew => "edit",
            InputAction::QuickCd => "cd",
            InputAction::FilteredView => "filter",
            InputAction::Chown { .. } => "chown",
            InputAction::HotlistLabel { .. } => "label",
            InputAction::MacroPrompt { .. } => return None,
        })
    }
}

/// What expanding a command template produced: the finished line, or a
/// stop at a `%{question}` whose answer the rest of the line waits on.
pub enum Expanded {
    Done(String),
    Ask {
        question: String,
        /// Everything before the question, already expanded.
        before: String,
        /// Everything after it, still a template.
        rest: String,
    },
}

pub enum InputAction {
    /// A `%{question}` from a command template: the answer joins
    /// `before` to what expanding `rest` produces.
    MacroPrompt {
        before: String,
        rest: String,
        quiet: bool,
    },
    CopyTo {
        sources: Vec<PathBuf>,
    },
    MoveTo {
        sources: Vec<PathBuf>,
    },
    Mkdir,
    /// C-g: the value is a command run once per marked file.
    Apply,
    /// The value is the checksum file to write.
    Checksum {
        paths: Vec<PathBuf>,
    },
    /// M-F5: the value is the archive to pack the marked files into.
    Pack {
        sources: Vec<PathBuf>,
    },
    /// F9 → Command → Remote link: the value is an sftp:// or ftp:// URL.
    SftpConnect,
    /// S-F4: the value is the file to edit (created on first save).
    EditNew,
    /// M-c: the value is a cd target (path or sftp:// URL).
    QuickCd,
    /// M-!: the value is a command whose output the viewer shows.
    FilteredView,
    /// The hotlist asking for a label: for a new entry (`path` set), a
    /// new group (`path` empty and `index` None) or a rename (`index`).
    HotlistLabel {
        group: Vec<usize>,
        index: Option<usize>,
        path: String,
    },
    /// C-x o: the value is `user[:group]` for these paths.
    Chown {
        paths: Vec<PathBuf>,
    },
}

/// An SFTP connection attempt on its worker thread; `ask` is the
/// interactive question currently shown (host key / password).
pub struct ConnectState {
    handle: ConnectHandle,
    panel: usize,
    pub ask: Option<ConnectAsk>,
}

pub enum ConnectAsk {
    HostKey {
        fingerprint: String,
        yes: bool,
    },
    /// Password / key passphrase / keyboard-interactive challenge;
    /// `echo` shows the input unmasked (server's wish per prompt).
    Password {
        prompt: String,
        value: String,
        cursor: usize,
        echo: bool,
    },
}

/// A remote file being edited via a local scratch copy (F4 on an SFTP
/// panel): uploaded back if the editor changed it.
pub struct RemoteEdit {
    fs: Arc<dyn FsProvider>,
    remote_path: PathBuf,
    temp: PathBuf,
    mtime_before: Option<std::time::SystemTime>,
}

/// Alt+F7 find dialog: mc's Find File, and the questions it never
/// asked - how big, how old, how deep.
#[derive(Clone)]
pub struct FindDialog {
    /// Where the walk starts; the panel's directory unless changed.
    pub start: TextField,
    pub name: TextField,
    pub content: TextField,
    /// mc's "Enable ignore directories": names or paths, `:` between.
    pub ignore: TextField,
    /// As the select dialog takes them: `>1M`, `30d`.
    pub size: TextField,
    pub newer: TextField,
    /// How many levels down; empty is all of them.
    pub depth: TextField,
    /// The filename is a glob; off = a regular expression.
    pub shell: bool,
    pub name_case: bool,
    pub case_sensitive: bool,
    pub whole_words: bool,
    /// The content is a regular expression, matched line by line.
    pub regex: bool,
    pub all_charsets: bool,
    /// One result per file; off, every matching line is one.
    pub first_hit: bool,
    /// Off = the start directory alone.
    pub recursive: bool,
    pub skip_hidden: bool,
    pub follow_links: bool,
    /// Skip gitignored trees when searching inside a work tree.
    pub skip_ignored: bool,
    /// Focused row: the fields, then the switches, then [`FIND_ROWS`]
    /// for the button row.
    pub row: usize,
    pub ok: bool,
}

/// The text fields, in row order: label, and the hint it carries.
pub const FIND_FIELD_LABELS: &[&str] = &[
    "Start at:",
    "File name:",
    "Content:",
    "Ignore dirs:",
    "Size:",
    "Newer than:",
    "Max depth:",
];
pub const FIND_FIELDS: usize = FIND_FIELD_LABELS.len();
/// The switches, in row order after the fields.
pub const FIND_SWITCHES: &[&str] = &[
    "Shell patterns",
    "Case sensitive name",
    "Case sensitive content",
    "Whole words",
    "Regular expression",
    "All charsets",
    "First hit only",
    "Find recursively",
    "Skip hidden",
    "Follow symlinks",
    "Skip gitignored",
];
/// Rows before the button row.
pub const FIND_ROWS: usize = FIND_FIELDS + FIND_SWITCHES.len();

impl FindDialog {
    fn switch_mut(&mut self, index: usize) -> Option<&mut bool> {
        Some(match index {
            0 => &mut self.shell,
            1 => &mut self.name_case,
            2 => &mut self.case_sensitive,
            3 => &mut self.whole_words,
            4 => &mut self.regex,
            5 => &mut self.all_charsets,
            6 => &mut self.first_hit,
            7 => &mut self.recursive,
            8 => &mut self.skip_hidden,
            9 => &mut self.follow_links,
            10 => &mut self.skip_ignored,
            _ => return None,
        })
    }

    pub fn switch(&self, index: usize) -> bool {
        [
            self.shell,
            self.name_case,
            self.case_sensitive,
            self.whole_words,
            self.regex,
            self.all_charsets,
            self.first_hit,
            self.recursive,
            self.skip_hidden,
            self.follow_links,
            self.skip_ignored,
        ]
        .get(index)
        .copied()
        .unwrap_or(false)
    }

    fn toggle(&mut self) {
        if let Some(on) = self
            .row
            .checked_sub(FIND_FIELDS)
            .and_then(|i| self.switch_mut(i))
        {
            *on = !*on;
        }
    }

    fn step(&mut self, step: isize) {
        let last = FIND_ROWS as isize; // the button row
        let mut row = self.row as isize + step;
        if row < 0 {
            row = last;
        } else if row > last {
            row = 0;
        }
        self.row = row as usize;
    }

    /// The field in row `i`.
    pub fn field_at(&self, i: usize) -> Option<&TextField> {
        [
            &self.start,
            &self.name,
            &self.content,
            &self.ignore,
            &self.size,
            &self.newer,
            &self.depth,
        ]
        .get(i)
        .copied()
    }

    /// The field the cursor is in, if it is in one.
    pub fn field(&mut self) -> Option<&mut TextField> {
        Some(match self.row {
            0 => &mut self.start,
            1 => &mut self.name,
            2 => &mut self.content,
            3 => &mut self.ignore,
            4 => &mut self.size,
            5 => &mut self.newer,
            6 => &mut self.depth,
            _ => return None,
        })
    }

    /// The answers worth opening the next find on.
    pub fn memory(&self) -> crate::state::FindMemory {
        crate::state::FindMemory {
            name: self.name.value.clone(),
            content: self.content.value.clone(),
            ignore: self.ignore.value.clone(),
            size: self.size.value.clone(),
            newer: self.newer.value.clone(),
            depth: self.depth.value.clone(),
            shell: self.shell,
            name_case: self.name_case,
            case_sensitive: self.case_sensitive,
            whole_words: self.whole_words,
            regex: self.regex,
            all_charsets: self.all_charsets,
            first_hit: self.first_hit,
            recursive: self.recursive,
            skip_hidden: self.skip_hidden,
            follow_links: self.follow_links,
            skip_ignored: self.skip_ignored,
        }
    }

    /// A dialog opening on `last`, the walk starting at `start`.
    pub fn from_memory(start: String, last: crate::state::FindMemory) -> FindDialog {
        FindDialog {
            start: TextField::new(start).with_history("find-start"),
            name: TextField::new(last.name).with_history("find-name"),
            content: TextField::new(last.content).with_history("find-content"),
            ignore: TextField::new(last.ignore).with_history("find-ignore"),
            size: TextField::new(last.size).with_history("size"),
            newer: TextField::new(last.newer).with_history("newer"),
            depth: TextField::new(last.depth),
            shell: last.shell,
            name_case: last.name_case,
            case_sensitive: last.case_sensitive,
            whole_words: last.whole_words,
            regex: last.regex,
            all_charsets: last.all_charsets,
            first_hit: last.first_hit,
            recursive: last.recursive,
            skip_hidden: last.skip_hidden,
            follow_links: last.follow_links,
            skip_ignored: last.skip_ignored,
            row: 1,
            ok: true,
        }
    }
}

/// A running find, streaming matches into the results window - or into
/// `panel`'s panelized listing, where the setting says so.
pub struct FindState {
    pub handle: FindHandle,
    pub panel: usize,
    pub count: usize,
    /// Matches go to [`Dialog::FindResults`] rather than to the panel.
    pub window: bool,
}

/// MC's find results window: the matches as they arrive, and the six
/// things to do with the one under the cursor.
pub struct FindResults {
    /// What was searched for, for the title.
    pub label: String,
    pub root: PathBuf,
    /// In the order they were found.
    pub rows: Vec<FindRow>,
    pub selected: usize,
    pub top: usize,
    /// Some(matches, scanned) once the walk has finished.
    pub done: Option<(u64, u64)>,
    pub button: usize,
    /// The dialog that started it, so "Again" can ask it again.
    pub query: Box<FindDialog>,
}

/// mc's three ways of comparing two directories, in its order.
pub const COMPARE_MODES: &[(&str, rcmd_core::compare::Mode)] = &[
    ("Quick (size and date)", rcmd_core::compare::Mode::Quick),
    ("Size only", rcmd_core::compare::Mode::SizeOnly),
    (
        "Thorough (read the files)",
        rcmd_core::compare::Mode::Thorough,
    ),
];

/// A find's results, walked from the viewer or the editor one hit at a
/// time: mc's results window as a quickfix list.
pub struct HitWalk {
    /// Every row of the window, in its order: the file and the line.
    pub rows: Vec<(PathBuf, Option<u64>)>,
    pub at: usize,
    /// The question that found them, for the search each hit seeds.
    pub query: Box<FindDialog>,
}

/// One row of the results window: a file, and - when the content was
/// searched - the line it was found on.
pub struct FindRow {
    /// Absolute.
    pub path: PathBuf,
    pub hit: Option<find::Hit>,
    /// Insert marks it for F5 / F6 / F8, as in a panel.
    pub marked: bool,
}

/// The buttons along the bottom, in mc's order.
pub const FIND_BUTTONS: &[&str] = &["Chdir", "Again", "Panelize", "View", "Edit", "Quit"];

impl FindResults {
    /// The path as the window shows it: relative to where the search
    /// started, which is what makes a long list readable.
    pub fn label_of(&self, at: usize) -> String {
        let path = &self.rows[at].path;
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// What F5, F6 and F8 act on: the marked rows' files, each once,
    /// or the file under the cursor when nothing is marked.
    pub fn targets(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        let marked = self.rows.iter().filter(|row| row.marked);
        for row in marked {
            if !out.contains(&row.path) {
                out.push(row.path.clone());
            }
        }
        if out.is_empty()
            && let Some(row) = self.rows.get(self.selected)
        {
            out.push(row.path.clone());
        }
        out
    }

    /// Move the cursor and keep it on screen. `shown` is how many rows
    /// the window has room for, which only the drawing knows.
    fn step(&mut self, delta: isize, shown: usize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        let shown = shown.max(1);
        self.top = self
            .top
            .min(self.selected)
            .max((self.selected + 1).saturating_sub(shown));
    }
}

/// A panelize command running on its own thread, its output becoming
/// panel entries as the lines arrive.
struct PanelizeJob {
    rx: std::sync::mpsc::Receiver<PanelizeEvent>,
    panel: usize,
    count: usize,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

enum PanelizeEvent {
    Line(String),
    Done(Option<String>),
}

/// A running thorough compare: the pairs the listings could not tell
/// apart, being read on a worker thread.
struct CompareState {
    handle: rcmd_core::compare::CompareHandle,
    /// How many pairs it was given, for the progress line.
    total: usize,
    done: usize,
}

/// One row of the synchronize plan: a file the comparison called a
/// difference, and which way copying it would settle that.
pub struct SyncRow {
    /// The path under both directories.
    pub rel: PathBuf,
    /// What happens to it: a copy one way, or a delete on one side.
    pub step: fsops::SyncStep,
    /// Off with Space: the row stays visible and is not run.
    pub on: bool,
    /// Why the row is here, in the words the list shows.
    pub note: &'static str,
    /// Which sides have something here, and whether each is a
    /// directory - what the arrows may choose between, and whether F3
    /// has two files to show.
    pub left: Option<bool>,
    pub right: Option<bool>,
}

impl SyncRow {
    /// The row for a difference, as a plan with no preference, or one
    /// that makes one side a mirror of the other.
    pub fn plan(d: &rcmd_core::sync::Difference, mirror: Mirror) -> SyncRow {
        use fsops::SyncStep::*;
        let (left, right) = (
            d.left.as_ref().map(|e| e.is_dir()),
            d.right.as_ref().map(|e| e.is_dir()),
        );
        let (step, on, note) = match (&d.left, &d.right, mirror) {
            _ if d.clash() => (
                ToRight,
                false,
                "a file on one side, a directory on the other",
            ),
            // both are directories, and one of them would not list
            (Some(l), Some(_), _) if l.is_dir() => (ToRight, false, "could not be read"),
            (Some(_), None, Mirror::Left) => (DeleteLeft, true, "only on the left - goes"),
            (Some(_), None, _) => (ToRight, true, "only on the left"),
            (None, Some(_), Mirror::Right) => (DeleteRight, true, "only on the right - goes"),
            (None, Some(_), _) => (ToLeft, true, "only on the right"),
            (Some(_), Some(_), Mirror::Right) => (ToRight, true, "differs"),
            (Some(_), Some(_), Mirror::Left) => (ToLeft, true, "differs"),
            // both have it and it differs: the newer one is the one to
            // keep, and a tie is left pointing right rather than
            // guessed at
            (Some(l), Some(r), Mirror::Off) => match (l.mtime, r.mtime) {
                (Some(a), Some(b)) if a > b => (ToRight, true, "newer on the left"),
                (Some(a), Some(b)) if b > a => (ToLeft, true, "newer on the right"),
                _ => (ToRight, true, "differs"),
            },
            (None, None, _) => (ToRight, false, ""),
        };
        SyncRow {
            rel: d.rel.clone(),
            step,
            on,
            note,
            left,
            right,
        }
    }

    /// Both sides have a file here.
    pub fn two_files(&self) -> bool {
        self.left == Some(false) && self.right == Some(false)
    }
}

/// Which way a plan leans: each difference on its own merits, or one
/// side made the other's copy - what is only on the copy goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mirror {
    Off,
    /// The right becomes a copy of the left.
    Right,
    /// The left becomes a copy of the right.
    Left,
}

/// F9 > Command > Synchronize: what the comparison found, as a plan
/// that can be read, flipped row by row and run.
pub struct SyncDialog {
    pub rows: Vec<SyncRow>,
    /// What the comparison found, for planning again when the mirror
    /// changes.
    pub diffs: Vec<rcmd_core::sync::Difference>,
    pub mirror: Mirror,
    pub cursor: usize,
    /// First row drawn, so a long plan scrolls.
    pub top: usize,
    pub left: String,
    pub right: String,
    /// `+` / `-` typed: a mask, and whether the rows it matches go on
    /// or off.
    pub mask: Option<(TextField, bool)>,
}

/// A running Ctrl+Space directory-size scan.
struct DuJob {
    rx: std::sync::mpsc::Receiver<(u64, u64)>,
    panel: usize,
    cwd: PathBuf,
    name: std::ffi::OsString,
}

/// Filesystem watcher: auto-reload panels on external changes, debounced.
struct WatchState {
    watcher: notify::RecommendedWatcher,
    rx: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    watched: [Option<PathBuf>; 2],
    /// When the first / most recent unprocessed event arrived.
    dirty: [Option<std::time::Instant>; 2],
    last: [Option<std::time::Instant>; 2],
}

pub struct InputDialog {
    pub title: String,
    /// The line, its cursor, and the ring of earlier answers
    /// `InputAction::history` names.
    pub field: TextField,
    pub action: InputAction,
}

impl InputDialog {
    pub fn new(title: impl Into<String>, value: impl Into<String>, action: InputAction) -> Self {
        let field = TextField::new(value);
        let field = match action.history() {
            Some(name) => field.with_history(name),
            None => field,
        };
        InputDialog {
            title: title.into(),
            field,
            action,
        }
    }

    /// Where the cursor starts, when not at the end.
    pub fn cursor(mut self, at: usize) -> Self {
        self.field = self.field.cursor(at);
        self
    }
}

/// F5/F6: MC's copy/move form - where the files go, the switches that
/// change what "copy" means, and OK / Background / Cancel.
pub struct TransferDialog {
    pub title: String,
    /// MC's source mask: which of the marked files take part, and what
    /// their wildcards capture for the destination to spend.
    pub mask: TextField,
    pub dest: TextField,
    pub is_move: bool,
    pub sources: Vec<PathBuf>,
    pub opts: TransferOpts,
    /// Focused row: 0 is the destination, then one per checkbox, then
    /// [`TRANSFER_ROWS`] for the button row.
    pub row: usize,
    /// 0 = OK, 1 = Background, 2 = Cancel.
    pub button: usize,
}

/// The field one checkbox drives, reached by reference so the same
/// function both reads and flips it.
type OptField = fn(&mut TransferOpts) -> &mut bool;

/// The checkboxes, in the order they are drawn.
pub const TRANSFER_OPTS: &[(&str, OptField)] = &[
    ("Preserve attributes", |o| &mut o.preserve),
    ("Follow links", |o| &mut o.follow_links),
    ("Dive into subdirs", |o| &mut o.dive),
    ("Stable symlinks", |o| &mut o.stable_symlinks),
    ("Verify (read the copy back)", |o| &mut o.verify),
    ("Sync to disk as it goes (fsync)", |o| &mut o.fsync),
];
/// Row index of the button line: after the mask, the destination and
/// the boxes.
pub const TRANSFER_ROWS: usize = TRANSFER_OPTS.len() + 2;
/// The row the destination is drawn on; the mask has row 0.
pub const TRANSFER_DEST_ROW: usize = 1;

impl TransferDialog {
    pub fn checked(&self, i: usize) -> bool {
        let mut opts = self.opts;
        *(TRANSFER_OPTS[i].1)(&mut opts)
    }

    fn toggle(&mut self, i: usize) {
        let field = (TRANSFER_OPTS[i].1)(&mut self.opts);
        *field = !*field;
    }
}

/// C-x c: MC's chmod window - the twelve attribute bits as check
/// boxes, the octal beside them, and what is being changed on screen.
pub struct ChmodDialog {
    pub paths: Vec<PathBuf>,
    /// What the boxes currently say, as a mode.
    pub mode: u32,
    /// The octal field, kept as text so a half-typed value survives.
    pub octal: String,
    pub octal_cursor: usize,
    /// The entry the File section describes (the cursor one).
    pub name: String,
    pub owner: String,
    pub group: String,
    /// Focused row: one per bit, then [`CHMOD_OCTAL_ROW`], then the
    /// buttons.
    pub row: usize,
    pub button: usize,
    /// Walk into directories. MC keeps this in its "advanced chown";
    /// rcmd puts it where the change is made.
    pub recurse: bool,
}

/// The bits, top to bottom, as MC lists them.
pub const CHMOD_BITS: &[(&str, u32)] = &[
    ("set-uid", 0o4000),
    ("set-gid", 0o2000),
    ("sticky", 0o1000),
    ("read    owner", 0o400),
    ("write   owner", 0o200),
    ("exec    owner", 0o100),
    ("read    group", 0o040),
    ("write   group", 0o020),
    ("exec    group", 0o010),
    ("read    other", 0o004),
    ("write   other", 0o002),
    ("exec    other", 0o001),
];
pub const CHMOD_OCTAL_ROW: usize = CHMOD_BITS.len();
pub const CHMOD_RECURSE_ROW: usize = CHMOD_BITS.len() + 1;
pub const CHMOD_ROWS: usize = CHMOD_BITS.len() + 2;
/// What the buttons do to each selected entry's own mode.
pub const CHMOD_BUTTONS: &[&str] = &["&Set", "Set &marked", "&Clear marked", "Ca&ncel"];

impl ChmodDialog {
    /// Re-render the octal field from the boxes.
    fn sync_octal(&mut self) {
        self.octal = format!("{:o}", self.mode);
        self.octal_cursor = self.octal.chars().count();
    }

    /// ...and the other way, for whatever the octal field now says.
    fn sync_mode(&mut self) {
        if let Ok(mode) = u32::from_str_radix(self.octal.trim(), 8)
            && mode <= 0o7777
        {
            self.mode = mode;
        }
    }
}

/// Something `C-x u` can put back.
#[derive(Debug, Clone)]
pub enum UndoStep {
    /// What a move job did, as `(from, to)` pairs in the order it did
    /// them.
    Moved(Vec<(PathBuf, PathBuf)>),
    /// A bulk rename's batch in its directory, as `(old, new)` names.
    /// It is put back the way it was done - in two phases - so a swap
    /// swaps back, where a pair at a time would find both names taken.
    Renamed {
        dir: PathBuf,
        renames: Vec<(OsString, OsString)>,
    },
    /// What F8 sent to the trash, by where it was.
    Trashed(Vec<PathBuf>),
    /// What came back out of the trash, by where it went.
    Restored(Vec<PathBuf>),
}

/// How many undo steps are kept: enough for a session's regrets, few
/// enough that the oldest are still about the disk as it is.
pub const UNDO_DEPTH: usize = 32;

impl UndoStep {
    /// The row C-x u shows: what undoing this would do.
    pub fn describe(&self) -> String {
        fn name(path: &Path) -> String {
            path.file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned()
        }
        match self {
            UndoStep::Moved(pairs) => match pairs.as_slice() {
                [(from, to)] => format!(
                    "Put \"{}\" back from {}",
                    name(from),
                    crate::ui::abbrev_home(to.parent().unwrap_or(to))
                ),
                _ => format!("Put {} moved items back", pairs.len()),
            },
            UndoStep::Renamed { dir, renames } => format!(
                "Put {} renamed name(s) back in {}",
                renames.len(),
                crate::ui::abbrev_home(dir)
            ),
            UndoStep::Trashed(paths) => match paths.as_slice() {
                [path] => format!("Take \"{}\" out of the trash", name(path)),
                _ => format!("Take {} items out of the trash", paths.len()),
            },
            UndoStep::Restored(paths) => match paths.as_slice() {
                [path] => format!("Put \"{}\" back in the trash", name(path)),
                _ => format!("Put {} items back in the trash", paths.len()),
            },
        }
    }
}

/// C-x e: mc's chattr window - the file flags `lsattr` shows, as check
/// boxes, and what the cursor entry has now in `lsattr`'s letters.
pub struct ChattrDialog {
    pub paths: Vec<PathBuf>,
    /// What the boxes say.
    pub flags: u32,
    /// What the cursor entry has, to show beside them.
    pub was: u32,
    pub name: String,
    /// Focused row: one per flag, then the buttons.
    pub row: usize,
    pub button: usize,
}

pub const CHATTR_ROWS: usize = rcmd_core::attrs::FLAGS.len();
/// chmod's three ways to spend the boxes: exactly, added, taken away.
pub const CHATTR_BUTTONS: &[&str] = &["&Set", "Set &marked", "&Clear marked", "Ca&ncel"];

/// C-x o: MC's chown window - the system's users and groups as two
/// pick lists, with what is being changed beside them.
pub struct ChownDialog {
    pub paths: Vec<PathBuf>,
    pub users: Vec<(u32, String)>,
    pub groups: Vec<(u32, String)>,
    pub user_row: usize,
    pub group_row: usize,
    /// 0 = the user list, 1 = the group list, 2 = the recurse box,
    /// 3 = the buttons.
    pub column: usize,
    pub button: usize,
    pub name: String,
    pub owner: String,
    pub group: String,
    /// Walk into directories.
    pub recurse: bool,
}

pub const CHOWN_BUTTONS: &[&str] = &["Set", "Cancel"];
/// F5/F6's buttons. Background is mc's, and it earns the `&` because
/// the two others start with letters it does not.
/// Queue is Total Commander's F2: the job waits until nothing else is
/// writing to the device it writes to, so two copies onto one USB stick
/// run one after the other instead of fighting over its head.
pub const TRANSFER_BUTTONS: &[&str] = &["OK", "&Background", "&Queue", "Cancel"];
pub const YES_NO: &[&str] = &["Yes", "No"];
pub const OK_CANCEL: &[&str] = &["OK", "Cancel"];
/// Focus stops in the chown window: two lists, the box, the buttons.
pub const CHOWN_STOPS: usize = 4;
pub const CHOWN_RECURSE_COL: usize = 2;
pub const CHOWN_BUTTON_COL: usize = 3;
/// Rows of each pick list on screen.
pub const CHOWN_ROWS: usize = 12;

impl ChownDialog {
    /// The list with the focus, and where its cursor sits.
    fn list(&self) -> (&[(u32, String)], usize) {
        if self.column == 0 {
            (&self.users, self.user_row)
        } else {
            (&self.groups, self.group_row)
        }
    }

    fn move_by(&mut self, delta: isize) {
        let (list, row) = self.list();
        let last = list.len().saturating_sub(1);
        let next = (row as isize + delta).clamp(0, last as isize) as usize;
        if self.column == 0 {
            self.user_row = next;
        } else {
            self.group_row = next;
        }
    }

    /// What Set would write.
    fn picked(&self) -> (Option<u32>, Option<u32>) {
        (
            self.users.get(self.user_row).map(|u| u.0),
            self.groups.get(self.group_row).map(|g| g.0),
        )
    }
}

/// C-x l / s / v / C-s: MC's four link commands in one form - what to
/// point at, and what to call it.
pub struct LinkDialog {
    pub kind: LinkKind,
    pub target: TextField,
    pub name: TextField,
    /// 0 = target, 1 = name, 2 = the buttons. Editing a symlink has no
    /// name row: the link already has one.
    pub row: usize,
    pub ok: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// C-x l: a second name for the same file.
    Hard,
    /// C-x s / C-x v: a link holding a path, absolute or relative.
    Symbolic,
    /// C-x C-s: rewrite an existing link's target.
    EditSymlink,
}

impl LinkDialog {
    pub fn title(&self) -> &'static str {
        match self.kind {
            LinkKind::Hard => " Hard link ",
            LinkKind::Symbolic => " Symlink ",
            LinkKind::EditSymlink => " Edit symlink ",
        }
    }

    /// Editing a link only has a target to change.
    pub fn rows(&self) -> usize {
        if self.kind == LinkKind::EditSymlink {
            1
        } else {
            2
        }
    }
}

pub struct ConfirmDialog {
    pub title: String,
    pub message: String,
    pub yes: bool,
    pub paths: Vec<PathBuf>,
    pub permanent: bool,
    pub kind: ConfirmKind,
    /// A command the answer would run, for the kinds that need one.
    pub command: Option<String>,
}

/// What a [`ConfirmDialog`] does when answered Yes.
#[derive(Clone, PartialEq, Eq)]
pub enum ConfirmKind {
    Delete,
    Quit,
    /// Dropping a hotlist entry. There is one dialog slot, so the
    /// confirm replaces the hotlist and puts it back either way.
    HotlistDelete {
        group: Vec<usize>,
        index: usize,
    },
    /// Enter about to run an `[[open]]` command.
    Execute,
    /// F6 on a `trash://` panel: put these back where they came from.
    Restore,
    /// `M-Del` about to overwrite and delete.
    Wipe,
}

/// mc's Learn keys: the keys a terminal is supposed to send, and which
/// of them have actually arrived. rcmd cannot reprogram a terminal the
/// way mc's version rewrites its keymap, so this one answers the
/// question a user actually has - *what does rcmd see when I press
/// this?* - and names it the way the config would.
pub struct LearnDialog {
    /// Which of [`LEARN_KEYS`] have been pressed and arrived intact.
    pub seen: Vec<bool>,
    pub row: usize,
    /// The last key that arrived, in config spelling, and whether it
    /// was the one the cursor was on.
    pub last: Option<(String, bool)>,
}

/// The keys worth checking, in mc's order-ish: the function keys, the
/// movement block, and the modified arrows that terminals disagree
/// about most.
pub const LEARN_KEYS: &[&str] = &[
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pgup",
    "pgdn",
    "insert",
    "delete",
    "tab",
    "shift+tab",
    "shift+f3",
    "shift+f4",
    "ctrl+left",
    "ctrl+right",
    "ctrl+up",
    "ctrl+down",
    "alt+enter",
];

/// The F2 user menu: the entries that apply here, the way down into
/// whatever submenu is open, and the row the cursor is on.
pub struct UserMenuDialog {
    /// The whole menu for this directory - `[[commands]]` plus a local
    /// `.mc.menu` if there is one - gathered when the menu opened, so
    /// the conditions are answered once and the list cannot change
    /// under the cursor.
    pub menu: Vec<UserCommand>,
    pub path: Vec<usize>,
    pub row: usize,
    /// Whether a `.mc.menu` in the panel's directory contributed.
    pub local: bool,
}

impl UserMenuDialog {
    /// The submenu being looked at.
    pub fn entries(&self) -> &[UserCommand] {
        let mut here = &self.menu[..];
        for &at in &self.path {
            match here.get(at) {
                Some(entry) => here = &entry.entries,
                None => break,
            }
        }
        here
    }
}

/// mc marks a dialog button's hotkey with `&` in front of a letter; a
/// label with no marker uses its first. Returns the letter (lowercased)
/// and where it sits in the label as drawn.
pub fn button_hotkey(label: &str) -> (String, Option<(usize, char)>) {
    match label.split_once('&') {
        Some((before, after)) => {
            let mut chars = after.chars();
            match chars.next() {
                Some(c) => (
                    format!("{before}{after}"),
                    Some((before.chars().count(), c.to_ascii_lowercase())),
                ),
                None => (before.to_string(), None),
            }
        }
        None => {
            let first = label.chars().next().map(|c| (0, c.to_ascii_lowercase()));
            (label.to_string(), first)
        }
    }
}

/// Which button a letter presses, if any.
pub fn button_for(labels: &[&str], c: char) -> Option<usize> {
    let c = c.to_ascii_lowercase();
    labels
        .iter()
        .position(|label| button_hotkey(label).1.is_some_and(|(_, hot)| hot == c))
}

/// A button label as it reads without its marker - what the code that
/// asks "which button is this?" compares against.
pub fn button_text(label: &str) -> String {
    button_hotkey(label).0
}

/// mc's underlined hotkeys: Alt and the letter a button is marked with
/// presses it, wherever the focus happens to be - which is what makes
/// them useful in a dialog with a text field in it. This moves the
/// focus onto that button; the caller then hands the dialog an Enter,
/// which every one of them already knows what to do with.
fn focus_button(dialog: &mut Dialog, c: char) -> bool {
    let pick = |labels: &[&str], at: &mut usize| match button_for(labels, c) {
        Some(found) => {
            *at = found;
            true
        }
        None => false,
    };
    let pick2 = |labels: &[&str], ok: &mut bool| match button_for(labels, c) {
        Some(found) => {
            *ok = found == 0;
            true
        }
        None => false,
    };
    match dialog {
        Dialog::Confirm(d) => pick2(YES_NO, &mut d.yes),
        Dialog::RenamePreview(d) => pick2(YES_NO, &mut d.yes),
        Dialog::Chmod(d) => pick(CHMOD_BUTTONS, &mut d.button),
        Dialog::Chattr(d) => pick(CHATTR_BUTTONS, &mut d.button),
        Dialog::Chown(d) => pick(CHOWN_BUTTONS, &mut d.button),
        Dialog::FindResults(d) => pick(FIND_BUTTONS, &mut d.button),
        // these three only act on their button row, so the focus has to
        // go there as well as onto the button
        Dialog::Transfer(d) => {
            let landed = pick(TRANSFER_BUTTONS, &mut d.button);
            if landed {
                d.row = TRANSFER_ROWS;
            }
            landed
        }
        Dialog::Options(d) => {
            let landed = pick2(OK_CANCEL, &mut d.ok);
            if landed {
                d.cursor = OPTION_ROWS.len();
            }
            landed
        }
        Dialog::Pattern(d) => {
            let landed = pick2(OK_CANCEL, &mut d.ok);
            if landed {
                d.row = PATTERN_ROWS;
            }
            landed
        }
        Dialog::Find(d) => {
            let landed = pick2(OK_CANCEL, &mut d.ok);
            if landed {
                d.row = FIND_ROWS;
            }
            landed
        }
        Dialog::Link(d) => {
            let last = d.rows();
            let landed = pick2(OK_CANCEL, &mut d.ok);
            if landed {
                d.row = last;
            }
            landed
        }
        _ => false,
    }
}

/// How many viewed/edited files are remembered.
const FILE_HISTORY: usize = 60;

/// One drawn row of the hotlist: the way back up, a group to walk into,
/// a place to go, or one of rcmd's own recent directories.
pub enum HotRow {
    Up,
    Group(usize),
    Entry(usize),
    Recent(String),
}

/// The directory hotlist, which is a tree: `group` is the way down to
/// the one being looked at, `row` the selected row inside it.
pub struct HotlistDialog {
    pub group: Vec<usize>,
    pub row: usize,
    /// An entry picked up with `m`, waiting for a group to be put in.
    /// mc calls this cut-and-insert; it is the only way to move an
    /// entry that is already somewhere else.
    pub moving: Option<HotEntry>,
    /// `C-s`: what has been typed to narrow the list. `None` is not
    /// filtering at all, which is what leaves the letter commands
    /// (a, g, e, m, d) meaning what they mean.
    pub filter: Option<String>,
}

impl HotlistDialog {
    pub fn at(group: Vec<usize>, row: usize) -> Box<Self> {
        Box::new(HotlistDialog {
            group,
            row,
            moving: None,
            filter: None,
        })
    }
}

pub enum Dialog {
    Input(InputDialog),
    Confirm(ConfirmDialog),
    /// Directory hotlist; the payload says where in the tree it is.
    Hotlist(Box<HotlistDialog>),
    Find(Box<FindDialog>),
    /// F2 user menu ([[commands]]); the payload says which submenu it
    /// is in and where the cursor is.
    UserMenu(Box<UserMenuDialog>),
    Options(OptionsDialog),
    /// Bulk rename: what the edited buffer asks for, awaiting Yes/No.
    RenamePreview(RenamePreview),
    /// Background-jobs list; the payload is the selected row.
    Jobs(usize),
    /// M-e: the panel's codepage, with the row it is on.
    Charset(usize),
    /// F9 > Options > Appearance: the theme list, with the row it is on.
    Skin(usize),
    /// F9 > Options > Learn keys.
    Learn(Box<LearnDialog>),
    /// C-x d: how to compare the two listings, with the row it is on.
    Compare(usize),
    /// The viewed/edited file history; the payload is the selected row.
    FileHistory(usize),
    /// F9 > Command > Synchronize: the plan the comparison produced.
    Sync(Box<SyncDialog>),
    /// `C-x f`: the named filter sets and which are on.
    Filters(Box<FiltersDialog>),
    /// External panelize: the saved commands and the one being typed.
    Panelize(Box<PanelizeDialog>),
    /// Select / unselect group, and the panel filter.
    Pattern(Box<PatternDialog>),
    /// MC's find results window.
    FindResults(Box<FindResults>),
    /// M-h: the command-line history; the payload is the selected row.
    History(usize),
    /// M-H: the active panel's directory history, newest first; the
    /// payload is the selected row.
    DirHistory(usize),
    /// F9 > Command > Directory tree. Enter here changes the *current*
    /// panel and closes, which is mc's rule for the dialog - the tree
    /// listing mode moves the other panel instead.
    Tree(Box<Tree>),
    /// F5/F6: the copy/move form.
    Transfer(Box<TransferDialog>),
    /// C-x u: what can be undone, newest first; the payload is the
    /// selected row.
    Undo(usize),
    /// C-x c: the chmod bit matrix.
    Chmod(Box<ChmodDialog>),
    /// C-x e: the chattr flags.
    Chattr(Box<ChattrDialog>),
    /// C-x o: the chown pick lists.
    Chown(Box<ChownDialog>),
    /// C-x l / s / v / C-s: the link form.
    Link(Box<LinkDialog>),
    /// C-x a: what the panels are sitting on that is not the local
    /// filesystem.
    Vfs(VfsDialog),
    /// M-/: the fuzzy finder over the tree under the panel.
    Fuzzy(Box<FuzzyDialog>),
}

/// The fuzzy finder: what is typed, and the tree it is matched against.
pub struct FuzzyDialog {
    pub field: TextField,
    pub root: PathBuf,
    /// Every path the walk has found so far, relative to `root`, and
    /// whether it is a directory.
    pub all: Vec<(String, bool)>,
    /// Indices into `all`, best match first.
    pub shown: Vec<usize>,
    pub selected: usize,
    pub top: usize,
    /// The walk, while it is still going.
    pub walking: Option<FindHandle>,
    /// The pattern `shown` was ranked for.
    pub ranked_for: Option<String>,
}

/// One line of the active VFS list.
pub struct VfsRow {
    /// What the row says.
    pub label: String,
    /// Where Enter goes: the `sftp://` prefix, the archive's path, or
    /// the mount point.
    pub target: String,
    /// Which panels are on it right now - 0 left, 1 right.
    pub used_by: Vec<usize>,
    pub kind: VfsKind,
}

/// What a row of the list is. The first two are rcmd's own doing and
/// can be freed; the third is the machine's, and Enter is all it
/// answers to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VfsKind {
    /// An SFTP or FTP connection, which can outlive the panel that
    /// opened it - the only kind that is ever idle.
    Remote,
    Archive,
    /// A mounted filesystem, from `df`.
    Mount,
}

/// `C-x f`: the named filter sets, and which of them this panel is
/// under. Far's filter menu.
pub struct FiltersDialog {
    /// One per `[[filter]]` entry, in config order.
    pub on: Vec<bool>,
    pub row: usize,
    /// Which panel it applies to - the active one when it opened.
    pub panel: usize,
}

pub struct VfsDialog {
    pub rows: Vec<VfsRow>,
    pub selected: usize,
    /// Which panel Enter moves. `C-x a` targets the active one; Far's
    /// M-F1 and M-F2 name a side outright.
    pub panel: usize,
}

/// The confirmation step of a bulk rename - nothing has touched the
/// filesystem yet when this is on screen.
pub struct RenamePreview {
    pub dir: PathBuf,
    pub renames: Vec<(std::ffi::OsString, String)>,
    pub deletes: Vec<std::ffi::OsString>,
    pub yes: bool,
}

/// An in-flight bulk rename editor session: the numbered temp buffer
/// and the original names its indices map back to.
pub struct BulkRename {
    dir: PathBuf,
    names: Vec<std::ffi::OsString>,
    temp: PathBuf,
}

/// What closing an editor has to do afterwards. It belongs to the
/// editor rather than to the App: with more than one open at a time,
/// an App-wide slot would run the wrong one's follow-up.
pub enum EditFollowUp {
    /// A scratch copy of a remote file, to upload if it changed.
    Remote(RemoteEdit),
    /// A bulk-rename buffer, to diff into renames.
    Bulk(BulkRename),
}

/// MC's external panelize: the saved commands, and the one being
/// typed. Running one streams its output into the panel as it arrives.
pub struct PanelizeDialog {
    /// The command whose output becomes the listing.
    pub command: TextField,
    /// Which saved preset the cursor is on.
    pub row: usize,
    /// The list has the focus rather than the command field.
    pub on_list: bool,
    /// Ctrl+S: the field is asking for a name to save the command as,
    /// and where its cursor is.
    pub naming: Option<(String, usize)>,
}

/// MC's select / unselect / filter dialog: a pattern and the three
/// answers that change what it means. One form for all three, because
/// in mc they are one dialog with a different title.
pub struct PatternDialog {
    pub title: String,
    pub value: TextField,
    /// DN's other two questions: how big, and how recently touched.
    /// Empty asks nothing, which is what they both start as.
    pub size: TextField,
    pub newer: TextField,
    pub shell: bool,
    pub case_sensitive: bool,
    pub files_only: bool,
    /// Focused row: 0 is the pattern, 1 the size, 2 the age, then one
    /// per switch, then [`PATTERN_ROWS`] for the button row.
    pub row: usize,
    pub ok: bool,
    /// What OK does: mark, unmark, or filter the listing.
    pub kind: PatternKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    Select { mark: bool },
    Filter,
}

/// The three fields plus the three switches.
pub const PATTERN_ROWS: usize = 6;
/// Rows 0..FIELDS are typed into; the rest are ticked.
pub const PATTERN_FIELDS: usize = 3;

impl PatternDialog {
    /// The core's shape of the same question.
    pub fn to_pattern(&self) -> rcmd_core::pattern::Pattern {
        rcmd_core::pattern::Pattern {
            text: self.value.value.trim().to_string(),
            shell: self.shell,
            case_sensitive: self.case_sensitive,
            files_only: self.files_only,
            size: self.size.value.trim().to_string(),
            newer: self.newer.value.trim().to_string(),
        }
    }

    /// The field the cursor is in, if it is in one.
    pub fn field_mut(&mut self) -> Option<&mut TextField> {
        match self.row {
            0 => Some(&mut self.value),
            1 => Some(&mut self.size),
            2 => Some(&mut self.newer),
            _ => None,
        }
    }

    /// The three fields, as they were answered.
    pub fn fields(&self) -> [&TextField; 3] {
        [&self.value, &self.size, &self.newer]
    }

    /// A pattern field whose history fits the question: what gets
    /// marked is not what a panel is filtered by.
    pub fn pattern_field(kind: PatternKind, value: impl Into<String>) -> TextField {
        TextField::new(value).with_history(match kind {
            PatternKind::Select { .. } => "select",
            PatternKind::Filter => "panel-filter",
        })
    }

    fn toggle(&mut self) {
        match self.row {
            3 => self.files_only = !self.files_only,
            4 => self.case_sensitive = !self.case_sensitive,
            5 => self.shell = !self.shell,
            _ => {}
        }
    }

    fn step(&mut self, step: isize) {
        let last = PATTERN_ROWS as isize; // the button row
        let mut row = self.row as isize + step;
        if row < 0 {
            row = last;
        } else if row > last {
            row = 0;
        }
        self.row = row as usize;
    }
}

/// F9 > Options > Panel options - MC-style checkbox form over the
/// config toggles. OK applies everything live and writes the config
/// file immediately (exit-time saves only cover panel state, so a
/// second running instance cannot clobber applied options).
pub struct OptionsDialog {
    /// Focused row: an index into [`OPTION_ROWS`], or its length for
    /// the OK/Cancel button row.
    pub cursor: usize,
    /// Current value of every toggle, indexed by [`Opt`].
    pub values: [bool; OPT_COUNT],
    /// Percentage of the window given to the left / top panel.
    pub ratio: u16,
    /// Focused button on the button row: true = OK.
    pub ok: bool,
}

/// One setting in the form. Radio pairs (editor, theme) are stored as a
/// bool too: the label spells out which side `true` means.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Opt {
    HorizontalSplit,
    MenuBar,
    MiniStatus,
    FreeSpace,
    StatusLine,
    CommandLine,
    KeyBar,
    Hidden,
    Lynx,
    Mouse,
    Watch,
    RestoreOtherDir,
    Git,
    ConfirmDelete,
    ConfirmOverwrite,
    ConfirmExit,
    ConfirmHotlistDelete,
    ConfirmExecute,
    Subshell,
    ExternalEditor,
    /// Not a setting: how many there are. Adding one above grows the
    /// values array with it, which is the point - [`OPT_COUNT`] used to
    /// be a hand-kept number, and getting it wrong indexed past the end
    /// of the array. Keep this last.
    Count,
}

pub const OPT_COUNT: usize = Opt::Count as usize;

/// A row of the options form: a section heading or a setting.
pub enum OptRow {
    Head(&'static str),
    /// Checkbox: label.
    Check(Opt, &'static str),
    /// Radio pair: (label, text for false, text for true).
    Radio(Opt, &'static str, &'static str, &'static str),
    /// The panel split percentage, adjusted with Left/Right.
    Ratio(&'static str),
}

/// The form, in display order. One dialog covering MC's five (PLAN4 S0):
/// sections keep it readable without five separate screens.
pub const OPTION_ROWS: &[OptRow] = &[
    OptRow::Head("Layout"),
    OptRow::Radio(Opt::HorizontalSplit, "Split ", "vertical", "horizontal"),
    OptRow::Ratio("Panel size"),
    OptRow::Check(Opt::MenuBar, "Menu bar"),
    OptRow::Check(Opt::StatusLine, "Status line (in the active panel)"),
    OptRow::Check(Opt::MiniStatus, "Mini status (per panel)"),
    OptRow::Check(Opt::FreeSpace, "Free space in the panel footer"),
    OptRow::Check(Opt::CommandLine, "Command line"),
    OptRow::Check(Opt::KeyBar, "Key bar"),
    OptRow::Head("Panel"),
    OptRow::Check(Opt::Hidden, "Show hidden files"),
    OptRow::Check(Opt::Lynx, "Lynx-like motion"),
    OptRow::Check(Opt::Mouse, "Mouse support"),
    OptRow::Check(Opt::Watch, "Auto-reload panels"),
    OptRow::Check(Opt::RestoreOtherDir, "Other panel starts where it was left"),
    OptRow::Check(Opt::Git, "Git status"),
    OptRow::Head("Confirmation"),
    OptRow::Check(Opt::ConfirmDelete, "Ask before deleting"),
    OptRow::Check(Opt::ConfirmOverwrite, "Ask before overwriting"),
    OptRow::Check(Opt::ConfirmExit, "Ask before quitting"),
    OptRow::Check(
        Opt::ConfirmHotlistDelete,
        "Ask before dropping a hotlist entry",
    ),
    OptRow::Check(Opt::ConfirmExecute, "Ask before Enter runs an opener"),
    OptRow::Head("Shell and editor"),
    OptRow::Check(Opt::Subshell, "Persistent subshell"),
    OptRow::Radio(Opt::ExternalEditor, "Editor", "internal", "external"),
    // Appearance is a list of its own (F9 > Options > Appearance): a
    // skin is one of however many files are installed, which is not a
    // thing a two-way radio can hold.
];

impl OptRow {
    pub fn opt(&self) -> Option<Opt> {
        match self {
            OptRow::Head(_) => None,
            OptRow::Check(opt, _) | OptRow::Radio(opt, ..) => Some(*opt),
            // the ratio is a stop for the cursor but has no bool
            OptRow::Ratio(_) => None,
        }
    }

    /// Rows the cursor may land on: settings and the ratio, not headings.
    pub fn selectable(&self) -> bool {
        !matches!(self, OptRow::Head(_))
    }
}

impl OptionsDialog {
    pub fn get(&self, opt: Opt) -> bool {
        self.values[opt as usize]
    }

    fn set(&mut self, opt: Opt, on: bool) {
        self.values[opt as usize] = on;
    }

    fn toggle(&mut self) {
        if let Some(opt) = OPTION_ROWS.get(self.cursor).and_then(OptRow::opt) {
            let now = self.get(opt);
            self.set(opt, !now);
        }
    }

    /// Left/Right on the ratio row nudges the split by 5%.
    fn nudge(&mut self, step: i16) -> bool {
        if !matches!(OPTION_ROWS.get(self.cursor), Some(OptRow::Ratio(_))) {
            return false;
        }
        self.ratio = (self.ratio as i16 + step).clamp(20, 80) as u16;
        true
    }

    /// Move the cursor by `step`, skipping section headings and
    /// stopping on the button row (which sits past the last option).
    fn step(&mut self, step: isize) {
        let last = OPTION_ROWS.len(); // the button row
        let mut cursor = self.cursor as isize;
        loop {
            cursor += step;
            if cursor < 0 {
                cursor = last as isize;
            } else if cursor > last as isize {
                cursor = 0;
            }
            if cursor as usize == last
                || OPTION_ROWS
                    .get(cursor as usize)
                    .is_some_and(OptRow::selectable)
            {
                self.cursor = cursor as usize;
                return;
            }
        }
    }
}

pub enum Ask {
    /// MC's overwrite prompt: what is on each side, and what may be
    /// done about it. `can_append` is false unless both sides are local
    /// files - Append and Reget have nothing to open otherwise.
    Overwrite {
        path: PathBuf,
        src: FileFacts,
        dst: FileFacts,
        can_append: bool,
    },
    Error {
        path: PathBuf,
        message: String,
    },
}

/// The overwrite buttons, in MC's two groups: what to do with *this*
/// file, then what to do with every remaining one.
const OVERWRITE_BUTTONS: &[&str] = &[
    "Overwrite",
    "Append",
    "Reget",
    "Skip",
    "All",
    "Update",
    "Size differs",
    "None",
    "Abort",
];
const OVERWRITE_REPLIES: &[Reply] = &[
    Reply::Overwrite,
    Reply::Append,
    Reply::Reget,
    Reply::Skip,
    Reply::OverwriteAll,
    Reply::UpdateAll,
    Reply::SizeDiffersAll,
    Reply::SkipAll,
    Reply::Abort,
];
/// The same without Append and Reget, for a target that is not a local
/// file.
const OVERWRITE_BUTTONS_PLAIN: &[&str] = &[
    "Overwrite",
    "Skip",
    "All",
    "Update",
    "Size differs",
    "None",
    "Abort",
];
const OVERWRITE_REPLIES_PLAIN: &[Reply] = &[
    Reply::Overwrite,
    Reply::Skip,
    Reply::OverwriteAll,
    Reply::UpdateAll,
    Reply::SizeDiffersAll,
    Reply::SkipAll,
    Reply::Abort,
];

impl Ask {
    pub fn buttons(&self) -> &'static [&'static str] {
        match self {
            Ask::Overwrite {
                can_append: true, ..
            } => OVERWRITE_BUTTONS,
            Ask::Overwrite { .. } => OVERWRITE_BUTTONS_PLAIN,
            Ask::Error { .. } => &["Retry", "Skip", "Skip all", "Abort"],
        }
    }

    /// How many buttons go on each drawn row. MC keeps "this file" and
    /// "all files" apart, and Abort on a line of its own.
    pub fn button_rows(&self) -> &'static [usize] {
        match self {
            Ask::Overwrite {
                can_append: true, ..
            } => &[4, 4, 1],
            Ask::Overwrite { .. } => &[2, 4, 1],
            Ask::Error { .. } => &[4],
        }
    }

    /// Up/Down between the rows, keeping the column where it fits.
    pub fn step_row(&self, button: usize, delta: isize) -> usize {
        let rows = self.button_rows();
        let mut start = 0;
        for (r, len) in rows.iter().enumerate() {
            if button < start + len {
                let column = button - start;
                let target = (r as isize + delta).rem_euclid(rows.len() as isize) as usize;
                let target_start: usize = rows[..target].iter().sum();
                return target_start + column.min(rows[target] - 1);
            }
            start += len;
        }
        button
    }

    fn reply(&self, button: usize) -> Reply {
        match self {
            Ask::Overwrite {
                can_append: true, ..
            } => OVERWRITE_REPLIES[button],
            Ask::Overwrite { .. } => OVERWRITE_REPLIES_PLAIN[button],
            Ask::Error { .. } => [Reply::Retry, Reply::Skip, Reply::SkipAll, Reply::Abort][button],
        }
    }
}

impl Job {
    /// Fold a fresh byte count into the smoothed throughput. Samples
    /// closer together than half a second are ignored: over a few
    /// milliseconds the arithmetic says either zero or gigabytes.
    pub fn sample_rate(&mut self, bytes_done: u64) {
        let (when, bytes) = self.rate_mark;
        let elapsed = when.elapsed().as_secs_f64();
        if elapsed < 0.25 {
            return;
        }
        let sample = bytes_done.saturating_sub(bytes) as f64 / elapsed;
        // the first reading stands on its own; later ones ease in
        self.rate = if self.rate == 0.0 {
            sample
        } else {
            self.rate * 0.6 + sample * 0.4
        };
        self.rate_mark = (Instant::now(), bytes_done);
    }

    /// Throughput to show. Before the first window closes there is no
    /// sample yet, so the average since the job started stands in - the
    /// first seconds of a copy are exactly when someone is looking.
    pub fn rate(&self) -> f64 {
        if self.rate > 0.0 {
            return self.rate;
        }
        let elapsed = self.started.elapsed().as_secs_f64();
        if elapsed > 0.05 {
            self.bytes_done as f64 / elapsed
        } else {
            0.0
        }
    }

    /// Seconds left at the current rate, once there is enough to say.
    pub fn eta(&self) -> Option<f64> {
        let left = self.total_bytes.checked_sub(self.bytes_done)?;
        let rate = self.rate();
        (rate > 1.0 && self.total_bytes > 0).then(|| left as f64 / rate)
    }
}

pub struct Job {
    pub handle: JobHandle,
    pub title: String,
    pub total_files: u64,
    pub total_bytes: u64,
    pub files_done: u64,
    pub bytes_done: u64,
    pub current: PathBuf,
    /// Bytes done and total for the file in hand; 0/0 for an operation
    /// that moves whole items rather than bytes.
    pub file_done: u64,
    pub file_total: u64,
    /// Throughput in bytes per second, smoothed - a raw sample jumps
    /// around too much to read while it is changing.
    pub rate: f64,
    /// The last sample the rate was taken from.
    rate_mark: (Instant, u64),
    /// When the job started, so the very first seconds can still quote
    /// an average instead of nothing at all.
    started: Instant,
    pub ask: Option<Ask>,
    pub button: usize,
    src_panel: usize,
    /// Running detached ('b' in the progress dialog): the dialog is
    /// hidden, the panels stay interactive, asks pull it back up.
    pub background: bool,
    /// Every clean move this job made, in the order it made them: the
    /// record `C-x u` puts back. Empty for everything that is not a
    /// move, which is what makes those operations un-undoable rather
    /// than wrongly undoable.
    moved: Vec<(PathBuf, PathBuf)>,
    /// What went to the trash, and what came back out of it: the same
    /// kind of record, for an F8 and for a restore.
    trashed: Vec<PathBuf>,
    restored: Vec<PathBuf>,
    /// What the job writes to - a device, or a server - for the queue:
    /// a queued job starts once no other job writes there. `None` for
    /// the jobs that are not copies or moves.
    pub device: Option<String>,
    /// What the job left alone, and why - the report C-x r shows.
    skips: Vec<(PathBuf, String)>,
    /// A checksum check, where the count that did not match is the
    /// answer rather than a footnote about skipping.
    checking: bool,
}

pub enum Exec {
    Command(String),
    /// Like Command, but without the "Press Enter" pause - for editors
    /// and [[open]] rules (append `&` in the rule for GUI apps).
    Quiet(String),
    Shell,
}

/// Full-screen F4 internal editor: buffer logic lives in `rcmd_edit`,
/// this is viewport + prompt presentation state.
pub struct EditorState {
    pub ed: rcmd_edit::Editor,
    pub hl: Option<rcmd_edit::Highlighter>,
    /// Shown in the title bar (the sftp URL for remote scratch edits).
    pub title: String,
    pub top: usize,
    /// In wrap mode: which wrapped segment of `top` is the first row.
    pub top_seg: usize,
    /// Horizontal scroll in screen columns.
    pub left: usize,
    /// Soft-wrap (Alt+W) instead of horizontal scrolling.
    pub wrap: bool,
    /// Text area size; updated on every draw.
    pub rows: usize,
    pub cols: usize,
    /// What the last search asked, answers and all, for Shift+F7.
    pub search: ViewSearch,
    pub prompt: Option<EditPrompt>,
    pub note: Option<String>,
    /// Fixed soft-wrap column from the editor options; 0 = the window
    /// width, which is mc's "dynamic" wrap.
    pub wrap_column: usize,
    /// The editor's own menu bar (F9) when it is open.
    pub menu: Option<MenuState>,
    /// What closing this editor has to do afterwards, if anything.
    pub follow_up: Option<EditFollowUp>,
    /// Bookmarked lines (M-k), in order.
    pub bookmarks: Vec<usize>,
    /// Draw the line-number gutter (M-n).
    pub line_numbers: bool,
    /// Width the gutter took in the last draw, so a mouse click knows
    /// where the text starts.
    pub gutter: usize,
}

impl EditorState {
    /// How wide a wrapped row is: the window, or the column the options
    /// pin it to when that is narrower.
    pub fn wrap_width(&self) -> usize {
        let cols = self.cols.max(1);
        match self.wrap_column {
            0 => cols,
            fixed => fixed.clamp(1, cols),
        }
    }
}

pub enum EditPrompt {
    /// F7: mc's search dialog, the viewer's own - a literal pattern, a
    /// regular expression or hexadecimal bytes, case, whole words,
    /// backwards.
    Search(Box<ViewSearch>),
    ReplaceFind(TextField),
    ReplaceWith {
        pattern: String,
        field: TextField,
    },
    /// Per-match decision: Replace / Skip / All / Quit.
    ConfirmReplace {
        pattern: String,
        replacement: String,
        m: rcmd_edit::Match,
        count: usize,
        button: usize,
    },
    /// Quit with unsaved changes: Save / Discard / Cancel.
    ConfirmQuit {
        button: usize,
    },
    /// mc's editor options, as a form of the editor's own.
    Options(EditOptions),
    /// M-l: which line to go to.
    Goto {
        value: String,
        cursor: usize,
    },
    /// F12: the name to save under.
    SaveAs(TextField),
    /// A save would overwrite a change someone else made on disk.
    Clobber {
        button: usize,
    },
    /// M-Tab with several words to choose from: which one.
    Complete {
        words: Vec<String>,
        selected: usize,
        /// How much of each word is already typed.
        typed: usize,
    },
    /// Options > Syntax: which syntax to highlight as, whatever the
    /// file is called. Row 0 is plain text.
    Syntax {
        row: usize,
        top: usize,
    },
    /// M-e: which codepage the file is in. Re-reads it, so it is only
    /// offered while there is nothing unsaved to lose.
    Charset(usize),
}

/// The codepage picker's rows, which are the charset labels.
pub static CHARSET_ROWS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    rcmd_core::charset::CHARSETS
        .iter()
        .map(|(label, _)| *label)
        .collect()
});

/// Which row a codepage sits on; row 0 (UTF-8) for "none".
pub fn charset_row(label: Option<&str>) -> usize {
    match label {
        None => 0,
        Some(label) => CHARSET_ROWS
            .iter()
            .position(|row| *row == label)
            .unwrap_or(0),
    }
}

/// ...and back: row 0 is UTF-8, which is no recoding at all.
pub fn charset_at(row: usize) -> Option<&'static rcmd_core::charset::Encoding> {
    match row {
        0 => None,
        at => CHARSET_ROWS
            .get(at)
            .and_then(|label| rcmd_core::charset::by_label(label)),
    }
}

/// What a key does in a pick list. One reading of the keys, so the
/// codepage picker answers to the same hands wherever it is opened.
pub enum PickKey {
    Move(usize),
    Chose(usize),
    Close,
    Ignored,
}

pub fn charset_pick_key(row: usize, key: KeyEvent) -> PickKey {
    pick_key(&CHARSET_ROWS, row, key)
}

/// One key in a pick-one list. Shared by the codepage picker and the
/// skin list, so both answer to the same hands.
pub fn pick_key(rows: &[impl AsRef<str>], row: usize, key: KeyEvent) -> PickKey {
    let last = rows.len().saturating_sub(1);
    match key.code {
        KeyCode::Esc => PickKey::Close,
        KeyCode::Enter => PickKey::Chose(row),
        KeyCode::Up => PickKey::Move(row.saturating_sub(1)),
        KeyCode::Down => PickKey::Move((row + 1).min(last)),
        KeyCode::PageUp => PickKey::Move(row.saturating_sub(10)),
        KeyCode::PageDown => PickKey::Move((row + 10).min(last)),
        KeyCode::Home => PickKey::Move(0),
        KeyCode::End => PickKey::Move(last),
        // a letter walks the rows starting with it, from the one the
        // cursor is on - a list of skins has a dozen `mod...` in it,
        // and always landing on the first is a key that stops working
        KeyCode::Char(c) => {
            let c = c.to_ascii_lowercase();
            let starts = |label: &str| {
                label
                    .chars()
                    .next()
                    .is_some_and(|f| f.to_ascii_lowercase() == c)
            };
            let after = rows
                .iter()
                .skip(row + 1)
                .position(|label| starts(label.as_ref()));
            match after.map(|at| at + row + 1).or_else(|| {
                rows.iter()
                    .take(row + 1)
                    .position(|label| starts(label.as_ref()))
            }) {
                Some(at) => PickKey::Move(at),
                None => PickKey::Ignored,
            }
        }
        _ => PickKey::Ignored,
    }
}

/// How many rows of the syntax picker are on screen at once.
pub const SYNTAX_ROWS: usize = 15;

/// The syntax picker's rows: plain text, then everything syntect knows.
pub fn syntax_rows() -> Vec<&'static str> {
    let mut rows = vec!["Plain text (no highlighting)"];
    rows.extend(rcmd_edit::syntax_names());
    rows
}

/// The editor's settings form. mc keeps these in a dialog of their own
/// and so does rcmd: they belong to the editor, are set while editing,
/// and the panel's grouped options dialog is already a screenful.
#[derive(Clone)]
pub struct EditOptions {
    pub tab_size: u16,
    pub fill_tabs: bool,
    pub auto_indent: bool,
    pub backspace_tabs: bool,
    /// 0 = the window width (mc's dynamic wrap).
    pub wrap_column: u16,
    pub line_numbers: bool,
    pub backups: bool,
    pub clipboard: bool,
    /// Focused row: an index into [`EDIT_OPTION_ROWS`], or its length
    /// for the OK/Cancel row.
    pub cursor: usize,
    pub ok: bool,
}

/// One row of that form. The numbers are nudged with Left/Right, the
/// rest tick with Space - the same hands the panel's form takes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditOpt {
    TabSize,
    FillTabs,
    AutoIndent,
    BackspaceTabs,
    WrapColumn,
    LineNumbers,
    Backups,
    Clipboard,
}

pub const EDIT_OPTION_ROWS: &[(EditOpt, &str)] = &[
    (EditOpt::TabSize, "Tab size"),
    (EditOpt::FillTabs, "Fill tabs with spaces"),
    (EditOpt::AutoIndent, "Return does autoindent"),
    (EditOpt::BackspaceTabs, "Backspace through tabs"),
    (EditOpt::WrapColumn, "Wrap column"),
    (EditOpt::LineNumbers, "Show line numbers"),
    (EditOpt::Backups, "Keep a file~ backup on save"),
    (EditOpt::Clipboard, "Share the system clipboard"),
];

impl EditOptions {
    pub fn get(&self, opt: EditOpt) -> bool {
        match opt {
            EditOpt::FillTabs => self.fill_tabs,
            EditOpt::AutoIndent => self.auto_indent,
            EditOpt::BackspaceTabs => self.backspace_tabs,
            EditOpt::LineNumbers => self.line_numbers,
            EditOpt::Backups => self.backups,
            EditOpt::Clipboard => self.clipboard,
            _ => false,
        }
    }

    /// How the row reads: a number shows its value, a switch its box.
    pub fn value(&self, opt: EditOpt) -> String {
        match opt {
            EditOpt::TabSize => format!("{:>6}", self.tab_size),
            // a column of zero is not a column: it is "as wide as the
            // window is", which is what mc calls dynamic wrapping
            EditOpt::WrapColumn => match self.wrap_column {
                0 => "window".to_string(),
                n => format!("{n:>6}"),
            },
            _ => String::new(),
        }
    }

    fn toggle(&mut self) {
        match EDIT_OPTION_ROWS.get(self.cursor).map(|(opt, _)| *opt) {
            Some(EditOpt::FillTabs) => self.fill_tabs = !self.fill_tabs,
            Some(EditOpt::AutoIndent) => self.auto_indent = !self.auto_indent,
            Some(EditOpt::BackspaceTabs) => self.backspace_tabs = !self.backspace_tabs,
            Some(EditOpt::LineNumbers) => self.line_numbers = !self.line_numbers,
            Some(EditOpt::Backups) => self.backups = !self.backups,
            Some(EditOpt::Clipboard) => self.clipboard = !self.clipboard,
            _ => {}
        }
    }

    /// Left/Right on a number row. False = this row has no number, so
    /// the key means something else.
    fn nudge(&mut self, step: i32) -> bool {
        match EDIT_OPTION_ROWS.get(self.cursor).map(|(opt, _)| *opt) {
            Some(EditOpt::TabSize) => {
                self.tab_size = (self.tab_size as i32 + step).clamp(1, 16) as u16;
                true
            }
            Some(EditOpt::WrapColumn) => {
                // 0, then the useful range: one step off zero lands on
                // a column worth wrapping at rather than on 1
                let now = self.wrap_column as i32;
                let next = match (now, step) {
                    (0, s) if s > 0 => 40,
                    (40, s) if s < 0 => 0,
                    (n, s) => (n + s * 5).clamp(0, 512),
                };
                self.wrap_column = if next < 40 && next != 0 {
                    if step < 0 { 0 } else { 40 }
                } else {
                    next as u16
                };
                true
            }
            _ => false,
        }
    }

    fn step(&mut self, step: isize) {
        let last = EDIT_OPTION_ROWS.len() as isize; // the button row
        let mut cursor = self.cursor as isize + step;
        if cursor < 0 {
            cursor = last;
        } else if cursor > last {
            cursor = 0;
        }
        self.cursor = cursor as usize;
    }
}

/// What an entry of the editor's menu bar does: mostly the actions its
/// keys already run, plus the one the menu owns.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditMenuAction {
    Key(keymap::EditorAction),
    Options,
    /// The syntax picker.
    Syntax,
    /// The screen list, which is the App's rather than the editor's.
    ScreenList,
}

pub type EditMenuEntry = Option<(&'static str, &'static str, EditMenuAction)>;

/// The editor's menu bar (F9), in mc's four groups. Every entry is
/// something a key already does - the menu is how you find the key.
pub const EDIT_MENUS: &[(&str, &[EditMenuEntry])] = &[
    ("&File", EDIT_FILE_MENU),
    ("&Edit", EDIT_EDIT_MENU),
    ("&Search", EDIT_SEARCH_MENU),
    ("&Options", EDIT_OPTIONS_MENU),
];

use keymap::EditorAction as EA;

mod connect;
mod dialog;
mod diffview;
mod editor;
mod exec;
mod focus;
mod fuzzy;
mod panel;
mod search;
mod viewer;

pub use diffview::{DiffPrompt, DiffSide, DiffSource, DiffView};
pub use exec::{SubshellSession, SubshellStep};

const EDIT_FILE_MENU: &[EditMenuEntry] = &[
    Some(("&Save", "F2", EditMenuAction::Key(EA::Save))),
    Some(("Save &as...", "F12", EditMenuAction::Key(EA::SaveAs))),
    None,
    Some(("Screen &list...", "M-`", EditMenuAction::ScreenList)),
    Some(("&Quit", "F10", EditMenuAction::Key(EA::Quit))),
];

const EDIT_EDIT_MENU: &[EditMenuEntry] = &[
    Some(("&Undo", "C-z", EditMenuAction::Key(EA::Undo))),
    Some(("&Redo", "C-y", EditMenuAction::Key(EA::Redo))),
    None,
    Some(("&Copy", "C-c", EditMenuAction::Key(EA::Copy))),
    Some(("Cu&t", "C-x", EditMenuAction::Key(EA::Cut))),
    Some(("&Paste", "C-v", EditMenuAction::Key(EA::Paste))),
    None,
    Some(("&Mark", "F3", EditMenuAction::Key(EA::Mark))),
    Some(("Select &all", "C-a", EditMenuAction::Key(EA::SelectAll))),
    Some(("&Delete line", "F8", EditMenuAction::Key(EA::DeleteLine))),
    Some(("Copy &block", "F5", EditMenuAction::Key(EA::BlockCopy))),
    Some(("Mo&ve block", "F6", EditMenuAction::Key(EA::BlockMove))),
];

const EDIT_SEARCH_MENU: &[EditMenuEntry] = &[
    Some(("&Search", "F7", EditMenuAction::Key(EA::Search))),
    Some(("Search &next", "S-F7", EditMenuAction::Key(EA::SearchNext))),
    Some(("&Replace", "F4", EditMenuAction::Key(EA::Replace))),
    None,
    Some(("&Go to line", "M-l", EditMenuAction::Key(EA::Goto))),
    Some((
        "Matching &bracket",
        "M-b",
        EditMenuAction::Key(EA::MatchBracket),
    )),
    Some(("Complete &word", "M-Tab", EditMenuAction::Key(EA::Complete))),
    Some((
        "&Toggle bookmark",
        "M-k",
        EditMenuAction::Key(EA::BookmarkToggle),
    )),
    Some((
        "Next book&mark",
        "M-j",
        EditMenuAction::Key(EA::BookmarkNext),
    )),
    Some((
        "Pre&vious bookmark",
        "M-i",
        EditMenuAction::Key(EA::BookmarkPrev),
    )),
    Some((
        "&Clear bookmarks",
        "M-o",
        EditMenuAction::Key(EA::BookmarkClear),
    )),
];

const EDIT_OPTIONS_MENU: &[EditMenuEntry] = &[
    Some(("&General...", "", EditMenuAction::Options)),
    Some(("Soft &wrap", "M-w", EditMenuAction::Key(EA::ToggleWrap))),
    Some((
        "Line &numbers",
        "M-n",
        EditMenuAction::Key(EA::ToggleLineNumbers),
    )),
    Some(("S&yntax...", "", EditMenuAction::Syntax)),
    Some(("Cod&epage...", "M-e", EditMenuAction::Key(EA::Charset))),
];

/// One panel side's cached free-space measurement.
pub type DiskSpace = Option<(PathBuf, Instant, Option<(u64, u64)>)>;

/// What a click on a form dialog landed on: one of its rows - a field
/// or a switch - or one of its buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormHit {
    Row(usize),
    Button(usize),
}

/// Where the open list dialog drew its rows, and which index the
/// topmost drawn row answers to. Filled in on every draw and spent by
/// the mouse.
#[derive(Clone)]
pub struct DialogRows {
    pub area: Rect,
    /// Which selectable row each drawn line answers to, top-down.
    /// `None` is a line that is not one - a heading, say.
    pub rows: Vec<Option<usize>>,
}

/// Where the main-screen regions landed in the last draw; filled by
/// [`ui::draw`], read by the mouse hit-testing.
#[derive(Default, Clone, Copy)]
pub struct Areas {
    pub screen: Rect,
    pub left: Rect,
    pub right: Rect,
    pub keybar: Rect,
    pub menubar: Rect,
}

/// A list over the line being typed into: M-h's history of a field,
/// or the candidates of a completion that had more than one.
pub struct FieldPopup {
    pub title: &'static str,
    /// What the rows say, top first.
    pub rows: Vec<String>,
    /// What picking each row puts on the line.
    pub picks: Vec<String>,
    pub selected: usize,
    /// The stretch of the line, in characters, a pick replaces: the
    /// word being completed. `None` = the whole line, as a history
    /// entry is.
    pub span: Option<(usize, usize)>,
}

/// The line that has the keyboard, if one does: a form's field with
/// its history, or a plain line - the command line, a password, a
/// goto - that has none.
pub enum FocusedLine<'a> {
    Field(&'a mut TextField),
    Plain(&'a mut String, &'a mut usize),
}

impl FocusedLine<'_> {
    fn insert(self, text: &str) {
        match self {
            FocusedLine::Field(field) => field.insert(text),
            FocusedLine::Plain(value, cursor) => crate::field::insert_text(value, cursor, text),
        }
    }

    /// The text and its cursor, whichever kind of line it is.
    fn parts(&mut self) -> (&mut String, &mut usize) {
        match self {
            FocusedLine::Field(field) => (&mut field.value, &mut field.cursor),
            FocusedLine::Plain(value, cursor) => (value, cursor),
        }
    }
}

/// Bracketed paste on or off. On, a paste arrives as one event rather
/// than as keystrokes, so a newline in it is text and not Enter, and a
/// leading `+` is not the select-group key. Off whenever the terminal
/// is handed to a shell or a program, which want pastes as their own.
pub fn set_bracketed_paste(on: bool) {
    let mut out = std::io::stdout();
    let _ = if on {
        ratatui::crossterm::execute!(out, event::EnableBracketedPaste)
    } else {
        ratatui::crossterm::execute!(out, event::DisableBracketedPaste)
    };
}

/// Turn terminal mouse reporting on or off (a no-op if the terminal
/// ignores it). Kept here so the shell suspend can toggle it too.
pub fn set_mouse_capture(on: bool) {
    let mut out = std::io::stdout();
    let _ = if on {
        ratatui::crossterm::execute!(out, EnableMouseCapture)
    } else {
        ratatui::crossterm::execute!(out, DisableMouseCapture)
    };
}

/// Ctrl+X Q: one panel becomes a live preview of the file under the
/// other panel's cursor (chunked access via [`FileView`], so huge files
/// preview instantly).
pub struct QuickView {
    /// Which panel renders the preview.
    pub side: usize,
    pub view: Option<(PathBuf, FileView)>,
    /// Shown instead of content when there is nothing to preview.
    pub note: String,
    pub top: usize,
    /// F4 while the preview is focused: hex dump instead of text.
    pub hex: bool,
    /// Content rows; updated on every draw, drives paging.
    pub rows: usize,
}

/// Full-screen F3 viewer state; the chunked file access lives in
/// [`FileView`], this is only presentation state.
pub struct Viewer {
    pub file: FileView,
    pub path: PathBuf,
    /// Syntect highlighting, present only under the editor's size
    /// ceiling (2 MB) for a recognized syntax; None = plain (fast).
    pub hl: Option<rcmd_edit::Highlighter>,
    pub hex: bool,
    /// Soft-wrap long lines (F2) instead of horizontal scrolling.
    pub wrap: bool,
    /// Follow mode ('f', tail -f): pick up appended data every loop
    /// tick and stick to the bottom.
    pub follow: bool,
    pub top: usize,
    /// In wrap mode: which wrapped segment of `top` is the first row.
    pub top_seg: usize,
    pub left: usize,
    /// Content columns; updated on every draw, drives wrapping.
    pub cols: usize,
    /// Top row of the hex view (16 bytes per row).
    pub hex_top: u64,
    /// Hex mode with a cursor on it: F2 turns it on where the viewer is
    /// on the file itself rather than a copy of it.
    pub hex_edit: bool,
    /// Which byte that cursor is on.
    pub hex_cursor: u64,
    /// In the hex column: the low nibble is what the next digit fills.
    pub hex_low: bool,
    /// The cursor is in the ASCII column rather than the hex one.
    pub hex_ascii: bool,
    /// Bytes changed and not yet written, by offset - the file on disk
    /// is untouched until F6.
    pub hex_edits: BTreeMap<u64, u8>,
    /// The last search hit in the hex view, as (offset, length): the
    /// hex view shows bytes, so its hit is a byte range, not a line.
    pub hex_hit: Option<(u64, u64)>,
    /// Leaving with bytes unwritten: Save / Discard / Cancel.
    pub confirm_quit: Option<usize>,
    /// The viewer is on a scratch copy (an archive member, a remote
    /// file), so writing to it would write to nothing that lasts.
    pub scratch: bool,
    /// Content rows; updated on every draw, drives paging.
    pub rows: usize,
    /// What the last search asked for, so "next" repeats it exactly.
    pub search: ViewSearch,
    pub found: Option<usize>,
    /// The search dialog when it is open.
    pub prompt: Option<ViewSearch>,
    /// The goto prompt (value, cursor) when it is open.
    pub goto: Option<(String, usize)>,
    /// MC's ten numbered marks; `m<digit>` sets one, `r<digit>`
    /// returns to it.
    pub bookmarks: [Option<usize>; 10],
    /// An `m` or `r` waiting for its digit - Some(true) is set,
    /// Some(false) is go.
    pub pending_mark: Option<bool>,
    /// A column ruler under the title.
    pub ruler: bool,
    /// The codepage picker (M-e) when it is open, with its row.
    pub charset_pick: Option<usize>,
    /// nroff mode (F8): overstrikes read as bold and underline rather
    /// than shown as the control characters they are.
    pub nroff: bool,
    /// The file on disk to read when unfiltered - the file itself, or
    /// the scratch copy of an archive member or a remote file.
    pub source: PathBuf,
    /// What the title says in that state.
    pub source_title: PathBuf,
    /// The `[[view]]` rule this file matches, if any: F6 swaps the
    /// filter in and out without leaving the viewer.
    pub filter: Option<crate::config::OpenRule>,
    /// Whether what is on screen is the filter's output.
    pub filtered: bool,
    /// Whether the filter is unwanted - Shift+F3, or F6 - so that
    /// stepping to the next file keeps the answer.
    pub opened_raw: bool,
    pub note: Option<String>,
    /// Scratch files (an extracted archive member, a filter's output);
    /// removed when the viewer closes.
    pub temps: Vec<PathBuf>,
}

/// Viewer state that survives swapping the filter in and out or moving
/// to the next file: how the text is shown, and what to look for in it.
#[derive(Clone, Default)]
pub struct ViewKeep {
    pub wrap: bool,
    pub hex: bool,
    pub ruler: bool,
    pub nroff: bool,
    pub search: ViewSearch,
    /// M-!'s command: a filter for this one opening, in place of
    /// whatever `[[view]]` rule the name would have matched.
    pub filter: Option<crate::config::OpenRule>,
}

impl Viewer {
    /// Whether the bytes on screen are bytes of a file that can be
    /// written back: the file itself, not a copy and not a filter's
    /// output.
    pub fn editable(&self) -> Option<&'static str> {
        if self.scratch {
            return Some(" this is a copy, not the file - hex edit needs the file ");
        }
        if self.filtered {
            return Some(" this is the filter's output - F6 for the file itself ");
        }
        None
    }

    /// The byte at `offset` as it stands, pending edits included.
    pub fn byte_at(&self, offset: u64) -> Option<u8> {
        if let Some(&byte) = self.hex_edits.get(&offset) {
            return Some(byte);
        }
        self.file.read_at(offset, 1).ok()?.first().copied()
    }
}

/// MC's viewer search dialog: what to look for, and the four answers
/// that change how. The same struct is the open dialog and the
/// remembered search, so "search next" repeats the options too.
#[derive(Clone, Debug, Default)]
pub struct ViewSearch {
    pub field: TextField,
    pub kind: SearchKind,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub backwards: bool,
    /// Which row has the focus, indexing [`VIEW_SEARCH_ROWS`].
    pub row: usize,
}

/// The dialog's rows in display order: the field, then the answers.
pub const VIEW_SEARCH_ROWS: usize = 5;
/// The row holding the pattern itself.
pub const VIEW_SEARCH_FIELD: usize = 0;
/// The row holding the Normal / Regular expression / Hexadecimal choice.
pub const VIEW_SEARCH_KIND: usize = 1;

impl ViewSearch {
    /// The core's shape of the same question.
    pub fn to_search(&self) -> Search {
        Search {
            pattern: self.field.value.trim().to_string(),
            kind: self.kind,
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            backwards: self.backwards,
            // the dialog does not ask; the viewer's mode answers
            nroff: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.field.value.trim().is_empty()
    }

    /// One key in the dialog. `Some(true)` = Enter, search; `Some(false)`
    /// = Esc, never mind; `None` = the dialog goes on.
    pub fn key(&mut self, key: KeyEvent) -> Option<bool> {
        match key.code {
            KeyCode::Esc => return Some(false),
            KeyCode::Enter => return Some(true),
            KeyCode::Tab | KeyCode::Down => self.row = (self.row + 1) % VIEW_SEARCH_ROWS,
            KeyCode::BackTab | KeyCode::Up => {
                self.row = (self.row + VIEW_SEARCH_ROWS - 1) % VIEW_SEARCH_ROWS
            }
            KeyCode::Char(' ') if self.row != VIEW_SEARCH_FIELD => self.toggle(),
            KeyCode::Left | KeyCode::Right if self.row == VIEW_SEARCH_KIND => self.toggle(),
            _ if self.row == VIEW_SEARCH_FIELD => {
                self.field.key(key);
            }
            _ => {}
        }
        None
    }

    /// The same question as a regular expression over a line of text,
    /// which is what the editor searches: a literal pattern escaped, a
    /// hexadecimal one spelled out as the text its bytes make, whole
    /// words bounded where the pattern begins and ends in a word.
    pub fn to_regex(&self) -> Result<regex::Regex, String> {
        let text = self.field.value.trim();
        let body = match self.kind {
            SearchKind::Regex => text.to_string(),
            SearchKind::Normal => regex::escape(text),
            SearchKind::Hex => {
                let bytes: Vec<u8> = text
                    .split_whitespace()
                    .flat_map(|w| {
                        let w = w.trim_start_matches("0x");
                        (0..w.len() / 2)
                            .filter_map(move |i| u8::from_str_radix(&w[i * 2..i * 2 + 2], 16).ok())
                    })
                    .collect();
                regex::escape(&String::from_utf8_lossy(&bytes))
            }
        };
        let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        let pattern = if self.whole_word {
            let lead = if word(text.chars().next()) { r"\b" } else { "" };
            let tail = if word(text.chars().last()) { r"\b" } else { "" };
            format!("{lead}(?:{body}){tail}")
        } else {
            body
        };
        regex::RegexBuilder::new(&pattern)
            .case_insensitive(!self.case_sensitive)
            .build()
            .map_err(|err| err.to_string())
    }

    /// Space on a row: the kind cycles, the rest tick.
    pub fn toggle(&mut self) {
        match self.row {
            VIEW_SEARCH_KIND => {
                self.kind = match self.kind {
                    SearchKind::Normal => SearchKind::Regex,
                    SearchKind::Regex => SearchKind::Hex,
                    SearchKind::Hex => SearchKind::Normal,
                }
            }
            2 => self.case_sensitive = !self.case_sensitive,
            3 => self.whole_word = !self.whole_word,
            4 => self.backwards = !self.backwards,
            _ => {}
        }
    }
}

/// Rows the tree dialog shows at most - also its page step.
pub const TREE_ROWS: usize = 18;

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Help,
    Menu,
    Mark,
    QuickSearch,
    Hotlist,
    /// F9 > Command > Directory tree: the tree in a dialog.
    DirTree,
    Filter,
    UpDir,
    Enter,
    FindFile,
    /// M-/: the fuzzy finder over the tree under the panel.
    FuzzyFind,
    Panelize,
    CompareDirs,
    DirSize,
    View,
    Edit,
    Copy,
    Move,
    Mkdir,
    /// M-F5: pack what is marked into an archive of its own.
    Pack,
    /// C-x u: put the last move back.
    Undo,
    /// F9 > Command > Synchronize: compare, then copy the differences.
    Sync,
    /// `C-x f`: the named filter sets.
    Filters,
    /// Copy the marked names to the clipboard; with paths, the whole
    /// path of each.
    CopyNames {
        paths: bool,
    },
    /// Put back the marks the last operation or reload cleared.
    RestoreMarks,
    /// Size every directory in the panel, not only the cursor one.
    DirSizeAll,
    /// Give one panel the whole screen by hiding the other; again
    /// brings it back. The index is the panel to hide.
    HidePanel(usize),
    /// M-Del: overwrite, then delete.
    Wipe,
    /// C-g: run one command per marked file.
    Apply,
    /// `C-x <digit>`: the ten numbered places, which are hotlist
    /// entries whose label is that digit.
    Shortcut(u8),
    /// The files opened in the viewer or the editor, newest first.
    FileHistory,
    /// Write a sha256sum file for what is marked.
    Checksum,
    /// Check the sha256sum file under the cursor.
    VerifyChecksum,
    /// Far's M-F1 / M-F2: the list of everywhere this *named* panel can
    /// go - the open connections and archives, and what is mounted.
    Drives(usize),
    Delete,
    DeletePerm,
    SelectGroup,
    UnselectGroup,
    InvertSelection,
    Quit,
    Shell,
    SftpLink,
    HistoryBack,
    HistoryForward,
    QuickView,
    InfoView,
    UserMenu,
    /// Run `config.commands[i]` directly (per-command `key = "..."`).
    UserCommand(usize),
    Listing(ListMode),
    /// M-t, like MC: brief → full → long → brief. The tree is not in
    /// the rotation - it is entered on purpose, not stumbled into.
    ListingCycle,
    OtherSameDir,
    OtherOpenDir,
    Reload,
    SwapPanels,
    ToggleHidden,
    Options,
    /// F9 > Options > Appearance: the skin list.
    Appearance,
    /// F9 > Options > Learn keys: what the terminal really sends.
    LearnKeys,
    /// F9 > Command > Edit config file: mc's "edit extension/menu
    /// file", of which rcmd has one.
    EditConfig,
    Sort(SortKey),
    SortReverse,
    /// mc's "Mix all files": directories among the files, or first.
    SortMix,
    /// M-g / M-r / M-j: the cursor to the top, the middle or the bottom
    /// of what the panel shows, as in mc.
    ScreenTop,
    ScreenMiddle,
    ScreenBottom,
    /// C-x h: the panel's directory into the hotlist, as mc's.
    HotlistAdd,
    /// C-x r: what the last job skipped, and why, in the viewer.
    JobReport,
    /// The `trash://` panel.
    Trash,
    /// The cursor file against the last commit's version of it.
    DiffHead,
    /// M-,: panels side by side, or one above the other.
    ToggleSplit,
    /// mc's "Case sensitive" sort switch.
    SortCase,
    /// S-F4: open the editor on a file that need not exist yet.
    EditNew,
    /// S-F5 / S-F6: copy / rename the cursor file in place - the
    /// dialog prefills the bare name, targeting the same directory.
    CopyHere,
    MoveHere,
    /// C-x t / C-x p: tagged names / the panel path → command line.
    PasteTags,
    PastePath,
    /// M-c: MC's quick cd dialog.
    QuickCd,
    /// Marked names open as an editable list; the saved diff becomes
    /// renames and deletes (after a preview).
    BulkRename,
    /// The active VFS list (C-x a / F9 > Command): archives and remote
    /// connections the panels are on.
    VfsList,
    /// The running-jobs list (C-x j / F9 > Command > Jobs).
    Jobs,
    /// M-h: the command-line history as a pick list.
    HistoryList,
    /// M-H: the panel's directory history as a pick list.
    DirHistory,
    /// Shift+F3: the internal viewer without any [[view]] filter.
    ViewRaw,
    /// M-!: the viewer on the output of a command asked for now.
    FilteredView,
    /// M-`: the list of open editors and viewers.
    ScreenList,
    /// M-e: which codepage this panel's filenames are written in.
    Charset,
    /// F9 > Command > Compare files: the cursor file of each panel,
    /// side by side.
    CompareFiles,
    /// C-l: redraw the screen from scratch.
    Repaint,
}

/// None = separator line. `&` in a label marks its hotkey letter,
/// MC-style: highlighted in the dropdown, pressing it runs the entry.
pub type MenuEntry = Option<(&'static str, &'static str, Action)>;

/// One menu bar: titles and their entries, whatever the entries do.
pub type MenuBar<'a, A> = &'a [(&'a str, &'a [Option<(&'static str, &'static str, A)>])];

/// Which bar a front end drawing its own menu bar should show - see
/// [`App::menu_bar_for`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuBarFor {
    /// [`MENUS`], the panels' bar.
    Panels,
    /// [`EDIT_MENUS`], the editor's own bar, while an editor is on top.
    Editor,
}

/// MC's menu bar: the two panel menus bracket the global ones. Left and
/// Right act on their own panel whichever one has the focus, which is
/// why their entries carry no side - [`App::menu_side`] reads it off
/// the menu that is open. (With a horizontal split they are still Left
/// and Right, as in mc, and mean top and bottom.)
pub const MENUS: &[(&str, &[MenuEntry])] = &[
    ("&Left", PANEL_MENU),
    (
        "&File",
        &[
            Some(("&View", "F3", Action::View)),
            Some(("Filtered vie&w...", "M-!", Action::FilteredView)),
            Some(("&Edit", "F4", Action::Edit)),
            Some(("&Copy...", "F5", Action::Copy)),
            Some(("&Move/rename...", "F6", Action::Move)),
            Some(("&Bulk rename (editor)...", "", Action::BulkRename)),
            Some(("&Pack into archive...", "M-F5", Action::Pack)),
            Some(("Undo &last move", "C-x u", Action::Undo)),
            Some(("Ma&ke directory...", "F7", Action::Mkdir)),
            Some(("&Delete (trash)", "F8", Action::Delete)),
            Some(("Delete perma&nently", "S-F8", Action::DeletePerm)),
            // no letters left in either label that some entry above or
            // a menu title has not already taken
            Some(("Wipe (overwrite, then delete)", "M-Del", Action::Wipe)),
            Some(("Appl&y a command to each...", "C-g", Action::Apply)),
            Some(("Checksum file (sha256)...", "", Action::Checksum)),
            Some(("Check the checksum file", "", Action::VerifyChecksum)),
            None,
            Some(("Recent files (viewed, edited)", "", Action::FileHistory)),
            None,
            Some(("&Select group...", "+", Action::SelectGroup)),
            Some(("&Unselect group...", "-", Action::UnselectGroup)),
            Some(("&Invert selection", "*", Action::InvertSelection)),
            None,
            Some(("Directory si&ze", "C-spc", Action::DirSize)),
            Some(("Size every directory", "C-x spc", Action::DirSizeAll)),
            None,
            Some(("&Quit", "F10", Action::Quit)),
        ],
    ),
    (
        "&Command",
        &[
            Some(("&Help", "F1", Action::Help)),
            Some(("&User menu...", "F2", Action::UserMenu)),
            Some(("&Quick search", "C-s", Action::QuickSearch)),
            Some(("Directory ho&tlist...", "C-\\", Action::Hotlist)),
            Some(("Directory tr&ee...", "", Action::DirTree)),
            Some(("&Find file...", "M-F7", Action::FindFile)),
            Some(("Fuzzy find by &path...", "M-/", Action::FuzzyFind)),
            Some(("&Compare directories", "C-x d", Action::CompareDirs)),
            Some(("Synchroni&ze directories...", "", Action::Sync)),
            Some(("Compare fi&les", "", Action::CompareFiles)),
            Some(("Diff against HEAD", "", Action::DiffHead)),
            Some(("&Open shell", "C-o", Action::Shell)),
            Some(("S&wap panels", "C-u", Action::SwapPanels)),
            Some(("Toggle hidde&n files", "M-.", Action::ToggleHidden)),
            None,
            Some(("Other panel: &same dir", "M-i", Action::OtherSameDir)),
            Some(("Other panel: this &dir", "M-o", Action::OtherOpenDir)),
            None,
            Some(("&Jobs...", "C-x j", Action::Jobs)),
            Some(("Acti&ve VFS list...", "C-x a", Action::VfsList)),
            Some(("Tr&ash (trash://)", "", Action::Trash)),
            Some(("Command histor&y...", "M-h", Action::HistoryList)),
            Some(("Directory histo&ry...", "M-H", Action::DirHistory)),
            // mc has three of these - extension file, menu file,
            // highlighting file. rcmd has one file, so it has one entry.
            // The `g`: `f` is spent on Find file, and a second entry
            // with the same letter is one nobody can reach.
            Some(("Edit confi&g file", "", Action::EditConfig)),
            // not "&list": Compare fi&les already spends the l, and a
            // second entry with the same letter is one nobody can reach
            Some(("Screen l&ist...", "M-`", Action::ScreenList)),
        ],
    ),
    (
        "&Options",
        &[
            Some(("&Panel options...", "", Action::Options)),
            Some(("&Appearance...", "", Action::Appearance)),
            Some(("&Learn keys...", "", Action::LearnKeys)),
        ],
    ),
    ("&Right", PANEL_MENU),
];

/// Index of the panel menus in [`MENUS`] - the two that act on a named
/// side rather than on the focused panel.
pub const LEFT_MENU: usize = 0;
pub const RIGHT_MENU: usize = 4;

/// The Left and Right menus have identical entries: mc's per-panel
/// commands, in mc's order. Which panel they land on comes from which
/// menu is open. No entry here may take `f`, `c`, `o` or `r`: an entry
/// letter beats a menu title, so those would strand File, Command,
/// Options and Right - and `F9 o p` for the options form is documented.
const PANEL_MENU: &[MenuEntry] = &[
    Some(("&Brief listing", "", Action::Listing(ListMode::Brief))),
    Some(("F&ull listing", "", Action::Listing(ListMode::Full))),
    Some(("&Long listing", "", Action::Listing(ListMode::Long))),
    Some(("User &defined", "", Action::Listing(ListMode::User))),
    Some(("&Tree", "", Action::Listing(ListMode::Tree))),
    None,
    Some(("&Quick view", "C-x q", Action::QuickView)),
    Some(("&Info panel", "C-x i", Action::InfoView)),
    None,
    Some(("Sort by &name", "M-n", Action::Sort(SortKey::Name))),
    Some(("Sort by &extension", "", Action::Sort(SortKey::Ext))),
    Some(("Sort by si&ze", "", Action::Sort(SortKey::Size))),
    Some(("Sort by &modify time", "", Action::Sort(SortKey::Mtime))),
    Some(("Sort by &access time", "", Action::Sort(SortKey::Atime))),
    Some(("Sort by c&hange time", "", Action::Sort(SortKey::Ctime))),
    Some(("Sort by o&wner", "", Action::Sort(SortKey::Owner))),
    // no letter for group: the ones in the word are the menu titles' or
    // already spoken for above, and shadowing Right to save an arrow
    // key would be a bad trade
    Some(("Sort by group", "", Action::Sort(SortKey::Group))),
    // no letter either: every one in "version" is spoken for
    Some((
        "Sort by version (2 < 10)",
        "",
        Action::Sort(SortKey::Version),
    )),
    // no letter: u is the full listing's, and every other letter in
    // the label is spoken for by an entry above or a menu title
    Some(("Unsorted (as listed)", "", Action::Sort(SortKey::Unsorted))),
    Some(("Re&verse sort", "", Action::SortReverse)),
    Some(("Mi&x directories and files", "", Action::SortMix)),
    Some(("Case sensitive sort", "", Action::SortCase)),
    None,
    // "Filter" cannot take a letter of its own here: f, i, l, t, e and
    // r are all spoken for by an entry above or by a menu title, and a
    // panel-menu entry that shadows File, Command, Options or Right
    // would make that menu unreachable by letter
    Some(("&Glob filter...", "C-f", Action::Filter)),
    Some(("Filter sets...", "C-x f", Action::Filters)),
    Some(("&Panelize command...", "", Action::Panelize)),
    Some(("Re&scan", "C-r", Action::Reload)),
    Some(("Remote lin&k...", "", Action::SftpLink)),
];

/// The character after `&` in a menu label - its hotkey, lowercased.
pub fn menu_hotkey(label: &str) -> Option<char> {
    let mut chars = label.chars();
    while let Some(c) = chars.next() {
        if c == '&' {
            return chars.next().map(|c| c.to_ascii_lowercase());
        }
    }
    None
}

/// Label split at the `&` marker: (before, hotkey letter, after).
pub fn menu_label(label: &str) -> (&str, Option<char>, &str) {
    match label.split_once('&') {
        Some((pre, rest)) => {
            let mut chars = rest.chars();
            let hot = chars.next();
            (pre, hot, chars.as_str())
        }
        None => (label, None, ""),
    }
}

/// A fresh filesystem watcher for panel auto-reload; the warning is
/// set when the platform watcher cannot start.
fn build_watch() -> (Option<WatchState>, Option<String>) {
    let (tx, rx) = std::sync::mpsc::channel();
    match notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    }) {
        Ok(watcher) => (
            Some(WatchState {
                watcher,
                rx,
                watched: [None, None],
                dirty: [None, None],
                last: [None, None],
            }),
            None,
        ),
        Err(err) => (None, Some(format!("watch disabled: {err}"))),
    }
}

/// The complete key table for a config: preset + lynx state + custom
/// bindings, plus any `[[commands]]` hotkeys. Rebuilt when a toggle
/// (F9 > Options) changes the config at runtime.
fn full_keymap(config: &config::Config) -> (Keymap, Vec<String>) {
    let (contexts, mut warnings) = config.key_contexts();
    let (mut keymap, keymap_warnings) =
        keymap::build(&config.keymap, config.lynx_on(), &contexts.panel);
    warnings.extend(keymap_warnings);
    for (i, cmd) in config.commands.iter().enumerate() {
        if let Some(key) = &cmd.key {
            match keymap::parse_key(key) {
                Some(parsed) => {
                    keymap.insert(parsed, Action::UserCommand(i));
                }
                None => warnings.push(format!("bad key '{key}' for command '{}'", cmd.name)),
            }
        }
    }
    (keymap, warnings)
}

/// One open full-screen view. mc calls these screens and switches
/// between them with M-`; rcmd keeps the same word and the same list.
pub enum Screen {
    Editor(Box<EditorState>),
    Viewer(Box<Viewer>),
    Diff(Box<DiffView>),
}

impl Screen {
    /// The row the screen list shows for it: what kind it is, and what
    /// it is on.
    pub fn title(&self) -> String {
        match self {
            Screen::Editor(st) => format!(
                "Edit  {}{}",
                st.title,
                if st.ed.modified() { " [+]" } else { "" }
            ),
            Screen::Viewer(v) => format!("View  {}", v.path.display()),
            Screen::Diff(d) => format!("Diff  {} | {}", d.left.title, d.right.title),
        }
    }
}

/// mc's quick search: what has been typed, and whether anything in the
/// listing answers to it. A character that matches nothing used to be
/// swallowed - the search stayed where it was and said nothing, which
/// reads as a dropped keystroke. It is kept now, and the field says so.
pub struct QuickSearch {
    pub text: String,
    pub miss: bool,
}

pub struct MenuState {
    pub menu: usize,
    pub item: usize,
}

/// Full-screen F1 help state.
pub struct HelpState {
    pub top: usize,
    /// Content rows; updated on every draw, drives paging.
    pub rows: usize,
    /// `/` typing a search: the field.
    pub typing: Option<TextField>,
    /// What was searched for last, for `n` and for highlighting.
    pub query: String,
    pub note: Option<String>,
}

impl HelpState {
    /// Help opened at `top`.
    pub fn at(top: usize) -> HelpState {
        HelpState {
            top,
            rows: 1,
            typing: None,
            query: String::new(),
            note: None,
        }
    }
}

/// The MC-style command line at the bottom of the screen.
#[derive(Default)]
pub struct CmdLine {
    pub value: String,
    /// Cursor position in characters, not bytes.
    pub cursor: usize,
    history: Vec<String>,
    hist_pos: Option<usize>,
    saved: String,
}

impl CmdLine {
    fn take(&mut self) -> String {
        let value = self.value.trim().to_string();
        self.value.clear();
        self.cursor = 0;
        self.hist_pos = None;
        value
    }

    fn push_history(&mut self, cmd: &str) {
        // the history is written to the state file: no passwords in it
        let cmd = rcmd_core::vfslog::redact_urls(cmd);
        if self.history.last() != Some(&cmd) {
            self.history.push(cmd);
        }
        if self.history.len() > HISTORY_CAP {
            let drop = self.history.len() - HISTORY_CAP;
            self.history.drain(..drop);
        }
    }

    /// Newest last, as stored: what M-h lists and the state file keeps.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Seed from the state file at startup (oldest first).
    fn restore_history(&mut self, history: Vec<String>) {
        // an older state file may still hold a password; the next save
        // writes it out without
        self.history = history
            .iter()
            .map(|cmd| rcmd_core::vfslog::redact_urls(cmd))
            .collect();
        if self.history.len() > HISTORY_CAP {
            let drop = self.history.len() - HISTORY_CAP;
            self.history.drain(..drop);
        }
    }

    /// M-h: put a history entry on the line, ready to edit or run.
    fn set_line(&mut self, text: &str) {
        self.value = text.to_string();
        self.cursor = self.value.chars().count();
        self.hist_pos = None;
    }

    fn hist_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.hist_pos {
            None => {
                self.saved = self.value.clone();
                self.history.len() - 1
            }
            Some(p) => p.saturating_sub(1),
        };
        self.hist_pos = Some(pos);
        self.value = self.history[pos].clone();
        self.cursor = self.value.chars().count();
    }

    fn hist_next(&mut self) {
        let Some(pos) = self.hist_pos else { return };
        if pos + 1 < self.history.len() {
            self.hist_pos = Some(pos + 1);
            self.value = self.history[pos + 1].clone();
        } else {
            self.hist_pos = None;
            self.value = self.saved.clone();
        }
        self.cursor = self.value.chars().count();
    }
}

/// What the program came up as. mc's `mcedit` / `mcview` / `mcdiff`
/// are the same binary in a different personality, reached by argv[0]
/// or by `-e` / `-v`, and the personality is: one screen instead of the
/// panels, and closing it ends the session rather than landing there.
pub enum Startup {
    Panels,
    Edit(Vec<PathBuf>),
    View(PathBuf),
    Diff(PathBuf, PathBuf),
}

impl Startup {
    /// The directories the panels open on, and the name to put the
    /// cursor on in each. A file named on the command line is opened
    /// through the panel that holds it, so the openers - the `[[view]]`
    /// filters, the codepage, the size guard on a diff - are the same
    /// ones F3 and F4 go through.
    fn panels(&self, dirs: &[PathBuf]) -> Result<(Vec<PathBuf>, [Option<OsString>; 2])> {
        let split = |file: &Path| -> Result<(PathBuf, OsString)> {
            let file = std::path::absolute(file)
                .with_context(|| format!("cannot resolve {}", file.display()))?;
            let dir = file
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/"));
            let name = file.file_name().unwrap_or_default().to_os_string();
            Ok((dir, name))
        };
        Ok(match self {
            Startup::Panels => (dirs.to_vec(), [None, None]),
            // the editor is opened on the path itself (it can be a file
            // that does not exist yet), so the panel only has to be
            // somewhere sensible underneath it
            Startup::Edit(files) => match files.first() {
                Some(file) => (vec![split(file)?.0], [None, None]),
                None => (dirs.to_vec(), [None, None]),
            },
            Startup::View(file) => {
                let (dir, name) = split(file)?;
                (vec![dir], [Some(name), None])
            }
            Startup::Diff(left, right) => {
                let (left_dir, left_name) = split(left)?;
                let (right_dir, right_name) = split(right)?;
                (
                    vec![left_dir, right_dir],
                    [Some(left_name), Some(right_name)],
                )
            }
        })
    }

    pub fn is_panels(&self) -> bool {
        matches!(self, Startup::Panels)
    }
}

pub struct App {
    pub panels: [Panel; 2],
    pub table_states: [TableState; 2],
    pub active: usize,
    pub status: Option<String>,
    /// Rows visible inside a panel; updated on every draw, drives PgUp/PgDn.
    pub panel_rows: usize,
    pub dialog: Option<Dialog>,
    /// Running jobs; at most one is foreground (its dialog is modal).
    pub jobs: Vec<Job>,
    /// What `C-x u` can put back, oldest first: every move, bulk
    /// rename, F8 to the trash and restore out of it this session. Each
    /// is undone on its own and checked against the disk as it stands -
    /// other programs write to it too - so an undo never overwrites.
    undo: Vec<UndoStep>,
    /// The `trash://` panel's filesystem, made the first time it is
    /// wanted: what F6 there and an undo of F8 restore through.
    trash: Option<Arc<rcmd_core::trashcan::TrashFs>>,
    /// The full-screen things open besides the panels - mc's screens,
    /// listed behind M-`. The panels are what is underneath them all
    /// rather than one of them, which is why this can be empty.
    pub screens: Vec<Screen>,
    /// Which screen is on top; None = the panels.
    pub current: Option<usize>,
    /// The screen list (M-`) when it is open, with the row it is on.
    pub screen_list: Option<usize>,
    pub quick_view: Option<QuickView>,
    /// Ctrl+X i: which panel shows the info pane, if any (mutually
    /// exclusive with `quick_view`).
    pub info: Option<usize>,
    /// `listing_format`, parsed once at startup - the config file is
    /// read-only while rcmd runs, so the format cannot change under it.
    pub listing_format: Format,
    /// The directory-tree figure of each panel in [`ListMode::Tree`],
    /// built when the mode is entered and dropped when it is left, so
    /// the next visit starts from wherever the panel has got to.
    pub trees: [Option<Tree>; 2],
    /// Free space per side: (dir it was measured for, when, free/total
    /// bytes). Local panels only, refreshed by [`Self::disk_tick`].
    pub disk: [DiskSpace; 2],
    pub menu: Option<MenuState>,
    pub help: Option<HelpState>,
    pub cmdline: CmdLine,
    /// Quick-search prefix while Ctrl+S type-ahead is active.
    pub quick_search: Option<QuickSearch>,
    /// M-h inside a text field: that field's history as a list.
    pub field_popup: Option<FieldPopup>,
    /// The last job that skipped anything: its title, and what it left
    /// alone with why.
    pub last_report: Option<(String, Vec<(PathBuf, String)>)>,
    /// The find results a viewer or editor was opened from, which M-.
    /// and M-, walk.
    pub hit_walk: Option<HitWalk>,
    pub find: Option<FindState>,
    pub connect: Option<ConnectState>,
    /// Live remote connections by URL prefix; weak so that leaving a
    /// remote directory on both panels closes the connection.
    connections: Vec<(String, Weak<dyn RemoteFs>)>,
    remote_edit: Option<RemoteEdit>,
    du: Option<DuJob>,
    compare: Option<CompareState>,
    /// Synchronize's tree comparison, while it runs.
    sync_scan: Option<rcmd_core::sync::ScanHandle>,
    /// The plan F3 left for a diff, to come back to when it closes.
    sync_return: Option<Box<SyncDialog>>,
    /// Which `[[filter]]` sets each panel is under, in config order.
    filter_sets_on: [Vec<bool>; 2],
    /// Files the viewer and the editor have opened, newest last.
    file_history: Vec<String>,
    /// Directories still waiting to be sized, when a whole listing was
    /// asked for.
    du_queue: Vec<std::ffi::OsString>,
    /// A panel hidden outright with C-F1 / C-F2.
    hidden: Option<usize>,
    /// The marks each panel had before the last thing that cleared
    /// them, for `C-x m`.
    marks_before: [Option<std::collections::HashSet<std::ffi::OsString>>; 2],
    /// The socket other processes drive this instance through; None
    /// when it could not be opened, which is not fatal.
    remote: Option<crate::remote::Server>,
    /// Where the panels have been, this session and every one before:
    /// the hotlist's recent half is ranked by it. Merged back into the
    /// state file on the way out.
    visits: Vec<state::Visit>,
    /// What each panel was showing when its last visit was counted, so
    /// one cd counts once however many redraws follow it.
    visited: [String; 2],
    /// The comparison now running was asked for by Synchronize, so its
    /// result opens the plan instead of only marking the panels.
    compare_then_sync: bool,
    panelize: Option<PanelizeJob>,
    watch: Option<WatchState>,
    /// Something on screen has changed since the last frame. The loop
    /// wakes on a timer to poll jobs, watches and the like; drawing on
    /// every one of those wakeups repainted an idle rcmd eighteen times
    /// a second, which is a stream of escape sequences down every ssh
    /// connection for a screen that is not moving.
    dirty: bool,
    /// Ctrl+X was pressed; the next key completes the chord.
    prefix_cx: bool,
    /// C-l: clear the terminal before the next draw.
    repaint: bool,
    /// What the terminal's title was last set to, so it is written only
    /// when it changes; `None` after the terminal was handed away.
    pub title_shown: Option<String>,
    /// Finished jobs to tell the desktop about, oldest first: the
    /// terminal loop rings for them, the window build sends a notice.
    pub notices: Vec<String>,
    pub areas: Areas,
    /// Set by the drawing code; see [`DialogRows`].
    pub dialog_rows: Option<DialogRows>,
    /// Where the open form dialog drew its rows and buttons, from the
    /// last draw, for the mouse.
    pub form_hits: Vec<(Rect, FormHit)>,
    /// Last left-button press, for double-click detection.
    last_click: Option<(Instant, u16, u16)>,
    /// A lone Esc waiting for its follow-up key (MC's ESC-as-Meta
    /// prefix); resolved by the next key or a 1 s timeout.
    esc_at: Option<Instant>,
    /// Git status per panel side (dir it was computed for + result);
    /// filled by background scans, cleared when a side leaves the repo.
    pub git_info: [Option<(PathBuf, git::GitStatus)>; 2],
    /// Directory a scan was already dispatched for; None forces a rescan.
    git_seen: [Option<PathBuf>; 2],
    git_tx: std::sync::mpsc::Sender<(usize, PathBuf, Option<git::GitStatus>)>,
    git_rx: std::sync::mpsc::Receiver<(usize, PathBuf, Option<git::GitStatus>)>,
    pub config: Config,
    keymap: Keymap,
    /// Action keys inside the F3 viewer and the F4 editor; rebindable
    /// through `[keys.viewer]` / `[keys.editor]`.
    viewer_keys: keymap::ViewerMap,
    editor_keys: keymap::EditorMap,
    /// `[keys.dialog]`: keys that stand in for Enter / Esc / Tab /
    /// Shift+Tab wherever a dialog is open.
    dialog_keys: keymap::DialogMap,
    pending_exec: Option<Exec>,
    /// The persistent subshell (PLAN3 R1); None = plain exec fallback,
    /// either by `subshell = false` or because the spawn failed.
    subshell: Option<Subshell>,
    /// No real terminal ever sees the subshell's output - a window
    /// draws it from a VT parser - so the query shim has to answer
    /// DA1 and DSR during a session too, not only while the shell is
    /// hidden. fish asks before every prompt and waits for the reply.
    subshell_headless: bool,
    /// The front end draws the menu bar itself, outside the cell grid
    /// (the window build's native bar). The row `show_menubar` would
    /// take is not drawn, and F9 asks that bar to open rather than
    /// dropping the terminal build's own menu over the panels.
    external_menubar: bool,
    /// F9 was pressed with an external menu bar: the front end takes
    /// this and opens its bar. Cleared by [`Self::take_menu_request`].
    menu_requested: bool,
    /// Started as an editor/viewer/diff rather than as a file manager:
    /// there is nothing to land back on, so closing the last screen is
    /// the end of the session.
    standalone: bool,
    pub quit: bool,
}

impl App {
    pub fn new(
        dirs: &[PathBuf],
        config: Config,
        mut warnings: Vec<String>,
        startup: &Startup,
    ) -> Result<Self> {
        let cwd = std::env::current_dir().context("cannot determine current directory")?;
        let (dirs, cursors) = startup.panels(dirs)?;
        let dirs = &dirs[..];
        let dir_at = |i: usize| -> Result<PathBuf> {
            match dirs.get(i) {
                Some(dir) => std::fs::canonicalize(dir)
                    .with_context(|| format!("cannot open directory {}", dir.display())),
                None => Ok(cwd.clone()),
            }
        };
        let left_dir = dir_at(0)?;
        // With no second directory named, the right panel picks up
        // where it was left - mc's panels.ini rule. A directory that
        // has since gone away is no reason to refuse to start.
        let right_dir = if dirs.len() > 1 {
            dir_at(1)?
        } else {
            config
                .restore_other_dir
                .then(|| state::load().0.other_dir)
                .flatten()
                .map(PathBuf::from)
                .filter(|dir| dir.is_dir())
                .unwrap_or_else(|| left_dir.clone())
        };
        let mut left = Panel::new(left_dir.clone())
            .with_context(|| format!("cannot read directory {}", left_dir.display()))?;
        let mut right = Panel::new(right_dir.clone())
            .with_context(|| format!("cannot read directory {}", right_dir.display()))?;
        // each panel as it was left, where the state file says; the
        // shared settings where it does not (a first run, or a state
        // file older than per-panel looks)
        let looks = state::load().0.panels;
        for (i, panel) in [&mut left, &mut right].into_iter().enumerate() {
            match looks.get(i) {
                Some(look) => look.put_on(panel),
                None => {
                    panel.show_hidden = config.show_hidden;
                    panel.sort_key = config::sort_key_from_name(&config.sort_key);
                    panel.sort_reverse = config.sort_reverse;
                    panel.list_mode = config::list_mode_from_name(&config.listing);
                }
            }
            let _ = panel.reload();
        }
        for (panel, name) in [&mut left, &mut right].into_iter().zip(&cursors) {
            if let Some(name) = name {
                panel.select_name(name);
            }
        }
        let (keymap, keymap_warnings) = full_keymap(&config);
        warnings.extend(keymap_warnings);
        let (contexts, _) = config.key_contexts();
        let (viewer_keys, viewer_warnings) = keymap::build_viewer(&contexts.viewer);
        let (editor_keys, editor_warnings) = keymap::build_editor(&contexts.editor);
        let (dialog_keys, dialog_warnings) = keymap::build_dialog(&contexts.dialog);
        warnings.extend(viewer_warnings);
        warnings.extend(editor_warnings);
        warnings.extend(dialog_warnings);
        let watch = if config.watch {
            let (watch, warning) = build_watch();
            warnings.extend(warning);
            watch
        } else {
            None
        };
        // no subshell behind an editor or a viewer: mcedit is not a
        // file manager, and a pty nobody can reach is a process to no end
        let subshell = if config.subshell && startup.is_panels() {
            let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
            match Subshell::spawn(&left.local_cwd(), cols, rows) {
                Ok(sub) => Some(sub),
                Err(err) => {
                    warnings.push(format!("subshell disabled: {err}"));
                    None
                }
            }
        } else {
            None
        };
        let (listing_format, format_warnings) = format::parse(&config.listing_format);
        warnings.extend(format_warnings);
        warnings.extend(crate::ui::init_highlight(&config.highlight));
        let status = if warnings.is_empty() {
            None
        } else {
            Some(format!(" {} ", warnings.join(" · ")))
        };
        // a panel that starts in tree mode (`listing = "tree"`) needs
        // its figure before the first draw
        let trees = [&left, &right].map(|panel| {
            (panel.list_mode == ListMode::Tree)
                .then(|| Tree::new(&panel.local_cwd(), panel.show_hidden))
        });
        let (git_tx, git_rx) = std::sync::mpsc::channel();
        // command history survives sessions (it lives in the state file)
        let mut cmdline = CmdLine::default();
        cmdline.restore_history(state::load().0.cmd_history);
        Ok(App {
            panels: [left, right],
            table_states: [TableState::default(), TableState::default()],
            active: 0,
            status,
            panel_rows: 1,
            dialog: None,
            jobs: Vec::new(),
            undo: Vec::new(),
            trash: None,
            filter_sets_on: [Vec::new(), Vec::new()],
            file_history: state::load().0.file_history,
            du_queue: Vec::new(),
            hidden: None,
            marks_before: [None, None],
            remote: None,
            visits: state::load().0.visits,
            visited: [String::new(), String::new()],
            compare_then_sync: false,
            sync_scan: None,
            sync_return: None,
            screens: Vec::new(),
            current: None,
            screen_list: None,
            quick_view: None,
            info: None,
            listing_format,
            trees,
            disk: [None, None],
            menu: None,
            help: None,
            cmdline,
            quick_search: None,
            field_popup: None,
            last_report: None,
            hit_walk: None,
            find: None,
            connect: None,
            connections: Vec::new(),
            remote_edit: None,
            du: None,
            compare: None,
            panelize: None,
            watch,
            dirty: true,
            prefix_cx: false,
            repaint: false,
            title_shown: None,
            notices: Vec::new(),
            areas: Areas::default(),
            dialog_rows: None,
            form_hits: Vec::new(),
            last_click: None,
            esc_at: None,
            git_info: [None, None],
            git_seen: [None, None],
            git_tx,
            git_rx,
            config,
            keymap,
            viewer_keys,
            editor_keys,
            dialog_keys,
            pending_exec: None,
            subshell,
            subshell_headless: false,
            external_menubar: false,
            menu_requested: false,
            standalone: !startup.is_panels(),
            quit: false,
        })
    }

    /// Open what the command line asked to come up on. Fails rather
    /// than dropping the user into the panels: `rcedit binary-file`
    /// that quietly turned into a file manager would be worse than an
    /// error message.
    pub fn open_startup(&mut self, startup: Startup) -> Result<()> {
        match startup {
            Startup::Panels => return Ok(()),
            Startup::Edit(files) => {
                for file in &files {
                    let title = file.display().to_string();
                    self.open_internal_editor(file, title);
                }
                // several files open as several screens, and the one in
                // front is the first named, not the last opened
                if !self.screens.is_empty() {
                    self.current = Some(0);
                }
            }
            Startup::View(_) => self.open_viewer(false),
            Startup::Diff(_, _) => self.open_diff(),
        }
        if self.screens.is_empty() {
            let why = self.status.take().unwrap_or_default();
            anyhow::bail!("{}", why.trim().trim_matches('-').trim());
        }
        Ok(())
    }

    /// The per-frame background work: drain the channels the worker
    /// threads report through, retire an abandoned ESC prefix, and say
    /// whether anything is still moving (which is what decides between
    /// a lazy and a busy poll timeout).
    ///
    /// Split out of [`Self::run`] so a front end that does not own its
    /// event loop can do the same work: `rcmd-egui` calls this once per
    /// egui frame and then draws the same [`ui::draw`] into a window.
    pub fn tick(&mut self) -> bool {
        self.note_visits();
        self.drain_remote();
        self.drain_job();
        self.drain_find();
        let fuzzy = self.drain_fuzzy();
        self.drain_connect();
        self.drain_du();
        self.drain_compare();
        self.drain_sync_scan();
        if self.poll_diff() {
            self.dirty = true;
        }
        self.drain_panelize();
        self.poll_loads();
        self.update_watches();
        self.tick_watch();
        self.follow_tick();
        self.update_quick_view();
        self.git_tick();
        self.disk_tick();
        self.subshell_tick();
        // an abandoned ESC prefix becomes a real Escape, like MC
        if let Some(at) = self.esc_at
            && at.elapsed() >= Duration::from_millis(self.config.esc_timeout_ms)
        {
            self.esc_at = None;
            self.dirty = true;
            self.dispatch_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
        let loading = self.panels.iter().any(Panel::is_loading);
        // a change waiting on a dialog to close is not something
        // to spin over: it cannot be acted on until the dialog goes
        let watch_pending = self.watch_can_fire()
            && self
                .watch
                .as_ref()
                .is_some_and(|w| w.dirty.iter().any(Option::is_some));
        // "busy" is anything that moves on its own: a job's
        // progress, a listing still arriving, a followed file
        !self.jobs.is_empty()
            || self.compare.is_some()
            || self.sync_scan.is_some()
            || self.panelize.is_some()
            || self.find.is_some()
            || (fuzzy && matches!(&self.dialog, Some(Dialog::Fuzzy(d)) if d.walking.is_some()))
            || self.connect.is_some()
            || self.du.is_some()
            || loading
            || watch_pending
            || self.esc_at.is_some()
            || self.subshell.as_ref().is_some_and(|s| !s.ready())
            || self.viewer().is_some_and(|v| v.follow)
            || self.diff().is_some_and(DiffView::is_pending)
    }

    /// The session is over: quit was asked for, or the last screen of a
    /// standalone editor/viewer/diff closed with nothing underneath it.
    pub fn exiting(&self) -> bool {
        self.quit || (self.standalone && self.screens.is_empty())
    }

    /// Something on screen changed since the last frame.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Say a frame is owed - a front end calls this when an event
    /// arrives, whatever the event turns out to be.
    pub fn set_dirty(&mut self) {
        self.dirty = true;
    }

    /// A frame has been drawn.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// C-l asked for the screen to be thrown away before the next draw;
    /// taking it clears it.
    pub fn take_repaint(&mut self) -> bool {
        std::mem::take(&mut self.repaint)
    }

    /// The command a key press queued, if any. What running it means is
    /// the front end's: a terminal hands over the tty, a window cannot.
    pub fn take_exec(&mut self) -> Option<Exec> {
        self.pending_exec.take()
    }

    /// The front end has no terminal behind the subshell: whatever the
    /// shell writes goes to a parser that never answers a query, so
    /// the shim keeps answering while a session is on screen.
    pub fn set_subshell_headless(&mut self) {
        self.subshell_headless = true;
    }

    /// The front end draws the menu bar itself, above the grid: the
    /// terminal build's bar row and its dropdowns stay off, and F9
    /// becomes a request the front end reads with
    /// [`Self::take_menu_request`].
    pub fn set_external_menubar(&mut self) {
        self.external_menubar = true;
    }

    pub fn external_menubar(&self) -> bool {
        self.external_menubar
    }

    /// F9 since the last call, with an external menu bar; taking it
    /// clears it.
    pub fn take_menu_request(&mut self) -> bool {
        std::mem::take(&mut self.menu_requested)
    }

    /// Whether an F9 is waiting to be taken.
    pub fn menu_requested(&self) -> bool {
        self.menu_requested
    }

    /// Which menu bar an external one should show, and whether it
    /// may act: the panels' bar is greyed under a dialog, a job, the
    /// help or a viewer, the editor's under one of the editor's own
    /// prompts. What is on top takes the keys, and a menu entry run
    /// from under it would act on a screen nobody is looking at.
    pub fn menu_bar_for(&self) -> (MenuBarFor, bool) {
        if self.editor().is_some() {
            let busy = self.fg_job().is_some()
                || self.dialog.is_some()
                || self.screen_list.is_some()
                || self.field_popup.is_some()
                || self
                    .editor()
                    .is_some_and(|st| st.prompt.is_some() || st.menu.is_some());
            return (MenuBarFor::Editor, !busy);
        }
        let busy = self.fg_job().is_some()
            || self.connect.is_some()
            || self.find.is_some()
            || self.dialog.is_some()
            || self.help.is_some()
            || self.screen_list.is_some()
            || self.field_popup.is_some()
            || self.viewer().is_some()
            || self.diff().is_some()
            || self.menu.is_some()
            || self.quick_search.is_some();
        (MenuBarFor::Panels, !busy)
    }

    /// An entry of [`MENUS`] chosen from an external menu bar - the
    /// same call the terminal build's dropdown makes on Enter, so the
    /// Left and Right menus land on their own panel either way.
    pub fn run_menu_entry(&mut self, menu: usize, action: Action) {
        self.run_menu_action(menu, action);
    }

    /// An entry of [`EDIT_MENUS`] chosen from an external menu bar.
    pub fn run_edit_menu_entry(&mut self, action: EditMenuAction) {
        self.run_edit_menu_action(action);
    }

    /// Tell the subshell how big its terminal now is.
    pub fn resize_subshell(&mut self, cols: u16, rows: u16) {
        if let Some(sub) = self.subshell.as_mut() {
            sub.resize(cols, rows);
        }
    }

    /// Quitting with jobs still running would orphan them, so the quit
    /// is refused and says why.
    pub fn hold_quit_for_jobs(&mut self) {
        if self.quit && !self.jobs.is_empty() {
            self.quit = false;
            self.status = Some(format!(
                " {} job(s) still running - C-x j lists them (Esc/c cancels) ",
                self.jobs.len()
            ));
        }
    }

    /// On the way out: nothing started here outlives the session.
    pub fn cancel_background(&mut self) {
        for job in &self.jobs {
            job.handle.cancel();
        }
        if let Some(find) = &self.find {
            find.handle.cancel();
        }
    }

    /// What the window or terminal title says: the program, and where
    /// the active panel is - and while jobs run, how far along they are,
    /// which is what a title is looked at for from another window.
    pub fn title(&self) -> String {
        let place = self.panels[self.active].display_path();
        match self.jobs_percent() {
            Some(pct) => format!("[{pct}%] rcmd - {place}"),
            None => format!("rcmd - {place}"),
        }
    }

    /// How far the running jobs are, together: by bytes where they
    /// count bytes, by items where they do not. `None` with none
    /// running.
    pub fn jobs_percent(&self) -> Option<u64> {
        let running = self.jobs.iter().filter(|j| !j.handle.is_held());
        let (mut done, mut total) = (0u64, 0u64);
        for job in running {
            let (d, t) = match job.total_bytes {
                0 => (job.files_done, job.total_files),
                bytes => (job.bytes_done, bytes),
            };
            // a job still counting has no total yet: it is at 0%
            let t = t.max(1);
            done += d.min(t) * 1000 / t;
            total += 1000;
        }
        (total > 0).then(|| done * 100 / total)
    }

    /// Ring for the jobs that finished since the last frame: the bell
    /// always - it reaches the user through tmux and ssh - and a
    /// desktop notice in the terminals known to pass one on.
    fn ring_notices(&mut self) {
        if self.notices.is_empty() {
            return;
        }
        use std::io::Write;
        let mut out = std::io::stdout();
        for notice in std::mem::take(&mut self.notices) {
            let _ = write!(out, "\x07{}", notice_escape(&notice));
        }
        let _ = out.flush();
    }

    /// Set the terminal's title when the place it names has changed.
    fn update_title(&mut self) {
        if !self.config.terminal_title {
            return;
        }
        let title = self.title();
        if self.title_shown.as_deref() != Some(title.as_str()) {
            let _ = ratatui::crossterm::execute!(
                std::io::stdout(),
                ratatui::crossterm::terminal::SetTitle(&title)
            );
            self.title_shown = Some(title);
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut last_frame = Instant::now();
        while !self.exiting() {
            let busy = self.tick();
            // ...and a frame is drawn when something changed, when
            // something is moving, or once in a while regardless - the
            // last of those is insurance against a state change that
            // forgot to say so, and at one frame every two seconds it
            // costs nothing.
            if self.dirty || busy || last_frame.elapsed() >= IDLE_FRAME {
                if self.take_repaint() {
                    terminal.clear()?;
                }
                terminal.draw(|frame| ui::draw(frame, self))?;
                self.update_title();
                self.ring_notices();
                self.dirty = false;
                last_frame = Instant::now();
            }
            let timeout = if busy {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(500)
            };
            if event::poll(timeout)? {
                // whatever the event turns out to be, the screen may
                // answer it
                self.dirty = true;
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
                    Event::Paste(text) => self.on_paste(&text),
                    Event::Mouse(mouse) => self.on_mouse(mouse),
                    Event::Resize(cols, rows) => self.resize_subshell(cols, rows),
                    _ => {}
                }
            }
            if let Some(exec) = self.take_exec() {
                if self.subshell.is_some() && !matches!(exec, Exec::Quiet(_)) {
                    // Ctrl+O and typed commands live in the subshell;
                    // Quiet (editors, openers) stays a one-shot child
                    self.subshell_session(terminal, exec)?;
                } else {
                    self.execute(terminal, exec)?;
                    self.finish_remote_edit();
                }
            }
            self.hold_quit_for_jobs();
        }
        self.cancel_background();
        Ok(())
    }

    fn poll_loads(&mut self) {
        for i in 0..2 {
            match self.panels[i].poll_pending() {
                Some(Err(err)) => {
                    self.status = Some(format!(" {err} "));
                    self.dirty = true;
                }
                Some(Ok(())) => self.dirty = true,
                None => {}
            }
        }
    }

    fn drain_du(&mut self) {
        let Some(du) = self.du.as_ref() else { return };
        match du.rx.try_recv() {
            Ok((files, bytes)) => {
                let du = self.du.take().expect("du present");
                let panel = &mut self.panels[du.panel];
                if panel.cwd == du.cwd
                    && let Some(entry) = panel.entries.iter_mut().find(|e| e.name == du.name)
                {
                    entry.size = bytes;
                }
                self.status = Some(match self.du_queue.len() {
                    0 => format!(
                        " {}: {bytes} bytes in {files} file(s) ",
                        du.name.to_string_lossy()
                    ),
                    left => format!(" sizing… {left} to go "),
                });
                // ...and on to the next one, where a whole listing was
                // asked for
                if !self.du_queue.is_empty() {
                    let next = self.du_queue.remove(0);
                    let was = self.active;
                    self.active = du.panel;
                    self.start_du(next);
                    self.active = was;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.status = Some(" sizing… ".into());
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.du = None,
        }
    }

    /// Watch the panels' current directories; rewire on cd.
    fn update_watches(&mut self) {
        let Some(watch) = self.watch.as_mut() else {
            return;
        };
        for i in 0..2 {
            let desired = {
                let panel = &self.panels[i];
                (panel.is_local() && panel.panelized.is_none()).then(|| panel.cwd.clone())
            };
            if desired != watch.watched[i] {
                if let Some(old) = &watch.watched[i] {
                    let _ = watch.watcher.unwatch(old);
                }
                if let Some(new) = &desired {
                    let _ = watch
                        .watcher
                        .watch(new, notify::RecursiveMode::NonRecursive);
                }
                watch.watched[i] = desired;
                watch.dirty[i] = None;
                watch.last[i] = None;
            }
        }
    }

    /// Debounced auto-reload: fire after 250 ms of quiet, or at the
    /// latest 2 s after the first event of a burst.
    /// Whether a pending directory reload could fire right now. While
    /// a dialog is up, the listing underneath is left alone - and
    /// until it can be reloaded, the pending flag must not keep the
    /// event loop awake either.
    fn watch_can_fire(&self) -> bool {
        self.fg_job().is_none()
            && self.find.is_none()
            && self.dialog.is_none()
            && self.quick_search.is_none()
    }

    fn tick_watch(&mut self) {
        use std::time::Instant;
        let can_fire = self.watch_can_fire();
        let Some(watch) = self.watch.as_mut() else {
            return;
        };
        while let Ok(Ok(event)) = watch.rx.try_recv() {
            for i in 0..2 {
                if let Some(dir) = &watch.watched[i]
                    && event
                        .paths
                        .iter()
                        .any(|p| p.parent() == Some(dir) || p == dir)
                {
                    let now = Instant::now();
                    watch.dirty[i].get_or_insert(now);
                    watch.last[i] = Some(now);
                }
            }
        }
        if !can_fire {
            return;
        }
        for i in 0..2 {
            let fire = match (watch.dirty[i], watch.last[i]) {
                (Some(first), Some(last)) => {
                    last.elapsed() >= Duration::from_millis(250)
                        || first.elapsed() >= Duration::from_secs(2)
                }
                _ => false,
            };
            if fire && !self.panels[i].is_loading() {
                watch.dirty[i] = None;
                watch.last[i] = None;
                let cwd = self.panels[i].cwd.clone();
                let _ = self.panels[i].request_dir(cwd, LoadKind::Reload);
                self.git_seen[i] = None;
            }
        }
    }
}
impl App {
    /// The job whose dialog is on screen (modal); background jobs run
    /// without one until they finish or need an answer.
    /// The subshell's own prompt for the command line - only while the
    /// shell stands where the panel does, or it would name a directory
    /// the panel has left (the two sync before the next command).
    pub fn subshell_prompt(&mut self) -> Option<String> {
        let here = self.panels[self.active].local_cwd();
        let sub = self.subshell.as_mut()?;
        (sub.cwd() == here).then(|| sub.prompt()).flatten()
    }

    /// A tree panel looks again around its selection after something
    /// changed the directories under it.
    pub(super) fn refresh_trees(&mut self) {
        for side in [0, 1] {
            if self.panels[side].list_mode == ListMode::Tree
                && let Some(tree) = self.trees[side].as_mut()
            {
                tree.refresh();
            }
        }
    }

    pub fn fg_job(&self) -> Option<&Job> {
        self.jobs.iter().find(|j| !j.background)
    }

    fn fg_job_mut(&mut self) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| !j.background)
    }

    /// Let each queued job begin whose device nothing ahead of it is
    /// writing to - in the order they were queued, so the first one
    /// released keeps the rest waiting.
    fn release_queued(&mut self) {
        for i in 0..self.jobs.len() {
            if !self.jobs[i].handle.is_held() {
                continue;
            }
            let device = &self.jobs[i].device;
            let busy =
                device.is_some()
                    && self.jobs.iter().enumerate().any(|(j, other)| {
                        j != i && !other.handle.is_held() && other.device == *device
                    });
            if !busy {
                self.jobs[i].handle.release();
            }
        }
    }

    fn drain_job(&mut self) {
        self.release_queued();
        let confirm_overwrite = self.config.confirm_overwrite;
        let mut any_done = false;
        let mut i = 0;
        while i < self.jobs.len() {
            let job = &mut self.jobs[i];
            let mut done = None;
            while let Ok(event) = job.handle.events.try_recv() {
                match event {
                    JobEvent::Total { files, bytes } => {
                        job.total_files = files;
                        job.total_bytes = bytes;
                    }
                    JobEvent::Progress {
                        files_done,
                        bytes_done,
                        current,
                        file_done,
                        file_total,
                    } => {
                        job.files_done = files_done;
                        job.bytes_done = bytes_done;
                        job.current = current;
                        job.file_done = file_done;
                        job.file_total = file_total;
                        job.sample_rate(bytes_done);
                    }
                    JobEvent::AskOverwrite {
                        path,
                        src,
                        dst,
                        can_append,
                    } => {
                        if !confirm_overwrite {
                            // the user turned the question off: answer it
                            // once, for every remaining file in this job
                            let _ = job.handle.replies.send(Reply::OverwriteAll);
                            continue;
                        }
                        job.ask = Some(Ask::Overwrite {
                            path,
                            src,
                            dst,
                            can_append,
                        });
                        job.button = 0;
                        // a question pulls a background job back up
                        job.background = false;
                    }
                    JobEvent::Moved { from, to } => job.moved.push((from, to)),
                    JobEvent::Trashed { path } => job.trashed.push(path),
                    JobEvent::Restored { path } => job.restored.push(path),
                    JobEvent::AskError { path, message } => {
                        job.ask = Some(Ask::Error { path, message });
                        job.button = 0;
                        job.background = false;
                    }
                    JobEvent::Skipped { path, reason } => {
                        // a report is for reading: past this it is noise
                        if job.skips.len() < 10_000 {
                            job.skips.push((path, reason));
                        }
                    }
                    JobEvent::Done {
                        files_done,
                        skipped,
                        aborted,
                    } => done = Some((files_done, skipped, aborted)),
                }
            }
            let Some((files_done, skipped, aborted)) = done else {
                i += 1;
                continue;
            };
            let mut job = self.jobs.remove(i);
            if let Some(thread) = job.handle.thread.take() {
                let _ = thread.join();
            }
            if !aborted {
                self.remember_marks(job.src_panel);
                self.panels[job.src_panel].marked.clear();
            }
            // what the job changed goes on the undo stack; a job that
            // changed nothing leaves it as it was
            if !job.moved.is_empty() {
                self.push_undo(UndoStep::Moved(std::mem::take(&mut job.moved)));
            }
            if !job.trashed.is_empty() {
                self.push_undo(UndoStep::Trashed(std::mem::take(&mut job.trashed)));
            }
            if !job.restored.is_empty() {
                self.push_undo(UndoStep::Restored(std::mem::take(&mut job.restored)));
            }
            any_done = true;
            if !job.skips.is_empty() {
                self.last_report = Some((job.title.clone(), std::mem::take(&mut job.skips)));
            }
            let report = if self.last_report.is_some() && skipped > 0 {
                " - C-x r says why"
            } else {
                ""
            };
            self.status = Some(match (job.checking, aborted, skipped) {
                (true, _, 0) => format!(" checked: {files_done} matched "),
                (true, _, bad) => {
                    format!(" checked: {files_done} matched, {bad} did NOT ")
                }
                (false, true, _) => format!(" aborted - {files_done} item(s) processed "),
                (false, false, 0) => format!(" done - {files_done} item(s) processed "),
                (false, false, n) => {
                    format!(" done - {files_done} item(s) processed, {n} skipped{report} ")
                }
            });
            // a job watched to the end needs no one to say it ended
            if self.config.notify_done
                && (job.background || job.started.elapsed() >= NOTIFY_AFTER)
                && let Some(status) = &self.status
            {
                self.notices
                    .push(format!("{} - {}", job.title.trim(), status.trim()));
            }
        }
        if any_done {
            for panel in &mut self.panels {
                let _ = panel.refresh();
            }
            self.refresh_trees();
            self.git_refresh();
            if self.jobs.is_empty() && matches!(self.dialog, Some(Dialog::Jobs(_))) {
                self.dialog = None;
            }
        }
    }

    /// MC's ESC-as-Meta prefix, for terminals without working F-keys or
    /// Alt: a lone Esc waits for a follow-up key - a digit becomes an
    /// F-key (Esc 1 = F1 … Esc 0 = F10), anything else gets Alt added
    /// (Esc t = Alt+T, Esc Enter = Alt+Enter), and Esc Esc is a real
    /// Escape. Fast Esc+key already arrives as Alt from the terminal;
    /// this handles the deliberate, slow-typed form.
    pub fn on_key(&mut self, key: KeyEvent) {
        if self.esc_at.take().is_some() {
            match key.code {
                KeyCode::Char(c @ '0'..='9') if key.modifiers.is_empty() => {
                    let n = if c == '0' { 10 } else { c as u8 - b'0' };
                    self.on_key(KeyEvent::new(KeyCode::F(n), KeyModifiers::NONE));
                    return;
                }
                KeyCode::Esc => {} // deliberate double-Esc: a real Escape
                _ => {
                    self.on_key(KeyEvent::new(key.code, key.modifiers | KeyModifiers::ALT));
                    return;
                }
            }
        } else if key.code == KeyCode::Esc {
            self.esc_at = Some(Instant::now());
            self.status = Some(" ESC-  (1..0 = F1..F10, key = Alt+key, Esc = Esc) ".into());
            return;
        }
        self.dispatch_key(key);
    }

    fn dispatch_key(&mut self, key: KeyEvent) {
        self.status = None;
        if self.screen_list.is_some() {
            self.on_screen_list_key(key);
            return;
        }
        if self.field_popup.is_some() {
            self.on_field_popup_key(key);
            return;
        }
        // help over a dialog, an editor or a viewer takes the keys until
        // it closes, and closing it lands back where F1 was pressed
        if self.help.is_some() {
            self.on_help_key(key);
            return;
        }
        if key.code == KeyCode::F(1) && self.help_here() {
            return;
        }
        // mc's M-h inside a field: its history as a list to pick from,
        // rather than M-p pressed until the right one comes round
        if key.code == KeyCode::Char('h')
            && key.modifiers == KeyModifiers::ALT
            && self.open_field_popup()
        {
            return;
        }
        // M-Tab completes in any field, as it does on the command line
        // (which the panels handle themselves, Tab included)
        if key.code == KeyCode::Tab
            && key.modifiers == KeyModifiers::ALT
            && !self.on_panels()
            && self.complete_focused(false)
        {
            return;
        }
        // M-` reaches the list from wherever you are - that is the
        // point of it - but not out from under a modal question
        if key.code == KeyCode::Char('`')
            && key.modifiers.contains(KeyModifiers::ALT)
            && self.fg_job().is_none()
            && self.connect.is_none()
            && self.find.is_none()
            && self.dialog.is_none()
            && self.help.is_none()
            && !self
                .editor()
                .is_some_and(|st| st.prompt.is_some() || st.menu.is_some())
            && !self
                .viewer()
                .is_some_and(|v| v.prompt.is_some() || v.goto.is_some() || v.confirm_quit.is_some())
        {
            self.open_screen_list();
            return;
        }
        if self.fg_job().is_some() {
            self.on_job_key(key);
        } else if self.connect.is_some() {
            self.on_connect_key(key);
        } else if self.find.is_some() {
            self.on_find_key(key);
        } else if self.dialog.is_some() {
            self.on_dialog_key(key);
        } else if self.help.is_some() {
            self.on_help_key(key);
        } else if self.editor().is_some() {
            self.on_editor_key(key);
        } else if self.viewer().is_some() {
            self.on_viewer_key(key);
        } else if self.diff().is_some() {
            self.on_diff_key(key);
        } else if self.menu.is_some() {
            self.on_menu_key(key);
        } else if self.quick_search.is_some() {
            self.on_quick_search_key(key);
        } else {
            self.on_panel_key(key);
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let now = Instant::now();
                let double = self.last_click.is_some_and(|(at, x, y)| {
                    x == mouse.column
                        && y == mouse.row
                        && now.duration_since(at) < Duration::from_millis(500)
                });
                self.last_click = if double {
                    None
                } else {
                    Some((now, mouse.column, mouse.row))
                };
                self.on_click(mouse.column, mouse.row, double);
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.on_right_click(mouse.column, mouse.row)
            }
            MouseEventKind::ScrollUp => self.on_wheel(mouse.column, mouse.row, -3),
            MouseEventKind::ScrollDown => self.on_wheel(mouse.column, mouse.row, 3),
            _ => {}
        }
    }

    fn on_click(&mut self, x: u16, y: u16, double: bool) {
        // A form dialog takes a click on a field, a switch or a button;
        // a list dialog on one of its rows.
        if self.dialog.is_some() {
            if !self.click_form(x, y) {
                self.click_dialog_row(x, y, double);
            }
            return;
        }
        // Other prompts stay keyboard-only; the menu is the exception.
        if self.fg_job().is_some()
            || self.connect.is_some()
            || self.find.is_some()
            || self.help.is_some()
            || self.viewer().is_some()
        {
            return;
        }
        if let Some(st) = self.editor_mut() {
            // the gutter is not text: a click in it lands on column 0
            let x = (x as usize).saturating_sub(st.gutter) as u16;
            if st.prompt.is_none() && y >= 1 && (y as usize) <= st.rows {
                let (line, col) = if st.wrap {
                    // walk visual rows down from the top to this row
                    let cols = st.wrap_width();
                    let (mut line, mut seg) = (st.top, st.top_seg);
                    for _ in 0..(y as usize - 1) {
                        seg += 1;
                        if seg >= ui::ed_line_segs(&st.ed, line, cols) {
                            if line + 1 >= st.ed.line_count() {
                                break;
                            }
                            line += 1;
                            seg = 0;
                        }
                    }
                    let line = line.min(st.ed.line_count().saturating_sub(1));
                    (
                        line,
                        col_at_screen(&st.ed.line(line), seg * cols + x as usize),
                    )
                } else {
                    let line = (st.top + y as usize - 1).min(st.ed.line_count().saturating_sub(1));
                    (line, col_at_screen(&st.ed.line(line), st.left + x as usize))
                };
                st.ed.goto(rcmd_edit::Pos { line, col }, false);
            }
            return;
        }
        if self.menu.is_some() {
            self.menu_click(x, y);
            return;
        }
        self.quick_search = None;
        self.prefix_cx = false;
        let pos = Position { x, y };
        if self.areas.menubar.height > 0 && self.areas.menubar.contains(pos) {
            self.menu = Some(MenuState {
                menu: 0,
                item: first_menu_item(MENUS[0].1),
            });
            self.menu_click(x, y);
            return;
        }
        if self.areas.keybar.contains(pos) {
            // ten boxes across the width → F1..F10
            let rel = x - self.areas.keybar.x;
            let n = (1..=10)
                .find(|&i| rel < crate::ui::keybar_box(self.areas.keybar.width, i))
                .unwrap_or(10) as u8;
            self.on_key(KeyEvent::new(KeyCode::F(n), KeyModifiers::NONE));
            return;
        }
        for side in [0, 1] {
            let area = if side == 0 {
                self.areas.left
            } else {
                self.areas.right
            };
            if area.contains(pos) {
                self.panel_click(side, area, x, y, double);
                return;
            }
        }
    }

    /// Whether panel `side` carries the framed row above its bottom
    /// edge: every panel with the mini status on, else the active one
    /// when the status line is - that row *is* the status line, drawn
    /// where mc draws its mini status rather than loose under the
    /// panels.
    pub fn panel_mini(&self, side: usize) -> bool {
        self.config.show_mini_status || (self.config.show_status && self.active == side)
    }

    fn panel_click(&mut self, side: usize, area: Rect, x: u16, y: u16, double: bool) {
        self.active = side;
        if self.quick_view.as_ref().is_some_and(|q| q.side == side) || self.info == Some(side) {
            return;
        }
        // The tree has no header row and scrolls itself, so a click in
        // one maps through the figure's own visible window.
        if self.panels[side].list_mode == ListMode::Tree {
            let top = area.y + 1;
            let height = area
                .height
                .saturating_sub(2 + crate::ui::MINI_STATUS_ROWS * u16::from(self.panel_mini(side)));
            if y < top || y >= top + height {
                return;
            }
            let row = (y - top) as usize;
            if let Some(tree) = self.trees[side].as_mut() {
                let first = tree.first_visible(height as usize);
                tree.select_row(first + row);
            }
            if double {
                self.tree_enter();
            }
            return;
        }
        if y == area.y + 1 {
            self.header_click(side, area, x);
            return;
        }
        if let Some(index) = self.entry_at(side, area, x, y) {
            self.panels[side].cursor = index;
            if double {
                self.enter_or_open();
            }
        }
    }

    /// Which entry of panel `side` is drawn at (x, y), for a listing
    /// (the tree maps its own clicks).
    fn entry_at(&self, side: usize, area: Rect, x: u16, y: u16) -> Option<usize> {
        // 2 border+header rows on top, 1 border row at the bottom
        let content_y = area.y + 2;
        if y < content_y || y + 1 >= area.y + area.height {
            return None;
        }
        let row = (y - content_y) as usize;
        let offset = self.table_states[side].offset();
        // a brief listing fills column by column, so the x tells us
        // which column was clicked
        let columns = match self.panels[side].list_mode {
            ListMode::User => self.listing_format.repeat.max(1),
            _ => self.config.columns(),
        };
        let index = if matches!(
            self.panels[side].list_mode,
            ListMode::Brief | ListMode::User
        ) && columns > 1
        {
            let inner_w = area.width.saturating_sub(2).max(1);
            let col_w = (inner_w / columns).max(1);
            let col = (x.saturating_sub(area.x + 1) / col_w).min(columns - 1) as usize;
            let rows = area
                .height
                .saturating_sub(3 + crate::ui::MINI_STATUS_ROWS * u16::from(self.panel_mini(side)))
                .max(1) as usize;
            offset + col * rows + row
        } else {
            offset + row
        };
        (index < self.panels[side].entries.len()).then_some(index)
    }

    /// The right button marks what it is on, as mc's does - the cursor
    /// goes there too, so what was marked is plain to see.
    fn on_right_click(&mut self, x: u16, y: u16) {
        if !self.on_panels() {
            return;
        }
        let pos = Position { x, y };
        for side in [0, 1] {
            let area = if side == 0 {
                self.areas.left
            } else {
                self.areas.right
            };
            if !area.contains(pos) || self.panels[side].list_mode == ListMode::Tree {
                continue;
            }
            if let Some(index) = self.entry_at(side, area, x, y) {
                self.active = side;
                self.panels[side].cursor = index;
                self.panels[side].toggle_mark();
            }
        }
    }

    /// Click on the column-header row: sort by that column, a second
    /// click reverses - mirroring the F9 > Sort menu. Column x-ranges
    /// re-derive the table layout (fixed widths + 1 spacing).
    fn header_click(&mut self, side: usize, area: Rect, x: u16) {
        let inner_w = area.width.saturating_sub(2) as usize;
        let rel = x.saturating_sub(area.x + 1) as usize;
        if rel >= inner_w {
            return;
        }
        // a user-defined format sorts by whichever field was clicked,
        // through the same layout the renderer used
        if self.panels[side].list_mode == ListMode::User {
            let sets = self.listing_format.repeat.max(1);
            let set_width = (area.width.saturating_sub(2) / sets).max(1);
            let mut x = rel as u16 % set_width.max(1);
            let mut key = None;
            for (item, width) in self.listing_format.layout(set_width) {
                if x < width {
                    key = match item {
                        Item::Field(Field::Name, _) => Some(SortKey::Name),
                        Item::Field(Field::Size | Field::BSize, _) => Some(SortKey::Size),
                        Item::Field(Field::Mtime, _) => Some(SortKey::Mtime),
                        Item::Field(Field::Atime, _) => Some(SortKey::Atime),
                        Item::Field(Field::Ctime, _) => Some(SortKey::Ctime),
                        Item::Field(Field::Owner, _) => Some(SortKey::Owner),
                        Item::Field(Field::Group, _) => Some(SortKey::Group),
                        _ => None,
                    };
                    break;
                }
                // +1 for the gap the renderer puts between columns
                x = x.saturating_sub(width + 1);
            }
            if let Some(key) = key {
                self.panels[side].set_sort(key);
            }
            return;
        }
        let panel = &mut self.panels[side];
        let key = match panel.list_mode {
            // the tree draws no header row, and a user format was
            // handled above; neither reaches here
            ListMode::Tree | ListMode::User => None,
            ListMode::Brief => Some(SortKey::Name),
            ListMode::Full => {
                // [Name (fill), Size 7, Modify time 12], spacing 1
                let name_w = inner_w.saturating_sub(21);
                if rel < name_w {
                    Some(SortKey::Name)
                } else if rel < name_w + 8 {
                    Some(SortKey::Size)
                } else {
                    Some(SortKey::Mtime)
                }
            }
            ListMode::Long => {
                // [Perms 10, Owner 8, Group 8, Size 7, Name (fill)]
                if rel < 29 {
                    None // perms/owner/group have no sort key
                } else if rel < 37 {
                    Some(SortKey::Size)
                } else {
                    Some(SortKey::Name)
                }
            }
        };
        if let Some(key) = key {
            panel.set_sort(key);
        }
    }

    fn menu_click(&mut self, x: u16, y: u16) {
        let Some(ms) = self.menu.as_mut() else { return };
        let (titles, dropdown) = crate::ui::menu_layout(ms.menu, self.areas.screen);
        if y == self.areas.screen.y {
            match titles.iter().position(|(tx, tw)| x >= *tx && x < tx + tw) {
                Some(menu) => {
                    ms.menu = menu;
                    ms.item = first_menu_item(MENUS[menu].1);
                }
                None => self.menu = None,
            }
            return;
        }
        let inner = Rect {
            x: dropdown.x + 1,
            y: dropdown.y + 1,
            width: dropdown.width.saturating_sub(2),
            height: dropdown.height.saturating_sub(2),
        };
        if inner.contains(Position { x, y }) {
            let idx = (y - inner.y) as usize;
            // a separator click keeps the menu open
            if let Some(Some((_, _, action))) = MENUS[ms.menu].1.get(idx) {
                let (action, menu) = (*action, ms.menu);
                self.menu = None;
                self.run_menu_action(menu, action);
            }
            return;
        }
        self.menu = None;
    }

    fn on_wheel(&mut self, x: u16, y: u16, delta: isize) {
        // a list dialog scrolls under the wheel as under the arrows
        let list = self.dialog_rows.is_some()
            || matches!(
                self.dialog,
                Some(
                    Dialog::FindResults(_)
                        | Dialog::Fuzzy(_)
                        | Dialog::History(_)
                        | Dialog::Undo(_)
                )
            );
        if self.dialog.is_some() && list && self.fg_job().is_none() && self.connect.is_none() {
            let code = if delta < 0 {
                KeyCode::Up
            } else {
                KeyCode::Down
            };
            for _ in 0..delta.unsigned_abs() {
                self.on_dialog_key(KeyEvent::new(code, KeyModifiers::NONE));
            }
            return;
        }
        if self.fg_job().is_some()
            || self.connect.is_some()
            || self.find.is_some()
            || self.dialog.is_some()
            || self.menu.is_some()
        {
            return;
        }
        if let Some(help) = self.help.as_mut() {
            let rows = help.rows.max(1);
            let max_top = crate::ui::help_lines().saturating_sub(rows);
            help.top = help.top.saturating_add_signed(delta).min(max_top);
            return;
        }
        if let Some(st) = self.editor_mut() {
            if st.prompt.is_none() {
                st.ed.move_vert(delta, false);
                self.ensure_editor_visible();
            }
            return;
        }
        if let Some(v) = self.viewer_mut() {
            let rows = v.rows.max(1);
            viewer_scroll(v, delta, rows);
            return;
        }
        let pos = Position { x, y };
        for side in [0, 1] {
            let area = if side == 0 {
                self.areas.left
            } else {
                self.areas.right
            };
            if !area.contains(pos) {
                continue;
            }
            if let Some(qv) = self.quick_view.as_mut()
                && qv.side == side
            {
                if delta < 0 {
                    qv.top = qv.top.saturating_sub(delta.unsigned_abs());
                } else if let Some((_, fv)) = qv.view.as_mut() {
                    let want = qv.top + delta as usize;
                    let _ = fv.ensure_lines(want + 1);
                    qv.top = want.min(fv.known_lines().saturating_sub(1));
                }
                return;
            }
            if self.info == Some(side) {
                return;
            }
            // a tree panel scrolls its figure, not the listing beneath
            if self.panels[side].list_mode == ListMode::Tree {
                if let Some(tree) = self.trees[side].as_mut() {
                    for _ in 0..delta.unsigned_abs() {
                        if delta < 0 {
                            tree.up();
                        } else {
                            tree.down();
                        }
                    }
                }
                return;
            }
            // scroll the hovered panel's cursor without stealing focus
            let panel = &mut self.panels[side];
            for _ in 0..delta.unsigned_abs() {
                if delta < 0 {
                    panel.move_up();
                } else {
                    panel.move_down();
                }
            }
            return;
        }
    }

    /// Leave, taking the scratch files any open viewer was reading
    /// with us - they were made for this session.
    fn quit_now(&mut self) {
        for screen in self.screens.drain(..) {
            if let Screen::Viewer(v) = screen {
                for temp in v.temps {
                    let _ = std::fs::remove_file(temp);
                }
            }
        }
        self.current = None;
        self.quit = true;
    }

    /// M-`: mc's screen list. Row 0 is the panels, which are what is
    /// underneath every screen rather than one of them.
    fn open_screen_list(&mut self) {
        self.screen_list = Some(self.current.map(|at| at + 1).unwrap_or(0));
    }

    fn on_screen_list_key(&mut self, key: KeyEvent) {
        let Some(mut row) = self.screen_list else {
            return;
        };
        let rows = self.screens.len() + 1;
        match key.code {
            KeyCode::Esc | KeyCode::Char('`') => self.screen_list = None,
            KeyCode::Up | KeyCode::BackTab => {
                self.screen_list = Some((row + rows - 1) % rows);
            }
            KeyCode::Down | KeyCode::Tab => {
                row = (row + 1) % rows;
                self.screen_list = Some(row);
            }
            KeyCode::Enter => {
                self.screen_list = None;
                self.current = row.checked_sub(1);
            }
            _ => {}
        }
    }

    /// The diff on top, if the screen on top is one.
    pub fn diff(&self) -> Option<&DiffView> {
        match self.screens.get(self.current?) {
            Some(Screen::Diff(d)) => Some(d),
            _ => None,
        }
    }

    pub fn diff_mut(&mut self) -> Option<&mut DiffView> {
        match self.screens.get_mut(self.current?) {
            Some(Screen::Diff(d)) => Some(d),
            _ => None,
        }
    }

    /// Put a new screen on top and switch to it.
    fn open_screen(&mut self, screen: Screen) {
        self.screens.push(screen);
        self.current = Some(self.screens.len() - 1);
    }

    /// Take the screen on top out of the list; the panels come back up,
    /// which is where mc lands after closing one too - unless there are
    /// no panels to land on, `rcedit a b` having opened two screens and
    /// nothing underneath them.
    fn take_current_screen(&mut self) -> Option<Screen> {
        let at = self.current.take()?;
        let screen = (at < self.screens.len()).then(|| self.screens.remove(at));
        if self.standalone && !self.screens.is_empty() {
            self.current = Some(at.min(self.screens.len() - 1));
        }
        screen
    }

    /// Collect finished git scans and dispatch new ones when a local
    /// panel sits in a directory we have no (fresh) status for.
    fn git_tick(&mut self) {
        while let Ok((side, dir, status)) = self.git_rx.try_recv() {
            let panel = &self.panels[side];
            if panel.is_local() && panel.cwd == dir {
                self.git_info[side] = status.map(|s| (dir, s));
                self.dirty = true;
            }
        }
        if !git::ENABLED || !self.config.git {
            return;
        }
        for side in [0, 1] {
            let panel = &self.panels[side];
            if !panel.is_local() {
                self.git_info[side] = None;
                self.git_seen[side] = None;
                continue;
            }
            if panel.is_loading() || self.git_seen[side].as_ref() == Some(&panel.cwd) {
                continue;
            }
            self.git_seen[side] = Some(panel.cwd.clone());
            if self.git_info[side]
                .as_ref()
                .is_some_and(|(dir, _)| dir != &panel.cwd)
            {
                self.git_info[side] = None;
            }
            let tx = self.git_tx.clone();
            let dir = panel.cwd.clone();
            std::thread::spawn(move || {
                let status = git::scan(&dir);
                let _ = tx.send((side, dir, status));
            });
        }
    }

    /// Something may have changed repo state (job, shell, editor save):
    /// rescan both sides on the next tick.
    fn git_refresh(&mut self) {
        self.git_seen = [None, None];
    }

    /// Keep the free-space cache fresh: per local panel, re-measure when
    /// the directory changed or the last figure is older than 3 s.
    fn disk_tick(&mut self) {
        for side in [0, 1] {
            let panel = &self.panels[side];
            if !panel.is_local() {
                self.disk[side] = None;
                continue;
            }
            let stale = match &self.disk[side] {
                Some((dir, at, _)) => dir != &panel.cwd || at.elapsed() > Duration::from_secs(3),
                None => true,
            };
            if stale {
                let now = Some((panel.cwd.clone(), Instant::now(), free_space(&panel.cwd)));
                // only a changed figure is worth a frame; the clock
                // ticking over is not
                if now.as_ref().map(|(dir, _, free)| (dir, free))
                    != self.disk[side].as_ref().map(|(dir, _, free)| (dir, free))
                {
                    self.dirty = true;
                }
                self.disk[side] = now;
            }
        }
    }

    fn on_job_key(&mut self, key: KeyEvent) {
        let Some(job) = self.fg_job_mut() else { return };
        let Some(ask) = &job.ask else {
            match key.code {
                KeyCode::Esc => job.handle.cancel(),
                // detach: the job keeps running, panels come back
                KeyCode::Char('b' | 'B') => job.background = true,
                KeyCode::Char('p' | 'P') => job.handle.set_paused(!job.handle.is_paused()),
                _ => {}
            }
            return;
        };
        let count = ask.buttons().len();
        let reply = match key.code {
            KeyCode::Left => {
                job.button = job.button.checked_sub(1).unwrap_or(count - 1);
                None
            }
            KeyCode::Right | KeyCode::Tab => {
                job.button = (job.button + 1) % count;
                None
            }
            KeyCode::Up => {
                job.button = ask.step_row(job.button, -1);
                None
            }
            KeyCode::Down => {
                job.button = ask.step_row(job.button, 1);
                None
            }
            KeyCode::Enter => Some(ask.reply(job.button)),
            KeyCode::Esc => Some(Reply::Abort),
            KeyCode::Char('o') => matches!(ask, Ask::Overwrite { .. }).then_some(Reply::Overwrite),
            KeyCode::Char('a') => {
                matches!(ask, Ask::Overwrite { .. }).then_some(Reply::OverwriteAll)
            }
            KeyCode::Char('r') => matches!(ask, Ask::Error { .. }).then_some(Reply::Retry),
            KeyCode::Char('s') => Some(Reply::Skip),
            KeyCode::Char('S') => Some(Reply::SkipAll),
            _ => None,
        };
        if let Some(reply) = reply {
            let _ = job.handle.replies.send(reply);
            job.ask = None;
        }
    }

    /// Start listening for `rcmd --remote`. A failure here is worth a
    /// warning and nothing more: the panels work without it.
    pub fn serve_remote(&mut self) -> Option<String> {
        match crate::remote::serve() {
            Ok(server) => {
                self.remote = Some(server);
                None
            }
            Err(err) => Some(format!("remote control off: {err}")),
        }
    }

    /// Whatever arrived on the socket since the last turn of the loop.
    fn drain_remote(&mut self) {
        let mut lines = Vec::new();
        if let Some(server) = &self.remote {
            while let Ok(request) = server.requests.try_recv() {
                lines.push(request);
            }
        }
        for request in lines {
            let answer = self.run_remote(&request.line);
            let _ = request.reply.send(answer);
            self.dirty = true;
        }
    }

    /// One line from the socket. The vocabulary is deliberately small,
    /// because the last of them is the whole keymap: anything rcmd can
    /// be told to do by a key can be asked for by name.
    fn run_remote(&mut self, line: &str) -> String {
        let (verb, rest) = match line.split_once(char::is_whitespace) {
            Some((verb, rest)) => (verb, rest.trim()),
            None => (line, ""),
        };
        match verb {
            "" => "error: nothing to do".into(),
            "pwd" => self.panels[self.active].display_path(),
            "other" => self.panels[self.active ^ 1].display_path(),
            "cursor" => self.panels[self.active]
                .selected()
                .map(|entry| entry.name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            "marked" => self.panels[self.active]
                .targets()
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
                .replace('\n', " "),
            "cd" if !rest.is_empty() => {
                self.navigate(rest);
                "ok".into()
            }
            "select" | "unselect" if !rest.is_empty() => {
                let pattern = rcmd_core::pattern::Pattern {
                    text: rest.to_string(),
                    ..Default::default()
                };
                match self.panels[self.active].mark_pattern(&pattern, verb == "select") {
                    Ok(moved) => format!("{moved}"),
                    Err(err) => format!("error: {err}"),
                }
            }
            "action" if !rest.is_empty() => match keymap::parse_action(rest) {
                Some(action) => {
                    self.run_action(action);
                    "ok".into()
                }
                None => format!("error: no action called {rest}"),
            },
            "status" if !rest.is_empty() => {
                self.status = Some(format!(" {rest} "));
                "ok".into()
            }
            other => format!(
                "error: {other} is not one of cd, select, unselect, action, \
                 status, pwd, other, cursor, marked"
            ),
        }
    }

    /// Note where the panels are now. A directory counts once per
    /// arrival, not once per redraw, which is what `visited` is for.
    fn note_visits(&mut self) {
        let now = Self::unix_now();
        for at in 0..2 {
            let here = self.panels[at].display_path();
            if here == self.visited[at] || self.panels[at].is_loading() {
                continue;
            }
            self.visited[at] = here.clone();
            state::merge_visit(&mut self.visits, &here, 1, now);
            state::trim_visits(&mut self.visits, now);
        }
    }

    /// Seconds since the epoch; 0 if the clock is before it, which no
    /// ranking has to survive gracefully.
    fn unix_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// This session's visit log, for the exit-time merge into the state
    /// file.
    pub fn visit_log(&self) -> &[state::Visit] {
        &self.visits
    }

    /// Recent directories for the hotlist dialog: everywhere rcmd has
    /// been, ranked by frecency - how often, weighted by how recently -
    /// rather than by arrival order, so the directory that is actually
    /// yours is at the top and is still there next session. Pinned
    /// entries and the place we are standing are left out, and the list
    /// is capped.
    pub fn hotlist_recent(&self) -> Vec<String> {
        let here = self.panels[self.active].display_path();
        let now = Self::unix_now();
        let mut ranked: Vec<(f64, &str)> = self
            .visits
            .iter()
            .filter(|v| v.path != here && !HotEntry::holds(&self.config.hotlist, &v.path))
            .map(|v| (state::frecency(v, now), v.path.as_str()))
            .collect();
        ranked.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(b.1))
        });
        ranked
            .into_iter()
            .take(15)
            .map(|(_, path)| path.to_string())
            .collect()
    }

    /// Hotlist edits write through to the state file like the options
    /// form - never to the user's config.
    /// The group the dialog is looking at.
    pub fn hot_group(&self, group: &[usize]) -> &Vec<HotEntry> {
        let mut here = &self.config.hotlist;
        for &at in group {
            match here.get(at) {
                Some(entry) => here = &entry.entries,
                None => break,
            }
        }
        here
    }

    /// The group names on the way down, for the dialog's title.
    pub fn hotlist_group_path(&self, d: &HotlistDialog) -> String {
        let mut names = Vec::new();
        let mut here = &self.config.hotlist;
        for &at in &d.group {
            match here.get(at) {
                Some(entry) => {
                    names.push(entry.label.as_str());
                    here = &entry.entries;
                }
                None => break,
            }
        }
        names.join(" / ")
    }

    fn hot_group_mut(&mut self, group: &[usize]) -> &mut Vec<HotEntry> {
        let mut here = &mut self.config.hotlist;
        for &at in group {
            match here.get(at).is_some() {
                true => here = &mut here[at].entries,
                false => break,
            }
        }
        here
    }

    /// What the dialog draws, and what each row answers to. The recent
    /// directories are rcmd's own and stay at the top level - they are
    /// a log of where you have been, not a list you arrange.
    pub fn hotlist_rows(&self, d: &HotlistDialog) -> Vec<HotRow> {
        let mut rows = Vec::new();
        if !d.group.is_empty() {
            rows.push(HotRow::Up);
        }
        for (at, entry) in self.hot_group(&d.group).iter().enumerate() {
            rows.push(match entry.is_group() {
                true => HotRow::Group(at),
                false => HotRow::Entry(at),
            });
        }
        if d.group.is_empty() {
            rows.extend(self.hotlist_recent().into_iter().map(HotRow::Recent));
        }
        // C-s narrows what is on screen. The Up row stays: it is how
        // you get out of a group, not a place to go
        if let Some(needle) = d.filter.as_ref().filter(|n| !n.is_empty()) {
            let needle = needle.to_lowercase();
            let group = self.hot_group(&d.group);
            rows.retain(|row| match row {
                HotRow::Up => true,
                HotRow::Recent(loc) => loc.to_lowercase().contains(&needle),
                HotRow::Entry(at) | HotRow::Group(at) => group.get(*at).is_some_and(|e| {
                    e.label.to_lowercase().contains(&needle)
                        || e.path.to_lowercase().contains(&needle)
                }),
            });
        }
        rows
    }

    /// Alt+Up / Alt+Down: move the entry under the cursor within its
    /// group, the cursor going with it.
    fn hotlist_reorder(&mut self, d: &mut HotlistDialog, rows: &[HotRow], down: bool) {
        let Some(HotRow::Entry(at) | HotRow::Group(at)) = rows.get(d.row) else {
            return;
        };
        let at = *at;
        let entries = self.hot_group_mut(&d.group);
        let to = match down {
            true if at + 1 < entries.len() => at + 1,
            false if at > 0 => at - 1,
            _ => return,
        };
        entries.swap(at, to);
        d.row = match down {
            true => d.row + 1,
            false => d.row - 1,
        };
        self.save_hotlist();
    }

    fn hotlist_drop(&mut self, group: &[usize], at: usize) {
        let entries = self.hot_group_mut(group);
        if at < entries.len() {
            entries.remove(at);
        }
        self.save_hotlist();
    }

    /// Enter on a hotlist entry: go there, wherever there is.
    fn hotlist_go(&mut self, path: &str) {
        if path.is_empty() {
            return;
        }
        if is_remote_url(path) {
            self.connect_remote(path);
            return;
        }
        let target = self.resolve(path);
        let panel = &mut self.panels[self.active];
        let moved = match panel.is_remote() {
            true => panel.to_local(target),
            false => panel.cd(target),
        };
        if let Err(err) = moved {
            self.status = Some(format!(" hotlist: {err} "));
        }
    }

    /// Ask for a label - to add, to name a group, or to rename.
    pub(super) fn ask_hotlist_label(
        &mut self,
        title: &str,
        value: String,
        group: Vec<usize>,
        index: Option<usize>,
        path: String,
    ) {
        self.dialog = Some(Dialog::Input(InputDialog::new(
            title.to_string(),
            value,
            InputAction::HotlistLabel { group, index, path },
        )));
    }

    /// ...and what the answer does. Either way the hotlist comes back
    /// up, which is where mc leaves you too.
    fn finish_hotlist_label(
        &mut self,
        label: &str,
        group: Vec<usize>,
        index: Option<usize>,
        path: String,
    ) {
        let label = label.trim().to_string();
        if !label.is_empty() {
            let entries = self.hot_group_mut(&group);
            match index {
                Some(at) if at < entries.len() => entries[at].label = label,
                // a new entry, or a new group when there is no path
                _ => entries.push(HotEntry {
                    label,
                    path,
                    entries: Vec::new(),
                }),
            }
            self.save_hotlist();
        }
        let row = self.hot_group(&group).len().saturating_sub(1) + usize::from(!group.is_empty());
        self.dialog = Some(Dialog::Hotlist(HotlistDialog::at(group, row)));
    }

    fn save_hotlist(&mut self) {
        let hotlist = self.config.hotlist.clone();
        if let Err(err) = state::update(move |s| s.hotlist = Some(hotlist)) {
            self.status = Some(format!(" could not save state: {err} "));
        }
    }

    /// The viewed/edited list, for the dialog to draw.
    pub fn file_history(&self) -> &[String] {
        &self.file_history
    }

    /// One key in the diff view.
    /// Close whatever screen is on top, cleaning up after a viewer.
    fn close_screen(&mut self) {
        match self.take_current_screen() {
            Some(Screen::Viewer(v)) => {
                for temp in v.temps {
                    let _ = std::fs::remove_file(temp);
                }
            }
            // a diff opened from the synchronize plan goes back to it
            Some(Screen::Diff(_)) => {
                if let Some(plan) = self.sync_return.take() {
                    self.dialog = Some(Dialog::Sync(plan));
                }
            }
            _ => {}
        }
    }

    /// A job with nothing special about it: the fields every one of them
    /// starts with, in one place.
    fn push_job(&mut self, title: String, handle: fsops::JobHandle) {
        self.push_job_kind(title, handle, false);
    }

    /// ...and the one job whose "skipped" is the point.
    fn push_job_kind(&mut self, title: String, handle: fsops::JobHandle, checking: bool) {
        self.jobs.push(Job {
            title,
            handle,
            total_files: 0,
            total_bytes: 0,
            files_done: 0,
            bytes_done: 0,
            current: PathBuf::new(),
            file_done: 0,
            file_total: 0,
            rate: 0.0,
            rate_mark: (Instant::now(), 0),
            started: Instant::now(),
            ask: None,
            button: 0,
            src_panel: self.active,
            background: false,
            moved: Vec::new(),
            trashed: Vec::new(),
            restored: Vec::new(),
            device: None,
            skips: Vec::new(),
            checking,
        });
    }
}

fn line_segs(v: &mut Viewer, idx: usize, cols: usize) -> usize {
    match v.file.line(idx) {
        Ok(Some(line)) => ui::expand_line(&line).chars().count().div_ceil(cols).max(1),
        _ => 1,
    }
}

fn line_exists(v: &mut Viewer, idx: usize) -> bool {
    matches!(v.file.line(idx), Ok(Some(_)))
}

fn viewer_scroll_wrapped(v: &mut Viewer, delta: isize) {
    let cols = v.cols.max(1);
    if delta >= 0 {
        for _ in 0..delta {
            if v.top_seg + 1 < line_segs(v, v.top, cols) {
                v.top_seg += 1;
            } else if line_exists(v, v.top + 1) {
                v.top += 1;
                v.top_seg = 0;
            } else {
                break;
            }
        }
    } else {
        for _ in 0..delta.unsigned_abs() {
            if v.top_seg > 0 {
                v.top_seg -= 1;
            } else if v.top > 0 {
                v.top -= 1;
                v.top_seg = line_segs(v, v.top, cols).saturating_sub(1);
            } else {
                break;
            }
        }
    }
}

/// Write the pending bytes out. The file keeps its length - a hex
/// editor replaces bytes and never moves them - so this is a handful of
/// writes into the file that is already there, not a rewrite.
fn hex_save(v: &mut Viewer) {
    if v.hex_edits.is_empty() {
        v.note = Some(" nothing changed ".into());
        return;
    }
    if let Some(why) = v.editable() {
        v.note = Some(why.into());
        return;
    }
    let edits: Vec<(u64, u8)> = v.hex_edits.iter().map(|(&at, &b)| (at, b)).collect();
    match rcmd_core::view::patch_bytes(&v.source, &edits) {
        Ok(()) => {
            v.note = Some(format!(" {} bytes written ", edits.len()));
            v.hex_edits.clear();
            // the text under the hex changed too
            if let Some(hl) = v.hl.as_mut() {
                hl.invalidate_from(0);
            }
        }
        Err(err) => v.note = Some(format!(" save: {err} ")),
    }
}

/// Keep the hex cursor on screen after it moves.
fn hex_follow(v: &mut Viewer, rows: usize) {
    let row = v.hex_cursor / 16;
    if row < v.hex_top {
        v.hex_top = row;
    } else if row >= v.hex_top + rows as u64 {
        v.hex_top = row - rows as u64 + 1;
    }
}

/// One key while the hex cursor is on. True = the key was the file's
/// rather than the viewer's, so nothing else may look at it - "q" is a
/// byte here, not the command to quit.
fn hex_edit_key(v: &mut Viewer, key: KeyEvent, rows: usize) -> bool {
    let last = v.file.size.saturating_sub(1);
    let step = |v: &mut Viewer, delta: i64| {
        v.hex_cursor = v.hex_cursor.saturating_add_signed(delta).min(last);
        v.hex_low = false;
        hex_follow(v, rows);
    };
    match key.code {
        KeyCode::Esc => {
            v.hex_edit = false;
            v.note = Some(" viewing ".into());
        }
        KeyCode::Tab | KeyCode::BackTab => {
            v.hex_ascii = !v.hex_ascii;
            v.hex_low = false;
        }
        KeyCode::Left | KeyCode::Backspace => step(v, -1),
        KeyCode::Right => step(v, 1),
        KeyCode::Up => step(v, -16),
        KeyCode::Down => step(v, 16),
        KeyCode::PageUp => step(v, -16 * rows as i64),
        KeyCode::PageDown => step(v, 16 * rows as i64),
        KeyCode::Home => {
            v.hex_cursor -= v.hex_cursor % 16;
            v.hex_low = false;
        }
        KeyCode::End => {
            v.hex_cursor = (v.hex_cursor - v.hex_cursor % 16 + 15).min(last);
            v.hex_low = false;
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if v.file.size == 0 {
                v.note = Some(" the file is empty ".into());
                return true;
            }
            let at = v.hex_cursor;
            let old = v.byte_at(at).unwrap_or(0);
            if v.hex_ascii {
                // the text column takes the character itself
                if !c.is_ascii() {
                    v.note = Some(" one byte per character here: type it in hex ".into());
                    return true;
                }
                v.hex_edits.insert(at, c as u8);
                step(v, 1);
            } else if let Some(digit) = c.to_digit(16) {
                // the hex column takes the two halves in turn
                let byte = if v.hex_low {
                    (old & 0xf0) | digit as u8
                } else {
                    (old & 0x0f) | (digit as u8) << 4
                };
                v.hex_edits.insert(at, byte);
                if v.hex_low {
                    step(v, 1);
                } else {
                    v.hex_low = true;
                }
            } else {
                v.note = Some(" hex digits here; Tab switches to the text column ".into());
            }
        }
        _ => return false,
    }
    true
}

fn viewer_scroll(v: &mut Viewer, delta: isize, rows: usize) {
    if v.wrap && !v.hex {
        viewer_scroll_wrapped(v, delta);
        return;
    }
    if v.hex {
        let total_rows = v.file.size.div_ceil(16);
        let cap = total_rows.saturating_sub(rows as u64);
        v.hex_top = if delta < 0 {
            v.hex_top.saturating_sub(delta.unsigned_abs() as u64)
        } else {
            (v.hex_top + delta as u64).min(cap)
        };
    } else if delta < 0 {
        v.top = v.top.saturating_sub(delta.unsigned_abs());
    } else {
        let want = v.top + delta as usize;
        let _ = v.file.ensure_lines(want + rows + 1);
        let cap = v.file.known_lines().saturating_sub(rows);
        v.top = want.min(cap);
    }
}

fn viewer_end(v: &mut Viewer, rows: usize) {
    if v.hex {
        v.hex_top = v.file.size.div_ceil(16).saturating_sub(rows as u64);
    } else if let Ok(total) = v.file.total_lines() {
        if v.wrap {
            let cols = v.cols.max(1);
            v.top = total.saturating_sub(1);
            v.top_seg = line_segs(v, v.top, cols).saturating_sub(1);
        } else {
            v.top = total.saturating_sub(rows);
        }
    }
}

/// Take the viewer where a goto input says. The three forms - a line,
/// a byte offset, a share of the file - are told apart by how the
/// number is written, so there is one field rather than a radio.
fn viewer_goto(v: &mut Viewer, input: &str) {
    let Some(goto) = rcmd_core::view::parse_goto(input) else {
        v.note = Some(" not a line, offset (0x1f or 31b) or percent ".into());
        return;
    };
    match v.file.goto_line(goto) {
        Ok(line) => {
            v.top = line;
            v.top_seg = 0;
            v.found = None;
            // the hex view goes to the byte itself: an offset names it,
            // a line or a percentage names where that line starts.
            // `hex_top` counts rows of sixteen, not bytes.
            let offset = match goto {
                rcmd_core::view::Goto::Offset(offset) => offset.min(v.file.size),
                _ => v.file.offset_of_line(line).unwrap_or(0),
            };
            let cap = v.file.size.div_ceil(16).saturating_sub(v.rows as u64);
            v.hex_top = (offset / 16).min(cap);
            if v.hex_edit {
                v.hex_cursor = offset.min(v.file.size.saturating_sub(1));
                v.hex_low = false;
            }
        }
        Err(err) => v.note = Some(format!(" {err} ")),
    }
}

fn viewer_search(v: &mut Viewer, from: usize, is_next: bool) {
    viewer_search_way(v, from, is_next, false);
}

/// ...the other way from the one the dialog asked: `N` after `n`.
fn viewer_search_way(v: &mut Viewer, from: usize, is_next: bool, flip: bool) {
    if v.hex {
        return viewer_search_hex(v, is_next, flip);
    }
    // in nroff mode the search runs over what the overstrikes spell,
    // which is what is on the screen to be looked for
    let mut search = Search {
        nroff: v.nroff,
        ..v.search.to_search()
    };
    search.backwards ^= flip;
    let mut found = v.file.find(from, &search);
    // past the last hit, round to the other end once, as less does
    let mut wrapped = false;
    if is_next && matches!(found, Ok(None)) {
        let restart = if search.backwards {
            v.file.total_lines().unwrap_or(0).saturating_sub(1)
        } else {
            0
        };
        found = v.file.find(restart, &search);
        wrapped = true;
    }
    match found {
        Ok(Some(idx)) => {
            v.found = Some(idx);
            v.hex_hit = None;
            v.top = idx.saturating_sub(2);
            v.top_seg = 0;
            if wrapped {
                v.note = Some(" search wrapped around ".into());
            } else if !is_next {
                v.note = match_count(v, &search);
            }
        }
        Ok(None) => not_found(v, is_next),
        Err(err) => v.note = Some(format!(" {err} ")),
    }
}

/// How many lines the search matches, said once when it first finds
/// one - for a file small enough to read through without making the
/// search itself wait.
fn match_count(v: &mut Viewer, search: &Search) -> Option<String> {
    const COUNTABLE: u64 = 32 * 1024 * 1024;
    if v.file.size > COUNTABLE {
        return None;
    }
    let count = v.file.count_matching(search).ok()?;
    Some(match count {
        1 => " 1 line matches ".into(),
        n => format!(" {n} lines match - n next, N previous "),
    })
}

/// A search from the hex view stays in it: the hit is a byte range,
/// shown where it is, rather than a line in a text view the search
/// used to switch to. It starts at the cursor, or the top of the view,
/// and "next" steps past the last hit whichever way it goes.
fn viewer_search_hex(v: &mut Viewer, is_next: bool, flip: bool) {
    let mut search = v.search.to_search();
    search.backwards ^= flip;
    let from = match v.hex_hit {
        Some((at, _)) if is_next && !search.backwards => at + 1,
        Some((at, _)) if is_next => at,
        _ if v.hex_edit => v.hex_cursor,
        _ => v.hex_top * 16,
    };
    match v.file.find_offset(from, &search) {
        Ok(Some((at, len))) => {
            v.hex_hit = Some((at, len));
            v.found = v.file.line_at_offset(at).ok();
            if let Some(line) = v.found {
                v.top = line.saturating_sub(2);
                v.top_seg = 0;
            }
            if v.hex_edit {
                v.hex_cursor = at;
                v.hex_low = false;
            }
            // a couple of rows of context above, as the text view has
            let row = at / 16;
            let rows = v.rows.max(1) as u64;
            if row < v.hex_top || row >= v.hex_top + rows {
                v.hex_top = row.saturating_sub(2.min(rows - 1));
            }
        }
        Ok(None) => not_found(v, is_next),
        Err(err) => v.note = Some(format!(" {err} ")),
    }
}

fn not_found(v: &mut Viewer, is_next: bool) {
    v.found = None;
    v.hex_hit = None;
    v.note = Some(
        if is_next {
            " no more matches "
        } else {
            " not found "
        }
        .into(),
    );
}

/// One position past the cursor, so "search next" skips the current hit.
fn next_pos(ed: &rcmd_edit::Editor) -> rcmd_edit::Pos {
    let c = ed.cursor;
    if c.col < ed.line_len(c.line) {
        rcmd_edit::Pos {
            line: c.line,
            col: c.col + 1,
        }
    } else if c.line + 1 < ed.line_count() {
        rcmd_edit::Pos {
            line: c.line + 1,
            col: 0,
        }
    } else {
        rcmd_edit::Pos { line: 0, col: 0 }
    }
}

/// Jump to a match and select it so the hit is visible.
fn select_match(ed: &mut rcmd_edit::Editor, m: rcmd_edit::Match) {
    let end = ed.after_match(m);
    ed.goto(m.pos, false);
    if m.len > 0 {
        ed.goto(end, true);
    }
}

/// (free, total) bytes of the filesystem holding `path`.
#[cfg(unix)]
fn free_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut vfs) } != 0 {
        return None;
    }
    let frsize = vfs.f_frsize as u64;
    Some((vfs.f_bavail as u64 * frsize, vfs.f_blocks as u64 * frsize))
}

#[cfg(not(unix))]
fn free_space(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// Inverse of [`ui::screen_col`]: the character index whose cell covers
/// screen column `target`, for mouse clicks.
fn col_at_screen(text: &str, target: usize) -> usize {
    rcmd_edit::col_at_screen(text, target, ui::tab_size())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Whether there is a desktop to have a clipboard at all. Over ssh
/// there is not, and the X tools would each be a process spawned to
/// fail - so the question is asked before they are.
fn desktop_clipboard() -> bool {
    cfg!(target_os = "macos")
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("DISPLAY").is_some()
}

/// mc's clipboard file - what `%q` spends and where mcedit leaves what
/// it copied. rcmd writes the same file, so a block yanked in either
/// editor is the same block to the other one.
fn clip_file() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cache"));
    cache.join("mc/mcedit/mcedit.clip")
}

fn clip_file_write(text: &str) {
    let path = clip_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
}

fn clip_file_read() -> Option<String> {
    std::fs::read_to_string(clip_file()).ok()
}

/// Hand text to the desktop clipboard through whichever tool is
/// installed. False = none was, so the editor's own clipboard is all
/// there is - which is not an error worth a message, only a smaller
/// world.
fn clipboard_set(text: &str) -> bool {
    // the file first: it works over ssh, where no clipboard tool does,
    // and it is what `%q` in a user command reads
    clip_file_write(text);
    if !desktop_clipboard() {
        return false;
    }
    const TOOLS: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ];
    for (tool, args) in TOOLS {
        let child = std::process::Command::new(tool)
            .args(*args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = child else { continue };
        if let Some(stdin) = child.stdin.as_mut() {
            use std::io::Write as _;
            let _ = stdin.write_all(text.as_bytes());
        }
        drop(child.stdin.take());
        let _ = child.wait();
        return true;
    }
    false
}

/// ...and back. None = no tool and no file, or neither had anything to
/// say.
fn clipboard_get() -> Option<String> {
    if !desktop_clipboard() {
        return clip_file_read().filter(|text| !text.is_empty());
    }
    const TOOLS: &[(&str, &[&str])] = &[
        ("wl-paste", &["--no-newline"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
        ("pbpaste", &[]),
    ];
    for (tool, args) in TOOLS {
        let out = std::process::Command::new(tool)
            .args(*args)
            .stderr(std::process::Stdio::null())
            .output();
        let Ok(out) = out else { continue };
        if !out.status.success() {
            continue;
        }
        return Some(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    clip_file_read().filter(|text| !text.is_empty())
}

/// What a listing entry looks like to mc's `t` conditions.
fn menu_kind(entry: &entry::Entry) -> rcmd_core::usermenu::FileKind {
    use rcmd_core::entry::EntryKind;
    use rcmd_core::usermenu::FileKind;
    match entry.kind {
        EntryKind::Dir => FileKind::Dir,
        EntryKind::SymlinkDir => FileKind::LinkDir,
        EntryKind::SymlinkFile | EntryKind::SymlinkBroken => FileKind::Link,
        EntryKind::File if entry.is_executable() => FileKind::Executable,
        EntryKind::File => FileKind::File,
    }
}

fn first_edit_item(entries: &[EditMenuEntry]) -> usize {
    entries.iter().position(Option::is_some).unwrap_or(0)
}

fn edit_menu_step(entries: &[EditMenuEntry], current: usize, delta: isize) -> usize {
    let len = entries.len() as isize;
    let mut i = current as isize;
    loop {
        i += delta;
        if i < 0 {
            i = len - 1;
        } else if i >= len {
            i = 0;
        }
        if entries[i as usize].is_some() || i == current as isize {
            return i as usize;
        }
    }
}

fn first_menu_item(entries: &[MenuEntry]) -> usize {
    entries.iter().position(Option::is_some).unwrap_or(0)
}

fn menu_step(entries: &[MenuEntry], current: usize, delta: isize) -> usize {
    let len = entries.len() as isize;
    let mut i = current as isize;
    loop {
        i += delta;
        if i < 0 {
            i = len - 1;
        } else if i >= len {
            i = 0;
        }
        if entries[i as usize].is_some() {
            return i as usize;
        }
        if i as usize == current {
            return current;
        }
    }
}

/// "archive.zip://sub/dir" → (archive path, path inside). Plain local
/// paths return None.
/// How long a job has to run before it is worth a notice when it ends.
pub const NOTIFY_AFTER: Duration = Duration::from_secs(10);

/// The desktop-notice escape for the terminal rcmd runs in, or nothing
/// where none is known to take one - an unknown OSC is ignored by most
/// terminals, but not by all, and a bell is enough there. kitty has its
/// own (99); foot, urxvt and Ghostty take 777; iTerm2, WezTerm, ConEmu
/// and Windows Terminal take 9.
fn notice_escape(text: &str) -> String {
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    let term = std::env::var("TERM").unwrap_or_default();
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    if std::env::var_os("TMUX").is_some() || term.starts_with("screen") {
        // a multiplexer eats OSC it does not know; the bell gets through
        return String::new();
    }
    if term == "xterm-kitty" {
        format!("\x1b]99;;{text}\x1b\\")
    } else if term.starts_with("foot") || term.starts_with("rxvt-unicode") || program == "ghostty" {
        format!("\x1b]777;notify;rcmd;{text}\x1b\\")
    } else if matches!(program.as_str(), "iTerm.app" | "WezTerm")
        || std::env::var_os("WT_SESSION").is_some()
        || std::env::var_os("ConEmuPID").is_some()
    {
        format!("\x1b]9;{text}\x07")
    } else {
        String::new()
    }
}

/// A location that lives on a server rather than on this machine.
fn is_remote_url(target: &str) -> bool {
    ["sftp://", "ftp://", "fish://", "rclone://", "trash://"]
        .iter()
        .any(|scheme| target.starts_with(scheme))
}

fn split_vfs_dest(input: &str) -> Option<(PathBuf, PathBuf)> {
    let (archive, inside) = input.split_once("://")?;
    Some((
        PathBuf::from(archive),
        PathBuf::from(inside.trim_matches('/')),
    ))
}

/// `cd`? Returns the target ("" = home) or None if this isn't a cd command.
fn parse_cd(cmd: &str) -> Option<&str> {
    let rest = cmd.strip_prefix("cd")?;
    if rest.is_empty() {
        return Some("");
    }
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim().trim_matches('"').trim_matches('\''))
}

/// Lexical path normalization: resolves `.` and `..` without touching the
/// filesystem, so `cd ..` yields a clean cwd for the panel title.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `user[:group]` → numeric ids for chown; either side may be empty
/// ("leave unchanged") or numeric. On remote panels (`numeric_only`)
/// names cannot be resolved - the server's passwd is not ours.
fn parse_owner_spec(spec: &str, numeric_only: bool) -> Result<(Option<u32>, Option<u32>), String> {
    let (user, group) = match spec.split_once(':') {
        Some((u, g)) => (u.trim(), g.trim()),
        None => (spec.trim(), ""),
    };
    let resolve = |name: &str, is_user: bool| -> Result<Option<u32>, String> {
        if name.is_empty() {
            return Ok(None);
        }
        if let Ok(id) = name.parse::<u32>() {
            return Ok(Some(id));
        }
        if numeric_only {
            return Err(format!("'{name}': numeric ids only on a remote panel"));
        }
        lookup_id(name, is_user)
            .ok_or_else(|| {
                format!(
                    "unknown {} '{name}'",
                    if is_user { "user" } else { "group" }
                )
            })
            .map(Some)
    };
    Ok((resolve(user, true)?, resolve(group, false)?))
}

/// getpwnam_r / getgrnam_r: name → uid/gid.
fn lookup_id(name: &str, is_user: bool) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut buf = vec![0u8; 4096];
    unsafe {
        if is_user {
            let mut pwd: libc::passwd = std::mem::zeroed();
            let mut out: *mut libc::passwd = std::ptr::null_mut();
            let rc = libc::getpwnam_r(
                cname.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut out,
            );
            (rc == 0 && !out.is_null()).then_some(pwd.pw_uid)
        } else {
            let mut grp: libc::group = std::mem::zeroed();
            let mut out: *mut libc::group = std::ptr::null_mut();
            let rc = libc::getgrnam_r(
                cname.as_ptr(),
                &mut grp,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut out,
            );
            (rc == 0 && !out.is_null()).then_some(grp.gr_gid)
        }
    }
}

fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-+/=:,@%~".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every setting must have a row, or it exists in the config and in
    /// the values array while being unreachable in the form - which is
    /// the other half of the mistake `Opt::Count` now prevents.
    #[test]
    fn every_option_has_a_row_in_the_form() {
        let mut seen = [false; OPT_COUNT];
        for row in OPTION_ROWS {
            match row {
                OptRow::Check(opt, _) | OptRow::Radio(opt, ..) => seen[*opt as usize] = true,
                OptRow::Head(_) | OptRow::Ratio(_) => {}
            }
        }
        let missing: Vec<usize> = seen
            .iter()
            .enumerate()
            .filter(|(_, shown)| !**shown)
            .map(|(i, _)| i)
            .collect();
        assert!(missing.is_empty(), "settings with no row: {missing:?}");
    }

    #[test]
    fn owner_spec_parsing() {
        assert_eq!(parse_owner_spec("0:0", true), Ok((Some(0), Some(0))));
        assert_eq!(parse_owner_spec("1000", true), Ok((Some(1000), None)));
        assert_eq!(parse_owner_spec(":5", true), Ok((None, Some(5))));
        assert_eq!(parse_owner_spec("", true), Ok((None, None)));
        assert!(parse_owner_spec("alice", true).is_err()); // names need local passwd
        assert_eq!(parse_owner_spec("root:", false), Ok((Some(0), None)));
        assert!(parse_owner_spec("no-such-user-xyz", false).is_err());
    }

    #[test]
    fn options_cursor_skips_section_headings() {
        let mut d = OptionsDialog {
            cursor: 1,
            values: [false; OPT_COUNT],
            ratio: 50,
            ok: true,
        };
        // every stop is a setting, never a heading, all the way down
        let mut seen = 0;
        for _ in 0..OPTION_ROWS.len() * 2 {
            d.step(1);
            if d.cursor == OPTION_ROWS.len() {
                seen += 1; // the button row
                continue;
            }
            assert!(
                OPTION_ROWS[d.cursor].selectable(),
                "landed on a heading at row {}",
                d.cursor
            );
        }
        assert!(seen >= 1, "never reached the button row");
        // and the same walking backwards
        for _ in 0..OPTION_ROWS.len() * 2 {
            d.step(-1);
            assert!(d.cursor == OPTION_ROWS.len() || OPTION_ROWS[d.cursor].selectable());
        }
    }

    #[test]
    fn options_toggle_flips_only_the_focused_setting() {
        // look the row up rather than hardcoding an index: the form
        // grows a section at a time as the parity work lands
        let hidden_row = OPTION_ROWS
            .iter()
            .position(|r| r.opt() == Some(Opt::Hidden))
            .expect("the form has a hidden-files row");
        let mut d = OptionsDialog {
            cursor: hidden_row,
            values: [false; OPT_COUNT],
            ratio: 50,
            ok: true,
        };
        d.toggle();
        assert!(d.get(Opt::Hidden));
        assert!(!d.get(Opt::Lynx));
        d.cursor = 0; // a heading: toggling does nothing (row 0 is one)
        d.toggle();
        assert_eq!(d.values.iter().filter(|v| **v).count(), 1);
    }

    #[test]
    fn options_ratio_nudges_within_bounds() {
        let ratio_row = OPTION_ROWS
            .iter()
            .position(|r| matches!(r, OptRow::Ratio(_)))
            .expect("the form has a ratio row");
        let mut d = OptionsDialog {
            cursor: ratio_row,
            values: [false; OPT_COUNT],
            ratio: 50,
            ok: true,
        };
        assert!(d.nudge(5));
        assert_eq!(d.ratio, 55);
        for _ in 0..20 {
            d.nudge(5);
        }
        assert_eq!(d.ratio, 80, "clamped at the top");
        for _ in 0..40 {
            d.nudge(-5);
        }
        assert_eq!(d.ratio, 20, "clamped at the bottom");
        // any other row ignores the nudge, so Left/Right still toggles
        d.cursor = OPTION_ROWS
            .iter()
            .position(|r| r.opt() == Some(Opt::Hidden))
            .unwrap();
        assert!(!d.nudge(5));
    }

    #[test]
    fn command_history_caps_and_dedups() {
        let mut cl = CmdLine::default();
        cl.push_history("ls");
        cl.push_history("ls"); // consecutive duplicate is dropped
        cl.push_history("pwd");
        assert_eq!(cl.history(), ["ls", "pwd"]);
        for i in 0..HISTORY_CAP + 10 {
            cl.push_history(&format!("cmd{i}"));
        }
        assert_eq!(cl.history().len(), HISTORY_CAP);
        // the oldest entries fell off the front, the newest is last
        assert_eq!(
            cl.history().last().unwrap(),
            &format!("cmd{}", HISTORY_CAP + 9)
        );
        assert!(!cl.history().contains(&"ls".to_string()));
    }

    #[test]
    fn restored_history_is_capped_too() {
        let mut cl = CmdLine::default();
        cl.restore_history((0..HISTORY_CAP + 5).map(|i| format!("c{i}")).collect());
        assert_eq!(cl.history().len(), HISTORY_CAP);
        assert_eq!(cl.history()[0], format!("c{}", 5));
    }

    #[test]
    fn parse_cd_variants() {
        assert_eq!(parse_cd("cd"), Some(""));
        assert_eq!(parse_cd("cd /tmp"), Some("/tmp"));
        assert_eq!(parse_cd("cd   sub dir"), Some("sub dir"));
        assert_eq!(parse_cd("cd \"my dir\""), Some("my dir"));
        assert_eq!(parse_cd("cdrecord -x"), None);
        assert_eq!(parse_cd("ls"), None);
    }

    #[test]
    fn normalize_resolves_dots_lexically() {
        assert_eq!(normalize(Path::new("/a/b/..")), PathBuf::from("/a"));
        assert_eq!(normalize(Path::new("/a/./b")), PathBuf::from("/a/b"));
        assert_eq!(normalize(Path::new("/../..")), PathBuf::from("/"));
        assert_eq!(normalize(Path::new("/a/b/../../c")), PathBuf::from("/c"));
    }

    #[test]
    fn shell_quote_only_when_needed() {
        assert_eq!(shell_quote("plain-name.txt"), "plain-name.txt");
        assert_eq!(shell_quote("with space"), "'with space'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn cmdline_history_round_trip() {
        let mut cl = CmdLine::default();
        cl.push_history("first");
        cl.push_history("second");
        cl.push_history("second"); // consecutive duplicate is dropped
        assert_eq!(cl.history.len(), 2);
        cl.value = "draft".into();
        cl.hist_prev();
        assert_eq!(cl.value, "second");
        cl.hist_prev();
        assert_eq!(cl.value, "first");
        cl.hist_next();
        assert_eq!(cl.value, "second");
        cl.hist_next();
        assert_eq!(cl.value, "draft"); // back to the stashed draft
        assert_eq!(cl.hist_pos, None);
    }

    fn macros() -> Macros {
        Macros {
            here: ("a.txt".into(), "a.txt b.txt".into(), "/here".into()),
            there: ("z.md".into(), String::new(), "/there".into()),
            clip: "yanked".into(),
        }
    }

    fn expanded(template: &str) -> String {
        match expand_template(template, &macros()).0 {
            Expanded::Done(out) => out,
            Expanded::Ask { before, .. } => panic!("asked, got {before}"),
        }
    }

    #[test]
    fn mcs_macros_all_expand() {
        assert_eq!(expanded("e %f in %d"), "e a.txt in /here");
        assert_eq!(expanded("%F %D"), "z.md /there");
        assert_eq!(expanded("%t | %T"), "a.txt b.txt | ");
        // %s is the marked files, or the cursor file when none are
        assert_eq!(expanded("%s"), "a.txt b.txt");
        assert_eq!(expanded("%S"), "z.md");
        assert_eq!(expanded("%q"), "yanked");
        // a literal percent, and anything rcmd does not know, survive
        assert_eq!(expanded("100%% of %z"), "100% of %z");
        assert_eq!(expanded("trailing %"), "trailing %");
    }

    #[test]
    fn u_and_capital_u_spend_the_marks() {
        let (_, untag) = expand_template("rm %u", &macros());
        assert_eq!(untag, [true, false]);
        let (_, untag) = expand_template("rm %U", &macros());
        assert_eq!(untag, [false, true]);
        // %t and %T leave them alone - that is the whole difference
        let (_, untag) = expand_template("rm %t %T", &macros());
        assert_eq!(untag, [false, false]);
    }

    #[test]
    fn a_question_stops_the_expansion_where_it_stands() {
        let (out, _) = expand_template("tar %{Options} %f", &macros());
        match out {
            Expanded::Ask {
                question,
                before,
                rest,
            } => {
                assert_eq!(question, "Options");
                assert_eq!(before, "tar ");
                // what follows is still a template - the answer cannot
                // be allowed to bring its own macros, so the two halves
                // stay apart until the very end
                assert_eq!(rest, " %f");
            }
            Expanded::Done(out) => panic!("did not ask, got {out}"),
        }
        // an unclosed one is not a question, just text
        assert_eq!(expanded("echo %{oops"), "echo %{oops");
    }

    #[test]
    fn a_letter_walks_the_rows_that_start_with_it() {
        let rows = ["mc", "dark", "mc46", "midnight", "sand"];
        let m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE);
        // from the first `m`, the next one - not the first one again
        assert!(matches!(pick_key(&rows, 0, m), PickKey::Move(2)));
        assert!(matches!(pick_key(&rows, 2, m), PickKey::Move(3)));
        // ...and round the end back to the top
        assert!(matches!(pick_key(&rows, 3, m), PickKey::Move(0)));
        let z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE);
        assert!(matches!(pick_key(&rows, 1, z), PickKey::Ignored));
    }
}
