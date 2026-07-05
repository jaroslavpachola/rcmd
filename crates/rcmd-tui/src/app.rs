use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::Watcher as _;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
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
use crate::{config, git, keymap, ui};

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
}

/// An SFTP connection attempt on its worker thread; `ask` is the
/// interactive question currently shown (host key / password).
pub struct ConnectState {
    handle: sftp::ConnectHandle,
    panel: usize,
    pub ask: Option<ConnectAsk>,
}

pub enum ConnectAsk {
    HostKey { fingerprint: String, yes: bool },
    Password { prompt: String, value: String },
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
    /// 0 = name field, 1 = content field.
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
}

pub enum Dialog {
    Input(InputDialog),
    Confirm(ConfirmDialog),
    /// Directory hotlist; the payload is the selected row.
    Hotlist(usize),
    Find(FindDialog),
    /// F2 user menu ([[commands]]); the payload is the selected row.
    UserMenu(usize),
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
}

enum Exec {
    Command(String),
    /// Like Command, but without the "Press Enter" pause — for editors
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
    /// Horizontal scroll in screen columns.
    pub left: usize,
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
    /// Content rows; updated on every draw, drives paging.
    pub rows: usize,
}

/// Full-screen F3 viewer state; the chunked file access lives in
/// [`FileView`], this is only presentation state.
pub struct Viewer {
    pub file: FileView,
    pub path: PathBuf,
    pub hex: bool,
    /// Soft-wrap long lines (F2) instead of horizontal scrolling.
    pub wrap: bool,
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
    OtherSameDir,
    OtherOpenDir,
    Reload,
    SwapPanels,
    ToggleHidden,
    Sort(SortKey),
    SortReverse,
}

/// None = separator line.
pub type MenuEntry = Option<(&'static str, &'static str, Action)>;

pub const MENUS: &[(&str, &[MenuEntry])] = &[
    (
        "File",
        &[
            Some(("View", "F3", Action::View)),
            Some(("Edit", "F4", Action::Edit)),
            Some(("Copy...", "F5", Action::Copy)),
            Some(("Move/rename...", "F6", Action::Move)),
            Some(("Make directory...", "F7", Action::Mkdir)),
            Some(("Delete (trash)", "F8", Action::Delete)),
            Some(("Delete permanently", "S-F8", Action::DeletePerm)),
            None,
            Some(("Select group...", "+", Action::SelectGroup)),
            Some(("Unselect group...", "-", Action::UnselectGroup)),
            Some(("Invert selection", "*", Action::InvertSelection)),
            None,
            Some(("Directory size", "C-spc", Action::DirSize)),
            Some(("Filter files...", "C-f", Action::Filter)),
            None,
            Some(("Quit", "F10", Action::Quit)),
        ],
    ),
    (
        "Command",
        &[
            Some(("Help", "F1", Action::Help)),
            Some(("User menu...", "F2", Action::UserMenu)),
            Some(("Quick search", "C-s", Action::QuickSearch)),
            Some(("Directory hotlist...", "C-\\", Action::Hotlist)),
            Some(("Find file...", "M-F7", Action::FindFile)),
            Some(("Panelize command...", "", Action::Panelize)),
            Some(("Compare directories", "C-x d", Action::CompareDirs)),
            Some(("SFTP link...", "", Action::SftpLink)),
            Some(("Open shell", "C-o", Action::Shell)),
            Some(("Reload panel", "C-r", Action::Reload)),
            Some(("Swap panels", "", Action::SwapPanels)),
            Some(("Toggle hidden files", "M-.", Action::ToggleHidden)),
        ],
    ),
    (
        "Sort",
        &[
            Some(("By name", "M-n", Action::Sort(SortKey::Name))),
            Some(("By extension", "M-e", Action::Sort(SortKey::Ext))),
            Some(("By size", "M-s", Action::Sort(SortKey::Size))),
            Some(("By modify time", "M-t", Action::Sort(SortKey::Mtime))),
            None,
            Some(("Toggle reverse", "", Action::SortReverse)),
        ],
    ),
    (
        "View",
        &[
            Some(("Brief listing", "", Action::Listing(ListMode::Brief))),
            Some(("Full listing", "", Action::Listing(ListMode::Full))),
            Some(("Long listing", "", Action::Listing(ListMode::Long))),
            None,
            Some(("Quick view", "C-x q", Action::QuickView)),
            Some(("Info panel", "C-x i", Action::InfoView)),
            None,
            Some(("Other panel: same dir", "M-i", Action::OtherSameDir)),
            Some(("Other panel: this dir", "M-o", Action::OtherOpenDir)),
        ],
    ),
];

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
    pub job: Option<Job>,
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
    du: Option<DuJob>,
    watch: Option<WatchState>,
    /// Ctrl+X was pressed; the next key completes the chord.
    prefix_cx: bool,
    pub areas: Areas,
    /// Last left-button press, for double-click detection.
    last_click: Option<(Instant, u16, u16)>,
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
        let (mut keymap, keymap_warnings) = keymap::build(&config.keymap, &config.keys);
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
        let watch = if config.watch {
            let (tx, rx) = std::sync::mpsc::channel();
            match notify::recommended_watcher(move |event| {
                let _ = tx.send(event);
            }) {
                Ok(watcher) => Some(WatchState {
                    watcher,
                    rx,
                    watched: [None, None],
                    dirty: [None, None],
                    last: [None, None],
                }),
                Err(err) => {
                    warnings.push(format!("watch disabled: {err}"));
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
        Ok(App {
            panels: [left, right],
            table_states: [TableState::default(), TableState::default()],
            active: 0,
            status,
            panel_rows: 1,
            dialog: None,
            job: None,
            viewer: None,
            editor: None,
            quick_view: None,
            info: None,
            disk: [None, None],
            menu: None,
            help: None,
            cmdline: CmdLine::default(),
            quick_search: None,
            find: None,
            connect: None,
            connections: Vec::new(),
            remote_edit: None,
            du: None,
            watch,
            prefix_cx: false,
            areas: Areas::default(),
            last_click: None,
            git_info: [None, None],
            git_seen: [None, None],
            git_tx,
            git_rx,
            config,
            keymap,
            pending_exec: None,
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
            self.update_quick_view();
            self.git_tick();
            self.disk_tick();
            terminal.draw(|frame| ui::draw(frame, self))?;
            let loading = self.panels.iter().any(Panel::is_loading);
            let watch_pending = self
                .watch
                .as_ref()
                .is_some_and(|w| w.dirty.iter().any(Option::is_some));
            let timeout = if self.job.is_some()
                || self.find.is_some()
                || self.connect.is_some()
                || self.du.is_some()
                || loading
                || watch_pending
            {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(500)
            };
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
                    Event::Mouse(mouse) => self.on_mouse(mouse),
                    _ => {}
                }
            }
            if let Some(exec) = self.pending_exec.take() {
                self.execute(terminal, exec)?;
                self.finish_remote_edit();
            }
        }
        if let Some(job) = &self.job {
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
        if self.job.is_some()
            || self.find.is_some()
            || self.dialog.is_some()
            || self.quick_search.is_some()
        {
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
                self.status = Some(format!(" searching… {} found — Esc cancels ", find.count));
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
            self.status = Some(" bad URL — sftp://[user@]host[:port][/path] ".into());
            return;
        };
        let handle = match self.connection(&url.prefix()) {
            Some(fs) => sftp::spawn_reuse(fs, url),
            None => sftp::spawn_connect(url),
        };
        self.status = Some(format!(
            " connecting to {}… — Esc cancels ",
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
                ConnectEvent::AskPassword { prompt } => {
                    connect.ask = Some(ConnectAsk::Password {
                        prompt,
                        value: String::new(),
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
                    " upload failed: {err} — local copy kept at {} ",
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
                        println!("[rcmd has no job control — resuming]");
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

    fn drain_job(&mut self) {
        let Some(job) = self.job.as_mut() else { return };
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
                    job.ask = Some(Ask::Overwrite { path });
                    job.button = 0;
                }
                JobEvent::AskError { path, message } => {
                    job.ask = Some(Ask::Error { path, message });
                    job.button = 0;
                }
                JobEvent::Done {
                    files_done,
                    skipped,
                    aborted,
                } => done = Some((files_done, skipped, aborted)),
            }
        }
        if let Some((files_done, skipped, aborted)) = done {
            let mut job = self.job.take().expect("job present");
            if let Some(thread) = job.handle.thread.take() {
                let _ = thread.join();
            }
            if !aborted {
                self.panels[job.src_panel].marked.clear();
            }
            for panel in &mut self.panels {
                let _ = panel.reload();
            }
            self.git_refresh();
            self.status = Some(match (aborted, skipped) {
                (true, _) => format!(" aborted — {files_done} item(s) processed "),
                (false, 0) => format!(" done — {files_done} item(s) processed "),
                (false, n) => format!(" done — {files_done} item(s) processed, {n} skipped "),
            });
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        self.status = None;
        if self.job.is_some() {
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
        if self.job.is_some()
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
                let line = (st.top + y as usize - 1).min(st.ed.line_count().saturating_sub(1));
                let col = col_at_screen(&st.ed.line(line), st.left + x as usize);
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
                self.panel_click(side, area, y, double);
                return;
            }
        }
    }

    fn panel_click(&mut self, side: usize, area: Rect, y: u16, double: bool) {
        self.active = side;
        if self.quick_view.as_ref().is_some_and(|q| q.side == side) || self.info == Some(side) {
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
        if self.job.is_some()
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
            _ => {}
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
            Action::View => self.open_viewer(),
            Action::Edit => self.open_editor(),
            Action::Copy => self.open_transfer(false),
            Action::Move => self.open_transfer(true),
            Action::Mkdir => self.open_mkdir(),
            Action::Delete => self.open_delete(false),
            Action::DeletePerm => self.open_delete(true),
            Action::SelectGroup => self.open_select(true),
            Action::UnselectGroup => self.open_select(false),
            Action::InvertSelection => self.panel().invert_marks(),
            Action::Quit => self.quit = true,
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
                    self.status = Some(" no [[commands]] in the config — see F1 ".into());
                } else {
                    self.dialog = Some(Dialog::UserMenu(0));
                }
            }
            Action::UserCommand(i) => self.run_user_command(i),
            Action::Listing(mode) => self.panel().list_mode = mode,
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
            Action::Sort(key) => self.panel().set_sort(key),
            Action::SortReverse => {
                let panel = self.panel();
                panel.sort_reverse = !panel.sort_reverse;
                panel.resort();
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
            qv.note = "no preview here — F3 views remote/archive files".into();
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
    /// `%t` marked files, `%%` a literal percent — all shell-quoted.
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

    fn open_viewer(&mut self) {
        let panel = &self.panels[self.active];
        let Some(entry) = panel.selected() else {
            return;
        };
        if entry.is_dir() {
            self.status = Some(" cannot view a directory ".into());
            return;
        }
        let name = entry.name.clone();
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
                    file,
                    path: title_path,
                    hex: false,
                    wrap: false,
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
                    left: 0,
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
        for panel in &mut self.panels {
            let _ = panel.reload();
        }
        self.git_refresh();
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
                                st.ed.replace_match(m, &replacement);
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
                            st.ed.replace_match(m, &replacement);
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
        let Some(job) = self.job.as_mut() else { return };
        let Some(ask) = &job.ask else {
            if key.code == KeyCode::Esc {
                job.handle.cancel();
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
            Dialog::Confirm(mut d) => match key.code {
                KeyCode::Esc | KeyCode::Char('n') => {}
                KeyCode::Char('y') => self.start_delete(d.paths, d.permanent),
                KeyCode::Enter => {
                    if d.yes {
                        self.start_delete(d.paths, d.permanent);
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
                        }
                        self.dialog = Some(Dialog::Hotlist(selected));
                    }
                    KeyCode::Char('d') => {
                        if selected < len {
                            self.config.hotlist.remove(selected);
                        }
                        let len = self.config.hotlist.len();
                        self.dialog = Some(Dialog::Hotlist(selected.min(len.saturating_sub(1))));
                    }
                    KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                        self.dialog = Some(Dialog::Hotlist(selected));
                    }
                    KeyCode::Down => {
                        if selected + 1 < len {
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
            Dialog::Find(mut d) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.submit_find(d),
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                    d.field ^= 1;
                    self.dialog = Some(Dialog::Find(d));
                }
                code => {
                    let (value, cursor) = if d.field == 0 {
                        (&mut d.name, &mut d.name_cursor)
                    } else {
                        (&mut d.content, &mut d.content_cursor)
                    };
                    edit_line(value, cursor, code, key.modifiers);
                    self.dialog = Some(Dialog::Find(d));
                }
            },
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
        self.find = Some(FindState {
            handle: find::spawn_find(root, pattern, content),
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
        if !self.require_local() {
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
        self.du = Some(DuJob {
            rx: fsops::spawn_dir_size(cwd.join(&name)),
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
        }
    }

    /// Send F5/F6 to the right job for the source panel and the typed
    /// destination: plain copy/move, archive pack/extract, or a
    /// cross-provider transfer when SFTP is on either side.
    fn route_transfer(&mut self, sources: Vec<PathBuf>, value: &str, is_move: bool) {
        let src_panel = &self.panels[self.active];
        let src_archive = src_panel.archive.is_some();
        // sftp:// destination (must match before the zip:// syntax —
        // a URL also contains "://")
        if value.starts_with("sftp://") {
            let Some(url) = SftpUrl::parse(value) else {
                self.status = Some(" bad URL — sftp://[user@]host[:port]/path ".into());
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
                self.status = Some(format!(" not connected — cd {} first ", url.prefix()));
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
        self.job = Some(Job {
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
        self.job = Some(Job {
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
        });
    }

    /// Copy INTO an archive: zip appends in place; tar would need a full
    /// rewrite, so it is refused.
    fn start_pack(&mut self, sources: Vec<PathBuf>, archive: PathBuf, inside: PathBuf) {
        let name = archive.file_name().unwrap_or_default().to_string_lossy();
        if !name.to_lowercase().ends_with(".zip") {
            self.status = Some(" can only copy into .zip archives ".into());
            return;
        }
        self.job = Some(Job {
            title: format!(
                " pack {} item(s) into {} ",
                sources.len(),
                archive.display()
            ),
            handle: fsops::spawn_pack_zip(sources, archive, inside),
            total_files: 0,
            total_bytes: 0,
            files_done: 0,
            bytes_done: 0,
            current: PathBuf::new(),
            ask: None,
            button: 0,
            src_panel: self.active,
        });
    }

    fn start_extract(&mut self, sources: Vec<PathBuf>, dest: &str) {
        let dest = self.resolve(dest);
        let fs = self.panels[self.active].fs.clone();
        self.job = Some(Job {
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
        self.job = Some(Job {
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
                _ => {}
            }
            return;
        }
        if ctrl && key.code == KeyCode::Char('x') {
            self.prefix_cx = true;
            self.status = Some(" C-x  (d = compare dirs, q = quick view, i = info panel) ".into());
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
                        let known = if want == usize::MAX {
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
        // Focused info pane: nothing to scroll — Tab back or quit.
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
            KeyCode::Char('p') if ctrl => {
                self.cmdline.hist_prev();
                return;
            }
            KeyCode::Char('n') if ctrl => {
                self.cmdline.hist_next();
                return;
            }
            _ => {}
        }

        // Action keys via the (config-driven) keymap. Plain characters and
        // Left/Right only qualify while the command line is empty — with
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
            if dir.starts_with("sftp://") {
                let dir = dir.to_string();
                self.connect_sftp(&dir);
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
                self.resolve(dir)
            };
            if let Err(err) = self.panels[self.active].cd(target) {
                self.status = Some(format!(" cd: {err} "));
            }
        } else {
            self.pending_exec = Some(Exec::Command(cmd));
        }
    }

    /// Alt+Enter: append the cursor entry's (shell-quoted) name.
    fn insert_selected_name(&mut self) {
        let Some(entry) = self.panels[self.active].selected() else {
            return;
        };
        let text = format!("{} ", shell_quote(&entry.name.to_string_lossy()));
        let idx = byte_index(&self.cmdline.value, self.cmdline.cursor);
        self.cmdline.value.insert_str(idx, &text);
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
        // virtual path — accepting it uploads / packs into the zip
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
        self.dialog = Some(Dialog::Confirm(ConfirmDialog {
            title: " Delete ".into(),
            message,
            yes: !permanent, // safer default for the irreversible variant
            paths,
            permanent,
        }));
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
