//! Alt+X: the command palette. Every action rcmd has already has a
//! name, because the config and the socket needed one; typed at here, a
//! few letters of it - or of its menu label - find it, with the key it
//! is on beside it. A feature with no key to spare needs no key.

use super::*;

/// One action in the palette.
pub struct PaletteRow {
    pub name: &'static str,
    /// Its entry in the F9 menus, `&`s taken out, where it has one.
    pub label: String,
    /// The keys it is on, as the keymap spells them.
    pub keys: String,
    action: Action,
}

pub struct PaletteDialog {
    pub field: TextField,
    pub rows: Vec<PaletteRow>,
    /// Indices into `rows`, best match first.
    pub shown: Vec<usize>,
    pub selected: usize,
}

impl PaletteDialog {
    fn rank(&mut self) {
        let texts: Vec<String> = self
            .rows
            .iter()
            .map(|r| format!("{} {}", r.name, r.label))
            .collect();
        self.shown = rcmd_core::fuzzy::rank(&self.field.value, texts.iter().map(String::as_str));
        self.selected = 0;
    }
}

/// An action's identity, for finding it in the menus and the keymap:
/// the enum has payloads and no `PartialEq`, and its `Debug` form is
/// exactly what tells two of them apart.
fn same(a: &Action, b: &Action) -> bool {
    format!("{a:?}") == format!("{b:?}")
}

impl App {
    pub(super) fn open_palette(&mut self) {
        let menu_entries: Vec<(&str, &str, Action)> = MENUS
            .iter()
            .flat_map(|(_, entries)| entries.iter().flatten())
            .map(|&(label, key, action)| (label, key, action))
            .collect();
        let rows = crate::keymap::ACTIONS
            .iter()
            .map(|&(name, action)| {
                let menu = menu_entries.iter().find(|(_, _, a)| same(a, &action));
                let mut keys: Vec<String> = self
                    .keymap
                    .iter()
                    .filter(|(_, bound)| same(bound, &action))
                    .map(|(&(code, mods), _)| crate::keymap::key_name(code, mods))
                    .collect();
                // the C-x chords are not in the keymap; the menu names them
                if keys.is_empty()
                    && let Some((_, hint, _)) = menu
                    && !hint.is_empty()
                {
                    keys.push(hint.to_string());
                }
                keys.sort();
                PaletteRow {
                    name,
                    label: menu.map_or_else(String::new, |(label, _, _)| label.replace('&', "")),
                    keys: keys.join(", "),
                    action,
                }
            })
            .collect();
        let mut dialog = PaletteDialog {
            field: TextField::new("").with_history("palette"),
            rows,
            shown: Vec::new(),
            selected: 0,
        };
        dialog.rank();
        self.dialog = Some(Dialog::Palette(Box::new(dialog)));
    }

    pub(super) fn on_palette_key(&mut self, mut d: Box<PaletteDialog>, key: KeyEvent) {
        let last = d.shown.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let Some(row) = d.shown.get(d.selected).and_then(|&at| d.rows.get(at)) else {
                    return;
                };
                let action = row.action;
                self.remember(&d.field);
                self.run_action(action);
            }
            KeyCode::Up => {
                d.selected = d.selected.saturating_sub(1);
                self.dialog = Some(Dialog::Palette(d));
            }
            KeyCode::Down => {
                d.selected = (d.selected + 1).min(last);
                self.dialog = Some(Dialog::Palette(d));
            }
            KeyCode::PageUp => {
                d.selected = d.selected.saturating_sub(10);
                self.dialog = Some(Dialog::Palette(d));
            }
            KeyCode::PageDown => {
                d.selected = (d.selected + 10).min(last);
                self.dialog = Some(Dialog::Palette(d));
            }
            _ => {
                if d.field.key(key) {
                    d.rank();
                }
                self.dialog = Some(Dialog::Palette(d));
            }
        }
    }
}
