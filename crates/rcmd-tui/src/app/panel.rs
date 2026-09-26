use super::*;

impl App {
    /// Enter on a file no rule claims: the desktop's opener, detached,
    /// with nothing of the terminal attached to it - the way a GUI
    /// opener is meant to be run from a `[[open]]` rule, minus the rule.
    fn desktop_open(&mut self, path: &Path) {
        if !self.config.desktop_open {
            return;
        }
        let Some(opener) = crate::config::desktop_opener() else {
            return;
        };
        let spawned = std::process::Command::new(opener)
            .arg(path)
            .current_dir(self.panels[self.active].local_cwd())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        self.status = Some(match spawned {
            Ok(_) => format!(" opened with {opener} "),
            Err(err) => format!(" {opener}: {err} "),
        });
    }

    /// Ctrl+S type-ahead: printable keys refine the prefix, Ctrl+S jumps
    /// to the next match, anything else leaves the mode (and is handled
    /// normally).
    pub(super) fn on_quick_search_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // In tree mode the search runs over the figure instead of the
        // listing. This is mc's rule for a tree *view*: plain characters
        // stay with the command line until Ctrl+S switches the search on.
        if self.panels[self.active].list_mode == ListMode::Tree {
            let mut close = true;
            if let Some(tree) = self.trees[self.active].as_mut() {
                close = false;
                match key.code {
                    KeyCode::Char('s') if ctrl => tree.search_next(),
                    KeyCode::Char(c) if !ctrl && !alt => {
                        tree.search_push(c);
                    }
                    KeyCode::Backspace => tree.search_pop(),
                    _ => {
                        tree.clear_search();
                        close = true;
                    }
                }
            }
            let search = self.trees[self.active].as_ref().map(|t| QuickSearch {
                text: t.search.clone(),
                miss: false,
            });
            self.quick_search = if close { None } else { search };
            // Esc only ends the search; anything else - Enter included,
            // as in mc - was meant for the panel underneath
            if close && key.code != KeyCode::Esc {
                self.on_panel_key(key);
            }
            return;
        }
        let text = |app: &Self| {
            app.quick_search
                .as_ref()
                .map(|q| q.text.clone())
                .unwrap_or_default()
        };
        match key.code {
            KeyCode::Esc => self.quick_search = None,
            // C-s / M-s again, and the arrows, walk the matches -
            // typing narrows, these two move
            KeyCode::Char('s') if ctrl || alt => self.search_step(&text(self), true),
            KeyCode::Down => self.search_step(&text(self), true),
            KeyCode::Up => self.search_step(&text(self), false),
            KeyCode::Char(c) if !ctrl && !alt => {
                let mut text = text(self);
                text.push(c);
                self.search_to(text, true);
            }
            KeyCode::Backspace => {
                let mut text = text(self);
                text.pop();
                match text.is_empty() {
                    // backspacing away the last character leaves the
                    // field open and empty, as mc does - the search is
                    // over when Esc says so
                    true => self.quick_search = Some(QuickSearch { text, miss: false }),
                    false => self.search_to(text, true),
                }
            }
            _ => {
                self.quick_search = None;
                self.on_panel_key(key);
            }
        }
    }

    /// Search for `text` from where the cursor is: the cursor moves if
    /// something matches, and the field remembers either way.
    fn search_to(&mut self, text: String, forward: bool) {
        let panel = self.panel();
        let found = panel.find_match(&text, panel.cursor, forward);
        if let Some(pos) = found {
            panel.cursor = pos;
        }
        self.quick_search = Some(QuickSearch {
            miss: found.is_none(),
            text,
        });
    }

    /// ...and the next one after that, which is what C-s does once the
    /// text is typed.
    fn search_step(&mut self, text: &str, forward: bool) {
        let panel = self.panel();
        let from = match forward {
            true => panel.cursor + 1,
            false => panel.cursor.saturating_sub(1),
        };
        let found = panel.find_match(text, from, forward);
        if let Some(pos) = found {
            panel.cursor = pos;
        }
        self.quick_search = Some(QuickSearch {
            text: text.to_string(),
            miss: found.is_none(),
        });
    }

    /// F1 anywhere but the panels: the help, opened at the part about
    /// what is on screen. False = the panels, whose F1 is the keymap's.
    pub(super) fn help_here(&mut self) -> bool {
        let heading = if self.editor().is_some() {
            "# Editor"
        } else if self.viewer().is_some() {
            "# Viewer"
        } else if self.diff().is_some() {
            "# Comparing"
        } else {
            match &self.dialog {
                Some(Dialog::Find(_) | Dialog::FindResults(_)) => "  M-F7",
                Some(Dialog::Fuzzy(_)) => "  M-/",
                Some(Dialog::Transfer(_) | Dialog::Confirm(_)) => "# File operations",
                Some(Dialog::Chmod(_) | Dialog::Chattr(_) | Dialog::Chown(_) | Dialog::Link(_)) => {
                    "# Attributes and links"
                }
                Some(Dialog::Pattern(_)) => "# Marking",
                Some(Dialog::Hotlist(_)) => "  C-\\",
                Some(Dialog::Panelize(_)) => "  C-x !",
                Some(Dialog::Sync(_)) => "  F9>Cmd>Synchronize",
                Some(Dialog::Options(_)) => "# Menus and options",
                Some(_) => "# Editing a line",
                None => return false,
            }
        };
        let (topic, line) = crate::help::locate(heading);
        self.help = Some(HelpState::at(topic, line));
        true
    }

    pub(super) fn on_help_key(&mut self, key: KeyEvent) {
        let Some(help) = self.help.as_mut() else {
            return;
        };
        let rows = help.rows.max(1) as isize;
        help.note = None;
        // `/` asked for a search: the field takes the keys until Enter
        if let Some(field) = help.typing.as_mut() {
            match key.code {
                KeyCode::Esc => help.typing = None,
                KeyCode::Enter => {
                    help.query = field.value.trim().to_string();
                    help.typing = None;
                    help.search(false);
                }
                _ => {
                    field.key(key);
                }
            }
            return;
        }
        match key.code {
            KeyCode::Char('/') | KeyCode::F(7) => {
                help.typing = Some(TextField::new("").with_history("help-search"));
            }
            KeyCode::Char('n') if !help.query.is_empty() => help.search(true),
            KeyCode::Tab => help.step_link(true),
            KeyCode::BackTab => help.step_link(false),
            // Enter with no link on screen closes, as it always has
            KeyCode::Enter => {
                if !help.follow() {
                    self.help = None
                }
            }
            KeyCode::Right => {
                help.follow();
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::F(3) => help.go_back(),
            KeyCode::F(2) | KeyCode::Char('c') => help.go(0, 0),
            KeyCode::F(1) => {
                if let Some(at) = crate::help::topic_named("Using the help") {
                    help.go(at, 0)
                }
            }
            KeyCode::Esc | KeyCode::F(10) | KeyCode::Char('q') => self.help = None,
            KeyCode::Up => help.scroll(-1),
            KeyCode::Down => help.scroll(1),
            KeyCode::PageUp => help.scroll(1 - rows),
            KeyCode::PageDown => help.scroll(rows - 1),
            KeyCode::Home => help.scroll_to(0),
            KeyCode::End => help.scroll_to(usize::MAX),
            _ => {}
        }
    }

    pub(super) fn on_menu_key(&mut self, key: KeyEvent) {
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
                    let menu = ms.menu;
                    self.menu = None;
                    self.run_menu_action(menu, action);
                }
            }
            KeyCode::Char(c) => {
                // MC-style hotkeys: an entry letter of the open menu
                // runs it; otherwise a title letter switches menus.
                let c = c.to_ascii_lowercase();
                let entry = MENUS[ms.menu]
                    .1
                    .iter()
                    .flatten()
                    .find(|(label, ..)| menu_hotkey(label) == Some(c));
                if let Some(&(_, _, action)) = entry {
                    let menu = ms.menu;
                    self.menu = None;
                    self.run_menu_action(menu, action);
                } else if let Some(menu) = MENUS
                    .iter()
                    .position(|(title, _)| menu_hotkey(title) == Some(c))
                {
                    ms.menu = menu;
                    ms.item = first_menu_item(MENUS[menu].1);
                }
            }
            _ => {}
        }
    }

    /// Which panel a menu acts on: mc's Left and Right menus act on
    /// their own panel, everything else on whichever has the focus.
    fn menu_side(menu: usize) -> Option<usize> {
        match menu {
            LEFT_MENU => Some(0),
            RIGHT_MENU => Some(1),
            _ => None,
        }
    }

    pub(super) fn run_menu_action(&mut self, menu: usize, action: Action) {
        match Self::menu_side(menu) {
            Some(side) => self.run_action_on(side, action),
            None => self.run_action(action),
        }
    }

    /// Run a Left/Right menu entry against that menu's panel. The focus
    /// moves there first, and stays: several of these entries open a
    /// dialog that only lands later (filter, panelize, the SFTP link),
    /// and a dialog that acts on a panel other than the focused one is
    /// how you delete the wrong file. mc leaves the focus alone; this
    /// is the one place rcmd would rather be obvious than identical.
    fn run_action_on(&mut self, side: usize, action: Action) {
        match action {
            // the preview and info panes replace the panel whose menu
            // was used, so the focus goes to the *other* one - the one
            // still doing the browsing
            Action::QuickView => self.quick_view_on(side),
            Action::InfoView => self.info_on(side),
            _ => {
                self.active = side;
                self.run_action(action);
            }
        }
    }

    fn quick_view_on(&mut self, side: usize) {
        if self.quick_view.as_ref().is_some_and(|qv| qv.side == side) {
            self.quick_view = None;
            return;
        }
        self.quick_view = None;
        self.active = side ^ 1;
        self.toggle_quick_view();
    }

    fn info_on(&mut self, side: usize) {
        if self.info == Some(side) {
            self.info = None;
            return;
        }
        self.info = None;
        self.active = side ^ 1;
        self.toggle_info();
    }

    /// Actions that mean "do this to the entry under the cursor" have
    /// nothing to act on while the tree has replaced the listing. The
    /// entries are still loaded underneath, which is exactly the
    /// problem: acting on a file nobody can see is not on. (mc runs
    /// F5-F8 against the selected *directory* instead; that belongs
    /// with the file-operation dialogs in S2.)
    fn blocked_in_tree(action: Action) -> bool {
        matches!(
            action,
            Action::View
                | Action::Edit
                | Action::Copy
                | Action::Move
                | Action::Mkdir
                | Action::Pack
                | Action::Delete
                | Action::DeletePerm
                | Action::Mark
                | Action::SelectGroup
                | Action::UnselectGroup
                | Action::InvertSelection
                | Action::DirSize
                | Action::BulkRename
        )
    }

    pub(super) fn run_action(&mut self, action: Action) {
        if self.panels[self.active].list_mode == ListMode::Tree && Self::blocked_in_tree(action) {
            self.status = Some(" not while this panel shows the tree ".into());
            return;
        }
        match action {
            Action::Help => self.help = Some(HelpState::at(0, 0)),
            Action::Menu => {
                if self.external_menubar {
                    self.menu_requested = true;
                } else {
                    self.menu = Some(MenuState {
                        menu: 0,
                        item: first_menu_item(MENUS[0].1),
                    })
                }
            }
            Action::Mark => self.panel().toggle_mark(),
            Action::QuickSearch => {
                self.quick_search = Some(QuickSearch {
                    text: String::new(),
                    miss: false,
                })
            }
            Action::Hotlist => {
                self.dialog = Some(Dialog::Hotlist(HotlistDialog::at(Vec::new(), 0)))
            }
            Action::Filter => self.open_filter(),
            Action::UpDir => self.fallible(|p| p.go_up()),
            Action::Enter => self.fallible(|p| p.enter()),
            Action::FindFile => self.open_find(),
            Action::FuzzyFind => self.open_fuzzy(),
            Action::Panelize => self.open_panelize(),
            Action::CompareDirs => self.open_compare(),
            Action::CompareFiles => self.open_diff(),
            Action::DirSize => self.dir_size(),
            Action::ScreenList => self.open_screen_list(),
            Action::Charset => {
                let now = self.panels[self.active]
                    .charset
                    .map(rcmd_core::charset::label_of);
                self.dialog = Some(Dialog::Charset(charset_row(now)));
            }
            Action::EditConfig => {
                let Some(path) = config::config_path() else {
                    self.status = Some(" no config directory to put one in ".into());
                    return;
                };
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                // the file need not exist yet: this is how the first
                // one gets written
                if !path.exists() {
                    let _ = std::fs::write(&path, "# rcmd configuration - see the README\n");
                }
                let title = path.display().to_string();
                if self.open_internal_editor(&path, title)
                    && let Some(st) = self.editor_mut()
                {
                    // on the editor's own status line, not the panel's:
                    // the panels are not what you are looking at now
                    st.note = Some(" changes apply on the next start ".into());
                }
            }
            Action::LearnKeys => {
                self.dialog = Some(Dialog::Learn(Box::new(LearnDialog {
                    seen: vec![false; LEARN_KEYS.len()],
                    row: 0,
                    last: None,
                })))
            }
            Action::Appearance => {
                let now = crate::theme::list()
                    .iter()
                    .position(|name| *name == self.config.theme)
                    .unwrap_or(0);
                self.dialog = Some(Dialog::Skin(now));
            }
            Action::View => self.open_viewer(false),
            Action::ViewRaw => self.open_viewer(true),
            Action::FilteredView => {
                // mc's shape: the field starts as the file name and the
                // command goes in front of it, so the cursor sits at 0
                let panel = &self.panels[self.active];
                let name = match panel.selected() {
                    Some(entry) if !entry.is_dir() => entry.name.to_string_lossy().into_owned(),
                    _ => {
                        self.status = Some(" cannot view a directory ".into());
                        return;
                    }
                };
                self.dialog = Some(Dialog::Input(
                    InputDialog::new(
                        " Filtered view: command and arguments ",
                        name,
                        InputAction::FilteredView,
                    )
                    .cursor(0),
                ));
            }
            Action::Edit => self.open_editor(),
            Action::Copy => self.open_transfer(false),
            Action::Move if self.in_trash() => self.open_restore(),
            Action::Move => self.open_transfer(true),
            Action::Mkdir => self.open_mkdir(),
            Action::Pack => self.open_pack(),
            Action::Undo => self.open_undo(),
            Action::Sync => self.open_sync(),
            Action::Filters => self.open_filters(),
            Action::CopyNames { paths } => self.copy_names(paths),
            Action::RestoreMarks => self.restore_marks(),
            Action::DirSizeAll => self.dir_size_all(),
            Action::DiskUsage => self.toggle_disk_usage(),
            Action::HidePanel(at) => self.hide_panel(at),
            Action::Wipe => self.open_wipe(),
            Action::Apply => self.open_apply(),
            Action::Shortcut(digit) => self.shortcut(digit),
            Action::FileHistory => self.open_file_history(),
            Action::Checksum => self.open_checksum(),
            Action::VerifyChecksum => self.verify_checksum(),
            Action::Delete => self.open_delete(false),
            Action::DeletePerm => self.open_delete(true),
            Action::SelectGroup => self.open_select(true),
            Action::UnselectGroup => self.open_select(false),
            Action::InvertSelection => self.panel().invert_marks(),
            Action::Quit => {
                // an editor left open on another screen still has the
                // changes in it: that is worth asking about even for
                // someone who turned the ordinary quit question off
                let unsaved = self
                    .screens
                    .iter()
                    .filter(|s| matches!(s, Screen::Editor(st) if st.ed.modified()))
                    .count();
                let message = match unsaved {
                    0 => "Quit rcmd?".to_string(),
                    1 => "1 editor has unsaved changes. Quit rcmd?".to_string(),
                    n => format!("{n} editors have unsaved changes. Quit rcmd?"),
                };
                if self.config.confirm_exit || unsaved > 0 {
                    self.dialog = Some(Dialog::Confirm(ConfirmDialog {
                        title: " Quit ".into(),
                        message,
                        yes: unsaved == 0,
                        paths: Vec::new(),
                        permanent: false,
                        kind: ConfirmKind::Quit,
                        command: None,
                    }));
                } else {
                    self.quit_now();
                }
            }
            Action::Shell => self.pending_exec = Some(Exec::Shell),
            Action::SftpLink => {
                self.dialog = Some(Dialog::Input(InputDialog::new(
                    " Remote link (sftp:// fish:// ftp:// docker:// k8s:// adb:// sudo://) ",
                    "sftp://",
                    InputAction::SftpConnect,
                )));
            }
            Action::HistoryBack => self.history_step(false),
            Action::HistoryForward => self.history_step(true),
            Action::QuickView => self.toggle_quick_view(),
            Action::InfoView => self.toggle_info(),
            Action::UserMenu => self.open_user_menu(),
            Action::UserCommand(i) => self.run_user_command(i),
            Action::Listing(mode) => {
                if mode == ListMode::Tree && !self.panels[self.active].is_local() {
                    self.status = Some(" the tree works on local panels only ".into());
                } else {
                    self.panel().list_mode = mode;
                    self.sync_tree(self.active);
                }
            }
            Action::ListingCycle => {
                let panel = self.panel();
                panel.list_mode = match panel.list_mode {
                    ListMode::Brief => ListMode::Full,
                    ListMode::Full => ListMode::Long,
                    // neither the tree nor a user-defined format is
                    // part of the cycle (mc does not cycle into them
                    // either); both are a deliberate visit
                    ListMode::Long | ListMode::Tree | ListMode::User => ListMode::Brief,
                };
                self.sync_tree(self.active);
            }
            Action::DirTree => {
                let panel = &self.panels[self.active];
                if panel.is_local() {
                    let tree = Tree::new(&panel.local_cwd(), panel.show_hidden);
                    self.dialog = Some(Dialog::Tree(Box::new(tree)));
                } else {
                    self.status = Some(" the tree works on local panels only ".into());
                }
            }
            Action::OtherSameDir => self.other_panel_dir(false),
            Action::OtherOpenDir => self.other_panel_dir(true),
            Action::Reload => self.fallible(|p| p.reload().map(|()| true)),
            Action::FlatView => self.flat_view(),
            Action::FindDuplicates => self.find_duplicates(),
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
            Action::ToggleHidden => {
                self.fallible(|p| p.toggle_hidden().map(|()| true));
                // the figure was scanned under the old flag, so rebuild
                // it rather than leave the tree and the listing at odds
                let side = self.active;
                if self.trees[side].is_some() {
                    let path = self.trees[side].as_ref().and_then(Tree::selected_path);
                    self.trees[side] = None;
                    self.sync_tree(side);
                    if let (Some(tree), Some(path)) = (self.trees[side].as_mut(), path) {
                        tree.reveal(&path);
                    }
                }
            }
            Action::Options => {
                let cfg = &self.config;
                let mut values = [false; OPT_COUNT];
                values[Opt::Hidden as usize] = self.panels[self.active].show_hidden;
                values[Opt::Lynx as usize] = cfg.lynx_on();
                values[Opt::Mouse as usize] = cfg.mouse;
                values[Opt::Watch as usize] = cfg.watch;
                values[Opt::RestoreOtherDir as usize] = cfg.restore_other_dir;
                values[Opt::Git as usize] = cfg.git;
                values[Opt::ConfirmDelete as usize] = cfg.confirm_delete;
                values[Opt::ConfirmOverwrite as usize] = cfg.confirm_overwrite;
                values[Opt::ConfirmExit as usize] = cfg.confirm_exit;
                values[Opt::ConfirmHotlistDelete as usize] = cfg.confirm_hotlist_delete;
                values[Opt::ConfirmExecute as usize] = cfg.confirm_execute;
                values[Opt::Subshell as usize] = cfg.subshell;
                values[Opt::ExternalEditor as usize] = cfg.editor == "external";
                values[Opt::HorizontalSplit as usize] = cfg.horizontal_split();
                values[Opt::MenuBar as usize] = cfg.show_menubar;
                values[Opt::StatusLine as usize] = cfg.show_status;
                values[Opt::MiniStatus as usize] = cfg.show_mini_status;
                values[Opt::FreeSpace as usize] = cfg.show_free_space;
                values[Opt::CommandLine as usize] = cfg.show_cmdline;
                values[Opt::KeyBar as usize] = cfg.show_keybar;
                let ratio = cfg.ratio();
                self.dialog = Some(Dialog::Options(OptionsDialog {
                    // start on the first setting, not the heading
                    cursor: 1,
                    values,
                    ratio,
                    ok: true,
                }));
            }
            Action::Sort(key) => self.panel().set_sort(key),
            Action::SortReverse => {
                let panel = self.panel();
                panel.sort_reverse = !panel.sort_reverse;
                panel.resort();
            }
            Action::ScreenTop | Action::ScreenMiddle | Action::ScreenBottom => {
                self.cursor_on_screen(action)
            }
            Action::JobReport => self.show_job_report(),
            Action::Trash => self.connect_remote(rcmd_core::trashcan::PREFIX),
            Action::DiffHead => self.open_diff_head(),
            Action::GitStage => self.git_index(true),
            Action::GitUnstage => self.git_index(false),
            Action::GitBranch => self.open_branches(),
            Action::Palette => self.open_palette(),
            Action::Connections => self.open_connections(),
            Action::Extract => self.extract_archives(),
            Action::HotlistAdd => {
                let panel = &self.panels[self.active];
                let path = match panel.is_remote() {
                    true => panel.display_path(),
                    false => panel.local_cwd().display().to_string(),
                };
                let label = Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                self.ask_hotlist_label(" Add to hotlist ", label, Vec::new(), None, path);
            }
            Action::ToggleSplit => {
                let horizontal = !self.config.horizontal_split();
                self.config.split = if horizontal { "horizontal" } else { "vertical" }.into();
                let split = self.config.split.clone();
                if let Err(err) = state::update(move |s| s.split = Some(split)) {
                    self.status = Some(format!(" could not save state: {err} "));
                }
            }
            Action::SortMix => {
                let panel = self.panel();
                panel.mix_dirs = !panel.mix_dirs;
                panel.resort();
                let now = if panel.mix_dirs {
                    " directories mixed with the files "
                } else {
                    " directories first "
                };
                self.status = Some(now.into());
            }
            Action::SortCase => {
                let panel = self.panel();
                panel.sort_case = !panel.sort_case;
                panel.resort();
                let now = if panel.sort_case {
                    " names sorted case-sensitively "
                } else {
                    " names sorted in any case "
                };
                self.status = Some(now.into());
            }
            Action::EditNew => self.open_edit_new(),
            Action::CopyHere => self.open_transfer_here(false),
            Action::MoveHere => self.open_transfer_here(true),
            Action::PasteTags => self.insert_tagged_names(),
            Action::PastePath => {
                let text = format!("{} ", shell_quote(&self.panels[self.active].display_path()));
                self.insert_cmdline(&text);
            }
            Action::QuickCd => {
                self.dialog = Some(Dialog::Input(InputDialog::new(
                    " Quick cd ",
                    "",
                    InputAction::QuickCd,
                )));
            }
            Action::Repaint => self.repaint = true,
            Action::BulkRename => self.open_bulk_rename(),
            Action::VfsList => self.open_vfs_list(self.active),
            Action::Drives(side) => self.open_vfs_list(side),
            Action::Jobs => {
                if self.jobs.is_empty() {
                    self.status = Some(" no jobs running ".into());
                } else {
                    self.dialog = Some(Dialog::Jobs(0));
                }
            }
            Action::HistoryList => {
                if self.cmdline.history().is_empty() {
                    self.status = Some(" command history is empty ".into());
                } else {
                    // newest first, so the row under the cursor is the
                    // command you most likely want back
                    self.dialog = Some(Dialog::History(0));
                }
            }
            Action::DirHistory => {
                let (entries, pos) = self.panels[self.active].history_entries();
                if entries.len() < 2 {
                    self.status = Some(" directory history: nowhere else yet ".into());
                } else {
                    // newest first, the cursor on where the panel is now
                    let row = entries.len() - 1 - pos.min(entries.len() - 1);
                    self.dialog = Some(Dialog::DirHistory(row));
                }
            }
        }
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
            hex: false,
            rows: 1,
        });
        self.update_quick_view();
    }

    /// Keep the preview in sync with the cursor of the browsing panel;
    /// called every loop iteration, reopens only when the file changes.
    pub(super) fn update_quick_view(&mut self) {
        if self.quick_view.is_none() {
            return;
        }
        // the preview follows the other panel's cursor, and what it
        // shows can change without a key of its own
        self.dirty = true;
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
            qv.note = "no preview here - F3 views remote/archive files".into();
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

    /// Enter on the cursor entry: directories and archives first; a
    /// plain file consults the [[open]] rules (local panels only, the
    /// first matching glob wins, case-insensitive). The `enter` keymap
    /// action (lynx-motion Right) stays dirs-only on purpose.
    /// Enter on an archive a panel cannot open where it lies - on a
    /// server, or inside the archive the panel is in: a local copy is
    /// made, as F3 makes one, and the panel goes into that. False = not
    /// such an archive.
    fn enter_nested_archive(&mut self) -> bool {
        let panel = &self.panels[self.active];
        if panel.is_local() {
            return false;
        }
        let Some(entry) = panel.selected().filter(|e| {
            e.kind == rcmd_core::entry::EntryKind::File && rcmd_core::vfs::is_archive_name(&e.name)
        }) else {
            return false;
        };
        let name = entry.name.clone();
        let source = panel.cwd.join(&name);
        let copied = crate::scratch::create(&name.to_string_lossy()).and_then(|(mut out, copy)| {
            let done = panel
                .fs
                .open_read(&source)
                .and_then(|mut reader| std::io::copy(&mut reader, &mut out));
            match done {
                Ok(_) => Ok(copy),
                Err(err) => {
                    let _ = std::fs::remove_file(&copy);
                    Err(err)
                }
            }
        });
        let entered = copied.and_then(|copy| {
            self.panels[self.active]
                .enter_nested(copy.clone(), name.clone())
                .inspect_err(|_| {
                    let _ = std::fs::remove_file(&copy);
                })
        });
        if let Err(err) = entered {
            self.status = Some(format!(" {}: {err} ", name.to_string_lossy()));
        }
        true
    }

    pub(super) fn enter_or_open(&mut self) {
        if self.enter_user_vfs() {
            return;
        }
        match self.panels[self.active].enter() {
            Ok(true) => return,
            Ok(false) => {}
            Err(err) => {
                self.status = Some(format!(" {err} "));
                return;
            }
        }
        if self.enter_nested_archive() {
            return;
        }
        let panel = &self.panels[self.active];
        if !panel.is_local() {
            // a file in the trash has nowhere to run; what it is asked
            // about is where it came from
            if let Some(entry) = panel.selected()
                && let Some(from) = panel.fs.note(&panel.cwd.join(&entry.name))
            {
                self.status = Some(format!(
                    " {} came {from} - F6 puts it back ",
                    panel.name_of(entry)
                ));
            }
            return;
        }
        let Some(entry) = panel.selected() else {
            return;
        };
        if entry.is_parent() || entry.is_dir() {
            return;
        }
        let name = entry.name.to_string_lossy().into_owned();
        let dir = panel.cwd.clone();
        let path = dir.join(&entry.name);
        // `file -b` is asked once, and only if a rule has type =
        let mut probed: Option<String> = None;
        let mut file_type = || {
            probed
                .get_or_insert_with(|| crate::config::file_type_of(&path))
                .clone()
        };
        let run = match self
            .config
            .open
            .iter()
            .find(|rule| rule.matches(&name, &dir, &mut file_type))
        {
            Some(rule) => rule.run.clone(),
            None => {
                self.desktop_open(&path);
                return;
            }
        };
        let cmd = self.expand_macros(&run);
        if self.config.confirm_execute {
            self.dialog = Some(Dialog::Confirm(ConfirmDialog {
                title: " Execute ".into(),
                message: format!("Run: {}", crate::ui::tail(&cmd, 60)),
                yes: true,
                paths: Vec::new(),
                permanent: false,
                kind: ConfirmKind::Execute,
                command: Some(cmd),
            }));
            return;
        }
        self.pending_exec = Some(Exec::Quiet(cmd));
    }

    /// `%f` cursor file, `%d` this directory, `%D` the other panel's,
    /// `%t` marked files, `%%` a literal percent - all shell-quoted.
    /// One panel's worth of macro material: the cursor file, the marked
    /// files, and the directory - all shell-quoted, because a filename
    /// is not a word.
    fn macro_side(&self, side: usize) -> (String, String, String) {
        let panel = &self.panels[side];
        let file = panel
            .selected()
            .filter(|e| !e.is_parent())
            .map(|e| shell_quote(&panel.name_of(e)))
            .unwrap_or_default();
        let tagged = panel
            .entries
            .iter()
            .filter(|e| panel.is_marked(e))
            .map(|e| shell_quote(&panel.name_of(e)))
            .collect::<Vec<_>>()
            .join(" ");
        let dir = shell_quote(&panel.local_cwd().to_string_lossy());
        (file, tagged, dir)
    }

    /// Expand mc's macros. Stops at the first `%{question}` and hands
    /// back what it has, because that one has to be asked before the
    /// rest can be spent - see [`Expanded`].
    ///
    /// `%u` and `%U` spend the marks: mc drops them once the command
    /// has them, which is why this takes `&mut self`.
    fn expand_macros_asking(&mut self, template: &str) -> Expanded {
        let material = Macros {
            here: self.macro_side(self.active),
            there: self.macro_side(self.active ^ 1),
            clip: clip_file_read().unwrap_or_default(),
        };
        let (expanded, untag) = expand_template(template, &material);
        // untag is in this-panel/other-panel terms; the panels are not
        let mut sides = [false, false];
        sides[self.active] = untag[0];
        sides[self.active ^ 1] = untag[1];
        self.spend_marks(sides);
        expanded
    }

    /// `%u` / `%U`: the marks go once the command has them, which is
    /// mc's rule and the difference between those and `%t` / `%T`.
    fn spend_marks(&mut self, untag: [bool; 2]) {
        for (at, want) in untag.into_iter().enumerate() {
            if want {
                self.remember_marks(at);
                self.panels[at].marked.clear();
            }
        }
    }

    /// Expansion where there is nobody to ask - a `%{...}` in a view
    /// filter has no dialog to open, the viewer being mid-open.
    pub(super) fn expand_macros(&mut self, template: &str) -> String {
        match self.expand_macros_asking(template) {
            Expanded::Done(cmd) => cmd,
            Expanded::Ask { before, rest, .. } => {
                format!("{before}{}", self.expand_macros(&rest))
            }
        }
    }

    /// Run a command template: expanded if it can be, asked about first
    /// if it carries a `%{question}`.
    pub(super) fn run_macro_command(&mut self, template: &str, quiet: bool) {
        match self.expand_macros_asking(template) {
            Expanded::Done(cmd) => {
                self.pending_exec = Some(match quiet {
                    true => Exec::Quiet(cmd),
                    false => Exec::Command(cmd),
                })
            }
            Expanded::Ask {
                question,
                before,
                rest,
            } => self.ask_macro(question, before, rest, quiet),
        }
    }

    fn ask_macro(&mut self, question: String, before: String, rest: String, quiet: bool) {
        self.dialog = Some(Dialog::Input(InputDialog::new(
            format!(" {} ", question.trim()),
            "",
            InputAction::MacroPrompt {
                before,
                rest,
                quiet,
            },
        )));
    }

    /// The answer to one `%{...}`, and on with the rest of the template.
    pub(super) fn finish_macro(&mut self, answer: &str, before: String, rest: String, quiet: bool) {
        let before = format!("{before}{answer}");
        match self.expand_macros_asking(&rest) {
            Expanded::Done(tail) => {
                let cmd = format!("{before}{tail}");
                self.pending_exec = Some(match quiet {
                    true => Exec::Quiet(cmd),
                    false => Exec::Command(cmd),
                })
            }
            Expanded::Ask {
                question,
                before: mid,
                rest,
            } => self.ask_macro(question, format!("{before}{mid}"), rest, quiet),
        }
    }

    /// F2: the entries that apply to what the panels are showing.
    /// mc reads a `.mc.menu` in the current directory before its own;
    /// rcmd puts those first and keeps the configured ones after them,
    /// because a project's menu is an addition to your own rather than
    /// a replacement for it.
    fn open_user_menu(&mut self) {
        let mut menu: Vec<UserCommand> = Vec::new();
        let local = self.local_menu();
        let have_local = !local.is_empty();
        menu.extend(local);
        menu.extend(self.config.commands.iter().cloned());
        let menu = self.applicable(menu);
        if menu.is_empty() {
            self.status = Some(match self.config.commands.is_empty() {
                true => " no [[commands]] in the config - see F1 ".into(),
                false => " no user-menu entry applies here ".into(),
            });
            return;
        }
        self.dialog = Some(Dialog::UserMenu(Box::new(UserMenuDialog {
            menu,
            path: Vec::new(),
            row: 0,
            local: have_local,
        })));
    }

    /// A `.mc.menu` in the panel's directory, in mc's own format.
    fn local_menu(&self) -> Vec<UserCommand> {
        let panel = &self.panels[self.active];
        if !panel.is_local() {
            return Vec::new();
        }
        let path = panel.local_cwd().join(".mc.menu");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        crate::mcimport::parse_menu(&text).0
    }

    /// Keep the entries whose condition holds, submenus and all. A
    /// submenu whose entries all dropped out drops out with them -
    /// an empty section is a dead end with a name.
    fn applicable(&self, entries: Vec<UserCommand>) -> Vec<UserCommand> {
        let cx = self.menu_context();
        let cx = rcmd_core::usermenu::Context {
            file: &cx.0,
            dir: &cx.1,
            kind: cx.2,
            tagged: cx.3,
            other_file: &cx.4,
            other_dir: &cx.5,
            other_kind: cx.6,
            other_tagged: cx.7,
        };
        fn keep(entries: Vec<UserCommand>, cx: &rcmd_core::usermenu::Context) -> Vec<UserCommand> {
            entries
                .into_iter()
                .filter_map(|mut entry| {
                    let when = entry.when.clone().unwrap_or_default();
                    if !rcmd_core::usermenu::matches(&when, cx) {
                        return None;
                    }
                    if entry.is_submenu() {
                        entry.entries = keep(std::mem::take(&mut entry.entries), cx);
                        if entry.entries.is_empty() {
                            return None;
                        }
                    }
                    Some(entry)
                })
                .collect()
        }
        keep(entries, &cx)
    }

    /// What the conditions look at, as owned strings - the panels are
    /// borrowed for the whole evaluation otherwise.
    #[allow(clippy::type_complexity)]
    fn menu_context(
        &self,
    ) -> (
        String,
        String,
        rcmd_core::usermenu::FileKind,
        bool,
        String,
        String,
        rcmd_core::usermenu::FileKind,
        bool,
    ) {
        let side = |i: usize| {
            let panel = &self.panels[i];
            let entry = panel.selected().filter(|e| !e.is_parent());
            let name = entry.map(|e| panel.name_of(e)).unwrap_or_default();
            let kind = entry.map(menu_kind).unwrap_or_default();
            let marked = panel.entries.iter().any(|e| panel.is_marked(e));
            (name, panel.display_path(), kind, marked)
        };
        let (file, dir, kind, tagged) = side(self.active);
        let (other_file, other_dir, other_kind, other_tagged) = side(self.active ^ 1);
        (
            file,
            dir,
            kind,
            tagged,
            other_file,
            other_dir,
            other_kind,
            other_tagged,
        )
    }

    fn run_user_command(&mut self, i: usize) {
        let Some(cmd) = self.config.commands.get(i) else {
            return;
        };
        let run = cmd.run.clone();
        self.run_macro_command(&run, false);
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
    /// full sftp:// or ftp:// URL (routed through the connection cache).
    pub(super) fn navigate(&mut self, target: &str) {
        self.navigate_panel(self.active, target);
    }

    /// The list of everywhere this panel could go: the connections and
    /// archives rcmd has open, and what the machine has mounted. Far
    /// calls the second half the drive menu and keeps it on M-F1/M-F2,
    /// which is why those name a side rather than taking the active
    /// one.
    fn open_vfs_list(&mut self, panel: usize) {
        let rows = self.vfs_rows();
        self.dialog = Some(Dialog::Vfs(VfsDialog {
            rows,
            selected: 0,
            panel,
        }));
    }

    /// Move one named panel, which is not always the active one.
    pub(super) fn navigate_panel(&mut self, at: usize, target: &str) {
        if is_remote_url(target) {
            self.active = at;
            self.connect_remote(target);
            return;
        }
        let path = PathBuf::from(target);
        let panel = &mut self.panels[at];
        let result = if panel.is_local() {
            panel.cd(path)
        } else {
            panel.to_local(path)
        };
        if let Err(err) = result {
            self.status = Some(format!(" {err} "));
        }
    }

    /// Ctrl+Space: recursive size of the selected directory, computed in
    /// the background and written into the Size column when done.
    fn dir_size(&mut self) {
        if self.du.is_some() {
            self.status = Some(" a size scan is already running ".into());
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
        self.start_du(name);
        self.panel().move_down();
    }

    /// M-Del: overwrite what is marked and then delete it. The confirm
    /// dialog says what that is and is not worth, since the difference
    /// matters more here than anywhere else in the program.
    fn open_wipe(&mut self) {
        if !self.require_local() {
            return;
        }
        let paths = self.panels[self.active].targets();
        if paths.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let what = self.describe(&paths);
        self.dialog = Some(Dialog::Confirm(ConfirmDialog {
            title: " Wipe ".into(),
            message: format!(
                "Overwrite {what} and delete? (no undo, and no promise against a snapshot)"
            ),
            yes: false,
            paths,
            permanent: true,
            kind: ConfirmKind::Wipe,
            command: None,
        }));
    }

    pub(super) fn start_wipe(&mut self, paths: Vec<PathBuf>) {
        let count = paths.len();
        let handle = fsops::spawn_wipe(paths);
        self.push_job(format!(" wipe {count} item(s) "), handle);
    }

    /// C-g: one command per marked file. `[[commands]]` hands every
    /// marked file to one invocation, which is the right shape for
    /// `tar` and the wrong one for `convert`.
    fn open_apply(&mut self) {
        if self.panels[self.active].targets().is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        self.dialog = Some(Dialog::Input(InputDialog::new(
            " Apply to each marked file ",
            "",
            InputAction::Apply,
        )));
    }

    /// Remember a file the viewer or the editor opened. The list is
    /// the state file's, so it is still there next session.
    pub(super) fn note_file(&mut self, path: &Path) {
        let path = path.display().to_string();
        self.file_history.retain(|old| old != &path);
        self.file_history.push(path.clone());
        let over = self.file_history.len().saturating_sub(FILE_HISTORY);
        self.file_history.drain(..over);
        let keep = self.file_history.clone();
        let _ = state::update(move |s| s.file_history = keep);
    }

    /// Far's M-F11: what has been viewed and edited, newest first.
    /// Enter puts the panel on it, with the cursor on the file.
    fn open_file_history(&mut self) {
        if self.file_history.is_empty() {
            self.status = Some(" nothing viewed or edited yet ".into());
            return;
        }
        self.dialog = Some(Dialog::FileHistory(0));
    }

    /// Enter on one of those rows: go to the file, wherever it is.
    pub(super) fn go_to_file(&mut self, path: &Path) {
        let Some(dir) = path.parent() else { return };
        let panel = &mut self.panels[self.active];
        let moved = match panel.is_remote() {
            true => panel.to_local(dir.to_path_buf()),
            false => panel.cd(dir.to_path_buf()),
        };
        if let Err(err) = moved {
            self.status = Some(format!(" {err} "));
            return;
        }
        if let Some(name) = path.file_name() {
            let panel = &mut self.panels[self.active];
            if let Some(at) = panel.entries.iter().position(|e| e.name == name) {
                panel.cursor = at;
            }
        }
    }

    /// Far's folder shortcuts, as hotlist entries: `C-x 3` goes to the
    /// one labelled `3`, and where there is none it makes one here.
    /// Ten numbered places you never have to look at a list for, and
    /// nothing new to store - the hotlist already persists, reorders
    /// and renames, and a shortcut is a hotlist entry with a short
    /// name.
    fn shortcut(&mut self, digit: u8) {
        let label = digit.to_string();
        let found = self
            .config
            .hotlist
            .iter()
            .find(|entry| !entry.is_group() && entry.label == label)
            .map(|entry| entry.path.clone());
        match found {
            Some(path) => self.hotlist_go(&path),
            None => {
                let panel = &self.panels[self.active];
                let path = match panel.is_remote() {
                    true => panel.display_path(),
                    false => panel.local_cwd().display().to_string(),
                };
                self.config.hotlist.push(HotEntry {
                    label,
                    path: path.clone(),
                    entries: Vec::new(),
                });
                self.save_hotlist();
                self.status = Some(format!(" shortcut {digit} is now {path} "));
            }
        }
    }

    /// Write a `sha256sum`-format file for what is marked - the
    /// checksum you hand to someone else, where Verify on the copy
    /// form is the one you keep to yourself.
    fn open_checksum(&mut self) {
        if !self.require_local() {
            return;
        }
        let paths = self.panels[self.active].targets();
        if paths.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let value = self.panels[self.active]
            .local_cwd()
            .join("SHA256SUMS")
            .display()
            .to_string();
        self.dialog = Some(Dialog::Input(InputDialog::new(
            format!(" Checksum {} into ", self.describe(&paths)),
            value,
            InputAction::Checksum { paths },
        )));
    }

    /// Check the checksum file under the cursor, in its own directory,
    /// as `sha256sum -c` would.
    fn verify_checksum(&mut self) {
        if !self.require_local() {
            return;
        }
        let panel = &self.panels[self.active];
        let Some(entry) = panel.selected().filter(|e| !e.is_dir()) else {
            self.status = Some(" not a file ".into());
            return;
        };
        let sums = panel.local_cwd().join(&entry.name);
        let dir = panel.local_cwd();
        let handle = fsops::spawn_verify_checksums(dir, sums.clone());
        self.push_job_kind(
            format!(" check {} ", entry.name.to_string_lossy()),
            handle,
            true,
        );
    }

    /// The answer to C-g: the template, expanded once per marked file
    /// and run as one script, so the output, the ordering and the
    /// Ctrl+C are the terminal's, as they are for every other command.
    pub(super) fn run_apply(&mut self, template: &str) {
        let template = template.trim();
        if template.is_empty() {
            return;
        }
        if template.contains("%{") {
            self.status = Some(" a %{question} cannot be asked once per file ".into());
            return;
        }
        let panel = &self.panels[self.active];
        let names: Vec<String> = panel
            .targets()
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| shell_quote(&name.to_string_lossy()))
            .collect();
        if names.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let dir = shell_quote(&panel.local_cwd().to_string_lossy());
        let there = self.macro_side(self.active ^ 1);
        let clip = clip_file_read().unwrap_or_default();
        let mut lines = Vec::with_capacity(names.len());
        for name in names {
            let material = Macros {
                // one file is the cursor file, the marked set and the
                // selection, all three: this command is about it alone
                here: (name.clone(), name.clone(), dir.clone()),
                there: there.clone(),
                clip: clip.clone(),
            };
            if let (Expanded::Done(cmd), _) = expand_template(template, &material) {
                lines.push(cmd);
            }
        }
        self.pending_exec = Some(Exec::Command(lines.join("\n")));
    }

    /// C-F1 / C-F2: hide a panel, and give the screen to the other.
    /// The hidden one keeps its directory, its marks and its listing -
    /// it is not showing, which is not the same as not being there.
    fn hide_panel(&mut self, at: usize) {
        self.hidden = match self.hidden {
            Some(was) if was == at => None,
            _ => Some(at),
        };
        // never leave the focus on a panel nobody can see
        if self.hidden == Some(self.active) {
            self.active ^= 1;
        }
        self.dirty = true;
    }

    /// Which panel is hidden, if any.
    pub fn hidden_panel(&self) -> Option<usize> {
        self.hidden
    }

    /// The marked names, or the cursor one, on the clipboard. Far
    /// keeps this on C-Ins and C-A-Ins; the editor has talked to the
    /// desktop clipboard since 4.0 and the panel never did.
    fn copy_names(&mut self, paths: bool) {
        let panel = &self.panels[self.active];
        let names: Vec<String> = match paths {
            true => panel
                .targets()
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            false => panel
                .targets()
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect(),
        };
        if names.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let count = names.len();
        let text = names.join("\n");
        let what = if paths { "path" } else { "name" };
        self.status = Some(match clipboard_set(&text) {
            true => format!(" {count} {what}(s) copied "),
            // the clipboard file still has it, which is what %q reads
            false => format!(" {count} {what}(s) copied to the clipboard file "),
        });
    }

    /// Put back the marks that the last operation, reload or select
    /// cleared. Far calls it restore selection, and it is the answer to
    /// having pressed the wrong key after marking forty files.
    fn restore_marks(&mut self) {
        match self.marks_before[self.active].take() {
            Some(marks) => {
                let count = marks.len();
                self.panels[self.active].marked = marks;
                self.status = Some(format!(" {count} mark(s) restored "));
            }
            None => self.status = Some(" no marks to restore ".into()),
        }
    }

    /// Remember what is marked before something clears it.
    pub(super) fn remember_marks(&mut self, at: usize) {
        let marked = &self.panels[at].marked;
        if !marked.is_empty() {
            self.marks_before[at] = Some(marked.clone());
        }
    }

    /// Every directory in the panel, one after the other. Total
    /// Commander does the whole listing in one keystroke; the scans are
    /// still one at a time, which is what the disk wants anyway.
    fn dir_size_all(&mut self) {
        if self.du.is_some() {
            self.status = Some(" a size scan is already running ".into());
            return;
        }
        let mut names: Vec<std::ffi::OsString> = self.panels[self.active]
            .entries
            .iter()
            .filter(|entry| entry.is_dir() && !entry.is_parent())
            .map(|entry| entry.name.clone())
            .collect();
        if names.is_empty() {
            self.status = Some(" no directories here ".into());
            return;
        }
        let first = names.remove(0);
        self.du_queue = names;
        self.start_du(first);
    }

    /// Start one scan, wherever it came from.
    pub(super) fn start_du(&mut self, name: std::ffi::OsString) {
        let panel = &self.panels[self.active];
        let cwd = panel.cwd.clone();
        let rx = if panel.is_local() {
            fsops::spawn_dir_size(cwd.join(&name))
        } else {
            // sftp and archive panels size through their provider
            fsops::spawn_dir_size_fs(panel.fs.clone(), cwd.join(&name))
        };
        self.du = Some(DuJob {
            rx,
            panel: self.active,
            cwd,
            name,
        });
    }

    /// C-x d: ask how, then compare. mc asks every time, and the
    /// answer matters - "the same size and date" and "the same bytes"
    /// are different questions.
    fn open_compare(&mut self) {
        if self.panels[0].archive.is_some() || self.panels[1].archive.is_some() {
            self.status = Some(" cannot compare inside an archive ".into());
            return;
        }
        self.compare_then_sync = false;
        self.dialog = Some(Dialog::Compare(0));
    }

    /// F9 > Command > Synchronize: the same comparison C-x d runs, and
    /// then the plan it implies. Marking the differences and leaving F5
    /// to guess the direction is what mc stops at, and it is wrong
    /// exactly when the differences run both ways.
    fn open_sync(&mut self) {
        if self.panels[0].archive.is_some() || self.panels[1].archive.is_some() {
            self.status = Some(" cannot synchronize inside an archive ".into());
            return;
        }
        self.compare_then_sync = true;
        self.dialog = Some(Dialog::Compare(0));
    }

    /// Whether the comparison dialog on screen was opened by
    /// Synchronize, which is all the drawing needs to know.
    pub fn comparing_to_sync(&self) -> bool {
        self.compare_then_sync
    }

    /// Synchronize's comparison: the two trees, walked on a thread,
    /// through whatever each panel is on.
    fn start_sync_scan(&mut self, mode: rcmd_core::compare::Mode) {
        self.compare_then_sync = false;
        let side = |p: &Panel| (p.fs.clone(), p.cwd.clone());
        let handle =
            rcmd_core::sync::spawn_scan(side(&self.panels[0]), side(&self.panels[1]), mode);
        self.sync_scan = Some(handle);
        self.status = Some(" comparing the two trees… Esc cancels ".into());
    }

    pub(super) fn drain_sync_scan(&mut self) {
        let Some(scan) = self.sync_scan.as_ref() else {
            return;
        };
        let mut done = None;
        while let Ok(event) = scan.events.try_recv() {
            match event {
                rcmd_core::sync::ScanEvent::At(rel) => {
                    self.status = Some(format!(
                        " comparing {}… Esc cancels ",
                        if rel.as_os_str().is_empty() {
                            ".".to_string()
                        } else {
                            rel.display().to_string()
                        }
                    ));
                }
                rcmd_core::sync::ScanEvent::Done(result) => done = Some(result),
            }
        }
        let Some(result) = done else {
            return;
        };
        self.sync_scan = None;
        self.dirty = true;
        match result {
            Ok(diffs) if diffs.is_empty() => {
                self.status = Some(" the two directories agree ".into());
            }
            Ok(diffs) => self.open_sync_plan(diffs),
            Err(err) => self.status = Some(format!(" synchronize: {err} ")),
        }
    }

    /// The plan, from what the comparison found.
    fn open_sync_plan(&mut self, diffs: Vec<rcmd_core::sync::Difference>) {
        let rows = diffs
            .iter()
            .map(|d| SyncRow::plan(d, Mirror::Off))
            .collect();
        self.status = None;
        self.dialog = Some(Dialog::Sync(Box::new(SyncDialog {
            rows,
            diffs,
            mirror: Mirror::Off,
            cursor: 0,
            top: 0,
            left: self.panels[0].display_path(),
            right: self.panels[1].display_path(),
            mask: None,
        })));
    }

    /// Run the plan: one job for all of it, copying each way and
    /// deleting where a row says so.
    pub(super) fn start_sync(&mut self, d: &SyncDialog) {
        let steps: Vec<(PathBuf, fsops::SyncStep)> = d
            .rows
            .iter()
            .filter(|r| r.on)
            .map(|r| (r.rel.clone(), r.step))
            .collect();
        if steps.is_empty() {
            self.status = Some(" nothing left switched on ".into());
            return;
        }
        let side = |p: &Panel| (p.fs.clone(), p.cwd.clone());
        let title = format!(" synchronize {} item(s) ", steps.len());
        let handle = fsops::spawn_sync(side(&self.panels[0]), side(&self.panels[1]), steps);
        self.push_job(title, handle);
    }

    /// F3 on a plan row: the two files it is about, side by side, and
    /// back to the plan when the diff closes.
    pub(super) fn sync_row_diff(&mut self, d: Box<SyncDialog>) {
        let Some(row) = d.rows.get(d.cursor).filter(|r| r.two_files()) else {
            self.status = Some(" F3 shows a row that has a file on both sides ".into());
            self.dialog = Some(Dialog::Sync(d));
            return;
        };
        let rel = row.rel.clone();
        let source = |p: &Panel, entry: Option<&rcmd_core::entry::Entry>| DiffSource {
            fs: p.fs.clone(),
            path: p.cwd.join(&rel),
            title: format!("{}/{}", p.display_path(), rel.display()),
            charset: p.charset,
            size: entry.map_or(0, |e| e.size),
        };
        let diff = d.diffs.iter().find(|x| x.rel == rel);
        let left = source(&self.panels[0], diff.and_then(|x| x.left.as_ref()));
        let right = source(&self.panels[1], diff.and_then(|x| x.right.as_ref()));
        if self.open_diff_pair(left, right) {
            self.sync_return = Some(d);
        } else {
            self.dialog = Some(Dialog::Sync(d));
        }
    }

    pub(super) fn compare_dirs(&mut self, mode: rcmd_core::compare::Mode) {
        use rcmd_core::compare;
        if self.compare_then_sync {
            self.start_sync_scan(mode);
            return;
        }
        if let Some(running) = self.compare.take() {
            running.handle.cancel();
        }
        let diff =
            compare::compare_listings(&self.panels[0].entries, &self.panels[1].entries, mode);
        self.remember_marks(0);
        self.remember_marks(1);
        self.panels[0].marked.clear();
        self.panels[1].marked.clear();
        for name in &diff.left {
            self.panels[0].marked.insert(name.clone());
        }
        for name in &diff.right {
            self.panels[1].marked.insert(name.clone());
        }
        let known = diff.count();
        if diff.undecided.is_empty() {
            self.status = Some(format!(" {known} difference(s) marked "));
            return;
        }
        // the pairs the listing could not settle are read on a worker
        // thread, and mark themselves as they are found to differ
        let total = diff.undecided.len();
        let handle = compare::spawn_content_compare(
            (self.panels[0].fs.clone(), self.panels[0].cwd.clone()),
            (self.panels[1].fs.clone(), self.panels[1].cwd.clone()),
            diff.undecided,
        );
        self.status = Some(format!(" comparing {total} pair(s)… Esc cancels "));
        self.compare = Some(CompareState {
            handle,
            total,
            done: 0,
        });
    }

    /// Matches from a thorough compare, as they arrive.
    pub(super) fn drain_compare(&mut self) {
        let Some(state) = self.compare.as_mut() else {
            return;
        };
        let mut finished = false;
        let mut differing = Vec::new();
        while let Ok(event) = state.handle.events.try_recv() {
            match event {
                rcmd_core::compare::CompareEvent::Differs(name) => {
                    state.done += 1;
                    differing.push(name);
                }
                rcmd_core::compare::CompareEvent::Done => finished = true,
            }
        }
        for name in differing {
            self.panels[0].marked.insert(name.clone());
            self.panels[1].marked.insert(name);
            self.dirty = true;
        }
        let (total, done) = self
            .compare
            .as_ref()
            .map(|c| (c.total, c.done))
            .unwrap_or((0, 0));
        if finished {
            self.compare = None;
            let marked = self.panels[0].marked.len().max(self.panels[1].marked.len());
            self.status = Some(format!(" {marked} difference(s) marked ({total} read) "));
            self.dirty = true;
        } else {
            self.status = Some(format!(" comparing… {done} differ so far - Esc cancels "));
        }
    }

    /// A recursive chmod/chown, as a job with a progress dialog and a
    /// Cancel button - which is what you want halfway down a big tree.
    pub(super) fn start_attrs_job(&mut self, paths: Vec<PathBuf>, attrs: fsops::Attrs, verb: &str) {
        self.jobs.push(Job {
            title: format!(" {verb} {} item(s), recursively ", paths.len()),
            handle: fsops::spawn_attrs(paths, attrs, true),
            total_files: 0,
            total_bytes: 0,
            files_done: 0,
            bytes_done: 0,
            current: PathBuf::new(),
            file_done: 0,
            file_total: 0,
            rate: 0.0,
            rate_mark: (Instant::now(), 0),
            started: Instant::now(),
            ask: None,
            button: 0,
            src_panel: self.active,
            background: false,
            moved: Vec::new(),
            trashed: Vec::new(),
            restored: Vec::new(),
            device: None,
            skips: Vec::new(),
            checking: false,
        });
    }

    /// `opts` is what the copy/move form asked for; the paths that do
    /// not go through it (S-F5, drops into an archive, VFS transfers)
    /// use the defaults, which is what they did before there was a form.
    pub(super) fn route_transfer(
        &mut self,
        sources: Vec<PathBuf>,
        value: &str,
        is_move: bool,
        opts: TransferOpts,
        rename: Option<Rename>,
    ) {
        let src_panel = &self.panels[self.active];
        let src_archive = src_panel.archive.is_some();
        // masks rename what a copy makes; the archive routes below build
        // their own targets and would drop one on the floor
        if rename.is_some() && (src_archive || split_vfs_dest(value).is_some()) {
            self.status = Some(" source masks work on copies, not archives ".into());
            return;
        }
        // a remote destination (must match before the zip:// syntax -
        // a URL also contains "://")
        if is_remote_url(value) {
            let parsed = if let Some(url) = fish::ShellUrl::parse(value) {
                Some((url.prefix(), value.to_string(), url.path))
            } else if value.starts_with("ftp://") {
                FtpUrl::parse(value)
                    .map(FtpUrl::with_netrc)
                    .map(|url| (url.prefix(), url.display(), url.path))
            } else {
                let scheme = if value.starts_with("fish://") {
                    "fish"
                } else {
                    "sftp"
                };
                // the same identity a connect gave it, ~/.ssh/config and
                // all, or the connection would not be found
                SftpUrl::parse_as(scheme, value)
                    .map(SftpUrl::with_ssh_config)
                    .map(|url| (url.prefix(), url.display(), url.path))
            };
            let Some((prefix, label, path)) = parsed else {
                self.status = Some(" bad URL - scheme://[user@]host[:port]/path ".into());
                return;
            };
            if path.as_os_str().is_empty() {
                self.status = Some(" destination URL needs a path ".into());
                return;
            }
            if is_move && src_archive {
                self.status = Some(" moving out of an archive is a copy - use F5 ".into());
                return;
            }
            let Some(dst_fs) = self.connection(&prefix) else {
                self.status = Some(format!(" not connected - cd {prefix} first "));
                return;
            };
            let src_fs = self.panels[self.active].fs.clone();
            self.start_vfs_transfer(src_fs, sources, dst_fs, path, is_move, label, opts, rename);
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
            self.start_vfs_transfer(
                src_fs,
                sources,
                Arc::new(LocalFs),
                dest,
                is_move,
                label,
                opts,
                rename,
            );
            return;
        }
        // local or archive source, local or zip:// destination
        if is_move {
            if src_archive {
                // a relative destination stays inside the archive: that
                // is a rename, which a rewrite can do. An absolute one
                // means leaving the archive, which is a copy followed by
                // a delete and is better asked for as those two things.
                if value.is_empty() || Path::new(value).is_absolute() || value.contains("://") {
                    self.status = Some(" moving out of an archive is a copy - use F5 ".into());
                } else if let Some(archive) = self.editable_archive() {
                    let inside = self.panels[self.active].cwd.join(value.trim_matches('/'));
                    let ops = self.archive_rename_ops(&sources, &inside);
                    self.start_archive_edit(archive, ops, "move");
                }
            } else if split_vfs_dest(value).is_some() {
                self.status = Some(" cannot move into an archive ".into());
            } else {
                self.start_transfer(sources, value, fsops::spawn_move, "move", opts, rename);
            }
            return;
        }
        match (split_vfs_dest(value), !src_archive) {
            (Some(_), false) => self.status = Some(" cannot copy from archive to archive ".into()),
            (Some((archive, inside)), true) => self.start_pack(sources, archive, inside, None),
            (None, true) => {
                self.start_transfer(sources, value, fsops::spawn_copy, "copy", opts, rename)
            }
            (None, false) => self.start_extract(sources, value),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn start_vfs_transfer(
        &mut self,
        src_fs: Arc<dyn FsProvider>,
        sources: Vec<PathBuf>,
        dst_fs: Arc<dyn FsProvider>,
        dest: PathBuf,
        is_move: bool,
        dest_label: String,
        opts: TransferOpts,
        rename: Option<Rename>,
    ) {
        let verb = if is_move { "move" } else { "copy" };
        self.jobs.push(Job {
            title: format!(" {verb} {} item(s) to {} ", sources.len(), dest_label),
            handle: fsops::spawn_transfer(src_fs, sources, dst_fs, dest, is_move, opts, rename),
            total_files: 0,
            total_bytes: 0,
            files_done: 0,
            bytes_done: 0,
            current: PathBuf::new(),
            file_done: 0,
            file_total: 0,
            rate: 0.0,
            rate_mark: (Instant::now(), 0),
            started: Instant::now(),
            ask: None,
            button: 0,
            src_panel: self.active,
            background: false,
            moved: Vec::new(),
            trashed: Vec::new(),
            restored: Vec::new(),
            device: None,
            skips: Vec::new(),
            checking: false,
        });
    }

    fn start_transfer(
        &mut self,
        sources: Vec<PathBuf>,
        dest: &str,
        spawn: fn(Vec<PathBuf>, PathBuf, TransferOpts, Option<Rename>) -> JobHandle,
        verb: &str,
        opts: TransferOpts,
        rename: Option<Rename>,
    ) {
        let dest = self.resolve(dest);
        self.jobs.push(Job {
            title: format!(" {verb} {} item(s) to {} ", sources.len(), dest.display()),
            handle: spawn(sources, dest, opts, rename),
            total_files: 0,
            total_bytes: 0,
            files_done: 0,
            bytes_done: 0,
            current: PathBuf::new(),
            file_done: 0,
            file_total: 0,
            rate: 0.0,
            rate_mark: (Instant::now(), 0),
            started: Instant::now(),
            ask: None,
            button: 0,
            src_panel: self.active,
            background: false,
            moved: Vec::new(),
            trashed: Vec::new(),
            restored: Vec::new(),
            device: None,
            skips: Vec::new(),
            checking: false,
        });
    }

    /// Run the undo as an ordinary job, so it has the same progress,
    /// the same error prompts and the same Esc. The moves it makes are
    /// themselves reported, so the log ends up describing the undo -
    /// and a second `C-x u` is a redo.
    /// Undo step `at` of the stack. It leaves the stack; what the undo
    /// itself does goes on as the newest step, so undoing that is the
    /// redo.
    pub(super) fn start_undo(&mut self, at: usize) {
        if at >= self.undo.len() {
            return;
        }
        match self.undo.remove(at) {
            UndoStep::Moved(pairs) => {
                let count = pairs.len();
                let handle = fsops::spawn_undo_move(pairs);
                self.push_job(format!(" put {count} item(s) back "), handle);
            }
            UndoStep::Renamed { dir, renames } => {
                let back: Vec<(OsString, OsString)> =
                    renames.into_iter().map(|(old, new)| (new, old)).collect();
                match rcmd_core::rename::apply_os(&dir, &back) {
                    Ok(()) => {
                        self.status = Some(format!(" renamed {} item(s) back ", back.len()));
                        self.push_undo(UndoStep::Renamed { dir, renames: back });
                    }
                    Err(err) => self.status = Some(format!(" undo: {err} ")),
                }
                self.reload_panels();
            }
            UndoStep::Trashed(paths) => self.start_restore(paths, true),
            UndoStep::Restored(paths) => self.start_delete(paths, false),
        }
    }

    /// Stage the targets in git's index, or take them back out.
    fn git_index(&mut self, stage: bool) {
        let panel = &self.panels[self.active];
        if !panel.is_local() {
            self.status = Some(" git works on local files ".into());
            return;
        }
        let targets = panel.targets();
        let done = match stage {
            true => crate::git::stage(&targets),
            false => crate::git::unstage(&targets),
        };
        self.status = Some(match done {
            Ok(n) => format!(
                " {} {n} item(s) ",
                if stage { "staged" } else { "unstaged" }
            ),
            Err(err) => format!(" git: {err} "),
        });
        self.git_refresh();
    }

    fn open_branches(&mut self) {
        let panel = &self.panels[self.active];
        if !panel.is_local() {
            self.status = Some(" git works on local files ".into());
            return;
        }
        let dir = panel.cwd.clone();
        match crate::git::branches(&dir) {
            Ok((names, _)) if names.is_empty() => {
                self.status = Some(" no branches yet - nothing has been committed ".into());
            }
            Ok((names, current)) => {
                let row = names
                    .iter()
                    .position(|n| Some(n) == current.as_ref())
                    .unwrap_or(0);
                let rows = names
                    .iter()
                    // the name first, so a letter finds it
                    .map(|n| match Some(n) == current.as_ref() {
                        true => format!("{n}  (checked out)"),
                        false => n.clone(),
                    })
                    .collect();
                self.dialog = Some(Dialog::Branches(Box::new(BranchPick {
                    dir,
                    rows,
                    names,
                    row,
                })));
            }
            Err(err) => self.status = Some(format!(" git: {err} ")),
        }
    }

    /// Copy INTO an archive: zip appends in place, tar (plain or
    /// compressed) goes through a full rewrite-append.
    pub(super) fn start_pack(
        &mut self,
        sources: Vec<PathBuf>,
        archive: PathBuf,
        inside: PathBuf,
        level: Option<u32>,
    ) {
        let name = archive
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        let handle = if name.ends_with(".zip") {
            fsops::spawn_pack_zip(sources.clone(), archive.clone(), inside, level)
        } else if fsops::is_tar_name(&name) {
            fsops::spawn_pack_tar(sources.clone(), archive.clone(), inside, level)
        } else {
            self.status =
                Some(" only .zip and .tar[.gz/.xz/.bz2/.zst] archives can be written ".into());
            return;
        };
        self.jobs.push(Job {
            title: format!(
                " pack {} item(s) into {} ",
                sources.len(),
                archive.display()
            ),
            handle,
            total_files: 0,
            total_bytes: 0,
            files_done: 0,
            bytes_done: 0,
            current: PathBuf::new(),
            file_done: 0,
            file_total: 0,
            rate: 0.0,
            rate_mark: (Instant::now(), 0),
            started: Instant::now(),
            ask: None,
            button: 0,
            src_panel: self.active,
            background: false,
            moved: Vec::new(),
            trashed: Vec::new(),
            restored: Vec::new(),
            device: None,
            skips: Vec::new(),
            checking: false,
        });
    }

    fn start_extract(&mut self, sources: Vec<PathBuf>, dest: &str) {
        let dest = self.resolve(dest);
        let fs = self.panels[self.active].fs.clone();
        self.jobs.push(Job {
            title: format!(" extract {} item(s) to {} ", sources.len(), dest.display()),
            handle: fsops::spawn_extract(fs, sources, dest),
            total_files: 0,
            total_bytes: 0,
            files_done: 0,
            bytes_done: 0,
            current: PathBuf::new(),
            file_done: 0,
            file_total: 0,
            rate: 0.0,
            rate_mark: (Instant::now(), 0),
            started: Instant::now(),
            ask: None,
            button: 0,
            src_panel: self.active,
            background: false,
            moved: Vec::new(),
            trashed: Vec::new(),
            restored: Vec::new(),
            device: None,
            skips: Vec::new(),
            checking: false,
        });
    }

    /// Where each source lands under `inside`. One source renamed onto a
    /// name that is not an existing directory is a plain rename - which
    /// is what F6 on a single entry means - and everything else moves
    /// into the destination directory under its own name.
    fn archive_rename_ops(&self, sources: &[PathBuf], inside: &Path) -> Vec<fsops::ArchiveOp> {
        let panel = &self.panels[self.active];
        let into_dir = sources.len() > 1
            || panel
                .fs
                .stat(inside)
                .map(|entry| entry.is_dir())
                .unwrap_or(false);
        sources
            .iter()
            .map(|from| fsops::ArchiveOp::Rename {
                from: from.clone(),
                to: if into_dir {
                    inside.join(from.file_name().unwrap_or_default())
                } else {
                    inside.to_path_buf()
                },
            })
            .collect()
    }

    /// The archive the active panel is inside, if that container is one
    /// rcmd can rewrite. zip and tar can be; a deb, an rpm, an iso or a
    /// cpio cannot - not because the code is missing but because
    /// rewriting a package or a disc image is not what a panel is for.
    fn editable_archive(&mut self) -> Option<PathBuf> {
        let archive = self.panels[self.active].archive.clone()?;
        if self.panels[self.active].nested() {
            self.status =
                Some(" an archive on a server or inside another is read here, not changed ".into());
            return None;
        }
        let name = archive
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        if name.ends_with(".zip") || fsops::is_tar_name(&name) {
            return Some(archive);
        }
        self.status = Some(" only .zip and .tar archives can be changed ".into());
        None
    }

    /// Run one batch of changes against an archive. Every op goes in one
    /// job because the container is rewritten once, however many members
    /// the batch touches.
    pub(super) fn start_archive_edit(
        &mut self,
        archive: PathBuf,
        ops: Vec<fsops::ArchiveOp>,
        verb: &str,
    ) {
        let count = ops.len();
        let handle = fsops::spawn_archive_edit(archive, ops);
        self.jobs.push(Job {
            title: format!(" {verb} {count} item(s) in the archive "),
            handle,
            total_files: 0,
            total_bytes: 0,
            files_done: 0,
            bytes_done: 0,
            current: PathBuf::new(),
            file_done: 0,
            file_total: 0,
            rate: 0.0,
            rate_mark: (Instant::now(), 0),
            started: Instant::now(),
            ask: None,
            button: 0,
            src_panel: self.active,
            background: false,
            moved: Vec::new(),
            trashed: Vec::new(),
            restored: Vec::new(),
            device: None,
            skips: Vec::new(),
            checking: false,
        });
    }

    pub(super) fn start_delete(&mut self, paths: Vec<PathBuf>, permanent: bool) {
        if let Some(archive) = self.panels[self.active].archive.clone() {
            let ops = paths.into_iter().map(fsops::ArchiveOp::Remove).collect();
            self.start_archive_edit(archive, ops, "delete");
            return;
        }
        let verb = if permanent { "delete" } else { "trash" };
        let count = paths.len();
        let handle = if self.panels[self.active].is_remote() {
            fsops::spawn_delete_fs(self.panels[self.active].fs.clone(), paths)
        } else {
            fsops::spawn_delete(paths, permanent)
        };
        self.jobs.push(Job {
            title: format!(" {verb} {count} item(s) "),
            handle,
            total_files: 0,
            total_bytes: 0,
            files_done: 0,
            bytes_done: 0,
            current: PathBuf::new(),
            file_done: 0,
            file_total: 0,
            rate: 0.0,
            rate_mark: (Instant::now(), 0),
            started: Instant::now(),
            ask: None,
            button: 0,
            src_panel: self.active,
            background: false,
            moved: Vec::new(),
            trashed: Vec::new(),
            restored: Vec::new(),
            device: None,
            skips: Vec::new(),
            checking: false,
        });
    }

    /// Alt+F6: each marked archive (or the cursor's) unpacked into the
    /// other panel, in a directory of its own named after it - an
    /// archive whose top level is forty files does not spill them over
    /// what is already there.
    fn extract_archives(&mut self) {
        if !self.panels[self.active].is_local() || !self.panels[self.active ^ 1].is_local() {
            self.status = Some(" extract works between two local panels ".into());
            return;
        }
        let archives: Vec<PathBuf> = self.panels[self.active]
            .targets()
            .into_iter()
            .filter(|p| p.file_name().is_some_and(rcmd_core::vfs::is_archive_name))
            .collect();
        if archives.is_empty() {
            self.status = Some(" no archive marked or under the cursor ".into());
            return;
        }
        let dest_dir = self.panels[self.active ^ 1].local_cwd();
        for archive in archives {
            let name = archive.file_name().unwrap_or_default();
            let stem = rcmd_core::vfs::archive_stem(name).unwrap_or_else(|| "extracted".into());
            // a name already taken gets a number, never an overwrite
            let target = (0..)
                .map(|n| match n {
                    0 => dest_dir.join(&stem),
                    n => dest_dir.join(format!("{stem}-{n}")),
                })
                .find(|p| std::fs::symlink_metadata(p).is_err())
                .expect("some name is free");
            let fs = match rcmd_core::archive::ArchiveFs::open(&archive) {
                Ok(fs) => fs,
                Err(err) => {
                    self.status = Some(format!(" {}: {err} ", name.to_string_lossy()));
                    continue;
                }
            };
            let sources: Vec<PathBuf> = match fs.read_dir(Path::new("")) {
                Ok(entries) => entries
                    .into_iter()
                    .filter(|e| !e.is_parent())
                    .map(|e| PathBuf::from(e.name))
                    .collect(),
                Err(err) => {
                    self.status = Some(format!(" {}: {err} ", name.to_string_lossy()));
                    continue;
                }
            };
            if let Err(err) = std::fs::create_dir(&target) {
                self.status = Some(format!(" {}: {err} ", target.display()));
                continue;
            }
            let title = format!(" extract {} ", name.to_string_lossy());
            let handle = fsops::spawn_extract(Arc::new(fs), sources, target);
            self.push_job(title, handle);
        }
    }

    /// Enter on a file a `[[vfs]]` rule claims: into it, like an
    /// archive. False = no rule has it, and Enter goes on as usual.
    fn enter_user_vfs(&mut self) -> bool {
        let panel = &self.panels[self.active];
        if !panel.is_local() {
            return false;
        }
        let Some(entry) = panel.selected().filter(|e| !e.is_parent() && !e.is_dir()) else {
            return false;
        };
        let name = entry.name.to_string_lossy().into_owned();
        let Some(rule) = self.config.vfs.iter().find(|r| r.matches(&name)) else {
            return false;
        };
        let Some(rule) = rule.rule() else {
            self.status = Some(" a [[vfs]] rule needs a script, or both list and copyout ".into());
            return true;
        };
        let path = panel.cwd.join(&entry.name);
        let opened = rcmd_core::extfs::ExtFs::open(&path, &rule)
            .and_then(|fs| self.panels[self.active].enter_provider(Arc::new(fs), path));
        if let Err(err) = opened {
            self.status = Some(format!(" {name}: {err} "));
        }
        true
    }

    /// Resolve user input to a normalized path: `~` expands to $HOME,
    /// relative paths are anchored at the active panel's directory.
    pub(super) fn resolve(&self, input: &str) -> PathBuf {
        // a name typed on a panel is spelled in that panel's codepage:
        // the bytes it makes are the bytes the names already there
        // have, so what is created is what the panel then shows
        let typed = |text: &str| PathBuf::from(self.panels[self.active].name_bytes(text));
        let raw = if input == "~" {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
        } else if let Some(rest) = input.strip_prefix("~/") {
            match std::env::var_os("HOME") {
                Some(home) => PathBuf::from(home).join(typed(rest)),
                None => typed(input),
            }
        } else {
            let path = typed(input);
            if path.is_absolute() {
                path
            } else {
                self.panels[self.active].local_cwd().join(path)
            }
        };
        normalize(&raw)
    }

    pub(super) fn on_panel_key(&mut self, key: KeyEvent) {
        // a thorough compare reads files: Esc stops it, as it stops a
        // find, rather than waiting for the last pair
        if key.code == KeyCode::Esc {
            if let Some(running) = self.sync_scan.take() {
                running.cancel();
                self.status = Some(" synchronize cancelled ".into());
                return;
            }
            if let Some(running) = self.compare.take() {
                running.handle.cancel();
                self.status = Some(" compare cancelled ".into());
                return;
            }
            if let Some(running) = self.panelize.take() {
                running
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.status = Some(" panelize cancelled ".into());
                return;
            }
        }
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
                KeyCode::Char('t' | 'T') => self.run_action(Action::PasteTags),
                KeyCode::Char('p' | 'P') => self.run_action(Action::PastePath),
                KeyCode::Char('c' | 'C') => self.open_chmod(),
                KeyCode::Char('e' | 'E') => self.open_chattr(),
                KeyCode::Char('o' | 'O') => self.open_chown(),
                // MC's four: l hard, s absolute, v relative, C-s edit
                KeyCode::Char('s' | 'S') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.open_edit_symlink()
                }
                KeyCode::Char('s' | 'S') => self.open_link(LinkKind::Symbolic, false),
                KeyCode::Char('v' | 'V') => self.open_link(LinkKind::Symbolic, true),
                KeyCode::Char('l' | 'L') => self.open_link(LinkKind::Hard, false),
                KeyCode::Char('j' | 'J') => self.run_action(Action::Jobs),
                KeyCode::Char('u' | 'U') => self.run_action(Action::Undo),
                KeyCode::Char('f' | 'F') => self.run_action(Action::Filters),
                KeyCode::Char('m' | 'M') => self.run_action(Action::RestoreMarks),
                KeyCode::Char(' ') => self.run_action(Action::DirSizeAll),
                KeyCode::Char(digit @ '0'..='9') => {
                    self.run_action(Action::Shortcut(digit as u8 - b'0'))
                }
                KeyCode::Char('a' | 'A') => self.run_action(Action::VfsList),
                KeyCode::Char('h' | 'H') => self.run_action(Action::HotlistAdd),
                KeyCode::Char('r' | 'R') => self.run_action(Action::JobReport),
                KeyCode::Char('!') => self.run_action(Action::Panelize),
                _ => {}
            }
            return;
        }
        if ctrl && key.code == KeyCode::Char('x') {
            self.prefix_cx = true;
            self.status = Some(
                " C-x  (d = compare, q = quick view, i = info, c = chmod, \
                 e = chattr, o = chown, s = symlink, j = jobs, a = active VFS, \
                 u = undo, f = filter sets, m = restore marks, \
                 space = size every directory, 0-9 = the numbered \
                 places, ! = panelize, t/p = paste tags/path) "
                    .into(),
            );
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
                KeyCode::F(4) => {
                    qv.hex = !qv.hex;
                    qv.top = 0;
                }
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
                        let known = if qv.hex {
                            fv.size.div_ceil(16) as usize
                        } else if want == usize::MAX {
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
        // A panel in tree mode: the figure has replaced the listing, so
        // it takes the movement keys and Enter. Everything else - Tab,
        // the command line, the F-keys - still belongs to the panel.
        if self.panels[self.active].list_mode == ListMode::Tree {
            let plain = !alt && !ctrl;
            let mut handled = true;
            if let Some(tree) = self.trees[self.active].as_mut() {
                match key.code {
                    KeyCode::Up if plain => tree.up(),
                    KeyCode::Down if plain => tree.down(),
                    KeyCode::PageUp if plain => tree.page_up(page),
                    KeyCode::PageDown if plain => tree.page_down(page),
                    KeyCode::Home if plain => tree.first(),
                    KeyCode::End if plain => tree.last(),
                    KeyCode::Left if plain => tree.left(),
                    KeyCode::Right if plain => tree.right(),
                    // mc's tree keys: F4 switches navigation mode, C-r
                    // (rcmd's reload) rescans the selected branch
                    KeyCode::F(4) => tree.toggle_mode(),
                    KeyCode::Char('r') if ctrl => tree.rescan(),
                    _ => handled = false,
                }
            } else {
                handled = false;
            }
            // F5-F8 act on the directory the tree has selected, not on
            // the listing hidden under the figure
            let selected = self.trees[self.active]
                .as_ref()
                .and_then(|tree| tree.selected_path());
            if !handled
                && plain
                && let Some(dir) = selected
            {
                handled = true;
                match key.code {
                    KeyCode::F(5) => self.open_transfer_of(false, vec![dir]),
                    KeyCode::F(6) => self.open_transfer_of(true, vec![dir]),
                    KeyCode::F(8) => self.open_delete_of(false, vec![dir]),
                    KeyCode::F(7) => {
                        let inside = format!("{}/", dir.display());
                        self.dialog = Some(Dialog::Input(InputDialog::new(
                            " Create directory ",
                            inside,
                            InputAction::Mkdir,
                        )));
                    }
                    _ => handled = false,
                }
            }
            // Enter needs the whole App: it moves the *other* panel
            if !handled && key.code == KeyCode::Enter && cmd_empty {
                self.tree_enter();
                handled = true;
            }
            if handled {
                return;
            }
        }
        // Focused info pane: nothing to scroll - Tab back or quit.
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
            // Tab completes a path once the command line has text (or as
            // MC's M-Tab always); on an empty line it switches panels.
            KeyCode::Tab if alt || !cmd_empty => {
                self.complete_cmdline();
                return;
            }
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
            // C-p/C-n are rcmd's; M-p/M-n are the same keys in MC
            KeyCode::Char('p') if ctrl || alt => {
                self.cmdline.hist_prev();
                return;
            }
            // M-n is also sort-by-name, and a history step with no walk
            // under way has nothing to step to: only then does it fall
            // through to the keymap - before, sort-name was unreachable
            KeyCode::Char('n') if ctrl || (alt && self.cmdline.hist_pos.is_some()) => {
                self.cmdline.hist_next();
                return;
            }
            _ => {}
        }

        // Action keys via the (config-driven) keymap. Plain characters and
        // Left/Right only qualify while the command line is empty - with
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

        // with the command line hidden there is nowhere to type: plain
        // characters only reach bindings (MC's "command prompt" off)
        if self.config.show_cmdline
            && edit_line(
                &mut self.cmdline.value,
                &mut self.cmdline.cursor,
                key.code,
                mods,
            )
        {
            self.cmdline.hist_pos = None;
        }
    }

    fn submit_command(&mut self) {
        let cmd = self.cmdline.take();
        if cmd.is_empty() {
            return;
        }
        self.cmdline.push_history(&cmd);
        // `= 2*(3+4)`: worked out here rather than handed to a shell,
        // the answer on the status line and, bare, on the command line
        // to go on from
        if let Some(expr) = cmd.trim_start().strip_prefix('=') {
            match rcmd_core::calc::eval(expr) {
                Ok(value) => {
                    self.status = Some(format!(" {} = {value} ", expr.trim()));
                    let bare = value.to_string();
                    let bare = bare.split_whitespace().next().unwrap_or_default();
                    self.cmdline.set_line(&format!("= {bare}"));
                }
                Err(err) => {
                    self.status = Some(format!(" = {err} "));
                    self.cmdline.set_line(&cmd);
                }
            }
            return;
        }
        if let Some(dir) = parse_cd(&cmd) {
            // no macro expansion here: the expansion shell-quotes, and a
            // quoted path is not what `cd` wants
            self.do_cd(dir);
        } else {
            // MC expands its macros on the command line too; unknown
            // percent sequences (printf "%s") are left alone
            self.run_macro_command(&cmd, false);
        }
    }

    /// `cd <dir>` - from the command line or the M-c quick-cd dialog.
    pub(super) fn do_cd(&mut self, dir: &str) {
        if is_remote_url(dir) {
            self.connect_remote(dir);
            return;
        }
        // `cd -`: back to where this panel came from, shell-style
        if dir == "-" {
            let previous = self.panels[self.active]
                .previous_location()
                .map(str::to_string);
            match previous {
                Some(loc) => self.navigate(&loc),
                None => self.status = Some(" cd: no previous directory ".into()),
            }
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
            self.resolve_cd(dir)
        };
        if let Err(err) = self.panels[self.active].cd(target) {
            self.status = Some(format!(" cd: {err} "));
        }
    }

    /// [`Self::resolve`] plus `$CDPATH`: a relative target that does not
    /// exist under the panel directory is looked up in each CDPATH entry
    /// in turn, like the shell builtin. Absolute and `~` paths, and any
    /// target that does exist here, never consult it.
    fn resolve_cd(&self, input: &str) -> PathBuf {
        let here = self.resolve(input);
        if here.exists()
            || input.starts_with('/')
            || input.starts_with('~')
            || input.starts_with('.')
        {
            return here;
        }
        let Some(cdpath) = std::env::var_os("CDPATH") else {
            return here;
        };
        for dir in std::env::split_paths(&cdpath) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = normalize(&dir.join(input));
            if candidate.is_dir() {
                return candidate;
            }
        }
        here
    }

    /// Tab: complete the path under the cursor (files/dirs only).
    fn complete_cmdline(&mut self) {
        self.complete_focused(true);
        self.cmdline.hist_pos = None;
    }

    /// Alt+Enter: append the cursor entry's (shell-quoted) name.
    fn insert_selected_name(&mut self) {
        let Some(entry) = self.panels[self.active].selected() else {
            return;
        };
        let text = format!("{} ", shell_quote(&entry.name.to_string_lossy()));
        self.insert_cmdline(&text);
    }

    /// C-x t: append every tagged name (or the cursor entry),
    /// shell-quoted, to the command line.
    fn insert_tagged_names(&mut self) {
        let text: String = self.panels[self.active]
            .target_names()
            .iter()
            .map(|n| format!("{} ", shell_quote(&n.to_string_lossy())))
            .collect();
        self.insert_cmdline(&text);
    }

    fn insert_cmdline(&mut self, text: &str) {
        let idx = byte_index(&self.cmdline.value, self.cmdline.cursor);
        self.cmdline.value.insert_str(idx, text);
        self.cmdline.cursor += text.chars().count();
        self.cmdline.hist_pos = None;
    }

    fn describe(&self, paths: &[PathBuf]) -> String {
        if paths.len() == 1 {
            // named in the panel's codepage, so the question is about
            // the file the panel is showing rather than about mojibake
            format!(
                "\"{}\"",
                rcmd_core::charset::decode_name(
                    paths[0].file_name().unwrap_or_default(),
                    self.panels[self.active].charset,
                )
            )
        } else {
            format!("{} items", paths.len())
        }
    }

    /// Operations that only make sense on a local directory.
    pub(super) fn require_local(&mut self) -> bool {
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
        let in_archive = self.panels[self.active].archive.is_some();
        if is_move && in_archive && self.editable_archive().is_none() {
            return;
        }
        let sources = self.panels[self.active].targets();
        self.open_transfer_of(is_move, sources);
    }

    /// The copy/move form for `sources`, wherever they came from - the
    /// panel's marks, or the find results window's.
    pub(super) fn open_transfer_of(&mut self, is_move: bool, sources: Vec<PathBuf>) {
        if sources.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let in_archive = self.panels[self.active].archive.is_some();
        let verb = if is_move { "Move" } else { "Copy" };
        let other = &self.panels[self.active ^ 1];
        // a remote or archive panel on the other side prefills its
        // virtual path - accepting it uploads / packs into the zip
        // moving inside an archive is a rename, so the bare name is the
        // useful default; an absolute path there would mean leaving the
        // archive, which F5 does and F6 does not
        let mut dest = if is_move && in_archive {
            sources
                .first()
                .and_then(|src| src.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else if other.is_remote() {
            other.display_path()
        } else if other.is_local() || is_move {
            other.local_cwd().display().to_string()
        } else {
            other.display_path()
        };
        if !(is_move && in_archive) && !dest.ends_with('/') {
            dest.push('/');
        }
        let title = format!(" {verb} {} to: ", self.describe(&sources));
        self.dialog = Some(Dialog::Transfer(Box::new(TransferDialog {
            title,
            mask: TextField::new("*").with_history("mask"),
            dest: TextField::new(dest).with_history("destination"),
            is_move,
            sources,
            opts: TransferOpts::default(),
            // the destination is what people type; the mask sits above
            // it for the rare copy that needs one, an Up away
            row: TRANSFER_DEST_ROW,
            button: 0,
        })));
    }

    /// S-F5 / S-F6: copy or rename the cursor file without leaving the
    /// directory - the dialog prefills the bare name for editing.
    fn open_transfer_here(&mut self, is_move: bool) {
        let in_archive = self.panels[self.active].archive.is_some();
        if in_archive {
            if self.editable_archive().is_none() {
                return;
            }
        } else if !self.require_local() {
            return;
        }
        let panel = &self.panels[self.active];
        let Some(entry) = panel.selected().filter(|e| !e.is_parent()) else {
            self.status = Some(" nothing selected ".into());
            return;
        };
        let name = panel.name_of(entry);
        let sources = vec![panel.cwd.join(&entry.name)];
        let (verb, action) = if is_move {
            ("Rename", InputAction::MoveTo { sources })
        } else {
            ("Copy", InputAction::CopyTo { sources })
        };
        self.dialog = Some(Dialog::Input(InputDialog::new(
            format!(" {verb} \"{name}\" in place to: "),
            name,
            action,
        )));
    }

    /// S-F4: prompt for a file name, then edit it - existing or not.
    fn open_edit_new(&mut self) {
        if !self.require_local() {
            return;
        }
        self.dialog = Some(Dialog::Input(InputDialog::new(
            " Edit new file ",
            "",
            InputAction::EditNew,
        )));
    }

    /// Open the editor on `path`; a missing file becomes an empty
    /// buffer that only lands on disk when saved.
    pub(super) fn edit_new(&mut self, path: PathBuf) {
        if self.config.editor == "external" {
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| "vi".to_string());
            self.pending_exec = Some(Exec::Quiet(format!(
                "{editor} {}",
                shell_quote(&path.to_string_lossy())
            )));
            return;
        }
        let title = path.display().to_string();
        if path.exists() {
            self.open_internal_editor(&path, title);
        } else {
            let mut ed = rcmd_edit::Editor::create(&path);
            ed.prefs = crate::editorconfig::for_file(&ed, self.config.edit_prefs());
            self.open_screen(Screen::Editor(Box::new(EditorState {
                marks: Default::default(),
                hl: rcmd_edit::Highlighter::new(&path, 0),
                ed,
                title,
                top: 0,
                top_seg: 0,
                left: 0,
                wrap: false,
                rows: 1,
                cols: 1,
                search: ViewSearch::default(),
                prompt: None,
                note: None,
                wrap_column: self.config.edit_wrap_column as usize,
                menu: None,
                follow_up: None,
                bookmarks: Vec::new(),
                line_numbers: self.config.edit_line_numbers,
                gutter: 0,
            })));
        }
    }

    /// The panel's write half, or a status message saying why not.
    fn writable_targets(&mut self) -> Option<Vec<PathBuf>> {
        let panel = &self.panels[self.active];
        if panel.fs.writer().is_none() {
            self.status = Some(" archive is read-only ".into());
            return None;
        }
        let targets = panel.targets();
        if targets.is_empty() {
            self.status = Some(" nothing selected ".into());
            return None;
        }
        Some(targets)
    }

    /// C-x c: octal chmod on the marked entries (or the cursor entry).
    fn open_chmod(&mut self) {
        let Some(paths) = self.writable_targets() else {
            return;
        };
        let remote = self.panels[self.active].is_remote();
        let panel = &self.panels[self.active];
        let entry = panel.selected();
        let mode = entry.map_or(0o644, |e| e.mode & 0o7777);
        let mut dialog = ChmodDialog {
            paths,
            mode,
            octal: String::new(),
            octal_cursor: 0,
            name: entry.map_or_else(String::new, |e| panel.name_of(e)),
            owner: entry.map_or_else(String::new, |e| {
                crate::ui::owner_label(e.extra.uid, remote, true)
            }),
            group: entry.map_or_else(String::new, |e| {
                crate::ui::owner_label(e.extra.gid, remote, false)
            }),
            // the octal has the focus: anyone who already knows the mode
            // types it and presses Enter, as they always could, and the
            // boxes are there for everyone else
            row: CHMOD_OCTAL_ROW,
            button: 0,
            recurse: false,
        };
        dialog.sync_octal();
        self.dialog = Some(Dialog::Chmod(Box::new(dialog)));
    }

    /// C-x e: the file flags of the marked entries (or the cursor
    /// entry). Local files only: no remote protocol carries them.
    fn open_chattr(&mut self) {
        if !self.panels[self.active].is_local() {
            self.status = Some(" chattr works on local files only ".into());
            return;
        }
        let Some(paths) = self.writable_targets() else {
            return;
        };
        let panel = &self.panels[self.active];
        let Some(entry) = panel.selected() else {
            return;
        };
        let path = panel.cwd.join(&entry.name);
        let was = match rcmd_core::attrs::get(&path) {
            Ok(flags) => flags,
            Err(err) => {
                self.status = Some(format!(" chattr: {err} "));
                return;
            }
        };
        self.dialog = Some(Dialog::Chattr(Box::new(ChattrDialog {
            name: panel.name_of(entry),
            paths,
            flags: was,
            was,
            row: 0,
            button: 0,
        })));
    }

    /// C-x o: chown on the marked entries (or the cursor entry).
    fn open_chown(&mut self) {
        let Some(paths) = self.writable_targets() else {
            return;
        };
        // On a remote panel our /etc/passwd means nothing: the ids
        // belong to the server, so it stays a typed spec.
        if self.panels[self.active].is_remote() {
            self.dialog = Some(Dialog::Input(InputDialog::new(
                format!(" Chown {} (user[:group]) ", self.describe(&paths)),
                "",
                InputAction::Chown { paths },
            )));
            return;
        }
        let panel = &self.panels[self.active];
        let entry = panel.selected();
        let users = crate::ui::all_users();
        let groups = crate::ui::all_groups();
        let find = |list: &[(u32, String)], id: Option<u32>| {
            id.and_then(|id| list.iter().position(|entry| entry.0 == id))
                .unwrap_or(0)
        };
        self.dialog = Some(Dialog::Chown(Box::new(ChownDialog {
            user_row: find(&users, entry.and_then(|e| e.extra.uid)),
            group_row: find(&groups, entry.and_then(|e| e.extra.gid)),
            users,
            groups,
            paths,
            column: 0,
            button: 0,
            name: entry.map_or_else(String::new, |e| panel.name_of(e)),
            owner: entry.map_or_else(String::new, |e| {
                crate::ui::owner_label(e.extra.uid, false, true)
            }),
            group: entry.map_or_else(String::new, |e| {
                crate::ui::owner_label(e.extra.gid, false, false)
            }),
            recurse: false,
        })));
    }

    /// C-x s: create a symlink to the cursor entry.
    /// C-x s (absolute), C-x v (relative), C-x l (hard). MC fills in the
    /// original's path and suggests a name for the link, and lets you
    /// change either.
    fn open_link(&mut self, kind: LinkKind, relative: bool) {
        let panel = &self.panels[self.active];
        if panel.fs.writer().is_none() {
            self.status = Some(" archive is read-only ".into());
            return;
        }
        let Some(entry) = panel.selected().filter(|e| !e.is_parent()) else {
            self.status = Some(" nothing selected ".into());
            return;
        };
        let name = panel.name_of(entry);
        // relative to the directory the link lands in, which is this one
        let target = if relative {
            name.clone()
        } else {
            panel.cwd.join(&entry.name).display().to_string()
        };
        let link = format!("{name}-link");
        self.dialog = Some(Dialog::Link(Box::new(LinkDialog {
            kind,
            target: TextField::new(target).with_history("link-target"),
            name: TextField::new(link).with_history("link-name"),
            row: 1, // the name is what usually needs changing
            ok: true,
        })));
    }

    /// C-x C-s: change where an existing symlink points.
    fn open_edit_symlink(&mut self) {
        let panel = &self.panels[self.active];
        if panel.fs.writer().is_none() {
            self.status = Some(" archive is read-only ".into());
            return;
        }
        let Some(entry) = panel.selected().filter(|e| !e.is_parent()) else {
            self.status = Some(" nothing selected ".into());
            return;
        };
        let Some(target) = entry.link_target.clone() else {
            self.status = Some(" not a symlink ".into());
            return;
        };
        let target = target.display().to_string();
        self.dialog = Some(Dialog::Link(Box::new(LinkDialog {
            kind: LinkKind::EditSymlink,
            target: TextField::new(target).with_history("link-target"),
            name: TextField::new(panel.name_of(entry)).cursor(0),
            row: 0,
            ok: true,
        })));
    }

    /// Run one FsWrite operation over several paths, reporting the
    /// first error (with a count of how many succeeded before it).
    pub(super) fn apply_fs_op(
        &mut self,
        paths: &[PathBuf],
        verb: &str,
        op: impl Fn(&dyn rcmd_core::vfs::FsWrite, &Path) -> std::io::Result<()>,
    ) {
        let fs = self.panels[self.active].fs.clone();
        let Some(writer) = fs.writer() else {
            self.status = Some(" read-only ".into());
            return;
        };
        let mut done = 0usize;
        for path in paths {
            if let Err(err) = op(writer, path) {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                self.status = Some(format!(" {verb} {name}: {err} ({done} done) "));
                self.reload_panels();
                return;
            }
            done += 1;
        }
        self.status = Some(format!(" {verb}: {done} item(s) "));
        self.reload_panels();
    }

    pub(super) fn reload_panels(&mut self) {
        for panel in &mut self.panels {
            let _ = panel.refresh();
        }
        self.git_refresh();
    }

    fn open_mkdir(&mut self) {
        if self.panels[self.active].archive.is_some() && self.editable_archive().is_none() {
            return;
        }
        self.dialog = Some(Dialog::Input(InputDialog::new(
            " Create directory ",
            "",
            InputAction::Mkdir,
        )));
    }

    /// `C-x f`: the named filter sets, with the ones this panel is
    /// under already ticked. Several at once is the point: what the
    /// panel shows is what any of them shows, minus what any hides.
    fn open_filters(&mut self) {
        if self.config.filter.is_empty() {
            self.status = Some(" no [[filter]] sets in the config - see the README ".into());
            return;
        }
        let on = self.filter_sets_on[self.active].clone();
        let on = match on.len() == self.config.filter.len() {
            true => on,
            false => vec![false; self.config.filter.len()],
        };
        self.dialog = Some(Dialog::Filters(Box::new(FiltersDialog {
            on,
            row: 0,
            panel: self.active,
        })));
    }

    /// C-x u: put back what the last move moved. Only a move leaves a
    /// record - a copy is undone by deleting what it wrote, which is a
    /// deletion and should be asked for as one, and a delete already
    /// went to the trash.
    fn open_undo(&mut self) {
        if self.undo.is_empty() {
            self.status = Some(" nothing to undo ".into());
            return;
        }
        self.dialog = Some(Dialog::Undo(0));
    }

    /// The rows of the C-x u list, oldest first - the list draws them
    /// newest first.
    pub fn undo_rows(&self) -> Vec<String> {
        self.undo.iter().map(UndoStep::describe).collect()
    }

    /// Something to undo later. The oldest goes once there are more
    /// than [`UNDO_DEPTH`].
    pub(super) fn push_undo(&mut self, step: UndoStep) {
        self.undo.push(step);
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
    }

    /// The trash as a filesystem, one for the session.
    pub(super) fn trash_fs(&mut self) -> Arc<rcmd_core::trashcan::TrashFs> {
        self.trash
            .get_or_insert_with(|| Arc::new(rcmd_core::trashcan::TrashFs::new()))
            .clone()
    }

    /// Whether the active panel is on `trash://`.
    pub(super) fn in_trash(&self) -> bool {
        self.panels[self.active].remote.as_deref() == Some(rcmd_core::trashcan::PREFIX)
    }

    /// F6 on the trash: put the marked (or cursor) items back where
    /// they came from, which is the one place a move out of the trash
    /// means.
    fn open_restore(&mut self) {
        if self.panels[self.active].cwd.parent().is_some() {
            self.status = Some(" only what was thrown away goes back - go up to the top ".into());
            return;
        }
        let paths = self.panels[self.active].targets();
        if paths.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let message = match paths.as_slice() {
            // where from is on the line under the panel; the one line
            // here has room for the name
            [path] => format!(
                "Put \"{}\" back?",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            _ => format!("Put {} items back where they came from?", paths.len()),
        };
        self.dialog = Some(Dialog::Confirm(ConfirmDialog {
            title: " Restore ".into(),
            message,
            yes: true,
            paths,
            permanent: false,
            kind: ConfirmKind::Restore,
            command: None,
        }));
    }

    pub(super) fn start_restore(&mut self, paths: Vec<PathBuf>, by_origin: bool) {
        let trash = self.trash_fs();
        let count = paths.len();
        let handle = fsops::spawn_restore(trash, paths, by_origin);
        self.push_job(format!(" restore {count} item(s) "), handle);
    }

    /// M-F5: pack what is marked into an archive of its own. The name
    /// decides the container, which is how NC and VC have always asked
    /// the question, and the other panel's directory is where it lands,
    /// as with F5. An archive already at that name is added to rather
    /// than replaced, which is what F5 into an open one does too.
    fn open_pack(&mut self) {
        if !self.require_local() {
            return;
        }
        let sources = self.panels[self.active].targets();
        if sources.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        // one entry names the archive after itself, several after the
        // directory holding them - which is what either would have been
        // called by hand
        let stem = match sources.len() {
            1 => sources[0].file_name(),
            _ => self.panels[self.active].cwd.file_name(),
        }
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".into());
        let other = &self.panels[self.active ^ 1];
        let dir = match other.is_local() {
            true => other.local_cwd(),
            false => self.panels[self.active].local_cwd(),
        };
        let value = dir.join(format!("{stem}.tar.gz")).display().to_string();
        self.dialog = Some(Dialog::Input(InputDialog::new(
            format!(" Pack {} to: ", self.describe(&sources)),
            value,
            InputAction::Pack {
                sources,
                level: None,
            },
        )));
    }

    fn open_select(&mut self, mark: bool) {
        self.dialog = Some(Dialog::Pattern(Box::new(PatternDialog {
            title: match mark {
                true => " Select group ".into(),
                false => " Unselect group ".into(),
            },
            value: PatternDialog::pattern_field(PatternKind::Select { mark }, "*"),
            size: TextField::new("").with_history("size"),
            newer: TextField::new("").with_history("newer"),
            shell: true,
            case_sensitive: true,
            files_only: true,
            row: 0,
            ok: true,
            kind: PatternKind::Select { mark },
        })));
    }

    fn open_delete(&mut self, permanent: bool) {
        if self.panels[self.active].archive.is_some() && self.editable_archive().is_none() {
            return;
        }
        let paths = self.panels[self.active].targets();
        self.open_delete_of(permanent, paths);
    }

    /// The delete question for `paths`, wherever they came from.
    pub(super) fn open_delete_of(&mut self, permanent: bool, paths: Vec<PathBuf>) {
        let panel = &self.panels[self.active];
        // no trash inside an archive or on a server: both delete outright
        let permanent = permanent || panel.is_remote() || panel.archive.is_some();
        if paths.is_empty() {
            self.status = Some(" nothing selected ".into());
            return;
        }
        let what = self.describe(&paths);
        let message = if self.in_trash() {
            format!("Delete {what} for good? Nothing comes back from here")
        } else if self.panels[self.active].is_remote() {
            format!("Permanently delete {what} from the server?")
        } else if permanent {
            format!("Permanently delete {what}?")
        } else {
            format!("Move {what} to trash?")
        };
        if !self.config.confirm_delete {
            self.start_delete(paths, permanent);
            return;
        }
        self.dialog = Some(Dialog::Confirm(ConfirmDialog {
            title: " Delete ".into(),
            message,
            yes: !permanent, // safer default for the irreversible variant
            paths,
            permanent,
            kind: ConfirmKind::Delete,
            command: None,
        }));
    }

    /// A panel in tree mode needs its figure; leaving the mode drops
    /// it, so coming back starts from wherever the panel has got to.
    fn sync_tree(&mut self, side: usize) {
        if self.panels[side].list_mode != ListMode::Tree {
            self.trees[side] = None;
            return;
        }
        if self.trees[side].is_none() {
            let panel = &self.panels[side];
            self.trees[side] = Some(Tree::new(&panel.local_cwd(), panel.show_hidden));
        }
    }

    /// Enter in a tree *panel*: mc changes the **other** panel and
    /// stays in the tree, which is what makes the mode a navigator
    /// rather than a one-shot chooser. (The tree *dialog* is the
    /// one-shot chooser, and moves this panel instead.)
    pub(super) fn tree_enter(&mut self) {
        let Some(path) = self.trees[self.active]
            .as_ref()
            .and_then(Tree::selected_path)
        else {
            return;
        };
        let other = &mut self.panels[self.active ^ 1];
        let result = if other.is_local() {
            other.cd(path)
        } else {
            other.to_local(path)
        };
        if let Err(err) = result {
            self.status = Some(format!(" {err} "));
        }
    }

    pub(super) fn panel(&mut self) -> &mut Panel {
        &mut self.panels[self.active]
    }

    pub(super) fn fallible(&mut self, op: impl FnOnce(&mut Panel) -> std::io::Result<bool>) {
        if let Err(err) = op(self.panel()) {
            self.status = Some(format!(" {err} "));
        }
    }
}

impl App {
    /// M-g / M-r / M-j: the first, the middle or the last row the panel
    /// has on screen - mc's way of getting across a screenful without
    /// counting.
    fn cursor_on_screen(&mut self, spot: Action) {
        let side = self.active;
        let offset = self.table_states[side].offset();
        let columns = match self.panels[side].list_mode {
            ListMode::Brief => usize::from(self.config.columns()),
            ListMode::User => usize::from(self.listing_format.repeat.max(1)),
            _ => 1,
        };
        let shown = self.panel_rows.max(1) * columns;
        let panel = &mut self.panels[side];
        let len = panel.entries.len();
        if len == 0 {
            return;
        }
        let last = (offset + shown).min(len) - 1;
        panel.cursor = match spot {
            Action::ScreenTop => offset.min(last),
            Action::ScreenMiddle => offset + (last.saturating_sub(offset)) / 2,
            _ => last,
        };
    }
}

impl App {
    /// C-x r: the last job's skip report, one line per thing left alone,
    /// in the viewer - where it can be searched and scrolled like any
    /// file. The count on the status line said how many; this says what.
    fn show_job_report(&mut self) {
        let Some((title, skips)) = &self.last_report else {
            self.status = Some(" no job has skipped anything ".into());
            return;
        };
        let mut text = format!("{} - {} skipped\n\n", title.trim(), skips.len());
        for (path, reason) in skips {
            text.push_str(&format!("{}\n    {reason}\n", path.display()));
        }
        let made = crate::scratch::create("job-report.txt").and_then(|(mut out, path)| {
            std::io::Write::write_all(&mut out, text.as_bytes()).map(|()| path)
        });
        match made {
            Ok(path) => self.open_viewer_on(&path),
            Err(err) => self.status = Some(format!(" report: {err} ")),
        }
    }
}
