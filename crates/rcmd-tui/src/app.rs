use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use ratatui::DefaultTerminal;
use rcmd_core::fsops::{self, JobEvent, JobHandle, Reply};
use rcmd_core::panel::{Panel, SortKey};
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
    pub top: usize,
    pub left: usize,
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
            config,
            keymap,
            pending_exec: None,
            quit: false,
        })
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            self.drain_job();
            terminal.draw(|frame| ui::draw(frame, self))?;
            let timeout = if self.job.is_some() {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(500)
            };
            if event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.on_key(key);
                    }
                }
            }
            if let Some(exec) = self.pending_exec.take() {
                self.execute(terminal, exec)?;
            }
        }
        if let Some(job) = &self.job {
            job.handle.cancel();
        }
        Ok(())
    }

    /// Leave the TUI, run a command or an interactive shell in the active
    /// panel's directory, then restore the TUI and reload both panels.
    fn execute(&mut self, terminal: &mut DefaultTerminal, exec: Exec) -> Result<()> {
        use std::io::Write as _;
        use std::os::unix::process::CommandExt as _;

        let cwd = self.panels[self.active].local_cwd();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        ratatui::restore();
        // The child must own Ctrl+C while it runs; restore our disposition after.
        let old_sigint = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
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
                Ok(())
            });
        }
        match command.status() {
            Ok(status) if !status.success() => println!("[{status}]"),
            Ok(_) => {}
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
                if let Some(viewer) = self.viewer.take() {
                    if let Some(temp) = viewer.temp {
                        let _ = std::fs::remove_file(temp);
                    }
                }
            }
            KeyCode::F(4) => v.hex = !v.hex,
            KeyCode::Up => viewer_scroll(v, -1, rows),
            KeyCode::Down => viewer_scroll(v, 1, rows),
            KeyCode::PageUp => viewer_scroll(v, -page, rows),
            KeyCode::PageDown => viewer_scroll(v, page, rows),
            KeyCode::Home => {
                v.top = 0;
                v.left = 0;
                v.hex_top = 0;
            }
            KeyCode::End => viewer_end(v, rows),
            KeyCode::Left => v.left = v.left.saturating_sub(8),
            KeyCode::Right => v.left += 8,
            KeyCode::F(7) | KeyCode::Char('/') => {
                v.prompt = Some((v.search.clone(), v.search.chars().count()));
            }
            KeyCode::Char('n') => {
                if !v.search.is_empty() {
                    let from = v.found.map(|f| f + 1).unwrap_or(v.top);
                    viewer_search(v, from, true);
                }
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
                    top: 0,
                    left: 0,
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
        }
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
                if self.panels[self.active].is_local() {
                    self.start_transfer(sources, &value, fsops::spawn_copy, "copy")
                } else {
                    self.start_extract(sources, &value)
                }
            }
            InputAction::MoveTo { sources } => {
                self.start_transfer(sources, &value, fsops::spawn_move, "move")
            }
            InputAction::Mkdir => {
                let path = self.resolve(&value);
                match std::fs::create_dir_all(&path) {
                    Ok(()) => {
                        for panel in &mut self.panels {
                            let _ = panel.reload();
                        }
                        let panel = &mut self.panels[self.active];
                        if path.parent() == Some(panel.cwd.as_path()) {
                            if let Some(name) = path.file_name() {
                                if let Some(pos) = panel.entries.iter().position(|e| e.name == name)
                                {
                                    panel.cursor = pos;
                                }
                            }
                        }
                    }
                    Err(err) => self.status = Some(format!(" mkdir: {err} ")),
                }
            }
            InputAction::SelectGlob { mark } => self.panels[self.active].mark_glob(&value, mark),
            InputAction::Filter => unreachable!("handled above"),
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
        let mut dest = self.panels[self.active ^ 1]
            .local_cwd()
            .display()
            .to_string();
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

fn viewer_scroll(v: &mut Viewer, delta: isize, rows: usize) {
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
        v.top = total.saturating_sub(rows);
    }
}

fn viewer_search(v: &mut Viewer, from: usize, is_next: bool) {
    match v.file.search_from(from, &v.search) {
        Ok(Some(idx)) => {
            v.found = Some(idx);
            v.top = idx.saturating_sub(2);
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
