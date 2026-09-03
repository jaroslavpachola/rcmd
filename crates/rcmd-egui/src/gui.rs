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
use ratatui::crossterm::event::KeyEvent;
use rcmd_tui::app::{App, Exec};
use rcmd_tui::{state, ui};

use crate::exec;
use crate::grid::{EguiBackend, Metrics, Palette};
use crate::keys::{self, Input};
use crate::menu;
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
}

impl Gui {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        app: App,
        font_size: f32,
        startup_keys: Vec<KeyEvent>,
    ) -> anyhow::Result<Self> {
        crate::font::install(&cc.egui_ctx);
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
        // Ctrl+O closes it; everything typed before that still reaches
        // the shell, the way the terminal build feeds the bytes ahead
        // of the 0x0F and then breaks
        let open = pane.feed(&mut self.app, &input) & pane.step(&mut self.app);
        pane.paint(ui.painter(), origin, self.metrics, &self.font, self.palette);
        let wait = pane.repaint_after();
        if !open {
            pane.close();
            self.app.end_subshell();
            self.app.finish_remote_edit();
            // the panels are back and owed a frame
            self.app.set_dirty();
            return Duration::ZERO;
        }
        wait
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
        let menu_open = Popup::is_any_open(&ctx);
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
        egui::Panel::top("menubar").show(ui, |ui| menu::show(app, ui, request, focus_menu));

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
            ctx.input(|i| keys::collect(i, origin, self.metrics))
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
            let wait = self.pane_frame(ui, origin, input);
            ctx.request_repaint_after(wait);
            return;
        }

        // Input, in arrival order, into the same handlers the terminal
        // build calls.
        if !input.is_empty() {
            // whatever the event turns out to be, the screen may answer it
            self.app.set_dirty();
            for event in input {
                match event {
                    Input::Key(key) => self.app.on_key(key),
                    Input::Mouse(mouse) => self.app.on_mouse(mouse),
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
