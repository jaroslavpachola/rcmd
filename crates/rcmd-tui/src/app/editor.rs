use super::*;

impl App {
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

    pub(super) fn open_editor(&mut self) {
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
        if panel.is_local() {
            let path = panel.local_cwd().join(&entry.name);
            self.note_file(&path);
        }
        let panel = &self.panels[self.active];
        let Some(entry) = panel.selected() else {
            return;
        };
        if panel.is_remote() {
            // edit a scratch copy; upload it back if the editor saved
            let name = entry.name.clone();
            let remote_path = panel.cwd.join(&name);
            let title = format!(
                "{}{}",
                panel.remote.clone().unwrap_or_default(),
                remote_path.display()
            );
            let fetched =
                crate::scratch::create(&name.to_string_lossy()).and_then(|(mut out, temp)| {
                    let copied = panel
                        .fs
                        .open_read(&remote_path)
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
                    self.status = Some(format!(" edit: {err} "));
                    return;
                }
            };
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

    pub(super) fn open_internal_editor(&mut self, path: &Path, title: String) -> bool {
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
                let hl = rcmd_edit::Highlighter::new(path, len);
                // the syntax set is built on first use, so a broken
                // user syntax file is only knowable once something has
                // asked to be highlighted
                let note = rcmd_edit::user_syntax_warning().map(|w| format!(" {w} "));
                self.open_screen(Screen::Editor(Box::new(EditorState {
                    hl,
                    ed,
                    title,
                    top: 0,
                    top_seg: 0,
                    left: 0,
                    wrap: false,
                    rows: 1,
                    cols: 1,
                    prompt: None,
                    note,
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

    pub(super) fn close_editor(&mut self) {
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
            let _ = panel.refresh();
        }
        self.git_refresh();
    }

    /// Bulk rename: marked names (or the cursor entry) become a
    /// numbered buffer in the built-in editor; closing it turns the
    /// diff into a previewed batch of renames and deletes.
    pub(super) fn open_bulk_rename(&mut self) {
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
        let written = crate::scratch::create("rename").and_then(|(mut out, temp)| {
            std::io::Write::write_all(&mut out, buffer.as_bytes()).map(|()| temp)
        });
        let temp = match written {
            Ok(temp) => temp,
            Err(err) => {
                self.status = Some(format!(" bulk rename: {err} "));
                return;
            }
        };
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
    pub(super) fn apply_bulk_rename(&mut self, preview: RenamePreview) {
        if !preview.renames.is_empty() {
            match rcmd_core::rename::apply(&preview.dir, &preview.renames) {
                Ok(()) => {
                    self.status = Some(format!(" renamed {} item(s) ", preview.renames.len()));
                    // a batch that succeeded was final before; it is
                    // the same (from, to) record every move leaves, so
                    // C-x u puts a bulk rename back too
                    self.undo = Some(
                        preview
                            .renames
                            .iter()
                            .map(|(old, new)| (preview.dir.join(old), preview.dir.join(new)))
                            .collect(),
                    );
                }
                Err(err) => self.status = Some(format!(" bulk rename: {err} ")),
            }
        }
        for panel in &mut self.panels {
            let _ = panel.refresh();
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
    pub(super) fn editor_find(&mut self, pattern: &str, from: rcmd_edit::Pos) {
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
    pub(super) fn on_editor_key(&mut self, key: KeyEvent) {
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
            EA::NextHit | EA::PrevHit => {
                self.step_hit(if action == EA::NextHit { 1 } else { -1 });
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
                if self.external_menubar {
                    self.menu_requested = true;
                } else if let Some(st) = self.editor_mut() {
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
                        st.prompt = Some(EditPrompt::Search(search_field("")));
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
            EA::Save
            | EA::Quit
            | EA::SearchNext
            | EA::Menu
            | EA::Goto
            | EA::NextHit
            | EA::PrevHit => {
                unreachable!("handled above")
            }
            EA::Mark => {
                st.ed.toggle_mark();
                edited = false;
            }
            EA::Replace => {
                st.prompt = Some(EditPrompt::ReplaceFind(search_field(&st.ed.search)));
                return;
            }
            EA::Search => {
                st.prompt = Some(EditPrompt::Search(search_field(&st.ed.search)));
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
            EditPrompt::Search(mut field) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let pattern = field.value.trim().to_string();
                    if !pattern.is_empty() {
                        remember_in(st, &field);
                        let from = next_pos(&st.ed);
                        self.editor_find(&pattern, from);
                    }
                }
                _ => {
                    field.key(key);
                    st.prompt = Some(EditPrompt::Search(field));
                }
            },
            EditPrompt::ReplaceFind(mut field) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let pattern = field.value.trim().to_string();
                    if !pattern.is_empty() {
                        remember_in(st, &field);
                        st.ed.search = pattern.clone();
                        st.prompt = Some(EditPrompt::ReplaceWith {
                            pattern,
                            field: TextField::new("").with_history("edit-replace"),
                        });
                    }
                }
                _ => {
                    field.key(key);
                    st.prompt = Some(EditPrompt::ReplaceFind(field));
                }
            },
            EditPrompt::ReplaceWith { pattern, mut field } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    remember_in(st, &field);
                    let value = field.value;
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
                _ => {
                    field.key(key);
                    st.prompt = Some(EditPrompt::ReplaceWith { pattern, field });
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
        if let Some(action) = run {
            self.run_edit_menu_action(action);
        }
    }

    pub(super) fn run_edit_menu_action(&mut self, action: EditMenuAction) {
        match action {
            EditMenuAction::Key(action) => self.editor_action(action),
            EditMenuAction::Options => self.open_edit_options(),
            EditMenuAction::ScreenList => self.open_screen_list(),
            EditMenuAction::Syntax => self.open_syntax_picker(),
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
    pub(super) fn ensure_editor_visible(&mut self) {
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
}

/// The editor's search field: the last search, with the ring of the
/// ones before it behind M-p.
fn search_field(last: &str) -> TextField {
    TextField::new(last).with_history("edit-search")
}

/// Keep what was asked, saying so on the editor's own note line when
/// the state file cannot be written.
fn remember_in(st: &mut EditorState, field: &TextField) {
    if let Err(err) = field.remember() {
        st.note = Some(format!(" could not save state: {err} "));
    }
}
