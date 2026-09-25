use super::*;

impl App {
    /// A click on a list dialog's row: it selects, and a double-click
    /// is the Enter that would have followed. A click anywhere else in
    /// the dialog does nothing - closing on a stray click outside would
    /// lose whatever was typed.
    /// A click on a form dialog: a field takes the focus, a switch is
    /// ticked, a button is pressed - what the keyboard would do on that
    /// row. False = the click hit none of those.
    pub(super) fn click_form(&mut self, x: u16, y: u16) -> bool {
        let pos = Position { x, y };
        let Some(&(_, hit)) = self.form_hits.iter().find(|(area, _)| area.contains(pos)) else {
            return false;
        };
        let press = match (self.dialog.as_mut(), hit) {
            (Some(Dialog::Find(d)), FormHit::Row(row)) => {
                d.row = row;
                if (FIND_FIELDS..FIND_ROWS).contains(&row) {
                    d.toggle();
                }
                false
            }
            (Some(Dialog::Find(d)), FormHit::Button(button)) => {
                d.row = FIND_ROWS;
                d.ok = button == 0;
                true
            }
            (Some(Dialog::Transfer(d)), FormHit::Row(row)) => {
                d.row = row;
                if (TRANSFER_DEST_ROW + 1..TRANSFER_ROWS).contains(&row) {
                    d.toggle(row - TRANSFER_DEST_ROW - 1);
                }
                false
            }
            (Some(Dialog::Transfer(d)), FormHit::Button(button)) => {
                d.row = TRANSFER_ROWS;
                d.button = button;
                true
            }
            (Some(Dialog::Pattern(d)), FormHit::Row(row)) => {
                d.row = row;
                if (PATTERN_FIELDS..PATTERN_ROWS).contains(&row) {
                    d.toggle();
                }
                false
            }
            (Some(Dialog::Pattern(d)), FormHit::Button(button)) => {
                d.row = PATTERN_ROWS;
                d.ok = button == 0;
                true
            }
            (Some(Dialog::Link(d)), FormHit::Row(row)) => {
                d.row = row;
                false
            }
            (Some(Dialog::Link(d)), FormHit::Button(button)) => {
                d.row = d.rows();
                d.ok = button == 0;
                true
            }
            // a bit flips where it is clicked, and the octal follows
            (Some(Dialog::Chmod(d)), FormHit::Row(row)) => {
                d.row = row;
                if let Some(&(_, bit)) = CHMOD_BITS.get(row) {
                    d.mode ^= bit;
                    d.sync_octal();
                } else if row == CHMOD_RECURSE_ROW {
                    d.recurse = !d.recurse;
                }
                false
            }
            (Some(Dialog::Chmod(d)), FormHit::Button(button)) => {
                d.row = CHMOD_ROWS;
                d.button = button;
                true
            }
            (Some(Dialog::Chown(d)), FormHit::Item(list, at)) => {
                d.column = list;
                match list {
                    0 => d.user_row = at,
                    _ => d.group_row = at,
                }
                false
            }
            (Some(Dialog::Chown(d)), FormHit::Row(_)) => {
                d.column = CHOWN_RECURSE_COL;
                d.recurse = !d.recurse;
                false
            }
            (Some(Dialog::Chown(d)), FormHit::Button(button)) => {
                d.column = CHOWN_BUTTON_COL;
                d.button = button;
                true
            }
            (Some(Dialog::Confirm(d)), FormHit::Button(button)) => {
                d.yes = button == 0;
                true
            }
            // a switch or a choice flips where it is clicked; the ratio
            // row only takes the focus, its arrows do the rest
            (Some(Dialog::Options(d)), FormHit::Row(row)) => {
                d.cursor = row;
                if matches!(
                    OPTION_ROWS.get(row),
                    Some(OptRow::Check(..) | OptRow::Radio(..))
                ) {
                    d.toggle();
                }
                false
            }
            (Some(Dialog::Options(d)), FormHit::Button(button)) => {
                d.cursor = OPTION_ROWS.len();
                d.ok = button == 0;
                true
            }
            _ => return false,
        };
        if press {
            self.on_dialog_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        true
    }

    pub(super) fn click_dialog_row(&mut self, x: u16, y: u16, double: bool) {
        let Some(rows) = self.dialog_rows.clone() else {
            return;
        };
        let area = rows.area;
        let inside =
            x >= area.x && x < area.x + area.width && y >= area.y && y < area.y + area.height;
        if !inside {
            return;
        }
        let Some(Some(at)) = rows.rows.get((y - area.y) as usize).copied() else {
            return;
        };
        if !self.select_dialog_row(at) {
            return;
        }
        self.dirty = true;
        if double {
            self.on_dialog_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
    }

    /// Put a list dialog's cursor on `at`, if it has one and the row
    /// exists. False = this dialog is not one of them.
    fn select_dialog_row(&mut self, at: usize) -> bool {
        let count = |n: usize| (at < n).then_some(at);
        // the hotlist's row count depends on the whole app (the recent
        // directories are part of the list), so it is worked out before
        // the dialog is borrowed
        let hotlist_len = match self.dialog.as_ref() {
            Some(Dialog::Hotlist(d)) => Some(self.hotlist_rows(d).len()),
            _ => None,
        };
        let undo_len = self.undo.len();
        let saved_len = match self.dialog {
            Some(Dialog::Connections(_)) => self.saved_connections().len(),
            _ => 0,
        };
        match self.dialog.as_mut() {
            Some(Dialog::Hotlist(d)) => {
                if let Some(at) = count(hotlist_len.unwrap_or(0)) {
                    d.row = at;
                }
                true
            }
            Some(Dialog::UserMenu(d)) => {
                if let Some(at) = count(d.entries().len()) {
                    d.row = at;
                }
                true
            }
            Some(Dialog::Jobs(row)) => {
                if let Some(at) = count(self.jobs.len()) {
                    *row = at;
                }
                true
            }
            Some(Dialog::Undo(row)) => {
                if let Some(at) = count(undo_len) {
                    *row = at;
                }
                true
            }
            Some(Dialog::History(row)) => {
                if let Some(at) = count(self.cmdline.history().len()) {
                    *row = at;
                }
                true
            }
            Some(Dialog::DirHistory(row)) => {
                if let Some(at) = count(self.panels[self.active].history_entries().0.len()) {
                    *row = at;
                }
                true
            }
            Some(Dialog::Charset(row)) => {
                if let Some(at) = count(CHARSET_ROWS.len()) {
                    *row = at;
                }
                true
            }
            Some(Dialog::Fuzzy(d)) => {
                if let Some(at) = count(d.shown.len()) {
                    d.selected = at;
                }
                true
            }
            Some(Dialog::Branches(d)) => {
                if let Some(at) = count(d.rows.len()) {
                    d.row = at;
                }
                true
            }
            Some(Dialog::Skin(row)) => {
                if let Some(at) = count(crate::theme::list().len()) {
                    *row = at;
                }
                true
            }
            Some(Dialog::Compare(row)) => {
                if let Some(at) = count(COMPARE_MODES.len()) {
                    *row = at;
                }
                true
            }
            Some(Dialog::Sync(d)) => {
                if let Some(at) = count(d.rows.len()) {
                    d.cursor = at;
                }
                true
            }
            Some(Dialog::Filters(d)) => {
                if let Some(at) = count(d.on.len()) {
                    d.row = at;
                }
                true
            }
            Some(Dialog::Connections(row)) => {
                if let Some(at) = count(saved_len) {
                    *row = at;
                }
                true
            }
            Some(Dialog::RemoteMenu(d)) => {
                if let Some(at) = count(d.items.len()) {
                    d.selected = at;
                }
                true
            }
            Some(Dialog::FileHistory(row)) => {
                if let Some(at) = count(self.file_history.len()) {
                    *row = at;
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn on_dialog_key(&mut self, key: KeyEvent) {
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        // `[keys.dialog]` is a translation: a rebound key arrives as
        // the one it stands for, so every dialog below sees the keys it
        // already knows and none of them had to learn a table
        let key = match self
            .dialog_keys
            .get(&(key.code, key.modifiers.difference(KeyModifiers::SHIFT)))
        {
            Some(&(code, modifiers)) => KeyEvent::new(code, modifiers),
            None => key,
        };
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // ...and mc's underlined hotkeys are a translation too: the
        // focus moves onto the button and the dialog is handed an Enter
        let key = match key.code {
            KeyCode::Char(c) if alt && focus_button(&mut dialog, c) => {
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            }
            _ => key,
        };
        match dialog {
            Dialog::Fuzzy(d) => self.on_fuzzy_key(d, key),
            Dialog::Input(mut d) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.submit_input(d),
                // one field and nowhere else to go: Tab completes, as it
                // does on the command line
                KeyCode::Tab => {
                    self.dialog = Some(Dialog::Input(d));
                    self.complete_focused(false);
                }
                // the pack form's level: M-0 to M-9, M-- for the default
                KeyCode::Char(c @ ('0'..='9' | '-'))
                    if key.modifiers.contains(KeyModifiers::ALT)
                        && matches!(d.action, InputAction::Pack { .. }) =>
                {
                    if let InputAction::Pack { level, .. } = &mut d.action {
                        *level = c.to_digit(10);
                    }
                    self.dialog = Some(Dialog::Input(d));
                }
                _ => {
                    d.field.key(key);
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
            Dialog::DirHistory(mut selected) => {
                // rows are newest first; Enter moves the history cursor
                // there, so Alt+←/→ carry on from that stop
                let len = self.panels[self.active].history_entries().0.len();
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        let idx = len.saturating_sub(1).saturating_sub(selected);
                        if let Some(loc) = self.panels[self.active].hist_goto(idx) {
                            self.navigate(&loc);
                        }
                    }
                    KeyCode::Up => {
                        self.dialog = Some(Dialog::DirHistory(selected.saturating_sub(1)));
                    }
                    KeyCode::Down => {
                        if selected + 1 < len {
                            selected += 1;
                        }
                        self.dialog = Some(Dialog::DirHistory(selected));
                    }
                    _ => self.dialog = Some(Dialog::DirHistory(selected)),
                }
            }
            Dialog::Vfs(mut d) => {
                let len = d.rows.len();
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        if let Some(row) = d.rows.get(d.selected) {
                            let (target, kind) = (row.target.clone(), row.kind);
                            let at = d.panel;
                            match kind {
                                // through the cache: no second login
                                VfsKind::Remote => {
                                    self.active = at;
                                    self.connect_remote(&target);
                                }
                                VfsKind::Archive => {
                                    if let Err(err) =
                                        self.panels[at].open_archive(PathBuf::from(&target))
                                    {
                                        self.status = Some(format!(" {err} "));
                                    }
                                }
                                VfsKind::Mount => self.navigate_panel(at, &target),
                            }
                        }
                    }
                    KeyCode::F(8) | KeyCode::Delete | KeyCode::Char('f' | 'F') => {
                        let panel = d.panel;
                        match d.rows.get(d.selected) {
                            // a mount point is the machine's, not ours
                            Some(row) if row.kind == VfsKind::Mount => {
                                self.status = Some(" a mount point is not rcmd's to free ".into());
                                self.dialog = Some(Dialog::Vfs(d));
                                return;
                            }
                            Some(row) => {
                                let row = VfsRow {
                                    label: row.label.clone(),
                                    target: row.target.clone(),
                                    used_by: row.used_by.clone(),
                                    kind: row.kind,
                                };
                                self.free_vfs(&row);
                            }
                            None => {}
                        }
                        // freeing changes the list under the cursor
                        let rows = self.vfs_rows();
                        if !rows.is_empty() {
                            let selected = d.selected.min(rows.len() - 1);
                            self.dialog = Some(Dialog::Vfs(VfsDialog {
                                rows,
                                selected,
                                panel,
                            }));
                        }
                    }
                    KeyCode::Up => {
                        d.selected = d.selected.saturating_sub(1);
                        self.dialog = Some(Dialog::Vfs(d));
                    }
                    KeyCode::Down => {
                        if d.selected + 1 < len {
                            d.selected += 1;
                        }
                        self.dialog = Some(Dialog::Vfs(d));
                    }
                    _ => self.dialog = Some(Dialog::Vfs(d)),
                }
            }
            Dialog::FindResults(mut d) => {
                let shown = ui::find_list_rows(self.areas.screen);
                let page = shown.saturating_sub(1).max(1) as isize;
                match key.code {
                    KeyCode::Esc | KeyCode::F(10) | KeyCode::Char('q') => self.close_find(),
                    KeyCode::Up => {
                        d.step(-1, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::Down => {
                        d.step(1, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::PageUp => {
                        d.step(-page, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::PageDown => {
                        d.step(page, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::Home => {
                        d.step(isize::MIN / 2, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::End => {
                        d.step(isize::MAX / 2, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::Left | KeyCode::BackTab => {
                        d.button = (d.button + FIND_BUTTONS.len() - 1) % FIND_BUTTONS.len();
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::Right | KeyCode::Tab => {
                        d.button = (d.button + 1) % FIND_BUTTONS.len();
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    // Enter on the list is Chdir, which is what mc's
                    // default button does and what the row invites
                    KeyCode::Enter => self.find_button(*d, None),
                    // marks, as in a panel, for the keys below
                    KeyCode::Insert | KeyCode::Char(' ') => {
                        if let Some(row) = d.rows.get_mut(d.selected) {
                            row.marked = !row.marked;
                        }
                        d.step(1, shown);
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::Char('*') => {
                        for row in &mut d.rows {
                            row.marked = !row.marked;
                        }
                        self.dialog = Some(Dialog::FindResults(d));
                    }
                    KeyCode::F(5) | KeyCode::F(6) | KeyCode::F(8) => {
                        self.find_operate(*d, key.code)
                    }
                    KeyCode::F(3) => self.find_button(*d, Some(3)),
                    KeyCode::F(4) => self.find_button(*d, Some(4)),
                    KeyCode::Char(c) => {
                        let c = c.to_ascii_lowercase();
                        match FIND_BUTTONS.iter().position(|b| {
                            b.chars()
                                .next()
                                .is_some_and(|f| f.to_ascii_lowercase() == c)
                        }) {
                            Some(at) => self.find_button(*d, Some(at)),
                            None => self.dialog = Some(Dialog::FindResults(d)),
                        }
                    }
                    _ => self.dialog = Some(Dialog::FindResults(d)),
                }
            }
            Dialog::Pattern(mut d) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if d.row != PATTERN_ROWS || d.ok {
                        self.submit_pattern(&d);
                    }
                }
                KeyCode::Up | KeyCode::BackTab => {
                    d.step(-1);
                    self.dialog = Some(Dialog::Pattern(d));
                }
                KeyCode::Down | KeyCode::Tab => {
                    d.step(1);
                    self.dialog = Some(Dialog::Pattern(d));
                }
                KeyCode::Char(' ') if d.row >= PATTERN_FIELDS => {
                    if d.row == PATTERN_ROWS {
                        d.ok = !d.ok;
                    } else {
                        d.toggle();
                    }
                    self.dialog = Some(Dialog::Pattern(d));
                }
                KeyCode::Left | KeyCode::Right if d.row == PATTERN_ROWS => {
                    d.ok = !d.ok;
                    self.dialog = Some(Dialog::Pattern(d));
                }
                _ if d.row < PATTERN_FIELDS => {
                    if let Some(field) = d.field_mut() {
                        field.key(key);
                    }
                    self.dialog = Some(Dialog::Pattern(d));
                }
                _ => self.dialog = Some(Dialog::Pattern(d)),
            },
            Dialog::Panelize(mut d) => {
                let presets = self.config.panelize.clone();
                // saving asks for a name in the same field: there is
                // one dialog slot, and a name is one line of typing
                if let Some((mut name, mut cursor)) = d.naming.take() {
                    match key.code {
                        KeyCode::Esc => {
                            self.dialog = Some(Dialog::Panelize(d));
                        }
                        KeyCode::Enter => {
                            let name = name.trim().to_string();
                            if !name.is_empty() {
                                self.save_panelize(
                                    Some(crate::config::PanelizePreset {
                                        name,
                                        run: d.command.value.clone(),
                                    }),
                                    None,
                                );
                            }
                            d.row = self.config.panelize.len().saturating_sub(1);
                            self.dialog = Some(Dialog::Panelize(d));
                        }
                        code => {
                            edit_line(&mut name, &mut cursor, code, key.modifiers);
                            d.naming = Some((name, cursor));
                            self.dialog = Some(Dialog::Panelize(d));
                        }
                    }
                    return;
                }
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        let command = match (d.on_list, presets.get(d.row)) {
                            (true, Some(preset)) => preset.run.clone(),
                            _ => d.command.value.trim().to_string(),
                        };
                        if command.is_empty() {
                            self.dialog = Some(Dialog::Panelize(d));
                        } else {
                            if !d.on_list {
                                self.remember(&d.command);
                            }
                            self.run_panelize(&command);
                        }
                    }
                    KeyCode::Tab | KeyCode::BackTab if !presets.is_empty() => {
                        d.on_list = !d.on_list;
                        self.dialog = Some(Dialog::Panelize(d));
                    }
                    KeyCode::Up | KeyCode::Down if d.on_list => {
                        let last = presets.len().saturating_sub(1);
                        d.row = match key.code {
                            KeyCode::Up => d.row.saturating_sub(1),
                            _ => (d.row + 1).min(last),
                        };
                        // the highlighted command is what Enter runs,
                        // so it shows in the field as well
                        if let Some(preset) = presets.get(d.row) {
                            d.command.set(preset.run.clone());
                        }
                        self.dialog = Some(Dialog::Panelize(d));
                    }
                    // C-s saves what is typed, F8 drops what is picked
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if !d.command.value.trim().is_empty() {
                            d.naming = Some((String::new(), 0));
                        }
                        self.dialog = Some(Dialog::Panelize(d));
                    }
                    KeyCode::F(8) | KeyCode::Delete if d.on_list && !presets.is_empty() => {
                        self.save_panelize(None, Some(d.row));
                        d.row = d.row.min(self.config.panelize.len().saturating_sub(1));
                        d.on_list = !self.config.panelize.is_empty();
                        self.dialog = Some(Dialog::Panelize(d));
                    }
                    _ => {
                        if !d.on_list {
                            d.command.key(key);
                        }
                        self.dialog = Some(Dialog::Panelize(d));
                    }
                }
            }
            Dialog::Compare(row) => {
                let last = COMPARE_MODES.len() - 1;
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => self.dialog = Some(Dialog::Compare(row.saturating_sub(1))),
                    KeyCode::Down | KeyCode::Tab => {
                        self.dialog = Some(Dialog::Compare((row + 1).min(last)))
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => self.compare_dirs(COMPARE_MODES[row].1),
                    // q, s, t: the first letter of each answer
                    KeyCode::Char(c) => {
                        let c = c.to_ascii_lowercase();
                        match COMPARE_MODES.iter().position(|(label, _)| {
                            label
                                .chars()
                                .next()
                                .is_some_and(|f| f.to_ascii_lowercase() == c)
                        }) {
                            Some(at) => self.compare_dirs(COMPARE_MODES[at].1),
                            None => self.dialog = Some(Dialog::Compare(row)),
                        }
                    }
                    _ => self.dialog = Some(Dialog::Compare(row)),
                }
            }
            Dialog::Sync(mut d) => {
                let last = d.rows.len().saturating_sub(1);
                // `+` or `-` asked for a mask: the keys are its
                if let Some((field, on)) = d.mask.as_mut() {
                    match key.code {
                        KeyCode::Esc => d.mask = None,
                        KeyCode::Enter => {
                            let (mask, on) = (Mask::new(&field.value), *on);
                            let field = std::mem::replace(field, TextField::new(""));
                            d.mask = None;
                            for row in &mut d.rows {
                                let name =
                                    row.rel.file_name().unwrap_or_default().to_string_lossy();
                                if mask.matches(&name) || mask.matches(&row.rel.to_string_lossy()) {
                                    row.on = on;
                                }
                            }
                            self.remember(&field);
                        }
                        _ => {
                            field.key(key);
                        }
                    }
                    self.dialog = Some(Dialog::Sync(d));
                    return;
                }
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => self.start_sync(&d),
                    KeyCode::F(3) => self.sync_row_diff(d),
                    KeyCode::Up => {
                        d.cursor = d.cursor.saturating_sub(1);
                        self.dialog = Some(Dialog::Sync(d));
                    }
                    KeyCode::Down => {
                        d.cursor = (d.cursor + 1).min(last);
                        self.dialog = Some(Dialog::Sync(d));
                    }
                    KeyCode::PageUp => {
                        d.cursor = d.cursor.saturating_sub(10);
                        self.dialog = Some(Dialog::Sync(d));
                    }
                    KeyCode::PageDown => {
                        d.cursor = (d.cursor + 10).min(last);
                        self.dialog = Some(Dialog::Sync(d));
                    }
                    KeyCode::Home => {
                        d.cursor = 0;
                        self.dialog = Some(Dialog::Sync(d));
                    }
                    KeyCode::End => {
                        d.cursor = last;
                        self.dialog = Some(Dialog::Sync(d));
                    }
                    // Space skips a row, the arrows say which way it
                    // goes: a plan you cannot argue with is a plan you
                    // have to accept whole
                    KeyCode::Char(' ') => {
                        if let Some(row) = d.rows.get_mut(d.cursor) {
                            row.on = !row.on;
                        }
                        d.cursor = (d.cursor + 1).min(last);
                        self.dialog = Some(Dialog::Sync(d));
                    }
                    // an arrow towards the side that has it copies it
                    // there; towards the side that has nothing, it
                    // deletes it where it is
                    KeyCode::Left | KeyCode::Right => {
                        use fsops::SyncStep::*;
                        if let Some(row) = d.rows.get_mut(d.cursor) {
                            row.step = match (key.code == KeyCode::Right, row.left, row.right) {
                                (true, Some(_), _) => ToRight,
                                (true, None, _) => DeleteRight,
                                (false, _, Some(_)) => ToLeft,
                                (false, _, None) => DeleteLeft,
                            };
                            row.on = true;
                        }
                        self.dialog = Some(Dialog::Sync(d));
                    }
                    KeyCode::Char('a' | 'A') => {
                        let all_on = d.rows.iter().all(|r| r.on);
                        for row in &mut d.rows {
                            row.on = !all_on;
                        }
                        self.dialog = Some(Dialog::Sync(d));
                    }
                    // m: no preference, the right a mirror of the left,
                    // the left a mirror of the right - planned afresh
                    KeyCode::Char('m' | 'M') => {
                        d.mirror = match d.mirror {
                            Mirror::Off => Mirror::Right,
                            Mirror::Right => Mirror::Left,
                            Mirror::Left => Mirror::Off,
                        };
                        d.rows = d.diffs.iter().map(|x| SyncRow::plan(x, d.mirror)).collect();
                        self.dialog = Some(Dialog::Sync(d));
                    }
                    KeyCode::Char(c @ ('+' | '-')) => {
                        let field = TextField::new("*").with_history("sync-mask");
                        d.mask = Some((field, c == '+'));
                        self.dialog = Some(Dialog::Sync(d));
                    }
                    _ => self.dialog = Some(Dialog::Sync(d)),
                }
            }
            Dialog::Palette(d) => self.on_palette_key(d, key),
            Dialog::Connections(row) => self.on_connections_key(row, key),
            Dialog::RemoteMenu(mut d) => {
                let last = d.items.len().saturating_sub(1);
                match key.code {
                    // dropping the reply unanswered is the script's cancel
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        let _ = d.reply.send(d.items[d.selected].clone());
                    }
                    KeyCode::Up => {
                        d.selected = d.selected.saturating_sub(1);
                        self.dialog = Some(Dialog::RemoteMenu(d));
                    }
                    KeyCode::Down => {
                        d.selected = (d.selected + 1).min(last);
                        self.dialog = Some(Dialog::RemoteMenu(d));
                    }
                    KeyCode::Home => {
                        d.selected = 0;
                        self.dialog = Some(Dialog::RemoteMenu(d));
                    }
                    KeyCode::End => {
                        d.selected = last;
                        self.dialog = Some(Dialog::RemoteMenu(d));
                    }
                    _ => self.dialog = Some(Dialog::RemoteMenu(d)),
                }
            }
            Dialog::Undo(mut row) => {
                // newest first on screen, newest last on the stack
                let last = self.undo_rows().len().saturating_sub(1);
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => self.start_undo(last.saturating_sub(row)),
                    KeyCode::Up => {
                        row = row.saturating_sub(1);
                        self.dialog = Some(Dialog::Undo(row));
                    }
                    KeyCode::Down => {
                        row = (row + 1).min(last);
                        self.dialog = Some(Dialog::Undo(row));
                    }
                    KeyCode::Home => self.dialog = Some(Dialog::Undo(0)),
                    KeyCode::End => self.dialog = Some(Dialog::Undo(last)),
                    _ => self.dialog = Some(Dialog::Undo(row)),
                }
            }
            Dialog::FileHistory(mut row) => {
                let files = self.file_history.clone();
                let last = files.len().saturating_sub(1);
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        // newest first on screen, newest last in the list
                        if let Some(path) = files.get(last.saturating_sub(row)) {
                            let path = PathBuf::from(path);
                            self.go_to_file(&path);
                        }
                    }
                    KeyCode::Up => {
                        row = row.saturating_sub(1);
                        self.dialog = Some(Dialog::FileHistory(row));
                    }
                    KeyCode::Down => {
                        row = (row + 1).min(last);
                        self.dialog = Some(Dialog::FileHistory(row));
                    }
                    KeyCode::Home => self.dialog = Some(Dialog::FileHistory(0)),
                    KeyCode::End => self.dialog = Some(Dialog::FileHistory(last)),
                    _ => self.dialog = Some(Dialog::FileHistory(row)),
                }
            }
            Dialog::Filters(mut d) => {
                let last = d.on.len().saturating_sub(1);
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => self.apply_filters(&d),
                    KeyCode::Up => {
                        d.row = d.row.saturating_sub(1);
                        self.dialog = Some(Dialog::Filters(d));
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        d.row = (d.row + 1).min(last);
                        self.dialog = Some(Dialog::Filters(d));
                    }
                    KeyCode::Home => {
                        d.row = 0;
                        self.dialog = Some(Dialog::Filters(d));
                    }
                    KeyCode::End => {
                        d.row = last;
                        self.dialog = Some(Dialog::Filters(d));
                    }
                    KeyCode::Char(' ') => {
                        if let Some(on) = d.on.get_mut(d.row) {
                            *on = !*on;
                        }
                        self.dialog = Some(Dialog::Filters(d));
                    }
                    // one key for "show everything again"
                    KeyCode::Char('a' | 'A') => {
                        let all_off = d.on.iter().all(|on| !on);
                        d.on.fill(all_off);
                        self.dialog = Some(Dialog::Filters(d));
                    }
                    _ => self.dialog = Some(Dialog::Filters(d)),
                }
            }
            Dialog::Learn(mut d) => {
                let name = keymap::key_name(key.code, key.modifiers);
                // Esc closes: it is the one key a dialog cannot also be
                // learning, and F10 is in the list
                if key.code == KeyCode::Esc {
                    return;
                }
                match LEARN_KEYS.iter().position(|k| *k == name) {
                    Some(at) => {
                        d.seen[at] = true;
                        d.last = Some((name, at == d.row));
                        // move on to the next one still unanswered
                        let next = (d.row + 1..LEARN_KEYS.len())
                            .chain(0..d.row)
                            .find(|i| !d.seen[*i]);
                        d.row = next.unwrap_or(d.row);
                    }
                    // any other key still gets named - that is what the
                    // dialog is for, as much as the checklist is
                    None => d.last = Some((name, false)),
                }
                self.dialog = Some(Dialog::Learn(d));
            }
            Dialog::Skin(row) => {
                let names = crate::theme::list();
                match pick_key(&names, row, key) {
                    PickKey::Move(to) => self.dialog = Some(Dialog::Skin(to)),
                    PickKey::Close => {}
                    PickKey::Ignored => self.dialog = Some(Dialog::Skin(row)),
                    PickKey::Chose(to) => {
                        let Some(name) = names.get(to) else { return };
                        self.config.theme = name.clone();
                        let warning = ui::init_theme(name);
                        self.repaint = true;
                        let saved = name.clone();
                        if let Err(err) = state::update(|s| s.theme = Some(saved)) {
                            self.status = Some(format!(" could not save state: {err} "));
                        } else {
                            self.status = Some(match warning {
                                Some(warning) => format!(" {warning} "),
                                None => format!(" theme: {name} "),
                            });
                        }
                    }
                }
            }
            Dialog::Branches(mut d) => match pick_key(&d.rows, d.row, key) {
                PickKey::Move(to) => {
                    d.row = to;
                    self.dialog = Some(Dialog::Branches(d));
                }
                PickKey::Close => {}
                PickKey::Chose(at) => {
                    let name = d.names[at].clone();
                    self.status = Some(match crate::git::switch(&d.dir, &name) {
                        Ok(()) => format!(" on {name} now "),
                        Err(err) => format!(" git: {err} "),
                    });
                    self.git_refresh();
                    self.reload_panels();
                }
                PickKey::Ignored => self.dialog = Some(Dialog::Branches(d)),
            },
            Dialog::Charset(row) => match charset_pick_key(row, key) {
                PickKey::Move(to) => self.dialog = Some(Dialog::Charset(to)),
                PickKey::Close => {}
                PickKey::Chose(to) => {
                    let side = self.active;
                    self.panels[side].charset = charset_at(to);
                    self.status = Some(format!(" {} ", CHARSET_ROWS[to]));
                }
                PickKey::Ignored => self.dialog = Some(Dialog::Charset(row)),
            },
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
                    KeyCode::Char('p' | 'P') => {
                        if let Some(job) = self.jobs.get(selected) {
                            job.handle.set_paused(!job.handle.is_paused());
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
                KeyCode::Esc | KeyCode::Char('n') => self.confirm_no(&d),
                KeyCode::Char('y') => self.confirm_yes(d),
                KeyCode::Enter => {
                    if d.yes {
                        self.confirm_yes(d);
                    } else {
                        self.confirm_no(&d);
                    }
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    d.yes = !d.yes;
                    self.dialog = Some(Dialog::Confirm(d));
                }
                _ => self.dialog = Some(Dialog::Confirm(d)),
            },
            Dialog::Hotlist(mut d) => {
                let rows = self.hotlist_rows(&d);
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let last = rows.len().saturating_sub(1);
                // while a filter is being typed the letters are the
                // filter, not the commands: the same bargain the panel's
                // own quick search makes
                if d.filter.is_some() {
                    match key.code {
                        KeyCode::Char(c) if !ctrl && !alt => {
                            if let Some(f) = d.filter.as_mut() {
                                f.push(c);
                            }
                            d.row = 0;
                            self.dialog = Some(Dialog::Hotlist(d));
                            return;
                        }
                        KeyCode::Backspace => {
                            match d.filter.as_mut() {
                                Some(f) if !f.is_empty() => {
                                    f.pop();
                                }
                                // backspacing past the start leaves the
                                // field rather than closing the dialog
                                _ => d.filter = None,
                            }
                            d.row = 0;
                            self.dialog = Some(Dialog::Hotlist(d));
                            return;
                        }
                        // the first Esc drops the filter, a second one
                        // closes the hotlist
                        KeyCode::Esc => {
                            d.filter = None;
                            d.row = 0;
                            self.dialog = Some(Dialog::Hotlist(d));
                            return;
                        }
                        _ => {}
                    }
                }
                match key.code {
                    KeyCode::Esc => {}
                    // C-s: narrow the list by what you type, which on a
                    // list ranked by where you actually go is the whole
                    // of "take me there"
                    KeyCode::Char('s') if ctrl => {
                        d.filter = Some(String::new());
                        d.row = 0;
                        self.dialog = Some(Dialog::Hotlist(d));
                    }
                    KeyCode::Up if alt => {
                        self.hotlist_reorder(&mut d, &rows, false);
                        self.dialog = Some(Dialog::Hotlist(d));
                    }
                    KeyCode::Down if alt => {
                        self.hotlist_reorder(&mut d, &rows, true);
                        self.dialog = Some(Dialog::Hotlist(d));
                    }
                    KeyCode::Up => {
                        d.row = d.row.saturating_sub(1);
                        self.dialog = Some(Dialog::Hotlist(d));
                    }
                    KeyCode::Down => {
                        d.row = (d.row + 1).min(last);
                        self.dialog = Some(Dialog::Hotlist(d));
                    }
                    KeyCode::Home => {
                        d.row = 0;
                        self.dialog = Some(Dialog::Hotlist(d));
                    }
                    KeyCode::End => {
                        d.row = last;
                        self.dialog = Some(Dialog::Hotlist(d));
                    }
                    KeyCode::Enter => match rows.get(d.row) {
                        Some(HotRow::Up) => {
                            let was = d.group.pop();
                            d.row = was.unwrap_or(0);
                            self.dialog = Some(Dialog::Hotlist(d));
                        }
                        Some(HotRow::Group(at)) => {
                            d.group.push(*at);
                            d.row = 0;
                            self.dialog = Some(Dialog::Hotlist(d));
                        }
                        Some(HotRow::Entry(at)) => {
                            let path = self
                                .hot_group(&d.group)
                                .get(*at)
                                .map(|e| e.path.clone())
                                .unwrap_or_default();
                            self.hotlist_go(&path);
                        }
                        Some(HotRow::Recent(loc)) => {
                            let loc = loc.clone();
                            self.navigate(&loc);
                        }
                        None => {}
                    },
                    // add the panel's directory, asking what to call it
                    KeyCode::Char('a') => {
                        let panel = &self.panels[self.active];
                        let path = match panel.is_remote() {
                            true => panel.display_path(),
                            false => panel.local_cwd().display().to_string(),
                        };
                        let label = Path::new(&path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.clone());
                        self.ask_hotlist_label(" Add to hotlist ", label, d.group, None, path);
                    }
                    // ...a group to put things in...
                    KeyCode::Char('g') => {
                        self.ask_hotlist_label(
                            " New group ",
                            String::new(),
                            d.group,
                            None,
                            String::new(),
                        );
                    }
                    // ...and a new name for whatever is under the cursor
                    KeyCode::Char('e') => match rows.get(d.row) {
                        Some(HotRow::Entry(at) | HotRow::Group(at)) => {
                            let at = *at;
                            let entry = self.hot_group(&d.group)[at].clone();
                            self.ask_hotlist_label(
                                " Rename ",
                                entry.label,
                                d.group,
                                Some(at),
                                entry.path,
                            );
                        }
                        _ => self.dialog = Some(Dialog::Hotlist(d)),
                    },
                    // pick an entry up, walk to a group, put it down
                    KeyCode::Char('m') => {
                        match d.moving.take() {
                            Some(entry) => self.hot_group_mut(&d.group).push(entry),
                            None => {
                                if let Some(HotRow::Entry(at) | HotRow::Group(at)) = rows.get(d.row)
                                {
                                    d.moving = Some(self.hot_group_mut(&d.group).remove(*at));
                                }
                            }
                        }
                        self.save_hotlist();
                        let last = self.hotlist_rows(&d).len().saturating_sub(1);
                        d.row = d.row.min(last);
                        self.dialog = Some(Dialog::Hotlist(d));
                    }
                    KeyCode::Char('d') => match rows.get(d.row) {
                        Some(HotRow::Entry(at) | HotRow::Group(at)) => {
                            let at = *at;
                            let entry = self.hot_group(&d.group)[at].clone();
                            if self.config.confirm_hotlist_delete {
                                let what = match entry.is_group() {
                                    true => "group",
                                    false => "entry",
                                };
                                self.dialog = Some(Dialog::Confirm(ConfirmDialog {
                                    title: " Hotlist ".into(),
                                    message: format!(
                                        "Drop the {what} \"{}\" from the hotlist?",
                                        entry.label
                                    ),
                                    yes: true,
                                    paths: Vec::new(),
                                    permanent: false,
                                    kind: ConfirmKind::HotlistDelete {
                                        group: d.group.clone(),
                                        index: at,
                                    },
                                    command: None,
                                }));
                            } else {
                                self.hotlist_drop(&d.group, at);
                                let last = self.hotlist_rows(&d).len().saturating_sub(1);
                                d.row = d.row.min(last);
                                self.dialog = Some(Dialog::Hotlist(d));
                            }
                        }
                        // the recent half is a log, not a list
                        _ => self.dialog = Some(Dialog::Hotlist(d)),
                    },
                    _ => self.dialog = Some(Dialog::Hotlist(d)),
                }
            }
            Dialog::Link(mut d) => {
                let last = d.rows(); // the button row
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => {
                        d.row = d.row.checked_sub(1).unwrap_or(last);
                        self.dialog = Some(Dialog::Link(d));
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        d.row = if d.row >= last { 0 } else { d.row + 1 };
                        self.dialog = Some(Dialog::Link(d));
                    }
                    KeyCode::Left | KeyCode::Right if d.row == last => {
                        d.ok = !d.ok;
                        self.dialog = Some(Dialog::Link(d));
                    }
                    KeyCode::Enter => {
                        if d.ok {
                            self.submit_link(*d);
                        }
                    }
                    _ => {
                        match d.row {
                            0 => d.target.key(key),
                            1 => d.name.key(key),
                            _ => false,
                        };
                        self.dialog = Some(Dialog::Link(d));
                    }
                }
            }
            Dialog::Chown(mut d) => {
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up if d.column < 2 => {
                        d.move_by(-1);
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::Down if d.column < 2 => {
                        d.move_by(1);
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::PageUp if d.column < 2 => {
                        d.move_by(-(CHOWN_ROWS as isize));
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::PageDown if d.column < 2 => {
                        d.move_by(CHOWN_ROWS as isize);
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::Home if d.column < 2 => {
                        d.move_by(isize::MIN / 2);
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::End if d.column < 2 => {
                        d.move_by(isize::MAX / 2);
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    // Tab walks user list -> group list -> recurse -> buttons
                    KeyCode::Tab => {
                        d.column = (d.column + 1) % CHOWN_STOPS;
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    // ...and only the box itself takes Space. A letter
                    // key must never flip it: names get typed at these
                    // lists, and "jarda" would tick it on the r
                    KeyCode::Char(' ') if d.column == CHOWN_RECURSE_COL => {
                        d.recurse = !d.recurse;
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::Left | KeyCode::Right => {
                        if d.column == CHOWN_BUTTON_COL {
                            let count = CHOWN_BUTTONS.len();
                            d.button = if key.code == KeyCode::Left {
                                d.button.checked_sub(1).unwrap_or(count - 1)
                            } else {
                                (d.button + 1) % count
                            };
                        } else {
                            d.column = if key.code == KeyCode::Left { 0 } else { 1 };
                        }
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::Down | KeyCode::Up => {
                        // on the button row, up returns to the lists
                        d.column = 0;
                        self.dialog = Some(Dialog::Chown(d));
                    }
                    KeyCode::Enter => {
                        if d.column == CHOWN_RECURSE_COL {
                            // Enter on the box is a Set, as it is on any
                            // other row of a form
                            d.column = CHOWN_BUTTON_COL;
                        }
                        if CHOWN_BUTTONS.get(d.button) != Some(&"Cancel") {
                            let (uid, gid) = d.picked();
                            let paths = d.paths.clone();
                            if d.recurse {
                                self.start_attrs_job(
                                    paths,
                                    fsops::Attrs {
                                        uid,
                                        gid,
                                        ..Default::default()
                                    },
                                    "chown",
                                );
                            } else {
                                self.apply_fs_op(&paths, "chown", |w, p| w.set_owner(p, uid, gid));
                            }
                        }
                    }
                    _ => self.dialog = Some(Dialog::Chown(d)),
                }
            }
            Dialog::Chmod(mut d) => {
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => {
                        d.row = d.row.checked_sub(1).unwrap_or(CHMOD_ROWS);
                        self.dialog = Some(Dialog::Chmod(d));
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        d.row = if d.row >= CHMOD_ROWS { 0 } else { d.row + 1 };
                        self.dialog = Some(Dialog::Chmod(d));
                    }
                    KeyCode::Left | KeyCode::Right if d.row == CHMOD_ROWS => {
                        let count = CHMOD_BUTTONS.len();
                        d.button = if key.code == KeyCode::Left {
                            d.button.checked_sub(1).unwrap_or(count - 1)
                        } else {
                            (d.button + 1) % count
                        };
                        self.dialog = Some(Dialog::Chmod(d));
                    }
                    // Space flips a bit, and the octal follows along
                    KeyCode::Char(' ') if d.row < CHMOD_BITS.len() => {
                        d.mode ^= CHMOD_BITS[d.row].1;
                        d.sync_octal();
                        self.dialog = Some(Dialog::Chmod(d));
                    }
                    KeyCode::Char(' ') if d.row == CHMOD_RECURSE_ROW => {
                        d.recurse = !d.recurse;
                        self.dialog = Some(Dialog::Chmod(d));
                    }
                    KeyCode::Enter => self.submit_chmod(*d),
                    code => {
                        // ...and typing in the octal moves the bits
                        if d.row == CHMOD_OCTAL_ROW {
                            edit_line(&mut d.octal, &mut d.octal_cursor, code, key.modifiers);
                            d.sync_mode();
                        }
                        self.dialog = Some(Dialog::Chmod(d));
                    }
                }
            }
            Dialog::Chattr(mut d) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Up => {
                    d.row = d.row.checked_sub(1).unwrap_or(CHATTR_ROWS);
                    self.dialog = Some(Dialog::Chattr(d));
                }
                KeyCode::Down | KeyCode::Tab => {
                    d.row = if d.row >= CHATTR_ROWS { 0 } else { d.row + 1 };
                    self.dialog = Some(Dialog::Chattr(d));
                }
                KeyCode::Left | KeyCode::Right if d.row == CHATTR_ROWS => {
                    let count = CHATTR_BUTTONS.len();
                    d.button = if key.code == KeyCode::Left {
                        d.button.checked_sub(1).unwrap_or(count - 1)
                    } else {
                        (d.button + 1) % count
                    };
                    self.dialog = Some(Dialog::Chattr(d));
                }
                KeyCode::Char(' ') if d.row < CHATTR_ROWS => {
                    d.flags ^= rcmd_core::attrs::FLAGS[d.row].1;
                    self.dialog = Some(Dialog::Chattr(d));
                }
                KeyCode::Enter => self.submit_chattr(*d),
                _ => self.dialog = Some(Dialog::Chattr(d)),
            },
            Dialog::Transfer(mut d) => {
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up => {
                        d.row = d.row.checked_sub(1).unwrap_or(TRANSFER_ROWS);
                        self.dialog = Some(Dialog::Transfer(d));
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        d.row = if d.row >= TRANSFER_ROWS { 0 } else { d.row + 1 };
                        self.dialog = Some(Dialog::Transfer(d));
                    }
                    KeyCode::Enter => self.submit_transfer(*d),
                    // on the button row the arrows pick a button, on a
                    // checkbox row Space flips it, and the destination
                    // line takes everything else as typing
                    KeyCode::Left | KeyCode::Right if d.row == TRANSFER_ROWS => {
                        d.button = if key.code == KeyCode::Left {
                            d.button
                                .checked_sub(1)
                                .unwrap_or(TRANSFER_BUTTONS.len() - 1)
                        } else {
                            (d.button + 1) % TRANSFER_BUTTONS.len()
                        };
                        self.dialog = Some(Dialog::Transfer(d));
                    }
                    KeyCode::Char(' ')
                        if (TRANSFER_DEST_ROW + 1..TRANSFER_ROWS).contains(&d.row) =>
                    {
                        let row = d.row - TRANSFER_DEST_ROW - 1;
                        d.toggle(row);
                        self.dialog = Some(Dialog::Transfer(d));
                    }
                    _ => {
                        match d.row {
                            0 => d.mask.key(key),
                            TRANSFER_DEST_ROW => d.dest.key(key),
                            _ => false,
                        };
                        self.dialog = Some(Dialog::Transfer(d));
                    }
                }
            }
            Dialog::Tree(mut tree) => {
                let plain = !key.modifiers.contains(KeyModifiers::ALT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => {}
                    // mc: Enter leaves the tree and takes *this* panel
                    // to the selected directory
                    KeyCode::Enter => {
                        if let Some(target) = tree.selected_path() {
                            let panel = &mut self.panels[self.active];
                            let moved = if panel.is_remote() {
                                panel.to_local(target)
                            } else {
                                panel.cd(target)
                            };
                            if let Err(err) = moved {
                                self.status = Some(format!(" tree: {err} "));
                            }
                        }
                    }
                    code => {
                        match code {
                            KeyCode::Up => tree.up(),
                            KeyCode::Down => tree.down(),
                            KeyCode::PageUp => tree.page_up(TREE_ROWS),
                            KeyCode::PageDown => tree.page_down(TREE_ROWS),
                            KeyCode::Home => tree.first(),
                            KeyCode::End => tree.last(),
                            KeyCode::Left => tree.left(),
                            KeyCode::Right => tree.right(),
                            KeyCode::F(2) => tree.rescan(),
                            KeyCode::F(3) => tree.forget(),
                            KeyCode::F(4) => tree.toggle_mode(),
                            KeyCode::Char('r') if !plain => tree.rescan(),
                            KeyCode::Char('s') if !plain => tree.search_next(),
                            KeyCode::Backspace => tree.search_pop(),
                            // mc's type-to-search: any other character
                            // jumps to the next directory starting with
                            // what has been typed so far
                            KeyCode::Char(c) if plain => {
                                tree.search_push(c);
                            }
                            _ => {}
                        }
                        self.dialog = Some(Dialog::Tree(tree));
                    }
                }
            }
            Dialog::UserMenu(mut d) => {
                let len = d.entries().len();
                let run =
                    |app: &mut Self, d: Box<UserMenuDialog>, at: usize| match d.entries().get(at) {
                        Some(entry) if entry.is_submenu() => {
                            let mut d = d;
                            d.path.push(at);
                            d.row = 0;
                            app.dialog = Some(Dialog::UserMenu(d));
                        }
                        Some(entry) => {
                            let run = entry.run.clone();
                            app.run_macro_command(&run, false);
                        }
                        None => app.dialog = Some(Dialog::UserMenu(d)),
                    };
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        let at = d.row;
                        run(self, d, at)
                    }
                    KeyCode::Left | KeyCode::Backspace if !d.path.is_empty() => {
                        let was = d.path.pop().unwrap_or(0);
                        d.row = was;
                        self.dialog = Some(Dialog::UserMenu(d));
                    }
                    KeyCode::Char(c @ '1'..='9') => {
                        let at = c as usize - '1' as usize;
                        match at < len {
                            true => run(self, d, at),
                            false => self.dialog = Some(Dialog::UserMenu(d)),
                        }
                    }
                    KeyCode::Up => {
                        d.row = d.row.saturating_sub(1);
                        self.dialog = Some(Dialog::UserMenu(d));
                    }
                    KeyCode::Down => {
                        d.row = (d.row + 1).min(len.saturating_sub(1));
                        self.dialog = Some(Dialog::UserMenu(d));
                    }
                    KeyCode::Home => {
                        d.row = 0;
                        self.dialog = Some(Dialog::UserMenu(d));
                    }
                    KeyCode::End => {
                        d.row = len.saturating_sub(1);
                        self.dialog = Some(Dialog::UserMenu(d));
                    }
                    _ => self.dialog = Some(Dialog::UserMenu(d)),
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
                KeyCode::Left | KeyCode::Right
                    if d.nudge(if key.code == KeyCode::Left { -5 } else { 5 }) =>
                {
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
                KeyCode::Enter => {
                    if d.row != FIND_ROWS || d.ok {
                        self.submit_find(*d);
                    }
                }
                KeyCode::Tab | KeyCode::Down => {
                    d.step(1);
                    self.dialog = Some(Dialog::Find(d));
                }
                KeyCode::BackTab | KeyCode::Up => {
                    d.step(-1);
                    self.dialog = Some(Dialog::Find(d));
                }
                KeyCode::Char(' ') if d.row >= FIND_FIELDS => {
                    if d.row == FIND_ROWS {
                        d.ok = !d.ok;
                    } else {
                        d.toggle();
                    }
                    self.dialog = Some(Dialog::Find(d));
                }
                KeyCode::Left | KeyCode::Right if d.row == FIND_ROWS => {
                    d.ok = !d.ok;
                    self.dialog = Some(Dialog::Find(d));
                }
                _ => {
                    if let Some(field) = d.field() {
                        field.key(key);
                    }
                    self.dialog = Some(Dialog::Find(d));
                }
            },
        }
    }

    /// OK in the options form: apply every change live and write it
    /// through to the state file right away.
    fn apply_options(&mut self, d: &OptionsDialog) {
        let before = self.config.clone();
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
        self.config.split = if d.get(Opt::HorizontalSplit) {
            "horizontal"
        } else {
            "vertical"
        }
        .to_string();
        self.config.split_ratio = d.ratio;
        self.config.show_menubar = d.get(Opt::MenuBar);
        self.config.show_status = d.get(Opt::StatusLine);
        self.config.show_mini_status = d.get(Opt::MiniStatus);
        self.config.show_free_space = d.get(Opt::FreeSpace);
        self.config.show_cmdline = d.get(Opt::CommandLine);
        self.config.show_keybar = d.get(Opt::KeyBar);
        self.config.restore_other_dir = d.get(Opt::RestoreOtherDir);
        self.config.confirm_delete = d.get(Opt::ConfirmDelete);
        self.config.confirm_overwrite = d.get(Opt::ConfirmOverwrite);
        self.config.confirm_exit = d.get(Opt::ConfirmExit);
        self.config.confirm_hotlist_delete = d.get(Opt::ConfirmHotlistDelete);
        self.config.confirm_execute = d.get(Opt::ConfirmExecute);
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
        // Write through immediately - waiting for exit would let any
        // other running instance clobber these on its own exit. Goes to
        // the state file: the user's config.toml is read-only for us.
        // Only what the form changed is written: a key the state file
        // does not hold keeps following the config, so an edit there
        // is not shadowed by a value nobody chose in the UI.
        let after = self.config.clone();
        if let Err(err) = state::update(move |s| {
            macro_rules! changed {
                ($($field:ident),+ $(,)?) => {$(
                    if before.$field != after.$field {
                        s.$field = Some(after.$field.clone());
                    }
                )+};
            }
            changed!(
                show_hidden,
                mouse,
                watch,
                restore_other_dir,
                git,
                subshell,
                editor,
                confirm_delete,
                confirm_overwrite,
                confirm_exit,
                confirm_hotlist_delete,
                confirm_execute,
                split,
                split_ratio,
                show_menubar,
                show_status,
                show_mini_status,
                show_free_space,
                show_cmdline,
                show_keybar,
            );
            // `lynx` is Option in the config too: unset means "follow the preset"
            if before.lynx != after.lynx {
                s.lynx = after.lynx;
            }
        }) {
            self.status = Some(format!(" could not save state: {err} "));
        }
    }

    /// Add what a field holds to its history; a state file that cannot
    /// be written is worth a word on the status line, nothing more.
    pub(super) fn remember(&mut self, field: &TextField) {
        if let Err(err) = field.remember() {
            self.status = Some(format!(" could not save state: {err} "));
        }
    }

    fn submit_input(&mut self, dialog: InputDialog) {
        // a script's question is answered even when the answer is empty
        if let InputAction::RemoteAnswer(reply) = &dialog.action {
            let _ = reply.send(dialog.field.value.clone());
            return;
        }
        let value = dialog.field.value.trim().to_string();
        if value.is_empty() {
            return;
        }
        self.remember(&dialog.field);
        match dialog.action {
            InputAction::CopyTo { sources } => {
                self.route_transfer(sources, &value, false, TransferOpts::default(), None)
            }
            InputAction::MoveTo { sources } => {
                self.route_transfer(sources, &value, true, TransferOpts::default(), None)
            }
            InputAction::Mkdir => {
                if self.panels[self.active].is_remote() {
                    self.remote_mkdir(&value);
                    return;
                }
                if let Some(archive) = self.panels[self.active].archive.clone() {
                    let dir = self.panels[self.active].cwd.join(value.trim_matches('/'));
                    self.start_archive_edit(archive, vec![fsops::ArchiveOp::Mkdir(dir)], "create");
                    return;
                }
                let path = self.resolve(&value);
                match std::fs::create_dir_all(&path) {
                    Ok(()) => {
                        for panel in &mut self.panels {
                            let _ = panel.refresh();
                        }
                        self.refresh_trees();
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

            InputAction::Pack { sources, level } => {
                let archive = self.resolve(value.trim());
                if archive.is_dir() {
                    self.status = Some(" that is a directory, not an archive name ".into());
                    return;
                }
                self.start_pack(sources, archive, PathBuf::new(), level);
            }
            InputAction::Apply => self.run_apply(&value),
            InputAction::Checksum { paths } => {
                let out = self.resolve(value.trim());
                let dir = self.panels[self.active].local_cwd();
                let count = paths.len();
                let handle = fsops::spawn_checksums(dir, paths, out);
                self.push_job(format!(" checksum {count} item(s) "), handle);
            }
            InputAction::SftpConnect => self.connect_remote(&value),
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
            InputAction::FilteredView => {
                if !value.trim().is_empty() {
                    let rule = crate::config::OpenRule::by_glob("*", value.trim());
                    self.open_viewer_keeping(
                        false,
                        ViewKeep {
                            filter: Some(rule),
                            ..ViewKeep::default()
                        },
                    );
                }
            }
            InputAction::HotlistLabel { group, index, path } => {
                self.finish_hotlist_label(&value, group, index, path)
            }
            InputAction::MacroPrompt {
                before,
                rest,
                quiet,
            } => self.finish_macro(&value, before, rest, quiet),
            // answered above, before an empty answer could be dropped
            InputAction::RemoteAnswer(_) => {}
            InputAction::SaveConnection => self.save_connection(&value),
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
        }
    }

    /// Send F5/F6 to the right job for the source panel and the typed
    /// destination: plain copy/move, archive pack/extract, or a
    /// cross-provider transfer when SFTP is on either side.
    /// A button on the chmod matrix. MC's three ways to spend the bits:
    /// set them exactly, add them, or take them away - the last two
    /// leave every other bit of each file alone, which is the whole
    /// point of chmod'ing a group of files at once.
    fn submit_chmod(&mut self, d: ChmodDialog) {
        let Some(action) = CHMOD_BUTTONS.get(d.button).map(|label| button_text(label)) else {
            return;
        };
        if action == "Cancel" {
            return;
        }
        let mode = d.mode;
        // each entry's current mode, for the add/remove variants
        let current: std::collections::HashMap<PathBuf, u32> = self.panels[self.active]
            .entries
            .iter()
            .map(|e| (self.panels[self.active].cwd.join(&e.name), e.mode & 0o7777))
            .collect();
        let apply = |path: &Path| -> u32 {
            let was = current.get(path).copied().unwrap_or(0);
            match action.as_str() {
                "Set marked" => was | mode,
                "Clear marked" => was & !mode,
                _ => mode,
            }
        };
        let paths = d.paths.clone();
        if d.recurse {
            // one mode for the whole tree: "add" and "remove" are per
            // file, and a tree has no single mode to add them to
            self.start_attrs_job(
                paths,
                fsops::Attrs {
                    mode: Some(mode),
                    ..Default::default()
                },
                "chmod",
            );
            return;
        }
        self.apply_fs_op(&paths, "chmod", |w, p| w.set_mode(p, apply(p)));
    }

    /// A button on the chattr window, chmod's rules: Set gives every
    /// entry exactly the boxes, Set marked adds them to what each has,
    /// Clear marked takes them away. Only the flags the window shows
    /// are touched - the rest (extents, inline data...) are the
    /// filesystem's, and it refuses to have them set.
    fn submit_chattr(&mut self, d: ChattrDialog) {
        let Some(action) = CHATTR_BUTTONS.get(d.button).map(|label| button_text(label)) else {
            return;
        };
        if action == "Cancel" {
            return;
        }
        let shown: u32 = rcmd_core::attrs::FLAGS.iter().map(|f| f.1).sum();
        let mut done = 0usize;
        for path in &d.paths {
            let result = rcmd_core::attrs::get(path).and_then(|was| {
                let flags = match action.as_str() {
                    "Set marked" => was | d.flags,
                    "Clear marked" => was & !d.flags,
                    _ => (was & !shown) | (d.flags & shown),
                };
                if flags == was {
                    return Ok(());
                }
                rcmd_core::attrs::set(path, flags)
            });
            if let Err(err) = result {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                self.status = Some(format!(" chattr {name}: {err} ({done} done) "));
                return;
            }
            done += 1;
        }
        self.status = Some(format!(" chattr: {done} item(s) "));
    }

    /// OK, Background or Queue on the copy/move form.
    fn submit_transfer(&mut self, d: TransferDialog) {
        let (background, queue) = match d.button {
            0 => (false, false),
            1 => (true, false),
            2 => (true, true),
            _ => return, // Cancel
        };
        self.remember(&d.dest);
        if d.mask.value != "*" {
            self.remember(&d.mask);
        }
        // MC puts the target mask in the destination's last component;
        // anything without a wildcard there is a plain destination
        let typed = d.dest.value.as_str();
        let (dest, target) = match typed.rsplit_once('/') {
            Some((dir, last)) if mask::is_target_mask(last) => {
                (format!("{dir}/"), Some(last.to_string()))
            }
            _ if mask::is_target_mask(typed) => (String::new(), Some(typed.to_string())),
            _ => (typed.to_string(), None),
        };
        let rename = Rename::new(Mask::new(&d.mask.value), target);
        let device = self.write_device(&dest);
        let before = self.jobs.len();
        // a queued job is held from the moment it is made, so it cannot
        // have begun before it is told to wait
        fsops::hold_new_jobs(queue);
        self.route_transfer(d.sources, &dest, d.is_move, d.opts, rename);
        fsops::hold_new_jobs(false);
        // every route ends in one pushed job, or in a status message and
        // no job at all
        if self.jobs.len() > before
            && let Some(job) = self.jobs.last_mut()
        {
            job.device = device;
            job.background |= background;
            if queue && job.handle.is_held() {
                self.status = Some(" queued: it starts when the device is free ".into());
            }
        }
        self.release_queued();
    }

    /// What a copy to `dest` writes to, as the queue tells jobs apart:
    /// a server by its URL's scheme and host, a local path by the
    /// device of the nearest directory of it that exists.
    fn write_device(&self, dest: &str) -> Option<String> {
        if is_remote_url(dest) {
            let (scheme, rest) = dest.split_once("://")?;
            let host = rest.split('/').next().unwrap_or_default();
            return Some(format!("{scheme}://{host}"));
        }
        let path = match split_vfs_dest(dest) {
            Some((archive, _)) => self.resolve(&archive.to_string_lossy()),
            None => self.resolve(dest),
        };
        use std::os::unix::fs::MetadataExt;
        let meta = path.ancestors().find_map(|p| std::fs::metadata(p).ok())?;
        Some(format!("dev:{}", meta.dev()))
    }

    pub(super) fn open_filter(&mut self) {
        // the filter in force, so the dialog opens on what is hiding
        // things rather than on a blank
        let current = self.panels[self.active].filter.clone().unwrap_or_default();
        self.dialog = Some(Dialog::Pattern(Box::new(PatternDialog {
            title: " Filter (show files matching) ".into(),
            value: PatternDialog::pattern_field(PatternKind::Filter, current.text),
            size: TextField::new(current.size).with_history("size"),
            newer: TextField::new(current.newer).with_history("newer"),
            shell: current.shell,
            case_sensitive: current.case_sensitive,
            files_only: current.files_only,
            row: 0,
            ok: true,
            kind: PatternKind::Filter,
        })));
    }

    /// OK on that form: mark, unmark, or set the panel's filter.
    fn submit_pattern(&mut self, d: &PatternDialog) {
        for field in d.fields() {
            self.remember(field);
        }
        let pattern = d.to_pattern();
        if let Err(err) = pattern.compile() {
            // the regular expression is the user's, so it is quoted
            // back rather than swallowed
            self.status = Some(format!(" {} ", err.lines().next().unwrap_or("bad pattern")));
            return;
        }
        match d.kind {
            PatternKind::Select { mark } => {
                match self.panels[self.active].mark_pattern(&pattern, mark) {
                    Ok(moved) => {
                        let verb = if mark { "selected" } else { "unselected" };
                        self.status = Some(format!(" {moved} {verb} "));
                    }
                    Err(err) => self.status = Some(format!(" {err} ")),
                }
            }
            PatternKind::Filter => {
                let panel = &mut self.panels[self.active];
                panel.filter = (!pattern.is_open()).then_some(pattern);
                self.fallible(|p| p.reload().map(|()| true));
            }
        }
    }

    /// OK on the link form.
    fn submit_link(&mut self, d: LinkDialog) {
        self.remember(&d.target);
        self.remember(&d.name);
        let (target, name) = (
            d.target.value.trim().to_string(),
            d.name.value.trim().to_string(),
        );
        if target.is_empty() || name.is_empty() {
            return;
        }
        let cwd = self.panels[self.active].cwd.clone();
        let link = if Path::new(&name).is_absolute() {
            PathBuf::from(&name)
        } else {
            cwd.join(&name)
        };
        let target = PathBuf::from(&target);
        match d.kind {
            LinkKind::Hard => {
                // the original is named relative to the panel, as it is
                // for every other command here
                let existing = if target.is_absolute() {
                    target
                } else {
                    cwd.join(&target)
                };
                self.apply_fs_op(&[link], "link", |w, p| w.hard_link(&existing, p));
            }
            LinkKind::Symbolic => {
                self.apply_fs_op(&[link], "symlink", |w, p| w.symlink(&target, p));
            }
            // there is no atomic retarget: the link is replaced
            LinkKind::EditSymlink => self.apply_fs_op(&[link], "symlink", |w, p| {
                w.remove_file(p)?;
                w.symlink(&target, p)
            }),
        }
    }

    /// OK on that list: the ticked sets become the panel's filter, and
    /// none ticked clears it.
    fn apply_filters(&mut self, d: &FiltersDialog) {
        let masks: Vec<&str> = self
            .config
            .filter
            .iter()
            .zip(&d.on)
            .filter(|(_, on)| **on)
            .map(|(set, _)| set.mask.as_str())
            .collect();
        let text = rcmd_core::pattern::join_masks(masks);
        self.filter_sets_on[d.panel] = d.on.clone();
        let panel = &mut self.panels[d.panel];
        panel.filter = match text.is_empty() {
            true => None,
            false => Some(rcmd_core::pattern::Pattern {
                text,
                ..Default::default()
            }),
        };
        let active = self.active;
        self.active = d.panel;
        self.fallible(|p| p.reload().map(|()| true));
        self.active = active;
    }

    /// Yes on a confirm dialog: do whatever it was asking about.
    fn confirm_yes(&mut self, d: ConfirmDialog) {
        match d.kind {
            ConfirmKind::Delete => self.start_delete(d.paths, d.permanent),
            ConfirmKind::Restore => self.start_restore(d.paths, false),
            ConfirmKind::Wipe => self.start_wipe(d.paths),
            ConfirmKind::Quit => self.quit_now(),
            ConfirmKind::HotlistDelete { group, index } => {
                self.hotlist_drop(&group, index);
                let d = HotlistDialog::at(group, index);
                let last = self.hotlist_rows(&d).len().saturating_sub(1);
                self.dialog = Some(Dialog::Hotlist(HotlistDialog::at(d.group, index.min(last))));
            }
            ConfirmKind::Execute => {
                if let Some(cmd) = d.command {
                    self.pending_exec = Some(Exec::Quiet(cmd));
                }
            }
        }
    }

    /// No (or Esc) on a confirm dialog. Only the hotlist needs anything
    /// done: its own dialog was displaced to ask the question.
    fn confirm_no(&mut self, d: &ConfirmDialog) {
        if let ConfirmKind::HotlistDelete { group, index } = &d.kind {
            self.dialog = Some(Dialog::Hotlist(HotlistDialog::at(group.clone(), *index)));
        }
    }
}
