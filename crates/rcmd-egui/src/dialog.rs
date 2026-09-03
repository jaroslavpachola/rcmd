//! The Font dialog: Options > Font... in the window's menu bar.
//!
//! The one setting the terminal build cannot have. A family from the
//! system's monospaced ones, or the built-in search, and a size; both
//! applied as they are changed, so the grid behind the dialog is the
//! preview. OK writes them to the window's state file, Cancel (or Esc)
//! puts back what the window opened with.

use eframe::egui::{self, Ui};

use crate::settings::Window;

/// What the window ran with when the dialog opened, to go back to.
pub struct FontDialog {
    /// The monospaced families found on the system, gathered once when
    /// the dialog opened: the lookup reads every font file's header.
    families: Vec<String>,
    /// The choice being made; `font` empty is the built-in search.
    pub choice: Window,
    before: Window,
    /// What `font::install` last said it loaded for the choice, shown
    /// under the list, so a name that resolved to something else is
    /// seen rather than wondered about.
    pub loaded: Option<String>,
}

/// What a frame of the dialog asks the window to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Still open; `changed` says the grid is owed a new font.
    Open { changed: bool },
    /// OK: keep the choice and write it down.
    Keep,
    /// Cancel, Esc, a click outside: back to what it was.
    Revert,
}

impl FontDialog {
    pub fn open(current: &Window, loaded: Option<String>) -> Self {
        Self {
            families: crate::font::families(),
            choice: current.clone(),
            before: current.clone(),
            loaded,
        }
    }

    /// What to go back to on Revert.
    pub fn before(&self) -> &Window {
        &self.before
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Verdict {
        let mut verdict = Verdict::Open { changed: false };
        let before = self.choice.clone();
        let response = egui::Modal::new(egui::Id::new("font-dialog")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading("Font");
            ui.add_space(6.0);
            egui::Grid::new("font-grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Family");
                    self.family_picker(ui);
                    ui.end_row();
                    ui.label("Size");
                    let mut size = self.choice.size();
                    if ui
                        .add(egui::Slider::new(&mut size, 6.0..=48.0).suffix(" pt"))
                        .changed()
                    {
                        self.choice.font_size = Some(size);
                    }
                    ui.end_row();
                });
            if let Some(name) = &self.loaded {
                ui.add_space(4.0);
                ui.weak(format!("drawing with {name}"));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    verdict = Verdict::Keep;
                }
                if ui.button("Cancel").clicked() {
                    verdict = Verdict::Revert;
                }
            });
        });
        if response.should_close() && verdict == (Verdict::Open { changed: false }) {
            verdict = Verdict::Revert;
        }
        if let Verdict::Open { changed } = &mut verdict {
            *changed = self.choice != before;
        }
        verdict
    }

    fn family_picker(&mut self, ui: &mut Ui) {
        let current = self.choice.font.clone().unwrap_or_default();
        let shown = if current.is_empty() {
            "(system monospace)".to_string()
        } else {
            current.clone()
        };
        egui::ComboBox::from_id_salt("font-family")
            .width(240.0)
            .selected_text(shown)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(current.is_empty(), "(system monospace)")
                    .clicked()
                {
                    self.choice.font = Some(String::new());
                }
                for name in &self.families {
                    if ui.selectable_label(current == *name, name).clicked() {
                        self.choice.font = Some(name.clone());
                    }
                }
            });
    }
}
