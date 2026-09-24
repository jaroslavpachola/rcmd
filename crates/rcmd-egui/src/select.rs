//! Selecting text on the screen with the mouse, and copying it.
//!
//! In a terminal this is the terminal's job: rcmd asks for the mouse,
//! and Shift is how a user tells the emulator to keep the drag for
//! itself. A window has no emulator behind it, so the window does it:
//! a drag in the shell pane (which takes no mouse of its own), or a
//! Shift+drag over the panels, selects cells the way a terminal does -
//! from where the button went down to where it is, row after row - and
//! letting go copies them to the clipboard.

use eframe::egui::{self, Color32, Pos2, Rect, Vec2};
use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use unicode_width::UnicodeWidthStr;

use crate::grid::Metrics;
use crate::keys::Input;

/// Cells from `anchor` (where the button went down) to `head` (where
/// the pointer is), both as (column, row) and both included.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Selection {
    anchor: (u16, u16),
    head: (u16, u16),
}

impl Selection {
    /// Both ends kept on a screen `cols` by `rows`: a drag carries on
    /// past the window's edge, and what is out there is nothing.
    pub fn clamp(self, cols: u16, rows: u16) -> Selection {
        let keep = |(col, row): (u16, u16)| {
            (
                col.min(cols.saturating_sub(1)),
                row.min(rows.saturating_sub(1)),
            )
        };
        Selection {
            anchor: keep(self.anchor),
            head: keep(self.head),
        }
    }

    /// The two ends in reading order.
    fn ends(&self) -> ((u16, u16), (u16, u16)) {
        let key = |(col, row): (u16, u16)| (row, col);
        if key(self.anchor) <= key(self.head) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The columns selected on `row` of a screen `cols` wide, as a
    /// half-open range; empty when the row is outside.
    fn span(&self, row: u16, cols: u16) -> std::ops::Range<u16> {
        let ((c0, r0), (c1, r1)) = self.ends();
        if row < r0 || row > r1 {
            return 0..0;
        }
        let from = if row == r0 { c0 } else { 0 };
        let to = if row == r1 {
            c1.saturating_add(1)
        } else {
            cols
        };
        from.min(cols)..to.min(cols)
    }

    /// The selected text. `symbol` says what a cell shows; a wide glyph
    /// covers the cell after it too, which is not read again. Each row
    /// loses its trailing blanks, as a terminal's copy does.
    pub fn text(&self, cols: u16, symbol: impl Fn(u16, u16) -> String) -> String {
        let ((_, r0), (_, r1)) = self.ends();
        let mut rows = Vec::new();
        for row in r0..=r1 {
            let mut line = String::new();
            let mut col = 0;
            // a wide glyph that starts left of the span still shows in
            // it: start from the row's start and skip, so the columns
            // line up with what was drawn
            let span = self.span(row, cols);
            while col < span.end {
                let text = symbol(col, row);
                let wide = text.width() > 1;
                if col >= span.start || (wide && col + 1 == span.start) {
                    line.push_str(if text.is_empty() { " " } else { &text });
                }
                col += if wide { 2 } else { 1 };
            }
            rows.push(line.trim_end().to_string());
        }
        rows.join("\n")
    }

    /// Paint the selection over what is already drawn.
    pub fn paint(&self, painter: &egui::Painter, origin: Pos2, metrics: Metrics, cols: u16) {
        let ((_, r0), (_, r1)) = self.ends();
        let tint = Color32::from_rgba_unmultiplied(120, 160, 255, 90);
        for row in r0..=r1 {
            let span = self.span(row, cols);
            if span.is_empty() {
                continue;
            }
            let min = Pos2::new(
                origin.x + span.start as f32 * metrics.width,
                origin.y + row as f32 * metrics.height,
            );
            let size = Vec2::new(span.len() as f32 * metrics.width, metrics.height);
            painter.rect_filled(Rect::from_min_size(min, size), 0.0, tint);
        }
    }
}

/// The drag in progress, and the selection left on screen after it.
#[derive(Default)]
pub struct Selecting {
    /// On screen: being dragged, or left after a copy until the next
    /// key or click.
    pub shown: Option<Selection>,
    /// The left button is down and this drag is the selection's.
    dragging: bool,
}

impl Selecting {
    /// Take out of a frame's input what belongs to a selection, and
    /// hand back the rest. `always`: every left drag selects (the shell
    /// pane); otherwise only one that started with Shift held. Returns
    /// a finished selection, ready to copy, when the button came up.
    pub fn filter(&mut self, input: Vec<Input>, always: bool) -> (Vec<Input>, Option<Selection>) {
        let mut rest = Vec::with_capacity(input.len());
        let mut done = None;
        for event in input {
            match event {
                Input::Mouse(MouseEvent {
                    kind,
                    column,
                    row,
                    modifiers,
                }) if self.dragging || starts(kind, modifiers, always) => {
                    let at = (column, row);
                    match kind {
                        MouseEventKind::Down(_) => {
                            self.dragging = true;
                            self.shown = Some(Selection {
                                anchor: at,
                                head: at,
                            });
                        }
                        MouseEventKind::Drag(_) => {
                            if let Some(sel) = self.shown.as_mut() {
                                sel.head = at;
                            }
                        }
                        MouseEventKind::Up(_) => {
                            self.dragging = false;
                            match self.shown {
                                // a click that never moved selects nothing
                                Some(sel) if sel.anchor == sel.head => self.shown = None,
                                Some(sel) => done = Some(sel),
                                None => {}
                            }
                        }
                        // the wheel and the rest go on as they came
                        _ => rest.push(event),
                    }
                }
                // anything else ends the showing of the last one
                Input::Key(_) | Input::Paste(_) | Input::Context { .. } => {
                    self.shown = None;
                    rest.push(event);
                }
                Input::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(_),
                    ..
                }) => {
                    self.shown = None;
                    rest.push(event);
                }
                _ => rest.push(event),
            }
        }
        (rest, done)
    }

    /// Forget it all: the screen under it changed hands.
    pub fn clear(&mut self) {
        *self = Selecting::default();
    }
}

/// Whether this event starts a selection.
fn starts(kind: MouseEventKind, modifiers: KeyModifiers, always: bool) -> bool {
    kind == MouseEventKind::Down(MouseButton::Left)
        && (always || modifiers.contains(KeyModifiers::SHIFT))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(kind: MouseEventKind, column: u16, row: u16, shift: bool) -> Input {
        Input::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: if shift {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            },
        })
    }

    const SCREEN: [&str; 3] = ["hello world   ", "second row    ", "third         "];

    fn symbol(col: u16, row: u16) -> String {
        let line = SCREEN[row as usize];
        line[col as usize..col as usize + 1].to_string()
    }

    #[test]
    fn one_row_is_the_cells_between_the_ends() {
        let sel = Selection {
            anchor: (6, 0),
            head: (10, 0),
        };
        assert_eq!(sel.text(14, symbol), "world");
    }

    #[test]
    fn rows_run_on_as_a_terminal_selects_them() {
        // dragged backwards: the ends are put in reading order
        let sel = Selection {
            anchor: (2, 2),
            head: (6, 0),
        };
        assert_eq!(sel.text(14, symbol), "world\nsecond row\nthi");
    }

    #[test]
    fn a_wide_glyph_is_read_once() {
        let cells = ["界", "", "a", " "];
        let sel = Selection {
            anchor: (0, 0),
            head: (3, 0),
        };
        assert_eq!(sel.text(4, |col, _| cells[col as usize].to_string()), "界a");
    }

    #[test]
    fn a_drag_past_the_edge_stops_at_it() {
        let sel = Selection {
            anchor: (6, 0),
            head: (500, 90),
        }
        .clamp(14, 3);
        assert_eq!(sel.text(14, symbol), "world\nsecond row\nthird");
    }

    #[test]
    fn a_plain_drag_selects_only_where_it_always_does() {
        let drag = vec![
            mouse(MouseEventKind::Down(MouseButton::Left), 1, 0, false),
            mouse(MouseEventKind::Drag(MouseButton::Left), 4, 0, false),
            mouse(MouseEventKind::Up(MouseButton::Left), 4, 0, false),
        ];
        let mut panels = Selecting::default();
        let (rest, done) = panels.filter(drag.clone(), false);
        assert_eq!(rest.len(), 3);
        assert!(done.is_none());

        let mut pane = Selecting::default();
        let (rest, done) = pane.filter(drag, true);
        assert!(rest.is_empty());
        assert_eq!(done.unwrap().text(14, symbol), "ello");
    }

    #[test]
    fn shift_selects_over_the_panels_and_the_panels_see_none_of_it() {
        let mut panels = Selecting::default();
        let (rest, done) = panels.filter(
            vec![
                mouse(MouseEventKind::Down(MouseButton::Left), 0, 1, true),
                // Shift let go half way: the drag is still the selection's
                mouse(MouseEventKind::Drag(MouseButton::Left), 5, 1, false),
                mouse(MouseEventKind::Up(MouseButton::Left), 5, 1, false),
            ],
            false,
        );
        assert!(rest.is_empty());
        assert_eq!(done.unwrap().text(14, symbol), "second");
        assert!(panels.shown.is_some());
    }

    #[test]
    fn a_click_selects_nothing_and_a_key_clears_what_was_shown() {
        let mut pane = Selecting::default();
        let (_, done) = pane.filter(
            vec![
                mouse(MouseEventKind::Down(MouseButton::Left), 3, 0, false),
                mouse(MouseEventKind::Up(MouseButton::Left), 3, 0, false),
            ],
            true,
        );
        assert!(done.is_none() && pane.shown.is_none());
        pane.shown = Some(Selection {
            anchor: (0, 0),
            head: (2, 0),
        });
        let key = Input::Key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('x'),
            KeyModifiers::NONE,
        ));
        let (rest, _) = pane.filter(vec![key], true);
        assert_eq!(rest.len(), 1);
        assert!(pane.shown.is_none());
    }
}
