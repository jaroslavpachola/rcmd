//! The menu bar, egui's own, above the grid.
//!
//! In a terminal the menu bar is a row of the screen: mc's titles across
//! the top and a dropdown drawn over the panels, which is what
//! `ui.rs` draws and `App::on_menu_key` walks. A window has a better
//! place for it - a real bar, with real dropdowns, that the pointer
//! knows how to use and that does not cost a row of the grid. So the
//! terminal build's bar stays off here (`App::set_external_menubar`)
//! and this draws the same [`MENUS`] and [`EDIT_MENUS`] as egui
//! widgets: the same titles, the same entries, the same key shown
//! beside each, and the same `&` hotkey letter underlined and working,
//! because those tables are the menu, and this is only another way of
//! drawing them.
//!
//! What an entry does is still `App`'s: a click runs the entry through
//! [`App::run_menu_entry`], the same call the terminal build's
//! dropdown makes on Enter, so Left and Right land on their own panel
//! and everything else on the focused one.

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{self, Button, Color32, Id, Popup, TextStyle, Ui};
use rcmd_tui::app::{App, EDIT_MENUS, MENUS, MenuBar, MenuBarFor, menu_hotkey, menu_label};

/// What the bar is told each frame, beyond the app itself.
#[derive(Clone, Copy, Default)]
pub struct Request {
    /// F9: open the leftmost menu, as F9 does in a terminal, and put
    /// the keyboard on its first entry so that the arrow keys and
    /// Enter walk it the way they walk mc's.
    pub open_first: bool,
    /// The shell has the window (Ctrl+O): the bar is drawn, so that
    /// the grid does not jump, but nothing in it acts.
    pub enabled: bool,
}

/// The entries the window adds to the tables: what only a window can
/// set. They hang off the end of the Options dropdown, in both bars.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowEntry {
    /// Options > Font...: the face and size of the grid.
    Font,
}

/// Draw the bar for whatever is on top and run what was chosen.
/// Returns the window's own entry if that is what was chosen.
pub fn show(
    app: &mut App,
    ui: &mut Ui,
    request: Request,
    focus_first: &mut bool,
) -> Option<WindowEntry> {
    let (kind, enabled) = app.menu_bar_for();
    let enabled = enabled && request.enabled;
    let mut extra = None;
    match kind {
        MenuBarFor::Panels => {
            if let Some((menu, action)) = bar(
                ui,
                MENUS,
                enabled,
                request.open_first,
                focus_first,
                &mut extra,
            ) {
                app.run_menu_entry(menu, action);
                app.set_dirty();
            }
        }
        MenuBarFor::Editor => {
            if let Some((_, action)) = bar(
                ui,
                EDIT_MENUS,
                enabled,
                request.open_first,
                focus_first,
                &mut extra,
            ) {
                app.run_edit_menu_entry(action);
                app.set_dirty();
            }
        }
    }
    extra
}

/// The window's entries, under the table's, in the Options dropdown.
fn window_entries(ui: &mut Ui, typed: Option<char>, extra: &mut Option<WindowEntry>) -> bool {
    ui.separator();
    let button = ui.add(Button::new(label(ui, "Fon&t...")));
    if button.clicked() || typed == Some('t') {
        *extra = Some(WindowEntry::Font);
        ui.close();
        return true;
    }
    false
}

/// One bar of titles, each a dropdown of entries. The entry chosen this
/// frame, by pointer, by Enter on the focused one, or by its hotkey
/// letter typed while the dropdown is open, comes back as (menu index,
/// its action).
fn bar<A: Copy>(
    ui: &mut Ui,
    menus: MenuBar<'_, A>,
    enabled: bool,
    open_first: bool,
    focus_first: &mut bool,
    extra: &mut Option<WindowEntry>,
) -> Option<(usize, A)> {
    let ctx = ui.ctx().clone();
    // a letter typed with a dropdown open is mc's hotkey: an entry of
    // that dropdown first, else the title of another
    let typed = if Popup::is_any_open(&ctx) {
        typed_letter(&ctx)
    } else {
        None
    };
    let mut chosen = None;
    let mut popups: Vec<Id> = Vec::with_capacity(menus.len());
    let mut consumed = false;
    ui.add_enabled_ui(enabled, |ui| {
        egui::MenuBar::new().ui(ui, |ui| {
            for (i, (title, entries)) in menus.iter().enumerate() {
                let response = ui
                    .menu_button(label(ui, title), |ui| {
                        let mut first = true;
                        for entry in entries.iter() {
                            let Some((text, key, action)) = entry else {
                                ui.separator();
                                continue;
                            };
                            let button = ui.add(Button::new(label(ui, text)).shortcut_text(*key));
                            if first && std::mem::take(focus_first) {
                                button.request_focus();
                            }
                            first = false;
                            let by_letter = typed.is_some() && typed == menu_hotkey(text);
                            if button.clicked() || by_letter {
                                chosen = Some((i, *action));
                                consumed = true;
                                ui.close();
                            }
                        }
                        if *title == "&Options" && window_entries(ui, typed, extra) {
                            consumed = true;
                        }
                    })
                    .response;
                popups.push(Popup::default_response_id(&response));
            }
        });
    });
    if open_first && enabled {
        if let Some(id) = popups.first() {
            Popup::open_id(&ctx, *id);
        }
        *focus_first = true;
        ctx.request_repaint();
    } else if let Some(c) = typed
        && !consumed
        && let Some(i) = menus
            .iter()
            .position(|(title, _)| menu_hotkey(title) == Some(c))
    {
        Popup::open_id(&ctx, popups[i]);
        *focus_first = true;
        ctx.request_repaint();
    }
    chosen
}

/// A menu label with its `&` hotkey letter underlined, mc's way of
/// showing which key runs the entry. The colour is left to the widget,
/// so a greyed bar greys its letters too.
fn label(ui: &Ui, text: &str) -> LayoutJob {
    let (pre, hot, post) = menu_label(text);
    let font = TextStyle::Button.resolve(ui.style());
    let plain = TextFormat::simple(font.clone(), Color32::PLACEHOLDER);
    let mut job = LayoutJob::default();
    job.append(pre, 0.0, plain.clone());
    if let Some(c) = hot {
        let mut under = plain.clone();
        under.underline = egui::Stroke::new(1.0, Color32::PLACEHOLDER);
        job.append(&c.to_string(), 0.0, under);
    }
    job.append(post, 0.0, plain);
    job
}

/// The one letter typed this frame, if it was a letter and nothing
/// else - what a hotkey is.
fn typed_letter(ctx: &egui::Context) -> Option<char> {
    ctx.input(|i| {
        i.events.iter().find_map(|event| match event {
            egui::Event::Text(text) => {
                let mut chars = text.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if c.is_alphabetic() => Some(c.to_ascii_lowercase()),
                    _ => None,
                }
            }
            _ => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every letter the terminal build underlines is one this bar can
    /// act on: `typed_letter` lowercases, and so does `menu_hotkey`,
    /// so a menu whose letters are unique in mc's table is unique here.
    #[test]
    fn hotkey_letters_are_unique_within_every_dropdown() {
        fn check<A>(menus: MenuBar<'_, A>) {
            for (title, entries) in menus {
                // the window's own Options entry, Fon&t..., spends t
                let mut seen = if *title == "&Options" {
                    vec!['t']
                } else {
                    Vec::new()
                };
                for (text, ..) in entries.iter().flatten() {
                    if let Some(c) = menu_hotkey(text) {
                        assert!(!seen.contains(&c), "{title}: letter {c:?} twice");
                        seen.push(c);
                    }
                }
            }
        }
        check(MENUS);
        check(EDIT_MENUS);
    }

    /// The label keeps every character of the text and loses only the
    /// marker, so "Filtered vie&w..." reads "Filtered view...".
    #[test]
    fn label_drops_only_the_marker() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(Default::default(), |ui| {
            let job = label(ui, "Filtered vie&w...");
            assert_eq!(job.text, "Filtered view...");
            assert_eq!(job.sections.len(), 3);
            assert_eq!(job.sections[1].byte_range.start.0, 12);
            assert_eq!(job.sections[1].byte_range.end.0, 13);
            let job = label(ui, "no marker");
            assert_eq!(job.text, "no marker");
            assert_eq!(job.sections.len(), 1);
        });
        out.textures_delta.clear();
    }

    /// A headless egui context, frame by frame. A real App underneath:
    /// the bar is nothing without one to run its entries on.
    fn app() -> App {
        let cfg = rcmd_tui::config::Config {
            subshell: false,
            ..Default::default()
        };
        let mut app = App::new(&[], cfg, Vec::new(), &rcmd_tui::app::Startup::Panels).unwrap();
        app.set_external_menubar();
        app
    }

    fn frame(
        ctx: &egui::Context,
        app: &mut App,
        events: Vec<egui::Event>,
        focus: &mut bool,
    ) -> Option<WindowEntry> {
        let input = egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let request = Request {
            open_first: app.take_menu_request(),
            enabled: true,
        };
        let mut entry = None;
        let mut out = ctx.run_ui(input, |ui| {
            entry = egui::Panel::top("menubar")
                .show(ui, |ui| show(app, ui, request, focus))
                .inner;
        });
        // no painter here to hand the font atlas to
        out.textures_delta.clear();
        entry
    }

    /// The window's Font... hangs off the Options dropdown and has a
    /// letter like the rest: F9 o t, and the bar hands it back rather
    /// than running anything, the dialog being the window's.
    #[test]
    fn the_windows_font_entry_is_reached_by_letter() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let ctx = egui::Context::default();
        let mut app = app();
        let mut focus = false;
        frame(&ctx, &mut app, Vec::new(), &mut focus);
        app.on_key(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE));
        frame(&ctx, &mut app, Vec::new(), &mut focus);
        frame(&ctx, &mut app, Vec::new(), &mut focus);
        frame(
            &ctx,
            &mut app,
            vec![egui::Event::Text("o".into())],
            &mut focus,
        );
        frame(&ctx, &mut app, Vec::new(), &mut focus);
        assert!(Popup::is_any_open(&ctx), "Options is open");
        let entry = frame(
            &ctx,
            &mut app,
            vec![egui::Event::Text("t".into())],
            &mut focus,
        );
        assert_eq!(entry, Some(WindowEntry::Font));
        assert!(!app.exiting());
        frame(&ctx, &mut app, Vec::new(), &mut focus);
        assert!(!Popup::is_any_open(&ctx), "and the dropdown closed");
    }

    /// F9 opens the leftmost dropdown, as it does in a terminal; a
    /// title's letter switches to that menu and an entry's letter runs
    /// the entry - here `f` for File, then `q` for Quit, which is mc's
    /// F9 f q and ends the session the same way.
    #[test]
    fn f9_opens_the_bar_and_letters_walk_it() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let ctx = egui::Context::default();
        let mut app = app();
        let mut focus = false;
        frame(&ctx, &mut app, Vec::new(), &mut focus);
        assert!(!Popup::is_any_open(&ctx));

        app.on_key(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE));
        assert!(app.menu_requested(), "F9 asks the bar to open");
        assert!(
            app.menu.is_none(),
            "and does not drop the terminal build's menu"
        );
        // the frame that takes the request opens the dropdown, which is
        // on screen from the one after
        frame(&ctx, &mut app, Vec::new(), &mut focus);
        frame(&ctx, &mut app, Vec::new(), &mut focus);
        assert!(Popup::is_any_open(&ctx), "F9 opened a dropdown");
        assert!(!app.exiting());

        frame(
            &ctx,
            &mut app,
            vec![egui::Event::Text("f".into())],
            &mut focus,
        );
        frame(&ctx, &mut app, Vec::new(), &mut focus);
        assert!(
            Popup::is_any_open(&ctx),
            "a title letter keeps a dropdown open"
        );
        assert!(!app.exiting(), "and runs nothing");

        frame(
            &ctx,
            &mut app,
            vec![egui::Event::Text("q".into())],
            &mut focus,
        );
        assert!(app.exiting(), "File > Quit by its letter");
        frame(&ctx, &mut app, Vec::new(), &mut focus);
        assert!(!Popup::is_any_open(&ctx), "and the dropdown closed with it");
    }
}
