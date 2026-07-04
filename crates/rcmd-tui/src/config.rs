//! `~/.config/rcmd/config.toml` — loaded at startup, written back on a
//! clean exit (sort/hidden state, hotlist). The file is regenerated, so
//! comments in it do not survive.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use rcmd_core::panel::SortKey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "mc" (classic blue) or "dark" (truecolor). Applied at startup.
    pub theme: String,
    /// "mc" or "modern" (adds lynx-style Left/Right navigation).
    pub keymap: String,
    pub show_hidden: bool,
    pub sort_key: String,
    pub sort_reverse: bool,
    /// Custom bindings on top of the preset, e.g. "ctrl+y" = "swap-panels".
    pub keys: BTreeMap<String, String>,
    pub hotlist: Vec<HotEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotEntry {
    pub label: String,
    pub path: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "mc".into(),
            keymap: "mc".into(),
            show_hidden: true,
            sort_key: "name".into(),
            sort_reverse: false,
            keys: BTreeMap::new(),
            hotlist: Vec::new(),
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
    fn sort_key_names_round_trip() {
        for key in [SortKey::Name, SortKey::Ext, SortKey::Size, SortKey::Mtime] {
            assert_eq!(sort_key_from_name(sort_key_name(key)), key);
        }
        assert_eq!(sort_key_from_name("garbage"), SortKey::Name);
    }
}
