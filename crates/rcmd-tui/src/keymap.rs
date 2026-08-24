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
    let spec = spec.trim().to_lowercase();
    let (mods_part, key_part): (&str, &str) = if let Some(stripped) = spec.strip_suffix('+') {
        // the key itself is '+': "" for "+", "ctrl+" for "ctrl++"
        (stripped.strip_suffix('+').unwrap_or(stripped), "+")
    } else if let Some(i) = spec.rfind('+') {
        (&spec[..i], &spec[i + 1..])
    } else {
        ("", spec.as_str())
    };

    let mut mods = KeyModifiers::NONE;
    for part in mods_part.split('+').filter(|p| !p.is_empty()) {
        mods |= match part {
            "ctrl" | "control" | "c" => KeyModifiers::CONTROL,
            "alt" | "meta" | "m" => KeyModifiers::ALT,
            "shift" | "s" => KeyModifiers::SHIFT,
            _ => return None,
        };
    }

    let code = match key_part {
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
        "listing-cycle" => Action::ListingCycle,
        "other-same-dir" => Action::OtherSameDir,
        "other-open-dir" => Action::OtherOpenDir,
        "reload" => Action::Reload,
        "swap-panels" => Action::SwapPanels,
        "toggle-hidden" => Action::ToggleHidden,
        "options" => Action::Options,
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
        "jobs" => Action::Jobs,
        "vfs-list" => Action::VfsList,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    /// C-f / C-b: the next / previous file of the panel.
    NextFile,
    PrevFile,
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
        "next-file" => ViewerAction::NextFile,
        "prev-file" => ViewerAction::PrevFile,
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
        _ => return None,
    })
}

/// Defaults plus the user's `[keys.viewer]` overrides.
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
