//! Panel-mode key bindings: a preset ("mc" or "modern") plus custom
//! overrides from config. Navigation and command-line editing keys stay
//! hardcoded; everything action-like routes through this map.

use std::collections::{BTreeMap, HashMap};

use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use rcmd_core::panel::{ListMode, SortKey};

use crate::app::Action;

pub type Keymap = HashMap<(KeyCode, KeyModifiers), Action>;

const DEFAULTS: &[(&str, &str)] = &[
    ("f1", "help"),
    ("f3", "view"),
    ("shift+f3", "view-raw"),
    ("f15", "view-raw"), // S-F3 on legacy terminals
    ("f4", "edit"),
    ("f5", "copy"),
    ("f6", "move"),
    ("f7", "mkdir"),
    ("f8", "delete"),
    ("shift+f8", "delete-perm"),
    ("f20", "delete-perm"), // Shift+F8 on legacy terminals
    ("f2", "user-menu"),
    ("f9", "menu"),
    ("f10", "quit"),
    ("insert", "mark"),
    ("ctrl+t", "mark"),
    ("ctrl+o", "shell"),
    ("ctrl+r", "reload"),
    ("ctrl+s", "quick-search"),
    ("ctrl+f", "filter"),
    ("alt+f7", "find-file"),
    ("ctrl+space", "dir-size"),
    ("ctrl+\\", "hotlist"),
    ("ctrl+4", "hotlist"), // Ctrl+\ (0x1C) on legacy terminals arrives as Ctrl+4
    ("alt+left", "history-back"),
    ("alt+right", "history-forward"),
    ("alt+y", "history-back"), // MC: M-y / M-u walk the history
    ("alt+u", "history-forward"),
    ("alt+?", "find-file"),    // MC: M-? find file
    ("alt+c", "quick-cd"),     // MC: M-c quick cd
    ("alt+h", "history-list"), // MC: M-h command-line history
    ("alt+H", "dir-history"),  // MC: M-H the panel's directory history
    ("alt+a", "paste-path"),   // MC: M-a paste the panel directory
    ("ctrl+l", "repaint"),
    ("shift+f4", "edit-new"),
    ("f16", "edit-new"), // S-F4 on legacy terminals
    ("shift+f5", "copy-here"),
    ("f17", "copy-here"), // S-F5 on legacy terminals
    ("shift+f6", "move-here"),
    ("f18", "move-here"), // S-F6 on legacy terminals
    ("alt+up", "hotlist"),
    ("alt+i", "other-same-dir"),
    ("alt+o", "other-open-dir"),
    ("alt+.", "toggle-hidden"),
    ("alt+n", "sort-name"),
    // MC hands own these: M-s = quick search, M-t = cycle listing,
    // C-u = swap panels (sort by ext/size/mtime lives in F9 > Sort).
    ("alt+e", "charset"), // MC: M-e, the panel's codepage
    ("alt+s", "quick-search"),
    ("alt+t", "listing-cycle"),
    ("ctrl+u", "swap-panels"),
    ("+", "select-group"),
    ("-", "unselect-group"),
    ("\\", "unselect-group"),
    ("*", "invert-selection"),
];

/// MC's "Lynx-like motion" (F9 > Options): Left = parent directory,
/// Right = enter the directory under the cursor. The "modern" preset
/// turns it on by default; the `lynx` config key overrides either way.
const LYNX_KEYS: &[(&str, &str)] = &[("left", "up-dir"), ("right", "enter")];

pub fn build(preset: &str, lynx: bool, custom: &BTreeMap<String, String>) -> (Keymap, Vec<String>) {
    let mut map = Keymap::new();
    let mut warnings = Vec::new();
    for (key, action) in DEFAULTS {
        bind(&mut map, key, action).expect("default binding must parse");
    }
    if !matches!(preset, "mc" | "modern") {
        warnings.push(format!("unknown keymap preset '{preset}', using mc"));
    }
    if lynx {
        for (key, action) in LYNX_KEYS {
            bind(&mut map, key, action).expect("lynx binding must parse");
        }
    }
    for (key, action) in custom {
        if let Err(warning) = bind(&mut map, key, action) {
            warnings.push(warning);
        }
    }
    (map, warnings)
}

fn bind(map: &mut Keymap, key: &str, action: &str) -> Result<(), String> {
    let key_parsed = parse_key(key).ok_or_else(|| format!("bad key '{key}'"))?;
    let action_parsed = parse_action(action).ok_or_else(|| format!("bad action '{action}'"))?;
    map.insert(key_parsed, action_parsed);
    Ok(())
}

/// "ctrl+shift+f8", "alt+.", "+", "ctrl++" - modifiers then one key.
pub fn parse_key(spec: &str) -> Option<(KeyCode, KeyModifiers)> {
    let spec = spec.trim();
    let (mods_part, key_part): (&str, &str) = if let Some(stripped) = spec.strip_suffix('+') {
        // the key itself is '+': "" for "+", "ctrl+" for "ctrl++"
        (stripped.strip_suffix('+').unwrap_or(stripped), "+")
    } else if let Some(i) = spec.rfind('+') {
        (&spec[..i], &spec[i + 1..])
    } else {
        ("", spec)
    };

    let mut mods = KeyModifiers::NONE;
    for part in mods_part.split('+').filter(|p| !p.is_empty()) {
        mods |= match part.to_lowercase().as_str() {
            "ctrl" | "control" | "c" => KeyModifiers::CONTROL,
            "alt" | "meta" | "m" => KeyModifiers::ALT,
            "shift" | "s" => KeyModifiers::SHIFT,
            _ => return None,
        };
    }

    // a letter keeps its case - "alt+H" is Alt+Shift+H, a different key
    // from "alt+h" - and "shift+h" is the same key spelled the long way,
    // since a terminal sends the capital and no shift bit
    let mut single = key_part.chars();
    if let (Some(c), None) = (single.next(), single.next()) {
        let c = if mods.contains(KeyModifiers::SHIFT) && c.is_alphabetic() {
            mods.remove(KeyModifiers::SHIFT);
            c.to_uppercase().next().unwrap_or(c)
        } else {
            c
        };
        return Some((KeyCode::Char(c), mods));
    }

    let code = match key_part.to_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        "insert" | "ins" => KeyCode::Insert,
        "delete" | "del" => KeyCode::Delete,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pgup" | "pageup" => KeyCode::PageUp,
        "pgdn" | "pagedown" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        f if f.len() >= 2 && f.starts_with('f') && f[1..].chars().all(|c| c.is_ascii_digit()) => {
            KeyCode::F(f[1..].parse().ok()?)
        }
        single => {
            let mut chars = single.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c)
        }
    };
    Some((code, mods))
}

/// The inverse of [`parse_key`]: what a key event is called in the
/// config. This is what the Learn keys dialog shows, so that whatever
/// the terminal sent can be pasted into `[keys.panel]` as it stands.
pub fn key_name(code: KeyCode, mods: KeyModifiers) -> String {
    let mut out = String::new();
    if mods.contains(KeyModifiers::CONTROL) {
        out.push_str("ctrl+");
    }
    if mods.contains(KeyModifiers::ALT) {
        out.push_str("alt+");
    }
    // shift is part of the character for letters; it only earns a name
    // where the key has no case of its own
    if mods.contains(KeyModifiers::SHIFT) && !matches!(code, KeyCode::Char(_)) {
        out.push_str("shift+");
    }
    let name = match code {
        KeyCode::Enter => "enter".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "shift+tab".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pgup".into(),
        KeyCode::PageDown => "pgdn".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::F(n) => format!("f{n}"),
        KeyCode::Char(c) => c.to_string(),
        other => format!("{other:?}").to_lowercase(),
    };
    out.push_str(&name);
    out
}

pub fn parse_action(name: &str) -> Option<Action> {
    Some(match name {
        "help" => Action::Help,
        "view" => Action::View,
        "view-raw" => Action::ViewRaw,
        "edit" => Action::Edit,
        "copy" => Action::Copy,
        "move" => Action::Move,
        "mkdir" => Action::Mkdir,
        "delete" => Action::Delete,
        "delete-perm" => Action::DeletePerm,
        "select-group" => Action::SelectGroup,
        "unselect-group" => Action::UnselectGroup,
        "invert-selection" => Action::InvertSelection,
        "quit" => Action::Quit,
        "shell" => Action::Shell,
        "sftp-link" => Action::SftpLink,
        "remote-link" => Action::SftpLink,
        "history-back" => Action::HistoryBack,
        "history-forward" => Action::HistoryForward,
        "quick-view" => Action::QuickView,
        "info-view" => Action::InfoView,
        "user-menu" => Action::UserMenu,
        "listing-brief" => Action::Listing(ListMode::Brief),
        "listing-full" => Action::Listing(ListMode::Full),
        "listing-long" => Action::Listing(ListMode::Long),
        "listing-tree" => Action::Listing(ListMode::Tree),
        "listing-user" => Action::Listing(ListMode::User),
        "dir-tree" => Action::DirTree,
        "charset" => Action::Charset,
        "listing-cycle" => Action::ListingCycle,
        "other-same-dir" => Action::OtherSameDir,
        "other-open-dir" => Action::OtherOpenDir,
        "reload" => Action::Reload,
        "swap-panels" => Action::SwapPanels,
        "toggle-hidden" => Action::ToggleHidden,
        "options" => Action::Options,
        "appearance" => Action::Appearance,
        "learn-keys" => Action::LearnKeys,
        "edit-config" => Action::EditConfig,
        "sort-name" => Action::Sort(SortKey::Name),
        "sort-ext" => Action::Sort(SortKey::Ext),
        "sort-size" => Action::Sort(SortKey::Size),
        "sort-mtime" => Action::Sort(SortKey::Mtime),
        "sort-reverse" => Action::SortReverse,
        "menu" => Action::Menu,
        "mark" => Action::Mark,
        "quick-search" => Action::QuickSearch,
        "hotlist" => Action::Hotlist,
        "filter" => Action::Filter,
        "find-file" => Action::FindFile,
        "panelize" => Action::Panelize,
        "compare-dirs" => Action::CompareDirs,
        "dir-size" => Action::DirSize,
        "up-dir" => Action::UpDir,
        "enter" => Action::Enter,
        "edit-new" => Action::EditNew,
        "copy-here" => Action::CopyHere,
        "move-here" => Action::MoveHere,
        "paste-tags" => Action::PasteTags,
        "paste-path" => Action::PastePath,
        "quick-cd" => Action::QuickCd,
        "repaint" => Action::Repaint,
        "bulk-rename" => Action::BulkRename,
        "history-list" => Action::HistoryList,
        "dir-history" => Action::DirHistory,
        "jobs" => Action::Jobs,
        "vfs-list" => Action::VfsList,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_round_trip() {
        for spec in [
            "f5",
            "ctrl+s",
            "alt+enter",
            "shift+f3",
            "ctrl+alt+left",
            "space",
            "pgdn",
            "insert",
            "a",
            "A",
            "alt+H",
        ] {
            let (code, mods) = parse_key(spec).expect(spec);
            assert_eq!(key_name(code, mods), spec, "{spec}");
        }
        // BackTab is one key crossterm reports as its own, and it is
        // spelled the way the config spells it
        assert_eq!(key_name(KeyCode::BackTab, KeyModifiers::NONE), "shift+tab");
        // a shifted letter is the capital; its case is the shift
        assert_eq!(key_name(KeyCode::Char('A'), KeyModifiers::SHIFT), "A");
        assert_eq!(
            parse_key("shift+h"),
            Some((KeyCode::Char('H'), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key("alt+shift+h"),
            Some((KeyCode::Char('H'), KeyModifiers::ALT))
        );
        assert_ne!(parse_key("alt+h"), parse_key("alt+H"));
        // modifier names are still case-blind
        assert_eq!(parse_key("Ctrl+F5"), parse_key("ctrl+f5"));
    }

    #[test]
    fn parse_key_variants() {
        assert_eq!(
            parse_key("ctrl+s"),
            Some((KeyCode::Char('s'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key("shift+f8"),
            Some((KeyCode::F(8), KeyModifiers::SHIFT))
        );
        assert_eq!(
            parse_key("+"),
            Some((KeyCode::Char('+'), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key("ctrl++"),
            Some((KeyCode::Char('+'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key("alt+."),
            Some((KeyCode::Char('.'), KeyModifiers::ALT))
        );
        assert_eq!(
            parse_key("ctrl+\\"),
            Some((KeyCode::Char('\\'), KeyModifiers::CONTROL))
        );
        assert_eq!(parse_key("f10"), Some((KeyCode::F(10), KeyModifiers::NONE)));
        assert_eq!(parse_key("bogus+x"), None);
        assert_eq!(parse_key("notakey"), None);
    }

    #[test]
    fn viewer_and_editor_maps_take_overrides() {
        let custom = BTreeMap::from([
            ("ctrl+w".to_string(), "wrap".to_string()),
            ("ctrl+bad".to_string(), "wrap".to_string()),
            ("ctrl+e".to_string(), "no-such-action".to_string()),
        ]);
        let (map, warnings) = build_viewer(&custom);
        assert_eq!(
            map.get(&(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            Some(&ViewerAction::ToggleWrap)
        );
        // defaults survive alongside the override
        assert_eq!(
            map.get(&(KeyCode::F(2), KeyModifiers::NONE)),
            Some(&ViewerAction::ToggleWrap)
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");

        let (map, warnings) = build_editor(&BTreeMap::from([(
            "ctrl+q".to_string(),
            "quit".to_string(),
        )]));
        assert_eq!(
            map.get(&(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(&EditorAction::Quit)
        );
        assert_eq!(
            map.get(&(KeyCode::F(2), KeyModifiers::NONE)),
            Some(&EditorAction::Save)
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn build_applies_preset_and_custom_overrides() {
        let custom = BTreeMap::from([
            ("ctrl+y".to_string(), "swap-panels".to_string()),
            ("zz+bad".to_string(), "quit".to_string()),
            ("f5".to_string(), "no-such-action".to_string()),
        ]);
        let (map, warnings) = build("modern", true, &custom);
        assert!(matches!(
            map.get(&(KeyCode::Left, KeyModifiers::NONE)),
            Some(Action::UpDir)
        ));
        assert!(matches!(
            map.get(&(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            Some(Action::SwapPanels)
        ));
        assert_eq!(warnings.len(), 2); // bad key + bad action
        // f5 keeps its default because the override failed to parse
        assert!(matches!(
            map.get(&(KeyCode::F(5), KeyModifiers::NONE)),
            Some(Action::Copy)
        ));
        let (_, warnings) = build("dvorak", false, &BTreeMap::new());
        assert_eq!(warnings.len(), 1);
        // lynx off: Left stays unbound (panel Left is a no-op in mc)
        let (map, _) = build("mc", false, &BTreeMap::new());
        assert!(!map.contains_key(&(KeyCode::Left, KeyModifiers::NONE)));
    }
}

// ---------------------------------------------------------------------
// Per-context maps (PLAN4 S0). The panel map above is the original one;
// the viewer and the editor now route their action keys through tables
// of their own, so `[keys.viewer]` / `[keys.editor]` can rebind them.
// Movement keys (arrows, PgUp/PgDn, Home/End) stay structural in every
// context, exactly as they are in the panel.
// ---------------------------------------------------------------------

/// What a key does in the F3 viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerAction {
    Quit,
    ToggleWrap,
    ToggleHex,
    Search,
    SearchNext,
    Follow,
    /// The goto prompt: a line, a byte offset or a percentage.
    Goto,
    /// `m` then a digit.
    SetMark,
    /// `r` then a digit.
    GoMark,
    ToggleRuler,
    /// F8: nroff overstrikes read as bold and underline.
    ToggleNroff,
    /// F6: the `[[view]]` filter in or out under the same file.
    ToggleRaw,
    /// M-e: which codepage the file is in.
    Charset,
    /// C-f / C-b: the next / previous file of the panel.
    NextFile,
    PrevFile,
    /// The hex cursor, and writing what it changed. Both live on keys
    /// that mean something else outside hex mode (F2 and F6, as mc's
    /// button bar spends them), so they have no default of their own.
    HexEdit,
    HexSave,
}

/// What a key does in the F4 editor (text entry and plain cursor
/// movement are handled before this map is consulted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorAction {
    Save,
    Quit,
    Mark,
    Replace,
    Search,
    SearchNext,
    BlockCopy,
    BlockMove,
    DeleteLine,
    Undo,
    Redo,
    Copy,
    Cut,
    Paste,
    SelectAll,
    ToggleWrap,
    /// F9: the editor's own menu bar.
    Menu,
    /// M-l: go to a line by number.
    Goto,
    /// M-k / M-j / M-i / M-o: mc's editor bookmarks.
    BookmarkToggle,
    BookmarkNext,
    BookmarkPrev,
    BookmarkClear,
    /// M-n: the line-number gutter.
    ToggleLineNumbers,
    /// M-e: which codepage the file is in.
    Charset,
}

pub type ViewerMap = HashMap<(KeyCode, KeyModifiers), ViewerAction>;
pub type EditorMap = HashMap<(KeyCode, KeyModifiers), EditorAction>;

const VIEWER_DEFAULTS: &[(&str, &str)] = &[
    ("f3", "quit"),
    ("f10", "quit"),
    ("q", "quit"),
    ("f2", "wrap"),
    ("f4", "hex"),
    ("f7", "search"),
    ("/", "search"),
    ("n", "search-next"),
    ("f", "follow"),
    ("f5", "goto"),
    ("alt+l", "goto"),
    (":", "goto"),
    ("m", "set-mark"),
    ("r", "go-mark"),
    ("alt+r", "ruler"),
    ("f8", "nroff"),
    ("f6", "raw"),
    ("alt+e", "charset"),
    ("ctrl+f", "next-file"),
    ("ctrl+b", "prev-file"),
];

const EDITOR_DEFAULTS: &[(&str, &str)] = &[
    ("f2", "save"),
    ("ctrl+s", "save"),
    ("f10", "quit"),
    ("f3", "mark"),
    ("f4", "replace"),
    ("f7", "search"),
    ("shift+f7", "search-next"),
    ("f19", "search-next"), // Shift+F7 on legacy terminals
    ("f5", "block-copy"),
    ("f6", "block-move"),
    ("f8", "delete-line"),
    ("ctrl+z", "undo"),
    ("ctrl+y", "redo"),
    ("ctrl+c", "copy"),
    ("ctrl+x", "cut"),
    ("ctrl+v", "paste"),
    ("ctrl+a", "select-all"),
    ("alt+w", "wrap"),
    ("f9", "menu"),
    ("ctrl+u", "undo"), // mc's undo key, beside rcmd's ctrl+z
    ("alt+l", "goto"),
    ("alt+k", "bookmark"),
    ("alt+j", "bookmark-next"),
    ("alt+i", "bookmark-prev"),
    ("alt+o", "bookmark-clear"),
    ("alt+n", "line-numbers"),
    ("alt+e", "charset"),
];

pub fn parse_viewer_action(name: &str) -> Option<ViewerAction> {
    Some(match name {
        "quit" => ViewerAction::Quit,
        "wrap" => ViewerAction::ToggleWrap,
        "hex" => ViewerAction::ToggleHex,
        "search" => ViewerAction::Search,
        "search-next" => ViewerAction::SearchNext,
        "follow" => ViewerAction::Follow,
        "goto" => ViewerAction::Goto,
        "set-mark" => ViewerAction::SetMark,
        "go-mark" => ViewerAction::GoMark,
        "ruler" => ViewerAction::ToggleRuler,
        "nroff" => ViewerAction::ToggleNroff,
        "raw" => ViewerAction::ToggleRaw,
        "charset" => ViewerAction::Charset,
        "next-file" => ViewerAction::NextFile,
        "prev-file" => ViewerAction::PrevFile,
        "hex-edit" => ViewerAction::HexEdit,
        "hex-save" => ViewerAction::HexSave,
        _ => return None,
    })
}

pub fn parse_editor_action(name: &str) -> Option<EditorAction> {
    Some(match name {
        "save" => EditorAction::Save,
        "quit" => EditorAction::Quit,
        "mark" => EditorAction::Mark,
        "replace" => EditorAction::Replace,
        "search" => EditorAction::Search,
        "search-next" => EditorAction::SearchNext,
        "block-copy" => EditorAction::BlockCopy,
        "block-move" => EditorAction::BlockMove,
        "delete-line" => EditorAction::DeleteLine,
        "undo" => EditorAction::Undo,
        "redo" => EditorAction::Redo,
        "copy" => EditorAction::Copy,
        "cut" => EditorAction::Cut,
        "paste" => EditorAction::Paste,
        "select-all" => EditorAction::SelectAll,
        "wrap" => EditorAction::ToggleWrap,
        "menu" => EditorAction::Menu,
        "goto" => EditorAction::Goto,
        "bookmark" => EditorAction::BookmarkToggle,
        "bookmark-next" => EditorAction::BookmarkNext,
        "bookmark-prev" => EditorAction::BookmarkPrev,
        "bookmark-clear" => EditorAction::BookmarkClear,
        "line-numbers" => EditorAction::ToggleLineNumbers,
        "charset" => EditorAction::Charset,
        _ => return None,
    })
}

/// Defaults plus the user's `[keys.viewer]` overrides.
/// What a dialog key can be rebound to. There are only four things a
/// dialog does with a key that is not text, and every dialog does them
/// the same way - so `[keys.dialog]` is a translation table rather than
/// a per-dialog action list: a bound key arrives at the dialog as the
/// key it stands for, and every dialog already knows that one.
pub fn build_dialog(custom: &BTreeMap<String, String>) -> (DialogMap, Vec<String>) {
    let mut map = DialogMap::new();
    let mut warnings = Vec::new();
    for (spec, action) in custom {
        let Some(key) = parse_key(spec) else {
            warnings.push(format!("[keys.dialog]: unknown key '{spec}'"));
            continue;
        };
        let canonical = match action.as_str() {
            "ok" | "accept" => (KeyCode::Enter, KeyModifiers::NONE),
            "cancel" => (KeyCode::Esc, KeyModifiers::NONE),
            "next" | "next-field" => (KeyCode::Tab, KeyModifiers::NONE),
            "prev" | "prev-field" => (KeyCode::BackTab, KeyModifiers::NONE),
            other => {
                warnings.push(format!(
                    "[keys.dialog]: unknown action '{other}' (ok, cancel, next, prev)"
                ));
                continue;
            }
        };
        map.insert((key.0, key.1), canonical);
    }
    (map, warnings)
}

/// A key in a dialog, and the key it stands in for.
pub type DialogMap = std::collections::HashMap<(KeyCode, KeyModifiers), (KeyCode, KeyModifiers)>;

pub fn build_viewer(custom: &BTreeMap<String, String>) -> (ViewerMap, Vec<String>) {
    build_context(VIEWER_DEFAULTS, custom, parse_viewer_action, "viewer")
}

/// Defaults plus the user's `[keys.editor]` overrides.
pub fn build_editor(custom: &BTreeMap<String, String>) -> (EditorMap, Vec<String>) {
    build_context(EDITOR_DEFAULTS, custom, parse_editor_action, "editor")
}

fn build_context<A: Copy>(
    defaults: &[(&str, &str)],
    custom: &BTreeMap<String, String>,
    parse: fn(&str) -> Option<A>,
    context: &str,
) -> (HashMap<(KeyCode, KeyModifiers), A>, Vec<String>) {
    let mut map = HashMap::new();
    let mut warnings = Vec::new();
    for (key, action) in defaults {
        let key = parse_key(key).expect("default binding must parse");
        map.insert(key, parse(action).expect("default action must parse"));
    }
    for (key, action) in custom {
        match (parse_key(key), parse(action)) {
            (Some(key), Some(action)) => {
                map.insert(key, action);
            }
            (None, _) => warnings.push(format!("bad key '{key}' in [keys.{context}]")),
            (_, None) => warnings.push(format!("bad {context} action '{action}'")),
        }
    }
    (map, warnings)
}
