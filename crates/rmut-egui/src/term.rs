//! The embedded terminal pane: Ctrl+O's output screen, in the window.
//!
//! The terminal build's answer to Ctrl+O is to leave the alternate
//! screen and hand the real tty to the shell, which then draws itself.
//! A window has no tty to hand over, so it has to *be* the terminal:
//! read what the shell writes, interpret it, and draw the screen it
//! describes.
//!
//! Half of that already existed. `subshell.rs` owns the pty, spawns the
//! shell, tracks its working directory through the prompt hooks, and
//! buffers every byte it writes - `pump` collects and `take_output`
//! hands over, whether anything is watching or not. What was missing is
//! only the interpreting half, which is [`vt100`]: bytes in, a grid of
//! cells with colours and attributes out. Painting that grid is
//! [`crate::grid`]'s job already.
//!
//! The shell protocol - wait for a prompt, sync the directory in, feed
//! the command, know when it finished, sync back out - is not here
//! either. It is `App::begin_subshell` / `step_subshell` /
//! `end_subshell`, shared with the terminal build, which loops over the
//! same three calls while this one makes one pass per frame.
//!
//! The pane outlives its sessions. In the terminal build the shell's
//! screen is the terminal's primary screen, which is still there with
//! `ls`'s output on it when the next Ctrl+O leaves the alternate one;
//! here the parser is that primary screen, so it is created once and
//! kept, and a session only opens and closes on top of it. A parser
//! made fresh for every session would open on a black screen, the
//! command's output having gone with the previous one.
//!
//! Nothing behind the parser answers a terminal query, and fish asks
//! DA1 before every prompt and waits for the reply; `App` is told the
//! subshell is headless so that `subshell.rs`'s shim keeps answering
//! while a session is on screen, not only while the shell is hidden.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke, Vec2};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rcmd_tui::app::{App, Exec, SubshellSession, SubshellStep};

use crate::grid::{Metrics, Palette};
use crate::keys::Input;

/// Ctrl+O, as a byte. It never reaches the shell - MC-compatible, and
/// yes, that shadows nano's save inside the subshell.
const CTRL_O: u8 = 0x0F;

pub struct TerminalPane {
    /// The shell's screen, kept across sessions.
    parser: vt100::Parser,
    /// The session on screen, when one is.
    session: Option<SubshellSession>,
    size: (u16, u16),
    /// The shell was fed a command rather than merely shown, so the
    /// pane closes itself when that command finishes.
    fed_command: bool,
    /// The last pass produced output. A shell that is printing wants
    /// the next frame promptly; one sitting at a prompt does not, and
    /// waking thirty times a second to redraw an unchanged prompt is a
    /// laptop battery for nothing.
    moving: bool,
}

impl TerminalPane {
    /// A pane with nothing on it yet, sized to the window.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            session: None,
            size: (cols, rows),
            fed_command: false,
            moving: false,
        }
    }

    /// Open a session for what a key press queued. `false` means there
    /// was none to open (no subshell, or a shell already busy - which
    /// [`App::begin_subshell`] puts on the status line itself).
    pub fn open(&mut self, app: &mut App, exec: Exec, cols: u16, rows: u16) -> bool {
        let fed_command = matches!(exec, Exec::Command(_));
        let Some(session) = app.begin_subshell(exec) else {
            return false;
        };
        self.resize(app, cols, rows);
        // Whatever the shell wrote while nothing was watching. The
        // terminal build replays these onto the output screen at the
        // same point; here they land on the kept screen, so a Ctrl+O
        // finds the prompt - and whatever came before it - already on it.
        self.parser.process(&app.take_subshell_output());
        self.session = Some(session);
        self.fed_command = fed_command;
        self.moving = true;
        true
    }

    pub fn is_open(&self) -> bool {
        self.session.is_some()
    }

    /// The session is over: the screen stays, the session goes. The
    /// caller then calls [`App::end_subshell`].
    pub fn close(&mut self) {
        self.session = None;
    }

    /// One pass. `false` means the session is over and the caller
    /// should [`Self::close`] it and call [`App::end_subshell`].
    pub fn step(&mut self, app: &mut App) -> bool {
        self.moving = false;
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        match app.step_subshell(session) {
            SubshellStep::Output(bytes) => {
                self.parser.process(&bytes);
                self.moving = true;
                true
            }
            SubshellStep::Waiting => true,
            // a fed command that has finished closes the pane, the way
            // the terminal build drops back to the panels; a plain
            // Ctrl+O session ends only when Ctrl+O ends it, so a shell
            // that died is the only other way out
            SubshellStep::Done => !self.fed_command && app.subshell_alive(),
        }
    }

    /// How long to wait before the next frame: a shell that is printing
    /// gets one straight away, an idle prompt gets the same lazy rate
    /// the panels get.
    pub fn repaint_after(&self) -> std::time::Duration {
        std::time::Duration::from_millis(if self.moving { 16 } else { 100 })
    }

    /// Follow the window. The subshell is told too, whether or not a
    /// session is open: `App` skips it when nothing changed.
    pub fn resize(&mut self, app: &mut App, cols: u16, rows: u16) {
        if (cols, rows) == self.size {
            return;
        }
        self.size = (cols, rows);
        self.parser.set_size(rows, cols);
        app.resize_subshell(cols, rows);
    }

    /// Hand a frame's input to the shell. Returns `false` when Ctrl+O
    /// asked to close the pane, in which case everything typed before
    /// it has still been passed on.
    pub fn feed(&mut self, app: &mut App, input: &[Input]) -> bool {
        let mut bytes = Vec::new();
        for event in input {
            let Input::Key(key) = event else { continue };
            encode(key, self.parser.screen().application_cursor(), &mut bytes);
        }
        match bytes.iter().position(|&b| b == CTRL_O) {
            Some(at) => {
                app.feed_subshell(&bytes[..at]);
                false
            }
            None => {
                app.feed_subshell(&bytes);
                true
            }
        }
    }

    /// Paint the shell's screen. Same grid, same font, same origin as
    /// the panels it is standing in front of.
    pub fn paint(
        &self,
        painter: &egui::Painter,
        origin: Pos2,
        metrics: Metrics,
        font: &FontId,
        palette: Palette,
    ) {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let cell_rect = |col: u16, row: u16| {
            Rect::from_min_size(
                Pos2::new(
                    origin.x + col as f32 * metrics.width,
                    origin.y + row as f32 * metrics.height,
                ),
                Vec2::new(metrics.width, metrics.height),
            )
        };

        for row in 0..rows {
            // backgrounds batched per run, as in `grid.rs` and for the
            // same reason: most of a shell's screen is one colour
            let mut col = 0;
            while col < cols {
                let bg = self.colors(screen.cell(row, col)).1;
                let mut end = col + 1;
                while end < cols && self.colors(screen.cell(row, end)).1 == bg {
                    end += 1;
                }
                if bg != palette.bg {
                    painter.rect_filled(
                        cell_rect(col, row).union(cell_rect(end - 1, row)),
                        0.0,
                        bg,
                    );
                }
                col = end;
            }
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                // the right-hand half of a wide glyph: the glyph itself
                // was drawn at the left half and runs into this one
                if cell.is_wide_continuation() {
                    continue;
                }
                let contents = cell.contents();
                if contents.trim().is_empty() && !cell.underline() {
                    continue;
                }
                let (fg, _) = self.colors(Some(cell));
                let rect = cell_rect(col, row);
                if !contents.trim().is_empty() {
                    painter.text(
                        rect.left_top(),
                        Align2::LEFT_TOP,
                        &contents,
                        font.clone(),
                        fg,
                    );
                    if cell.bold() {
                        painter.text(
                            rect.left_top() + Vec2::new(0.6, 0.0),
                            Align2::LEFT_TOP,
                            &contents,
                            font.clone(),
                            fg,
                        );
                    }
                }
                if cell.underline() {
                    painter.hline(rect.x_range(), rect.bottom() - 1.0, Stroke::new(1.0, fg));
                }
            }
        }

        if !screen.hide_cursor() {
            let (row, col) = screen.cursor_position();
            if row < rows && col < cols {
                let rect = cell_rect(col, row);
                let (fg, bg) = self.colors(screen.cell(row, col));
                painter.rect_filled(rect, 0.0, fg);
                if let Some(cell) = screen.cell(row, col)
                    && !cell.contents().trim().is_empty()
                {
                    painter.text(
                        rect.left_top(),
                        Align2::LEFT_TOP,
                        cell.contents(),
                        font.clone(),
                        bg,
                    );
                }
            }
        }
    }

    /// What a cell paints as, once `inverse` and the screen's own
    /// default colours have had their say.
    ///
    /// Only the cell's own attributes count. `Screen::inverse` and its
    /// siblings are the pen state for text still to be drawn, not a
    /// mode over the screen: folding those in here would flip every
    /// cell the moment a program left the pen inverted.
    fn colors(&self, cell: Option<&vt100::Cell>) -> (Color32, Color32) {
        let default = default_palette();
        let (mut fg, mut bg) = match cell {
            Some(cell) => (
                to_color32(cell.fgcolor(), default.fg),
                to_color32(cell.bgcolor(), default.bg),
            ),
            None => (default.fg, default.bg),
        };
        if cell.is_some_and(vt100::Cell::inverse) {
            std::mem::swap(&mut fg, &mut bg);
        }
        (fg, bg)
    }
}

/// The shell's screen has its own default colours, independent of
/// rcmd's theme: a terminal's default is the terminal's, not the file
/// manager's, and a shell prompt written for a dark terminal should get
/// one.
fn default_palette() -> Palette {
    Palette::default()
}

/// A vt100 colour as a paintable one. The named eight and their bright
/// halves go through the same xterm table `grid.rs` uses, so a colour
/// means the same thing in the panels and in the shell.
fn to_color32(color: vt100::Color, default: Color32) -> Color32 {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => crate::grid::indexed_color(i),
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

/// A key press as the bytes a terminal would have sent for it.
///
/// The inverse of `keys.rs`: that turns a window's input into what
/// `app.rs` expects from a terminal, this turns it back into what a
/// shell expects from one. `application_cursor` is DECCKM - a program
/// that turned it on (vim, less, anything using ncurses) wants `SS3 A`
/// for Up rather than `CSI A`, and gets a stray character otherwise.
fn encode(key: &KeyEvent, application_cursor: bool, out: &mut Vec<u8>) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    // Meta is ESC-prefixed, which is what every terminal does and what
    // readline's `Alt+f` and friends are waiting for
    if alt {
        out.push(0x1b);
    }
    let arrow = |c: u8, out: &mut Vec<u8>| {
        out.extend_from_slice(if application_cursor {
            b"\x1bO"
        } else {
            b"\x1b["
        });
        out.push(c);
    };
    match key.code {
        KeyCode::Char(c) => match ctrl {
            // Ctrl+letter is the letter with the top three bits
            // cleared; Ctrl+Space is NUL, as every terminal sends it
            true => match c {
                ' ' => out.push(0),
                '?' => out.push(0x7f),
                c if c.is_ascii() => out.push((c as u8) & 0x1f),
                c => push_char(c, out),
            },
            false => push_char(c, out),
        },
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        // DEL rather than BS: that is what every terminal emulator
        // sends now, and what the shells' line editors expect
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => arrow(b'A', out),
        KeyCode::Down => arrow(b'B', out),
        KeyCode::Right => arrow(b'C', out),
        KeyCode::Left => arrow(b'D', out),
        KeyCode::Home => arrow(b'H', out),
        KeyCode::End => arrow(b'F', out),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        // the F-keys, in the encoding xterm settled on and every
        // terminfo since has agreed with
        KeyCode::F(n) => match n {
            1..=4 => {
                out.extend_from_slice(b"\x1bO");
                out.push(b'P' + (n - 1));
            }
            5..=12 => {
                const TAIL: [&[u8]; 8] = [
                    b"15~", b"17~", b"18~", b"19~", b"20~", b"21~", b"23~", b"24~",
                ];
                out.extend_from_slice(b"\x1b[");
                out.extend_from_slice(TAIL[(n - 5) as usize]);
            }
            _ => {}
        },
        _ => {}
    }
}

fn push_char(c: char, out: &mut Vec<u8>) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(code: KeyCode, modifiers: KeyModifiers, application_cursor: bool) -> Vec<u8> {
        let mut out = Vec::new();
        encode(
            &KeyEvent::new(code, modifiers),
            application_cursor,
            &mut out,
        );
        out
    }

    #[test]
    fn a_letter_is_itself_and_ctrl_strips_it_down() {
        assert_eq!(bytes(KeyCode::Char('a'), KeyModifiers::NONE, false), b"a");
        // Ctrl+C has to be 0x03 or nothing can be interrupted
        assert_eq!(bytes(KeyCode::Char('c'), KeyModifiers::CONTROL, false), [3]);
        assert_eq!(bytes(KeyCode::Char('d'), KeyModifiers::CONTROL, false), [4]);
        // Ctrl+O is 0x0F, which is what the pane watches for
        assert_eq!(
            bytes(KeyCode::Char('o'), KeyModifiers::CONTROL, false),
            [CTRL_O]
        );
    }

    #[test]
    fn alt_is_an_escape_prefix() {
        // readline's Alt+f (forward-word) is ESC f
        assert_eq!(
            bytes(KeyCode::Char('f'), KeyModifiers::ALT, false),
            b"\x1bf"
        );
    }

    #[test]
    fn the_arrows_follow_the_cursor_mode() {
        // a plain shell gets CSI, a program that turned DECCKM on gets
        // SS3 - the difference between history and a stray "A"
        assert_eq!(bytes(KeyCode::Up, KeyModifiers::NONE, false), b"\x1b[A");
        assert_eq!(bytes(KeyCode::Up, KeyModifiers::NONE, true), b"\x1bOA");
        assert_eq!(bytes(KeyCode::Home, KeyModifiers::NONE, false), b"\x1b[H");
    }

    #[test]
    fn the_editing_keys_are_the_ones_terminfo_expects() {
        assert_eq!(bytes(KeyCode::Enter, KeyModifiers::NONE, false), b"\r");
        assert_eq!(bytes(KeyCode::Backspace, KeyModifiers::NONE, false), [0x7f]);
        assert_eq!(
            bytes(KeyCode::Delete, KeyModifiers::NONE, false),
            b"\x1b[3~"
        );
        assert_eq!(bytes(KeyCode::F(1), KeyModifiers::NONE, false), b"\x1bOP");
        assert_eq!(bytes(KeyCode::F(5), KeyModifiers::NONE, false), b"\x1b[15~");
        assert_eq!(
            bytes(KeyCode::F(12), KeyModifiers::NONE, false),
            b"\x1b[24~"
        );
    }

    #[test]
    fn a_shell_screen_comes_out_of_the_parser() {
        // the contract the pane rests on: bytes in, cells out
        let mut parser = vt100::Parser::new(3, 20, 0);
        parser.process(b"\x1b[31mred\x1b[0m plain");
        let screen = parser.screen();
        assert_eq!(screen.cell(0, 0).unwrap().contents(), "r");
        assert_eq!(screen.cell(0, 0).unwrap().fgcolor(), vt100::Color::Idx(1));
        assert_eq!(screen.cell(0, 4).unwrap().fgcolor(), vt100::Color::Default);
        // and the mode that decides how the arrows are encoded
        assert!(!screen.application_cursor());
        parser.process(b"\x1b[?1h");
        assert!(parser.screen().application_cursor());
    }
}
