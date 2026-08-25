use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

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
use rcmd_core::glob::glob_match;
use rcmd_core::mask::{self, Mask};
use rcmd_core::panel::{ListMode, LoadKind, Panel, SortKey};
use rcmd_core::remote::{self, ConnectEvent, ConnectHandle, ConnectReply};
use rcmd_core::sftp::{self, SftpUrl};
use rcmd_core::tree::Tree;

use crate::format::{self, Field, Format, Item};
use rcmd_core::vfs::{FsProvider, LocalFs, RemoteFs};
use rcmd_core::view::{FileView, Search, SearchKind};

use crate::config::{Config, HotEntry};
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

pub enum InputAction {
    CopyTo {
        sources: Vec<PathBuf>,
    },
    MoveTo {
        sources: Vec<PathBuf>,
    },
    Mkdir,
    /// F9 → Command → Remote link: the value is an sftp:// or ftp:// URL.
    SftpConnect,
    /// S-F4: the value is the file to edit (created on first save).
    EditNew,
    /// M-c: the value is a cd target (path or sftp:// URL).
    QuickCd,
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

/// Alt+F7 find dialog: filename glob + optional content substring.
pub struct FindDialog {
    /// Where the walk starts; the panel's directory unless changed.
    pub start: String,
    pub start_cursor: usize,
    pub name: String,
    pub name_cursor: usize,
    pub content: String,
    pub content_cursor: usize,
    /// The filename is a glob; off = a regular expression.
    pub shell: bool,
    pub case_sensitive: bool,
    pub whole_words: bool,
    /// The content is a regular expression, matched line by line.
    pub regex: bool,
    pub all_charsets: bool,
    pub skip_hidden: bool,
    pub follow_links: bool,
    /// Skip gitignored trees when searching inside a work tree.
    pub skip_ignored: bool,
    /// Focused row: the three fields, then the switches, then
    /// [`FIND_ROWS`] for the button row.
    pub row: usize,
    pub ok: bool,
}

/// The three text fields, in row order.
pub const FIND_FIELDS: usize = 3;
/// The switches, in row order after the fields: label and which field
/// of the dialog they tick.
pub const FIND_SWITCHES: &[&str] = &[
    "Shell patterns (name; off = regular expression)",
    "Case sensitive (content)",
    "Whole words (content)",
    "Regular expression (content)",
    "All charsets (content)",
    "Skip hidden files",
    "Follow symlinks",
    "Skip gitignored files",
];
/// Rows before the button row.
pub const FIND_ROWS: usize = FIND_FIELDS + FIND_SWITCHES.len();

impl FindDialog {
    pub fn switch(&self, index: usize) -> bool {
        match index {
            0 => self.shell,
            1 => self.case_sensitive,
            2 => self.whole_words,
            3 => self.regex,
            4 => self.all_charsets,
            5 => self.skip_hidden,
            6 => self.follow_links,
            _ => self.skip_ignored,
        }
    }

    fn toggle(&mut self) {
        match self.row.checked_sub(FIND_FIELDS) {
            Some(0) => self.shell = !self.shell,
            Some(1) => self.case_sensitive = !self.case_sensitive,
            Some(2) => self.whole_words = !self.whole_words,
            Some(3) => self.regex = !self.regex,
            Some(4) => self.all_charsets = !self.all_charsets,
            Some(5) => self.skip_hidden = !self.skip_hidden,
            Some(6) => self.follow_links = !self.follow_links,
            Some(7) => self.skip_ignored = !self.skip_ignored,
            _ => {}
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

    /// The field the cursor is in, if it is in one.
    fn field(&mut self) -> Option<(&mut String, &mut usize)> {
        match self.row {
            0 => Some((&mut self.start, &mut self.start_cursor)),
            1 => Some((&mut self.name, &mut self.name_cursor)),
            2 => Some((&mut self.content, &mut self.content_cursor)),
            _ => None,
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
    /// Absolute paths, in the order they were found.
    pub rows: Vec<PathBuf>,
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

/// The buttons along the bottom, in mc's order.
pub const FIND_BUTTONS: &[&str] = &["Chdir", "Again", "Panelize", "View", "Edit", "Quit"];

impl FindResults {
    /// The path as the window shows it: relative to where the search
    /// started, which is what makes a long list readable.
    pub fn label_of(&self, at: usize) -> String {
        let path = &self.rows[at];
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
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
    pub value: String,
    /// Cursor position in characters, not bytes.
    pub cursor: usize,
    pub action: InputAction,
}

/// F5/F6: MC's copy/move form - where the files go, the switches that
/// change what "copy" means, and OK / Background / Cancel.
pub struct TransferDialog {
    pub title: String,
    /// MC's source mask: which of the marked files take part, and what
    /// their wildcards capture for the destination to spend.
    pub mask: String,
    pub mask_cursor: usize,
    pub dest: String,
    /// Cursor position in the destination, in characters.
    pub cursor: usize,
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
pub const CHMOD_BUTTONS: &[&str] = &["Set", "Set marked", "Clear marked", "Cancel"];

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
    pub target: String,
    pub target_cursor: usize,
    pub name: String,
    pub name_cursor: usize,
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
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    Delete,
    Quit,
    /// Dropping a hotlist entry. There is one dialog slot, so the
    /// confirm replaces the hotlist and puts it back either way.
    HotlistDelete {
        index: usize,
    },
    /// Enter about to run an `[[open]]` command.
    Execute,
}

pub enum Dialog {
    Input(InputDialog),
    Confirm(ConfirmDialog),
    /// Directory hotlist; the payload is the selected row.
    Hotlist(usize),
    Find(Box<FindDialog>),
    /// F2 user menu ([[commands]]); the payload is the selected row.
    UserMenu(usize),
    Options(OptionsDialog),
    /// Bulk rename: what the edited buffer asks for, awaiting Yes/No.
    RenamePreview(RenamePreview),
    /// Background-jobs list; the payload is the selected row.
    Jobs(usize),
    /// M-e: the panel's codepage, with the row it is on.
    Charset(usize),
    /// C-x d: how to compare the two listings, with the row it is on.
    Compare(usize),
    /// External panelize: the saved commands and the one being typed.
    Panelize(Box<PanelizeDialog>),
    /// Select / unselect group, and the panel filter.
    Pattern(Box<PatternDialog>),
    /// MC's find results window.
    FindResults(Box<FindResults>),
    /// M-h: the command-line history; the payload is the selected row.
    History(usize),
    /// F9 > Command > Directory tree. Enter here changes the *current*
    /// panel and closes, which is mc's rule for the dialog - the tree
    /// listing mode moves the other panel instead.
    Tree(Box<Tree>),
    /// F5/F6: the copy/move form.
    Transfer(Box<TransferDialog>),
    /// C-x c: the chmod bit matrix.
    Chmod(Box<ChmodDialog>),
    /// C-x o: the chown pick lists.
    Chown(Box<ChownDialog>),
    /// C-x l / s / v / C-s: the link form.
    Link(Box<LinkDialog>),
    /// C-x a: what the panels are sitting on that is not the local
    /// filesystem.
    Vfs(VfsDialog),
}

/// One line of the active VFS list.
pub struct VfsRow {
    /// What the row says.
    pub label: String,
    /// Where Enter goes: the `sftp://` prefix, or the archive's path.
    pub target: String,
    /// Which panels are on it right now - 0 left, 1 right.
    pub used_by: Vec<usize>,
    /// An SFTP connection can outlive the panel that opened it; an
    /// archive cannot, so only the first kind is ever idle.
    pub remote: bool,
}

pub struct VfsDialog {
    pub rows: Vec<VfsRow>,
    pub selected: usize,
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
    pub value: String,
    pub cursor: usize,
    /// Which saved preset the cursor is on.
    pub row: usize,
    /// The list has the focus rather than the command field.
    pub on_list: bool,
    /// Ctrl+S: the field is asking for a name to save the command as.
    pub naming: Option<String>,
}

/// MC's select / unselect / filter dialog: a pattern and the three
/// answers that change what it means. One form for all three, because
/// in mc they are one dialog with a different title.
pub struct PatternDialog {
    pub title: String,
    pub value: String,
    pub cursor: usize,
    pub shell: bool,
    pub case_sensitive: bool,
    pub files_only: bool,
    /// Focused row: 0 is the pattern, then one per switch, then
    /// [`PATTERN_ROWS`] for the button row.
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

/// The pattern field plus the three switches.
pub const PATTERN_ROWS: usize = 4;

impl PatternDialog {
    /// The core's shape of the same question.
    pub fn to_pattern(&self) -> rcmd_core::pattern::Pattern {
        rcmd_core::pattern::Pattern {
            text: self.value.trim().to_string(),
            shell: self.shell,
            case_sensitive: self.case_sensitive,
            files_only: self.files_only,
        }
    }

    fn toggle(&mut self) {
        match self.row {
            1 => self.files_only = !self.files_only,
            2 => self.case_sensitive = !self.case_sensitive,
            3 => self.shell = !self.shell,
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
    StatusLine,
    CommandLine,
    KeyBar,
    Hidden,
    Lynx,
    Mouse,
    Watch,
    Git,
    ConfirmDelete,
    ConfirmOverwrite,
    ConfirmExit,
    ConfirmHotlistDelete,
    ConfirmExecute,
    Subshell,
    ExternalEditor,
    DarkTheme,
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
    OptRow::Check(Opt::StatusLine, "Status line"),
    OptRow::Check(Opt::MiniStatus, "Mini status (per panel)"),
    OptRow::Check(Opt::CommandLine, "Command line"),
    OptRow::Check(Opt::KeyBar, "Key bar"),
    OptRow::Head("Panel"),
    OptRow::Check(Opt::Hidden, "Show hidden files"),
    OptRow::Check(Opt::Lynx, "Lynx-like motion"),
    OptRow::Check(Opt::Mouse, "Mouse support"),
    OptRow::Check(Opt::Watch, "Auto-reload panels"),
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
    OptRow::Head("Appearance"),
    OptRow::Radio(Opt::DarkTheme, "Theme", "mc", "dark"),
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
}

enum Exec {
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
    Search {
        value: String,
        cursor: usize,
    },
    ReplaceFind {
        value: String,
        cursor: usize,
    },
    ReplaceWith {
        pattern: String,
        value: String,
        cursor: usize,
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
    let last = CHARSET_ROWS.len() - 1;
    match key.code {
        KeyCode::Esc => PickKey::Close,
        KeyCode::Enter => PickKey::Chose(row),
        KeyCode::Up => PickKey::Move(row.saturating_sub(1)),
        KeyCode::Down => PickKey::Move((row + 1).min(last)),
        KeyCode::PageUp => PickKey::Move(row.saturating_sub(10)),
        KeyCode::PageDown => PickKey::Move((row + 10).min(last)),
        KeyCode::Home => PickKey::Move(0),
        KeyCode::End => PickKey::Move(last),
        // a letter jumps to the first codepage starting with it
        KeyCode::Char(c) => {
            let c = c.to_ascii_lowercase();
            match CHARSET_ROWS.iter().position(|label| {
                label
                    .chars()
                    .next()
                    .is_some_and(|f| f.to_ascii_lowercase() == c)
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

const EDIT_FILE_MENU: &[EditMenuEntry] = &[
    Some(("&Save", "F2", EditMenuAction::Key(EA::Save))),
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
    pub value: String,
    pub cursor: usize,
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
            pattern: self.value.trim().to_string(),
            kind: self.kind,
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            backwards: self.backwards,
            // the dialog does not ask; the viewer's mode answers
            nroff: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.value.trim().is_empty()
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
    Panelize,
    CompareDirs,
    DirSize,
    View,
    Edit,
    Copy,
    Move,
    Mkdir,
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
    Sort(SortKey),
    SortReverse,
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
    /// Shift+F3: the internal viewer without any [[view]] filter.
    ViewRaw,
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
            Some(("&Edit", "F4", Action::Edit)),
            Some(("&Copy...", "F5", Action::Copy)),
            Some(("&Move/rename...", "F6", Action::Move)),
            Some(("&Bulk rename (editor)...", "", Action::BulkRename)),
            Some(("Ma&ke directory...", "F7", Action::Mkdir)),
            Some(("&Delete (trash)", "F8", Action::Delete)),
            Some(("Delete &permanently", "S-F8", Action::DeletePerm)),
            None,
            Some(("&Select group...", "+", Action::SelectGroup)),
            Some(("&Unselect group...", "-", Action::UnselectGroup)),
            Some(("&Invert selection", "*", Action::InvertSelection)),
            None,
            Some(("Directory si&ze", "C-spc", Action::DirSize)),
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
            Some(("&Compare directories", "C-x d", Action::CompareDirs)),
            Some(("Compare fi&les", "", Action::CompareFiles)),
            Some(("&Open shell", "C-o", Action::Shell)),
            Some(("S&wap panels", "C-u", Action::SwapPanels)),
            Some(("Toggle hidde&n files", "M-.", Action::ToggleHidden)),
            None,
            Some(("Other panel: &same dir", "M-i", Action::OtherSameDir)),
            Some(("Other panel: this &dir", "M-o", Action::OtherOpenDir)),
            None,
            Some(("&Jobs...", "C-x j", Action::Jobs)),
            Some(("Acti&ve VFS list...", "C-x a", Action::VfsList)),
            Some(("Command histor&y...", "M-h", Action::HistoryList)),
            // not "&list": Compare fi&les already spends the l, and a
            // second entry with the same letter is one nobody can reach
            Some(("Screen l&ist...", "M-`", Action::ScreenList)),
        ],
    ),
    (
        "&Options",
        &[Some(("&Panel options...", "", Action::Options))],
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
    Some(("Re&verse sort", "", Action::SortReverse)),
    None,
    // "Filter" cannot take a letter of its own here: f, i, l, t, e and
    // r are all spoken for by an entry above or by a menu title, and a
    // panel-menu entry that shadows File, Command, Options or Right
    // would make that menu unreachable by letter
    Some(("&Glob filter...", "C-f", Action::Filter)),
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

/// Two files side by side, mc's Compare files. The rows are the two
/// files paired up by the diff; a row missing one side is a line that
/// only one of them has.
pub struct DiffView {
    pub left_title: String,
    pub right_title: String,
    pub left: Vec<String>,
    pub right: Vec<String>,
    pub rows: Vec<rcmd_core::diff::Row>,
    /// Where the changes are, for "next difference".
    pub blocks: Vec<(usize, usize)>,
    pub top: usize,
    /// Horizontal scroll, in characters, shared by both sides.
    pub col: usize,
    /// Rows on screen; updated on every draw, drives paging.
    pub height: usize,
    pub note: Option<String>,
}

impl DiffView {
    pub fn line(&self, row: usize, right: bool) -> Option<&str> {
        let row = self.rows.get(row)?;
        let (at, side) = match right {
            true => (row.right?, &self.right),
            false => (row.left?, &self.left),
        };
        side.get(at).map(String::as_str)
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
            true => self
                .blocks
                .iter()
                .find(|(start, _)| *start > here + 2)
                .copied(),
            false => self
                .blocks
                .iter()
                .rev()
                .find(|(start, _)| *start + 2 < here)
                .copied(),
        };
        match found {
            Some((start, _)) => self.top = start.saturating_sub(2),
            None if self.blocks.is_empty() => self.note = Some(" the files are identical ".into()),
            None => self.note = Some(" no more differences that way ".into()),
        }
    }
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
            Screen::Diff(d) => format!("Diff  {} | {}", d.left_title, d.right_title),
        }
    }
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
        if self.history.last().map(String::as_str) != Some(cmd) {
            self.history.push(cmd.to_string());
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
        self.history = history;
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
    pub quick_search: Option<String>,
    pub find: Option<FindState>,
    pub connect: Option<ConnectState>,
    /// Live remote connections by URL prefix; weak so that leaving a
    /// remote directory on both panels closes the connection.
    connections: Vec<(String, Weak<dyn RemoteFs>)>,
    remote_edit: Option<RemoteEdit>,
    du: Option<DuJob>,
    compare: Option<CompareState>,
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
    pub areas: Areas,
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
    pending_exec: Option<Exec>,
    /// The persistent subshell (PLAN3 R1); None = plain exec fallback,
    /// either by `subshell = false` or because the spawn failed.
    subshell: Option<Subshell>,
    pub quit: bool,
}

impl App {
    pub fn new(dirs: &[PathBuf], config: Config, mut warnings: Vec<String>) -> Result<Self> {
        let cwd = std::env::current_dir().context("cannot determine current directory")?;
        let dir_at = |i: usize| -> Result<PathBuf> {
            match dirs.get(i) {
                Some(dir) => std::fs::canonicalize(dir)
                    .with_context(|| format!("cannot open directory {}", dir.display())),
                None => Ok(cwd.clone()),
            }
        };
        let left_dir = dir_at(0)?;
        let right_dir = if dirs.len() > 1 {
            dir_at(1)?
        } else {
            left_dir.clone()
        };
        let mut left = Panel::new(left_dir.clone())
            .with_context(|| format!("cannot read directory {}", left_dir.display()))?;
        let mut right = Panel::new(right_dir.clone())
            .with_context(|| format!("cannot read directory {}", right_dir.display()))?;
        for panel in [&mut left, &mut right] {
            panel.show_hidden = config.show_hidden;
            panel.sort_key = config::sort_key_from_name(&config.sort_key);
            panel.sort_reverse = config.sort_reverse;
            panel.list_mode = config::list_mode_from_name(&config.listing);
            let _ = panel.reload();
        }
        let (keymap, keymap_warnings) = full_keymap(&config);
        warnings.extend(keymap_warnings);
        let (contexts, _) = config.key_contexts();
        let (viewer_keys, viewer_warnings) = keymap::build_viewer(&contexts.viewer);
        let (editor_keys, editor_warnings) = keymap::build_editor(&contexts.editor);
        warnings.extend(viewer_warnings);
        warnings.extend(editor_warnings);
        let watch = if config.watch {
            let (watch, warning) = build_watch();
            warnings.extend(warning);
            watch
        } else {
            None
        };
        let subshell = if config.subshell {
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
            areas: Areas::default(),
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
            pending_exec: None,
            subshell,
            quit: false,
        })
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut last_frame = Instant::now();
        while !self.quit {
            self.drain_job();
            self.drain_find();
            self.drain_connect();
            self.drain_du();
            self.drain_compare();
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
            let busy = !self.jobs.is_empty()
                || self.compare.is_some()
                || self.panelize.is_some()
                || self.find.is_some()
                || self.connect.is_some()
                || self.du.is_some()
                || loading
                || watch_pending
                || self.esc_at.is_some()
                || self.subshell.as_ref().is_some_and(|s| !s.ready())
                || self.viewer().is_some_and(|v| v.follow);
            // ...and a frame is drawn when something changed, when
            // something is moving, or once in a while regardless - the
            // last of those is insurance against a state change that
            // forgot to say so, and at one frame every two seconds it
            // costs nothing.
            if self.dirty || busy || last_frame.elapsed() >= IDLE_FRAME {
                if self.repaint {
                    self.repaint = false;
                    terminal.clear()?;
                }
                terminal.draw(|frame| ui::draw(frame, self))?;
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
                    Event::Mouse(mouse) => self.on_mouse(mouse),
                    Event::Resize(cols, rows) => {
                        if let Some(sub) = self.subshell.as_mut() {
                            sub.resize(cols, rows);
                        }
                    }
                    _ => {}
                }
            }
            if let Some(exec) = self.pending_exec.take() {
                if self.subshell.is_some() && !matches!(exec, Exec::Quiet(_)) {
                    // Ctrl+O and typed commands live in the subshell;
                    // Quiet (editors, openers) stays a one-shot child
                    self.subshell_session(terminal, exec)?;
                } else {
                    self.execute(terminal, exec)?;
                    self.finish_remote_edit();
                }
            }
            // quitting with jobs still running would orphan them
            if self.quit && !self.jobs.is_empty() {
                self.quit = false;
                self.status = Some(format!(
                    " {} job(s) still running - C-x j lists them (Esc/c cancels) ",
                    self.jobs.len()
                ));
            }
        }
        for job in &self.jobs {
            job.handle.cancel();
        }
        if let Some(find) = &self.find {
            find.handle.cancel();
        }
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
                self.status = Some(format!(
                    " {}: {bytes} bytes in {files} file(s) ",
                    du.name.to_string_lossy()
                ));
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

    fn drain_find(&mut self) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let window = find.window;
        let mut done = None;
        let mut found: Vec<Box<rcmd_core::entry::Entry>> = Vec::new();
        while let Ok(event) = find.handle.events.try_recv() {
            match event {
                FindEvent::Match(entry) => {
                    find.count += 1;
                    found.push(entry);
                }
                FindEvent::Done { matches, scanned } => done = Some((matches, scanned)),
            }
        }
        let panel = find.panel;
        if window {
            // the window owns the list; closing it cancels the walk
            let Some(Dialog::FindResults(results)) = self.dialog.as_mut() else {
                if let Some(find) = self.find.take() {
                    find.handle.cancel();
                }
                return;
            };
            for entry in found {
                results.rows.push(results.root.join(&entry.name));
            }
            results.done = done;
        } else {
            for entry in found {
                self.panels[panel].entries.push(*entry);
            }
        }
        match done {
            Some((matches, scanned)) => {
                let mut find = self.find.take().expect("find present");
                if let Some(thread) = find.handle.thread.take() {
                    let _ = thread.join();
                }
                self.status = Some(format!(
                    " find: {matches} match(es), {scanned} entries scanned "
                ));
            }
            None => {
                self.status = Some(format!(" searching… {} found - Esc cancels ", find.count));
            }
        }
    }

    /// Start (or resume) a connection for the active panel. The scheme
    /// picks the protocol; everything after that - the password
    /// prompt, the cache, the panel switch - is the same either way.
    fn connect_remote(&mut self, input: &str) {
        if self.connect.is_some() {
            self.status = Some(" a connection attempt is already running ".into());
            return;
        }
        let handle = if input.starts_with("ftp://") {
            let Some(url) = FtpUrl::parse(input) else {
                self.status = Some(" bad URL - ftp://[user[:password]@]host[:port][/path] ".into());
                return;
            };
            match self.connection(&url.prefix()) {
                Some(fs) => remote::spawn_reuse(fs, url.path, url.host),
                None => ftp::spawn_connect(url),
            }
        } else {
            let fish = input.starts_with("fish://");
            let scheme = if fish { "fish" } else { "sftp" };
            let Some(url) = SftpUrl::parse_as(scheme, input) else {
                self.status = Some(format!(" bad URL - {scheme}://[user@]host[:port][/path] "));
                return;
            };
            match (self.connection(&url.prefix()), fish) {
                (Some(fs), _) => remote::spawn_reuse(fs, url.path, url.host),
                (None, true) => fish::spawn_connect(url),
                (None, false) => sftp::spawn_connect(url),
            }
        };
        self.status = Some(format!(" connecting to {}… - Esc cancels ", handle.host));
        self.connect = Some(ConnectState {
            handle,
            panel: self.active,
            ask: None,
        });
    }

    /// What the panels are sitting on that is not the local filesystem,
    /// plus any SFTP connection still cached. An archive belongs to the
    /// panel that opened it and disappears with it; a connection is
    /// kept so that going back to the same host does not mean logging
    /// in again, which is why one can be listed with no panel on it.
    fn vfs_rows(&mut self) -> Vec<VfsRow> {
        self.connections.retain(|(_, weak)| weak.strong_count() > 0);
        let mut rows: Vec<VfsRow> = Vec::new();
        for (prefix, _) in &self.connections {
            let used_by = (0..self.panels.len())
                .filter(|i| self.panels[*i].remote.as_deref() == Some(prefix.as_str()))
                .collect();
            rows.push(VfsRow {
                label: prefix.clone(),
                target: prefix.clone(),
                used_by,
                remote: true,
            });
        }
        for (index, panel) in self.panels.iter().enumerate() {
            let Some(archive) = &panel.archive else {
                continue;
            };
            let target = archive.display().to_string();
            if let Some(row) = rows
                .iter_mut()
                .find(|row| !row.remote && row.target == target)
            {
                row.used_by.push(index);
                continue;
            }
            rows.push(VfsRow {
                label: format!("{target}://"),
                target,
                used_by: vec![index],
                remote: false,
            });
        }
        rows
    }

    /// Send whichever panels are on this entry back to a local
    /// directory, and forget the connection if it was one. mc calls
    /// this "free"; what it frees is the panel as much as the handle.
    fn free_vfs(&mut self, row: &VfsRow) {
        for index in row.used_by.iter().copied() {
            let home = self.panels[index].local_cwd();
            if let Err(err) = self.panels[index].to_local(home) {
                self.status = Some(format!(" {err} "));
                return;
            }
        }
        if row.remote {
            self.connections.retain(|(prefix, _)| prefix != &row.target);
        }
        self.status = Some(format!(" freed {} ", row.label));
    }

    /// Look up a live connection by URL prefix, dropping dead ones.
    fn connection(&mut self, prefix: &str) -> Option<Arc<dyn RemoteFs>> {
        self.connections.retain(|(_, weak)| weak.strong_count() > 0);
        self.connections
            .iter()
            .find(|(p, _)| p == prefix)
            .and_then(|(_, weak)| weak.upgrade())
    }

    fn drain_connect(&mut self) {
        let Some(connect) = self.connect.as_mut() else {
            return;
        };
        while let Ok(event) = connect.handle.events.try_recv() {
            match event {
                ConnectEvent::Info(msg) => self.status = Some(format!(" {msg} ")),
                ConnectEvent::AskHostKey { fingerprint } => {
                    connect.ask = Some(ConnectAsk::HostKey {
                        fingerprint,
                        yes: false, // safe default
                    });
                }
                ConnectEvent::AskPassword { prompt, echo } => {
                    connect.ask = Some(ConnectAsk::Password {
                        prompt,
                        value: String::new(),
                        echo,
                    });
                }
                ConnectEvent::Ok { fs, start, entries } => {
                    let connect = self.connect.take().expect("connect present");
                    let prefix = fs.prefix().to_string();
                    self.connections.retain(|(p, _)| p != &prefix);
                    self.connections.push((prefix.clone(), Arc::downgrade(&fs)));
                    self.panels[connect.panel].adopt_remote(fs, prefix.clone(), start, entries);
                    self.status = Some(format!(" connected to {prefix} "));
                    return;
                }
                ConnectEvent::Err(msg) => {
                    self.connect = None;
                    self.status = Some(format!(" sftp: {msg} "));
                    return;
                }
            }
        }
    }

    fn on_connect_key(&mut self, key: KeyEvent) {
        let Some(connect) = self.connect.as_mut() else {
            return;
        };
        match connect.ask.as_mut() {
            None => {
                if key.code == KeyCode::Esc {
                    // dropping the handle closes the reply channel; the
                    // worker unblocks and gives up
                    self.connect = None;
                    self.status = Some(" connection cancelled ".into());
                }
            }
            Some(ConnectAsk::HostKey { yes, .. }) => {
                let reply = match key.code {
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                        *yes = !*yes;
                        None
                    }
                    KeyCode::Enter => Some(*yes),
                    KeyCode::Char('y' | 'Y') => Some(true),
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
                    _ => None,
                };
                if let Some(accept) = reply {
                    let _ = connect.handle.replies.send(ConnectReply::Accept(accept));
                    connect.ask = None;
                    if !accept {
                        self.connect = None;
                        self.status = Some(" host key rejected ".into());
                    }
                }
            }
            Some(ConnectAsk::Password { value, .. }) => match key.code {
                KeyCode::Esc => {
                    let _ = connect.handle.replies.send(ConnectReply::Cancel);
                    self.connect = None;
                    self.status = Some(" connection cancelled ".into());
                }
                KeyCode::Enter => {
                    let password = std::mem::take(value);
                    let _ = connect
                        .handle
                        .replies
                        .send(ConnectReply::Password(password));
                    connect.ask = None;
                }
                code => {
                    let mut cursor = value.chars().count();
                    edit_line(value, &mut cursor, code, key.modifiers);
                }
            },
        }
    }

    /// After F4 on a remote file: upload the scratch copy back if the
    /// editor modified it, then clean up.
    /// The external editor's copy, once the child has exited.
    fn finish_remote_edit(&mut self) {
        let Some(edit) = self.remote_edit.take() else {
            return;
        };
        self.upload_remote_edit(edit);
    }

    /// Send a scratch copy back where it came from, if it changed.
    fn upload_remote_edit(&mut self, edit: RemoteEdit) {
        let mtime_now = std::fs::metadata(&edit.temp)
            .and_then(|m| m.modified())
            .ok();
        if mtime_now != edit.mtime_before {
            let uploaded = (|| -> std::io::Result<()> {
                let writer = edit
                    .fs
                    .writer()
                    .ok_or_else(|| std::io::Error::other("read-only filesystem"))?;
                let mut input = std::fs::File::open(&edit.temp)?;
                let mut output = writer.open_write(&edit.remote_path)?;
                std::io::copy(&mut input, &mut output)?;
                Ok(())
            })();
            self.status = Some(match &uploaded {
                Ok(()) => format!(" uploaded {} ", edit.remote_path.display()),
                Err(err) => format!(
                    " upload failed: {err} - local copy kept at {} ",
                    edit.temp.display()
                ),
            });
            if uploaded.is_err() {
                return; // keep the scratch file for rescue
            }
        }
        let _ = std::fs::remove_file(&edit.temp);
    }

    /// Leave the TUI, run a command or an interactive shell in the active
    /// panel's directory, then restore the TUI and reload both panels.
    fn execute(&mut self, terminal: &mut DefaultTerminal, exec: Exec) -> Result<()> {
        use std::io::Write as _;
        use std::os::unix::process::CommandExt as _;

        let cwd = self.panels[self.active].local_cwd();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        if self.config.mouse {
            set_mouse_capture(false);
        }
        ratatui::restore();
        // Shell-style job control: the child runs in its own foreground
        // process group, so Ctrl+C/Ctrl+Z hit it and never rcmd. We ignore
        // the terminal signals meanwhile (SIGTTOU also lets us tcsetpgrp
        // back from the "background").
        let old_sigint = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
        let old_sigtstp = unsafe { libc::signal(libc::SIGTSTP, libc::SIG_IGN) };
        let old_sigttou = unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) };
        let mut command = std::process::Command::new(&shell);
        match &exec {
            Exec::Command(cmd) => {
                println!("{}$ {cmd}", cwd.display());
                command.arg("-c").arg(cmd);
            }
            Exec::Quiet(cmd) => {
                command.arg("-c").arg(cmd);
            }
            Exec::Shell => {}
        }
        command.current_dir(&cwd);
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                libc::signal(libc::SIGTTIN, libc::SIG_DFL);
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        match command.spawn() {
            Ok(child) => {
                let pid = child.id() as libc::pid_t;
                unsafe {
                    libc::setpgid(pid, pid); // idempotent with pre_exec's
                    libc::tcsetpgrp(libc::STDIN_FILENO, pid);
                }
                loop {
                    let mut status: libc::c_int = 0;
                    let rc = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
                    if rc < 0 {
                        break;
                    }
                    if libc::WIFSTOPPED(status) {
                        // Ctrl+Z: we cannot park a stopped child (no jobs
                        // table), so take the tty, say so, and resume it.
                        unsafe {
                            libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
                        }
                        println!("[rcmd has no job control - resuming]");
                        unsafe {
                            libc::tcsetpgrp(libc::STDIN_FILENO, pid);
                            libc::kill(-pid, libc::SIGCONT);
                        }
                        continue;
                    }
                    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) != 0 {
                        unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) };
                        println!("[exit code {}]", libc::WEXITSTATUS(status));
                    } else if libc::WIFSIGNALED(status) {
                        unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) };
                        println!("[killed by signal {}]", libc::WTERMSIG(status));
                    }
                    break;
                }
                unsafe {
                    libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
                }
            }
            Err(err) => println!("cannot run {shell}: {err}"),
        }
        if matches!(exec, Exec::Command(_)) {
            print!("Press Enter to return to rcmd...");
            let _ = std::io::stdout().flush();
            let mut sink = String::new();
            let _ = std::io::stdin().read_line(&mut sink);
        }
        unsafe {
            libc::signal(libc::SIGINT, old_sigint);
            libc::signal(libc::SIGTSTP, old_sigtstp);
            libc::signal(libc::SIGTTOU, old_sigttou);
        }
        *terminal = ratatui::init();
        if self.config.mouse {
            set_mouse_capture(true);
        }
        let _ = terminal.clear();
        for panel in &mut self.panels {
            let _ = panel.reload();
        }
        self.git_refresh();
        Ok(())
    }

    /// Pump the hidden subshell every loop tick: collect output for the
    /// next Ctrl+O, keep the hook channel drained, respawn on `exit`.
    fn subshell_tick(&mut self) {
        let Some(sub) = self.subshell.as_mut() else {
            return;
        };
        sub.pump(false);
        let note = sub.note.take();
        let failed = sub.failed;
        if let Some(note) = note {
            self.status = Some(note);
            self.dirty = true;
        }
        if failed {
            self.subshell = None;
            self.dirty = true;
        }
    }

    /// Ctrl+O or a typed command while the persistent subshell lives:
    /// leave the alternate screen (the shell owns the primary one - its
    /// scrollback IS MC's "output screen") and pass keys through raw
    /// until Ctrl+O comes back or the fed command finishes.
    fn subshell_session(&mut self, terminal: &mut DefaultTerminal, exec: Exec) -> Result<()> {
        use std::io::Write as _;

        let mut pending_cmd = match exec {
            Exec::Command(cmd) => Some(cmd),
            _ => None,
        };
        let panel = &self.panels[self.active];
        let panel_dir = panel.is_local().then(|| panel.cwd.clone());
        let Some(sub) = self.subshell.as_mut() else {
            return Ok(());
        };
        // a busy shell can't take a command (MC says the same); a shell
        // still starting up (slow rc files, compinit) is worth waiting
        // for - the session shows it booting
        if pending_cmd.is_some() && !sub.ready() && !sub.starting() {
            self.status = Some(" the shell is already running a command - Ctrl+O shows it ".into());
            return Ok(());
        }
        sub.debug(&format!(
            "session start: cmd={pending_cmd:?} panel_dir={panel_dir:?} starting={}",
            sub.starting()
        ));
        let sub = self.subshell.as_mut().expect("subshell present");

        if self.config.mouse {
            set_mouse_capture(false);
        }
        let mut out = std::io::stdout();
        ratatui::crossterm::execute!(out, LeaveAlternateScreen, cursor::Show)?;
        out.write_all(&sub.take_output())?;
        out.flush()?;

        // wait for the prompt before feeding anything; when the shell
        // was ready on entry this resolves on the first loop pass
        let mut start_wait = pending_cmd.is_some().then(Instant::now);
        // cd sync first if the panels moved since the last agreement
        // (the reverse sync happens on the way out)
        let mut inject = panel_dir.filter(|dir| *dir != sub.agreed && *dir != sub.cwd());
        let mut cd_wait: Option<Instant> = None;
        let mut awaiting = false;
        if pending_cmd.is_none() && sub.ready() {
            // plain Ctrl+O at a prompt: sync the directory right away
            if let Some(dir) = inject.take() {
                sub.feed_line(&format!("cd {}", shell_quote(&dir.to_string_lossy())));
            }
        }

        let mut size = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
        if let Some(sub) = self.subshell.as_mut() {
            sub.resize(size.0, size.1); // no-op unless it drifted
        }
        loop {
            let sub = self.subshell.as_mut().expect("subshell present");
            sub.pump(true);
            let bytes = sub.take_output();
            if !bytes.is_empty() {
                out.write_all(&bytes)?;
                out.flush()?;
            }
            if sub.failed {
                break;
            }
            if let Some(since) = start_wait {
                if sub.ready() {
                    start_wait = None;
                    if let Some(dir) = inject.take() {
                        sub.feed_line(&format!("cd {}", shell_quote(&dir.to_string_lossy())));
                        cd_wait = Some(Instant::now());
                    } else if let Some(cmd) = pending_cmd.take() {
                        sub.feed_line(&cmd);
                        awaiting = true;
                    }
                } else if since.elapsed() > Duration::from_secs(30) {
                    // cold-start compinit can take a long while; the
                    // user watches the shell boot and can Ctrl+O out
                    start_wait = None;
                    pending_cmd = None;
                    out.write_all(
                        b"\r\n[rcmd: the shell never reached a prompt - command not sent]\r\n",
                    )?;
                    out.flush()?;
                }
            }
            if let Some(since) = cd_wait
                && (sub.ready() || since.elapsed() > Duration::from_secs(2))
            {
                cd_wait = None;
                if let Some(cmd) = pending_cmd.take() {
                    sub.feed_line(&cmd);
                    awaiting = true;
                }
            }
            if awaiting && cd_wait.is_none() && sub.ready() {
                break; // the command finished - back to the panels, like MC
            }
            let mut fds = [
                libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: sub.master_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: sub.pipe_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            unsafe { libc::poll(fds.as_mut_ptr(), 3, 100) };
            if fds[0].revents & libc::POLLIN != 0 {
                let mut keys = [0u8; 1024];
                let n = unsafe { libc::read(0, keys.as_mut_ptr().cast(), keys.len()) };
                if n > 0 {
                    let keys = &keys[..n as usize];
                    // Ctrl+O never reaches the shell (MC-compatible -
                    // yes, that shadows nano's save inside the subshell)
                    if let Some(pos) = keys.iter().position(|&b| b == 0x0F) {
                        sub.feed(&keys[..pos]);
                        break;
                    }
                    sub.feed(keys);
                }
            }
            let now = ratatui::crossterm::terminal::size().unwrap_or(size);
            if now != size {
                size = now;
                sub.resize(now.0, now.1);
            }
        }

        ratatui::crossterm::execute!(out, EnterAlternateScreen)?;
        if self.config.mouse {
            set_mouse_capture(true);
        }
        let _ = terminal.clear();
        if let Some(sub) = self.subshell.as_mut() {
            if sub.failed {
                self.status = sub.note.take();
                self.subshell = None;
            } else {
                let _ = sub.note.take(); // it was visible on the output screen
                let cwd = sub.cwd();
                sub.debug("session exit");
                sub.agreed = cwd.clone();
                // the shell moved → the active panel follows
                let panel = &mut self.panels[self.active];
                if panel.is_local() && panel.cwd != cwd && cwd.is_dir() {
                    let _ = panel.cd(cwd);
                }
            }
        }
        for panel in &mut self.panels {
            let _ = panel.reload();
        }
        self.git_refresh();
        Ok(())
    }

    /// The job whose dialog is on screen (modal); background jobs run
    /// without one until they finish or need an answer.
    pub fn fg_job(&self) -> Option<&Job> {
        self.jobs.iter().find(|j| !j.background)
    }

    fn fg_job_mut(&mut self) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| !j.background)
    }

    fn drain_job(&mut self) {
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
                    JobEvent::AskError { path, message } => {
                        job.ask = Some(Ask::Error { path, message });
                        job.button = 0;
                        job.background = false;
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
                self.panels[job.src_panel].marked.clear();
            }
            any_done = true;
            self.status = Some(match (aborted, skipped) {
                (true, _) => format!(" aborted - {files_done} item(s) processed "),
                (false, 0) => format!(" done - {files_done} item(s) processed "),
                (false, n) => format!(" done - {files_done} item(s) processed, {n} skipped "),
            });
        }
        if any_done {
            for panel in &mut self.panels {
                let _ = panel.reload();
            }
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
    fn on_key(&mut self, key: KeyEvent) {
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

    fn on_mouse(&mut self, mouse: MouseEvent) {
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
            MouseEventKind::ScrollUp => self.on_wheel(mouse.column, mouse.row, -3),
            MouseEventKind::ScrollDown => self.on_wheel(mouse.column, mouse.row, 3),
            _ => {}
        }
    }

    fn on_click(&mut self, x: u16, y: u16, double: bool) {
        // Dialogs and prompts stay keyboard-only; the menu is the exception.
        if self.fg_job().is_some()
            || self.connect.is_some()
            || self.find.is_some()
            || self.dialog.is_some()
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
            // 10 buttons, 8 cells each ("nnLabel  ") → F1..F10
            let n = ((x - self.areas.keybar.x) / 8 + 1).min(10) as u8;
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
                .saturating_sub(2 + u16::from(self.config.show_mini_status));
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
        // 2 border+header rows on top, 1 border row at the bottom
        let content_y = area.y + 2;
        if y < content_y || y + 1 >= area.y + area.height {
            return;
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
                .saturating_sub(3 + u16::from(self.config.show_mini_status))
                .max(1) as usize;
            offset + col * rows + row
        } else {
            offset + row
        };
        if index < self.panels[side].entries.len() {
            self.panels[side].cursor = index;
            if double {
                self.enter_or_open();
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
                        Item::Field(Field::Mtime | Field::Atime | Field::Ctime, _) => {
                            Some(SortKey::Mtime)
                        }
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

    /// Ctrl+S type-ahead: printable keys refine the prefix, Ctrl+S jumps
    /// to the next match, anything else leaves the mode (and is handled
    /// normally).
    fn on_quick_search_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // In tree mode the search runs over the figure instead of the
        // listing. This is mc's rule for a tree *view*: plain characters
        // stay with the command line until Ctrl+S switches the search on.
        if self.panels[self.active].list_mode == ListMode::Tree {
            let mut close = true;
            if let Some(tree) = self.trees[self.active].as_mut() {
                close = false;
                match key.code {
                    KeyCode::Char('s') if ctrl => tree.search_next(),
                    KeyCode::Char(c) if !ctrl && !alt => {
                        tree.search_push(c);
                    }
                    KeyCode::Backspace => tree.search_pop(),
                    _ => {
                        tree.clear_search();
                        close = true;
                    }
                }
            }
            let search = self.trees[self.active].as_ref().map(|t| t.search.clone());
            self.quick_search = if close { None } else { search };
            // Esc and Enter only end the search; anything else was meant
            // for the panel underneath
            if close && !matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.on_panel_key(key);
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.quick_search = None,
            KeyCode::Char('s') if ctrl => {
                let prefix = self.quick_search.clone().unwrap_or_default();
                let panel = self.panel();
                if let Some(pos) = panel.find_prefix(&prefix, panel.cursor + 1) {
                    panel.cursor = pos;
                }
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                let mut prefix = self.quick_search.clone().unwrap_or_default();
                prefix.push(c);
                let panel = self.panel();
                // reject characters that match nothing, like MC
                if let Some(pos) = panel.find_prefix(&prefix, panel.cursor) {
                    panel.cursor = pos;
                    self.quick_search = Some(prefix);
                }
            }
            KeyCode::Backspace => {
                if let Some(prefix) = self.quick_search.as_mut() {
                    prefix.pop();
                }
            }
            _ => {
                self.quick_search = None;
                self.on_panel_key(key);
            }
        }
    }

    fn on_help_key(&mut self, key: KeyEvent) {
        let Some(help) = self.help.as_mut() else {
            return;
        };
        let rows = help.rows.max(1);
        let max_top = crate::ui::help_lines().saturating_sub(rows);
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) | KeyCode::F(10) | KeyCode::Char('q') => {
                self.help = None
            }
            KeyCode::Up => help.top = help.top.saturating_sub(1),
            KeyCode::Down => help.top = (help.top + 1).min(max_top),
            KeyCode::PageUp => help.top = help.top.saturating_sub(rows.saturating_sub(1)),
            KeyCode::PageDown => help.top = (help.top + rows.saturating_sub(1)).min(max_top),
            KeyCode::Home => help.top = 0,
            KeyCode::End => help.top = max_top,
            _ => {}
        }
    }

    fn on_viewer_key(&mut self, key: KeyEvent) {
        // looked up before the borrow below, which is the whole App
        let bound = self
            .viewer_keys
            .get(&(key.code, key.modifiers.difference(KeyModifiers::SHIFT)))
            .copied();
        let Some(v) = self.viewer_mut() else {
            return;
        };
        v.note = None;
        // the codepage picker
        if let Some(row) = v.charset_pick {
            match charset_pick_key(row, key) {
                PickKey::Move(to) => v.charset_pick = Some(to),
                PickKey::Close => v.charset_pick = None,
                PickKey::Chose(to) => {
                    v.charset_pick = None;
                    v.file.charset = charset_at(to);
                    // the text under every line changed
                    if let Some(hl) = v.hl.as_mut() {
                        hl.invalidate_from(0);
                    }
                    v.found = None;
                    v.note = Some(format!(" {} ", CHARSET_ROWS[to]));
                }
                PickKey::Ignored => {}
            }
            return;
        }
        // leaving with bytes unwritten: Save / Discard / Cancel
        if let Some(mut button) = v.confirm_quit {
            match key.code {
                KeyCode::Esc | KeyCode::Char('c') => v.confirm_quit = None,
                KeyCode::Char('s') => {
                    v.confirm_quit = None;
                    hex_save(v);
                    if v.hex_edits.is_empty() {
                        self.close_viewer();
                    }
                }
                KeyCode::Char('d') => self.close_viewer(),
                KeyCode::Enter => match button {
                    0 => {
                        v.confirm_quit = None;
                        hex_save(v);
                        if v.hex_edits.is_empty() {
                            self.close_viewer();
                        }
                    }
                    1 => self.close_viewer(),
                    _ => v.confirm_quit = None,
                },
                KeyCode::Left => {
                    button = button.checked_sub(1).unwrap_or(2);
                    v.confirm_quit = Some(button);
                }
                KeyCode::Right | KeyCode::Tab => v.confirm_quit = Some((button + 1) % 3),
                _ => {}
            }
            return;
        }
        if let Some((value, cursor)) = v.goto.as_mut() {
            match key.code {
                KeyCode::Esc => v.goto = None,
                KeyCode::Enter => {
                    let asked = value.clone();
                    v.goto = None;
                    viewer_goto(v, &asked);
                }
                code => {
                    edit_line(value, cursor, code, key.modifiers);
                }
            }
            return;
        }
        // m / r wait for the digit that names the mark
        if let Some(setting) = v.pending_mark.take() {
            match key.code {
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    let slot = c as usize - '0' as usize;
                    if setting {
                        v.bookmarks[slot] = Some(v.top);
                        v.note = Some(format!(" mark {slot} set here "));
                    } else {
                        match v.bookmarks[slot] {
                            Some(line) => {
                                v.top = line;
                                v.top_seg = 0;
                                v.hex = false;
                            }
                            None => v.note = Some(format!(" mark {slot} is not set ")),
                        }
                    }
                }
                _ => v.note = Some(" a mark is a digit ".into()),
            }
            return;
        }
        if let Some(dialog) = v.prompt.as_mut() {
            match key.code {
                KeyCode::Esc => v.prompt = None,
                KeyCode::Enter => {
                    let asked = dialog.clone();
                    v.prompt = None;
                    if !asked.is_empty() {
                        let from = if asked.backwards {
                            v.top.saturating_sub(1)
                        } else {
                            v.top
                        };
                        v.search = asked;
                        viewer_search(v, from, false);
                    }
                }
                KeyCode::Tab | KeyCode::Down => {
                    dialog.row = (dialog.row + 1) % VIEW_SEARCH_ROWS;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    dialog.row = (dialog.row + VIEW_SEARCH_ROWS - 1) % VIEW_SEARCH_ROWS;
                }
                KeyCode::Char(' ') if dialog.row != VIEW_SEARCH_FIELD => dialog.toggle(),
                KeyCode::Left | KeyCode::Right if dialog.row == VIEW_SEARCH_KIND => dialog.toggle(),
                code if dialog.row == VIEW_SEARCH_FIELD => {
                    let (value, cursor) = (&mut dialog.value, &mut dialog.cursor);
                    edit_line(value, cursor, code, key.modifiers);
                }
                _ => {}
            }
            return;
        }
        let rows = v.rows.max(1);
        // in hex edit mode the keyboard is the file's: a letter is a
        // byte, not the action that letter is bound to. The dialogs
        // above have already had their say, so this is only the keys
        // nothing else wanted.
        if v.hex && v.hex_edit && hex_edit_key(v, key, rows) {
            return;
        }
        let page = rows.saturating_sub(1).max(1) as isize;
        // action keys first (rebindable via [keys.viewer]), then the
        // structural movement keys
        if let Some(action) = bound {
            self.viewer_action(action, rows);
            return;
        }
        match key.code {
            KeyCode::Esc => self.viewer_quit(),
            KeyCode::Up => viewer_scroll(v, -1, rows),
            KeyCode::Down => viewer_scroll(v, 1, rows),
            KeyCode::PageUp => viewer_scroll(v, -page, rows),
            KeyCode::PageDown => viewer_scroll(v, page, rows),
            KeyCode::Home => {
                v.top = 0;
                v.top_seg = 0;
                v.left = 0;
                v.hex_top = 0;
            }
            KeyCode::End => viewer_end(v, rows),
            KeyCode::Left if !v.wrap => v.left = v.left.saturating_sub(8),
            KeyCode::Right if !v.wrap => v.left += 8,
            _ => {}
        }
    }

    /// One rebindable viewer action.
    fn viewer_action(&mut self, action: keymap::ViewerAction, rows: usize) {
        use keymap::ViewerAction as VA;
        // the actions that replace what the viewer is showing need the
        // whole App, so they run before the borrow below
        let hex = self.viewer().is_some_and(|v| v.hex);
        match action {
            VA::Quit => return self.viewer_quit(),
            // mc's button bar spends F2 and F6 twice: in hex mode they
            // are the edit toggle and Save
            VA::ToggleRaw if hex => return self.viewer_hex_save(),
            VA::ToggleWrap if hex => return self.viewer_hex_edit(),
            VA::HexSave => return self.viewer_hex_save(),
            VA::HexEdit => return self.viewer_hex_edit(),
            VA::ToggleRaw => return self.viewer_toggle_raw(),
            VA::NextFile => return self.viewer_step_file(1),
            VA::PrevFile => return self.viewer_step_file(-1),
            _ => {}
        }
        let Some(v) = self.viewer_mut() else {
            return;
        };
        match action {
            VA::Quit | VA::ToggleRaw | VA::NextFile | VA::PrevFile | VA::HexEdit | VA::HexSave => {
                unreachable!("handled above")
            }
            VA::ToggleWrap => {
                v.wrap = !v.wrap;
                v.top_seg = 0;
                v.left = 0;
            }
            VA::ToggleHex => v.hex = !v.hex,
            VA::Search => {
                let mut dialog = v.search.clone();
                dialog.cursor = dialog.value.chars().count();
                dialog.row = VIEW_SEARCH_FIELD;
                v.prompt = Some(dialog);
            }
            VA::SearchNext => {
                if !v.search.is_empty() {
                    // step past the current hit, whichever way we go
                    let from = match (v.found, v.search.backwards) {
                        (Some(0), true) => return,
                        (Some(found), true) => found - 1,
                        (Some(found), false) => found + 1,
                        (None, _) => v.top,
                    };
                    viewer_search(v, from, true);
                }
            }
            VA::Goto => {
                let at = (v.top + 1).to_string();
                let cursor = at.chars().count();
                v.goto = Some((at, cursor));
            }
            VA::SetMark => {
                v.pending_mark = Some(true);
                v.note = Some(" mark: press a digit ".into());
            }
            VA::GoMark => {
                v.pending_mark = Some(false);
                v.note = Some(" go to mark: press a digit ".into());
            }
            VA::ToggleRuler => v.ruler = !v.ruler,
            VA::Charset => {
                let now = v.file.charset.map(rcmd_core::charset::label_of);
                v.charset_pick = Some(charset_row(now));
            }
            VA::ToggleNroff => {
                v.nroff = !v.nroff;
                v.note = Some(if v.nroff {
                    " formatted: overstrikes read as bold and underline ".into()
                } else {
                    " unformatted ".into()
                });
            }

            VA::Follow => {
                v.follow = !v.follow;
                if v.follow {
                    let _ = v.file.refresh();
                    viewer_end(v, rows);
                    v.note = Some(" following - f stops ".into());
                }
            }
        }
    }

    /// The editor on top, if the screen on top is one.
    pub fn editor(&self) -> Option<&EditorState> {
        match self.screens.get(self.current?) {
            Some(Screen::Editor(st)) => Some(st),
            _ => None,
        }
    }

    pub fn editor_mut(&mut self) -> Option<&mut EditorState> {
        match self.screens.get_mut(self.current?) {
            Some(Screen::Editor(st)) => Some(st),
            _ => None,
        }
    }

    /// The viewer on top, if the screen on top is one.
    pub fn viewer(&self) -> Option<&Viewer> {
        match self.screens.get(self.current?) {
            Some(Screen::Viewer(v)) => Some(v),
            _ => None,
        }
    }

    pub fn viewer_mut(&mut self) -> Option<&mut Viewer> {
        match self.screens.get_mut(self.current?) {
            Some(Screen::Viewer(v)) => Some(v),
            _ => None,
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
    /// which is where mc lands after closing one too.
    fn take_current_screen(&mut self) -> Option<Screen> {
        let at = self.current.take()?;
        (at < self.screens.len()).then(|| self.screens.remove(at))
    }

    /// Follow mode: re-index on growth and stick to the bottom.
    fn follow_tick(&mut self) {
        if let Some(v) = self.viewer_mut()
            && v.follow
        {
            let before = v.file.size;
            if v.file.refresh().unwrap_or(false) {
                if let Some(hl) = v.hl.as_mut()
                    && v.file.size < before
                {
                    // rotation/truncation: earlier lines changed
                    hl.invalidate_from(0);
                }
                viewer_end(v, v.rows.max(1));
            }
        }
    }

    fn on_menu_key(&mut self, key: KeyEvent) {
        let Some(ms) = self.menu.as_mut() else { return };
        match key.code {
            KeyCode::Esc | KeyCode::F(9) | KeyCode::F(10) => self.menu = None,
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                let len = MENUS.len();
                ms.menu = if key.code == KeyCode::Left {
                    (ms.menu + len - 1) % len
                } else {
                    (ms.menu + 1) % len
                };
                ms.item = first_menu_item(MENUS[ms.menu].1);
            }
            KeyCode::Up => ms.item = menu_step(MENUS[ms.menu].1, ms.item, -1),
            KeyCode::Down => ms.item = menu_step(MENUS[ms.menu].1, ms.item, 1),
            KeyCode::Enter => {
                if let Some((_, _, action)) = MENUS[ms.menu].1[ms.item] {
                    let menu = ms.menu;
                    self.menu = None;
                    self.run_menu_action(menu, action);
                }
            }
            KeyCode::Char(c) => {
                // MC-style hotkeys: an entry letter of the open menu
                // runs it; otherwise a title letter switches menus.
                let c = c.to_ascii_lowercase();
                let entry = MENUS[ms.menu]
                    .1
                    .iter()
                    .flatten()
                    .find(|(label, ..)| menu_hotkey(label) == Some(c));
                if let Some(&(_, _, action)) = entry {
                    let menu = ms.menu;
                    self.menu = None;
                    self.run_menu_action(menu, action);
                } else if let Some(menu) = MENUS
                    .iter()
                    .position(|(title, _)| menu_hotkey(title) == Some(c))
                {
                    ms.menu = menu;
                    ms.item = first_menu_item(MENUS[menu].1);
                }
            }
            _ => {}
        }
    }

    /// Which panel a menu acts on: mc's Left and Right menus act on
    /// their own panel, everything else on whichever has the focus.
    fn menu_side(menu: usize) -> Option<usize> {
        match menu {
            LEFT_MENU => Some(0),
            RIGHT_MENU => Some(1),
            _ => None,
        }
    }

    fn run_menu_action(&mut self, menu: usize, action: Action) {
        match Self::menu_side(menu) {
            Some(side) => self.run_action_on(side, action),
            None => self.run_action(action),
        }
    }

    /// Run a Left/Right menu entry against that menu's panel. The focus
    /// moves there first, and stays: several of these entries open a
    /// dialog that only lands later (filter, panelize, the SFTP link),
    /// and a dialog that acts on a panel other than the focused one is
    /// how you delete the wrong file. mc leaves the focus alone; this
    /// is the one place rcmd would rather be obvious than identical.
    fn run_action_on(&mut self, side: usize, action: Action) {
        match action {
            // the preview and info panes replace the panel whose menu
            // was used, so the focus goes to the *other* one - the one
            // still doing the browsing
            Action::QuickView => self.quick_view_on(side),
            Action::InfoView => self.info_on(side),
            _ => {
                self.active = side;
                self.run_action(action);
            }
        }
    }

    fn quick_view_on(&mut self, side: usize) {
        if self.quick_view.as_ref().is_some_and(|qv| qv.side == side) {
            self.quick_view = None;
            return;
        }
        self.quick_view = None;
        self.active = side ^ 1;
        self.toggle_quick_view();
    }

    fn info_on(&mut self, side: usize) {
        if self.info == Some(side) {
            self.info = None;
            return;
        }
        self.info = None;
        self.active = side ^ 1;
        self.toggle_info();
    }

    /// Actions that mean "do this to the entry under the cursor" have
    /// nothing to act on while the tree has replaced the listing. The
    /// entries are still loaded underneath, which is exactly the
    /// problem: acting on a file nobody can see is not on. (mc runs
    /// F5-F8 against the selected *directory* instead; that belongs
    /// with the file-operation dialogs in S2.)
    fn blocked_in_tree(action: Action) -> bool {
        matches!(
            action,
            Action::View
                | Action::Edit
                | Action::Copy
                | Action::Move
                | Action::Mkdir
                | Action::Delete
                | Action::DeletePerm
                | Action::Mark
                | Action::SelectGroup
                | Action::UnselectGroup
                | Action::InvertSelection
                | Action::DirSize
                | Action::BulkRename
        )
    }

    fn run_action(&mut self, action: Action) {
        if self.panels[self.active].list_mode == ListMode::Tree && Self::blocked_in_tree(action) {
            self.status = Some(" not while this panel shows the tree ".into());
            return;
        }
        match action {
            Action::Help => self.help = Some(HelpState { top: 0, rows: 1 }),
            Action::Menu => {
                self.menu = Some(MenuState {
                    menu: 0,
                    item: first_menu_item(MENUS[0].1),
                })
            }
            Action::Mark => self.panel().toggle_mark(),
            Action::QuickSearch => self.quick_search = Some(String::new()),
            Action::Hotlist => self.dialog = Some(Dialog::Hotlist(0)),
            Action::Filter => self.open_filter(),
            Action::UpDir => self.fallible(|p| p.go_up()),
            Action::Enter => self.fallible(|p| p.enter()),
            Action::FindFile => self.open_find(),
            Action::Panelize => self.open_panelize(),
            Action::CompareDirs => self.open_compare(),
            Action::CompareFiles => self.open_diff(),
            Action::DirSize => self.dir_size(),
            Action::ScreenList => self.open_screen_list(),
            Action::Charset => {
                let now = self.panels[self.active]
                    .charset
                    .map(rcmd_core::charset::label_of);
                self.dialog = Some(Dialog::Charset(charset_row(now)));
            }
            Action::View => self.open_viewer(false),
            Action::ViewRaw => self.open_viewer(true),
            Action::Edit => self.open_editor(),
            Action::Copy => self.open_transfer(false),
            Action::Move => self.open_transfer(true),
            Action::Mkdir => self.open_mkdir(),
            Action::Delete => self.open_delete(false),
            Action::DeletePerm => self.open_delete(true),
            Action::SelectGroup => self.open_select(true),
            Action::UnselectGroup => self.open_select(false),
            Action::InvertSelection => self.panel().invert_marks(),
            Action::Quit => {
                // an editor left open on another screen still has the
                // changes in it: that is worth asking about even for
                // someone who turned the ordinary quit question off
                let unsaved = self
                    .screens
                    .iter()
                    .filter(|s| matches!(s, Screen::Editor(st) if st.ed.modified()))
                    .count();
                let message = match unsaved {
                    0 => "Quit rcmd?".to_string(),
                    1 => "1 editor has unsaved changes. Quit rcmd?".to_string(),
                    n => format!("{n} editors have unsaved changes. Quit rcmd?"),
                };
                if self.config.confirm_exit || unsaved > 0 {
                    self.dialog = Some(Dialog::Confirm(ConfirmDialog {
                        title: " Quit ".into(),
                        message,
                        yes: unsaved == 0,
                        paths: Vec::new(),
                        permanent: false,
                        kind: ConfirmKind::Quit,
                        command: None,
                    }));
                } else {
                    self.quit_now();
                }
            }
            Action::Shell => self.pending_exec = Some(Exec::Shell),
            Action::SftpLink => {
                self.dialog = Some(Dialog::Input(InputDialog {
                    title: " Remote link (sftp:// fish:// ftp://[user@]host[/path]) ".into(),
                    value: "sftp://".into(),
                    cursor: 7,
                    action: InputAction::SftpConnect,
                }));
            }
            Action::HistoryBack => self.history_step(false),
            Action::HistoryForward => self.history_step(true),
            Action::QuickView => self.toggle_quick_view(),
            Action::InfoView => self.toggle_info(),
            Action::UserMenu => {
                if self.config.commands.is_empty() {
                    self.status = Some(" no [[commands]] in the config - see F1 ".into());
                } else {
                    self.dialog = Some(Dialog::UserMenu(0));
                }
            }
            Action::UserCommand(i) => self.run_user_command(i),
            Action::Listing(mode) => {
                if mode == ListMode::Tree && !self.panels[self.active].is_local() {
                    self.status = Some(" the tree works on local panels only ".into());
                } else {
                    self.panel().list_mode = mode;
                    self.sync_tree(self.active);
                }
            }
            Action::ListingCycle => {
                let panel = self.panel();
                panel.list_mode = match panel.list_mode {
                    ListMode::Brief => ListMode::Full,
                    ListMode::Full => ListMode::Long,
                    // neither the tree nor a user-defined format is
                    // part of the cycle (mc does not cycle into them
                    // either); both are a deliberate visit
                    ListMode::Long | ListMode::Tree | ListMode::User => ListMode::Brief,
                };
                self.sync_tree(self.active);
            }
            Action::DirTree => {
                let panel = &self.panels[self.active];
                if panel.is_local() {
                    let tree = Tree::new(&panel.local_cwd(), panel.show_hidden);
                    self.dialog = Some(Dialog::Tree(Box::new(tree)));
                } else {
                    self.status = Some(" the tree works on local panels only ".into());
                }
            }
            Action::OtherSameDir => self.other_panel_dir(false),
            Action::OtherOpenDir => self.other_panel_dir(true),
            Action::Reload => self.fallible(|p| p.reload().map(|()| true)),
            Action::SwapPanels => {
                self.panels.swap(0, 1);
                self.table_states.swap(0, 1);
                self.git_info.swap(0, 1);
                self.git_seen.swap(0, 1);
                self.disk.swap(0, 1);
                if let Some(qv) = self.quick_view.as_mut() {
                    qv.side ^= 1;
                }
                if let Some(side) = self.info.as_mut() {
                    *side ^= 1;
                }
            }
            Action::ToggleHidden => {
                self.fallible(|p| p.toggle_hidden().map(|()| true));
                // the figure was scanned under the old flag, so rebuild
                // it rather than leave the tree and the listing at odds
                let side = self.active;
                if self.trees[side].is_some() {
                    let path = self.trees[side].as_ref().and_then(Tree::selected_path);
                    self.trees[side] = None;
                    self.sync_tree(side);
                    if let (Some(tree), Some(path)) = (self.trees[side].as_mut(), path) {
                        tree.reveal(&path);
                    }
                }
            }
            Action::Options => {
                let cfg = &self.config;
                let mut values = [false; OPT_COUNT];
                values[Opt::Hidden as usize] = self.panels[self.active].show_hidden;
                values[Opt::Lynx as usize] = cfg.lynx_on();
                values[Opt::Mouse as usize] = cfg.mouse;
                values[Opt::Watch as usize] = cfg.watch;
                values[Opt::Git as usize] = cfg.git;
                values[Opt::ConfirmDelete as usize] = cfg.confirm_delete;
                values[Opt::ConfirmOverwrite as usize] = cfg.confirm_overwrite;
                values[Opt::ConfirmExit as usize] = cfg.confirm_exit;
                values[Opt::ConfirmHotlistDelete as usize] = cfg.confirm_hotlist_delete;
                values[Opt::ConfirmExecute as usize] = cfg.confirm_execute;
                values[Opt::Subshell as usize] = cfg.subshell;
                values[Opt::ExternalEditor as usize] = cfg.editor == "external";
                values[Opt::DarkTheme as usize] = cfg.theme == "dark";
                values[Opt::HorizontalSplit as usize] = cfg.horizontal_split();
                values[Opt::MenuBar as usize] = cfg.show_menubar;
                values[Opt::StatusLine as usize] = cfg.show_status;
                values[Opt::MiniStatus as usize] = cfg.show_mini_status;
                values[Opt::CommandLine as usize] = cfg.show_cmdline;
                values[Opt::KeyBar as usize] = cfg.show_keybar;
                let ratio = cfg.ratio();
                self.dialog = Some(Dialog::Options(OptionsDialog {
                    // start on the first setting, not the heading
                    cursor: 1,
                    values,
                    ratio,
                    ok: true,
                }));
            }
            Action::Sort(key) => self.panel().set_sort(key),
            Action::SortReverse => {
                let panel = self.panel();
                panel.sort_reverse = !panel.sort_reverse;
                panel.resort();
            }
            Action::EditNew => self.open_edit_new(),
            Action::CopyHere => self.open_transfer_here(false),
            Action::MoveHere => self.open_transfer_here(true),
            Action::PasteTags => self.insert_tagged_names(),
            Action::PastePath => {
                let text = format!("{} ", shell_quote(&self.panels[self.active].display_path()));
                self.insert_cmdline(&text);
            }
            Action::QuickCd => {
                self.dialog = Some(Dialog::Input(InputDialog {
                    title: " Quick cd ".into(),
                    value: String::new(),
                    cursor: 0,
                    action: InputAction::QuickCd,
                }));
            }
            Action::Repaint => self.repaint = true,
            Action::BulkRename => self.open_bulk_rename(),
            Action::VfsList => {
                let rows = self.vfs_rows();
                if rows.is_empty() {
                    self.status = Some(" no archives or connections open ".into());
                } else {
                    self.dialog = Some(Dialog::Vfs(VfsDialog { rows, selected: 0 }));
                }
            }
            Action::Jobs => {
                if self.jobs.is_empty() {
                    self.status = Some(" no jobs running ".into());
                } else {
                    self.dialog = Some(Dialog::Jobs(0));
                }
            }
            Action::HistoryList => {
                if self.cmdline.history().is_empty() {
                    self.status = Some(" command history is empty ".into());
                } else {
                    // newest first, so the row under the cursor is the
                    // command you most likely want back
                    self.dialog = Some(Dialog::History(0));
                }
            }
        }
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

    /// Ctrl+X Q: turn the other panel into a live file preview (again
    /// turns it back into a listing).
    fn toggle_quick_view(&mut self) {
        if self.quick_view.take().is_some() {
            return;
        }
        self.info = None;
        self.quick_view = Some(QuickView {
            side: self.active ^ 1,
            view: None,
            note: String::new(),
            top: 0,
            hex: false,
            rows: 1,
        });
        self.update_quick_view();
    }

    /// Keep the preview in sync with the cursor of the browsing panel;
    /// called every loop iteration, reopens only when the file changes.
    fn update_quick_view(&mut self) {
        if self.quick_view.is_none() {
            return;
        }
        // the preview follows the other panel's cursor, and what it
        // shows can change without a key of its own
        self.dirty = true;
        let Some(qv) = self.quick_view.as_mut() else {
            return;
        };
        let browse = &self.panels[qv.side ^ 1];
        let entry = browse.selected();
        let name = match entry {
            Some(e) if !e.is_parent() && !e.is_dir() => e.name.clone(),
            _ => {
                qv.view = None;
                qv.note = String::new();
                return;
            }
        };
        if !browse.is_local() {
            qv.view = None;
            qv.note = "no preview here - F3 views remote/archive files".into();
            return;
        }
        let path = browse.cwd.join(name);
        if qv.view.as_ref().is_some_and(|(p, _)| p == &path) {
            return;
        }
        match FileView::open(&path) {
            Ok(fv) => {
                qv.view = Some((path, fv));
                qv.top = 0;
                qv.note.clear();
            }
            Err(err) => {
                qv.view = None;
                qv.note = err.to_string();
            }
        }
    }

    /// Ctrl+X i: turn the other panel into a stat/info pane (again
    /// restores the listing).
    fn toggle_info(&mut self) {
        if self.info.take().is_some() {
            return;
        }
        self.quick_view = None;
        self.info = Some(self.active ^ 1);
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

    /// Enter on the cursor entry: directories and archives first; a
    /// plain file consults the [[open]] rules (local panels only, the
    /// first matching glob wins, case-insensitive). The `enter` keymap
    /// action (lynx-motion Right) stays dirs-only on purpose.
    fn enter_or_open(&mut self) {
        match self.panels[self.active].enter() {
            Ok(true) => return,
            Ok(false) => {}
            Err(err) => {
                self.status = Some(format!(" {err} "));
                return;
            }
        }
        let panel = &self.panels[self.active];
        if !panel.is_local() {
            return;
        }
        let Some(entry) = panel.selected() else {
            return;
        };
        if entry.is_parent() || entry.is_dir() {
            return;
        }
        let name = entry.name.to_string_lossy().to_lowercase();
        let run = match self
            .config
            .open
            .iter()
            .find(|rule| glob_match(&rule.pattern.to_lowercase(), &name))
        {
            Some(rule) => rule.run.clone(),
            None => return,
        };
        let cmd = self.expand_macros(&run);
        if self.config.confirm_execute {
            self.dialog = Some(Dialog::Confirm(ConfirmDialog {
                title: " Execute ".into(),
                message: format!("Run: {}", crate::ui::tail(&cmd, 60)),
                yes: true,
                paths: Vec::new(),
                permanent: false,
                kind: ConfirmKind::Execute,
                command: Some(cmd),
            }));
            return;
        }
        self.pending_exec = Some(Exec::Quiet(cmd));
    }

    /// `%f` cursor file, `%d` this directory, `%D` the other panel's,
    /// `%t` marked files, `%%` a literal percent - all shell-quoted.
    fn expand_macros(&self, template: &str) -> String {
        let panel = &self.panels[self.active];
        let file = panel
            .selected()
            .filter(|e| !e.is_parent())
            .map(|e| shell_quote(&e.name.to_string_lossy()))
            .unwrap_or_default();
        let tagged = panel
            .entries
            .iter()
            .filter(|e| panel.is_marked(e))
            .map(|e| shell_quote(&e.name.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
        let mut out = String::new();
        let mut chars = template.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('f') => out.push_str(&file),
                Some('d') => out.push_str(&shell_quote(&panel.local_cwd().to_string_lossy())),
                Some('D') => out.push_str(&shell_quote(
                    &self.panels[self.active ^ 1].local_cwd().to_string_lossy(),
                )),
                Some('t') => out.push_str(&tagged),
                Some('%') => out.push('%'),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        }
        out
    }

    fn run_user_command(&mut self, i: usize) {
        let Some(cmd) = self.config.commands.get(i) else {
            return;
        };
        let run = cmd.run.clone();
        let expanded = self.expand_macros(&run);
        self.pending_exec = Some(Exec::Command(expanded));
    }

    /// Alt+i / Alt+o: point the other panel at the active panel's
    /// directory, or at the directory under its cursor.
    fn other_panel_dir(&mut self, under_cursor: bool) {
        let active = &self.panels[self.active];
        if !active.is_local() {
            self.status = Some(" works on local panels only ".into());
            return;
        }
        let target = if under_cursor {
            match active.selected() {
                Some(e) if e.is_dir() && !e.is_parent() => active.cwd.join(&e.name),
                _ => active.cwd.clone(),
            }
        } else {
            active.cwd.clone()
        };
        let other = &mut self.panels[self.active ^ 1];
        let result = if other.is_local() {
            other.cd(target)
        } else {
            other.to_local(target)
        };
        if let Err(err) = result {
            self.status = Some(format!(" {err} "));
        }
    }

    /// Alt+←/→: walk the active panel's directory history.
    fn history_step(&mut self, forward: bool) {
        let panel = &mut self.panels[self.active];
        let target = if forward {
            panel.hist_forward()
        } else {
            panel.hist_back()
        };
        match target {
            Some(loc) => self.navigate(&loc),
            None => {
                self.status = Some(if forward {
                    " history: already at the newest entry ".into()
                } else {
                    " history: already at the oldest entry ".into()
                });
            }
        }
    }

    /// Send the active panel to a history location: a local path or a
    /// full sftp:// or ftp:// URL (routed through the connection cache).
    fn navigate(&mut self, target: &str) {
        if is_remote_url(target) {
            self.connect_remote(target);
            return;
        }
        let path = PathBuf::from(target);
        let panel = &mut self.panels[self.active];
        let result = if panel.is_local() {
            panel.cd(path)
        } else {
            panel.to_local(path)
        };
        if let Err(err) = result {
            self.status = Some(format!(" {err} "));
        }
    }

    fn open_viewer(&mut self, raw: bool) {
        self.open_viewer_keeping(raw, ViewKeep::default());
    }

    /// Open the cursor file in the internal viewer, carrying `keep`
    /// over from the viewer this one replaces (F6, C-f / C-b).
    fn open_viewer_keeping(&mut self, raw: bool, keep: ViewKeep) {
        let panel = &self.panels[self.active];
        let Some(entry) = panel.selected() else {
            return;
        };
        if entry.is_dir() {
            self.status = Some(" cannot view a directory ".into());
            return;
        }
        let name = entry.name.clone();
        let Some((source, source_title, mut temps)) = self.fetch_view_source(&name) else {
            return;
        };
        // anything but a local panel handed back a copy, and a copy is
        // not something to write bytes into
        let scratch = !temps.is_empty();
        // the [[view]] filter is a local-panel thing: its command runs
        // on a path, and an archive member has none until it is fetched
        let lower = name.to_string_lossy().to_lowercase();
        let filter = self.panels[self.active].is_local().then(|| {
            self.config
                .view
                .iter()
                .find(|rule| glob_match(&rule.pattern.to_lowercase(), &lower))
                .cloned()
        });
        let filter = filter.flatten();
        // F3 runs the filter, Shift+F3 does not; a filter that cannot
        // run says so and the raw file is shown instead
        let mut filtered = None;
        if !raw && let Some(rule) = filter.as_ref() {
            match self.run_view_filter(rule) {
                Ok(pair) => filtered = Some(pair),
                Err(err) => self.status = Some(format!(" view filter: {err} - showing raw ")),
            }
        }
        let (open_path, title_path, is_filtered) = match filtered {
            Some((temp, title)) => {
                temps.push(temp.clone());
                (temp, title, true)
            }
            None => (source.clone(), source_title.clone(), false),
        };
        match FileView::open(&open_path) {
            Ok(file) => {
                self.open_screen(Screen::Viewer(Box::new(Viewer {
                    // filter output carries the command's syntax, not
                    // the file's, so it is shown plain
                    hl: (!is_filtered)
                        .then(|| rcmd_edit::Highlighter::new(&source, file.size as usize))
                        .flatten(),
                    file,
                    path: title_path,
                    hex: keep.hex,
                    wrap: keep.wrap,
                    follow: false,
                    top: 0,
                    top_seg: 0,
                    left: 0,
                    cols: 1,
                    hex_top: 0,
                    hex_edit: false,
                    hex_cursor: 0,
                    hex_low: false,
                    hex_ascii: false,
                    hex_edits: BTreeMap::new(),
                    confirm_quit: None,
                    scratch,
                    rows: 1,
                    search: keep.search,
                    goto: None,
                    bookmarks: [None; 10],
                    pending_mark: None,
                    ruler: keep.ruler,
                    charset_pick: None,
                    nroff: keep.nroff,
                    found: None,
                    prompt: None,
                    source,
                    source_title,
                    filter,
                    filtered: is_filtered,
                    opened_raw: raw,
                    note: None,
                    temps,
                })))
            }
            Err(err) => {
                for temp in temps {
                    let _ = std::fs::remove_file(temp);
                }
                self.status = Some(format!(" view: {err} "));
            }
        }
    }

    /// The cursor file as something on disk: itself on a local panel, a
    /// scratch copy anywhere else. Returns the path, the title to show
    /// for it, and any scratch file the viewer must clean up.
    fn fetch_view_source(
        &mut self,
        name: &std::ffi::OsStr,
    ) -> Option<(PathBuf, PathBuf, Vec<PathBuf>)> {
        let panel = &self.panels[self.active];
        if panel.is_local() {
            let path = panel.cwd.join(name);
            return Some((path.clone(), path, Vec::new()));
        }
        let vpath = panel.cwd.join(name);
        let temp = std::env::temp_dir().join(format!(
            "rcmd-view-{}-{}",
            std::process::id(),
            name.to_string_lossy()
        ));
        let fetched = panel.fs.open_read(&vpath).and_then(|mut reader| {
            let mut out = std::fs::File::create(&temp)?;
            std::io::copy(&mut reader, &mut out)?;
            Ok(())
        });
        if let Err(err) = fetched {
            let _ = std::fs::remove_file(&temp);
            self.status = Some(format!(" view: {err} "));
            return None;
        }
        let title = if let Some(prefix) = &panel.remote {
            PathBuf::from(format!("{prefix}{}", vpath.display()))
        } else {
            let archive = panel.archive.clone().unwrap_or_default();
            PathBuf::from(format!("{}://{}", archive.display(), vpath.display()))
        };
        Some((temp.clone(), title, vec![temp]))
    }

    /// Run a `[[view]]` filter into a scratch file. Ok = (that file,
    /// the command to title the view with); Err = why it is unusable,
    /// for the caller to put wherever it has room.
    fn run_view_filter(
        &mut self,
        rule: &crate::config::OpenRule,
    ) -> Result<(PathBuf, PathBuf), String> {
        let cwd = self.panels[self.active].local_cwd();
        let cmd = self.expand_macros(&rule.run);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let output = std::process::Command::new(&shell)
            .arg("-c")
            .arg(&cmd)
            .current_dir(&cwd)
            .output()
            .map_err(|err| err.to_string())?;
        if output.stdout.is_empty() && !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(err.lines().next().unwrap_or("failed").trim().to_string());
        }
        let temp = std::env::temp_dir().join(format!("rcmd-view-{}-filtered", std::process::id()));
        std::fs::write(&temp, &output.stdout).map_err(|err| err.to_string())?;
        Ok((temp, PathBuf::from(cmd)))
    }

    /// F6: swap the `[[view]]` filter in and out under the same file.
    fn viewer_toggle_raw(&mut self) {
        let Some(v) = self.viewer() else {
            return;
        };
        let (source, filter, filtered) = (v.source.clone(), v.filter.clone(), v.filtered);
        let swapped = if filtered {
            let size = std::fs::metadata(&source).map(|m| m.len()).unwrap_or(0);
            FileView::open(&source)
                .map(|file| {
                    let hl = rcmd_edit::Highlighter::new(&source, size as usize);
                    (file, source.clone(), hl, false, None)
                })
                .map_err(|err| err.to_string())
        } else {
            match filter.as_ref() {
                Some(rule) => self.run_view_filter(rule).and_then(|(temp, title)| {
                    FileView::open(&temp)
                        .map(|file| (file, title, None, true, Some(temp)))
                        .map_err(|err| err.to_string())
                }),
                None => Err("no [[view]] filter for this file".to_string()),
            }
        };
        let Some(v) = self.viewer_mut() else {
            return;
        };
        match swapped {
            Ok((file, title, hl, now_filtered, temp)) => {
                let title = if now_filtered {
                    title
                } else {
                    v.source_title.clone()
                };
                v.file = file;
                v.hl = hl;
                v.path = title;
                v.filtered = now_filtered;
                v.opened_raw = !now_filtered;
                if let Some(temp) = temp
                    && !v.temps.contains(&temp)
                {
                    v.temps.push(temp);
                }
                // the text underneath changed: line numbers, the hit and
                // the marks all pointed into the other one
                v.top = 0;
                v.top_seg = 0;
                v.left = 0;
                v.hex_top = 0;
                v.found = None;
                v.bookmarks = [None; 10];
                v.note = Some(if now_filtered {
                    " parsed ".into()
                } else {
                    " raw ".into()
                });
            }
            Err(err) => v.note = Some(format!(" {err} ")),
        }
    }

    /// C-f / C-b: the next or previous file of the panel, in the same
    /// viewer with the same wrap, hex, ruler, nroff and search.
    fn viewer_step_file(&mut self, delta: isize) {
        let Some(v) = self.viewer() else {
            return;
        };
        let keep = ViewKeep {
            wrap: v.wrap,
            hex: v.hex,
            ruler: v.ruler,
            nroff: v.nroff,
            search: v.search.clone(),
        };
        let raw = v.opened_raw;
        if !v.hex_edits.is_empty() {
            if let Some(v) = self.viewer_mut() {
                v.note = Some(" bytes are still unwritten - F6 writes them ".into());
            }
            return;
        }
        let panel = &self.panels[self.active];
        let mut idx = panel.cursor as isize;
        let target = loop {
            idx += delta;
            if idx < 0 || idx as usize >= panel.entries.len() {
                break None;
            }
            // directories are not files to read, and neither is ".."
            if !panel.entries[idx as usize].is_dir() {
                break Some(idx as usize);
            }
        };
        let Some(target) = target else {
            if let Some(v) = self.viewer_mut() {
                v.note = Some(match delta {
                    d if d > 0 => " no next file in the panel ".into(),
                    _ => " no previous file in the panel ".into(),
                });
            }
            return;
        };
        self.close_viewer();
        self.panels[self.active].cursor = target;
        self.open_viewer_keeping(raw, keep);
    }

    /// F2 in hex mode: the cursor that lets bytes be typed over. Only
    /// where the viewer is on the file itself - editing a scratch copy
    /// would write to something about to be deleted.
    fn viewer_hex_edit(&mut self) {
        let Some(v) = self.viewer_mut() else {
            return;
        };
        if v.hex_edit {
            v.hex_edit = false;
            v.note = Some(" viewing ".into());
            return;
        }
        if let Some(why) = v.editable() {
            v.note = Some(why.into());
            return;
        }
        v.hex_edit = true;
        v.hex_low = false;
        // start where the screen is rather than where the file is
        v.hex_cursor = (v.hex_top * 16).min(v.file.size.saturating_sub(1));
        v.note = Some(" editing: hex digits or Tab for the text column, F6 writes ".into());
    }

    /// F6 in hex mode: write the changed bytes into the file.
    fn viewer_hex_save(&mut self) {
        if let Some(v) = self.viewer_mut() {
            hex_save(v);
        }
    }

    /// Quit, unless there are bytes the file has not been told about.
    fn viewer_quit(&mut self) {
        match self.viewer_mut() {
            Some(v) if !v.hex_edits.is_empty() => v.confirm_quit = Some(0),
            _ => self.close_viewer(),
        }
    }

    /// Close the viewer, taking its scratch files with it.
    fn close_viewer(&mut self) {
        if let Some(Screen::Viewer(viewer)) = self.take_current_screen() {
            for temp in viewer.temps {
                let _ = std::fs::remove_file(temp);
            }
        }
    }

    fn open_editor(&mut self) {
        let panel = &self.panels[self.active];
        let Some(entry) = panel.selected() else {
            return;
        };
        if entry.is_dir() {
            self.status = Some(" cannot edit a directory ".into());
            return;
        }
        let external = self.config.editor == "external";
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());
        if panel.is_remote() {
            // edit a scratch copy; upload it back if the editor saved
            let name = entry.name.clone();
            let remote_path = panel.cwd.join(&name);
            let title = format!(
                "{}{}",
                panel.remote.clone().unwrap_or_default(),
                remote_path.display()
            );
            let temp = std::env::temp_dir().join(format!(
                "rcmd-edit-{}-{}",
                std::process::id(),
                name.to_string_lossy()
            ));
            let fetched = panel.fs.open_read(&remote_path).and_then(|mut reader| {
                let mut out = std::fs::File::create(&temp)?;
                std::io::copy(&mut reader, &mut out)?;
                Ok(())
            });
            if let Err(err) = fetched {
                let _ = std::fs::remove_file(&temp);
                self.status = Some(format!(" edit: {err} "));
                return;
            }
            let hook = RemoteEdit {
                fs: panel.fs.clone(),
                remote_path,
                mtime_before: std::fs::metadata(&temp).and_then(|m| m.modified()).ok(),
                temp: temp.clone(),
            };
            if external {
                // an editor outside rcmd has no screen to hang the
                // upload on, so the App holds it until the child exits
                self.remote_edit = Some(hook);
                self.pending_exec = Some(Exec::Quiet(format!(
                    "{editor} {}",
                    shell_quote(&temp.to_string_lossy())
                )));
            } else if !self.open_internal_editor_with(
                &temp,
                title,
                Some(EditFollowUp::Remote(hook)),
            ) {
                let _ = std::fs::remove_file(&temp);
            }
            return;
        }
        if !panel.is_local() {
            self.status = Some(" cannot edit inside an archive ".into());
            return;
        }
        let path = panel.cwd.join(&entry.name);
        if external {
            self.pending_exec = Some(Exec::Quiet(format!(
                "{editor} {}",
                shell_quote(&path.to_string_lossy())
            )));
        } else {
            let title = path.display().to_string();
            self.open_internal_editor(&path, title);
        }
    }

    fn open_internal_editor(&mut self, path: &Path, title: String) -> bool {
        self.open_internal_editor_with(path, title, None)
    }

    /// ...and with whatever closing it has to trigger: an upload back
    /// to a server, or a bulk rename to diff.
    fn open_internal_editor_with(
        &mut self,
        path: &Path,
        title: String,
        follow_up: Option<EditFollowUp>,
    ) -> bool {
        match rcmd_edit::Editor::open(path) {
            Ok(mut ed) => {
                let len = std::fs::metadata(path)
                    .map(|m| m.len() as usize)
                    .unwrap_or(0);
                ed.prefs = self.config.edit_prefs();
                self.open_screen(Screen::Editor(Box::new(EditorState {
                    hl: rcmd_edit::Highlighter::new(path, len),
                    ed,
                    title,
                    top: 0,
                    top_seg: 0,
                    left: 0,
                    wrap: false,
                    rows: 1,
                    cols: 1,
                    prompt: None,
                    note: None,
                    wrap_column: self.config.edit_wrap_column as usize,
                    menu: None,
                    follow_up,
                    bookmarks: Vec::new(),
                    line_numbers: self.config.edit_line_numbers,
                    gutter: 0,
                })));
                true
            }
            Err(err) => {
                self.status = Some(format!(" edit: {err} "));
                false
            }
        }
    }

    fn close_editor(&mut self) {
        let follow_up = match self.take_current_screen() {
            Some(Screen::Editor(st)) => st.follow_up,
            _ => None,
        };
        match follow_up {
            Some(EditFollowUp::Remote(edit)) => self.upload_remote_edit(edit),
            Some(EditFollowUp::Bulk(bulk)) => self.finish_bulk_rename(bulk),
            None => {}
        }
        for panel in &mut self.panels {
            let _ = panel.reload();
        }
        self.git_refresh();
    }

    /// Bulk rename: marked names (or the cursor entry) become a
    /// numbered buffer in the built-in editor; closing it turns the
    /// diff into a previewed batch of renames and deletes.
    fn open_bulk_rename(&mut self) {
        if !self.require_local() {
            return;
        }
        let panel = &self.panels[self.active];
        let names = panel.target_names();
        if names.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let dir = panel.cwd.clone();
        let buffer = rcmd_core::rename::buffer_for(&names);
        let temp = std::env::temp_dir().join(format!("rcmd-rename-{}", std::process::id()));
        if let Err(err) = std::fs::write(&temp, &buffer) {
            self.status = Some(format!(" bulk rename: {err} "));
            return;
        }
        // always the built-in editor - the diff must be processed when
        // the session ends inside rcmd, $EDITOR can't signal that
        let title = format!(
            "bulk rename: {} name(s) - edit, save, close (keep the numbers)",
            names.len()
        );
        let bulk = BulkRename {
            dir,
            names,
            temp: temp.clone(),
        };
        if !self.open_internal_editor_with(&temp, title, Some(EditFollowUp::Bulk(bulk))) {
            let _ = std::fs::remove_file(&temp);
        }
    }

    /// After the bulk-rename editor closes: diff the saved buffer and
    /// hand the outcome to the preview dialog. An unsaved session left
    /// the temp file untouched, which diffs to "no changes".
    fn finish_bulk_rename(&mut self, bulk: BulkRename) {
        let text = std::fs::read_to_string(&bulk.temp).unwrap_or_default();
        let _ = std::fs::remove_file(&bulk.temp);
        match rcmd_core::rename::parse(&text, &bulk.names) {
            Err(err) => self.status = Some(format!(" bulk rename: {err} - nothing done ")),
            Ok(plan) if plan.is_empty() => self.status = Some(" bulk rename: no changes ".into()),
            Ok(plan) => {
                self.dialog = Some(Dialog::RenamePreview(RenamePreview {
                    dir: bulk.dir,
                    renames: plan.renames,
                    deletes: plan.deletes,
                    yes: false,
                }));
            }
        }
    }

    /// Yes on the preview: two-phase renames now, deletes (to trash)
    /// through the ordinary job engine.
    fn apply_bulk_rename(&mut self, preview: RenamePreview) {
        if !preview.renames.is_empty() {
            match rcmd_core::rename::apply(&preview.dir, &preview.renames) {
                Ok(()) => {
                    self.status = Some(format!(" renamed {} item(s) ", preview.renames.len()));
                }
                Err(err) => self.status = Some(format!(" bulk rename: {err} ")),
            }
        }
        for panel in &mut self.panels {
            let _ = panel.reload();
        }
        self.git_refresh();
        if !preview.deletes.is_empty() {
            let paths = preview
                .deletes
                .iter()
                .map(|name| preview.dir.join(name))
                .collect();
            self.start_delete(paths, false);
        }
    }

    /// Save (used by F2 and the quit confirm); returns success.
    fn editor_save(&mut self) -> bool {
        let Some(st) = self.editor_mut() else {
            return false;
        };
        match st.ed.save() {
            Ok(()) => {
                st.note = Some(" saved ".into());
                true
            }
            Err(err) => {
                st.note = Some(format!(" save failed: {err} "));
                false
            }
        }
    }

    fn editor_quit(&mut self) {
        let Some(st) = self.editor_mut() else {
            return;
        };
        if st.ed.modified() {
            st.prompt = Some(EditPrompt::ConfirmQuit { button: 0 });
        } else {
            self.close_editor();
        }
    }

    /// Search from just after `from`; select the match so it is visible.
    fn editor_find(&mut self, pattern: &str, from: rcmd_edit::Pos) {
        let Some(st) = self.editor_mut() else {
            return;
        };
        let re = match rcmd_edit::Editor::compile(pattern) {
            Ok(re) => re,
            Err(err) => {
                let first = err.to_string();
                st.note = Some(format!(
                    " {} ",
                    first.lines().last().unwrap_or("bad pattern")
                ));
                return;
            }
        };
        st.ed.search = pattern.to_string();
        match st.ed.find_from(from, &re) {
            Some(m) => select_match(&mut st.ed, m),
            None => st.note = Some(" not found ".into()),
        }
        self.ensure_editor_visible();
    }

    /// A key in the editor, with the bookmarks kept pointing at the
    /// lines they were put on: text inserted or removed above one moves
    /// it, and nothing else does.
    fn on_editor_key(&mut self, key: KeyEvent) {
        let before = self
            .editor()
            .map(|st| (st.ed.line_count(), st.ed.cursor.line));
        self.on_editor_key_inner(key);
        if let (Some((lines, at)), Some(st)) = (before, self.editor_mut())
            && st.ed.line_count() != lines
        {
            let now = st.ed.line_count();
            let delta = now as isize - lines as isize;
            let edit_at = at.min(st.ed.cursor.line);
            for mark in st.bookmarks.iter_mut() {
                if *mark > edit_at {
                    *mark = mark.saturating_add_signed(delta).min(now.saturating_sub(1));
                }
            }
            st.bookmarks.dedup();
        }
    }

    fn on_editor_key_inner(&mut self, key: KeyEvent) {
        // looked up before the borrow below, which is the whole App
        let bound = self.editor_keys.get(&(key.code, key.modifiers)).copied();
        if self.editor().is_some_and(|st| st.menu.is_some()) {
            self.editor_menu_key(key);
            return;
        }
        if self.editor().is_some_and(|st| st.prompt.is_some()) {
            self.on_editor_prompt_key(key);
            self.ensure_editor_visible();
            return;
        }
        let Some(st) = self.editor_mut() else {
            return;
        };
        st.note = None;
        let mods = key.modifiers;
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);
        let select = mods.contains(KeyModifiers::SHIFT);
        let page = st.rows.saturating_sub(1).max(1) as isize;
        // for highlight invalidation: lowest line this key might touch
        let lo = st
            .ed
            .sel_line_range()
            .map(|(a, _)| a)
            .unwrap_or(usize::MAX)
            .min(st.ed.cursor.line);
        let mut edited = true; // most arms below edit; movement resets it
        // action keys first (rebindable via [keys.editor]); Shift is
        // part of the lookup so Shift+F7 can differ from F7
        if let Some(action) = bound {
            self.editor_action(action);
            return;
        }
        match key.code {
            KeyCode::Left if ctrl => {
                st.ed.move_word(false, select);
                edited = false;
            }
            KeyCode::Right if ctrl => {
                st.ed.move_word(true, select);
                edited = false;
            }
            KeyCode::Left => {
                st.ed.move_left(select);
                edited = false;
            }
            KeyCode::Right => {
                st.ed.move_right(select);
                edited = false;
            }
            KeyCode::Up => {
                st.ed.move_vert(-1, select);
                edited = false;
            }
            KeyCode::Down => {
                st.ed.move_vert(1, select);
                edited = false;
            }
            KeyCode::PageUp => {
                st.ed.move_vert(-page, select);
                edited = false;
            }
            KeyCode::PageDown => {
                st.ed.move_vert(page, select);
                edited = false;
            }
            KeyCode::Home if ctrl => {
                st.ed.move_top(select);
                edited = false;
            }
            KeyCode::End if ctrl => {
                st.ed.move_bottom(select);
                edited = false;
            }
            KeyCode::Home => {
                st.ed.move_home(select);
                edited = false;
            }
            KeyCode::End => {
                st.ed.move_end(select);
                edited = false;
            }
            KeyCode::Enter => st.ed.newline(),
            KeyCode::Tab => st.ed.insert_tab(),
            KeyCode::Backspace => st.ed.backspace(),
            KeyCode::Delete => st.ed.delete_forward(),
            KeyCode::Esc => {
                edited = false;
                if st.ed.has_selection() {
                    st.ed.clear_selection();
                } else {
                    self.editor_quit();
                    return;
                }
            }
            KeyCode::Char(c) if !alt => st.ed.insert(&c.to_string()),
            _ => edited = false,
        }
        if edited && let Some(hl) = st.hl.as_mut() {
            hl.invalidate_from(lo.min(st.ed.cursor.line));
        }
        self.ensure_editor_visible();
    }

    /// One rebindable editor action.
    fn editor_action(&mut self, action: keymap::EditorAction) {
        use keymap::EditorAction as EA;
        match action {
            EA::Save => {
                self.editor_save();
                return;
            }
            EA::Quit => {
                self.editor_quit();
                return;
            }
            EA::Goto => {
                if let Some(st) = self.editor_mut() {
                    let at = (st.ed.cursor.line + 1).to_string();
                    st.prompt = Some(EditPrompt::Goto {
                        cursor: at.chars().count(),
                        value: at,
                    });
                }
                return;
            }
            EA::Menu => {
                if let Some(st) = self.editor_mut() {
                    st.menu = Some(MenuState {
                        menu: 0,
                        item: first_edit_item(EDIT_MENUS[0].1),
                    });
                }
                return;
            }
            EA::SearchNext => {
                let Some(st) = self.editor() else {
                    return;
                };
                let pattern = st.ed.search.clone();
                let from = next_pos(&st.ed);
                if pattern.is_empty() {
                    if let Some(st) = self.editor_mut() {
                        st.prompt = Some(EditPrompt::Search {
                            value: String::new(),
                            cursor: 0,
                        });
                    }
                } else {
                    self.editor_find(&pattern, from);
                }
                return;
            }
            _ => {}
        }
        let share_clipboard = self.config.edit_clipboard;
        let Some(st) = self.editor_mut() else {
            return;
        };
        // lowest line this action might touch, for highlight invalidation
        let lo = st
            .ed
            .sel_line_range()
            .map(|(a, _)| a)
            .unwrap_or(usize::MAX)
            .min(st.ed.cursor.line);
        let mut edited = true;
        match action {
            EA::Save | EA::Quit | EA::SearchNext | EA::Menu | EA::Goto => {
                unreachable!("handled above")
            }
            EA::Mark => {
                st.ed.toggle_mark();
                edited = false;
            }
            EA::Replace => {
                let value = st.ed.search.clone();
                st.prompt = Some(EditPrompt::ReplaceFind {
                    cursor: value.chars().count(),
                    value,
                });
                return;
            }
            EA::Search => {
                let value = st.ed.search.clone();
                st.prompt = Some(EditPrompt::Search {
                    cursor: value.chars().count(),
                    value,
                });
                return;
            }
            EA::BlockCopy | EA::BlockMove => {
                // the block ops fill the same clipboard copy and cut
                // do, so they reach the desktop's the same way - or
                // paste would prefer whatever the desktop last held
                if action == EA::BlockCopy {
                    st.ed.block_copy();
                } else {
                    st.ed.block_move();
                }
                if share_clipboard {
                    clipboard_set(st.ed.clipboard());
                }
            }
            EA::DeleteLine => st.ed.delete_selection_or_line(),
            EA::Undo => {
                if !st.ed.undo() {
                    st.note = Some(" nothing to undo ".into());
                }
            }
            EA::Redo => {
                if !st.ed.redo() {
                    st.note = Some(" nothing to redo ".into());
                }
            }
            EA::Copy => {
                st.ed.copy();
                if share_clipboard {
                    clipboard_set(st.ed.clipboard());
                }
                edited = false;
            }
            EA::Cut => {
                st.ed.cut();
                if share_clipboard {
                    clipboard_set(st.ed.clipboard());
                }
            }
            EA::Paste => {
                // what the desktop holds wins, so a copy from anywhere
                // else pastes here; with no tool installed, or nothing
                // in it, the editor's own clipboard stands
                if share_clipboard
                    && let Some(text) = clipboard_get()
                    && !text.is_empty()
                {
                    st.ed.set_clipboard(text);
                }
                st.ed.paste();
            }
            EA::SelectAll => {
                st.ed.select_all();
                edited = false;
            }
            EA::ToggleWrap => {
                st.wrap = !st.wrap;
                st.top_seg = 0;
                st.left = 0;
                edited = false;
            }
            EA::Charset => {
                st.prompt = Some(EditPrompt::Charset(charset_row(
                    st.ed.charset.map(rcmd_core::charset::label_of),
                )));
                return;
            }
            EA::ToggleLineNumbers => {
                st.line_numbers = !st.line_numbers;
                edited = false;
            }
            EA::BookmarkToggle => {
                let line = st.ed.cursor.line;
                match st.bookmarks.binary_search(&line) {
                    Ok(at) => {
                        st.bookmarks.remove(at);
                        st.note = Some(format!(" bookmark off line {} ", line + 1));
                    }
                    Err(at) => {
                        st.bookmarks.insert(at, line);
                        st.note = Some(format!(" bookmark on line {} ", line + 1));
                    }
                }
                edited = false;
            }
            EA::BookmarkNext | EA::BookmarkPrev => {
                let line = st.ed.cursor.line;
                let target = if action == EA::BookmarkNext {
                    st.bookmarks.iter().find(|&&b| b > line).copied()
                } else {
                    st.bookmarks.iter().rev().find(|&&b| b < line).copied()
                };
                match target {
                    Some(line) => st.ed.goto(rcmd_edit::Pos { line, col: 0 }, false),
                    None if st.bookmarks.is_empty() => {
                        st.note = Some(" no bookmarks - M-k sets one ".into());
                    }
                    // one is the whole list, or the ends of it: say so
                    // rather than wrapping around silently
                    None => st.note = Some(" no bookmark that way ".into()),
                }
                edited = false;
            }
            EA::BookmarkClear => {
                let had = st.bookmarks.len();
                st.bookmarks.clear();
                st.note = Some(format!(" {had} bookmark(s) cleared "));
                edited = false;
            }
        }
        if edited && let Some(hl) = st.hl.as_mut() {
            hl.invalidate_from(lo.min(st.ed.cursor.line));
        }
        self.ensure_editor_visible();
    }

    fn on_editor_prompt_key(&mut self, key: KeyEvent) {
        let Some(st) = self.editor_mut() else {
            return;
        };
        let Some(prompt) = st.prompt.take() else {
            return;
        };
        match prompt {
            EditPrompt::Search {
                mut value,
                mut cursor,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let pattern = value.trim().to_string();
                    if !pattern.is_empty() {
                        let from = next_pos(&st.ed);
                        self.editor_find(&pattern, from);
                    }
                }
                code => {
                    edit_line(&mut value, &mut cursor, code, key.modifiers);
                    st.prompt = Some(EditPrompt::Search { value, cursor });
                }
            },
            EditPrompt::ReplaceFind {
                mut value,
                mut cursor,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let pattern = value.trim().to_string();
                    if !pattern.is_empty() {
                        st.ed.search = pattern.clone();
                        st.prompt = Some(EditPrompt::ReplaceWith {
                            pattern,
                            value: String::new(),
                            cursor: 0,
                        });
                    }
                }
                code => {
                    edit_line(&mut value, &mut cursor, code, key.modifiers);
                    st.prompt = Some(EditPrompt::ReplaceFind { value, cursor });
                }
            },
            EditPrompt::ReplaceWith {
                pattern,
                mut value,
                mut cursor,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let re = match rcmd_edit::Editor::compile(&pattern) {
                        Ok(re) => re,
                        Err(_) => {
                            st.note = Some(" bad pattern ".into());
                            return;
                        }
                    };
                    match st.ed.find_from(st.ed.cursor, &re) {
                        Some(m) => {
                            select_match(&mut st.ed, m);
                            st.prompt = Some(EditPrompt::ConfirmReplace {
                                pattern,
                                replacement: value,
                                m,
                                count: 0,
                                button: 0,
                            });
                        }
                        None => st.note = Some(" not found ".into()),
                    }
                }
                code => {
                    edit_line(&mut value, &mut cursor, code, key.modifiers);
                    st.prompt = Some(EditPrompt::ReplaceWith {
                        pattern,
                        value,
                        cursor,
                    });
                }
            },
            EditPrompt::ConfirmReplace {
                pattern,
                replacement,
                m,
                mut count,
                mut button,
            } => {
                enum Act {
                    Replace,
                    Skip,
                    All,
                    Quit,
                    None,
                }
                let act = match key.code {
                    KeyCode::Left => {
                        button = button.checked_sub(1).unwrap_or(3);
                        Act::None
                    }
                    KeyCode::Right | KeyCode::Tab => {
                        button = (button + 1) % 4;
                        Act::None
                    }
                    KeyCode::Enter => [Act::Replace, Act::Skip, Act::All, Act::Quit]
                        .into_iter()
                        .nth(button)
                        .unwrap_or(Act::None),
                    KeyCode::Char('y' | 'r') => Act::Replace,
                    KeyCode::Char('n' | 's') => Act::Skip,
                    KeyCode::Char('a') => Act::All,
                    KeyCode::Char('q') | KeyCode::Esc => Act::Quit,
                    _ => Act::None,
                };
                let re = match rcmd_edit::Editor::compile(&pattern) {
                    Ok(re) => re,
                    Err(_) => return,
                };
                let finish = |st: &mut EditorState, count: usize| {
                    st.ed.clear_selection();
                    st.note = Some(format!(" {count} replaced "));
                };
                match act {
                    Act::None => {
                        st.prompt = Some(EditPrompt::ConfirmReplace {
                            pattern,
                            replacement,
                            m,
                            count,
                            button,
                        });
                    }
                    Act::Quit => finish(st, count),
                    Act::Replace | Act::Skip => {
                        let from = match act {
                            Act::Replace => {
                                if let Some(hl) = st.hl.as_mut() {
                                    hl.invalidate_from(m.pos.line);
                                }
                                st.ed.replace_match_with_groups(m, &re, &replacement);
                                count += 1;
                                st.ed.cursor
                            }
                            _ => st.ed.after_match(m),
                        };
                        match st.ed.find_from(from, &re) {
                            // stop when the search wraps back around
                            Some(next) if next.pos >= from => {
                                select_match(&mut st.ed, next);
                                st.prompt = Some(EditPrompt::ConfirmReplace {
                                    pattern,
                                    replacement,
                                    m: next,
                                    count,
                                    button,
                                });
                            }
                            _ => finish(st, count),
                        }
                    }
                    Act::All => {
                        let mut m = m;
                        loop {
                            if let Some(hl) = st.hl.as_mut() {
                                hl.invalidate_from(m.pos.line);
                            }
                            st.ed.replace_match_with_groups(m, &re, &replacement);
                            count += 1;
                            if count > 1_000_000 {
                                break;
                            }
                            match st.ed.find_from(st.ed.cursor, &re) {
                                Some(next) if next.pos >= st.ed.cursor => m = next,
                                _ => break,
                            }
                        }
                        finish(st, count);
                    }
                }
            }
            EditPrompt::ConfirmQuit { mut button } => match key.code {
                KeyCode::Esc | KeyCode::Char('c') => {}
                KeyCode::Char('s') => {
                    if self.editor_save() {
                        self.close_editor();
                    }
                }
                KeyCode::Char('d') => self.close_editor(),
                KeyCode::Enter => match button {
                    0 => {
                        if self.editor_save() {
                            self.close_editor();
                        }
                    }
                    1 => self.close_editor(),
                    _ => {}
                },
                KeyCode::Left => {
                    button = button.checked_sub(1).unwrap_or(2);
                    st.prompt = Some(EditPrompt::ConfirmQuit { button });
                }
                KeyCode::Right | KeyCode::Tab => {
                    button = (button + 1) % 3;
                    st.prompt = Some(EditPrompt::ConfirmQuit { button });
                }
                _ => st.prompt = Some(EditPrompt::ConfirmQuit { button }),
            },
            EditPrompt::Goto {
                mut value,
                mut cursor,
            } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => match value.trim().parse::<usize>() {
                    Ok(line) if line >= 1 => {
                        let line = (line - 1).min(st.ed.line_count().saturating_sub(1));
                        st.ed.goto(rcmd_edit::Pos { line, col: 0 }, false);
                    }
                    _ => st.note = Some(format!(" {} is not a line ", value.trim())),
                },
                code => {
                    edit_line(&mut value, &mut cursor, code, key.modifiers);
                    st.prompt = Some(EditPrompt::Goto { value, cursor });
                }
            },
            EditPrompt::Charset(row) => {
                match charset_pick_key(row, key) {
                    PickKey::Move(to) => st.prompt = Some(EditPrompt::Charset(to)),
                    PickKey::Close => {}
                    PickKey::Chose(to) => {
                        // re-reading is the only way to change what the
                        // bytes mean, so anything unsaved would go with
                        // it - mc re-reads too, and says so first
                        if st.ed.modified() {
                            st.note = Some(
                                " save first: changing the codepage re-reads the file ".into(),
                            );
                            return;
                        }
                        let (path, charset) = (st.ed.path.clone(), charset_at(to));
                        match rcmd_edit::Editor::open_in(&path, charset) {
                            Ok(mut ed) => {
                                ed.prefs = self.config.edit_prefs();
                                if let Some(st) = self.editor_mut() {
                                    st.ed = ed;
                                    st.top = 0;
                                    st.top_seg = 0;
                                    st.left = 0;
                                    if let Some(hl) = st.hl.as_mut() {
                                        hl.invalidate_from(0);
                                    }
                                    st.note = Some(format!(" {} ", CHARSET_ROWS[to]));
                                }
                            }
                            Err(err) => st.note = Some(format!(" {err} ")),
                        }
                    }
                    PickKey::Ignored => st.prompt = Some(EditPrompt::Charset(row)),
                }
            }
            EditPrompt::Syntax { mut row, mut top } => {
                let rows = syntax_rows();
                let page = SYNTAX_ROWS;
                let mut keep = true;
                match key.code {
                    KeyCode::Esc => keep = false,
                    KeyCode::Enter => {
                        keep = false;
                        // row 0 is plain text: no highlighter at all,
                        // which is also the fast path
                        st.hl = match row {
                            0 => None,
                            at => rcmd_edit::Highlighter::by_name(rows[at]),
                        };
                        st.note = Some(format!(" {} ", rows[row]));
                    }
                    KeyCode::Up => row = row.saturating_sub(1),
                    KeyCode::Down => row = (row + 1).min(rows.len() - 1),
                    KeyCode::PageUp => row = row.saturating_sub(page),
                    KeyCode::PageDown => row = (row + page).min(rows.len() - 1),
                    KeyCode::Home => row = 0,
                    KeyCode::End => row = rows.len() - 1,
                    // a letter jumps to the first syntax starting with
                    // it, which is the only way to walk 200 of them
                    KeyCode::Char(c) => {
                        let c = c.to_ascii_lowercase();
                        if let Some(at) = rows.iter().position(|name| {
                            name.chars()
                                .next()
                                .is_some_and(|f| f.to_ascii_lowercase() == c)
                        }) {
                            row = at;
                        }
                    }
                    _ => {}
                }
                if keep {
                    top = top.min(row).max((row + 1).saturating_sub(page));
                    st.prompt = Some(EditPrompt::Syntax { row, top });
                }
            }
            EditPrompt::Options(mut d) => {
                let rows = EDIT_OPTION_ROWS.len();
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        if d.cursor != rows || d.ok {
                            self.apply_edit_options(&d);
                        }
                    }
                    KeyCode::Char(' ') if d.cursor == rows => {
                        if d.ok {
                            self.apply_edit_options(&d);
                        } else {
                            st.prompt = Some(EditPrompt::Options(d));
                        }
                    }
                    KeyCode::Up | KeyCode::BackTab => {
                        d.step(-1);
                        st.prompt = Some(EditPrompt::Options(d));
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        d.step(1);
                        st.prompt = Some(EditPrompt::Options(d));
                    }
                    KeyCode::Left | KeyCode::Right
                        if d.nudge(if key.code == KeyCode::Left { -1 } else { 1 }) =>
                    {
                        st.prompt = Some(EditPrompt::Options(d));
                    }
                    KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => {
                        if d.cursor == rows {
                            d.ok = !d.ok;
                        } else {
                            d.toggle();
                        }
                        st.prompt = Some(EditPrompt::Options(d));
                    }
                    _ => st.prompt = Some(EditPrompt::Options(d)),
                }
            }
        }
    }

    /// OK on the editor options: the settings take effect in the open
    /// editor at once and are written through to the state file, the
    /// way the panel's options form does it.
    fn apply_edit_options(&mut self, d: &EditOptions) {
        let cfg = &mut self.config;
        cfg.edit_tab_size = d.tab_size;
        cfg.edit_fill_tabs = d.fill_tabs;
        cfg.edit_auto_indent = d.auto_indent;
        cfg.edit_backspace_tabs = d.backspace_tabs;
        cfg.edit_wrap_column = d.wrap_column;
        cfg.edit_line_numbers = d.line_numbers;
        cfg.edit_backups = d.backups;
        cfg.edit_clipboard = d.clipboard;
        ui::set_tab_size(d.tab_size as usize);
        let prefs = self.config.edit_prefs();
        if let Some(st) = self.editor_mut() {
            st.ed.prefs = prefs;
            st.wrap_column = d.wrap_column as usize;
            st.line_numbers = d.line_numbers;
            st.note = Some(" options saved ".into());
        }
        let (tab, fill, indent) = (d.tab_size, d.fill_tabs, d.auto_indent);
        let (bstab, wrap) = (d.backspace_tabs, d.wrap_column);
        let (numbers, backups, clip) = (d.line_numbers, d.backups, d.clipboard);
        if let Err(err) = state::update(move |s| {
            s.edit_tab_size = Some(tab);
            s.edit_fill_tabs = Some(fill);
            s.edit_auto_indent = Some(indent);
            s.edit_backspace_tabs = Some(bstab);
            s.edit_wrap_column = Some(wrap);
            s.edit_line_numbers = Some(numbers);
            s.edit_backups = Some(backups);
            s.edit_clipboard = Some(clip);
        }) && let Some(st) = self.editor_mut()
        {
            st.note = Some(format!(" could not save state: {err} "));
        }
    }

    /// F9 in the editor: mc's menu bar over the text.
    fn editor_menu_key(&mut self, key: KeyEvent) {
        let Some(st) = self.editor_mut() else {
            return;
        };
        let Some(ms) = st.menu.as_mut() else { return };
        let mut run = None;
        match key.code {
            KeyCode::Esc | KeyCode::F(9) => st.menu = None,
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                let len = EDIT_MENUS.len();
                ms.menu = if key.code == KeyCode::Left {
                    (ms.menu + len - 1) % len
                } else {
                    (ms.menu + 1) % len
                };
                ms.item = first_edit_item(EDIT_MENUS[ms.menu].1);
            }
            KeyCode::Up => ms.item = edit_menu_step(EDIT_MENUS[ms.menu].1, ms.item, -1),
            KeyCode::Down => ms.item = edit_menu_step(EDIT_MENUS[ms.menu].1, ms.item, 1),
            KeyCode::Enter => {
                if let Some((_, _, action)) = EDIT_MENUS[ms.menu].1[ms.item] {
                    st.menu = None;
                    run = Some(action);
                }
            }
            KeyCode::Char(c) => {
                // the open menu's entry letters first, then the titles
                let c = c.to_ascii_lowercase();
                let entry = EDIT_MENUS[ms.menu]
                    .1
                    .iter()
                    .flatten()
                    .find(|(label, ..)| menu_hotkey(label) == Some(c));
                if let Some(&(_, _, action)) = entry {
                    st.menu = None;
                    run = Some(action);
                } else if let Some(menu) = EDIT_MENUS
                    .iter()
                    .position(|(title, _)| menu_hotkey(title) == Some(c))
                {
                    ms.menu = menu;
                    ms.item = first_edit_item(EDIT_MENUS[menu].1);
                }
            }
            _ => {}
        }
        match run {
            Some(EditMenuAction::Key(action)) => self.editor_action(action),
            Some(EditMenuAction::Options) => self.open_edit_options(),
            Some(EditMenuAction::ScreenList) => self.open_screen_list(),
            Some(EditMenuAction::Syntax) => self.open_syntax_picker(),
            None => {}
        }
    }

    /// Options > Syntax: the list, opened on what is in force now.
    fn open_syntax_picker(&mut self) {
        let rows = syntax_rows();
        let Some(st) = self.editor_mut() else {
            return;
        };
        let now = st.hl.as_ref().map(|hl| hl.syntax_name()).unwrap_or("");
        let row = rows.iter().position(|name| *name == now).unwrap_or(0);
        st.prompt = Some(EditPrompt::Syntax {
            row,
            top: row.saturating_sub(5),
        });
    }

    /// The editor options form, filled from what is in force now.
    fn open_edit_options(&mut self) {
        let cfg = &self.config;
        let dialog = EditOptions {
            tab_size: cfg.edit_tab_size.clamp(1, 16),
            fill_tabs: cfg.edit_fill_tabs,
            auto_indent: cfg.edit_auto_indent,
            backspace_tabs: cfg.edit_backspace_tabs,
            wrap_column: cfg.edit_wrap_column,
            line_numbers: cfg.edit_line_numbers,
            backups: cfg.edit_backups,
            clipboard: cfg.edit_clipboard,
            cursor: 0,
            ok: true,
        };
        if let Some(st) = self.editor_mut() {
            st.prompt = Some(EditPrompt::Options(dialog));
        }
    }

    /// Scroll the editor viewport so the cursor stays on screen.
    fn ensure_editor_visible(&mut self) {
        let Some(st) = self.editor_mut() else {
            return;
        };
        let rows = st.rows.max(1);
        let cols = st.wrap_width();
        if st.wrap {
            st.left = 0;
            let segs_of = |ed: &rcmd_edit::Editor, line: usize| ui::ed_line_segs(ed, line, cols);
            if st.top >= st.ed.line_count() {
                st.top = st.ed.line_count().saturating_sub(1);
                st.top_seg = 0;
            }
            if st.top_seg >= segs_of(&st.ed, st.top) {
                st.top_seg = 0;
            }
            let cline = st.ed.cursor.line;
            let cseg = ui::screen_col(&st.ed.line(cline), st.ed.cursor.col) / cols;
            if (cline, cseg) < (st.top, st.top_seg) {
                st.top = cline;
                st.top_seg = cseg;
                return;
            }
            // cursor at or below the top: done if it fits in the window
            let (mut line, mut seg) = (st.top, st.top_seg);
            for _ in 0..rows {
                if (line, seg) == (cline, cseg) {
                    return;
                }
                seg += 1;
                if seg >= segs_of(&st.ed, line) {
                    line += 1;
                    seg = 0;
                }
            }
            // below: walk rows-1 visual rows back from the cursor
            let (mut line, mut seg) = (cline, cseg);
            for _ in 0..rows.saturating_sub(1) {
                if seg > 0 {
                    seg -= 1;
                } else if line > 0 {
                    line -= 1;
                    seg = segs_of(&st.ed, line) - 1;
                } else {
                    break;
                }
            }
            st.top = line;
            st.top_seg = seg;
            return;
        }
        st.top_seg = 0;
        if st.ed.cursor.line < st.top {
            st.top = st.ed.cursor.line;
        }
        if st.ed.cursor.line >= st.top + rows {
            st.top = st.ed.cursor.line + 1 - rows;
        }
        let scol = ui::screen_col(&st.ed.line(st.ed.cursor.line), st.ed.cursor.col);
        if scol < st.left {
            st.left = scol;
        }
        if scol >= st.left + cols {
            st.left = scol + 1 - cols;
        }
    }

    /// While a find streams: Esc cancels, navigation browses the results
    /// as they arrive, everything else waits.
    fn on_find_key(&mut self, key: KeyEvent) {
        let page = self.panel_rows.saturating_sub(1).max(1);
        match key.code {
            KeyCode::Esc => {
                if let Some(find) = &self.find {
                    find.handle.cancel();
                }
            }
            KeyCode::Up => self.panel().move_up(),
            KeyCode::Down => self.panel().move_down(),
            KeyCode::PageUp => self.panel().page_up(page),
            KeyCode::PageDown => self.panel().page_down(page),
            KeyCode::Home => self.panel().move_top(),
            KeyCode::End => self.panel().move_bottom(),
            _ => {}
        }
    }

    fn on_job_key(&mut self, key: KeyEvent) {
        let Some(job) = self.fg_job_mut() else { return };
        let Some(ask) = &job.ask else {
            match key.code {
                KeyCode::Esc => job.handle.cancel(),
                // detach: the job keeps running, panels come back
                KeyCode::Char('b' | 'B') => job.background = true,
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

    fn on_dialog_key(&mut self, key: KeyEvent) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        match dialog {
            Dialog::Input(mut d) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.submit_input(d),
                code => {
                    edit_line(&mut d.value, &mut d.cursor, code, key.modifiers);
                    self.dialog = Some(Dialog::Input(d));
                }
            },
            Dialog::RenamePreview(mut d) => match key.code {
                KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                    self.status = Some(" bulk rename cancelled ".into());
                }
                KeyCode::Char('y' | 'Y') => self.apply_bulk_rename(d),
                KeyCode::Enter => {
                    if d.yes {
                        self.apply_bulk_rename(d);
                    } else {
                        self.status = Some(" bulk rename cancelled ".into());
                    }
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    d.yes = !d.yes;
                    self.dialog = Some(Dialog::RenamePreview(d));
                }
                _ => self.dialog = Some(Dialog::RenamePreview(d)),
            },
            Dialog::History(mut selected) => {
                // rows are newest first; Enter puts one on the line
                let len = self.cmdline.history().len();
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        let history = self.cmdline.history();
                        if let Some(cmd) = history.get(len.saturating_sub(1) - selected).cloned() {
                            self.cmdline.set_line(&cmd);
                        }
                    }
                    KeyCode::Up => {
                        self.dialog = Some(Dialog::History(selected.saturating_sub(1)));
                    }
                    KeyCode::Down => {
                        if selected + 1 < len {
                            selected += 1;
                        }
                        self.dialog = Some(Dialog::History(selected));
                    }
                    _ => self.dialog = Some(Dialog::History(selected)),
                }
            }
            Dialog::Vfs(mut d) => {
                let len = d.rows.len();
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        if let Some(row) = d.rows.get(d.selected) {
                            let (target, remote) = (row.target.clone(), row.remote);
                            if remote {
                                // through the cache: no second login
                                self.connect_remote(&target);
                            } else if let Err(err) =
                                self.panels[self.active].open_archive(PathBuf::from(&target))
                            {
                                self.status = Some(format!(" {err} "));
                            }
                        }
                    }
                    KeyCode::F(8) | KeyCode::Delete | KeyCode::Char('f' | 'F') => {
                        if let Some(row) = d.rows.get(d.selected) {
                            let row = VfsRow {
                                label: row.label.clone(),
                                target: row.target.clone(),
                                used_by: row.used_by.clone(),
                                remote: row.remote,
                            };
                            self.free_vfs(&row);
                        }
                        // freeing changes the list under the cursor
                        let rows = self.vfs_rows();
                        if !rows.is_empty() {
                            let selected = d.selected.min(rows.len() - 1);
                            self.dialog = Some(Dialog::Vfs(VfsDialog { rows, selected }));
                        }
                    }
                    KeyCode::Up => {
                        d.selected = d.selected.saturating_sub(1);
                        self.dialog = Some(Dialog::Vfs(d));
                    }
                    KeyCode::Down => {
                        if d.selected + 1 < len {
                            d.selected += 1;
                        }
                        self.dialog = Some(Dialog::Vfs(d));
                    }
                    _ => self.dialog = Some(Dialog::Vfs(d)),
                }
            }
            Dialog::FindResults(mut d) => {
                let shown = ui::find_list_rows(self.areas.screen);
                let page = shown.saturating_sub(1).max(1) as isize;
                match key.code {
                    KeyCode::Esc | KeyCode::F(10) | KeyCode::Char('q') => self.close_find(),
                    KeyCode::Up => {
                        d.step(-1, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::Down => {
                        d.step(1, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::PageUp => {
                        d.step(-page, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::PageDown => {
                        d.step(page, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::Home => {
                        d.step(isize::MIN / 2, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::End => {
                        d.step(isize::MAX / 2, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::Left | KeyCode::BackTab => {
                        d.button = (d.button + FIND_BUTTONS.len() - 1) % FIND_BUTTONS.len();
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::Right | KeyCode::Tab => {
                        d.button = (d.button + 1) % FIND_BUTTONS.len();
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    // Enter on the list is Chdir, which is what mc's
                    // default button does and what the row invites
                    KeyCode::Enter => self.find_button(*d, None),
                    KeyCode::F(3) => self.find_button(*d, Some(3)),
                    KeyCode::F(4) => self.find_button(*d, Some(4)),
                    KeyCode::Char(c) => {
                        let c = c.to_ascii_lowercase();
                        match FIND_BUTTONS.iter().position(|b| {
                            b.chars()
                                .next()
                                .is_some_and(|f| f.to_ascii_lowercase() == c)
                        }) {
                            Some(at) => self.find_button(*d, Some(at)),
                            None => self.dialog = Some(Dialog::FindResults(d)),
                        }
                    }
                    _ => self.dialog = Some(Dialog::FindResults(d)),
                }
            }
            Dialog::Pattern(mut d) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if d.row != PATTERN_ROWS || d.ok {
                        self.submit_pattern(&d);
                    }
                }
                KeyCode::Up | KeyCode::BackTab => {
                    d.step(-1);
                    self.dialog = Some(Dialog::Pattern(d));
                }
                KeyCode::Down | KeyCode::Tab => {
                    d.step(1);
                    self.dialog = Some(Dialog::Pattern(d));
                }
                KeyCode::Char(' ') if d.row != 0 => {
                    if d.row == PATTERN_ROWS {
                        d.ok = !d.ok;
                    } else {
                        d.toggle();
                    }
                    self.dialog = Some(Dialog::Pattern(d));
                }
                KeyCode::Left | KeyCode::Right if d.row == PATTERN_ROWS => {
                    d.ok = !d.ok;
                    self.dialog = Some(Dialog::Pattern(d));
                }
                code if d.row == 0 => {
                    let (value, cursor) = (&mut d.value, &mut d.cursor);
                    edit_line(value, cursor, code, key.modifiers);
                    self.dialog = Some(Dialog::Pattern(d));
                }
                _ => self.dialog = Some(Dialog::Pattern(d)),
            },
            Dialog::Panelize(mut d) => {
                let presets = self.config.panelize.clone();
                // saving asks for a name in the same field: there is
                // one dialog slot, and a name is one line of typing
                if let Some(mut name) = d.naming.take() {
                    match key.code {
                        KeyCode::Esc => {
                            self.dialog = Some(Dialog::Panelize(d));
                        }
                        KeyCode::Enter => {
                            let name = name.trim().to_string();
                            if !name.is_empty() {
                                self.save_panelize(
                                    Some(crate::config::PanelizePreset {
                                        name,
                                        run: d.value.clone(),
                                    }),
                                    None,
                                );
                            }
                            d.row = self.config.panelize.len().saturating_sub(1);
                            self.dialog = Some(Dialog::Panelize(d));
                        }
                        code => {
                            let mut cursor = name.chars().count();
                            edit_line(&mut name, &mut cursor, code, key.modifiers);
                            d.naming = Some(name);
                            self.dialog = Some(Dialog::Panelize(d));
                        }
                    }
                    return;
                }
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        let command = match (d.on_list, presets.get(d.row)) {
                            (true, Some(preset)) => preset.run.clone(),
                            _ => d.value.trim().to_string(),
                        };
                        if command.is_empty() {
                            self.dialog = Some(Dialog::Panelize(d));
                        } else {
                            self.run_panelize(&command);
                        }
                    }
                    KeyCode::Tab | KeyCode::BackTab if !presets.is_empty() => {
                        d.on_list = !d.on_list;
                        self.dialog = Some(Dialog::Panelize(d));
                    }
                    KeyCode::Up | KeyCode::Down if d.on_list => {
                        let last = presets.len().saturating_sub(1);
                        d.row = match key.code {
                            KeyCode::Up => d.row.saturating_sub(1),
                            _ => (d.row + 1).min(last),
                        };
                        // the highlighted command is what Enter runs,
                        // so it shows in the field as well
                        if let Some(preset) = presets.get(d.row) {
                            d.value = preset.run.clone();
                            d.cursor = d.value.chars().count();
                        }
                        self.dialog = Some(Dialog::Panelize(d));
                    }
                    // C-s saves what is typed, F8 drops what is picked
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if !d.value.trim().is_empty() {
                            d.naming = Some(String::new());
                        }
                        self.dialog = Some(Dialog::Panelize(d));
                    }
                    KeyCode::F(8) | KeyCode::Delete if d.on_list && !presets.is_empty() => {
                        self.save_panelize(None, Some(d.row));
                        d.row = d.row.min(self.config.panelize.len().saturating_sub(1));
                        d.on_list = !self.config.panelize.is_empty();
                        self.dialog = Some(Dialog::Panelize(d));
                    }
                    code => {
                        if !d.on_list {
                            let (value, cursor) = (&mut d.value, &mut d.cursor);
                            edit_line(value, cursor, code, key.modifiers);
                        }
                        self.dialog = Some(Dialog::Panelize(d));
                    }
                }
            }
            Dialog::Compare(row) => {
                let last = COMPARE_MODES.len() - 1;
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => self.dialog = Some(Dialog::Compare(row.saturating_sub(1))),
                    KeyCode::Down | KeyCode::Tab => {
                        self.dialog = Some(Dialog::Compare((row + 1).min(last)))
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => self.compare_dirs(COMPARE_MODES[row].1),
                    // q, s, t: the first letter of each answer
                    KeyCode::Char(c) => {
                        let c = c.to_ascii_lowercase();
                        match COMPARE_MODES.iter().position(|(label, _)| {
                            label
                                .chars()
                                .next()
                                .is_some_and(|f| f.to_ascii_lowercase() == c)
                        }) {
                            Some(at) => self.compare_dirs(COMPARE_MODES[at].1),
                            None => self.dialog = Some(Dialog::Compare(row)),
                        }
                    }
                    _ => self.dialog = Some(Dialog::Compare(row)),
                }
            }
            Dialog::Charset(row) => match charset_pick_key(row, key) {
                PickKey::Move(to) => self.dialog = Some(Dialog::Charset(to)),
                PickKey::Close => {}
                PickKey::Chose(to) => {
                    let side = self.active;
                    self.panels[side].charset = charset_at(to);
                    self.status = Some(format!(" {} ", CHARSET_ROWS[to]));
                }
                PickKey::Ignored => self.dialog = Some(Dialog::Charset(row)),
            },
            Dialog::Jobs(mut selected) => {
                let len = self.jobs.len();
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        // bring it to the front: its dialog returns
                        if let Some(job) = self.jobs.get_mut(selected) {
                            job.background = false;
                        }
                    }
                    KeyCode::Char('c' | 'C') | KeyCode::Delete => {
                        if let Some(job) = self.jobs.get(selected) {
                            job.handle.cancel();
                        }
                        self.dialog = Some(Dialog::Jobs(selected));
                    }
                    KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                        self.dialog = Some(Dialog::Jobs(selected));
                    }
                    KeyCode::Down => {
                        if selected + 1 < len {
                            selected += 1;
                        }
                        self.dialog = Some(Dialog::Jobs(selected));
                    }
                    _ => self.dialog = Some(Dialog::Jobs(selected)),
                }
            }
            Dialog::Confirm(mut d) => match key.code {
                KeyCode::Esc | KeyCode::Char('n') => self.confirm_no(&d),
                KeyCode::Char('y') => self.confirm_yes(d),
                KeyCode::Enter => {
                    if d.yes {
                        self.confirm_yes(d);
                    } else {
                        self.confirm_no(&d);
                    }
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    d.yes = !d.yes;
                    self.dialog = Some(Dialog::Confirm(d));
                }
                _ => self.dialog = Some(Dialog::Confirm(d)),
            },
            Dialog::Hotlist(mut selected) => {
                let len = self.config.hotlist.len();
                let recent = self.hotlist_recent();
                let total = len + recent.len();
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        if let Some(entry) = self.config.hotlist.get(selected).cloned() {
                            if is_remote_url(&entry.path) {
                                self.connect_remote(&entry.path);
                            } else {
                                let target = self.resolve(&entry.path);
                                let panel = &mut self.panels[self.active];
                                let moved = if panel.is_remote() {
                                    panel.to_local(target)
                                } else {
                                    panel.cd(target)
                                };
                                if let Err(err) = moved {
                                    self.status = Some(format!(" hotlist: {err} "));
                                }
                            }
                        } else if let Some(loc) = recent.get(selected - len) {
                            // recent entries are display paths / sftp URLs
                            self.navigate(&loc.clone());
                        }
                    }
                    KeyCode::Char('a') => {
                        let panel = &self.panels[self.active];
                        let path = if panel.is_remote() {
                            panel.display_path()
                        } else {
                            panel.local_cwd().display().to_string()
                        };
                        if !self.config.hotlist.iter().any(|h| h.path == path) {
                            let label = Path::new(&path)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.clone());
                            self.config.hotlist.push(HotEntry { label, path });
                            self.save_hotlist();
                        }
                        self.dialog = Some(Dialog::Hotlist(selected));
                    }
                    KeyCode::Char('d') => {
                        match self.config.hotlist.get(selected) {
                            // only the pinned half can be dropped; the
                            // recent half is a log, not a list
                            Some(entry) if self.config.confirm_hotlist_delete => {
                                let label = entry.label.clone();
                                self.dialog = Some(Dialog::Confirm(ConfirmDialog {
                                    title: " Hotlist ".into(),
                                    message: format!("Drop \"{label}\" from the hotlist?"),
                                    yes: true,
                                    paths: Vec::new(),
                                    permanent: false,
                                    kind: ConfirmKind::HotlistDelete { index: selected },
                                    command: None,
                                }));
                            }
                            Some(_) => {
                                self.config.hotlist.remove(selected);
                                self.save_hotlist();
                                let total = self.config.hotlist.len() + recent.len();
                                self.dialog =
                                    Some(Dialog::Hotlist(selected.min(total.saturating_sub(1))));
                            }
                            None => self.dialog = Some(Dialog::Hotlist(selected)),
                        }
                    }
                    KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                        self.dialog = Some(Dialog::Hotlist(selected));
                    }
                    KeyCode::Down => {
                        if selected + 1 < total {
                            selected += 1;
                        }
                        self.dialog = Some(Dialog::Hotlist(selected));
                    }
                    _ => self.dialog = Some(Dialog::Hotlist(selected)),
                }
            }
            Dialog::Link(mut d) => {
                let last = d.rows(); // the button row
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => {
                        d.row = d.row.checked_sub(1).unwrap_or(last);
                        self.dialog = Some(Dialog::Link(d));
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        d.row = if d.row >= last { 0 } else { d.row + 1 };
                        self.dialog = Some(Dialog::Link(d));
                    }
                    KeyCode::Left | KeyCode::Right if d.row == last => {
                        d.ok = !d.ok;
                        self.dialog = Some(Dialog::Link(d));
                    }
                    KeyCode::Enter => {
                        if d.ok {
                            self.submit_link(*d);
                        }
                    }
                    code => {
                        match d.row {
                            0 => {
                                edit_line(&mut d.target, &mut d.target_cursor, code, key.modifiers);
                            }
                            1 => {
                                edit_line(&mut d.name, &mut d.name_cursor, code, key.modifiers);
                            }
                            _ => {}
                        }
                        self.dialog = Some(Dialog::Link(d));
                    }
                }
            }
            Dialog::Chown(mut d) => {
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up if d.column < 2 => {
                        d.move_by(-1);
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::Down if d.column < 2 => {
                        d.move_by(1);
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::PageUp if d.column < 2 => {
                        d.move_by(-(CHOWN_ROWS as isize));
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::PageDown if d.column < 2 => {
                        d.move_by(CHOWN_ROWS as isize);
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::Home if d.column < 2 => {
                        d.move_by(isize::MIN / 2);
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::End if d.column < 2 => {
                        d.move_by(isize::MAX / 2);
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    // Tab walks user list -> group list -> recurse -> buttons
                    KeyCode::Tab => {
                        d.column = (d.column + 1) % CHOWN_STOPS;
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    // ...and only the box itself takes Space. A letter
                    // key must never flip it: names get typed at these
                    // lists, and "jarda" would tick it on the r
                    KeyCode::Char(' ') if d.column == CHOWN_RECURSE_COL => {
                        d.recurse = !d.recurse;
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::Left | KeyCode::Right => {
                        if d.column == CHOWN_BUTTON_COL {
                            let count = CHOWN_BUTTONS.len();
                            d.button = if key.code == KeyCode::Left {
                                d.button.checked_sub(1).unwrap_or(count - 1)
                            } else {
                                (d.button + 1) % count
                            };
                        } else {
                            d.column = if key.code == KeyCode::Left { 0 } else { 1 };
                        }
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::Down | KeyCode::Up => {
                        // on the button row, up returns to the lists
                        d.column = 0;
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::Enter => {
                        if d.column == CHOWN_RECURSE_COL {
                            // Enter on the box is a Set, as it is on any
                            // other row of a form
                            d.column = CHOWN_BUTTON_COL;
                        }
                        if CHOWN_BUTTONS.get(d.button) != Some(&"Cancel") {
                            let (uid, gid) = d.picked();
                            let paths = d.paths.clone();
                            if d.recurse {
                                self.start_attrs_job(
                                    paths,
                                    fsops::Attrs {
                                        uid,
                                        gid,
                                        ..Default::default()
                                    },
                                    "chown",
                                );
                            } else {
                                self.apply_fs_op(&paths, "chown", |w, p| w.set_owner(p, uid, gid));
                            }
                        }
                    }
                    _ => self.dialog = Some(Dialog::Chown(d)),
                }
            }
            Dialog::Chmod(mut d) => {
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => {
                        d.row = d.row.checked_sub(1).unwrap_or(CHMOD_ROWS);
                        self.dialog = Some(Dialog::Chmod(d));
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        d.row = if d.row >= CHMOD_ROWS { 0 } else { d.row + 1 };
                        self.dialog = Some(Dialog::Chmod(d));
                    }
                    KeyCode::Left | KeyCode::Right if d.row == CHMOD_ROWS => {
                        let count = CHMOD_BUTTONS.len();
                        d.button = if key.code == KeyCode::Left {
                            d.button.checked_sub(1).unwrap_or(count - 1)
                        } else {
                            (d.button + 1) % count
                        };
                        self.dialog = Some(Dialog::Chmod(d));
                    }
                    // Space flips a bit, and the octal follows along
                    KeyCode::Char(' ') if d.row < CHMOD_BITS.len() => {
                        d.mode ^= CHMOD_BITS[d.row].1;
                        d.sync_octal();
                        self.dialog = Some(Dialog::Chmod(d));
                    }
                    KeyCode::Char(' ') if d.row == CHMOD_RECURSE_ROW => {
                        d.recurse = !d.recurse;
                        self.dialog = Some(Dialog::Chmod(d));
                    }
                    KeyCode::Enter => self.submit_chmod(*d),
                    code => {
                        // ...and typing in the octal moves the bits
                        if d.row == CHMOD_OCTAL_ROW {
                            edit_line(&mut d.octal, &mut d.octal_cursor, code, key.modifiers);
                            d.sync_mode();
                        }
                        self.dialog = Some(Dialog::Chmod(d));
                    }
                }
            }
            Dialog::Transfer(mut d) => {
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => {
                        d.row = d.row.checked_sub(1).unwrap_or(TRANSFER_ROWS);
                        self.dialog = Some(Dialog::Transfer(d));
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        d.row = if d.row >= TRANSFER_ROWS { 0 } else { d.row + 1 };
                        self.dialog = Some(Dialog::Transfer(d));
                    }
                    KeyCode::Enter => self.submit_transfer(*d),
                    // on the button row the arrows pick a button, on a
                    // checkbox row Space flips it, and the destination
                    // line takes everything else as typing
                    KeyCode::Left | KeyCode::Right if d.row == TRANSFER_ROWS => {
                        d.button = if key.code == KeyCode::Left {
                            d.button.checked_sub(1).unwrap_or(2)
                        } else {
                            (d.button + 1) % 3
                        };
                        self.dialog = Some(Dialog::Transfer(d));
                    }
                    KeyCode::Char(' ')
                        if (TRANSFER_DEST_ROW + 1..TRANSFER_ROWS).contains(&d.row) =>
                    {
                        let row = d.row - TRANSFER_DEST_ROW - 1;
                        d.toggle(row);
                        self.dialog = Some(Dialog::Transfer(d));
                    }
                    code => {
                        match d.row {
                            0 => {
                                edit_line(&mut d.mask, &mut d.mask_cursor, code, key.modifiers);
                            }
                            TRANSFER_DEST_ROW => {
                                edit_line(&mut d.dest, &mut d.cursor, code, key.modifiers);
                            }
                            _ => {}
                        }
                        self.dialog = Some(Dialog::Transfer(d));
                    }
                }
            }
            Dialog::Tree(mut tree) => {
                let plain = !key.modifiers.contains(KeyModifiers::ALT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => {}
                    // mc: Enter leaves the tree and takes *this* panel
                    // to the selected directory
                    KeyCode::Enter => {
                        if let Some(target) = tree.selected_path() {
                            let panel = &mut self.panels[self.active];
                            let moved = if panel.is_remote() {
                                panel.to_local(target)
                            } else {
                                panel.cd(target)
                            };
                            if let Err(err) = moved {
                                self.status = Some(format!(" tree: {err} "));
                            }
                        }
                    }
                    code => {
                        match code {
                            KeyCode::Up => tree.up(),
                            KeyCode::Down => tree.down(),
                            KeyCode::PageUp => tree.page_up(TREE_ROWS),
                            KeyCode::PageDown => tree.page_down(TREE_ROWS),
                            KeyCode::Home => tree.first(),
                            KeyCode::End => tree.last(),
                            KeyCode::Left => tree.left(),
                            KeyCode::Right => tree.right(),
                            KeyCode::F(2) => tree.rescan(),
                            KeyCode::F(3) => tree.forget(),
                            KeyCode::F(4) => tree.toggle_mode(),
                            KeyCode::Char('r') if !plain => tree.rescan(),
                            KeyCode::Char('s') if !plain => tree.search_next(),
                            KeyCode::Backspace => tree.search_pop(),
                            // mc's type-to-search: any other character
                            // jumps to the next directory starting with
                            // what has been typed so far
                            KeyCode::Char(c) if plain => {
                                tree.search_push(c);
                            }
                            _ => {}
                        }
                        self.dialog = Some(Dialog::Tree(tree));
                    }
                }
            }
            Dialog::UserMenu(mut selected) => {
                let len = self.config.commands.len();
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => self.run_user_command(selected),
                    KeyCode::Char(c @ '1'..='9') => {
                        let i = c as usize - '1' as usize;
                        if i < len {
                            self.run_user_command(i);
                        } else {
                            self.dialog = Some(Dialog::UserMenu(selected));
                        }
                    }
                    KeyCode::Up => {
                        self.dialog = Some(Dialog::UserMenu(selected.saturating_sub(1)));
                    }
                    KeyCode::Down => {
                        if selected + 1 < len {
                            selected += 1;
                        }
                        self.dialog = Some(Dialog::UserMenu(selected));
                    }
                    _ => self.dialog = Some(Dialog::UserMenu(selected)),
                }
            }
            Dialog::Options(mut d) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if d.cursor != OPTION_ROWS.len() || d.ok {
                        self.apply_options(&d);
                    }
                }
                KeyCode::Char(' ') if d.cursor == OPTION_ROWS.len() => {
                    // Space presses the focused button, like MC
                    if d.ok {
                        self.apply_options(&d);
                    }
                }
                KeyCode::Up => {
                    d.step(-1);
                    self.dialog = Some(Dialog::Options(d));
                }
                KeyCode::Down | KeyCode::Tab => {
                    d.step(1);
                    self.dialog = Some(Dialog::Options(d));
                }
                KeyCode::Left | KeyCode::Right
                    if d.nudge(if key.code == KeyCode::Left { -5 } else { 5 }) =>
                {
                    self.dialog = Some(Dialog::Options(d));
                }
                KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => {
                    if d.cursor == OPTION_ROWS.len() {
                        d.ok = !d.ok;
                    } else {
                        d.toggle();
                    }
                    self.dialog = Some(Dialog::Options(d));
                }
                _ => self.dialog = Some(Dialog::Options(d)),
            },
            Dialog::Find(mut d) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if d.row != FIND_ROWS || d.ok {
                        self.submit_find(*d);
                    }
                }
                KeyCode::Tab | KeyCode::Down => {
                    d.step(1);
                    self.dialog = Some(Dialog::Find(d));
                }
                KeyCode::BackTab | KeyCode::Up => {
                    d.step(-1);
                    self.dialog = Some(Dialog::Find(d));
                }
                KeyCode::Char(' ') if d.row >= FIND_FIELDS => {
                    if d.row == FIND_ROWS {
                        d.ok = !d.ok;
                    } else {
                        d.toggle();
                    }
                    self.dialog = Some(Dialog::Find(d));
                }
                KeyCode::Left | KeyCode::Right if d.row == FIND_ROWS => {
                    d.ok = !d.ok;
                    self.dialog = Some(Dialog::Find(d));
                }
                code => {
                    if let Some((value, cursor)) = d.field() {
                        edit_line(value, cursor, code, key.modifiers);
                    }
                    self.dialog = Some(Dialog::Find(d));
                }
            },
        }
    }

    /// Stop a running find and put its window away.
    fn close_find(&mut self) {
        if let Some(find) = self.find.take() {
            find.handle.cancel();
        }
        self.dialog = None;
    }

    /// One of the six things the results window can do with the match
    /// under the cursor. `None` = the focused button.
    fn find_button(&mut self, d: FindResults, button: Option<usize>) {
        let button = button.unwrap_or(d.button);
        let target = d.rows.get(d.selected).cloned();
        match (FIND_BUTTONS[button], target) {
            ("Quit", _) => self.close_find(),
            ("Again", _) => {
                self.close_find();
                self.dialog = Some(Dialog::Find(d.query));
            }
            ("Panelize", _) => {
                // the list becomes the panel, which is where marking
                // and F5/F6/F8 live
                let root = d.root.clone();
                let entries: Vec<_> = d
                    .rows
                    .iter()
                    .filter_map(|path| {
                        let mut entry = rcmd_core::entry::stat(path).ok()?;
                        if let Ok(rel) = path.strip_prefix(&root) {
                            entry.name = rel.as_os_str().to_os_string();
                        }
                        Some(entry)
                    })
                    .collect();
                let (label, side) = (d.label.clone(), self.active);
                self.close_find();
                let _ = self.panels[side].request_dir(root, LoadKind::Enter);
                self.panels[side].panelize(entries, label);
            }
            (_, None) => self.dialog = Some(Dialog::FindResults(Box::new(d))),
            ("Chdir", Some(path)) => {
                let side = self.active;
                self.close_find();
                if let Some(dir) = path.parent() {
                    let _ = self.panels[side].request_dir(dir.to_path_buf(), LoadKind::Enter);
                    if let Some(name) = path.file_name() {
                        self.panels[side].select_name(name);
                    }
                }
            }
            ("View", Some(path)) | ("Edit", Some(path)) => {
                let side = self.active;
                let edit = FIND_BUTTONS[button] == "Edit";
                self.close_find();
                // the panel goes where the file is first, so quitting
                // the viewer leaves you standing on what you read
                if let Some(dir) = path.parent() {
                    let _ = self.panels[side].request_dir(dir.to_path_buf(), LoadKind::Enter);
                    if let Some(name) = path.file_name() {
                        self.panels[side].select_name(name);
                    }
                }
                if edit {
                    self.open_editor();
                } else {
                    self.open_viewer(false);
                }
            }
            _ => {}
        }
    }

    /// Recent directories for the hotlist dialog: both panels'
    /// histories merged (active panel first), deduped, pinned entries
    /// and the place we're standing excluded, capped.
    pub fn hotlist_recent(&self) -> Vec<String> {
        let here = self.panels[self.active].display_path();
        let mut out: Vec<String> = Vec::new();
        let locations = self.panels[self.active]
            .recent_locations()
            .chain(self.panels[self.active ^ 1].recent_locations());
        for loc in locations {
            if loc == here
                || out.iter().any(|x| x == loc)
                || self.config.hotlist.iter().any(|h| h.path == loc)
            {
                continue;
            }
            out.push(loc.to_string());
            if out.len() == 15 {
                break;
            }
        }
        out
    }

    /// Hotlist edits write through to the state file like the options
    /// form - never to the user's config.
    fn save_hotlist(&mut self) {
        let hotlist = self.config.hotlist.clone();
        if let Err(err) = state::update(move |s| s.hotlist = Some(hotlist)) {
            self.status = Some(format!(" could not save state: {err} "));
        }
    }

    /// OK in the options form: apply every change live and write it
    /// through to the state file right away.
    fn apply_options(&mut self, d: &OptionsDialog) {
        let show_hidden = d.get(Opt::Hidden);
        for i in 0..2 {
            if self.panels[i].show_hidden != show_hidden
                && let Err(err) = self.panels[i].toggle_hidden()
            {
                self.status = Some(format!(" {err} "));
            }
        }
        self.config.show_hidden = show_hidden;
        if self.config.lynx_on() != d.get(Opt::Lynx) {
            self.config.lynx = Some(d.get(Opt::Lynx));
            (self.keymap, _) = full_keymap(&self.config);
        }
        if self.config.mouse != d.get(Opt::Mouse) {
            self.config.mouse = d.get(Opt::Mouse);
            set_mouse_capture(self.config.mouse);
        }
        if self.config.watch != d.get(Opt::Watch) {
            self.config.watch = d.get(Opt::Watch);
            self.watch = if self.config.watch {
                let (watch, warning) = build_watch();
                if let Some(warning) = warning {
                    self.status = Some(format!(" {warning} "));
                }
                watch
            } else {
                None
            };
        }
        if self.config.git != d.get(Opt::Git) {
            self.config.git = d.get(Opt::Git);
            self.git_info = [None, None];
            self.git_refresh();
        }
        self.config.split = if d.get(Opt::HorizontalSplit) {
            "horizontal"
        } else {
            "vertical"
        }
        .to_string();
        self.config.split_ratio = d.ratio;
        self.config.show_menubar = d.get(Opt::MenuBar);
        self.config.show_status = d.get(Opt::StatusLine);
        self.config.show_mini_status = d.get(Opt::MiniStatus);
        self.config.show_cmdline = d.get(Opt::CommandLine);
        self.config.show_keybar = d.get(Opt::KeyBar);
        self.config.confirm_delete = d.get(Opt::ConfirmDelete);
        self.config.confirm_overwrite = d.get(Opt::ConfirmOverwrite);
        self.config.confirm_exit = d.get(Opt::ConfirmExit);
        self.config.confirm_hotlist_delete = d.get(Opt::ConfirmHotlistDelete);
        self.config.confirm_execute = d.get(Opt::ConfirmExecute);
        self.config.subshell = d.get(Opt::Subshell);
        if !self.config.subshell {
            self.subshell = None;
        } else if self.subshell.is_none() {
            let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
            match Subshell::spawn(&self.panels[self.active].local_cwd(), cols, rows) {
                Ok(sub) => self.subshell = Some(sub),
                Err(err) => self.status = Some(format!(" subshell disabled: {err} ")),
            }
        }
        self.config.editor = if d.get(Opt::ExternalEditor) {
            "external"
        } else {
            "internal"
        }
        .to_string();
        let theme = if d.get(Opt::DarkTheme) { "dark" } else { "mc" };
        if self.config.theme != theme {
            self.config.theme = theme.to_string();
            ui::init_theme(theme);
        }
        // Write through immediately - waiting for exit would let any
        // other running instance clobber these on its own exit. Goes to
        // the state file: the user's config.toml is read-only for us.
        let cfg = &self.config;
        let (lynx, mouse, watch, git, subshell) =
            (cfg.lynx, cfg.mouse, cfg.watch, cfg.git, cfg.subshell);
        let (show_hidden, editor, theme) = (cfg.show_hidden, cfg.editor.clone(), cfg.theme.clone());
        let (del, over, exit) = (cfg.confirm_delete, cfg.confirm_overwrite, cfg.confirm_exit);
        let (hot, exec) = (cfg.confirm_hotlist_delete, cfg.confirm_execute);
        let (split, ratio) = (cfg.split.clone(), cfg.split_ratio);
        let mini_status = cfg.show_mini_status;
        let (menubar, status_bar, cmdline, keybar) = (
            cfg.show_menubar,
            cfg.show_status,
            cfg.show_cmdline,
            cfg.show_keybar,
        );
        if let Err(err) = state::update(move |s| {
            s.show_hidden = Some(show_hidden);
            s.lynx = lynx;
            s.mouse = Some(mouse);
            s.watch = Some(watch);
            s.git = Some(git);
            s.subshell = Some(subshell);
            s.editor = Some(editor);
            s.theme = Some(theme);
            s.confirm_delete = Some(del);
            s.confirm_overwrite = Some(over);
            s.confirm_exit = Some(exit);
            s.confirm_hotlist_delete = Some(hot);
            s.confirm_execute = Some(exec);
            s.split = Some(split);
            s.split_ratio = Some(ratio);
            s.show_menubar = Some(menubar);
            s.show_status = Some(status_bar);
            s.show_mini_status = Some(mini_status);
            s.show_cmdline = Some(cmdline);
            s.show_keybar = Some(keybar);
        }) {
            self.status = Some(format!(" could not save state: {err} "));
        }
    }

    fn submit_find(&mut self, dialog: FindDialog) {
        let text = dialog.name.trim();
        let name = rcmd_core::pattern::Pattern {
            text: if text.is_empty() { "*" } else { text }.to_string(),
            shell: dialog.shell,
            case_sensitive: dialog.case_sensitive,
            files_only: false,
        };
        let content = {
            let text = dialog.content.trim();
            (!text.is_empty()).then(|| find::Content {
                text: text.to_string(),
                regex: dialog.regex,
                case_sensitive: dialog.case_sensitive,
                whole_words: dialog.whole_words,
                all_charsets: dialog.all_charsets,
            })
        };
        let label = match &content {
            Some(c) => format!("find: {} ~ \"{}\"", name.text, c.text),
            None => format!("find: {}", name.text),
        };
        let query = find::Query {
            name,
            content,
            skip_hidden: dialog.skip_hidden,
            follow_links: dialog.follow_links,
        };
        let root = match dialog.start.trim() {
            "" => self.panels[self.active].local_cwd(),
            typed => self.resolve(typed),
        };
        if !root.is_dir() {
            self.status = Some(format!(" {} is not a directory ", root.display()));
            self.dialog = Some(Dialog::Find(Box::new(dialog)));
            return;
        }
        let skip = if dialog.skip_ignored {
            git::ignore_filter(&root)
        } else {
            None
        };
        let root_for_window = root.clone();
        // a pattern that will not compile stops here, with the dialog
        // still open on it: the message is about what was typed
        let handle = match find::spawn_find(root, query, skip) {
            Ok(handle) => handle,
            Err(err) => {
                self.status = Some(format!(" {} ", err.lines().next().unwrap_or("bad pattern")));
                self.dialog = Some(Dialog::Find(Box::new(dialog)));
                return;
            }
        };
        let panel_idx = self.active;
        let window = self.config.find_window;
        if window {
            self.dialog = Some(Dialog::FindResults(Box::new(FindResults {
                label: label.clone(),
                root: root_for_window,
                rows: Vec::new(),
                selected: 0,
                top: 0,
                done: None,
                button: 0,
                query: Box::new(dialog),
            })));
        } else {
            self.panels[panel_idx].panelize(Vec::new(), label);
        }
        self.find = Some(FindState {
            handle,
            panel: panel_idx,
            count: 0,
            window,
        });
    }

    /// Ctrl+Space: recursive size of the selected directory, computed in
    /// the background and written into the Size column when done.
    fn dir_size(&mut self) {
        if self.du.is_some() {
            self.status = Some(" a size scan is already running ".into());
            return;
        }
        let panel = &self.panels[self.active];
        let Some(entry) = panel.selected() else {
            return;
        };
        if !entry.is_dir() || entry.is_parent() {
            self.status = Some(" not a directory ".into());
            return;
        }
        let name = entry.name.clone();
        let cwd = panel.cwd.clone();
        let rx = if panel.is_local() {
            fsops::spawn_dir_size(cwd.join(&name))
        } else {
            // sftp and archive panels size through their provider
            fsops::spawn_dir_size_fs(panel.fs.clone(), cwd.join(&name))
        };
        self.du = Some(DuJob {
            rx,
            panel: self.active,
            cwd,
            name,
        });
        self.panel().move_down();
    }

    fn open_find(&mut self) {
        if !self.require_local() {
            return;
        }
        let start = self.panels[self.active].local_cwd().display().to_string();
        self.dialog = Some(Dialog::Find(Box::new(FindDialog {
            start_cursor: start.chars().count(),
            start,
            name: "*".into(),
            name_cursor: 1,
            content: String::new(),
            content_cursor: 0,
            shell: true,
            case_sensitive: false,
            whole_words: false,
            regex: false,
            all_charsets: false,
            skip_hidden: false,
            follow_links: false,
            skip_ignored: true,
            row: 1,
            ok: true,
        })));
    }

    fn open_panelize(&mut self) {
        if !self.require_local() {
            return;
        }
        self.dialog = Some(Dialog::Panelize(Box::new(PanelizeDialog {
            value: String::new(),
            cursor: 0,
            row: 0,
            // the saved list has the focus when there is one to pick
            // from, which is the point of saving them
            on_list: !self.config.panelize.is_empty(),
            naming: None,
        })));
    }

    /// Save the typed command under a name, or drop the highlighted
    /// preset. Both write through to the state file at once, the way
    /// the hotlist does.
    fn save_panelize(
        &mut self,
        preset: Option<crate::config::PanelizePreset>,
        drop_row: Option<usize>,
    ) {
        if let Some(preset) = preset {
            match self
                .config
                .panelize
                .iter()
                .position(|p| p.name == preset.name)
            {
                Some(at) => self.config.panelize[at] = preset,
                None => self.config.panelize.push(preset),
            }
        }
        if let Some(row) = drop_row
            && row < self.config.panelize.len()
        {
            self.config.panelize.remove(row);
        }
        let list = self.config.panelize.clone();
        if let Err(err) = state::update(move |s| s.panelize = Some(list)) {
            self.status = Some(format!(" could not save state: {err} "));
        }
    }

    /// Quick compare of both panel listings: marks files that are missing
    /// on the other side or differ in size/mtime.
    /// F9 > Command > Compare files: the cursor file of each panel,
    /// paired up line by line.
    fn open_diff(&mut self) {
        const MAX_BYTES: u64 = 8 * 1024 * 1024;
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
            if entry.size > MAX_BYTES {
                self.status = Some(format!(
                    " {} is too big to diff (over {} MB) ",
                    panel.name_of(entry),
                    MAX_BYTES / (1024 * 1024)
                ));
                return;
            }
            let path = panel.cwd.join(&entry.name);
            let read = panel.fs.open_read(&path).and_then(|mut reader| {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut reader, &mut bytes)?;
                Ok(bytes)
            });
            match read {
                Ok(bytes) => {
                    let text = rcmd_core::charset::decode(&bytes, panel.charset);
                    let lines: Vec<String> = text.lines().map(str::to_string).collect();
                    sides.push((panel.display_path() + "/" + &panel.name_of(entry), lines));
                }
                Err(err) => {
                    self.status = Some(format!(" compare files: {err} "));
                    return;
                }
            }
        }
        let (right_title, right) = sides.pop().expect("two sides");
        let (left_title, left) = sides.pop().expect("two sides");
        let rows = rcmd_core::diff::rows(&left, &right);
        let blocks = rcmd_core::diff::blocks(&rows);
        let note = match blocks.len() {
            0 => Some(" the files are identical ".into()),
            n => Some(format!(" {n} difference(s) - n and p walk them ")),
        };
        self.open_screen(Screen::Diff(Box::new(DiffView {
            left_title,
            right_title,
            left,
            right,
            rows,
            blocks,
            top: 0,
            col: 0,
            height: 1,
            note,
        })));
        // open on the first difference rather than on whatever the two
        // files happen to agree about at the top
        if let Some(d) = self.diff_mut()
            && let Some((start, _)) = d.blocks.first().copied()
        {
            d.top = start.saturating_sub(2);
        }
    }

    /// One key in the diff view.
    fn on_diff_key(&mut self, key: KeyEvent) {
        let Some(d) = self.diff_mut() else { return };
        d.note = None;
        let page = d.height.saturating_sub(1).max(1) as isize;
        match key.code {
            KeyCode::Esc | KeyCode::F(10) | KeyCode::F(3) | KeyCode::Char('q') => {
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
            KeyCode::Char('n') | KeyCode::Tab => d.jump(true),
            KeyCode::Char('p') | KeyCode::BackTab => d.jump(false),
            _ => {}
        }
    }

    /// Close whatever screen is on top, cleaning up after a viewer.
    fn close_screen(&mut self) {
        if let Some(Screen::Viewer(v)) = self.take_current_screen() {
            for temp in v.temps {
                let _ = std::fs::remove_file(temp);
            }
        }
    }

    /// C-x d: ask how, then compare. mc asks every time, and the
    /// answer matters - "the same size and date" and "the same bytes"
    /// are different questions.
    fn open_compare(&mut self) {
        if self.panels[0].archive.is_some() || self.panels[1].archive.is_some() {
            self.status = Some(" cannot compare inside an archive ".into());
            return;
        }
        self.dialog = Some(Dialog::Compare(0));
    }

    fn compare_dirs(&mut self, mode: rcmd_core::compare::Mode) {
        use rcmd_core::compare;
        if let Some(running) = self.compare.take() {
            running.handle.cancel();
        }
        let diff =
            compare::compare_listings(&self.panels[0].entries, &self.panels[1].entries, mode);
        self.panels[0].marked.clear();
        self.panels[1].marked.clear();
        for name in &diff.left {
            self.panels[0].marked.insert(name.clone());
        }
        for name in &diff.right {
            self.panels[1].marked.insert(name.clone());
        }
        let known = diff.count();
        if diff.undecided.is_empty() {
            self.status = Some(format!(" {known} difference(s) marked "));
            return;
        }
        // the pairs the listing could not settle are read on a worker
        // thread, and mark themselves as they are found to differ
        let total = diff.undecided.len();
        let handle = compare::spawn_content_compare(
            (self.panels[0].fs.clone(), self.panels[0].cwd.clone()),
            (self.panels[1].fs.clone(), self.panels[1].cwd.clone()),
            diff.undecided,
        );
        self.status = Some(format!(" comparing {total} pair(s)… Esc cancels "));
        self.compare = Some(CompareState {
            handle,
            total,
            done: 0,
        });
    }

    /// Matches from a thorough compare, as they arrive.
    fn drain_compare(&mut self) {
        let Some(state) = self.compare.as_mut() else {
            return;
        };
        let mut finished = false;
        let mut differing = Vec::new();
        while let Ok(event) = state.handle.events.try_recv() {
            match event {
                rcmd_core::compare::CompareEvent::Differs(name) => {
                    state.done += 1;
                    differing.push(name);
                }
                rcmd_core::compare::CompareEvent::Done => finished = true,
            }
        }
        for name in differing {
            self.panels[0].marked.insert(name.clone());
            self.panels[1].marked.insert(name);
            self.dirty = true;
        }
        let (total, done) = self
            .compare
            .as_ref()
            .map(|c| (c.total, c.done))
            .unwrap_or((0, 0));
        if finished {
            self.compare = None;
            let marked = self.panels[0].marked.len().max(self.panels[1].marked.len());
            self.status = Some(format!(" {marked} difference(s) marked ({total} read) "));
            self.dirty = true;
        } else {
            self.status = Some(format!(" comparing… {done} differ so far - Esc cancels "));
        }
    }

    fn submit_input(&mut self, dialog: InputDialog) {
        let value = dialog.value.trim().to_string();
        if value.is_empty() {
            return;
        }
        match dialog.action {
            InputAction::CopyTo { sources } => {
                self.route_transfer(sources, &value, false, TransferOpts::default(), None)
            }
            InputAction::MoveTo { sources } => {
                self.route_transfer(sources, &value, true, TransferOpts::default(), None)
            }
            InputAction::Mkdir => {
                if self.panels[self.active].is_remote() {
                    self.remote_mkdir(&value);
                    return;
                }
                if let Some(archive) = self.panels[self.active].archive.clone() {
                    let dir = self.panels[self.active].cwd.join(value.trim_matches('/'));
                    self.start_archive_edit(archive, vec![fsops::ArchiveOp::Mkdir(dir)], "create");
                    return;
                }
                let path = self.resolve(&value);
                match std::fs::create_dir_all(&path) {
                    Ok(()) => {
                        for panel in &mut self.panels {
                            let _ = panel.reload();
                        }
                        let panel = &mut self.panels[self.active];
                        if path.parent() == Some(panel.cwd.as_path())
                            && let Some(name) = path.file_name()
                            && let Some(pos) = panel.entries.iter().position(|e| e.name == name)
                        {
                            panel.cursor = pos;
                        }
                    }
                    Err(err) => self.status = Some(format!(" mkdir: {err} ")),
                }
            }

            InputAction::SftpConnect => self.connect_remote(&value),
            InputAction::EditNew => {
                let name = value.trim();
                if !name.is_empty() {
                    self.edit_new(self.resolve(name));
                }
            }
            InputAction::QuickCd => {
                if !value.trim().is_empty() {
                    self.do_cd(value.trim());
                }
            }
            InputAction::Chown { paths } => {
                let remote = self.panels[self.active].is_remote();
                match parse_owner_spec(value.trim(), remote) {
                    Err(err) => self.status = Some(format!(" chown: {err} ")),
                    Ok((None, None)) => self.status = Some(" chown: nothing to change ".into()),
                    Ok((uid, gid)) => {
                        self.apply_fs_op(&paths, "chown", |w, p| w.set_owner(p, uid, gid));
                    }
                }
            }
        }
    }

    /// Send F5/F6 to the right job for the source panel and the typed
    /// destination: plain copy/move, archive pack/extract, or a
    /// cross-provider transfer when SFTP is on either side.
    /// A button on the chmod matrix. MC's three ways to spend the bits:
    /// set them exactly, add them, or take them away - the last two
    /// leave every other bit of each file alone, which is the whole
    /// point of chmod'ing a group of files at once.
    fn submit_chmod(&mut self, d: ChmodDialog) {
        let Some(action) = CHMOD_BUTTONS.get(d.button) else {
            return;
        };
        if *action == "Cancel" {
            return;
        }
        let mode = d.mode;
        // each entry's current mode, for the add/remove variants
        let current: std::collections::HashMap<PathBuf, u32> = self.panels[self.active]
            .entries
            .iter()
            .map(|e| (self.panels[self.active].cwd.join(&e.name), e.mode & 0o7777))
            .collect();
        let apply = |path: &Path| -> u32 {
            let was = current.get(path).copied().unwrap_or(0);
            match *action {
                "Set marked" => was | mode,
                "Clear marked" => was & !mode,
                _ => mode,
            }
        };
        let paths = d.paths.clone();
        if d.recurse {
            // one mode for the whole tree: "add" and "remove" are per
            // file, and a tree has no single mode to add them to
            self.start_attrs_job(
                paths,
                fsops::Attrs {
                    mode: Some(mode),
                    ..Default::default()
                },
                "chmod",
            );
            return;
        }
        self.apply_fs_op(&paths, "chmod", |w, p| w.set_mode(p, apply(p)));
    }

    /// A recursive chmod/chown, as a job with a progress dialog and a
    /// Cancel button - which is what you want halfway down a big tree.
    fn start_attrs_job(&mut self, paths: Vec<PathBuf>, attrs: fsops::Attrs, verb: &str) {
        self.jobs.push(Job {
            title: format!(" {verb} {} item(s), recursively ", paths.len()),
            handle: fsops::spawn_attrs(paths, attrs, true),
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
        });
    }

    /// OK or Background on the copy/move form. Cancel never gets here.
    fn submit_transfer(&mut self, d: TransferDialog) {
        if d.button == 2 {
            return; // Cancel
        }
        let background = d.button == 1;
        // MC puts the target mask in the destination's last component;
        // anything without a wildcard there is a plain destination
        let (dest, target) = match d.dest.rsplit_once('/') {
            Some((dir, last)) if mask::is_target_mask(last) => {
                (format!("{dir}/"), Some(last.to_string()))
            }
            _ if mask::is_target_mask(&d.dest) => (String::new(), Some(d.dest.clone())),
            _ => (d.dest.clone(), None),
        };
        let rename = Rename::new(Mask::new(&d.mask), target);
        self.route_transfer(d.sources, &dest, d.is_move, d.opts, rename);
        // every route ends in a pushed job, or in a status message and
        // no job at all - marking the last one is right either way
        if background && let Some(job) = self.jobs.last_mut() {
            job.background = true;
        }
    }

    /// `opts` is what the copy/move form asked for; the paths that do
    /// not go through it (S-F5, drops into an archive, VFS transfers)
    /// use the defaults, which is what they did before there was a form.
    fn route_transfer(
        &mut self,
        sources: Vec<PathBuf>,
        value: &str,
        is_move: bool,
        opts: TransferOpts,
        rename: Option<Rename>,
    ) {
        let src_panel = &self.panels[self.active];
        let src_archive = src_panel.archive.is_some();
        // masks rename local files; the archive and SFTP routes below
        // build their own targets and would drop one on the floor
        if rename.is_some()
            && (is_remote_url(value)
                || src_panel.is_remote()
                || src_archive
                || split_vfs_dest(value).is_some())
        {
            self.status = Some(" source masks work on local copies ".into());
            return;
        }
        // a remote destination (must match before the zip:// syntax -
        // a URL also contains "://")
        if is_remote_url(value) {
            let parsed = if value.starts_with("ftp://") {
                FtpUrl::parse(value).map(|url| (url.prefix(), url.display(), url.path))
            } else {
                let scheme = if value.starts_with("fish://") {
                    "fish"
                } else {
                    "sftp"
                };
                SftpUrl::parse_as(scheme, value).map(|url| (url.prefix(), url.display(), url.path))
            };
            let Some((prefix, label, path)) = parsed else {
                self.status = Some(" bad URL - scheme://[user@]host[:port]/path ".into());
                return;
            };
            if path.as_os_str().is_empty() {
                self.status = Some(" destination URL needs a path ".into());
                return;
            }
            if is_move && src_archive {
                self.status = Some(" moving out of an archive is a copy - use F5 ".into());
                return;
            }
            let Some(dst_fs) = self.connection(&prefix) else {
                self.status = Some(format!(" not connected - cd {prefix} first "));
                return;
            };
            let src_fs = self.panels[self.active].fs.clone();
            self.start_vfs_transfer(src_fs, sources, dst_fs, path, is_move, label);
            return;
        }
        if src_panel.is_remote() {
            if split_vfs_dest(value).is_some() {
                self.status = Some(" cannot copy from remote into an archive ".into());
                return;
            }
            let dest = self.resolve(value);
            let src_fs = self.panels[self.active].fs.clone();
            let label = dest.display().to_string();
            self.start_vfs_transfer(src_fs, sources, Arc::new(LocalFs), dest, is_move, label);
            return;
        }
        // local or archive source, local or zip:// destination
        if is_move {
            if src_archive {
                // a relative destination stays inside the archive: that
                // is a rename, which a rewrite can do. An absolute one
                // means leaving the archive, which is a copy followed by
                // a delete and is better asked for as those two things.
                if value.is_empty() || Path::new(value).is_absolute() || value.contains("://") {
                    self.status = Some(" moving out of an archive is a copy - use F5 ".into());
                } else if let Some(archive) = self.editable_archive() {
                    let inside = self.panels[self.active].cwd.join(value.trim_matches('/'));
                    let ops = self.archive_rename_ops(&sources, &inside);
                    self.start_archive_edit(archive, ops, "move");
                }
            } else if split_vfs_dest(value).is_some() {
                self.status = Some(" cannot move into an archive ".into());
            } else {
                self.start_transfer(sources, value, fsops::spawn_move, "move", opts, rename);
            }
            return;
        }
        match (split_vfs_dest(value), !src_archive) {
            (Some(_), false) => self.status = Some(" cannot copy from archive to archive ".into()),
            (Some((archive, inside)), true) => self.start_pack(sources, archive, inside),
            (None, true) => {
                self.start_transfer(sources, value, fsops::spawn_copy, "copy", opts, rename)
            }
            (None, false) => self.start_extract(sources, value),
        }
    }

    fn start_vfs_transfer(
        &mut self,
        src_fs: Arc<dyn FsProvider>,
        sources: Vec<PathBuf>,
        dst_fs: Arc<dyn FsProvider>,
        dest: PathBuf,
        is_move: bool,
        dest_label: String,
    ) {
        let verb = if is_move { "move" } else { "copy" };
        self.jobs.push(Job {
            title: format!(" {verb} {} item(s) to {} ", sources.len(), dest_label),
            handle: fsops::spawn_transfer(src_fs, sources, dst_fs, dest, is_move),
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
        });
    }

    fn remote_mkdir(&mut self, value: &str) {
        let panel = &mut self.panels[self.active];
        let path = {
            let raw = Path::new(value);
            if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                panel.cwd.join(raw)
            }
        };
        let made = panel
            .fs
            .writer()
            .ok_or_else(|| std::io::Error::other("read-only filesystem"))
            .and_then(|w| w.mkdir(&normalize(&path)));
        match made {
            Ok(()) => self.fallible(|p| p.reload().map(|()| true)),
            Err(err) => self.status = Some(format!(" mkdir: {err} ")),
        }
    }

    /// Run a command, its stdout lines become the panel listing.
    /// Synchronous: meant for fast listers (git ls-files, rg -l, …).
    /// Run the command with its output streaming into the panel. A
    /// listing that takes a while to produce - a find, a git command
    /// over a big tree - fills in as it goes rather than after.
    fn run_panelize(&mut self, command: &str) {
        use std::io::{BufRead, BufReader};
        use std::sync::atomic::{AtomicBool, Ordering};
        if let Some(running) = self.panelize.take() {
            running.cancel.store(true, Ordering::Relaxed);
        }
        let cwd = self.panels[self.active].local_cwd();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let child = std::process::Command::new(&shell)
            .arg("-c")
            .arg(command)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(err) => {
                self.status = Some(format!(" panelize: {err} "));
                return;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        std::thread::spawn(move || {
            let stdout = child.stdout.take();
            if let Some(stdout) = stdout {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if flag.load(Ordering::Relaxed) {
                        let _ = child.kill();
                        break;
                    }
                    if !line.trim().is_empty() && tx.send(PanelizeEvent::Line(line)).is_err() {
                        let _ = child.kill();
                        return;
                    }
                }
            }
            // the error is worth having only when nothing came out
            let status = child.wait();
            let complaint = match status {
                Ok(status) if !status.success() => {
                    let mut text = String::new();
                    if let Some(mut err) = child.stderr.take() {
                        use std::io::Read as _;
                        let _ = err.read_to_string(&mut text);
                    }
                    Some(
                        text.lines()
                            .next()
                            .unwrap_or("command failed")
                            .trim()
                            .to_string(),
                    )
                }
                _ => None,
            };
            let _ = tx.send(PanelizeEvent::Done(complaint));
        });
        let panel = self.active;
        self.panels[panel].panelize(Vec::new(), format!("cmd: {command}"));
        self.panelize = Some(PanelizeJob {
            rx,
            panel,
            count: 0,
            cancel,
        });
    }

    /// Lines from a running panelize, as they arrive.
    fn drain_panelize(&mut self) {
        let Some(job) = self.panelize.as_mut() else {
            return;
        };
        let mut done = None;
        let mut lines = Vec::new();
        while let Ok(event) = job.rx.try_recv() {
            match event {
                PanelizeEvent::Line(line) => lines.push(line),
                PanelizeEvent::Done(complaint) => done = Some(complaint),
            }
        }
        let panel = job.panel;
        let cwd = self.panels[panel].local_cwd();
        for line in lines {
            let line = line.trim().to_string();
            if let Ok(mut entry) = entry::stat(&cwd.join(&line)) {
                entry.name = std::ffi::OsString::from(line);
                self.panels[panel].entries.push(entry);
                if let Some(job) = self.panelize.as_mut() {
                    job.count += 1;
                }
                self.dirty = true;
            }
        }
        let count = self.panelize.as_ref().map(|j| j.count).unwrap_or(0);
        match done {
            Some(complaint) => {
                self.panelize = None;
                self.dirty = true;
                self.status = Some(match complaint {
                    Some(err) if count == 0 => format!(" panelize: {err} "),
                    _ => format!(" panelized {count} item(s) "),
                });
            }
            None => self.status = Some(format!(" panelizing… {count} so far - Esc cancels ")),
        }
    }

    fn open_filter(&mut self) {
        // the filter in force, so the dialog opens on what is hiding
        // things rather than on a blank
        let current = self.panels[self.active].filter.clone().unwrap_or_default();
        self.dialog = Some(Dialog::Pattern(Box::new(PatternDialog {
            title: " Filter (show files matching) ".into(),
            cursor: current.text.chars().count(),
            value: current.text,
            shell: current.shell,
            case_sensitive: current.case_sensitive,
            files_only: current.files_only,
            row: 0,
            ok: true,
            kind: PatternKind::Filter,
        })));
    }

    /// OK on that form: mark, unmark, or set the panel's filter.
    fn submit_pattern(&mut self, d: &PatternDialog) {
        let pattern = d.to_pattern();
        if let Err(err) = pattern.compile() {
            // the regular expression is the user's, so it is quoted
            // back rather than swallowed
            self.status = Some(format!(" {} ", err.lines().next().unwrap_or("bad pattern")));
            return;
        }
        match d.kind {
            PatternKind::Select { mark } => {
                match self.panels[self.active].mark_pattern(&pattern, mark) {
                    Ok(moved) => {
                        let verb = if mark { "selected" } else { "unselected" };
                        self.status = Some(format!(" {moved} {verb} "));
                    }
                    Err(err) => self.status = Some(format!(" {err} ")),
                }
            }
            PatternKind::Filter => {
                let panel = &mut self.panels[self.active];
                panel.filter = (!pattern.is_open()).then_some(pattern);
                self.fallible(|p| p.reload().map(|()| true));
            }
        }
    }

    fn start_transfer(
        &mut self,
        sources: Vec<PathBuf>,
        dest: &str,
        spawn: fn(Vec<PathBuf>, PathBuf, TransferOpts, Option<Rename>) -> JobHandle,
        verb: &str,
        opts: TransferOpts,
        rename: Option<Rename>,
    ) {
        let dest = self.resolve(dest);
        self.jobs.push(Job {
            title: format!(" {verb} {} item(s) to {} ", sources.len(), dest.display()),
            handle: spawn(sources, dest, opts, rename),
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
        });
    }

    /// Copy INTO an archive: zip appends in place, tar (plain or
    /// compressed) goes through a full rewrite-append.
    fn start_pack(&mut self, sources: Vec<PathBuf>, archive: PathBuf, inside: PathBuf) {
        let name = archive
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        let is_tar = [
            ".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.bz2", ".tbz2", ".tbz",
        ]
        .iter()
        .any(|ext| name.ends_with(ext));
        let handle = if name.ends_with(".zip") {
            fsops::spawn_pack_zip(sources.clone(), archive.clone(), inside)
        } else if is_tar {
            fsops::spawn_pack_tar(sources.clone(), archive.clone(), inside)
        } else {
            self.status = Some(" can only copy into .zip or .tar[.gz/xz/bz2] archives ".into());
            return;
        };
        self.jobs.push(Job {
            title: format!(
                " pack {} item(s) into {} ",
                sources.len(),
                archive.display()
            ),
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
        });
    }

    fn start_extract(&mut self, sources: Vec<PathBuf>, dest: &str) {
        let dest = self.resolve(dest);
        let fs = self.panels[self.active].fs.clone();
        self.jobs.push(Job {
            title: format!(" extract {} item(s) to {} ", sources.len(), dest.display()),
            handle: fsops::spawn_extract(fs, sources, dest),
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
        });
    }

    /// Where each source lands under `inside`. One source renamed onto a
    /// name that is not an existing directory is a plain rename - which
    /// is what F6 on a single entry means - and everything else moves
    /// into the destination directory under its own name.
    fn archive_rename_ops(&self, sources: &[PathBuf], inside: &Path) -> Vec<fsops::ArchiveOp> {
        let panel = &self.panels[self.active];
        let into_dir = sources.len() > 1
            || panel
                .fs
                .stat(inside)
                .map(|entry| entry.is_dir())
                .unwrap_or(false);
        sources
            .iter()
            .map(|from| fsops::ArchiveOp::Rename {
                from: from.clone(),
                to: if into_dir {
                    inside.join(from.file_name().unwrap_or_default())
                } else {
                    inside.to_path_buf()
                },
            })
            .collect()
    }

    /// The archive the active panel is inside, if that container is one
    /// rcmd can rewrite. zip and tar can be; a deb, an rpm, an iso or a
    /// cpio cannot - not because the code is missing but because
    /// rewriting a package or a disc image is not what a panel is for.
    fn editable_archive(&mut self) -> Option<PathBuf> {
        let archive = self.panels[self.active].archive.clone()?;
        let name = archive
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        if name.ends_with(".zip") || fsops::is_tar_name(&name) {
            return Some(archive);
        }
        self.status = Some(" only .zip and .tar archives can be changed ".into());
        None
    }

    /// Run one batch of changes against an archive. Every op goes in one
    /// job because the container is rewritten once, however many members
    /// the batch touches.
    fn start_archive_edit(&mut self, archive: PathBuf, ops: Vec<fsops::ArchiveOp>, verb: &str) {
        let count = ops.len();
        let handle = fsops::spawn_archive_edit(archive, ops);
        self.jobs.push(Job {
            title: format!(" {verb} {count} item(s) in the archive "),
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
        });
    }

    fn start_delete(&mut self, paths: Vec<PathBuf>, permanent: bool) {
        if let Some(archive) = self.panels[self.active].archive.clone() {
            let ops = paths.into_iter().map(fsops::ArchiveOp::Remove).collect();
            self.start_archive_edit(archive, ops, "delete");
            return;
        }
        let verb = if permanent { "delete" } else { "trash" };
        let count = paths.len();
        let handle = if self.panels[self.active].is_remote() {
            fsops::spawn_delete_fs(self.panels[self.active].fs.clone(), paths)
        } else {
            fsops::spawn_delete(paths, permanent)
        };
        self.jobs.push(Job {
            title: format!(" {verb} {count} item(s) "),
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
        });
    }

    /// Resolve user input to a normalized path: `~` expands to $HOME,
    /// relative paths are anchored at the active panel's directory.
    fn resolve(&self, input: &str) -> PathBuf {
        // a name typed on a panel is spelled in that panel's codepage:
        // the bytes it makes are the bytes the names already there
        // have, so what is created is what the panel then shows
        let typed = |text: &str| PathBuf::from(self.panels[self.active].name_bytes(text));
        let raw = if input == "~" {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
        } else if let Some(rest) = input.strip_prefix("~/") {
            match std::env::var_os("HOME") {
                Some(home) => PathBuf::from(home).join(typed(rest)),
                None => typed(input),
            }
        } else {
            let path = typed(input);
            if path.is_absolute() {
                path
            } else {
                self.panels[self.active].local_cwd().join(path)
            }
        };
        normalize(&raw)
    }

    fn on_panel_key(&mut self, key: KeyEvent) {
        // a thorough compare reads files: Esc stops it, as it stops a
        // find, rather than waiting for the last pair
        if key.code == KeyCode::Esc {
            if let Some(running) = self.compare.take() {
                running.handle.cancel();
                self.status = Some(" compare cancelled ".into());
                return;
            }
            if let Some(running) = self.panelize.take() {
                running
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.status = Some(" panelize cancelled ".into());
                return;
            }
        }
        let mods = key.modifiers;
        let alt = mods.contains(KeyModifiers::ALT);
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let page = self.panel_rows.saturating_sub(1).max(1);
        let cmd_empty = self.cmdline.value.is_empty();
        // Ctrl+X chord: the next key selects the command.
        if self.prefix_cx {
            self.prefix_cx = false;
            match key.code {
                KeyCode::Char('d' | 'D') => self.run_action(Action::CompareDirs),
                KeyCode::Char('q' | 'Q') => self.run_action(Action::QuickView),
                KeyCode::Char('i' | 'I') => self.run_action(Action::InfoView),
                KeyCode::Char('t' | 'T') => self.run_action(Action::PasteTags),
                KeyCode::Char('p' | 'P') => self.run_action(Action::PastePath),
                KeyCode::Char('c' | 'C') => self.open_chmod(),
                KeyCode::Char('o' | 'O') => self.open_chown(),
                // MC's four: l hard, s absolute, v relative, C-s edit
                KeyCode::Char('s' | 'S') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.open_edit_symlink()
                }
                KeyCode::Char('s' | 'S') => self.open_link(LinkKind::Symbolic, false),
                KeyCode::Char('v' | 'V') => self.open_link(LinkKind::Symbolic, true),
                KeyCode::Char('l' | 'L') => self.open_link(LinkKind::Hard, false),
                KeyCode::Char('j' | 'J') => self.run_action(Action::Jobs),
                KeyCode::Char('a' | 'A') => self.run_action(Action::VfsList),
                KeyCode::Char('!') => self.run_action(Action::Panelize),
                _ => {}
            }
            return;
        }
        if ctrl && key.code == KeyCode::Char('x') {
            self.prefix_cx = true;
            self.status = Some(
                " C-x  (d = compare, q = quick view, i = info, c = chmod, \
                 o = chown, s = symlink, j = jobs, a = active VFS, \
                 ! = panelize, t/p = paste tags/path) "
                    .into(),
            );
            return;
        }
        // Focused preview pane: a reduced key set (scrolling, Tab back,
        // quit); everything else is ignored rather than acting on the
        // hidden listing underneath.
        if let Some(qv) = self.quick_view.as_mut()
            && qv.side == self.active
        {
            let rows = qv.rows.max(1);
            let page = rows.saturating_sub(1).max(1);
            match key.code {
                KeyCode::Tab | KeyCode::BackTab => self.active ^= 1,
                KeyCode::F(4) => {
                    qv.hex = !qv.hex;
                    qv.top = 0;
                }
                KeyCode::Up => qv.top = qv.top.saturating_sub(1),
                KeyCode::PageUp => qv.top = qv.top.saturating_sub(page),
                KeyCode::Home => qv.top = 0,
                KeyCode::Down | KeyCode::PageDown | KeyCode::End => {
                    if let Some((_, fv)) = qv.view.as_mut() {
                        let want = match key.code {
                            KeyCode::Down => qv.top + 1,
                            KeyCode::PageDown => qv.top + page,
                            _ => usize::MAX,
                        };
                        let known = if qv.hex {
                            fv.size.div_ceil(16) as usize
                        } else if want == usize::MAX {
                            fv.total_lines().unwrap_or(0)
                        } else {
                            let _ = fv.ensure_lines(want + 1);
                            fv.known_lines()
                        };
                        let max_top =
                            known.saturating_sub(if want == usize::MAX { rows } else { 1 });
                        qv.top = want.min(max_top);
                    }
                }
                KeyCode::F(10) => self.quit = true,
                _ => {}
            }
            return;
        }
        // A panel in tree mode: the figure has replaced the listing, so
        // it takes the movement keys and Enter. Everything else - Tab,
        // the command line, the F-keys - still belongs to the panel.
        if self.panels[self.active].list_mode == ListMode::Tree {
            let plain = !alt && !ctrl;
            let mut handled = true;
            if let Some(tree) = self.trees[self.active].as_mut() {
                match key.code {
                    KeyCode::Up if plain => tree.up(),
                    KeyCode::Down if plain => tree.down(),
                    KeyCode::PageUp if plain => tree.page_up(page),
                    KeyCode::PageDown if plain => tree.page_down(page),
                    KeyCode::Home if plain => tree.first(),
                    KeyCode::End if plain => tree.last(),
                    KeyCode::Left if plain => tree.left(),
                    KeyCode::Right if plain => tree.right(),
                    // mc's tree keys: F4 switches navigation mode, C-r
                    // (rcmd's reload) rescans the selected branch
                    KeyCode::F(4) => tree.toggle_mode(),
                    KeyCode::Char('r') if ctrl => tree.rescan(),
                    _ => handled = false,
                }
            } else {
                handled = false;
            }
            // Enter needs the whole App: it moves the *other* panel
            if !handled && key.code == KeyCode::Enter && cmd_empty {
                self.tree_enter();
                handled = true;
            }
            if handled {
                return;
            }
        }
        // Focused info pane: nothing to scroll - Tab back or quit.
        if self.info == Some(self.active) {
            match key.code {
                KeyCode::Tab | KeyCode::BackTab => self.active ^= 1,
                KeyCode::F(10) => self.quit = true,
                _ => {}
            }
            return;
        }
        // Structural keys: navigation and command-line plumbing.
        match key.code {
            // Tab completes a path once the command line has text (or as
            // MC's M-Tab always); on an empty line it switches panels.
            KeyCode::Tab if alt || !cmd_empty => {
                self.complete_cmdline();
                return;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.active ^= 1;
                return;
            }
            KeyCode::Up if !alt => {
                self.panel().move_up();
                return;
            }
            KeyCode::Down if !alt => {
                self.panel().move_down();
                return;
            }
            KeyCode::PageUp => {
                self.panel().page_up(page);
                return;
            }
            KeyCode::PageDown => {
                self.panel().page_down(page);
                return;
            }
            KeyCode::Home if cmd_empty => {
                self.panel().move_top();
                return;
            }
            KeyCode::End if cmd_empty => {
                self.panel().move_bottom();
                return;
            }
            KeyCode::Enter if alt => {
                self.insert_selected_name();
                return;
            }
            KeyCode::Enter if cmd_empty => {
                self.enter_or_open();
                return;
            }
            KeyCode::Enter => {
                self.submit_command();
                return;
            }
            KeyCode::Esc if self.panels[self.active].is_loading() => {
                self.panels[self.active].cancel_pending();
                self.status = Some(" load cancelled ".into());
                return;
            }
            KeyCode::Esc if !cmd_empty => {
                self.cmdline.value.clear();
                self.cmdline.cursor = 0;
                self.cmdline.hist_pos = None;
                return;
            }
            KeyCode::Backspace if cmd_empty => {
                self.fallible(|p| p.go_up());
                return;
            }
            // C-p/C-n are rcmd's; M-p/M-n are the same keys in MC
            KeyCode::Char('p') if ctrl || alt => {
                self.cmdline.hist_prev();
                return;
            }
            KeyCode::Char('n') if ctrl || alt => {
                self.cmdline.hist_next();
                return;
            }
            _ => {}
        }

        // Action keys via the (config-driven) keymap. Plain characters and
        // Left/Right only qualify while the command line is empty - with
        // text present they belong to line editing.
        let eligible = match key.code {
            KeyCode::Char(_) if !ctrl && !alt => cmd_empty,
            KeyCode::Left | KeyCode::Right => cmd_empty || alt,
            _ => true,
        };
        if eligible {
            let lookup_mods = match key.code {
                KeyCode::Char(_) => mods.difference(KeyModifiers::SHIFT),
                _ => mods,
            };
            if let Some(action) = self.keymap.get(&(key.code, lookup_mods)).copied() {
                self.run_action(action);
                return;
            }
        }

        // with the command line hidden there is nowhere to type: plain
        // characters only reach bindings (MC's "command prompt" off)
        if self.config.show_cmdline
            && edit_line(
                &mut self.cmdline.value,
                &mut self.cmdline.cursor,
                key.code,
                mods,
            )
        {
            self.cmdline.hist_pos = None;
        }
    }

    fn submit_command(&mut self) {
        let cmd = self.cmdline.take();
        if cmd.is_empty() {
            return;
        }
        self.cmdline.push_history(&cmd);
        if let Some(dir) = parse_cd(&cmd) {
            // no macro expansion here: the expansion shell-quotes, and a
            // quoted path is not what `cd` wants
            self.do_cd(dir);
        } else {
            // MC expands its macros on the command line too; unknown
            // percent sequences (printf "%s") are left alone
            self.pending_exec = Some(Exec::Command(self.expand_macros(&cmd)));
        }
    }

    /// `cd <dir>` - from the command line or the M-c quick-cd dialog.
    fn do_cd(&mut self, dir: &str) {
        if is_remote_url(dir) {
            self.connect_remote(dir);
            return;
        }
        // `cd -`: back to where this panel came from, shell-style
        if dir == "-" {
            let previous = self.panels[self.active]
                .previous_location()
                .map(str::to_string);
            match previous {
                Some(loc) => self.navigate(&loc),
                None => self.status = Some(" cd: no previous directory ".into()),
            }
            return;
        }
        let panel = &mut self.panels[self.active];
        if panel.is_remote() {
            // relative/absolute stays on the server; bare `cd` or a
            // `~` path returns to the local filesystem
            if !dir.is_empty() && !dir.starts_with('~') {
                let raw = if Path::new(dir).is_absolute() {
                    PathBuf::from(dir)
                } else {
                    panel.cwd.join(dir)
                };
                if let Err(err) = panel.cd(normalize(&raw)) {
                    self.status = Some(format!(" cd: {err} "));
                }
                return;
            }
            let target = if dir.is_empty() {
                home_dir()
            } else {
                self.resolve(dir)
            };
            if let Err(err) = self.panels[self.active].to_local(target) {
                self.status = Some(format!(" cd: {err} "));
            }
            return;
        }
        let target = if dir.is_empty() {
            home_dir()
        } else {
            self.resolve_cd(dir)
        };
        if let Err(err) = self.panels[self.active].cd(target) {
            self.status = Some(format!(" cd: {err} "));
        }
    }

    /// [`Self::resolve`] plus `$CDPATH`: a relative target that does not
    /// exist under the panel directory is looked up in each CDPATH entry
    /// in turn, like the shell builtin. Absolute and `~` paths, and any
    /// target that does exist here, never consult it.
    fn resolve_cd(&self, input: &str) -> PathBuf {
        let here = self.resolve(input);
        if here.exists()
            || input.starts_with('/')
            || input.starts_with('~')
            || input.starts_with('.')
        {
            return here;
        }
        let Some(cdpath) = std::env::var_os("CDPATH") else {
            return here;
        };
        for dir in std::env::split_paths(&cdpath) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = normalize(&dir.join(input));
            if candidate.is_dir() {
                return candidate;
            }
        }
        here
    }

    /// Tab: complete the path under the cursor (files/dirs only).
    fn complete_cmdline(&mut self) {
        use rcmd_core::complete::{complete_word, word_start};
        let cur = byte_index(&self.cmdline.value, self.cmdline.cursor);
        let head = &self.cmdline.value[..cur];
        let start = word_start(head);
        let cwd = self.panels[self.active].local_cwd();
        match complete_word(&cwd, &head[start..cur]) {
            None => self.status = Some(" no match ".into()),
            Some(done) => {
                let word_chars = self.cmdline.value[start..cur].chars().count();
                self.cmdline.value.replace_range(start..cur, &done.word);
                self.cmdline.cursor = self.cmdline.cursor - word_chars + done.word.chars().count();
                self.cmdline.hist_pos = None;
                if done.matches.len() > 1 {
                    let mut list = done.matches.join("  ");
                    if list.chars().count() > 76 {
                        list = format!("{}…", list.chars().take(75).collect::<String>());
                    }
                    self.status = Some(format!(" {list} "));
                }
            }
        }
    }

    /// Alt+Enter: append the cursor entry's (shell-quoted) name.
    fn insert_selected_name(&mut self) {
        let Some(entry) = self.panels[self.active].selected() else {
            return;
        };
        let text = format!("{} ", shell_quote(&entry.name.to_string_lossy()));
        self.insert_cmdline(&text);
    }

    /// C-x t: append every tagged name (or the cursor entry),
    /// shell-quoted, to the command line.
    fn insert_tagged_names(&mut self) {
        let text: String = self.panels[self.active]
            .target_names()
            .iter()
            .map(|n| format!("{} ", shell_quote(&n.to_string_lossy())))
            .collect();
        self.insert_cmdline(&text);
    }

    fn insert_cmdline(&mut self, text: &str) {
        let idx = byte_index(&self.cmdline.value, self.cmdline.cursor);
        self.cmdline.value.insert_str(idx, text);
        self.cmdline.cursor += text.chars().count();
        self.cmdline.hist_pos = None;
    }

    fn describe(&self, paths: &[PathBuf]) -> String {
        if paths.len() == 1 {
            // named in the panel's codepage, so the question is about
            // the file the panel is showing rather than about mojibake
            format!(
                "\"{}\"",
                rcmd_core::charset::decode_name(
                    paths[0].file_name().unwrap_or_default(),
                    self.panels[self.active].charset,
                )
            )
        } else {
            format!("{} items", paths.len())
        }
    }

    /// Operations that only make sense on a local directory.
    fn require_local(&mut self) -> bool {
        let panel = &self.panels[self.active];
        if panel.is_local() {
            true
        } else {
            self.status = Some(if panel.is_remote() {
                " not available on a remote panel ".into()
            } else {
                " archive is read-only ".into()
            });
            false
        }
    }

    fn open_transfer(&mut self, is_move: bool) {
        let in_archive = self.panels[self.active].archive.is_some();
        if is_move && in_archive && self.editable_archive().is_none() {
            return;
        }
        let sources = self.panels[self.active].targets();
        if sources.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let verb = if is_move { "Move" } else { "Copy" };
        let other = &self.panels[self.active ^ 1];
        // a remote or archive panel on the other side prefills its
        // virtual path - accepting it uploads / packs into the zip
        // moving inside an archive is a rename, so the bare name is the
        // useful default; an absolute path there would mean leaving the
        // archive, which F5 does and F6 does not
        let mut dest = if is_move && in_archive {
            sources
                .first()
                .and_then(|src| src.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else if other.is_remote() {
            other.display_path()
        } else if other.is_local() || is_move {
            other.local_cwd().display().to_string()
        } else {
            other.display_path()
        };
        if !(is_move && in_archive) && !dest.ends_with('/') {
            dest.push('/');
        }
        let title = format!(" {verb} {} to: ", self.describe(&sources));
        self.dialog = Some(Dialog::Transfer(Box::new(TransferDialog {
            title,
            mask: "*".into(),
            mask_cursor: 1,
            cursor: dest.chars().count(),
            dest,
            is_move,
            sources,
            opts: TransferOpts::default(),
            // the destination is what people type; the mask sits above
            // it for the rare copy that needs one, an Up away
            row: TRANSFER_DEST_ROW,
            button: 0,
        })));
    }

    /// S-F5 / S-F6: copy or rename the cursor file without leaving the
    /// directory - the dialog prefills the bare name for editing.
    fn open_transfer_here(&mut self, is_move: bool) {
        let in_archive = self.panels[self.active].archive.is_some();
        if in_archive {
            if self.editable_archive().is_none() {
                return;
            }
        } else if !self.require_local() {
            return;
        }
        let panel = &self.panels[self.active];
        let Some(entry) = panel.selected().filter(|e| !e.is_parent()) else {
            self.status = Some(" nothing selected ".into());
            return;
        };
        let name = panel.name_of(entry);
        let sources = vec![panel.cwd.join(&entry.name)];
        let (verb, action) = if is_move {
            ("Rename", InputAction::MoveTo { sources })
        } else {
            ("Copy", InputAction::CopyTo { sources })
        };
        self.dialog = Some(Dialog::Input(InputDialog {
            title: format!(" {verb} \"{name}\" in place to: "),
            cursor: name.chars().count(),
            value: name,
            action,
        }));
    }

    /// S-F4: prompt for a file name, then edit it - existing or not.
    fn open_edit_new(&mut self) {
        if !self.require_local() {
            return;
        }
        self.dialog = Some(Dialog::Input(InputDialog {
            title: " Edit new file ".into(),
            value: String::new(),
            cursor: 0,
            action: InputAction::EditNew,
        }));
    }

    /// Open the editor on `path`; a missing file becomes an empty
    /// buffer that only lands on disk when saved.
    fn edit_new(&mut self, path: PathBuf) {
        if self.config.editor == "external" {
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| "vi".to_string());
            self.pending_exec = Some(Exec::Quiet(format!(
                "{editor} {}",
                shell_quote(&path.to_string_lossy())
            )));
            return;
        }
        let title = path.display().to_string();
        if path.exists() {
            self.open_internal_editor(&path, title);
        } else {
            let mut ed = rcmd_edit::Editor::create(&path);
            ed.prefs = self.config.edit_prefs();
            self.open_screen(Screen::Editor(Box::new(EditorState {
                hl: rcmd_edit::Highlighter::new(&path, 0),
                ed,
                title,
                top: 0,
                top_seg: 0,
                left: 0,
                wrap: false,
                rows: 1,
                cols: 1,
                prompt: None,
                note: None,
                wrap_column: self.config.edit_wrap_column as usize,
                menu: None,
                follow_up: None,
                bookmarks: Vec::new(),
                line_numbers: self.config.edit_line_numbers,
                gutter: 0,
            })));
        }
    }

    /// The panel's write half, or a status message saying why not.
    fn writable_targets(&mut self) -> Option<Vec<PathBuf>> {
        let panel = &self.panels[self.active];
        if panel.fs.writer().is_none() {
            self.status = Some(" archive is read-only ".into());
            return None;
        }
        let targets = panel.targets();
        if targets.is_empty() {
            self.status = Some(" nothing selected ".into());
            return None;
        }
        Some(targets)
    }

    /// C-x c: octal chmod on the marked entries (or the cursor entry).
    fn open_chmod(&mut self) {
        let Some(paths) = self.writable_targets() else {
            return;
        };
        let remote = self.panels[self.active].is_remote();
        let panel = &self.panels[self.active];
        let entry = panel.selected();
        let mode = entry.map_or(0o644, |e| e.mode & 0o7777);
        let mut dialog = ChmodDialog {
            paths,
            mode,
            octal: String::new(),
            octal_cursor: 0,
            name: entry.map_or_else(String::new, |e| panel.name_of(e)),
            owner: entry.map_or_else(String::new, |e| {
                crate::ui::owner_label(e.extra.uid, remote, true)
            }),
            group: entry.map_or_else(String::new, |e| {
                crate::ui::owner_label(e.extra.gid, remote, false)
            }),
            // the octal has the focus: anyone who already knows the mode
            // types it and presses Enter, as they always could, and the
            // boxes are there for everyone else
            row: CHMOD_OCTAL_ROW,
            button: 0,
            recurse: false,
        };
        dialog.sync_octal();
        self.dialog = Some(Dialog::Chmod(Box::new(dialog)));
    }

    /// C-x o: chown on the marked entries (or the cursor entry).
    fn open_chown(&mut self) {
        let Some(paths) = self.writable_targets() else {
            return;
        };
        // On a remote panel our /etc/passwd means nothing: the ids
        // belong to the server, so it stays a typed spec.
        if self.panels[self.active].is_remote() {
            self.dialog = Some(Dialog::Input(InputDialog {
                title: format!(" Chown {} (user[:group]) ", self.describe(&paths)),
                value: String::new(),
                cursor: 0,
                action: InputAction::Chown { paths },
            }));
            return;
        }
        let panel = &self.panels[self.active];
        let entry = panel.selected();
        let users = crate::ui::all_users();
        let groups = crate::ui::all_groups();
        let find = |list: &[(u32, String)], id: Option<u32>| {
            id.and_then(|id| list.iter().position(|entry| entry.0 == id))
                .unwrap_or(0)
        };
        self.dialog = Some(Dialog::Chown(Box::new(ChownDialog {
            user_row: find(&users, entry.and_then(|e| e.extra.uid)),
            group_row: find(&groups, entry.and_then(|e| e.extra.gid)),
            users,
            groups,
            paths,
            column: 0,
            button: 0,
            name: entry.map_or_else(String::new, |e| panel.name_of(e)),
            owner: entry.map_or_else(String::new, |e| {
                crate::ui::owner_label(e.extra.uid, false, true)
            }),
            group: entry.map_or_else(String::new, |e| {
                crate::ui::owner_label(e.extra.gid, false, false)
            }),
            recurse: false,
        })));
    }

    /// C-x s: create a symlink to the cursor entry.
    /// C-x s (absolute), C-x v (relative), C-x l (hard). MC fills in the
    /// original's path and suggests a name for the link, and lets you
    /// change either.
    fn open_link(&mut self, kind: LinkKind, relative: bool) {
        let panel = &self.panels[self.active];
        if panel.fs.writer().is_none() {
            self.status = Some(" archive is read-only ".into());
            return;
        }
        let Some(entry) = panel.selected().filter(|e| !e.is_parent()) else {
            self.status = Some(" nothing selected ".into());
            return;
        };
        let name = panel.name_of(entry);
        // relative to the directory the link lands in, which is this one
        let target = if relative {
            name.clone()
        } else {
            panel.cwd.join(&entry.name).display().to_string()
        };
        let link = format!("{name}-link");
        self.dialog = Some(Dialog::Link(Box::new(LinkDialog {
            kind,
            target_cursor: target.chars().count(),
            target,
            name_cursor: link.chars().count(),
            name: link,
            row: 1, // the name is what usually needs changing
            ok: true,
        })));
    }

    /// C-x C-s: change where an existing symlink points.
    fn open_edit_symlink(&mut self) {
        let panel = &self.panels[self.active];
        if panel.fs.writer().is_none() {
            self.status = Some(" archive is read-only ".into());
            return;
        }
        let Some(entry) = panel.selected().filter(|e| !e.is_parent()) else {
            self.status = Some(" nothing selected ".into());
            return;
        };
        let Some(target) = entry.link_target.clone() else {
            self.status = Some(" not a symlink ".into());
            return;
        };
        let target = target.display().to_string();
        self.dialog = Some(Dialog::Link(Box::new(LinkDialog {
            kind: LinkKind::EditSymlink,
            target_cursor: target.chars().count(),
            target,
            name: panel.name_of(entry),
            name_cursor: 0,
            row: 0,
            ok: true,
        })));
    }

    /// OK on the link form.
    fn submit_link(&mut self, d: LinkDialog) {
        let (target, name) = (d.target.trim().to_string(), d.name.trim().to_string());
        if target.is_empty() || name.is_empty() {
            return;
        }
        let cwd = self.panels[self.active].cwd.clone();
        let link = if Path::new(&name).is_absolute() {
            PathBuf::from(&name)
        } else {
            cwd.join(&name)
        };
        let target = PathBuf::from(&target);
        match d.kind {
            LinkKind::Hard => {
                // the original is named relative to the panel, as it is
                // for every other command here
                let existing = if target.is_absolute() {
                    target
                } else {
                    cwd.join(&target)
                };
                self.apply_fs_op(&[link], "link", |w, p| w.hard_link(&existing, p));
            }
            LinkKind::Symbolic => {
                self.apply_fs_op(&[link], "symlink", |w, p| w.symlink(&target, p));
            }
            // there is no atomic retarget: the link is replaced
            LinkKind::EditSymlink => self.apply_fs_op(&[link], "symlink", |w, p| {
                w.remove_file(p)?;
                w.symlink(&target, p)
            }),
        }
    }

    /// Run one FsWrite operation over several paths, reporting the
    /// first error (with a count of how many succeeded before it).
    fn apply_fs_op(
        &mut self,
        paths: &[PathBuf],
        verb: &str,
        op: impl Fn(&dyn rcmd_core::vfs::FsWrite, &Path) -> std::io::Result<()>,
    ) {
        let fs = self.panels[self.active].fs.clone();
        let Some(writer) = fs.writer() else {
            self.status = Some(" read-only ".into());
            return;
        };
        let mut done = 0usize;
        for path in paths {
            if let Err(err) = op(writer, path) {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                self.status = Some(format!(" {verb} {name}: {err} ({done} done) "));
                self.reload_panels();
                return;
            }
            done += 1;
        }
        self.status = Some(format!(" {verb}: {done} item(s) "));
        self.reload_panels();
    }

    fn reload_panels(&mut self) {
        for panel in &mut self.panels {
            let _ = panel.reload();
        }
        self.git_refresh();
    }

    fn open_mkdir(&mut self) {
        if self.panels[self.active].archive.is_some() && self.editable_archive().is_none() {
            return;
        }
        self.dialog = Some(Dialog::Input(InputDialog {
            title: " Create directory ".into(),
            value: String::new(),
            cursor: 0,
            action: InputAction::Mkdir,
        }));
    }

    fn open_select(&mut self, mark: bool) {
        self.dialog = Some(Dialog::Pattern(Box::new(PatternDialog {
            title: match mark {
                true => " Select group ".into(),
                false => " Unselect group ".into(),
            },
            value: "*".into(),
            cursor: 1,
            shell: true,
            case_sensitive: true,
            files_only: true,
            row: 0,
            ok: true,
            kind: PatternKind::Select { mark },
        })));
    }

    fn open_delete(&mut self, permanent: bool) {
        if self.panels[self.active].archive.is_some() && self.editable_archive().is_none() {
            return;
        }
        let panel = &self.panels[self.active];
        // no trash inside an archive or on a server: both delete outright
        let permanent = permanent || panel.is_remote() || panel.archive.is_some();
        let paths = panel.targets();
        if paths.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let what = self.describe(&paths);
        let message = if self.panels[self.active].is_remote() {
            format!("Permanently delete {what} from the server?")
        } else if permanent {
            format!("Permanently delete {what}?")
        } else {
            format!("Move {what} to trash?")
        };
        if !self.config.confirm_delete {
            self.start_delete(paths, permanent);
            return;
        }
        self.dialog = Some(Dialog::Confirm(ConfirmDialog {
            title: " Delete ".into(),
            message,
            yes: !permanent, // safer default for the irreversible variant
            paths,
            permanent,
            kind: ConfirmKind::Delete,
            command: None,
        }));
    }

    /// Yes on a confirm dialog: do whatever it was asking about.
    fn confirm_yes(&mut self, d: ConfirmDialog) {
        match d.kind {
            ConfirmKind::Delete => self.start_delete(d.paths, d.permanent),
            ConfirmKind::Quit => self.quit_now(),
            ConfirmKind::HotlistDelete { index } => {
                if index < self.config.hotlist.len() {
                    self.config.hotlist.remove(index);
                    self.save_hotlist();
                }
                let total = self.config.hotlist.len() + self.hotlist_recent().len();
                self.dialog = Some(Dialog::Hotlist(index.min(total.saturating_sub(1))));
            }
            ConfirmKind::Execute => {
                if let Some(cmd) = d.command {
                    self.pending_exec = Some(Exec::Quiet(cmd));
                }
            }
        }
    }

    /// No (or Esc) on a confirm dialog. Only the hotlist needs anything
    /// done: its own dialog was displaced to ask the question.
    fn confirm_no(&mut self, d: &ConfirmDialog) {
        if let ConfirmKind::HotlistDelete { index } = d.kind {
            self.dialog = Some(Dialog::Hotlist(index));
        }
    }

    /// A panel in tree mode needs its figure; leaving the mode drops
    /// it, so coming back starts from wherever the panel has got to.
    fn sync_tree(&mut self, side: usize) {
        if self.panels[side].list_mode != ListMode::Tree {
            self.trees[side] = None;
            return;
        }
        if self.trees[side].is_none() {
            let panel = &self.panels[side];
            self.trees[side] = Some(Tree::new(&panel.local_cwd(), panel.show_hidden));
        }
    }

    /// Enter in a tree *panel*: mc changes the **other** panel and
    /// stays in the tree, which is what makes the mode a navigator
    /// rather than a one-shot chooser. (The tree *dialog* is the
    /// one-shot chooser, and moves this panel instead.)
    fn tree_enter(&mut self) {
        let Some(path) = self.trees[self.active]
            .as_ref()
            .and_then(Tree::selected_path)
        else {
            return;
        };
        let other = &mut self.panels[self.active ^ 1];
        let result = if other.is_local() {
            other.cd(path)
        } else {
            other.to_local(path)
        };
        if let Err(err) = result {
            self.status = Some(format!(" {err} "));
        }
    }

    fn panel(&mut self) -> &mut Panel {
        &mut self.panels[self.active]
    }

    fn fallible(&mut self, op: impl FnOnce(&mut Panel) -> std::io::Result<bool>) {
        if let Err(err) = op(self.panel()) {
            self.status = Some(format!(" {err} "));
        }
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
            if let rcmd_core::view::Goto::Offset(offset) = goto {
                // a hex view is where an offset is worth naming
                v.hex_top = offset - offset % 16;
            }
        }
        Err(err) => v.note = Some(format!(" {err} ")),
    }
}

fn viewer_search(v: &mut Viewer, from: usize, is_next: bool) {
    // in nroff mode the search runs over what the overstrikes spell,
    // which is what is on the screen to be looked for
    let search = Search {
        nroff: v.nroff,
        ..v.search.to_search()
    };
    match v.file.find(from, &search) {
        Ok(Some(idx)) => {
            v.found = Some(idx);
            v.top = idx.saturating_sub(2);
            v.top_seg = 0;
            v.hex = false;
        }
        Ok(None) => {
            v.found = None;
            v.note = Some(
                if is_next {
                    " no more matches "
                } else {
                    " not found "
                }
                .into(),
            );
        }
        Err(err) => v.note = Some(format!(" {err} ")),
    }
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

/// Hand text to the desktop clipboard through whichever tool is
/// installed. False = none was, so the editor's own clipboard is all
/// there is - which is not an error worth a message, only a smaller
/// world.
fn clipboard_set(text: &str) -> bool {
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

/// ...and back. None = no tool, or it had nothing to say.
fn clipboard_get() -> Option<String> {
    desktop_clipboard().then_some(())?;
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
    None
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

/// Shared line editing for the command line and input dialogs.
/// Returns true when the key changed the value or cursor.
fn edit_line(value: &mut String, cursor: &mut usize, code: KeyCode, mods: KeyModifiers) -> bool {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    match code {
        KeyCode::Char('u') if ctrl => {
            value.clear();
            *cursor = 0;
        }
        KeyCode::Char('a') if ctrl => *cursor = 0,
        KeyCode::Char('e') if ctrl => *cursor = value.chars().count(),
        KeyCode::Char(c) if !ctrl && !alt => {
            value.insert(byte_index(value, *cursor), c);
            *cursor += 1;
        }
        KeyCode::Backspace => {
            if *cursor > 0 {
                *cursor -= 1;
                value.remove(byte_index(value, *cursor));
            }
        }
        KeyCode::Delete => {
            let idx = byte_index(value, *cursor);
            if idx < value.len() {
                value.remove(idx);
            }
        }
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(value.chars().count()),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = value.chars().count(),
        _ => return false,
    }
    true
}

/// "archive.zip://sub/dir" → (archive path, path inside). Plain local
/// paths return None.
/// A location that lives on a server rather than on this machine.
fn is_remote_url(target: &str) -> bool {
    target.starts_with("sftp://") || target.starts_with("ftp://") || target.starts_with("fish://")
}

fn split_vfs_dest(input: &str) -> Option<(PathBuf, PathBuf)> {
    let (archive, inside) = input.split_once("://")?;
    Some((
        PathBuf::from(archive),
        PathBuf::from(inside.trim_matches('/')),
    ))
}

fn byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
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
    fn edit_line_handles_unicode_and_shortcuts() {
        let mut value = String::new();
        let mut cursor = 0;
        for c in "héllo".chars() {
            edit_line(
                &mut value,
                &mut cursor,
                KeyCode::Char(c),
                KeyModifiers::NONE,
            );
        }
        assert_eq!(value, "héllo");
        assert_eq!(cursor, 5);
        edit_line(
            &mut value,
            &mut cursor,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(cursor, 0);
        edit_line(&mut value, &mut cursor, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(value, "éllo");
        edit_line(
            &mut value,
            &mut cursor,
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        );
        assert_eq!(value, "");
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
}
