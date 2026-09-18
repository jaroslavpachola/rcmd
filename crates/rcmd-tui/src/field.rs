//! The one-line text field every form is made of.
//!
//! Two layers. [`edit_line`] is the editing itself - the keys a shell's
//! line editor has taught every hand - over a plain `(String, cursor)`
//! pair, which is what the command line and the editor's and viewer's
//! prompts hold. [`TextField`] is that pair plus a history: the ring of
//! earlier answers in `state.toml`, walked with M-p / M-n and listed
//! with M-h, and written to when a form is submitted.
//!
//! What text is cut with C-w, C-k, C-u, M-d or M-Backspace goes to one
//! kill buffer shared by every field, and C-y puts it back - in this
//! field or in another one, which is what makes it more than undo.

use std::cell::RefCell;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// How many answers a field's history keeps. A convenience, not an
/// archive.
pub const HISTORY_CAP: usize = 30;

// the UI is one thread; a thread-local keeps the tests, which are not,
// from cutting into each other's buffer
thread_local! {
    static KILLED: RefCell<String> = const { RefCell::new(String::new()) };
}

fn kill(text: String) {
    if !text.is_empty() {
        KILLED.with(|k| *k.borrow_mut() = text);
    }
}

fn killed() -> String {
    KILLED.with(|k| k.borrow().clone())
}

/// Byte offset of the `char_idx`-th character, or the end.
pub fn byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Remove characters `from..to` and hand them to the kill buffer.
fn cut(value: &mut String, from: usize, to: usize) {
    if from >= to {
        return;
    }
    let (a, b) = (byte_index(value, from), byte_index(value, to));
    kill(value[a..b].to_string());
    value.replace_range(a..b, "");
}

/// A word is a run of letters, digits and `_`; everything else - `/`,
/// `.`, `-`, a space - separates them, so a path is walked a component
/// at a time. C-w is the exception: it goes back to the last space, the
/// way a shell's does, and takes a whole path argument with it.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_left(chars: &[char], mut at: usize, word: fn(char) -> bool) -> usize {
    while at > 0 && !word(chars[at - 1]) {
        at -= 1;
    }
    while at > 0 && word(chars[at - 1]) {
        at -= 1;
    }
    at
}

fn word_right(chars: &[char], mut at: usize) -> usize {
    while at < chars.len() && !is_word(chars[at]) {
        at += 1;
    }
    while at < chars.len() && is_word(chars[at]) {
        at += 1;
    }
    at
}

/// Put `text` in at the cursor, as if typed. Line breaks become spaces:
/// a field is one line, and a pasted newline must not submit the form.
pub fn insert_text(value: &mut String, cursor: &mut usize, text: &str) {
    let text: String = text
        .trim_end_matches(['\r', '\n'])
        .chars()
        .map(|c| {
            if matches!(c, '\r' | '\n' | '\t') {
                ' '
            } else {
                c
            }
        })
        .filter(|c| !c.is_control())
        .collect();
    value.insert_str(byte_index(value, *cursor), &text);
    *cursor += text.chars().count();
}

/// One key applied to a line being edited. False = not an editing key,
/// for the caller to do something else with.
pub fn edit_line(
    value: &mut String,
    cursor: &mut usize,
    code: KeyCode,
    mods: KeyModifiers,
) -> bool {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    let len = value.chars().count();
    *cursor = (*cursor).min(len);
    let chars = || value.chars().collect::<Vec<_>>();
    match code {
        // the whole line: every test and every hand clears a field so
        KeyCode::Char('u') if ctrl => {
            cut(value, 0, len);
            *cursor = 0;
        }
        KeyCode::Char('k') if ctrl => cut(value, *cursor, len),
        KeyCode::Char('w') if ctrl => {
            let from = word_left(&chars(), *cursor, |c| !c.is_whitespace());
            cut(value, from, *cursor);
            *cursor = from;
        }
        KeyCode::Backspace if alt => {
            let from = word_left(&chars(), *cursor, is_word);
            cut(value, from, *cursor);
            *cursor = from;
        }
        KeyCode::Char('d') if alt => {
            let to = word_right(&chars(), *cursor);
            cut(value, *cursor, to);
        }
        KeyCode::Char('y') if ctrl => insert_text(value, cursor, &killed()),
        KeyCode::Char('a') if ctrl => *cursor = 0,
        KeyCode::Char('e') if ctrl => *cursor = len,
        KeyCode::Char('b') if alt => *cursor = word_left(&chars(), *cursor, is_word),
        KeyCode::Char('f') if alt => *cursor = word_right(&chars(), *cursor),
        KeyCode::Left if ctrl => *cursor = word_left(&chars(), *cursor, is_word),
        KeyCode::Right if ctrl => *cursor = word_right(&chars(), *cursor),
        KeyCode::Char(c) if !ctrl && !alt => {
            value.insert(byte_index(value, *cursor), c);
            *cursor += 1;
        }
        KeyCode::Backspace => {
            if *cursor > 0 {
                *cursor -= 1;
                value.remove(byte_index(value, *cursor));
            }
        }
        KeyCode::Delete => {
            let idx = byte_index(value, *cursor);
            if idx < value.len() {
                value.remove(idx);
            }
        }
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(len),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = len,
        _ => return false,
    }
    true
}

/// What completing the word before the cursor came to.
pub enum Completion {
    NoMatch,
    /// One candidate, and the word is it now.
    Done,
    /// Several: the word advanced as far as they agree, and here they
    /// are, each with the word it would make and the stretch of the line
    /// (in characters) that word takes.
    Several {
        names: Vec<String>,
        words: Vec<String>,
        span: (usize, usize),
    },
}

/// Complete the word before the cursor: paths relative to `cwd`, and a
/// command from `$PATH` as the first word when `commands` says the line
/// is one.
pub fn complete(
    value: &mut String,
    cursor: &mut usize,
    cwd: &std::path::Path,
    commands: bool,
) -> Completion {
    let at = byte_index(value, *cursor);
    let start = rcmd_core::complete::word_start(&value[..at]);
    let Some(done) = rcmd_core::complete::complete(cwd, &value[..at], commands) else {
        return Completion::NoMatch;
    };
    value.replace_range(start..at, &done.word);
    let from = value[..start].chars().count();
    *cursor = from + done.word.chars().count();
    if done.matches.len() > 1 {
        Completion::Several {
            names: done.matches,
            words: done.options,
            span: (from, *cursor),
        }
    } else {
        Completion::Done
    }
}

/// Put `word` in place of characters `span.0..span.1`, cursor after it.
pub fn replace_span(value: &mut String, cursor: &mut usize, span: (usize, usize), word: &str) {
    let (a, b) = (byte_index(value, span.0), byte_index(value, span.1));
    value.replace_range(a..b, word);
    *cursor = span.0 + word.chars().count();
}

/// A text field: the line, its cursor, and - when it has one - the
/// history it walks and adds to.
#[derive(Clone, Debug, Default)]
pub struct TextField {
    pub value: String,
    /// In characters, not bytes.
    pub cursor: usize,
    /// The ring in `state.toml` this field reads and writes.
    history: Option<&'static str>,
    /// How far back M-p has walked, counted from the newest. `None` =
    /// on the line itself.
    walk: Option<usize>,
    /// What the line was when the walk began, typed or prefilled.
    draft: String,
    /// The ring, read once per field the first time it is walked
    /// rather than on every key.
    ring: Option<Vec<String>>,
}

impl TextField {
    /// A field holding `value`, cursor at its end.
    pub fn new(value: impl Into<String>) -> TextField {
        let value = value.into();
        TextField {
            cursor: value.chars().count(),
            value,
            ..TextField::default()
        }
    }

    /// ...that keeps its answers in the ring called `name`.
    pub fn with_history(mut self, name: &'static str) -> TextField {
        self.history = Some(name);
        self
    }

    /// Where the cursor starts, when not at the end.
    pub fn cursor(mut self, at: usize) -> TextField {
        self.cursor = at.min(self.value.chars().count());
        self
    }

    pub fn history_name(&self) -> Option<&'static str> {
        self.history
    }

    /// Replace the text, cursor at the end, off any history walk.
    pub fn set(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.value.chars().count();
        self.walk = None;
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Pasted text, in at the cursor.
    pub fn insert(&mut self, text: &str) {
        insert_text(&mut self.value, &mut self.cursor, text);
        self.walk = None;
    }

    /// One key: editing, or M-p / M-n through the history. False = not
    /// a key a field takes.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Char('p') => return self.step(true),
                KeyCode::Char('n') => return self.step(false),
                _ => {}
            }
        }
        let edited = edit_line(&mut self.value, &mut self.cursor, key.code, key.modifiers);
        if edited {
            // typing leaves the history and keeps what is typed
            self.walk = None;
        }
        edited
    }

    /// The ring, newest last, as the state file has it.
    pub fn entries(&mut self) -> &[String] {
        if self.ring.is_none() {
            self.ring = Some(match self.history {
                Some(name) => crate::state::load()
                    .0
                    .field_history
                    .remove(name)
                    .unwrap_or_default(),
                None => Vec::new(),
            });
        }
        self.ring.as_deref().unwrap_or_default()
    }

    /// M-p (`back`) / M-n. True whenever the field has a history, so
    /// the key is not also taken as something else.
    fn step(&mut self, back: bool) -> bool {
        if self.history.is_none() {
            return false;
        }
        let len = self.entries().len();
        if len == 0 {
            return true;
        }
        let at = match (self.walk, back) {
            (None, true) => {
                self.draft = self.value.clone();
                Some(0)
            }
            // nothing newer than the line itself: leave it be
            (None, false) => return true,
            (Some(at), true) => Some((at + 1).min(len - 1)),
            (Some(0), false) => None,
            (Some(at), false) => Some(at - 1),
        };
        let value = match at {
            Some(at) => self.entries()[len - 1 - at].clone(),
            None => std::mem::take(&mut self.draft),
        };
        self.value = value;
        self.cursor = self.value.chars().count();
        self.walk = at;
        true
    }

    /// Add what the field holds to its ring, newest last. URL passwords
    /// are taken out first: the ring is written to disk in plain text.
    pub fn remember(&self) -> anyhow::Result<()> {
        match self.history {
            Some(name) => remember(name, &self.value),
            None => Ok(()),
        }
    }
}

/// Add `value` to the ring called `name`.
pub fn remember(name: &str, value: &str) -> anyhow::Result<()> {
    if value.trim().is_empty() {
        return Ok(());
    }
    let (name, value) = (name.to_string(), rcmd_core::vfslog::redact_urls(value));
    crate::state::update(move |s| {
        let ring = s.field_history.entry(name).or_default();
        ring.retain(|old| old != &value);
        ring.push(value);
        let over = ring.len().saturating_sub(HISTORY_CAP);
        ring.drain(..over);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(value: &mut String, cursor: &mut usize, code: KeyCode, mods: KeyModifiers) {
        assert!(edit_line(value, cursor, code, mods), "{code:?} {mods:?}");
    }

    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;

    #[test]
    fn unicode_and_the_basic_keys() {
        let (mut value, mut cursor) = (String::new(), 0);
        for c in "héllo".chars() {
            press(
                &mut value,
                &mut cursor,
                KeyCode::Char(c),
                KeyModifiers::NONE,
            );
        }
        assert_eq!((value.as_str(), cursor), ("héllo", 5));
        press(&mut value, &mut cursor, KeyCode::Char('a'), CTRL);
        assert_eq!(cursor, 0);
        press(&mut value, &mut cursor, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(value, "éllo");
        press(&mut value, &mut cursor, KeyCode::Char('u'), CTRL);
        assert_eq!(value, "");
    }

    #[test]
    fn words_are_walked_and_cut_a_component_at_a_time() {
        let mut value = "cp /usr/local/bin".to_string();
        let mut cursor = value.chars().count();
        press(&mut value, &mut cursor, KeyCode::Char('b'), ALT);
        assert_eq!(cursor, 14, "back to the start of bin");
        press(&mut value, &mut cursor, KeyCode::Left, CTRL);
        assert_eq!(cursor, 8, "and of local");
        press(&mut value, &mut cursor, KeyCode::Char('f'), ALT);
        assert_eq!(cursor, 13, "forward to the end of local");
        press(&mut value, &mut cursor, KeyCode::Right, CTRL);
        assert_eq!(cursor, 17);

        press(&mut value, &mut cursor, KeyCode::Backspace, ALT);
        assert_eq!(value, "cp /usr/local/");
        // C-w goes back to the space, taking the whole argument
        press(&mut value, &mut cursor, KeyCode::Char('w'), CTRL);
        assert_eq!(value, "cp ");
    }

    #[test]
    fn what_is_cut_can_be_yanked_back_anywhere() {
        let mut value = "one two three".to_string();
        let mut cursor = 4;
        press(&mut value, &mut cursor, KeyCode::Char('k'), CTRL);
        assert_eq!((value.as_str(), cursor), ("one ", 4));

        // another field altogether
        let (mut other, mut at) = ("x".to_string(), 0);
        press(&mut other, &mut at, KeyCode::Char('y'), CTRL);
        assert_eq!((other.as_str(), at), ("two threex", 9));

        press(&mut value, &mut cursor, KeyCode::Home, KeyModifiers::NONE);
        press(&mut value, &mut cursor, KeyCode::Char('d'), ALT);
        assert_eq!(value, " ");
        press(&mut value, &mut cursor, KeyCode::Char('y'), CTRL);
        assert_eq!(value, "one ");
    }

    #[test]
    fn pasted_text_stays_on_one_line() {
        let (mut value, mut cursor) = ("ab".to_string(), 1);
        insert_text(&mut value, &mut cursor, "x\ny\r\n");
        assert_eq!((value.as_str(), cursor), ("ax yb", 4));
    }

    #[test]
    fn the_history_walk_keeps_the_draft() {
        let mut field = TextField::new("typed").with_history("test");
        field.ring = Some(vec!["old".into(), "new".into()]);
        let alt = |c| KeyEvent::new(KeyCode::Char(c), ALT);
        assert!(field.key(alt('n')));
        assert_eq!(field.value, "typed", "nothing newer than the line");
        field.key(alt('p'));
        assert_eq!(field.value, "new");
        field.key(alt('p'));
        field.key(alt('p'));
        assert_eq!(field.value, "old", "stops at the oldest");
        field.key(alt('n'));
        field.key(alt('n'));
        assert_eq!(field.value, "typed", "past the newest: the draft again");
        assert_eq!(field.cursor, 5);
    }

    #[test]
    fn a_field_without_history_lets_the_keys_through() {
        let mut field = TextField::new("x");
        assert!(!field.key(KeyEvent::new(KeyCode::Char('p'), ALT)));
    }
}
