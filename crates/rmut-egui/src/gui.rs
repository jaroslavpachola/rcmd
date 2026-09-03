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

use eframe::egui::{self, FontId, Vec2};
use ratatui::Terminal;
use ratatui::crossterm::event::KeyEvent;
use rcmd_tui::app::{App, Exec};
use rcmd_tui::{state, ui};

use crate::exec;
use crate::grid::{EguiBackend, Metrics, Palette};
use crate::keys::{self, Input};
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
    /// Keys still to be played in as if typed, one per frame. `$RMUT_KEYS`
    /// fills this: a window cannot be driven from a script the way the
    /// pty suite drives the terminal build, so this is how a screenshot
    /// gets taken of anything that needs a keystroke to reach.
    startup_keys: Vec<KeyEvent>,
    /// Ctrl+O's output screen, when it is open. While it is, the
    /// panels are neither drawn nor given any input: the shell has the
    /// window, exactly as it has the terminal in the other build.
    pane: Option<TerminalPane>,
    /// Set once the state file has been written, so the closing frames
    /// do not write it again.
    saved: bool,
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
            pane: None,
            saved: false,
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
            self.pane = TerminalPane::open(&mut self.app, cmd, cols, rows);
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
        let Some(pane) = self.pane.as_mut() else {
            return Duration::from_millis(500);
        };
        let (cols, rows) = self.size;
        pane.resize(&mut self.app, cols, rows);
        // Ctrl+O closes it; everything typed before that still reaches
        // the shell, the way the terminal build feeds the bytes ahead
        // of the 0x0F and then breaks
        let open = pane.feed(&mut self.app, &input) & pane.step(&mut self.app);
        pane.paint(ui.painter(), origin, self.metrics, &self.font, self.palette);
        let wait = pane.repaint_after();
        if !open {
            self.pane = None;
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
            eprintln!("rmut: could not save state: {err}");
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

        // The whole window is the grid: no margins, because a cell
        // grid that does not start at the top-left corner is a cell
        // grid with a wasted row and column.
        let available = ui.max_rect();
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

        let mut input = ctx.input(|i| keys::collect(i, origin, self.metrics));
        // $RMUT_KEYS, one per frame ahead of anything real. One per
        // frame rather than all at once because that is what typing is:
        // a key that opens a screen has to be given the frame in which
        // to open it before the next key arrives, or the next key goes
        // to the screen that was on its way out.
        if !self.startup_keys.is_empty() {
            let key = self.startup_keys.remove(0);
            input.insert(0, Input::Key(key));
            ctx.request_repaint();
        }

        // With the pane open the shell owns the window: the panels are
        // neither drawn nor given any input, exactly as they are not in
        // the terminal build while the output screen is up.
        if self.pane.is_some() {
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
                eprintln!("rmut: draw failed: {err}");
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

/// The window's starting size in points: 100x30 cells, which is a
/// comfortable two panels. Estimated rather than measured, the fonts
/// not existing until the window is up.
pub fn window_size(font_size: f32) -> Vec2 {
    let metrics = Metrics::estimate(font_size);
    Vec2::new(metrics.width * 100.0, metrics.height * 30.0)
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
