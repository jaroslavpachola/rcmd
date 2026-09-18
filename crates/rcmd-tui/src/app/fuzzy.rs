//! Alt+/: a fuzzy finder over the tree under the panel. The walk runs on
//! find's thread pool and streams in; every letter typed re-ranks what
//! has arrived so far, best first, and Enter goes where the pick is.
//! mc has nothing like it; "go to file" in every editor is the model.

use std::sync::mpsc;

use super::*;

/// The most paths one walk collects: past that the tree is too big for
/// a box to be the way into it, and memory is worth more.
const FUZZY_CAP: usize = 500_000;
/// How many ranked rows are kept for the list.
const FUZZY_SHOWN: usize = 1_000;

impl App {
    pub(super) fn open_fuzzy(&mut self) {
        if !self.require_local() {
            return;
        }
        let panel = &self.panels[self.active];
        let root = panel.local_cwd();
        let query = find::Query {
            skip_hidden: !panel.show_hidden,
            ..find::Query::default()
        };
        let walking = match find::spawn_find(root.clone(), query, git::ignore_filter(&root)) {
            Ok(handle) => Some(handle),
            Err(err) => {
                self.status = Some(format!(" {err} "));
                return;
            }
        };
        self.dialog = Some(Dialog::Fuzzy(Box::new(FuzzyDialog {
            field: TextField::new("").with_history("fuzzy"),
            root,
            all: Vec::new(),
            shown: Vec::new(),
            selected: 0,
            top: 0,
            walking,
            ranked_for: None,
        })));
    }

    /// Take in what the walk found since the last frame, and rank again
    /// when there is something new or the pattern changed. False = no
    /// finder open.
    pub(super) fn drain_fuzzy(&mut self) -> bool {
        let Some(Dialog::Fuzzy(d)) = self.dialog.as_mut() else {
            return false;
        };
        let mut arrived = false;
        if let Some(handle) = d.walking.as_ref() {
            // a frame's worth at most, so typing never waits on a walk
            for _ in 0..20_000 {
                match handle.events.try_recv() {
                    Ok(FindEvent::Match(found)) => {
                        if d.all.len() < FUZZY_CAP {
                            let name = found.entry.name.to_string_lossy().into_owned();
                            d.all.push((name, found.entry.is_dir()));
                            arrived = true;
                        }
                    }
                    Ok(FindEvent::Done { .. }) | Err(mpsc::TryRecvError::Disconnected) => {
                        d.walking = None;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
        }
        if arrived || d.ranked_for.as_deref() != Some(d.field.value.as_str()) {
            d.rank();
        }
        true
    }

    pub(super) fn on_fuzzy_key(&mut self, mut d: Box<FuzzyDialog>, key: KeyEvent) {
        let page = ui::find_list_rows(self.areas.screen)
            .saturating_sub(2)
            .max(1);
        match key.code {
            KeyCode::Esc => {
                if let Some(handle) = d.walking.take() {
                    handle.cancel();
                }
                return;
            }
            KeyCode::Enter | KeyCode::F(3) | KeyCode::F(4) => {
                let Some(&at) = d.shown.get(d.selected) else {
                    self.dialog = Some(Dialog::Fuzzy(d));
                    return;
                };
                if let Some(handle) = d.walking.take() {
                    handle.cancel();
                }
                let _ = d.field.remember();
                let (rel, is_dir) = d.all[at].clone();
                let path = d.root.join(&rel);
                let side = self.active;
                if is_dir && key.code == KeyCode::Enter {
                    let _ = self.panels[side].request_dir(path, LoadKind::Enter);
                    return;
                }
                if let Some(dir) = path.parent() {
                    let _ = self.panels[side].request_dir(dir.to_path_buf(), LoadKind::Enter);
                    if let Some(name) = path.file_name() {
                        self.panels[side].select_name(name);
                    }
                }
                match key.code {
                    KeyCode::F(3) => self.open_viewer(false),
                    KeyCode::F(4) => self.open_editor(),
                    _ => {}
                }
                return;
            }
            KeyCode::Up => d.selected = d.selected.saturating_sub(1),
            KeyCode::Down => d.selected = (d.selected + 1).min(d.shown.len().saturating_sub(1)),
            KeyCode::PageUp => d.selected = d.selected.saturating_sub(page),
            KeyCode::PageDown => {
                d.selected = (d.selected + page).min(d.shown.len().saturating_sub(1))
            }
            _ => {
                if d.field.key(key) {
                    d.rank();
                }
            }
        }
        self.dialog = Some(Dialog::Fuzzy(d));
    }
}

impl FuzzyDialog {
    /// Rank everything that has arrived against what is typed.
    pub fn rank(&mut self) {
        let pattern = self.field.value.clone();
        let mut ranked = rcmd_core::fuzzy::rank(&pattern, self.all.iter().map(|(p, _)| p.as_str()));
        ranked.truncate(FUZZY_SHOWN);
        // the same row stays picked while more of the tree comes in
        let keep = self.shown.get(self.selected).copied();
        self.shown = ranked;
        self.selected = keep
            .filter(|_| self.ranked_for.as_deref() == Some(pattern.as_str()))
            .and_then(|at| self.shown.iter().position(|&i| i == at))
            .unwrap_or(0);
        self.ranked_for = Some(pattern);
    }
}
