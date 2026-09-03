//! A ratatui [`Backend`] that paints into an egui window.
//!
//! The whole point of this crate is that `rcmd_tui::ui::draw` is not
//! rewritten: it still builds ratatui widgets into a ratatui `Buffer`,
//! and this backend is what that buffer lands in. Instead of escape
//! sequences down a pipe, the cells are kept in a `Vec` and painted as
//! a monospace grid - one background rectangle per run of equal
//! background, one glyph per non-blank cell.
//!
//! Per-glyph painting rather than per-run text is deliberate. A run of
//! text laid out by egui advances by whatever the font's metrics say,
//! and a fraction of a pixel of drift per character is invisible for
//! one word and fatal across an eighty-column table drawn under a
//! box-drawing frame. Placing every glyph at its own cell origin makes
//! the grid exact, and egui's galley cache makes the repeated
//! single-character layouts cheap.

use std::io;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke, Vec2};
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};

/// How big one character cell is, in points.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub width: f32,
    pub height: f32,
}

impl Metrics {
    /// A guess at the metrics, for before egui has run a frame: fonts
    /// do not exist until `Context::run` has been called once, and the
    /// window needs a size to open at before that.
    pub fn estimate(size: f32) -> Self {
        Self {
            width: (size * 0.6).max(1.0),
            height: (size * 1.35).max(1.0),
        }
    }

    /// Measure the monospace font at `size`. `M` is the representative
    /// glyph: in a monospace face every advance is the same, and asking
    /// for a space can hit a font that treats it specially. Only valid
    /// from inside a frame - see [`Self::estimate`] for before that.
    pub fn measure(ctx: &egui::Context, font: &FontId) -> Self {
        let (width, height) = ctx.fonts_mut(|f| (f.glyph_width(font, 'M'), f.row_height(font)));
        Self {
            // a zero would divide by zero downstream; before the fonts
            // are loaded on the first frame the metrics can be absent
            width: width.max(1.0),
            height: height.max(1.0),
        }
    }

    /// How many whole cells fit in `size`.
    pub fn cells(&self, size: Vec2) -> (u16, u16) {
        let cols = (size.x / self.width).floor().max(1.0);
        let rows = (size.y / self.height).floor().max(1.0);
        // A ceiling, because painting is per cell and the buffers are
        // allocated per cell: `--font-size 4` on a 4K screen is around
        // 1600 columns, which should still work, and nothing sane goes
        // past this. The cast wants the clamp anyway - `size` is a
        // float from the window manager and a NaN would wrap.
        (cols.min(4000.0) as u16, rows.min(2000.0) as u16)
    }
}

/// The default colours a `Color::Reset` cell resolves to - the terminal
/// equivalent of "whatever the user's terminal is set to". rcmd's own
/// themes paint most of the screen themselves; this is what shows
/// through where they don't.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub fg: Color32,
    pub bg: Color32,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            fg: Color32::from_rgb(0xcc, 0xcc, 0xcc),
            bg: Color32::from_rgb(0x0c, 0x0c, 0x0c),
        }
    }
}

pub struct EguiBackend {
    cells: Vec<Cell>,
    width: u16,
    height: u16,
    cursor: Position,
    cursor_visible: bool,
    palette: Palette,
}

impl EguiBackend {
    pub fn new(width: u16, height: u16, palette: Palette) -> Self {
        let mut backend = Self {
            cells: Vec::new(),
            width: 0,
            height: 0,
            cursor: Position::ORIGIN,
            cursor_visible: false,
            palette,
        };
        backend.set_size(width, height);
        backend
    }

    /// Resize the grid. A no-op when nothing changed, so the GUI can
    /// call it every frame; ratatui's own `autoresize` notices the new
    /// [`Backend::size`] on the next draw and reshapes its buffers.
    pub fn set_size(&mut self, width: u16, height: u16) {
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        self.cells = vec![Cell::EMPTY; width as usize * height as usize];
        self.cursor = Position::ORIGIN;
    }

    pub fn cursor(&self) -> Option<Position> {
        self.cursor_visible.then_some(self.cursor)
    }

    fn index(&self, x: u16, y: u16) -> Option<usize> {
        (x < self.width && y < self.height).then(|| y as usize * self.width as usize + x as usize)
    }

    /// What one cell actually renders as, once `Reset`, `REVERSED`,
    /// `DIM` and `HIDDEN` have had their say.
    fn resolve(&self, cell: &Cell) -> (Color32, Color32) {
        let mut fg = to_color32(cell.fg, self.palette.fg);
        let mut bg = to_color32(cell.bg, self.palette.bg);
        if cell.modifier.contains(Modifier::REVERSED) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.modifier.contains(Modifier::DIM) {
            fg = blend(fg, bg, 0.5);
        }
        if cell.modifier.contains(Modifier::HIDDEN) {
            fg = bg;
        }
        (fg, bg)
    }

    /// Paint the grid with `origin` as the top-left of cell (0, 0).
    pub fn paint(&self, painter: &egui::Painter, origin: Pos2, metrics: Metrics, font: &FontId) {
        let cell_rect = |x: u16, y: u16| {
            let min = Pos2::new(
                origin.x + x as f32 * metrics.width,
                origin.y + y as f32 * metrics.height,
            );
            Rect::from_min_size(min, Vec2::new(metrics.width, metrics.height))
        };

        // Backgrounds first, batched: a run of cells sharing a colour
        // is one rectangle, which is most of a screen that is mostly
        // panel and dialog fill. The window itself is already painted
        // in the default background, so runs of that are skipped.
        for y in 0..self.height {
            let mut x = 0;
            while x < self.width {
                let Some(i) = self.index(x, y) else { break };
                let (_, bg) = self.resolve(&self.cells[i]);
                let mut end = x + 1;
                while end < self.width {
                    let Some(j) = self.index(end, y) else { break };
                    if self.resolve(&self.cells[j]).1 != bg {
                        break;
                    }
                    end += 1;
                }
                if bg != self.palette.bg {
                    let rect = cell_rect(x, y).union(cell_rect(end - 1, y));
                    painter.rect_filled(rect, 0.0, bg);
                }
                x = end;
            }
        }

        // ...then one glyph per cell that has one. A wide glyph (CJK)
        // is drawn at its own origin and allowed to run into the cell
        // to its right, which ratatui has left blank for exactly that.
        for y in 0..self.height {
            for x in 0..self.width {
                let Some(i) = self.index(x, y) else { continue };
                let cell = &self.cells[i];
                let symbol = cell.symbol();
                if symbol.is_empty() || symbol == " " {
                    // an underline still has to be drawn under a blank
                    if !cell.modifier.contains(Modifier::UNDERLINED) {
                        continue;
                    }
                }
                let (fg, _) = self.resolve(cell);
                let rect = cell_rect(x, y);
                if !symbol.trim().is_empty() {
                    painter.text(rect.left_top(), Align2::LEFT_TOP, symbol, font.clone(), fg);
                    if cell.modifier.contains(Modifier::BOLD) {
                        // the bundled monospace face has one weight, so
                        // bold is the old trick of drawing it again a
                        // hair to the right
                        painter.text(
                            rect.left_top() + Vec2::new(0.6, 0.0),
                            Align2::LEFT_TOP,
                            symbol,
                            font.clone(),
                            fg,
                        );
                    }
                }
                if cell.modifier.contains(Modifier::UNDERLINED) {
                    let y = rect.bottom() - 1.0;
                    painter.hline(rect.x_range(), y, Stroke::new(1.0, fg));
                }
                if cell.modifier.contains(Modifier::CROSSED_OUT) {
                    let y = rect.center().y;
                    painter.hline(rect.x_range(), y, Stroke::new(1.0, fg));
                }
            }
        }

        // The cursor, where a widget asked for one: a filled block with
        // the cell drawn back over it in the background colour, which
        // is what a terminal block cursor looks like.
        if let Some(pos) = self.cursor()
            && let Some(i) = self.index(pos.x, pos.y)
        {
            let cell = &self.cells[i];
            let (fg, bg) = self.resolve(cell);
            let rect = cell_rect(pos.x, pos.y);
            painter.rect_filled(rect, 0.0, fg);
            let symbol = cell.symbol();
            if !symbol.trim().is_empty() {
                painter.text(rect.left_top(), Align2::LEFT_TOP, symbol, font.clone(), bg);
            }
        }
    }
}

impl Backend for EguiBackend {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, cell) in content {
            if let Some(i) = self.index(x, y) {
                self.cells[i] = cell.clone();
            }
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.cursor_visible = false;
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.cursor_visible = true;
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.cursor = position.into();
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.cells.fill(Cell::EMPTY);
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        let Position { x, y } = self.cursor;
        let width = self.width as usize;
        let cursor = y as usize * width + x as usize;
        let range = match clear_type {
            ClearType::All => 0..self.cells.len(),
            ClearType::AfterCursor => (cursor + 1).min(self.cells.len())..self.cells.len(),
            ClearType::BeforeCursor => 0..cursor.min(self.cells.len()),
            ClearType::CurrentLine => {
                let start = y as usize * width;
                start..(start + width).min(self.cells.len())
            }
            ClearType::UntilNewLine => {
                let end = (y as usize + 1) * width;
                cursor.min(self.cells.len())..end.min(self.cells.len())
            }
        };
        self.cells[range].fill(Cell::EMPTY);
        Ok(())
    }

    fn size(&self) -> io::Result<Size> {
        Ok(Size::new(self.width, self.height))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: Size::new(self.width, self.height),
            // the pixel size is only ever asked for by image protocols
            // no window here speaks, so the honest answer is "unknown",
            // which is what a terminal that does not know reports too
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The xterm 16-colour palette, which is what a ratatui named colour
/// means. Values are the widely-copied "xterm" set rather than any one
/// terminal's, so a theme written against mc looks like mc.
const ANSI: [Color32; 16] = [
    Color32::from_rgb(0x00, 0x00, 0x00), // black
    Color32::from_rgb(0xcd, 0x00, 0x00), // red
    Color32::from_rgb(0x00, 0xcd, 0x00), // green
    Color32::from_rgb(0xcd, 0xcd, 0x00), // yellow
    Color32::from_rgb(0x00, 0x00, 0xee), // blue
    Color32::from_rgb(0xcd, 0x00, 0xcd), // magenta
    Color32::from_rgb(0x00, 0xcd, 0xcd), // cyan
    Color32::from_rgb(0xe5, 0xe5, 0xe5), // white (ratatui's Gray)
    Color32::from_rgb(0x7f, 0x7f, 0x7f), // bright black (DarkGray)
    Color32::from_rgb(0xff, 0x00, 0x00), // bright red
    Color32::from_rgb(0x00, 0xff, 0x00), // bright green
    Color32::from_rgb(0xff, 0xff, 0x00), // bright yellow
    Color32::from_rgb(0x5c, 0x5c, 0xff), // bright blue
    Color32::from_rgb(0xff, 0x00, 0xff), // bright magenta
    Color32::from_rgb(0x00, 0xff, 0xff), // bright cyan
    Color32::from_rgb(0xff, 0xff, 0xff), // bright white (White)
];

/// A ratatui colour as a paintable one. `Reset` becomes `default`,
/// which is the caller's foreground or background as appropriate.
pub fn to_color32(color: Color, default: Color32) -> Color32 {
    match color {
        Color::Reset => default,
        Color::Black => ANSI[0],
        Color::Red => ANSI[1],
        Color::Green => ANSI[2],
        Color::Yellow => ANSI[3],
        Color::Blue => ANSI[4],
        Color::Magenta => ANSI[5],
        Color::Cyan => ANSI[6],
        Color::Gray => ANSI[7],
        Color::DarkGray => ANSI[8],
        Color::LightRed => ANSI[9],
        Color::LightGreen => ANSI[10],
        Color::LightYellow => ANSI[11],
        Color::LightBlue => ANSI[12],
        Color::LightMagenta => ANSI[13],
        Color::LightCyan => ANSI[14],
        Color::White => ANSI[15],
        Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        Color::Indexed(i) => indexed_color(i),
    }
}

/// The 256-colour cube: 0-15 are the named ones, 16-231 a 6x6x6 cube,
/// 232-255 a 24-step grey ramp. Shared with the terminal pane, so a
/// colour means the same thing in the panels and in the shell.
pub fn indexed_color(i: u8) -> Color32 {
    match i {
        0..=15 => ANSI[i as usize],
        16..=231 => {
            const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let i = i - 16;
            Color32::from_rgb(
                STEPS[(i / 36) as usize],
                STEPS[(i / 6 % 6) as usize],
                STEPS[(i % 6) as usize],
            )
        }
        232..=255 => {
            let level = 8 + (i - 232) * 10;
            Color32::from_rgb(level, level, level)
        }
    }
}

fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grid_reshapes_and_forgets() {
        let mut backend = EguiBackend::new(4, 2, Palette::default());
        assert_eq!(backend.size().unwrap(), Size::new(4, 2));
        let mut cell = Cell::default();
        cell.set_char('x');
        backend.draw([(1u16, 1u16, &cell)].into_iter()).unwrap();
        assert_eq!(backend.cells[5].symbol(), "x");
        // out of bounds is dropped rather than panicking: a resize can
        // land between ratatui's size query and its draw
        backend.draw([(99u16, 99u16, &cell)].into_iter()).unwrap();
        backend.set_size(3, 3);
        assert_eq!(backend.size().unwrap(), Size::new(3, 3));
        assert!(backend.cells.iter().all(|c| c.symbol() == " "));
    }

    #[test]
    fn reverse_swaps_and_reset_falls_back() {
        let palette = Palette {
            fg: Color32::WHITE,
            bg: Color32::BLACK,
        };
        let backend = EguiBackend::new(1, 1, palette);
        let mut cell = Cell::default();
        assert_eq!(backend.resolve(&cell), (Color32::WHITE, Color32::BLACK));
        cell.modifier |= Modifier::REVERSED;
        assert_eq!(backend.resolve(&cell), (Color32::BLACK, Color32::WHITE));
    }

    #[test]
    fn the_256_colour_cube_lands_where_xterm_puts_it() {
        assert_eq!(indexed_color(0), ANSI[0]);
        assert_eq!(indexed_color(16), Color32::from_rgb(0, 0, 0));
        assert_eq!(indexed_color(231), Color32::from_rgb(255, 255, 255));
        assert_eq!(indexed_color(196), Color32::from_rgb(255, 0, 0));
        assert_eq!(indexed_color(232), Color32::from_rgb(8, 8, 8));
        assert_eq!(indexed_color(255), Color32::from_rgb(238, 238, 238));
    }

    #[test]
    fn a_real_widget_lands_in_the_grid() {
        // The contract this crate rests on: ratatui's own widgets,
        // through ratatui's own Terminal, into these cells. A window is
        // not needed to check it, and a break here is the one that
        // would otherwise only show up as a wrong-looking screenshot.
        use ratatui::Terminal;
        use ratatui::style::{Style, Stylize};
        use ratatui::widgets::{Block, Borders, Paragraph};

        let mut terminal = Terminal::new(EguiBackend::new(10, 3, Palette::default())).unwrap();
        terminal
            .draw(|frame| {
                let block = Block::default().borders(Borders::ALL);
                frame.render_widget(
                    Paragraph::new("hi").style(Style::new().bold()).block(block),
                    frame.area(),
                );
            })
            .unwrap();
        let backend = terminal.backend();

        // the frame's corners, which is the box-drawing the bundled
        // egui font cannot draw and `font.rs` goes looking for
        assert_eq!(backend.cells[0].symbol(), "\u{250c}");
        assert_eq!(backend.cells[9].symbol(), "\u{2510}");
        // ...and the text inside it, with its modifier intact
        assert_eq!(backend.cells[11].symbol(), "h");
        assert_eq!(backend.cells[12].symbol(), "i");
        assert!(backend.cells[11].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn cells_divide_the_window_downwards() {
        let metrics = Metrics {
            width: 10.0,
            height: 20.0,
        };
        assert_eq!(metrics.cells(Vec2::new(105.0, 45.0)), (10, 2));
        // never zero: a buffer with no cells in it is not a thing
        assert_eq!(metrics.cells(Vec2::new(0.0, 0.0)), (1, 1));
    }
}
