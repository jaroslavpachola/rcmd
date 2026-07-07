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

/// "ctrl+shift+f8", "alt+.", "+", "ctrl++" — modifiers then one key.
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
        "history-back" => Action::HistoryBack,
        "history-forward" => Action::HistoryForward,
        "quick-view" => Action::QuickView,
        "info-view" => Action::InfoView,
        "user-menu" => Action::UserMenu,
        "listing-brief" => Action::Listing(ListMode::Brief),
        "listing-full" => Action::Listing(ListMode::Full),
        "listing-long" => Action::Listing(ListMode::Long),
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
