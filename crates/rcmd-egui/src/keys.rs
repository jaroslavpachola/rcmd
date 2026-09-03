//! egui input, spelled as the crossterm events `rcmd_tui::app` already
//! understands.
//!
//! `App::on_key` takes a `crossterm::event::KeyEvent`, and that type is
//! a plain data enum - no terminal is needed to build one. So the whole
//! of `app.rs`'s dispatch, `keymap.rs`, and every user keybinding in
//! `config.toml` are reused exactly as they are, and this file is the
//! only place that knows a window is involved.
//!
//! Two conventions are inherited from what a terminal actually sends,
//! because `keymap.rs` was written against them:
//!
//! * A shifted letter arrives as the capital with no SHIFT bit. egui's
//!   `Event::Text` has already applied the keyboard layout, so text is
//!   where printable characters come from, and the `Key` events for
//!   those are dropped to avoid delivering each keystroke twice.
//! * Ctrl and Alt combinations never produce text, so those do come
//!   from the `Key` events, lowercased the way crossterm reports them.

use eframe::egui;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::grid::Metrics;

/// What one frame of egui input turned into, in arrival order.
pub enum Input {
    Key(KeyEvent),
    Mouse(MouseEvent),
}

/// Drain a frame's input. `origin` is where cell (0, 0) is painted, so
/// pointer positions can be turned into columns and rows.
pub fn collect(input: &egui::InputState, origin: egui::Pos2, metrics: Metrics) -> Vec<Input> {
    let mut out = Vec::new();
    let modifiers = to_modifiers(&input.modifiers);
    let at = |pos: egui::Pos2| -> (u16, u16) {
        let col = ((pos.x - origin.x) / metrics.width).floor().max(0.0);
        let row = ((pos.y - origin.y) / metrics.height).floor().max(0.0);
        (
            col.min(u16::MAX as f32) as u16,
            row.min(u16::MAX as f32) as u16,
        )
    };

    for event in &input.events {
        match event {
            // Printable input, layout already applied. Alt+letter is
            // excluded: some platforms send text for it as well, and
            // the Key event below is the one that carries the ALT bit.
            egui::Event::Text(text) if !input.modifiers.alt && !input.modifiers.command => {
                for c in text.chars() {
                    // a newline here would be Enter arriving twice
                    if matches!(c, '\n' | '\r' | '\t' | '\u{7f}') {
                        continue;
                    }
                    out.push(Input::Key(KeyEvent::new(
                        KeyCode::Char(c),
                        modifiers - KeyModifiers::SHIFT,
                    )));
                }
            }
            egui::Event::Key {
                key,
                pressed: true,
                modifiers: mods,
                ..
            } => {
                let mods_ct = to_modifiers(mods);
                let Some(code) = to_code(*key) else {
                    continue;
                };
                // a bare printable key was already delivered as text
                if matches!(code, KeyCode::Char(_)) && !mods.ctrl && !mods.alt && !mods.command {
                    continue;
                }
                // ...and a shifted letter is the capital with no SHIFT,
                // which is what a terminal sends and what keymap.rs
                // parses `alt+H` into
                let (code, mods_ct) = match code {
                    KeyCode::Char(c) if mods.shift && c.is_alphabetic() => (
                        KeyCode::Char(c.to_ascii_uppercase()),
                        mods_ct - KeyModifiers::SHIFT,
                    ),
                    _ => (code, mods_ct),
                };
                out.push(Input::Key(KeyEvent::new(code, mods_ct)));
            }
            egui::Event::PointerButton {
                pos,
                button,
                pressed: true,
                ..
            } => {
                let (column, row) = at(*pos);
                out.push(Input::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(to_button(*button)),
                    column,
                    row,
                    modifiers,
                }));
            }
            egui::Event::MouseWheel { unit, delta, .. } => {
                let Some(pos) = input.pointer.latest_pos() else {
                    continue;
                };
                // egui reports a wheel as a distance in whichever unit
                // the device speaks; a terminal reports one event per
                // notch, and `on_wheel` already moves three rows per
                // event. A notch is a line, a page is a screen's worth
                // capped at what a hand can flick, and points are
                // divided back into lines by the cell height.
                let lines = match unit {
                    egui::MouseWheelUnit::Line => delta.y.abs(),
                    egui::MouseWheelUnit::Page => delta.y.abs() * 10.0,
                    egui::MouseWheelUnit::Point => delta.y.abs() / metrics.height,
                };
                let notches = lines.ceil().min(5.0) as usize;
                if notches == 0 || delta.y == 0.0 {
                    continue;
                }
                let kind = if delta.y > 0.0 {
                    MouseEventKind::ScrollUp
                } else {
                    MouseEventKind::ScrollDown
                };
                let (column, row) = at(pos);
                for _ in 0..notches {
                    out.push(Input::Mouse(MouseEvent {
                        kind,
                        column,
                        row,
                        modifiers,
                    }));
                }
            }
            _ => {}
        }
    }
    out
}

fn to_modifiers(mods: &egui::Modifiers) -> KeyModifiers {
    let mut out = KeyModifiers::NONE;
    if mods.shift {
        out |= KeyModifiers::SHIFT;
    }
    // `command` is Ctrl everywhere but macOS, where it is Cmd; a file
    // manager whose keys are all Ctrl wants Cmd to mean Ctrl there
    if mods.ctrl || mods.mac_cmd {
        out |= KeyModifiers::CONTROL;
    }
    if mods.alt {
        out |= KeyModifiers::ALT;
    }
    out
}

fn to_button(button: egui::PointerButton) -> MouseButton {
    match button {
        egui::PointerButton::Secondary => MouseButton::Right,
        egui::PointerButton::Middle => MouseButton::Middle,
        _ => MouseButton::Left,
    }
}

/// An egui key as the code crossterm would have reported. `None` for
/// keys a terminal has no spelling for (the modifiers themselves, the
/// browser and media keys), which are simply not events here.
fn to_code(key: egui::Key) -> Option<KeyCode> {
    use egui::Key as K;
    let code = match key {
        K::ArrowDown => KeyCode::Down,
        K::ArrowLeft => KeyCode::Left,
        K::ArrowRight => KeyCode::Right,
        K::ArrowUp => KeyCode::Up,
        K::Escape => KeyCode::Esc,
        K::Tab => KeyCode::Tab,
        K::Backspace => KeyCode::Backspace,
        K::Enter => KeyCode::Enter,
        K::Space => KeyCode::Char(' '),
        K::Insert => KeyCode::Insert,
        K::Delete => KeyCode::Delete,
        K::Home => KeyCode::Home,
        K::End => KeyCode::End,
        K::PageUp => KeyCode::PageUp,
        K::PageDown => KeyCode::PageDown,
        K::Copy => return None,
        K::Cut => return None,
        K::Paste => return None,
        K::Colon => KeyCode::Char(':'),
        K::Comma => KeyCode::Char(','),
        K::Backslash => KeyCode::Char('\\'),
        K::Slash => KeyCode::Char('/'),
        K::Pipe => KeyCode::Char('|'),
        K::Questionmark => KeyCode::Char('?'),
        K::Exclamationmark => KeyCode::Char('!'),
        K::OpenBracket => KeyCode::Char('['),
        K::CloseBracket => KeyCode::Char(']'),
        K::OpenCurlyBracket => KeyCode::Char('{'),
        K::CloseCurlyBracket => KeyCode::Char('}'),
        K::Backtick => KeyCode::Char('`'),
        K::Minus => KeyCode::Char('-'),
        K::Period => KeyCode::Char('.'),
        K::Plus => KeyCode::Char('+'),
        K::Equals => KeyCode::Char('='),
        K::Semicolon => KeyCode::Char(';'),
        K::Quote => KeyCode::Char('\''),
        K::F1 => KeyCode::F(1),
        K::F2 => KeyCode::F(2),
        K::F3 => KeyCode::F(3),
        K::F4 => KeyCode::F(4),
        K::F5 => KeyCode::F(5),
        K::F6 => KeyCode::F(6),
        K::F7 => KeyCode::F(7),
        K::F8 => KeyCode::F(8),
        K::F9 => KeyCode::F(9),
        K::F10 => KeyCode::F(10),
        K::F11 => KeyCode::F(11),
        K::F12 => KeyCode::F(12),
        K::F13 => KeyCode::F(13),
        K::F14 => KeyCode::F(14),
        K::F15 => KeyCode::F(15),
        K::F16 => KeyCode::F(16),
        K::F17 => KeyCode::F(17),
        K::F18 => KeyCode::F(18),
        K::F19 => KeyCode::F(19),
        K::F20 => KeyCode::F(20),
        K::F21 => KeyCode::F(21),
        K::F22 => KeyCode::F(22),
        K::F23 => KeyCode::F(23),
        K::F24 => KeyCode::F(24),
        K::F25
        | K::F26
        | K::F27
        | K::F28
        | K::F29
        | K::F30
        | K::F31
        | K::F32
        | K::F33
        | K::F34
        | K::F35 => return None,
        other => {
            let name = other.name();
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                // the letter and digit keys, whose names are the
                // character itself; crossterm reports the lowercase
                (Some(c), None) => KeyCode::Char(c.to_ascii_lowercase()),
                _ => return None,
            }
        }
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_named_keys_are_the_terminal_ones() {
        assert_eq!(to_code(egui::Key::F5), Some(KeyCode::F(5)));
        assert_eq!(to_code(egui::Key::PageDown), Some(KeyCode::PageDown));
        assert_eq!(to_code(egui::Key::Space), Some(KeyCode::Char(' ')));
    }

    #[test]
    fn letters_and_digits_come_through_lowercase() {
        // Ctrl+O has to be Char('o'), which is what crossterm sends and
        // what `keymap.rs` parses "ctrl+o" into
        assert_eq!(to_code(egui::Key::O), Some(KeyCode::Char('o')));
        assert_eq!(to_code(egui::Key::Num7), Some(KeyCode::Char('7')));
    }

    #[test]
    fn modifiers_carry_over_and_cmd_counts_as_ctrl() {
        let mods = egui::Modifiers {
            alt: true,
            ctrl: false,
            shift: true,
            mac_cmd: false,
            command: false,
        };
        let out = to_modifiers(&mods);
        assert!(out.contains(KeyModifiers::ALT));
        assert!(out.contains(KeyModifiers::SHIFT));
        assert!(!out.contains(KeyModifiers::CONTROL));

        let cmd = egui::Modifiers {
            mac_cmd: true,
            ..Default::default()
        };
        assert!(to_modifiers(&cmd).contains(KeyModifiers::CONTROL));
    }

    /// The whole translation, on a synthetic frame's worth of input.
    // InputState keeps most of itself private, so it is built by
    // assignment rather than with struct-update syntax
    #[allow(clippy::field_reassign_with_default)]
    fn collected(events: Vec<egui::Event>, modifiers: egui::Modifiers) -> Vec<KeyEvent> {
        let mut input = egui::InputState::default();
        input.events = events;
        input.modifiers = modifiers;
        collect(&input, egui::Pos2::ZERO, Metrics::estimate(10.0))
            .into_iter()
            .filter_map(|i| match i {
                Input::Key(key) => Some(key),
                Input::Mouse(_) => None,
            })
            .collect()
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn a_printable_key_arrives_once() {
        // egui reports a typed letter twice - as a Key and as Text -
        // and a file manager that acts on each keystroke twice is
        // unusable, so only the text half is delivered
        let typed = collected(
            vec![
                key(egui::Key::A, egui::Modifiers::default()),
                egui::Event::Text("a".into()),
            ],
            egui::Modifiers::default(),
        );
        assert_eq!(
            typed,
            vec![KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)]
        );
    }

    #[test]
    fn a_capital_is_the_capital_with_no_shift_bit() {
        // what a terminal sends, and what `keymap.rs` parses "shift+h"
        // into - the SHIFT bit would make it a key nobody bound
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let typed = collected(
            vec![key(egui::Key::H, shift), egui::Event::Text("H".into())],
            shift,
        );
        assert_eq!(
            typed,
            vec![KeyEvent::new(KeyCode::Char('H'), KeyModifiers::NONE)]
        );
    }

    #[test]
    fn ctrl_and_alt_come_from_the_key_half() {
        // Ctrl+O produces no text, so the Key event is the only one
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(
            collected(vec![key(egui::Key::O, ctrl)], ctrl),
            vec![KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)]
        );

        // Alt+Shift+H is Alt + the capital: a different key from Alt+h,
        // which is what `keymap.rs`'s own comment says
        let alt_shift = egui::Modifiers {
            alt: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            collected(vec![key(egui::Key::H, alt_shift)], alt_shift),
            vec![KeyEvent::new(KeyCode::Char('H'), KeyModifiers::ALT)]
        );
    }

    #[test]
    fn the_keys_a_panel_is_driven_with_survive_the_trip() {
        let none = egui::Modifiers::default();
        let events = vec![
            key(egui::Key::F5, none),
            key(egui::Key::ArrowDown, none),
            key(egui::Key::Enter, none),
            key(egui::Key::Tab, none),
            key(egui::Key::Escape, none),
            // Enter and Tab can arrive as text as well; delivering them
            // twice would insert a line nobody asked for
            egui::Event::Text("\n".into()),
            egui::Event::Text("\t".into()),
        ];
        assert_eq!(
            collected(events, none),
            vec![
                KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            ]
        );
    }
}
