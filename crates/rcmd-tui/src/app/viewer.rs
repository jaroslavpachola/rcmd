use super::*;

impl App {
    pub(super) fn on_viewer_key(&mut self, key: KeyEvent) {
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
                        if let Err(err) = asked.field.remember() {
                            v.note = Some(format!(" could not save state: {err} "));
                        }
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
                _ if dialog.row == VIEW_SEARCH_FIELD => {
                    dialog.field.key(key);
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
                // the last search, with the ones before it behind M-p
                dialog.field = TextField::new(dialog.field.value).with_history("view-search");
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

    /// Follow mode: re-index on growth and stick to the bottom.
    pub(super) fn follow_tick(&mut self) {
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

    pub(super) fn open_viewer(&mut self, raw: bool) {
        self.open_viewer_keeping(raw, ViewKeep::default());
    }

    /// Open the cursor file in the internal viewer, carrying `keep`
    /// over from the viewer this one replaces (F6, C-f / C-b).
    pub(super) fn open_viewer_keeping(&mut self, raw: bool, keep: ViewKeep) {
        let panel = &self.panels[self.active];
        let Some(entry) = panel.selected() else {
            return;
        };
        if entry.is_dir() {
            self.status = Some(" cannot view a directory ".into());
            return;
        }
        let name = entry.name.clone();
        if self.panels[self.active].is_local() {
            let path = self.panels[self.active].local_cwd().join(&name);
            self.note_file(&path);
        }
        let Some((source, source_title, mut temps)) = self.fetch_view_source(&name) else {
            return;
        };
        // anything but a local panel handed back a copy, and a copy is
        // not something to write bytes into
        let scratch = !temps.is_empty();
        // the [[view]] filter is a local-panel thing: its command runs
        // on a path, and an archive member has none until it is fetched
        let filter = self.panels[self.active].is_local().then(|| {
            keep.filter.clone().or_else(|| {
                let dir = self.panels[self.active].cwd.clone();
                let path = dir.join(&name);
                let plain = name.to_string_lossy().into_owned();
                let mut probed: Option<String> = None;
                let mut file_type = || {
                    probed
                        .get_or_insert_with(|| crate::config::file_type_of(&path))
                        .clone()
                };
                self.config
                    .view
                    .iter()
                    .find(|rule| rule.matches(&plain, &dir, &mut file_type))
                    .cloned()
            })
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
                    hex_hit: None,
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
        let fetched =
            crate::scratch::create(&name.to_string_lossy()).and_then(|(mut out, temp)| {
                let copied = panel
                    .fs
                    .open_read(&vpath)
                    .and_then(|mut reader| std::io::copy(&mut reader, &mut out));
                match copied {
                    Ok(_) => Ok(temp),
                    Err(err) => {
                        let _ = std::fs::remove_file(&temp);
                        Err(err)
                    }
                }
            });
        let temp = match fetched {
            Ok(temp) => temp,
            Err(err) => {
                self.status = Some(format!(" view: {err} "));
                return None;
            }
        };
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
        let (mut out, temp) = crate::scratch::create("filtered").map_err(|err| err.to_string())?;
        std::io::Write::write_all(&mut out, &output.stdout).map_err(|err| err.to_string())?;
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
            // an M-! command was asked about this file; the next one
            // gets whatever [[view]] rule its own name matches
            filter: None,
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
}
