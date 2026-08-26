//! Skins: a theme that lives in a file instead of in the binary.
//!
//! Two formats, because an mc user already has one. rcmd's own is TOML
//! naming the fields it sets (`dir_fg = "brightblue"`), optionally on
//! top of a built-in with `base = "dark"` - so a skin can be three
//! lines rather than a full palette. mc's is its skin ini, read where
//! it lies: `-S xoterm` finds `/usr/share/mc/skins/xoterm.ini` and maps
//! its sections onto the same fields, which is policy 3 of MC-DIFF -
//! mc's files are imported, not adopted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The themes compiled in. Everything else is a file.
pub const BUILTIN: [&str; 3] = ["mc", "dark", "bw"];

/// Where a named theme can live, in the order a name is looked up:
/// rcmd's own directory first, then mc's skins wherever they are
/// installed.
pub fn dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(config) = crate::config::config_path()
        && let Some(parent) = config.parent()
    {
        dirs.push(parent.join("themes"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/share/mc/skins"));
    }
    dirs.push(PathBuf::from("/usr/local/share/mc/skins"));
    dirs.push(PathBuf::from("/usr/share/mc/skins"));
    dirs
}

/// Every theme that can be named: the three built in, then every file
/// in the search path by its stem. A name found twice is listed once -
/// the first directory that has it is the one that answers.
pub fn list() -> Vec<String> {
    let mut names: Vec<String> = BUILTIN.iter().map(|name| name.to_string()).collect();
    let mut found: Vec<String> = Vec::new();
    for dir in dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !matches!(ext, "toml" | "ini") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && !names.iter().any(|n| n == stem)
                && !found.iter().any(|n| n == stem)
            {
                found.push(stem.to_string());
            }
        }
    }
    found.sort();
    names.extend(found);
    names
}

/// The file a name refers to. A name with a separator or an extension
/// is taken as a path; anything else is looked up, `.toml` before
/// `.ini` in each directory in turn.
pub fn find(name: &str) -> Option<PathBuf> {
    if name.contains('/') || name.ends_with(".toml") || name.ends_with(".ini") {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }
    for dir in dirs() {
        for ext in ["toml", "ini"] {
            let path = dir.join(format!("{name}.{ext}"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// A theme file, read: the built-in it starts from, the fields it sets
/// by their rcmd names, and whatever could not be understood.
pub struct Loaded {
    pub base: String,
    pub fields: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

pub fn load(path: &Path) -> std::io::Result<Loaded> {
    let text = std::fs::read_to_string(path)?;
    Ok(match path.extension().and_then(|e| e.to_str()) {
        Some("ini") => from_mc_skin(&text),
        _ => from_toml(&text),
    })
}

fn from_toml(text: &str) -> Loaded {
    let mut loaded = Loaded {
        base: "mc".into(),
        fields: Vec::new(),
        warnings: Vec::new(),
    };
    let table: BTreeMap<String, toml::Value> = match toml::from_str(text) {
        Ok(table) => table,
        Err(err) => {
            let first = err.to_string().lines().next().unwrap_or("").to_string();
            loaded.warnings.push(first);
            return loaded;
        }
    };
    for (key, value) in table {
        let Some(text) = value.as_str() else {
            loaded
                .warnings
                .push(format!("'{key}' is not a colour name"));
            continue;
        };
        match key.as_str() {
            "base" => loaded.base = text.to_string(),
            // a description is for the person reading the file
            "description" | "name" => {}
            _ => loaded.fields.push((key, text.to_string())),
        }
    }
    loaded
}

/// Which mc skin entries land on which rcmd field. Everything mc has
/// that rcmd has nowhere to put (its line-drawing characters, the
/// editor's own palette) is simply not here: a skin is read for its
/// colours, and rcmd draws its own frames.
const MC_SKIN: &[(&str, &str, &str, &str)] = &[
    // section, key, foreground field, background field ("" = none)
    ("core", "_default_", "panel_fg", "panel_bg"),
    ("core", "selected", "select_fg", "select_bg"),
    ("core", "marked", "mark_fg", ""),
    ("core", "header", "header_fg", ""),
    ("core", "input", "prompt_fg", ""),
    ("dialog", "_default_", "dialog_fg", "dialog_bg"),
    ("dialog", "dhotnormal", "dialog_hot_fg", ""),
    ("menu", "_default_", "menu_fg", "menu_bg"),
    ("menu", "menuhot", "menu_hot_fg", ""),
    ("menu", "menusel", "menu_sel_fg", "menu_sel_bg"),
    ("error", "_default_", "error_fg", "error_bg"),
    ("help", "_default_", "help_fg", "help_bg"),
    ("help", "helpbold", "help_header_fg", ""),
    ("filehighlight", "directory", "dir_fg", ""),
    ("filehighlight", "executable", "exec_fg", ""),
    ("filehighlight", "stalelink", "broken_fg", ""),
    ("buttonbar", "button", "label_fg", "label_bg"),
    ("buttonbar", "hotkey", "key_fg", "key_bg"),
];

fn from_mc_skin(text: &str) -> Loaded {
    let mut entries: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_lowercase();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            entries.insert(
                (section.clone(), key.trim().to_lowercase()),
                value.trim().to_string(),
            );
        }
    }
    let mut loaded = Loaded {
        // an mc skin says every colour it means to say, and the ones it
        // leaves out it means to leave at the terminal's own
        base: "bw".into(),
        fields: Vec::new(),
        warnings: Vec::new(),
    };
    if entries.is_empty() {
        loaded.warnings.push("no colours in it".into());
        return loaded;
    }
    for (section, key, fg_field, bg_field) in MC_SKIN {
        let Some(value) = entries.get(&(section.to_string(), key.to_string())) else {
            continue;
        };
        // mc writes fg;bg;attributes, and any of the three can be empty
        // to mean "leave that one alone"
        let mut parts = value.split(';').map(str::trim);
        let fg = parts.next().unwrap_or("");
        let bg = parts.next().unwrap_or("");
        if !fg.is_empty() {
            loaded.fields.push((fg_field.to_string(), fg.to_string()));
        }
        if !bg.is_empty() && !bg_field.is_empty() {
            loaded.fields.push((bg_field.to_string(), bg.to_string()));
        }
    }
    loaded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_toml_theme_is_a_patch_over_a_base() {
        let loaded = from_toml("base = \"dark\"\ndir_fg = \"brightblue\"\n");
        assert_eq!(loaded.base, "dark");
        assert_eq!(loaded.fields, vec![("dir_fg".into(), "brightblue".into())]);
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn a_theme_that_is_not_toml_is_a_warning_not_a_crash() {
        let loaded = from_toml("dir_fg = ");
        assert!(!loaded.warnings.is_empty());
        assert!(loaded.fields.is_empty());
    }

    #[test]
    fn an_mc_skin_maps_onto_the_fields_rcmd_has() {
        let loaded = from_mc_skin(
            "[skin]\ndescription=Test\n\n[Lines]\nhoriz=-\n\n\
             [core]\n_default_=lightgray;blue\nselected=black;cyan\nmarked=yellow;\n\n\
             [filehighlight]\ndirectory=white;\nexecutable=brightgreen;\n\n\
             [buttonbar]\nbutton=black;cyan\nhotkey=white;cyan\n",
        );
        let fields: BTreeMap<_, _> = loaded.fields.into_iter().collect();
        assert_eq!(fields["panel_fg"], "lightgray");
        assert_eq!(fields["panel_bg"], "blue");
        assert_eq!(fields["select_fg"], "black");
        assert_eq!(fields["dir_fg"], "white");
        assert_eq!(fields["key_bg"], "cyan");
        // an empty half means "leave that one alone", not "black"
        assert_eq!(fields.get("mark_fg").map(String::as_str), Some("yellow"));
        assert!(!fields.contains_key("panel_bg_unused"));
        // the sections rcmd has nowhere to put are simply not read
        assert!(!fields.values().any(|v| v == "-"));
    }

    #[test]
    fn a_name_with_a_slash_is_a_path() {
        assert!(find("/nonexistent/theme.toml").is_none());
        assert!(list().contains(&"dark".to_string()));
    }
}
