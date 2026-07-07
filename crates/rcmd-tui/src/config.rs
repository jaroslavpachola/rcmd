//! `~/.config/rcmd/config.toml` — loaded at startup, written back on a
//! clean exit (sort/hidden state, hotlist). The file is regenerated, so
//! comments in it do not survive.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use rcmd_core::panel::{ListMode, SortKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "mc" (classic blue) or "dark" (truecolor). Applied at startup.
    pub theme: String,
    /// "mc" or "modern" (turns lynx-like motion on by default).
    pub keymap: String,
    /// Lynx-like motion: Left = parent directory, Right = enter the
    /// directory under the cursor (files stay untouched). Toggled from
    /// F9 > Options and persisted; unset follows the keymap preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lynx: Option<bool>,
    pub show_hidden: bool,
    pub sort_key: String,
    pub sort_reverse: bool,
    /// Panel listing format: "brief" | "full" | "long".
    pub listing: String,
    /// Auto-reload panels when their directory changes on disk.
    pub watch: bool,
    /// Mouse support (click, double-click, wheel). Additive only —
    /// hold Shift to select terminal text while it is on.
    pub mouse: bool,
    /// Git status column + branch in the panel title inside work trees
    /// (needs the `git` build feature, on by default).
    pub git: bool,
    /// "internal" (the built-in editor) or "external" ($VISUAL/$EDITOR).
    pub editor: String,
    /// Keep a persistent `$SHELL` on its own pty: Ctrl+O toggles to its
    /// screen, typed commands run inside it, panels follow its cwd.
    /// false = the pre-3.0 one-shot command execution.
    pub subshell: bool,
    /// Custom bindings on top of the preset, e.g. "ctrl+y" = "swap-panels".
    pub keys: BTreeMap<String, String>,
    pub hotlist: Vec<HotEntry>,
    /// Openers consulted by Enter on a file, in file order — the first
    /// matching glob wins.
    pub open: Vec<OpenRule>,
    /// User commands: the F2 menu, in file order.
    pub commands: Vec<UserCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotEntry {
    pub label: String,
    pub path: String,
}

/// `[[open]]` — `match = "*.pdf"`, `run = "zathura %f &"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRule {
    #[serde(rename = "match")]
    pub pattern: String,
    pub run: String,
}

/// `[[commands]]` — a named shell template with `%f %d %D %t` macros,
/// shown in the F2 menu; `key` optionally binds it directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCommand {
    pub name: String,
    pub run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl Config {
    /// Effective lynx-motion state: an explicit `lynx` wins, otherwise
    /// the "modern" preset implies on.
    pub fn lynx_on(&self) -> bool {
        self.lynx.unwrap_or(self.keymap == "modern")
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "mc".into(),
            keymap: "mc".into(),
            lynx: None,
            show_hidden: true,
            sort_key: "name".into(),
            sort_reverse: false,
            listing: "full".into(),
            watch: true,
            mouse: true,
            git: true,
            editor: "internal".into(),
            subshell: true,
            keys: BTreeMap::new(),
            hotlist: Vec::new(),
            open: Vec::new(),
            commands: Vec::new(),
        }
    }
}

pub fn config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("rcmd/config.toml"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/rcmd/config.toml"))
}

/// Missing file → defaults; unparsable file → defaults plus a warning
/// (never refuse to start over a config typo).
pub fn load() -> (Config, Option<String>) {
    let Some(path) = config_path() else {
        return (Config::default(), None);
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str(&text) {
            Ok(config) => (config, None),
            Err(err) => {
                let first = err
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                (Config::default(), Some(format!("config: {first}")))
            }
        },
        Err(_) => (Config::default(), None),
    }
}

pub fn save(config: &Config) -> Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string_pretty(config)?)?;
    Ok(())
}

pub fn list_mode_from_name(name: &str) -> ListMode {
    match name {
        "brief" => ListMode::Brief,
        "long" => ListMode::Long,
        _ => ListMode::Full,
    }
}

pub fn list_mode_name(mode: ListMode) -> &'static str {
    match mode {
        ListMode::Brief => "brief",
        ListMode::Full => "full",
        ListMode::Long => "long",
    }
}

pub fn sort_key_from_name(name: &str) -> SortKey {
    match name {
        "ext" => SortKey::Ext,
        "size" => SortKey::Size,
        "mtime" => SortKey::Mtime,
        _ => SortKey::Name,
    }
}

pub fn sort_key_name(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "name",
        SortKey::Ext => "ext",
        SortKey::Size => "size",
        SortKey::Mtime => "mtime",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_gives_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.theme, "mc");
        assert!(config.show_hidden);
        assert!(config.hotlist.is_empty());
    }

    #[test]
    fn round_trip_preserves_keys_and_hotlist() {
        let text = r#"
theme = "dark"
sort_key = "mtime"
sort_reverse = true

[keys]
"ctrl+y" = "swap-panels"
'ctrl+\' = "hotlist"

[[hotlist]]
label = "projects"
path = "/home/user/git"
"#;
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.theme, "dark");
        assert!(config.sort_reverse);
        assert_eq!(config.keys["ctrl+y"], "swap-panels");
        assert_eq!(config.keys["ctrl+\\"], "hotlist");
        assert_eq!(config.hotlist[0].label, "projects");

        let out = toml::to_string_pretty(&config).unwrap();
        let back: Config = toml::from_str(&out).unwrap();
        assert_eq!(back.keys.len(), 2);
        assert_eq!(back.hotlist.len(), 1);
        assert_eq!(back.sort_key, "mtime");
    }

    #[test]
    fn openers_and_commands_round_trip() {
        let text = r#"
[[open]]
match = "*.pdf"
run = "zathura %f &"

[[commands]]
name = "git status"
run = "git status | less"
key = "ctrl+g"

[[commands]]
name = "disk usage"
run = "du -sh %t | less"
"#;
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.open[0].pattern, "*.pdf");
        assert_eq!(config.commands[0].key.as_deref(), Some("ctrl+g"));
        assert_eq!(config.commands[1].key, None);

        let out = toml::to_string_pretty(&config).unwrap();
        let back: Config = toml::from_str(&out).unwrap();
        assert_eq!(back.open.len(), 1);
        assert_eq!(back.commands.len(), 2);
        assert_eq!(back.commands[0].name, "git status");
    }

    #[test]
    fn sort_key_names_round_trip() {
        for key in [SortKey::Name, SortKey::Ext, SortKey::Size, SortKey::Mtime] {
            assert_eq!(sort_key_from_name(sort_key_name(key)), key);
        }
        assert_eq!(sort_key_from_name("garbage"), SortKey::Name);
    }
}
