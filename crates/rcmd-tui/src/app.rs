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
use rcmd_core::fsops::{self, JobEvent, JobHandle, Reply};
use rcmd_core::glob::glob_match;
use rcmd_core::panel::{ListMode, LoadKind, Panel, SortKey};
use rcmd_core::sftp::{self, ConnectEvent, ConnectReply, SftpFs, SftpUrl};
use rcmd_core::vfs::{FsProvider, LocalFs};
use rcmd_core::view::FileView;

use crate::config::{Config, HotEntry};
use crate::keymap::Keymap;
use crate::subshell::Subshell;
use crate::{config, git, keymap, state, ui};

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
    SelectGlob {
        mark: bool,
    },
    Filter,
    Panelize,
    /// F9 → Command → SFTP link: the value is an sftp:// URL.
    SftpConnect,
    /// S-F4: the value is the file to edit (created on first save).
    EditNew,
    /// M-c: the value is a cd target (path or sftp:// URL).
    QuickCd,
    /// C-x c: the value is an octal mode for these paths.
    Chmod {
        paths: Vec<PathBuf>,
    },
    /// C-x o: the value is `user[:group]` for these paths.
    Chown {
        paths: Vec<PathBuf>,
    },
    /// C-x s: the value is the link name; `target` is what it points to.
    SymlinkTo {
        target: PathBuf,
    },
}

/// An SFTP connection attempt on its worker thread; `ask` is the
/// interactive question currently shown (host key / password).
pub struct ConnectState {
    handle: sftp::ConnectHandle,
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
struct RemoteEdit {
    fs: Arc<dyn FsProvider>,
    remote_path: PathBuf,
    temp: PathBuf,
    mtime_before: Option<std::time::SystemTime>,
}

/// Alt+F7 find dialog: filename glob + optional content substring.
pub struct FindDialog {
    pub name: String,
    pub name_cursor: usize,
    pub content: String,
    pub content_cursor: usize,
    /// Skip gitignored trees when searching inside a work tree.
    pub skip_ignored: bool,
    /// 0 = name field, 1 = content field, 2 = the gitignore checkbox.
    pub field: usize,
}

/// A running find, streaming matches into `panel`'s panelized listing.
pub struct FindState {
    pub handle: FindHandle,
    pub panel: usize,
    pub count: usize,
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

pub struct ConfirmDialog {
    pub title: String,
    pub message: String,
    pub yes: bool,
    pub paths: Vec<PathBuf>,
    pub permanent: bool,
    pub kind: ConfirmKind,
}

/// What a [`ConfirmDialog`] does when answered Yes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    Delete,
    Quit,
}

pub enum Dialog {
    Input(InputDialog),
    Confirm(ConfirmDialog),
    /// Directory hotlist; the payload is the selected row.
    Hotlist(usize),
    Find(FindDialog),
    /// F2 user menu ([[commands]]); the payload is the selected row.
    UserMenu(usize),
    Options(OptionsDialog),
    /// Bulk rename: what the edited buffer asks for, awaiting Yes/No.
    RenamePreview(RenamePreview),
    /// Background-jobs list; the payload is the selected row.
    Jobs(usize),
    /// M-h: the command-line history; the payload is the selected row.
    History(usize),
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
struct BulkRename {
    dir: PathBuf,
    names: Vec<std::ffi::OsString>,
    temp: PathBuf,
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
    /// Focused button on the button row: true = OK.
    pub ok: bool,
}

/// One setting in the form. Radio pairs (editor, theme) are stored as a
/// bool too: the label spells out which side `true` means.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Opt {
    Hidden,
    Lynx,
    Mouse,
    Watch,
    Git,
    ConfirmDelete,
    ConfirmOverwrite,
    ConfirmExit,
    Subshell,
    ExternalEditor,
    DarkTheme,
}

pub const OPT_COUNT: usize = 11;

/// A row of the options form: a section heading or a setting.
pub enum OptRow {
    Head(&'static str),
    /// Checkbox: label.
    Check(Opt, &'static str),
    /// Radio pair: (label, text for false, text for true).
    Radio(Opt, &'static str, &'static str, &'static str),
}

/// The form, in display order. One dialog covering MC's five (PLAN4 S0):
/// sections keep it readable without five separate screens.
pub const OPTION_ROWS: &[OptRow] = &[
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
        }
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
                    .and_then(OptRow::opt)
                    .is_some()
            {
                self.cursor = cursor as usize;
                return;
            }
        }
    }
}

pub enum Ask {
    Overwrite { path: PathBuf },
    Error { path: PathBuf, message: String },
}

impl Ask {
    pub fn buttons(&self) -> &'static [&'static str] {
        match self {
            Ask::Overwrite { .. } => &["Overwrite", "All", "Skip", "Skip all", "Abort"],
            Ask::Error { .. } => &["Retry", "Skip", "Skip all", "Abort"],
        }
    }

    fn reply(&self, button: usize) -> Reply {
        match self {
            Ask::Overwrite { .. } => [
                Reply::Overwrite,
                Reply::OverwriteAll,
                Reply::Skip,
                Reply::SkipAll,
                Reply::Abort,
            ][button],
            Ask::Error { .. } => [Reply::Retry, Reply::Skip, Reply::SkipAll, Reply::Abort][button],
        }
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
}

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
    /// Content rows; updated on every draw, drives paging.
    pub rows: usize,
    pub search: String,
    pub found: Option<usize>,
    /// Search prompt (value, cursor) when open.
    pub prompt: Option<(String, usize)>,
    pub note: Option<String>,
    /// Extraction scratch file when viewing inside an archive;
    /// deleted when the viewer closes.
    pub temp: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Help,
    Menu,
    Mark,
    QuickSearch,
    Hotlist,
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
    /// M-t, like MC: brief → full → long → brief.
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
    /// The running-jobs list (C-x j / F9 > Command > Jobs).
    Jobs,
    /// M-h: the command-line history as a pick list.
    HistoryList,
    /// Shift+F3: the internal viewer without any [[view]] filter.
    ViewRaw,
    /// C-l: redraw the screen from scratch.
    Repaint,
}

/// None = separator line. `&` in a label marks its hotkey letter,
/// MC-style: highlighted in the dropdown, pressing it runs the entry.
pub type MenuEntry = Option<(&'static str, &'static str, Action)>;

pub const MENUS: &[(&str, &[MenuEntry])] = &[
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
            Some(("&Filter files...", "C-f", Action::Filter)),
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
            Some(("&Find file...", "M-F7", Action::FindFile)),
            Some(("&Panelize command...", "", Action::Panelize)),
            Some(("&Compare directories", "C-x d", Action::CompareDirs)),
            Some(("SFTP &link...", "", Action::SftpLink)),
            Some(("&Open shell", "C-o", Action::Shell)),
            Some(("&Reload panel", "C-r", Action::Reload)),
            Some(("S&wap panels", "C-u", Action::SwapPanels)),
            Some(("Toggle hidde&n files", "M-.", Action::ToggleHidden)),
            Some(("&Jobs...", "C-x j", Action::Jobs)),
            Some(("Command &history...", "M-h", Action::HistoryList)),
        ],
    ),
    (
        "&Sort",
        &[
            Some(("By &name", "M-n", Action::Sort(SortKey::Name))),
            Some(("By &extension", "", Action::Sort(SortKey::Ext))),
            Some(("By &size", "", Action::Sort(SortKey::Size))),
            Some(("By &modify time", "", Action::Sort(SortKey::Mtime))),
            None,
            Some(("Toggle &reverse", "", Action::SortReverse)),
        ],
    ),
    (
        "&View",
        &[
            Some(("&Brief listing", "", Action::Listing(ListMode::Brief))),
            Some(("&Full listing", "", Action::Listing(ListMode::Full))),
            Some(("&Long listing", "", Action::Listing(ListMode::Long))),
            None,
            Some(("&Quick view", "C-x q", Action::QuickView)),
            Some(("&Info panel", "C-x i", Action::InfoView)),
            None,
            Some(("Other panel: &same dir", "M-i", Action::OtherSameDir)),
            Some(("Other panel: &this dir", "M-o", Action::OtherOpenDir)),
        ],
    ),
    (
        "&Options",
        &[Some(("&Panel options...", "", Action::Options))],
    ),
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
    let (mut keymap, mut warnings) = keymap::build(&config.keymap, config.lynx_on(), &config.keys);
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
    pub viewer: Option<Viewer>,
    pub editor: Option<EditorState>,
    pub quick_view: Option<QuickView>,
    /// Ctrl+X i: which panel shows the info pane, if any (mutually
    /// exclusive with `quick_view`).
    pub info: Option<usize>,
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
    /// Live SFTP connections by URL prefix; weak so that leaving a
    /// remote directory on both panels closes the connection.
    connections: Vec<(String, Weak<SftpFs>)>,
    remote_edit: Option<RemoteEdit>,
    bulk_rename: Option<BulkRename>,
    du: Option<DuJob>,
    watch: Option<WatchState>,
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
        let status = if warnings.is_empty() {
            None
        } else {
            Some(format!(" {} ", warnings.join(" · ")))
        };
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
            viewer: None,
            editor: None,
            quick_view: None,
            info: None,
            disk: [None, None],
            menu: None,
            help: None,
            cmdline,
            quick_search: None,
            find: None,
            connect: None,
            connections: Vec::new(),
            remote_edit: None,
            bulk_rename: None,
            du: None,
            watch,
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
            pending_exec: None,
            subshell,
            quit: false,
        })
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            self.drain_job();
            self.drain_find();
            self.drain_connect();
            self.drain_du();
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
                self.dispatch_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            }
            if self.repaint {
                self.repaint = false;
                terminal.clear()?;
            }
            terminal.draw(|frame| ui::draw(frame, self))?;
            let loading = self.panels.iter().any(Panel::is_loading);
            let watch_pending = self
                .watch
                .as_ref()
                .is_some_and(|w| w.dirty.iter().any(Option::is_some));
            let timeout = if !self.jobs.is_empty()
                || self.find.is_some()
                || self.connect.is_some()
                || self.du.is_some()
                || loading
                || watch_pending
                || self.esc_at.is_some()
                || self.subshell.as_ref().is_some_and(|s| !s.ready())
                || self.viewer.as_ref().is_some_and(|v| v.follow)
            {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(500)
            };
            if event::poll(timeout)? {
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
        for panel in &mut self.panels {
            if let Some(Err(err)) = panel.poll_pending() {
                self.status = Some(format!(" {err} "));
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
    fn tick_watch(&mut self) {
        use std::time::Instant;
        let busy = self.fg_job().is_some();
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
        if busy || self.find.is_some() || self.dialog.is_some() || self.quick_search.is_some() {
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
        let mut done = None;
        while let Ok(event) = find.handle.events.try_recv() {
            match event {
                FindEvent::Match(entry) => {
                    self.panels[find.panel].entries.push(*entry);
                    find.count += 1;
                }
                FindEvent::Done { matches, scanned } => done = Some((matches, scanned)),
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

    /// Start (or resume) an SFTP connection for the active panel.
    fn connect_sftp(&mut self, input: &str) {
        if self.connect.is_some() {
            self.status = Some(" a connection attempt is already running ".into());
            return;
        }
        let Some(url) = SftpUrl::parse(input) else {
            self.status = Some(" bad URL - sftp://[user@]host[:port][/path] ".into());
            return;
        };
        let handle = match self.connection(&url.prefix()) {
            Some(fs) => sftp::spawn_reuse(fs, url),
            None => sftp::spawn_connect(url),
        };
        self.status = Some(format!(
            " connecting to {}… - Esc cancels ",
            handle.url.host
        ));
        self.connect = Some(ConnectState {
            handle,
            panel: self.active,
            ask: None,
        });
    }

    /// Look up a live connection by URL prefix, dropping dead ones.
    fn connection(&mut self, prefix: &str) -> Option<Arc<SftpFs>> {
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
    fn finish_remote_edit(&mut self) {
        let Some(edit) = self.remote_edit.take() else {
            return;
        };
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
        if let Some(note) = sub.note.take() {
            self.status = Some(note);
        }
        if sub.failed {
            self.subshell = None;
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
                    } => {
                        job.files_done = files_done;
                        job.bytes_done = bytes_done;
                        job.current = current;
                    }
                    JobEvent::AskOverwrite { path } => {
                        if !confirm_overwrite {
                            // the user turned the question off: answer it
                            // once, for every remaining file in this job
                            let _ = job.handle.replies.send(Reply::OverwriteAll);
                            continue;
                        }
                        job.ask = Some(Ask::Overwrite { path });
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
        } else if self.editor.is_some() {
            self.on_editor_key(key);
        } else if self.viewer.is_some() {
            self.on_viewer_key(key);
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
            || self.viewer.is_some()
        {
            return;
        }
        if let Some(st) = self.editor.as_mut() {
            if st.prompt.is_none() && y >= 1 && (y as usize) <= st.rows {
                let (line, col) = if st.wrap {
                    // walk visual rows down from the top to this row
                    let cols = st.cols.max(1);
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
        if y == area.y + 1 {
            self.header_click(side, area, x);
            return;
        }
        // 2 border+header rows on top, 1 border row at the bottom
        let content_y = area.y + 2;
        if y < content_y || y + 1 >= area.y + area.height {
            return;
        }
        let index = self.table_states[side].offset() + (y - content_y) as usize;
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
        let panel = &mut self.panels[side];
        let key = match panel.list_mode {
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
                let action = *action;
                self.menu = None;
                self.run_action(action);
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
        if let Some(st) = self.editor.as_mut() {
            if st.prompt.is_none() {
                st.ed.move_vert(delta, false);
                self.ensure_editor_visible();
            }
            return;
        }
        if let Some(v) = self.viewer.as_mut() {
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
        let Some(v) = self.viewer.as_mut() else {
            return;
        };
        v.note = None;
        if let Some((value, cursor)) = v.prompt.as_mut() {
            match key.code {
                KeyCode::Esc => v.prompt = None,
                KeyCode::Enter => {
                    let needle = value.trim().to_string();
                    v.prompt = None;
                    if !needle.is_empty() {
                        v.search = needle;
                        viewer_search(v, v.top, false);
                    }
                }
                code => {
                    edit_line(value, cursor, code, key.modifiers);
                }
            }
            return;
        }
        let rows = v.rows.max(1);
        let page = rows.saturating_sub(1).max(1) as isize;
        match key.code {
            KeyCode::F(3) | KeyCode::F(10) | KeyCode::Esc | KeyCode::Char('q') => {
                if let Some(viewer) = self.viewer.take()
                    && let Some(temp) = viewer.temp
                {
                    let _ = std::fs::remove_file(temp);
                }
            }
            KeyCode::F(2) => {
                v.wrap = !v.wrap;
                v.top_seg = 0;
                v.left = 0;
            }
            KeyCode::F(4) => v.hex = !v.hex,
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
            KeyCode::F(7) | KeyCode::Char('/') => {
                v.prompt = Some((v.search.clone(), v.search.chars().count()));
            }
            KeyCode::Char('n') if !v.search.is_empty() => {
                let from = v.found.map(|f| f + 1).unwrap_or(v.top);
                viewer_search(v, from, true);
            }
            KeyCode::Char('f' | 'F') => {
                v.follow = !v.follow;
                if v.follow {
                    let _ = v.file.refresh();
                    viewer_end(v, rows);
                    v.note = Some(" following - f stops ".into());
                }
            }
            _ => {}
        }
    }

    /// Follow mode: re-index on growth and stick to the bottom.
    fn follow_tick(&mut self) {
        if let Some(v) = self.viewer.as_mut()
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
                    self.menu = None;
                    self.run_action(action);
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
                    self.menu = None;
                    self.run_action(action);
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

    fn run_action(&mut self, action: Action) {
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
            Action::CompareDirs => self.compare_dirs(),
            Action::DirSize => self.dir_size(),
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
                if self.config.confirm_exit {
                    self.dialog = Some(Dialog::Confirm(ConfirmDialog {
                        title: " Quit ".into(),
                        message: "Quit rcmd?".into(),
                        yes: true,
                        paths: Vec::new(),
                        permanent: false,
                        kind: ConfirmKind::Quit,
                    }));
                } else {
                    self.quit = true;
                }
            }
            Action::Shell => self.pending_exec = Some(Exec::Shell),
            Action::SftpLink => {
                self.dialog = Some(Dialog::Input(InputDialog {
                    title: " SFTP link (sftp://[user@]host[:port][/path]) ".into(),
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
            Action::Listing(mode) => self.panel().list_mode = mode,
            Action::ListingCycle => {
                let panel = self.panel();
                panel.list_mode = match panel.list_mode {
                    ListMode::Brief => ListMode::Full,
                    ListMode::Full => ListMode::Long,
                    ListMode::Long => ListMode::Brief,
                };
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
            Action::ToggleHidden => self.fallible(|p| p.toggle_hidden().map(|()| true)),
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
                values[Opt::Subshell as usize] = cfg.subshell;
                values[Opt::ExternalEditor as usize] = cfg.editor == "external";
                values[Opt::DarkTheme as usize] = cfg.theme == "dark";
                self.dialog = Some(Dialog::Options(OptionsDialog {
                    // start on the first setting, not the heading
                    cursor: 1,
                    values,
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
                self.disk[side] = Some((panel.cwd.clone(), Instant::now(), free_space(&panel.cwd)));
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
    /// full sftp:// URL (routed through the connection cache).
    fn navigate(&mut self, target: &str) {
        if target.starts_with("sftp://") {
            self.connect_sftp(target);
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
        let panel = &self.panels[self.active];
        let Some(entry) = panel.selected() else {
            return;
        };
        if entry.is_dir() {
            self.status = Some(" cannot view a directory ".into());
            return;
        }
        let name = entry.name.clone();
        // [[view]] filter (F3 only - Shift+F3 stays raw): the
        // command's stdout becomes the viewed text
        if !raw && panel.is_local() {
            let lower = name.to_string_lossy().to_lowercase();
            let rule = self
                .config
                .view
                .iter()
                .find(|rule| glob_match(&rule.pattern.to_lowercase(), &lower))
                .cloned();
            if let Some(rule) = rule
                && self.open_filtered_view(&rule)
            {
                return;
            }
        }
        let panel = &self.panels[self.active];
        let (open_path, title_path, temp) = if panel.is_local() {
            let path = panel.cwd.join(&name);
            (path.clone(), path, None)
        } else {
            // fetch the archive member / remote file to a scratch file
            let vpath = panel.cwd.join(&name);
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
                return;
            }
            let title = if let Some(prefix) = &panel.remote {
                PathBuf::from(format!("{prefix}{}", vpath.display()))
            } else {
                let archive = panel.archive.clone().unwrap_or_default();
                PathBuf::from(format!("{}://{}", archive.display(), vpath.display()))
            };
            (temp.clone(), title, Some(temp))
        };
        match FileView::open(&open_path) {
            Ok(file) => {
                self.viewer = Some(Viewer {
                    hl: rcmd_edit::Highlighter::new(&open_path, file.size as usize),
                    file,
                    path: title_path,
                    hex: false,
                    wrap: false,
                    follow: false,
                    top: 0,
                    top_seg: 0,
                    left: 0,
                    cols: 1,
                    hex_top: 0,
                    rows: 1,
                    search: String::new(),
                    found: None,
                    prompt: None,
                    note: None,
                    temp,
                })
            }
            Err(err) => {
                if let Some(temp) = temp {
                    let _ = std::fs::remove_file(temp);
                }
                self.status = Some(format!(" view: {err} "));
            }
        }
    }

    /// Run a `[[view]]` filter and open the internal viewer on its
    /// stdout. False = filter unusable (spawn failed, or it failed
    /// with nothing to show) - the caller falls back to the raw view.
    fn open_filtered_view(&mut self, rule: &crate::config::OpenRule) -> bool {
        let cwd = self.panels[self.active].local_cwd();
        let cmd = self.expand_macros(&rule.run);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let output = std::process::Command::new(&shell)
            .arg("-c")
            .arg(&cmd)
            .current_dir(&cwd)
            .output();
        let output = match output {
            Ok(out) => out,
            Err(err) => {
                self.status = Some(format!(" view filter: {err} - showing raw "));
                return false;
            }
        };
        if output.stdout.is_empty() && !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            let first = err.lines().next().unwrap_or("failed").trim();
            self.status = Some(format!(" view filter: {first} - showing raw "));
            return false;
        }
        let temp = std::env::temp_dir().join(format!("rcmd-view-{}-filtered", std::process::id()));
        if std::fs::write(&temp, &output.stdout).is_err() {
            return false;
        }
        match FileView::open(&temp) {
            Ok(file) => {
                self.viewer = Some(Viewer {
                    hl: None, // filter output, not the file's syntax
                    file,
                    path: PathBuf::from(cmd),
                    hex: false,
                    wrap: false,
                    follow: false,
                    top: 0,
                    top_seg: 0,
                    left: 0,
                    cols: 1,
                    hex_top: 0,
                    rows: 1,
                    search: String::new(),
                    found: None,
                    prompt: None,
                    note: None,
                    temp: Some(temp),
                });
                true
            }
            Err(_) => {
                let _ = std::fs::remove_file(&temp);
                false
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
            self.remote_edit = Some(RemoteEdit {
                fs: panel.fs.clone(),
                remote_path,
                mtime_before: std::fs::metadata(&temp).and_then(|m| m.modified()).ok(),
                temp: temp.clone(),
            });
            if external {
                self.pending_exec = Some(Exec::Quiet(format!(
                    "{editor} {}",
                    shell_quote(&temp.to_string_lossy())
                )));
            } else if !self.open_internal_editor(&temp, title) {
                // failed to open: don't leave a stale upload hook behind
                self.remote_edit = None;
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
        match rcmd_edit::Editor::open(path) {
            Ok(ed) => {
                let len = std::fs::metadata(path)
                    .map(|m| m.len() as usize)
                    .unwrap_or(0);
                self.editor = Some(EditorState {
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
                });
                true
            }
            Err(err) => {
                self.status = Some(format!(" edit: {err} "));
                false
            }
        }
    }

    fn close_editor(&mut self) {
        self.editor = None;
        self.finish_remote_edit();
        self.finish_bulk_rename();
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
        if self.open_internal_editor(&temp, title) {
            self.bulk_rename = Some(BulkRename { dir, names, temp });
        } else {
            let _ = std::fs::remove_file(&temp);
        }
    }

    /// After the bulk-rename editor closes: diff the saved buffer and
    /// hand the outcome to the preview dialog. An unsaved session left
    /// the temp file untouched, which diffs to "no changes".
    fn finish_bulk_rename(&mut self) {
        let Some(bulk) = self.bulk_rename.take() else {
            return;
        };
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
        let Some(st) = self.editor.as_mut() else {
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
        let Some(st) = self.editor.as_mut() else {
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
        let Some(st) = self.editor.as_mut() else {
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

    fn on_editor_key(&mut self, key: KeyEvent) {
        if self.editor.as_ref().is_some_and(|st| st.prompt.is_some()) {
            self.on_editor_prompt_key(key);
            self.ensure_editor_visible();
            return;
        }
        let Some(st) = self.editor.as_mut() else {
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
            KeyCode::Tab => st.ed.insert("\t"),
            KeyCode::Backspace => st.ed.backspace(),
            KeyCode::Delete => st.ed.delete_forward(),
            KeyCode::F(2) => {
                self.editor_save();
                return;
            }
            KeyCode::F(3) => {
                st.ed.toggle_mark();
                edited = false;
            }
            KeyCode::F(4) => {
                let value = st.ed.search.clone();
                st.prompt = Some(EditPrompt::ReplaceFind {
                    cursor: value.chars().count(),
                    value,
                });
                return;
            }
            KeyCode::F(7) if !select => {
                let value = st.ed.search.clone();
                st.prompt = Some(EditPrompt::Search {
                    cursor: value.chars().count(),
                    value,
                });
                return;
            }
            // Shift+F7 (legacy terminals send F19): repeat last search
            KeyCode::F(7) | KeyCode::F(19) => {
                let pattern = st.ed.search.clone();
                let from = next_pos(&st.ed);
                if pattern.is_empty() {
                    st.prompt = Some(EditPrompt::Search {
                        value: String::new(),
                        cursor: 0,
                    });
                } else {
                    self.editor_find(&pattern, from);
                }
                return;
            }
            KeyCode::Char('w') if alt => {
                st.wrap = !st.wrap;
                st.top_seg = 0;
                st.left = 0;
                edited = false;
            }
            // mcedit hands: F5 copies the block (or line), F6 moves it
            KeyCode::F(5) => st.ed.block_copy(),
            KeyCode::F(6) => st.ed.block_move(),
            KeyCode::F(8) => st.ed.delete_selection_or_line(),
            KeyCode::F(10) => {
                self.editor_quit();
                return;
            }
            KeyCode::Esc => {
                edited = false;
                if st.ed.has_selection() {
                    st.ed.clear_selection();
                } else {
                    self.editor_quit();
                    return;
                }
            }
            KeyCode::Char(c) if ctrl => {
                edited = false;
                match c.to_ascii_lowercase() {
                    'z' => {
                        edited = true;
                        if !st.ed.undo() {
                            st.note = Some(" nothing to undo ".into());
                        }
                    }
                    'y' => {
                        edited = true;
                        if !st.ed.redo() {
                            st.note = Some(" nothing to redo ".into());
                        }
                    }
                    'c' => st.ed.copy(),
                    'x' => {
                        edited = true;
                        st.ed.cut();
                    }
                    'v' => {
                        edited = true;
                        st.ed.paste();
                    }
                    'a' => st.ed.select_all(),
                    's' => {
                        self.editor_save();
                        return;
                    }
                    _ => {}
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

    fn on_editor_prompt_key(&mut self, key: KeyEvent) {
        let Some(st) = self.editor.as_mut() else {
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
        }
    }

    /// Scroll the editor viewport so the cursor stays on screen.
    fn ensure_editor_visible(&mut self) {
        let Some(st) = self.editor.as_mut() else {
            return;
        };
        let rows = st.rows.max(1);
        let cols = st.cols.max(1);
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
                KeyCode::Esc | KeyCode::Char('n') => {}
                KeyCode::Char('y') => self.confirm_yes(d),
                KeyCode::Enter => {
                    if d.yes {
                        self.confirm_yes(d);
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
                            if entry.path.starts_with("sftp://") {
                                self.connect_sftp(&entry.path);
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
                        if selected < len {
                            self.config.hotlist.remove(selected);
                            self.save_hotlist();
                        }
                        let total = self.config.hotlist.len() + recent.len();
                        self.dialog = Some(Dialog::Hotlist(selected.min(total.saturating_sub(1))));
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
                KeyCode::Enter => self.submit_find(d),
                KeyCode::Tab | KeyCode::Down => {
                    d.field = (d.field + 1) % 3;
                    self.dialog = Some(Dialog::Find(d));
                }
                KeyCode::BackTab | KeyCode::Up => {
                    d.field = (d.field + 2) % 3;
                    self.dialog = Some(Dialog::Find(d));
                }
                KeyCode::Char(' ') if d.field == 2 => {
                    d.skip_ignored = !d.skip_ignored;
                    self.dialog = Some(Dialog::Find(d));
                }
                code => {
                    if d.field < 2 {
                        let (value, cursor) = if d.field == 0 {
                            (&mut d.name, &mut d.name_cursor)
                        } else {
                            (&mut d.content, &mut d.content_cursor)
                        };
                        edit_line(value, cursor, code, key.modifiers);
                    }
                    self.dialog = Some(Dialog::Find(d));
                }
            },
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
        self.config.confirm_delete = d.get(Opt::ConfirmDelete);
        self.config.confirm_overwrite = d.get(Opt::ConfirmOverwrite);
        self.config.confirm_exit = d.get(Opt::ConfirmExit);
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
        }) {
            self.status = Some(format!(" could not save state: {err} "));
        }
    }

    fn submit_find(&mut self, dialog: FindDialog) {
        let panel_idx = self.active;
        let panel = &mut self.panels[panel_idx];
        let pattern = {
            let name = dialog.name.trim();
            if name.is_empty() { "*" } else { name }.to_string()
        };
        let content = {
            let text = dialog.content.trim();
            (!text.is_empty()).then(|| text.to_string())
        };
        let root = panel.cwd.clone();
        let label = match &content {
            Some(text) => format!("find: {pattern} ~ \"{text}\""),
            None => format!("find: {pattern}"),
        };
        panel.panelize(Vec::new(), label);
        let skip = if dialog.skip_ignored {
            git::ignore_filter(&root)
        } else {
            None
        };
        self.find = Some(FindState {
            handle: find::spawn_find(root, pattern, content, skip),
            panel: panel_idx,
            count: 0,
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
        self.dialog = Some(Dialog::Find(FindDialog {
            name: "*".into(),
            name_cursor: 1,
            content: String::new(),
            content_cursor: 0,
            skip_ignored: true,
            field: 0,
        }));
    }

    fn open_panelize(&mut self) {
        if !self.require_local() {
            return;
        }
        self.dialog = Some(Dialog::Input(InputDialog {
            title: " Panelize (command output as listing) ".into(),
            value: String::new(),
            cursor: 0,
            action: InputAction::Panelize,
        }));
    }

    /// Quick compare of both panel listings: marks files that are missing
    /// on the other side or differ in size/mtime.
    fn compare_dirs(&mut self) {
        use std::collections::HashMap;
        use std::ffi::OsString;
        use std::time::SystemTime;

        // works on local and remote listings alike; only archives are out
        if self.panels[0].archive.is_some() || self.panels[1].archive.is_some() {
            self.status = Some(" cannot compare inside an archive ".into());
            return;
        }
        let files = |panel: &Panel| -> HashMap<OsString, (u64, Option<SystemTime>)> {
            panel
                .entries
                .iter()
                .filter(|e| !e.is_dir() && !e.is_parent())
                .map(|e| (e.name.clone(), (e.size, e.mtime)))
                .collect()
        };
        let close = |a: Option<SystemTime>, b: Option<SystemTime>| match (a, b) {
            (Some(a), Some(b)) => {
                a.duration_since(b).unwrap_or_else(|e| e.duration()) <= Duration::from_secs(2)
            }
            _ => true, // unknown mtimes never count as a difference
        };
        let left = files(&self.panels[0]);
        let right = files(&self.panels[1]);
        self.panels[0].marked.clear();
        self.panels[1].marked.clear();
        let mut differ = 0usize;
        for (name, (size, mtime)) in &left {
            match right.get(name) {
                None => {
                    self.panels[0].marked.insert(name.clone());
                    differ += 1;
                }
                Some((rsize, rmtime)) if size != rsize || !close(*mtime, *rmtime) => {
                    self.panels[0].marked.insert(name.clone());
                    self.panels[1].marked.insert(name.clone());
                    differ += 1;
                }
                Some(_) => {}
            }
        }
        for name in right.keys() {
            if !left.contains_key(name) {
                self.panels[1].marked.insert(name.clone());
                differ += 1;
            }
        }
        self.status = Some(format!(" {differ} difference(s) marked "));
    }

    fn submit_input(&mut self, dialog: InputDialog) {
        let value = dialog.value.trim().to_string();
        if let InputAction::Filter = dialog.action {
            let panel = &mut self.panels[self.active];
            panel.filter = if value.is_empty() || value == "*" {
                None
            } else {
                Some(value)
            };
            self.fallible(|p| p.reload().map(|()| true));
            return;
        }
        if value.is_empty() {
            return;
        }
        match dialog.action {
            InputAction::CopyTo { sources } => self.route_transfer(sources, &value, false),
            InputAction::MoveTo { sources } => self.route_transfer(sources, &value, true),
            InputAction::Mkdir => {
                if self.panels[self.active].is_remote() {
                    self.remote_mkdir(&value);
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
            InputAction::SelectGlob { mark } => self.panels[self.active].mark_glob(&value, mark),
            InputAction::Filter => unreachable!("handled above"),
            InputAction::Panelize => self.run_panelize(&value),
            InputAction::SftpConnect => self.connect_sftp(&value),
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
            InputAction::Chmod { paths } => {
                let Ok(mode) = u32::from_str_radix(value.trim(), 8) else {
                    self.status = Some(format!(" chmod: '{}' is not octal ", value.trim()));
                    return;
                };
                if mode > 0o7777 {
                    self.status = Some(" chmod: mode out of range ".into());
                    return;
                }
                self.apply_fs_op(&paths, "chmod", |w, p| w.set_mode(p, mode));
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
            InputAction::SymlinkTo { target } => {
                let name = value.trim();
                if name.is_empty() {
                    return;
                }
                let link = if Path::new(name).is_absolute() {
                    PathBuf::from(name)
                } else {
                    self.panels[self.active].cwd.join(name)
                };
                self.apply_fs_op(&[link], "symlink", |w, p| w.symlink(&target, p));
            }
        }
    }

    /// Send F5/F6 to the right job for the source panel and the typed
    /// destination: plain copy/move, archive pack/extract, or a
    /// cross-provider transfer when SFTP is on either side.
    fn route_transfer(&mut self, sources: Vec<PathBuf>, value: &str, is_move: bool) {
        let src_panel = &self.panels[self.active];
        let src_archive = src_panel.archive.is_some();
        // sftp:// destination (must match before the zip:// syntax -
        // a URL also contains "://")
        if value.starts_with("sftp://") {
            let Some(url) = SftpUrl::parse(value) else {
                self.status = Some(" bad URL - sftp://[user@]host[:port]/path ".into());
                return;
            };
            if url.path.as_os_str().is_empty() {
                self.status = Some(" destination URL needs a path ".into());
                return;
            }
            if is_move && src_archive {
                self.status = Some(" archive is read-only ".into());
                return;
            }
            let Some(dst_fs) = self.connection(&url.prefix()) else {
                self.status = Some(format!(" not connected - cd {} first ", url.prefix()));
                return;
            };
            let src_fs = self.panels[self.active].fs.clone();
            let label = url.display();
            self.start_vfs_transfer(src_fs, sources, dst_fs, url.path, is_move, label);
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
                self.status = Some(" archive is read-only ".into());
            } else if split_vfs_dest(value).is_some() {
                self.status = Some(" cannot move into an archive ".into());
            } else {
                self.start_transfer(sources, value, fsops::spawn_move, "move");
            }
            return;
        }
        match (split_vfs_dest(value), !src_archive) {
            (Some(_), false) => self.status = Some(" cannot copy from archive to archive ".into()),
            (Some((archive, inside)), true) => self.start_pack(sources, archive, inside),
            (None, true) => self.start_transfer(sources, value, fsops::spawn_copy, "copy"),
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
    fn run_panelize(&mut self, command: &str) {
        let cwd = self.panels[self.active].local_cwd();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let output = std::process::Command::new(&shell)
            .arg("-c")
            .arg(command)
            .current_dir(&cwd)
            .output();
        match output {
            Ok(out) => {
                let mut entries = Vec::new();
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(mut entry) = entry::stat(&cwd.join(line)) {
                        entry.name = std::ffi::OsString::from(line);
                        entries.push(entry);
                    }
                }
                if entries.is_empty() && !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    let first = err.lines().next().unwrap_or("command failed");
                    self.status = Some(format!(" panelize: {first} "));
                    return;
                }
                let count = entries.len();
                self.panels[self.active].panelize(entries, format!("cmd: {command}"));
                self.status = Some(format!(" panelized {count} item(s) "));
            }
            Err(err) => self.status = Some(format!(" panelize: {err} ")),
        }
    }

    fn open_filter(&mut self) {
        let current = self.panels[self.active]
            .filter
            .clone()
            .unwrap_or_else(|| "*".into());
        self.dialog = Some(Dialog::Input(InputDialog {
            title: " Filter (files matching) ".into(),
            cursor: current.chars().count(),
            value: current,
            action: InputAction::Filter,
        }));
    }

    fn start_transfer(
        &mut self,
        sources: Vec<PathBuf>,
        dest: &str,
        spawn: fn(Vec<PathBuf>, PathBuf) -> JobHandle,
        verb: &str,
    ) {
        let dest = self.resolve(dest);
        self.jobs.push(Job {
            title: format!(" {verb} {} item(s) to {} ", sources.len(), dest.display()),
            handle: spawn(sources, dest),
            total_files: 0,
            total_bytes: 0,
            files_done: 0,
            bytes_done: 0,
            current: PathBuf::new(),
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
            ask: None,
            button: 0,
            src_panel: self.active,
            background: false,
        });
    }

    fn start_delete(&mut self, paths: Vec<PathBuf>, permanent: bool) {
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
            ask: None,
            button: 0,
            src_panel: self.active,
            background: false,
        });
    }

    /// Resolve user input to a normalized path: `~` expands to $HOME,
    /// relative paths are anchored at the active panel's directory.
    fn resolve(&self, input: &str) -> PathBuf {
        let raw = if input == "~" {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
        } else if let Some(rest) = input.strip_prefix("~/") {
            match std::env::var_os("HOME") {
                Some(home) => PathBuf::from(home).join(rest),
                None => PathBuf::from(input),
            }
        } else {
            let path = PathBuf::from(input);
            if path.is_absolute() {
                path
            } else {
                self.panels[self.active].local_cwd().join(path)
            }
        };
        normalize(&raw)
    }

    fn on_panel_key(&mut self, key: KeyEvent) {
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
                KeyCode::Char('s' | 'S') => self.open_symlink(),
                KeyCode::Char('j' | 'J') => self.run_action(Action::Jobs),
                KeyCode::Char('!') => self.run_action(Action::Panelize),
                _ => {}
            }
            return;
        }
        if ctrl && key.code == KeyCode::Char('x') {
            self.prefix_cx = true;
            self.status = Some(
                " C-x  (d = compare, q = quick view, i = info, c = chmod, \
                 o = chown, s = symlink, j = jobs, ! = panelize, \
                 t/p = paste tags/path) "
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

        if edit_line(
            &mut self.cmdline.value,
            &mut self.cmdline.cursor,
            key.code,
            mods,
        ) {
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
        if dir.starts_with("sftp://") {
            self.connect_sftp(dir);
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

    fn describe(paths: &[PathBuf]) -> String {
        if paths.len() == 1 {
            format!(
                "\"{}\"",
                paths[0].file_name().unwrap_or_default().to_string_lossy()
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
        if is_move && self.panels[self.active].archive.is_some() {
            self.status = Some(" archive is read-only ".into());
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
        let mut dest = if other.is_remote() {
            other.display_path()
        } else if other.is_local() || is_move {
            other.local_cwd().display().to_string()
        } else {
            other.display_path()
        };
        if !dest.ends_with('/') {
            dest.push('/');
        }
        let action = if is_move {
            InputAction::MoveTo {
                sources: sources.clone(),
            }
        } else {
            InputAction::CopyTo {
                sources: sources.clone(),
            }
        };
        self.dialog = Some(Dialog::Input(InputDialog {
            title: format!(" {verb} {} to: ", Self::describe(&sources)),
            cursor: dest.chars().count(),
            value: dest,
            action,
        }));
    }

    /// S-F5 / S-F6: copy or rename the cursor file without leaving the
    /// directory - the dialog prefills the bare name for editing.
    fn open_transfer_here(&mut self, is_move: bool) {
        if !self.require_local() {
            return;
        }
        let panel = &self.panels[self.active];
        let Some(entry) = panel.selected().filter(|e| !e.is_parent()) else {
            self.status = Some(" nothing selected ".into());
            return;
        };
        let name = entry.name.to_string_lossy().into_owned();
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
            self.editor = Some(EditorState {
                hl: rcmd_edit::Highlighter::new(&path, 0),
                ed: rcmd_edit::Editor::create(&path),
                title,
                top: 0,
                top_seg: 0,
                left: 0,
                wrap: false,
                rows: 1,
                cols: 1,
                prompt: None,
                note: None,
            });
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
        let mode = self.panels[self.active]
            .selected()
            .map(|e| format!("{:o}", e.mode))
            .unwrap_or_else(|| "644".into());
        self.dialog = Some(Dialog::Input(InputDialog {
            title: format!(" Chmod {} (octal) ", Self::describe(&paths)),
            cursor: mode.chars().count(),
            value: mode,
            action: InputAction::Chmod { paths },
        }));
    }

    /// C-x o: chown on the marked entries (or the cursor entry).
    fn open_chown(&mut self) {
        let Some(paths) = self.writable_targets() else {
            return;
        };
        self.dialog = Some(Dialog::Input(InputDialog {
            title: format!(" Chown {} (user[:group]) ", Self::describe(&paths)),
            value: String::new(),
            cursor: 0,
            action: InputAction::Chown { paths },
        }));
    }

    /// C-x s: create a symlink to the cursor entry.
    fn open_symlink(&mut self) {
        let panel = &self.panels[self.active];
        if panel.fs.writer().is_none() {
            self.status = Some(" archive is read-only ".into());
            return;
        }
        let Some(entry) = panel.selected().filter(|e| !e.is_parent()) else {
            self.status = Some(" nothing selected ".into());
            return;
        };
        let target = PathBuf::from(&entry.name);
        let value = format!("{}-link", entry.name.to_string_lossy());
        self.dialog = Some(Dialog::Input(InputDialog {
            title: format!(" Symlink to \"{}\" named: ", entry.name.to_string_lossy()),
            cursor: value.chars().count(),
            value,
            action: InputAction::SymlinkTo { target },
        }));
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
        if self.panels[self.active].archive.is_some() {
            self.status = Some(" archive is read-only ".into());
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
        self.dialog = Some(Dialog::Input(InputDialog {
            title: if mark {
                " Select group "
            } else {
                " Unselect group "
            }
            .into(),
            value: "*".into(),
            cursor: 1,
            action: InputAction::SelectGlob { mark },
        }));
    }

    fn open_delete(&mut self, permanent: bool) {
        let panel = &self.panels[self.active];
        if panel.archive.is_some() {
            self.status = Some(" archive is read-only ".into());
            return;
        }
        // no remote trash: F8 on an SFTP panel deletes permanently
        let permanent = permanent || panel.is_remote();
        let paths = panel.targets();
        if paths.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let what = Self::describe(&paths);
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
        }));
    }

    /// Yes on a confirm dialog: do whatever it was asking about.
    fn confirm_yes(&mut self, d: ConfirmDialog) {
        match d.kind {
            ConfirmKind::Delete => self.start_delete(d.paths, d.permanent),
            ConfirmKind::Quit => self.quit = true,
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

fn viewer_search(v: &mut Viewer, from: usize, is_next: bool) {
    match v.file.search_from(from, &v.search) {
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
/// screen column `target` (8-wide tab stops), for mouse clicks.
fn col_at_screen(text: &str, target: usize) -> usize {
    let mut screen = 0;
    for (i, ch) in text.chars().enumerate() {
        let width = if ch == '\t' { 8 - screen % 8 } else { 1 };
        if screen + width > target {
            return i;
        }
        screen += width;
    }
    text.chars().count()
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
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
                OPTION_ROWS[d.cursor].opt().is_some(),
                "landed on a heading at row {}",
                d.cursor
            );
        }
        assert!(seen >= 1, "never reached the button row");
        // and the same walking backwards
        for _ in 0..OPTION_ROWS.len() * 2 {
            d.step(-1);
            assert!(d.cursor == OPTION_ROWS.len() || OPTION_ROWS[d.cursor].opt().is_some());
        }
    }

    #[test]
    fn options_toggle_flips_only_the_focused_setting() {
        let mut d = OptionsDialog {
            cursor: 1, // first setting: Show hidden files
            values: [false; OPT_COUNT],
            ok: true,
        };
        d.toggle();
        assert!(d.get(Opt::Hidden));
        assert!(!d.get(Opt::Lynx));
        d.cursor = 0; // a heading: toggling does nothing
        d.toggle();
        assert_eq!(d.values.iter().filter(|v| **v).count(), 1);
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
