use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use ratatui::DefaultTerminal;
use rcmd_core::fsops::{self, JobEvent, JobHandle, Reply};
use rcmd_core::panel::{Panel, SortKey};

use crate::ui;

pub enum InputAction {
    CopyTo { sources: Vec<PathBuf> },
    MoveTo { sources: Vec<PathBuf> },
    Mkdir,
    SelectGlob { mark: bool },
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

pub struct App {
    pub panels: [Panel; 2],
    pub table_states: [TableState; 2],
    pub active: usize,
    pub status: Option<String>,
    /// Rows visible inside a panel; updated on every draw, drives PgUp/PgDn.
    pub panel_rows: usize,
    pub dialog: Option<Dialog>,
    pub job: Option<Job>,
    pub quit: bool,
}

impl App {
    pub fn new() -> Result<Self> {
        let cwd = std::env::current_dir().context("cannot determine current directory")?;
        let left = Panel::new(cwd.clone())
            .with_context(|| format!("cannot read directory {}", cwd.display()))?;
        let right = Panel::new(cwd.clone())
            .with_context(|| format!("cannot read directory {}", cwd.display()))?;
        Ok(App {
            panels: [left, right],
            table_states: [TableState::default(), TableState::default()],
            active: 0,
            status: None,
            panel_rows: 1,
            dialog: None,
            job: None,
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
        }
        if let Some(job) = &self.job {
            job.handle.cancel();
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
        } else {
            self.on_panel_key(key);
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
                    edit_input(&mut d, code, key.modifiers);
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
        }
    }

    fn submit_input(&mut self, dialog: InputDialog) {
        let value = dialog.value.trim().to_string();
        if value.is_empty() {
            return;
        }
        match dialog.action {
            InputAction::CopyTo { sources } => {
                self.start_transfer(sources, &value, fsops::spawn_copy, "copy")
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
        }
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

    /// Resolve dialog input to a path: `~` expands to $HOME, relative paths
    /// are anchored at the active panel's directory.
    fn resolve(&self, input: &str) -> PathBuf {
        if input == "~" {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home);
            }
        } else if let Some(rest) = input.strip_prefix("~/") {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home).join(rest);
            }
        }
        let path = PathBuf::from(input);
        if path.is_absolute() {
            path
        } else {
            self.panels[self.active].cwd.join(path)
        }
    }

    fn on_panel_key(&mut self, key: KeyEvent) {
        let mods = key.modifiers;
        let alt = mods.contains(KeyModifiers::ALT);
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let page = self.panel_rows.saturating_sub(1).max(1);
        match key.code {
            KeyCode::F(10) => self.quit = true,
            KeyCode::Char('q') if mods.is_empty() => self.quit = true,
            KeyCode::Tab | KeyCode::BackTab => self.active ^= 1,
            KeyCode::Up => self.panel().move_up(),
            KeyCode::Down => self.panel().move_down(),
            KeyCode::Home => self.panel().move_top(),
            KeyCode::End => self.panel().move_bottom(),
            KeyCode::PageUp => self.panel().page_up(page),
            KeyCode::PageDown => self.panel().page_down(page),
            KeyCode::Enter => self.fallible(|p| p.enter()),
            KeyCode::Backspace => self.fallible(|p| p.go_up()),
            KeyCode::Insert => self.panel().toggle_mark(),
            KeyCode::Char('t') if ctrl => self.panel().toggle_mark(),
            KeyCode::Char('r') if ctrl => self.fallible(|p| p.reload().map(|()| true)),
            KeyCode::Char('.') if alt => self.fallible(|p| p.toggle_hidden().map(|()| true)),
            KeyCode::Char('n') if alt => self.panel().set_sort(SortKey::Name),
            KeyCode::Char('e') if alt => self.panel().set_sort(SortKey::Ext),
            KeyCode::Char('s') if alt => self.panel().set_sort(SortKey::Size),
            KeyCode::Char('t') if alt => self.panel().set_sort(SortKey::Mtime),
            KeyCode::Char('+') if !ctrl && !alt => self.open_select(true),
            KeyCode::Char('-') if !ctrl && !alt => self.open_select(false),
            KeyCode::Char('\\') if !ctrl && !alt => self.open_select(false),
            KeyCode::Char('*') if !ctrl && !alt => self.panel().invert_marks(),
            KeyCode::F(5) => self.open_transfer(false),
            KeyCode::F(6) => self.open_transfer(true),
            KeyCode::F(7) => self.open_mkdir(),
            KeyCode::F(8) => self.open_delete(mods.contains(KeyModifiers::SHIFT)),
            KeyCode::F(20) => self.open_delete(true), // Shift+F8 on legacy terminals
            _ => {}
        }
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

    fn open_transfer(&mut self, is_move: bool) {
        let sources = self.panels[self.active].targets();
        if sources.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let verb = if is_move { "Move" } else { "Copy" };
        let mut dest = self.panels[self.active ^ 1].cwd.display().to_string();
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
        let paths = self.panels[self.active].targets();
        if paths.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let what = Self::describe(&paths);
        let (title, message) = if permanent {
            (" Delete ".into(), format!("Permanently delete {what}?"))
        } else {
            (" Delete ".into(), format!("Move {what} to trash?"))
        };
        self.dialog = Some(Dialog::Confirm(ConfirmDialog {
            title,
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

fn edit_input(d: &mut InputDialog, code: KeyCode, mods: KeyModifiers) {
    match code {
        KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
            d.value.clear();
            d.cursor = 0;
        }
        KeyCode::Char(c)
            if !mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT) =>
        {
            d.value.insert(byte_index(&d.value, d.cursor), c);
            d.cursor += 1;
        }
        KeyCode::Backspace => {
            if d.cursor > 0 {
                d.cursor -= 1;
                d.value.remove(byte_index(&d.value, d.cursor));
            }
        }
        KeyCode::Delete => {
            let idx = byte_index(&d.value, d.cursor);
            if idx < d.value.len() {
                d.value.remove(idx);
            }
        }
        KeyCode::Left => d.cursor = d.cursor.saturating_sub(1),
        KeyCode::Right => d.cursor = (d.cursor + 1).min(d.value.chars().count()),
        KeyCode::Home => d.cursor = 0,
        KeyCode::End => d.cursor = d.value.chars().count(),
        _ => {}
    }
}

fn byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}
