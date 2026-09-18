use super::*;

impl App {
    pub(super) fn drain_find(&mut self) {
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
}

impl App {
    /// While a find streams: Esc cancels, navigation browses the results
    /// as they arrive, everything else waits.
    pub(super) fn on_find_key(&mut self, key: KeyEvent) {
        if matches!(&self.dialog, Some(Dialog::FindResults(_))) {
            self.on_dialog_key(key);
            return;
        }
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

    /// Stop a running find and put its window away.
    pub(super) fn close_find(&mut self) {
        if let Some(find) = self.find.take() {
            find.handle.cancel();
        }
        self.dialog = None;
    }

    /// One of the six things the results window can do with the match
    /// under the cursor. `None` = the focused button.
    pub(super) fn find_button(&mut self, d: FindResults, button: Option<usize>) {
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

    pub(super) fn submit_find(&mut self, dialog: FindDialog) {
        for field in [&dialog.start, &dialog.name, &dialog.content] {
            self.remember(field);
        }
        let memory = dialog.memory();
        if let Err(err) = state::update(move |s| s.find = Some(memory)) {
            self.status = Some(format!(" could not save state: {err} "));
        }
        let text = dialog.name.value.trim();
        let name = rcmd_core::pattern::Pattern {
            text: if text.is_empty() { "*" } else { text }.to_string(),
            shell: dialog.shell,
            case_sensitive: dialog.case_sensitive,
            files_only: false,
            ..Default::default()
        };
        let content = {
            let text = dialog.content.value.trim();
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
        let root = match dialog.start.value.trim() {
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

    pub(super) fn open_find(&mut self) {
        if !self.require_local() {
            return;
        }
        let start = self.panels[self.active].local_cwd().display().to_string();
        // the last question, from this session or the one before
        let last = state::load().0.find.unwrap_or(state::FindMemory {
            name: "*".into(),
            shell: true,
            skip_ignored: true,
            ..Default::default()
        });
        self.dialog = Some(Dialog::Find(Box::new(FindDialog {
            start: TextField::new(start).with_history("find-start"),
            name: TextField::new(last.name).with_history("find-name"),
            content: TextField::new(last.content).with_history("find-content"),
            shell: last.shell,
            case_sensitive: last.case_sensitive,
            whole_words: last.whole_words,
            regex: last.regex,
            all_charsets: last.all_charsets,
            skip_hidden: last.skip_hidden,
            follow_links: last.follow_links,
            skip_ignored: last.skip_ignored,
            row: 1,
            ok: true,
        })));
    }

    pub(super) fn open_panelize(&mut self) {
        if !self.require_local() {
            return;
        }
        self.dialog = Some(Dialog::Panelize(Box::new(PanelizeDialog {
            command: TextField::new("").with_history("panelize"),
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
    pub(super) fn save_panelize(
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

    /// Run a command, its stdout lines become the panel listing.
    /// Synchronous: meant for fast listers (git ls-files, rg -l, …).
    /// Run the command with its output streaming into the panel. A
    /// listing that takes a while to produce - a find, a git command
    /// over a big tree - fills in as it goes rather than after.
    pub(super) fn run_panelize(&mut self, command: &str) {
        use std::io::{BufRead, BufReader};
        use std::sync::atomic::{AtomicBool, Ordering};
        if let Some(running) = self.panelize.take() {
            running.cancel.store(true, Ordering::Relaxed);
        }
        let cwd = self.panels[self.active].local_cwd();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut spawn = std::process::Command::new(&shell);
        spawn
            .arg("-c")
            .arg(command)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(server) = &self.remote {
            spawn.env("RCMD_SOCKET", server.path());
        }
        let child = spawn.spawn();
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
    pub(super) fn drain_panelize(&mut self) {
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
}
