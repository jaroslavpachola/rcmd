use super::*;

/// How a flat view's listing is labelled, and known again.
const FLAT: &str = "flat:";

impl App {
    pub(super) fn drain_find(&mut self) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let window = find.window;
        let mut done = None;
        let mut found: Vec<Box<find::Found>> = Vec::new();
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
            for hit in found {
                results.rows.push(FindRow {
                    path: results.root.join(&hit.entry.name),
                    inside: hit
                        .inside
                        .map(|(archive, inner)| (results.root.join(archive), inner)),
                    hit: hit.hit,
                    marked: false,
                });
            }
            results.done = done;
        } else {
            // a listing holds each file once, however many lines hit;
            // a file's hits arrive one after the other
            // a listing is of one filesystem: a member of an archive
            // is only in the results window, which can go into it
            for hit in found.into_iter().filter(|hit| hit.inside.is_none()) {
                let panel = &mut self.panels[panel];
                if hit.mark {
                    panel.marked.insert(hit.entry.name.clone());
                }
                if panel
                    .entries
                    .last()
                    .is_none_or(|last| last.name != hit.entry.name)
                {
                    panel.entries.push(hit.entry);
                }
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
        let target = d.rows.get(d.selected).map(|row| row.path.clone());
        let hit = d.rows.get(d.selected).and_then(|row| row.hit.clone());
        let inside = d.rows.get(d.selected).and_then(|row| row.inside.clone());
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
                let mut paths: Vec<&PathBuf> = Vec::new();
                for row in &d.rows {
                    if !paths.contains(&&row.path) {
                        paths.push(&row.path);
                    }
                }
                let entries: Vec<_> = paths
                    .into_iter()
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
            ("Chdir", Some(_)) if inside.is_some() => {
                let side = self.active;
                self.close_find();
                self.go_into_archive(side, inside.expect("checked"));
            }
            ("View", Some(_)) | ("Edit", Some(_)) if inside.is_some() => {
                let side = self.active;
                self.close_find();
                // the result walk steps from file to file on disk; a
                // member of an archive is read where it is, once
                self.hit_walk = None;
                if self.go_into_archive(side, inside.expect("checked")) {
                    match FIND_BUTTONS[button] {
                        "Edit" => self.open_editor(),
                        _ => self.open_viewer(false),
                    }
                    if let Some(hit) = hit {
                        self.go_to_hit(&d.query, hit.line);
                    }
                }
            }
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
                self.hit_walk = Some(HitWalk {
                    rows: d
                        .rows
                        .iter()
                        .map(|row| (row.path.clone(), row.hit.as_ref().map(|hit| hit.line)))
                        .collect(),
                    at: d.selected,
                    query: d.query.clone(),
                });
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
                if let Some(hit) = hit {
                    self.go_to_hit(&d.query, hit.line);
                }
            }
            _ => {}
        }
    }

    /// Put a panel inside an archive, on one of its members. False, with
    /// the reason on the status line, when the archive will not open.
    fn go_into_archive(&mut self, side: usize, (archive, inner): (PathBuf, PathBuf)) -> bool {
        // `..` at the archive's top comes out beside it, wherever the
        // panel was before
        self.panels[side].cancel_pending();
        if let Err(err) = self.panels[side].open_archive(archive.clone()) {
            self.status = Some(format!(" {}: {err} ", archive.display()));
            return false;
        }
        if let Some(dir) = inner.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = self.panels[side].request_dir(dir.to_path_buf(), LoadKind::Enter);
        }
        if let Some(name) = inner.file_name() {
            self.panels[side].select_name(name);
        }
        true
    }

    /// F5, F6 or F8 from the results window: the marked files, or the
    /// one under the cursor, without panelizing them first.
    pub(super) fn find_operate(&mut self, d: FindResults, key: KeyCode) {
        let targets = d.targets();
        if targets.is_empty() {
            self.status = Some(" a member of an archive - Chdir goes into it ".into());
            self.dialog = Some(Dialog::FindResults(Box::new(d)));
            return;
        }
        self.close_find();
        match key {
            KeyCode::F(5) => self.open_transfer_of(false, targets),
            KeyCode::F(6) => self.open_transfer_of(true, targets),
            _ => self.open_delete_of(false, targets),
        }
    }

    /// M-. / M-, in the viewer or the editor: the next or previous row
    /// of the find it came from - further down this file, or the next
    /// file, opened in the same kind of screen.
    pub(super) fn step_hit(&mut self, delta: isize) {
        let edit = self.editor().is_some();
        let note = |app: &mut App, text: String| {
            if let Some(st) = app.editor_mut() {
                st.note = Some(text);
            } else if let Some(v) = app.viewer_mut() {
                v.note = Some(text);
            }
        };
        let Some(walk) = self.hit_walk.as_ref() else {
            note(self, " no find results to walk - Alt+F7 first ".into());
            return;
        };
        let next = walk.at as isize + delta;
        if next < 0 || next as usize >= walk.rows.len() {
            let end = if delta > 0 { "last" } else { "first" };
            note(self, format!(" that was the {end} result "));
            return;
        }
        let (at, total) = (next as usize, walk.rows.len());
        let (path, line) = walk.rows[at].clone();
        let query = walk.query.clone();
        let here = match (self.editor(), self.viewer()) {
            (Some(st), _) => st.ed.path.clone(),
            (None, Some(v)) => v.source.clone(),
            _ => return,
        };
        if here != path {
            // leaving this file: nothing it holds may be lost on the way
            if edit && self.editor().is_some_and(|st| st.ed.modified()) {
                note(
                    self,
                    " save first (F2) - the next result is in another file ".into(),
                );
                return;
            }
            if !edit && self.viewer().is_some_and(|v| !v.hex_edits.is_empty()) {
                note(self, " bytes are still unwritten - F6 writes them ".into());
                return;
            }
            if edit {
                self.close_editor();
            } else {
                self.close_viewer();
            }
            let side = self.active;
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
        if let Some(walk) = self.hit_walk.as_mut() {
            walk.at = at;
        }
        if let Some(line) = line {
            self.go_to_hit(&query, line);
        }
        note(self, format!(" result {} of {total} ", at + 1));
    }

    /// The viewer or the editor just opened on a hit: take it to the
    /// line, with the find's content as its search, so `n` or Shift+F7
    /// goes on to the next one.
    fn go_to_hit(&mut self, query: &FindDialog, line: u64) {
        let text = query.content.value.trim().to_string();
        let line = line.saturating_sub(1) as usize;
        let search = ViewSearch {
            field: TextField::new(text.clone()).with_history("view-search"),
            kind: if query.regex {
                SearchKind::Regex
            } else {
                SearchKind::Normal
            },
            case_sensitive: query.case_sensitive,
            whole_word: query.whole_words,
            backwards: false,
            row: 0,
        };
        if let Some(st) = self.editor_mut() {
            st.ed.search = text;
            st.search = search;
            st.ed.goto(rcmd_edit::Pos { line, col: 0 }, false);
            self.editor_search(false);
        } else if let Some(v) = self.viewer_mut() {
            v.search = search;
            viewer_search(v, line, false);
        }
    }

    pub(super) fn submit_find(&mut self, dialog: FindDialog) {
        for i in 0..FIND_FIELDS {
            if let Some(field) = dialog.field_at(i) {
                self.remember(field);
            }
        }
        let memory = dialog.memory();
        if let Err(err) = state::update(move |s| s.find = Some(memory)) {
            self.status = Some(format!(" could not save state: {err} "));
        }
        let max_depth = match (dialog.recursive, dialog.depth.value.trim()) {
            (false, _) => Some(1),
            (true, "") => None,
            (true, typed) => match typed.parse::<usize>() {
                Ok(depth) if depth > 0 => Some(depth),
                _ => {
                    self.status = Some(format!(" max depth: {typed} is not a number of levels "));
                    self.dialog = Some(Dialog::Find(Box::new(dialog)));
                    return;
                }
            },
        };
        let text = dialog.name.value.trim();
        let name = rcmd_core::pattern::Pattern {
            text: if text.is_empty() { "*" } else { text }.to_string(),
            shell: dialog.shell,
            case_sensitive: dialog.name_case,
            files_only: false,
            size: dialog.size.value.trim().to_string(),
            newer: dialog.newer.value.trim().to_string(),
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
            first_hit: dialog.first_hit,
            max_depth,
            ignore_dirs: find::parse_ignore_dirs(&dialog.ignore.value),
            archives: dialog.archives,
            files_only: false,
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

    /// Every file under the panel's directory, as one listing: a find
    /// for any name, files only, streamed into the panel the way a
    /// panelizing find is. On a flat view already, the directory again.
    pub(super) fn flat_view(&mut self) {
        let side = self.active;
        if self.panels[side]
            .panelized
            .as_deref()
            .is_some_and(|label| label.starts_with(FLAT))
        {
            self.fallible(|p| p.reload().map(|()| true));
            return;
        }
        if self.find.is_some() || !self.require_local() {
            return;
        }
        let root = self.panels[side].local_cwd();
        let query = find::Query {
            name: rcmd_core::pattern::Pattern {
                text: "*".into(),
                shell: true,
                ..rcmd_core::pattern::Pattern::default()
            },
            skip_hidden: !self.panels[side].show_hidden,
            files_only: true,
            ..find::Query::default()
        };
        let handle = match find::spawn_find(root.clone(), query, None) {
            Ok(handle) => handle,
            Err(err) => {
                self.status = Some(format!(" {err} "));
                return;
            }
        };
        self.panels[side].panelize(Vec::new(), format!("{FLAT} {}", root.display()));
        self.find = Some(FindState {
            handle,
            panel: side,
            count: 0,
            window: false,
        });
    }

    /// The files under the panel's directory that have a twin, group by
    /// group, biggest first, streamed into the panel - every copy but
    /// the first marked, so F8 keeps one of each.
    pub(super) fn find_duplicates(&mut self) {
        if self.find.is_some() || !self.require_local() {
            return;
        }
        let side = self.active;
        let root = self.panels[side].local_cwd();
        let handle =
            rcmd_core::dupes::spawn_duplicates(root.clone(), !self.panels[side].show_hidden);
        // the label is short: the marked total shares the bottom edge
        self.panels[side].panelize(Vec::new(), "duplicates".into());
        self.find = Some(FindState {
            handle,
            panel: side,
            count: 0,
            window: false,
        });
    }

    pub(super) fn open_find(&mut self) {
        if !self.require_local() {
            return;
        }
        let start = self.panels[self.active].local_cwd().display().to_string();
        // the last question, from this session or the one before
        let last = state::load().0.find.unwrap_or_default();
        self.dialog = Some(Dialog::Find(Box::new(FindDialog::from_memory(start, last))));
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
