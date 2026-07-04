use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::Watcher as _;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use rcmd_core::entry;
use rcmd_core::find::{self, FindEvent, FindHandle};
use rcmd_core::fsops::{self, JobEvent, JobHandle, Reply};
use rcmd_core::panel::{LoadKind, Panel, SortKey};
use rcmd_core::view::FileView;

use crate::config::{Config, HotEntry};
use crate::keymap::Keymap;
use crate::{config, keymap, ui};

pub enum InputAction {
    CopyTo { sources: Vec<PathBuf> },
    MoveTo { sources: Vec<PathBuf> },
    Mkdir,
    SelectGlob { mark: bool },
    Filter,
    Panelize,
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
    Editor(String),
    Shell,
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
            Some(("Quick search", "C-s", Action::QuickSearch)),
            Some(("Directory hotlist...", "C-\\", Action::Hotlist)),
            Some(("Find file...", "M-F7", Action::FindFile)),
            Some(("Panelize command...", "", Action::Panelize)),
            Some(("Compare directories", "C-x d", Action::CompareDirs)),
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
    pub menu: Option<MenuState>,
    pub help: Option<HelpState>,
    pub cmdline: CmdLine,
    /// Quick-search prefix while Ctrl+S type-ahead is active.
    pub quick_search: Option<String>,
    pub find: Option<FindState>,
    du: Option<DuJob>,
    watch: Option<WatchState>,
    /// Ctrl+X was pressed; the next key completes the chord.
    prefix_cx: bool,
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
            let _ = panel.reload();
        }
        let (keymap, keymap_warnings) = keymap::build(&config.keymap, &config.keys);
        warnings.extend(keymap_warnings);
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
        Ok(App {
            panels: [left, right],
            table_states: [TableState::default(), TableState::default()],
            active: 0,
            status,
            panel_rows: 1,
            dialog: None,
            job: None,
            viewer: None,
            menu: None,
            help: None,
            cmdline: CmdLine::default(),
            quick_search: None,
            find: None,
            du: None,
            watch,
            prefix_cx: false,
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
            self.drain_du();
            self.poll_loads();
            self.update_watches();
            self.tick_watch();
            terminal.draw(|frame| ui::draw(frame, self))?;
            let loading = self.panels.iter().any(Panel::is_loading);
            let watch_pending = self
                .watch
                .as_ref()
                .is_some_and(|w| w.dirty.iter().any(Option::is_some));
            let timeout = if self.job.is_some()
                || self.find.is_some()
                || self.du.is_some()
                || loading
                || watch_pending
            {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(500)
            };
            if event::poll(timeout)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.on_key(key);
            }
            if let Some(exec) = self.pending_exec.take() {
                self.execute(terminal, exec)?;
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

    /// Leave the TUI, run a command or an interactive shell in the active
    /// panel's directory, then restore the TUI and reload both panels.
    fn execute(&mut self, terminal: &mut DefaultTerminal, exec: Exec) -> Result<()> {
        use std::io::Write as _;
        use std::os::unix::process::CommandExt as _;

        let cwd = self.panels[self.active].local_cwd();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
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
            Exec::Editor(cmd) => {
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
        let _ = terminal.clear();
        for panel in &mut self.panels {
            let _ = panel.reload();
        }
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
        } else if self.find.is_some() {
            self.on_find_key(key);
        } else if self.dialog.is_some() {
            self.on_dialog_key(key);
        } else if self.help.is_some() {
            self.on_help_key(key);
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
            Action::Reload => self.fallible(|p| p.reload().map(|()| true)),
            Action::SwapPanels => {
                self.panels.swap(0, 1);
                self.table_states.swap(0, 1);
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
            // extract the archive member to a scratch file first
            let vpath = panel.cwd.join(&name);
            let temp = std::env::temp_dir().join(format!(
                "rcmd-view-{}-{}",
                std::process::id(),
                name.to_string_lossy()
            ));
            let extracted = panel.fs.open_read(&vpath).and_then(|mut reader| {
                let mut out = std::fs::File::create(&temp)?;
                std::io::copy(&mut reader, &mut out)?;
                Ok(())
            });
            if let Err(err) = extracted {
                let _ = std::fs::remove_file(&temp);
                self.status = Some(format!(" view: {err} "));
                return;
            }
            let archive = panel.archive.clone().unwrap_or_default();
            let title = PathBuf::from(format!("{}://{}", archive.display(), vpath.display()));
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
        if !panel.is_local() {
            self.status = Some(" cannot edit inside an archive ".into());
            return;
        }
        let path = panel.cwd.join(&entry.name);
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());
        self.pending_exec = Some(Exec::Editor(format!(
            "{editor} {}",
            shell_quote(&path.to_string_lossy())
        )));
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
                            let target = self.resolve(&entry.path);
                            if let Err(err) = self.panels[self.active].cd(target) {
                                self.status = Some(format!(" hotlist: {err} "));
                            }
                        }
                    }
                    KeyCode::Char('a') => {
                        let cwd = self.panels[self.active].local_cwd();
                        let path = cwd.display().to_string();
                        if !self.config.hotlist.iter().any(|h| h.path == path) {
                            let label = cwd
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "/".into());
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

        if !self.panels[0].is_local() || !self.panels[1].is_local() {
            self.status = Some(" both panels must be local ".into());
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
            InputAction::CopyTo { sources } => {
                match (split_vfs_dest(&value), self.panels[self.active].is_local()) {
                    (Some(_), false) => {
                        self.status = Some(" cannot copy from archive to archive ".into())
                    }
                    (Some((archive, inside)), true) => self.start_pack(sources, archive, inside),
                    (None, true) => self.start_transfer(sources, &value, fsops::spawn_copy, "copy"),
                    (None, false) => self.start_extract(sources, &value),
                }
            }
            InputAction::MoveTo { sources } => {
                if split_vfs_dest(&value).is_some() {
                    self.status = Some(" cannot move into an archive ".into());
                } else {
                    self.start_transfer(sources, &value, fsops::spawn_move, "move")
                }
            }
            InputAction::Mkdir => {
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
        self.job = Some(Job {
            title: format!(" {verb} {} item(s) ", paths.len()),
            handle: fsops::spawn_delete(paths, permanent),
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
            if let KeyCode::Char('d' | 'D') = key.code {
                self.run_action(Action::CompareDirs);
            }
            return;
        }
        if ctrl && key.code == KeyCode::Char('x') {
            self.prefix_cx = true;
            self.status = Some(" C-x  (d = compare directories) ".into());
            return;
        }
        // Structural keys: navigation and command-line plumbing.
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.active ^= 1;
                return;
            }
            KeyCode::Up => {
                self.panel().move_up();
                return;
            }
            KeyCode::Down => {
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
                self.fallible(|p| p.enter());
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
            KeyCode::Left | KeyCode::Right => cmd_empty,
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
            let target = if dir.is_empty() {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/"))
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

    /// Operations that write to the panel's filesystem need a local one.
    fn require_local(&mut self) -> bool {
        if self.panels[self.active].is_local() {
            true
        } else {
            self.status = Some(" archive is read-only ".into());
            false
        }
    }

    fn open_transfer(&mut self, is_move: bool) {
        if is_move && !self.require_local() {
            return;
        }
        let sources = self.panels[self.active].targets();
        if sources.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let verb = if is_move { "Move" } else { "Copy" };
        let other = &self.panels[self.active ^ 1];
        // an archive on the other side prefills its virtual path — accepting
        // it packs into the zip (copy only)
        let mut dest = if other.is_local() || is_move {
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
        if !self.require_local() {
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
        if !self.require_local() {
            return;
        }
        let paths = self.panels[self.active].targets();
        if paths.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let what = Self::describe(&paths);
        let message = if permanent {
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
