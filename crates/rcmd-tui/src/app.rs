use std::path::{Component, Path, PathBuf};
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

enum Exec {
    Command(String),
    Shell,
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
    pub cmdline: CmdLine,
    pending_exec: Option<Exec>,
    pub quit: bool,
}

impl App {
    pub fn new(dirs: &[PathBuf]) -> Result<Self> {
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
        let left = Panel::new(left_dir.clone())
            .with_context(|| format!("cannot read directory {}", left_dir.display()))?;
        let right = Panel::new(right_dir.clone())
            .with_context(|| format!("cannot read directory {}", right_dir.display()))?;
        Ok(App {
            panels: [left, right],
            table_states: [TableState::default(), TableState::default()],
            active: 0,
            status: None,
            panel_rows: 1,
            dialog: None,
            job: None,
            cmdline: CmdLine::default(),
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

        let cwd = self.panels[self.active].cwd.clone();
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
                self.panels[self.active].cwd.join(path)
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
        match key.code {
            KeyCode::F(10) => self.quit = true,
            KeyCode::Tab | KeyCode::BackTab => self.active ^= 1,
            KeyCode::Up => self.panel().move_up(),
            KeyCode::Down => self.panel().move_down(),
            KeyCode::PageUp => self.panel().page_up(page),
            KeyCode::PageDown => self.panel().page_down(page),
            KeyCode::Enter if alt => self.insert_selected_name(),
            KeyCode::Enter if cmd_empty => self.fallible(|p| p.enter()),
            KeyCode::Enter => self.submit_command(),
            KeyCode::Esc if !cmd_empty => {
                self.cmdline.value.clear();
                self.cmdline.cursor = 0;
                self.cmdline.hist_pos = None;
            }
            KeyCode::Insert => self.panel().toggle_mark(),
            KeyCode::Char('o') if ctrl => self.pending_exec = Some(Exec::Shell),
            KeyCode::Char('t') if ctrl => self.panel().toggle_mark(),
            KeyCode::Char('r') if ctrl => self.fallible(|p| p.reload().map(|()| true)),
            KeyCode::Char('p') if ctrl => self.cmdline.hist_prev(),
            KeyCode::Char('n') if ctrl => self.cmdline.hist_next(),
            KeyCode::Char('.') if alt => self.fallible(|p| p.toggle_hidden().map(|()| true)),
            KeyCode::Char('n') if alt => self.panel().set_sort(SortKey::Name),
            KeyCode::Char('e') if alt => self.panel().set_sort(SortKey::Ext),
            KeyCode::Char('s') if alt => self.panel().set_sort(SortKey::Size),
            KeyCode::Char('t') if alt => self.panel().set_sort(SortKey::Mtime),
            KeyCode::Char('+') if cmd_empty && !ctrl && !alt => self.open_select(true),
            KeyCode::Char('-') if cmd_empty && !ctrl && !alt => self.open_select(false),
            KeyCode::Char('\\') if cmd_empty && !ctrl && !alt => self.open_select(false),
            KeyCode::Char('*') if cmd_empty && !ctrl && !alt => self.panel().invert_marks(),
            KeyCode::Home if cmd_empty => self.panel().move_top(),
            KeyCode::End if cmd_empty => self.panel().move_bottom(),
            KeyCode::Backspace if cmd_empty => self.fallible(|p| p.go_up()),
            KeyCode::F(5) => self.open_transfer(false),
            KeyCode::F(6) => self.open_transfer(true),
            KeyCode::F(7) => self.open_mkdir(),
            KeyCode::F(8) => self.open_delete(mods.contains(KeyModifiers::SHIFT)),
            KeyCode::F(20) => self.open_delete(true), // Shift+F8 on legacy terminals
            code => {
                if edit_line(
                    &mut self.cmdline.value,
                    &mut self.cmdline.cursor,
                    code,
                    mods,
                ) {
                    self.cmdline.hist_pos = None;
                }
            }
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
