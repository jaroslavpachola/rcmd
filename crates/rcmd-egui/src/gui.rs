//! The window: an [`eframe::App`] wrapped around the same
//! [`rcmd_tui::app::App`] the terminal build drives.
//!
//! The loops are inside out from each other. `App::run` owns its loop
//! and blocks on `event::poll`; egui owns its loop and calls us once
//! per frame. What makes the second one possible is that the body of
//! the first is `App::tick` - drain the worker channels, retire the ESC
//! prefix, say whether anything is moving - and that is a function, not
//! a loop. Here it runs once per frame, and the poll timeout becomes a
//! `request_repaint_after`, so an idle window sleeps exactly as an idle
//! terminal does.

use std::time::{Duration, Instant};

use eframe::egui::{self, FontId, Popup, Vec2};
use ratatui::Terminal;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rcmd_tui::app::{App, Exec};
use rcmd_tui::{state, ui};

use crate::dialog::{FontDialog, Verdict};
use crate::exec;
use crate::grid::{EguiBackend, Metrics, Palette};
use crate::keys::{self, Input};
use crate::menu::{self, WindowEntry};
use crate::select::Selecting;
use crate::settings::{self, Window};
use crate::term::TerminalPane;

/// Redraw at least this often even when nothing said it changed - the
/// same insurance `App::run` takes, for the same reason.
const IDLE_FRAME: Duration = Duration::from_secs(2);

pub struct Gui {
    app: App,
    terminal: Terminal<EguiBackend>,
    font: FontId,
    metrics: Metrics,
    palette: Palette,
    /// The grid size the backend was last told about, so a resize is
    /// noticed without asking the backend through a trait import.
    size: (u16, u16),
    last_frame: Instant,
    /// Keys still to be played in as if typed, one per frame. `$RCMD_EGUI_KEYS`
    /// fills this: a window cannot be driven from a script the way the
    /// pty suite drives the terminal build, so this is how a screenshot
    /// gets taken of anything that needs a keystroke to reach.
    startup_keys: Vec<KeyEvent>,
    /// Ctrl+O's output screen. While a session is open on it, the
    /// panels are neither drawn nor given any input: the shell has the
    /// window, exactly as it has the terminal in the other build. The
    /// screen itself stays between sessions, as a terminal's does.
    pane: TerminalPane,
    /// Set once the state file has been written, so the closing frames
    /// do not write it again.
    saved: bool,
    /// F9 opened a dropdown and its first entry is owed the keyboard
    /// focus as soon as the dropdown is on screen, which is a frame
    /// after it was asked for.
    focus_menu: bool,
    /// The font settings in force: config, state and the session's
    /// overrides layered. What the Font dialog opens on and what
    /// Ctrl+= / Ctrl+- move.
    window: Window,
    /// The size `[window]` in config.toml gives, or the default: what
    /// Ctrl+0 goes back to.
    config_size: f32,
    /// What `font::install` last loaded, for the dialog to say.
    loaded_font: Option<String>,
    font_dialog: Option<FontDialog>,
    /// A scroll too small to be a step yet: see [`keys::collect`].
    wheel: f32,
    /// The right-click menu, open at this point.
    context: Option<egui::Pos2>,
    /// Text being selected with the mouse: a plain drag in the shell
    /// pane, a Shift+drag over the panels - see [`crate::select`].
    pane_select: Selecting,
    grid_select: Selecting,
}

/// The right-click menu: what a file manager is asked to do to the
/// thing under the pointer, by the names the keymap knows them by.
const CONTEXT_MENU: &[(&str, &str)] = &[
    ("View", "view"),
    ("Edit", "edit"),
    ("Copy...", "copy"),
    ("Move/rename...", "move"),
    ("Delete", "delete"),
    ("Mark / unmark", "mark"),
    ("Pack into archive...", "pack"),
    ("Checksum...", "checksum"),
    ("Diff against HEAD", "diff-head"),
    ("Info", "info-view"),
];

impl Gui {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        app: App,
        window: Window,
        config_size: f32,
        startup_keys: Vec<KeyEvent>,
    ) -> anyhow::Result<Self> {
        let loaded_font = crate::font::install(&cc.egui_ctx, window.font.as_deref());
        // pictures in the viewer and the quick view: an image here is a
        // texture and a rectangle, which is all a terminal cannot do
        egui_extras::install_image_loaders(&cc.egui_ctx);
        // Ctrl+= / Ctrl+- / Ctrl+0 change the grid's font size below,
        // not egui's zoom factor: the two on the same keys would be a
        // bar growing twice as fast as the grid
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let font_size = window.size();
        let font = FontId::monospace(font_size);
        // no fonts exist until egui has run a frame; `ui` measures
        // for real on the first one
        let metrics = Metrics::estimate(font_size);
        let palette = palette_from_theme();
        // The menu bar and its dropdowns are egui widgets on the grid's
        // own background, and egui's light or dark set of widget
        // colours is chosen by how bright that background is - a
        // dark-grey menu bar over `-S bw`'s white would be a bar from
        // some other program.
        cc.egui_ctx.set_visuals(visuals_for(palette));
        // a placeholder size: the first frame measures the window and
        // resizes before anything is drawn into it
        let terminal = Terminal::new(EguiBackend::new(80, 25, palette))?;
        Ok(Self {
            app,
            terminal,
            font,
            metrics,
            palette,
            size: (80, 25),
            last_frame: Instant::now(),
            startup_keys,
            pane: TerminalPane::new(80, 25),
            saved: false,
            focus_menu: false,
            window,
            config_size,
            loaded_font,
            font_dialog: None,
            wheel: 0.0,
            context: None,
            pane_select: Selecting::default(),
            grid_select: Selecting::default(),
        })
    }

    /// Run whatever a key press queued.
    ///
    /// Ctrl+O and typed commands open the embedded terminal pane, which
    /// is this build's answer to having no tty to hand over: the pty is
    /// the subshell's already, and the pane is the half that reads what
    /// it writes. An opener still goes to a detached child, which is
    /// what an opener always wanted, and a machine with no subshell at
    /// all falls back to a terminal emulator.
    fn run_exec(&mut self, cmd: Exec) {
        let quiet = matches!(cmd, Exec::Quiet(_));
        if !quiet && self.app.subshell_alive() {
            let (cols, rows) = self.size;
            self.pane.open(&mut self.app, cmd, cols, rows);
            self.app.set_dirty();
            return;
        }
        let cwd = self.app.panels[self.app.active].local_cwd();
        match exec::run(&cmd, &cwd) {
            Ok(Some(note)) => self.app.status = Some(format!(" {note} ")),
            Ok(None) => {}
            Err(err) => self.app.status = Some(format!(" {err} ")),
        }
        self.app.finish_remote_edit();
        self.app.set_dirty();
    }

    /// One frame with the shell in front. Returns the repaint interval:
    /// a pane is redrawn far more eagerly than idle panels, because
    /// what it is showing moves on its own.
    fn pane_frame(&mut self, ui: &mut egui::Ui, origin: egui::Pos2, input: Vec<Input>) -> Duration {
        let pane = &mut self.pane;
        let (cols, rows) = self.size;
        pane.resize(&mut self.app, cols, rows);
        // the shell takes no mouse, so every drag is a selection, and
        // letting go copies it, as a terminal does
        let (input, done) = self.pane_select.filter(input, true);
        if let Some(sel) = done {
            let sel = sel.clamp(cols, rows);
            ui.ctx()
                .copy_text(sel.text(cols, |col, row| pane.symbol(col, row)));
        }
        // Ctrl+O closes it; everything typed before that still reaches
        // the shell, the way the terminal build feeds the bytes ahead
        // of the 0x0F and then breaks
        let open = pane.feed(&mut self.app, &input) & pane.step(&mut self.app);
        pane.paint(ui.painter(), origin, self.metrics, &self.font, self.palette);
        if let Some(sel) = self.pane_select.shown {
            sel.clamp(cols, rows)
                .paint(ui.painter(), origin, self.metrics, cols);
        }
        let wait = pane.repaint_after();
        if !open {
            pane.close();
            self.pane_select.clear();
            self.app.end_subshell();
            self.app.finish_remote_edit();
            // the panels are back and owed a frame
            self.app.set_dirty();
            return Duration::ZERO;
        }
        wait
    }

    /// A new font size, from the keys or the dialog: the grid picks it
    /// up on the next frame, and it is written down so the next window
    /// starts with it.
    fn set_font_size(&mut self, size: f32, persist: bool) {
        let size = size.clamp(4.0, 96.0);
        self.window.font_size = Some(size);
        self.font = FontId::monospace(size);
        self.app.set_dirty();
        if persist && let Err(err) = settings::save(|state| state.font_size = Some(size)) {
            self.app.status = Some(format!(" could not save font size: {err} "));
        }
    }

    /// A new face, from the dialog: installed now, so the grid behind
    /// the dialog shows it.
    fn set_font(&mut self, ctx: &egui::Context, font: Option<String>) {
        self.window.font = font;
        self.loaded_font = crate::font::install(ctx, self.window.font.as_deref());
        self.app.set_dirty();
    }

    /// Ctrl+= / Ctrl++ and Ctrl+- step the size, Ctrl+0 puts it back
    /// to the configured one, the way every windowed terminal does.
    /// Anything else goes through to the panels.
    fn size_key(&mut self, key: &KeyEvent) -> bool {
        let Some(size) = stepped_size(key, self.window.size(), self.config_size) else {
            return false;
        };
        self.set_font_size(size, true);
        true
    }

    /// The Font dialog's frame: what it changed is applied at once,
    /// what it decided is kept or put back.
    fn font_dialog_frame(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.font_dialog.as_mut() else {
            return;
        };
        match dialog.show(ctx) {
            Verdict::Open { changed } => {
                if changed {
                    let choice = dialog.choice.clone();
                    if choice.font != self.window.font {
                        self.set_font(ctx, choice.font.clone());
                        if let Some(dialog) = self.font_dialog.as_mut() {
                            dialog.loaded = self.loaded_font.clone();
                        }
                    }
                    if choice.font_size != self.window.font_size {
                        self.set_font_size(choice.size(), false);
                    }
                }
            }
            Verdict::Keep => {
                let choice = self.window.clone();
                self.font_dialog = None;
                if let Err(err) = settings::save(|state| {
                    state.font = choice.font.clone();
                    state.font_size = choice.font_size;
                }) {
                    self.app.status = Some(format!(" could not save the font: {err} "));
                }
                self.app.set_dirty();
            }
            Verdict::Revert => {
                let before = dialog.before().clone();
                self.font_dialog = None;
                if before.font != self.window.font {
                    self.set_font(ctx, before.font.clone());
                }
                self.set_font_size(before.size(), false);
            }
        }
    }

    /// An image the viewer or the quick view is on, painted over the
    /// cells that would show its bytes, as large as fits.
    fn paint_image(&self, ui: &mut egui::Ui, origin: egui::Pos2) {
        let Some((path, cells)) = self.app.image_on_screen() else {
            return;
        };
        let m = self.metrics;
        let rect = egui::Rect::from_min_size(
            origin + Vec2::new(cells.x as f32 * m.width, cells.y as f32 * m.height),
            Vec2::new(cells.width as f32 * m.width, cells.height as f32 * m.height),
        );
        ui.painter().rect_filled(rect, 0.0, self.palette.bg);
        let image = egui::Image::new(format!("file://{}", path.display()))
            .max_size(rect.size())
            .maintain_aspect_ratio(true);
        ui.put(rect, image);
    }

    /// The right-click menu, while it is open.
    fn context_menu(&mut self, ctx: &egui::Context) {
        let Some(pos) = self.context else {
            return;
        };
        let mut chosen = None;
        let area = egui::Area::new(egui::Id::new("context-menu"))
            .fixed_pos(pos)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    for (label, action) in CONTEXT_MENU {
                        if ui.button(*label).clicked() {
                            chosen = Some(*action);
                        }
                    }
                })
            });
        let away = ctx.input(|i| {
            i.key_pressed(egui::Key::Escape)
                || (i.pointer.any_pressed()
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|p| !area.response.rect.contains(p)))
        });
        if let Some(action) = chosen {
            self.context = None;
            self.app.run_named_action(action);
        } else if away {
            self.context = None;
            self.app.set_dirty();
        }
    }

    /// What the window does that a terminal cannot: copies go to the
    /// desktop's clipboard through egui, files dropped on a panel are
    /// copied into it, and an input method is told where the cursor is.
    fn window_io(&mut self, ctx: &egui::Context, origin: egui::Pos2) {
        if let Some(text) = self.app.take_clipboard_out() {
            ctx.copy_text(text);
        }
        let m = self.metrics;
        let cell = |p: egui::Pos2| {
            (
                ((p.x - origin.x) / m.width).max(0.0) as u16,
                ((p.y - origin.y) / m.height).max(0.0) as u16,
            )
        };
        let (dropped, hovering, at) = ctx.input(|i| {
            (
                i.raw.dropped_files.clone(),
                !i.raw.hovered_files.is_empty(),
                i.pointer.latest_pos(),
            )
        });
        let paths: Vec<std::path::PathBuf> = dropped
            .iter()
            .map(|f| f.path().to_path_buf())
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        if !paths.is_empty() {
            let (x, y) = at.map(cell).unwrap_or((0, 0));
            self.app.drop_paths(paths, x, y);
            ctx.request_repaint();
        } else if hovering {
            self.app.status = Some(" drop to copy into the panel under the pointer ".into());
            self.app.set_dirty();
        }
        if let Some(pos) = self.terminal.backend().cursor() {
            let rect = egui::Rect::from_min_size(
                origin + Vec2::new(pos.x as f32 * m.width, pos.y as f32 * m.height),
                Vec2::new(m.width, m.height),
            );
            ctx.output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    purpose: Default::default(),
                    rect,
                    cursor_rect: rect,
                    should_interrupt_composition: false,
                })
            });
        }
    }

    fn save_once(&mut self) {
        if self.saved {
            return;
        }
        self.saved = true;
        self.app.cancel_background();
        if let Err(err) = state::save_session(&self.app) {
            eprintln!("rcmd-egui: could not save state: {err}");
        }
    }
}

impl eframe::App for Gui {
    /// The window's own background, behind the grid. Painting it in the
    /// palette's background stops a one-pixel border of egui's default
    /// grey showing along the edges where the cells do not divide the
    /// window evenly.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let bg = self.palette.bg;
        [
            bg.r() as f32 / 255.0,
            bg.g() as f32 / 255.0,
            bg.b() as f32 / 255.0,
            1.0,
        ]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // the window's title says where the active panel is
        let title = self.app.title();
        if self.app.title_shown.as_deref() != Some(title.as_str()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.app.title_shown = Some(title);
        }
        // a finished job says so on the desktop: the window has no bell
        // anyone hears, and is likely not the one being looked at
        for notice in std::mem::take(&mut self.app.notices) {
            notify_desktop(&notice);
        }
        // Fonts are only loaded once egui has run a frame, so the
        // metrics are re-measured until they settle rather than trusted
        // from construction time.
        self.metrics = Metrics::measure(&ctx, &self.font);

        // The menu bar first, egui's own across the top: F9 pressed
        // last frame asked for it to open, and while the shell has the
        // window it is there but greyed. With a dropdown open the
        // keyboard and the pointer are egui's, not the panels': a Down
        // that walked the dropdown must not also walk the file list,
        // and the click that closes the dropdown must not also land
        // on whatever was under it.
        // with the Font dialog up the keyboard is its, like a dropdown's
        let menu_open =
            Popup::is_any_open(&ctx) || self.font_dialog.is_some() || self.context.is_some();
        // F9 with a dropdown open closes it, as it does in a terminal;
        // Esc egui does by itself
        if menu_open && ctx.input(|i| i.key_pressed(egui::Key::F9)) {
            Popup::close_all(&ctx);
        }
        let request = menu::Request {
            open_first: self.app.take_menu_request(),
            enabled: !self.pane.is_open(),
        };
        let Self {
            app, focus_menu, ..
        } = self;
        let entry = egui::Panel::top("menubar")
            .show(ui, |ui| menu::show(app, ui, request, focus_menu))
            .inner;
        if entry == Some(WindowEntry::Font) && self.font_dialog.is_none() {
            self.font_dialog = Some(FontDialog::open(&self.window, self.loaded_font.clone()));
        }
        self.font_dialog_frame(&ctx);

        // No dropdown is deliberately open: clear any focus that landed
        // on a menu-bar button so that Enter and arrow keys reach the
        // grid instead of activating egui's widget.
        if !menu_open && let Some(id) = ctx.memory(|m| m.focused()) {
            ctx.memory_mut(|m| m.surrender_focus(id));
        }

        // The rest of the window is the grid: no margins, because a
        // cell grid that does not start at the corner under the bar is
        // a cell grid with a wasted row and column.
        let available = ui.available_rect_before_wrap();
        let origin = available.min;
        let (cols, rows) = self.metrics.cells(available.size());
        if (cols, rows) != self.size {
            self.size = (cols, rows);
            self.terminal.backend_mut().set_size(cols, rows);
            // the pane resizes itself from `self.size`; this is the
            // subshell's own idea of how big its terminal is
            self.app.resize_subshell(cols, rows);
            self.app.set_dirty();
        }

        let mut input = if menu_open {
            Vec::new()
        } else {
            ctx.input(|i| keys::collect(i, origin, self.metrics, &mut self.wheel))
        };
        // $RCMD_EGUI_KEYS, one per frame ahead of anything real. One per
        // frame rather than all at once because that is what typing is:
        // a key that opens a screen has to be given the frame in which
        // to open it before the next key arrives, or the next key goes
        // to the screen that was on its way out.
        if !self.startup_keys.is_empty() && !menu_open {
            let key = self.startup_keys.remove(0);
            input.insert(0, Input::Key(key));
            ctx.request_repaint();
        }

        // With the pane open the shell owns the window: the panels are
        // neither drawn nor given any input, exactly as they are not in
        // the terminal build while the output screen is up.
        if self.pane.is_open() {
            // a selection over the panels does not outlive them
            self.grid_select.clear();
            let wait = self.pane_frame(ui, origin, input);
            ctx.request_repaint_after(wait);
            return;
        }

        // Shift+drag selects the screen's text, as Shift does in a
        // terminal that rcmd has asked for the mouse; the panels never
        // see that drag
        let (input, done) = self.grid_select.filter(input, false);
        if let Some(sel) = done {
            let (cols, rows) = self.size;
            let backend = self.terminal.backend();
            ctx.copy_text(
                sel.clamp(cols, rows)
                    .text(cols, |col, row| backend.symbol(col, row)),
            );
        }

        // Input, in arrival order, into the same handlers the terminal
        // build calls.
        if !input.is_empty() {
            // whatever the event turns out to be, the screen may answer it
            self.app.set_dirty();
            for event in input {
                match event {
                    Input::Key(key) if self.size_key(&key) => {}
                    Input::Key(key) => self.app.on_key(key),
                    Input::Mouse(mouse) => self.app.on_mouse(mouse),
                    Input::Paste(text) => self.app.on_paste(&text),
                    // the cursor goes to what was clicked, and the menu
                    // is about that
                    Input::Context { pos, column, row } => {
                        self.app.on_mouse(ratatui::crossterm::event::MouseEvent {
                            kind: ratatui::crossterm::event::MouseEventKind::Down(
                                ratatui::crossterm::event::MouseButton::Left,
                            ),
                            column,
                            row,
                            modifiers: KeyModifiers::NONE,
                        });
                        self.context = Some(pos);
                    }
                }
            }
            // F9: the bar opens on the next frame, which has to come
            // sooner than the idle wake-up would bring it
            if self.app.menu_requested() {
                ctx.request_repaint();
            }
        }

        let busy = self.app.tick();

        if self.app.dirty() || busy || self.last_frame.elapsed() >= IDLE_FRAME {
            if self.app.take_repaint() {
                let _ = self.terminal.clear();
            }
            // the one line this whole crate exists to be able to write:
            // the terminal build's drawing code, unchanged
            let Self { app, terminal, .. } = self;
            if let Err(err) = terminal.draw(|frame| ui::draw(frame, app)) {
                eprintln!("rcmd-egui: draw failed: {err}");
            }
            self.app.clear_dirty();
            self.last_frame = Instant::now();
        }

        self.terminal
            .backend()
            .paint(ui.painter(), origin, self.metrics, &self.font);
        if let Some(sel) = self.grid_select.shown {
            let (cols, rows) = self.size;
            sel.clamp(cols, rows)
                .paint(ui.painter(), origin, self.metrics, cols);
        }
        self.paint_image(ui, origin);
        self.context_menu(&ctx);
        self.window_io(&ctx, origin);

        if let Some(cmd) = self.app.take_exec() {
            self.run_exec(cmd);
        }
        self.app.hold_quit_for_jobs();

        if self.app.exiting() {
            self.save_once();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // The poll timeout, spelled as a wake-up. An idle rcmd in a
        // terminal wakes twice a second to check its channels; so does
        // this, and for the same reason it costs nothing.
        ctx.request_repaint_after(Duration::from_millis(if busy { 50 } else { 500 }));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_once();
    }
}

/// The size a Ctrl+= / Ctrl++ / Ctrl+- / Ctrl+0 asks for, from the
/// one in force and the configured one; `None` for any other key.
fn stepped_size(key: &KeyEvent, size: f32, config_size: f32) -> Option<f32> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        KeyCode::Char('=') | KeyCode::Char('+') => Some(size + 1.0),
        KeyCode::Char('-') => Some(size - 1.0),
        KeyCode::Char('0') => Some(config_size),
        _ => None,
    }
}

/// About what the menu bar takes at the top, in points: egui's
/// interact height plus the panel's margins and its separator. An
/// estimate, as the rest of the starting size is.
const MENU_BAR_HEIGHT: f32 = 26.0;

/// The window's starting size in points: 100x30 cells under the menu
/// bar, which is a comfortable two panels. Estimated rather than
/// measured, the fonts not existing until the window is up.
pub fn window_size(font_size: f32) -> Vec2 {
    let metrics = Metrics::estimate(font_size);
    Vec2::new(
        metrics.width * 100.0,
        metrics.height * 30.0 + MENU_BAR_HEIGHT,
    )
}

/// egui's widget colours for the menu bar and its dropdowns: the light
/// set on a bright grid, the dark set on a dark one, either way on the
/// grid's own background so that the bar is a part of the window and
/// not a strip of egui's grey across the top of it.
fn visuals_for(palette: Palette) -> egui::Visuals {
    let bg = palette.bg;
    let bright =
        0.299 * f32::from(bg.r()) + 0.587 * f32::from(bg.g()) + 0.114 * f32::from(bg.b()) > 140.0;
    let mut visuals = if bright {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    visuals.panel_fill = bg;
    visuals.window_fill = bg;
    visuals
}

/// What a `Color::Reset` cell resolves to. In a terminal that is the
/// user's own foreground and background; here the closest honest thing
/// is the theme's, so `-S bw` gives a white window and `-S dark` a dark
/// one rather than both being whatever this file happened to hardcode.
fn palette_from_theme() -> Palette {
    let (fg, bg) = ui::base_colors();
    let fallback = Palette::default();
    Palette {
        fg: crate::grid::to_color32(fg, fallback.fg),
        bg: crate::grid::to_color32(bg, fallback.bg),
    }
}

/// A desktop notification through whatever the system has for it -
/// `notify-send` on a freedesktop desktop, `osascript` on a Mac. Neither
/// there is no error: the status line said it already.
fn notify_desktop(text: &str) {
    let spawned = if cfg!(target_os = "macos") {
        let quoted = text.replace('\\', "\\\\").replace('"', "\\\"");
        std::process::Command::new("osascript")
            .args([
                "-e",
                &format!("display notification \"{quoted}\" with title \"rcmd\""),
            ])
            .spawn()
    } else {
        std::process::Command::new("notify-send")
            .args(["--app-name=rcmd", "rcmd", text])
            .spawn()
    };
    // reaped on a thread of its own, so no zombie is left behind
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || child.wait());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_size_keys_step_and_reset_and_nothing_else_is_theirs() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert_eq!(stepped_size(&ctrl('='), 14.0, 12.0), Some(15.0));
        assert_eq!(stepped_size(&ctrl('+'), 14.0, 12.0), Some(15.0));
        assert_eq!(stepped_size(&ctrl('-'), 14.0, 12.0), Some(13.0));
        assert_eq!(stepped_size(&ctrl('0'), 14.0, 12.0), Some(12.0));
        // a bare - is unselect group, a bare 0 a digit typed: the panels'
        let plain = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert_eq!(stepped_size(&plain('-'), 14.0, 12.0), None);
        assert_eq!(stepped_size(&plain('0'), 14.0, 12.0), None);
        assert_eq!(stepped_size(&ctrl('x'), 14.0, 12.0), None);
    }
}
