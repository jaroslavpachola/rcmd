//! Runtime state rcmd owns and writes: panel state, the hotlist, and
//! whatever the options form toggles. Split out of `config.toml`
//! (PLAN4 S0) so the user's own file is never rewritten - comments and
//! hand formatting survive because rcmd only ever *reads* it.
//!
//! Layering: defaults < `config.toml` < `state.toml`. State is sparse -
//! only keys rcmd actually changed are stored, so a config edit keeps
//! working for everything the user never touched in the UI.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{Config, HotEntry};

/// Every field optional: `None` = "rcmd never changed this", so the
/// config file (or the built-in default) still decides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_reverse: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listing: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lynx: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mouse: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subshell: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brief_columns: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split_ratio: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_menubar: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_mini_status: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_free_space: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_status: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_cmdline: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_keybar: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_delete: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_overwrite: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_exit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_hotlist_delete: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_execute: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_tab_size: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_fill_tabs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_auto_indent: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_backspace_tabs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_wrap_column: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_line_numbers: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_backups: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_clipboard: Option<bool>,
    /// `None` = never edited in rcmd, so `config.toml`'s list stands.
    /// Once `a`/`d` touches it, this owns the list outright.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotlist: Option<Vec<HotEntry>>,
    /// Panelize presets. `None` = never touched here, so the config
    /// file's list stands; once one is saved or dropped, this owns it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panelize: Option<Vec<crate::config::PanelizePreset>>,
    /// Command lines from previous sessions, oldest first (M-h lists
    /// them, C-p/M-p walk them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd_history: Vec<String>,
    /// What was typed into each dialog field before, newest last, kept
    /// per field (`mkdir`, `cd`, `chown`…). mc keeps these too; M-p and
    /// M-n walk them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub field_history: BTreeMap<String, Vec<String>>,
}

/// `$XDG_STATE_HOME/rcmd/state.toml`, falling back to
/// `~/.local/state/rcmd/state.toml` per the XDG spec.
pub fn state_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(dir).join("rcmd/state.toml"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state/rcmd/state.toml"))
}

/// Missing file → empty state; unparsable → empty state plus a warning
/// (a broken state file must never stop rcmd from starting).
pub fn load() -> (State, Option<String>) {
    let Some(path) = state_path() else {
        return (State::default(), None);
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str(&text) {
            Ok(state) => (state, None),
            Err(err) => {
                let first = err.to_string().lines().next().unwrap_or("").to_string();
                (State::default(), Some(format!("state: {first}")))
            }
        },
        Err(_) => (State::default(), None),
    }
}

pub fn save(state: &State) -> Result<()> {
    let Some(path) = state_path() else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string_pretty(state)?)?;
    Ok(())
}

/// Read-modify-write against the *on-disk* state, so a long-lived
/// instance never clobbers what another one saved meanwhile.
pub fn update(f: impl FnOnce(&mut State)) -> Result<()> {
    let (mut state, _) = load();
    f(&mut state);
    save(&state)
}

/// Overlay the state onto an effective config. Only `Some` fields win.
pub fn apply(state: &State, config: &mut Config) {
    macro_rules! set {
        ($($field:ident),+ $(,)?) => {$(
            if let Some(value) = state.$field.clone() {
                config.$field = value;
            }
        )+};
    }
    set!(
        show_hidden,
        sort_key,
        sort_reverse,
        listing,
        mouse,
        watch,
        git,
        subshell,
        editor,
        theme,
        confirm_delete,
        confirm_overwrite,
        confirm_exit,
        confirm_hotlist_delete,
        confirm_execute,
        brief_columns,
        split,
        split_ratio,
        show_menubar,
        show_mini_status,
        show_free_space,
        show_status,
        show_cmdline,
        show_keybar,
        edit_tab_size,
        edit_fill_tabs,
        edit_auto_indent,
        edit_backspace_tabs,
        edit_wrap_column,
        edit_line_numbers,
        edit_backups,
        edit_clipboard
    );
    // `lynx` is Option in the config too: unset means "follow the preset".
    if state.lynx.is_some() {
        config.lynx = state.lynx;
    }
    if let Some(hotlist) = state.hotlist.clone() {
        config.hotlist = hotlist;
    }
    if let Some(panelize) = state.panelize.clone() {
        config.panelize = panelize;
    }
}

/// One-release migration: the keys rcmd used to write into
/// `config.toml`. Seeds an empty state from whichever of them are
/// actually present in the user's file - absent keys stay `None` so
/// they keep following the config/defaults. A later release drops this.
pub fn seed_from_config(config_text: &str) -> State {
    let mut state = State::default();
    let Ok(value) = config_text.parse::<toml::Value>() else {
        return state;
    };
    let Some(table) = value.as_table() else {
        return state;
    };
    state.show_hidden = table.get("show_hidden").and_then(toml::Value::as_bool);
    state.sort_reverse = table.get("sort_reverse").and_then(toml::Value::as_bool);
    state.sort_key = table
        .get("sort_key")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    state.listing = table
        .get("listing")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    if let Some(entries) = table.get("hotlist").and_then(toml::Value::as_array) {
        let hotlist: Vec<HotEntry> = entries
            .iter()
            .filter_map(|entry| entry.clone().try_into().ok())
            .collect();
        if !hotlist.is_empty() {
            state.hotlist = Some(hotlist);
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_state_changes_nothing() {
        let mut config = Config::default();
        let before = format!("{config:?}");
        apply(&State::default(), &mut config);
        assert_eq!(before, format!("{config:?}"));
    }

    #[test]
    fn state_overrides_config() {
        let mut config = Config {
            theme: "mc".into(),
            show_hidden: true,
            ..Config::default()
        };
        let state = State {
            theme: Some("dark".into()),
            show_hidden: Some(false),
            ..State::default()
        };
        apply(&state, &mut config);
        assert_eq!(config.theme, "dark");
        assert!(!config.show_hidden);
        // untouched keys still come from the config
        assert_eq!(config.editor, "internal");
    }

    #[test]
    fn sparse_state_round_trips_without_none_keys() {
        let state = State {
            listing: Some("long".into()),
            ..State::default()
        };
        let text = toml::to_string_pretty(&state).unwrap();
        assert!(text.contains("listing"));
        assert!(!text.contains("theme"));
        let back: State = toml::from_str(&text).unwrap();
        assert_eq!(back.listing.as_deref(), Some("long"));
        assert_eq!(back.theme, None);
    }

    #[test]
    fn migration_seeds_only_present_keys() {
        let text = r#"
theme = "dark"
show_hidden = false
sort_key = "mtime"

[[hotlist]]
label = "projects"
path = "/home/you/git"
"#;
        let state = seed_from_config(text);
        assert_eq!(state.show_hidden, Some(false));
        assert_eq!(state.sort_key.as_deref(), Some("mtime"));
        assert_eq!(state.hotlist.unwrap()[0].label, "projects");
        // listing was absent, and theme is not a migrated key
        assert_eq!(state.listing, None);
        assert_eq!(state.theme, None);
    }

    #[test]
    fn migration_ignores_a_broken_config() {
        assert_eq!(seed_from_config("this is not toml =").listing, None);
    }
}
