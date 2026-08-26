//! `~/.config/rcmd/config.toml` - the user's file, and **read-only**
//! from rcmd's side: comments and hand formatting survive because
//! nothing here ever writes it back. Everything rcmd changes at runtime
//! (panel state, hotlist, options-form toggles) lives in
//! [`crate::state`] and is overlaid on top at load time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rcmd_core::glob::glob_match;
use rcmd_core::panel::{ListMode, SortKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "mc" (classic blue), "dark" (truecolor) or "bw" (no colour at
    /// all, mc's `-b`). Applied at startup.
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
    /// Panel listing format: "brief" | "full" | "long" | "tree" |
    /// "user" (the last one draws `listing_format`).
    pub listing: String,
    /// MC's user-defined listing format, drawn when `listing = "user"`:
    /// a panel size, an optional repeat count and then field names.
    /// See [`crate::format`].
    pub listing_format: String,
    /// Auto-reload panels when their directory changes on disk.
    pub watch: bool,
    /// Mouse support (click, double-click, wheel). Additive only -
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
    /// Columns of names in the brief listing (MC shows two). 1 keeps
    /// the pre-4.0 single full-width column.
    pub brief_columns: u16,
    /// Panel split direction: "vertical" (side by side, the default) or
    /// "horizontal" (one above the other), as in MC's Layout dialog.
    pub split: String,
    /// Percentage of the window given to the left / top panel, 20..80.
    pub split_ratio: u16,
    /// Draw MC's permanent menu bar across the top (F9 opens the menu
    /// either way).
    pub show_menubar: bool,
    /// Draw a status row inside each panel describing that panel's
    /// cursor entry (MC's "mini status"). Off by default: rcmd's single
    /// status line already covers the active panel, so this earns its
    /// row mainly by showing the *other* panel's entry too.
    pub show_mini_status: bool,
    /// Draw the filesystem's free space in a local panel's footer and
    /// the info panel (MC's `free_space`). The marked-files total stays
    /// either way: it is only there while something is marked.
    pub show_free_space: bool,
    /// Draw the status line showing the cursor entry.
    pub show_status: bool,
    /// Draw the command line. With it hidden, plain characters only
    /// trigger key bindings - there is nowhere for them to be typed.
    pub show_cmdline: bool,
    /// Draw the F1..F10 key bar along the bottom.
    pub show_keybar: bool,
    /// Ask before deleting (F8 / Shift+F8).
    pub confirm_delete: bool,
    /// Ask before overwriting an existing file during copy/move; false
    /// answers "overwrite all" for every job.
    pub confirm_overwrite: bool,
    /// Ask before quitting (F10). MC asks by default; rcmd does not.
    pub confirm_exit: bool,
    /// Ask before dropping an entry from the directory hotlist. There
    /// is no undo for that list, so this one starts on.
    pub confirm_hotlist_delete: bool,
    /// Ask before Enter runs an `[[open]]` command. MC's "confirm
    /// execute", and off for the same reason: you configured the rule.
    pub confirm_execute: bool,
    /// How long a lone Esc waits for its follow-up key before acting as
    /// a plain Escape (MC's meta prefix: Esc 1..0 = F1..F10, Esc x =
    /// Alt+X). Raise it towards MC's 1000 when typing those by hand.
    pub esc_timeout_ms: u64,
    /// Built-in editor, mc's editor options: columns between tab
    /// stops - what a tab is worth on screen and how far one Tab key
    /// gets you when tabs are filled with spaces.
    pub edit_tab_size: u16,
    /// Tab inserts spaces up to the next stop instead of a tab.
    pub edit_fill_tabs: bool,
    /// Enter copies the current line's leading whitespace.
    pub edit_auto_indent: bool,
    /// Inside leading whitespace, Backspace takes the whole tab stop.
    pub edit_backspace_tabs: bool,
    /// Column the editor's soft wrap (Alt+W) folds at; 0 = the window
    /// width, which is mc's "dynamic" wrap.
    pub edit_wrap_column: u16,
    /// Find file shows its matches in mc's results window (Chdir,
    /// Again, Panelize, View, Edit). false = the pre-4.0 behaviour,
    /// where they stream straight into the panel as a panelized
    /// listing - which is one keystroke shorter and loses the list.
    pub find_window: bool,
    /// Enter on a file no `[[open]]` rule claims hands it to the desktop
    /// - `xdg-open`, or `open` on macOS - when there is a display to
    /// open it on. Off: such an Enter does nothing, as before 4.9.
    pub desktop_open: bool,
    /// Draw the line-number gutter (Alt+N toggles it).
    pub edit_line_numbers: bool,
    /// Keep the previous contents as `file~` on every save.
    pub edit_backups: bool,
    /// Copy and cut also reach the desktop clipboard, and paste reads
    /// it - through wl-copy / xclip / xsel / pbcopy, whichever is
    /// there. Off = the editor's own clipboard only.
    pub edit_clipboard: bool,
    /// Custom bindings on top of the presets. Bare entries under
    /// `[keys]` bind in the panel, and `[keys.panel|viewer|editor]`
    /// sub-tables bind in that context:
    ///
    /// ```toml
    /// [keys]
    /// "ctrl+y" = "swap-panels"     # panel, as before
    /// [keys.viewer]
    /// "ctrl+w" = "wrap"
    /// ```
    pub keys: BTreeMap<String, toml::Value>,
    pub hotlist: Vec<HotEntry>,
    /// Openers consulted by Enter on a file, in file order - the first
    /// matching glob wins.
    pub open: Vec<OpenRule>,
    /// View filters consulted by F3: the command's stdout is shown in
    /// the internal viewer (`view = "pdftotext %f -"`). Shift+F3 views
    /// the raw bytes. Same shape and matching as `[[open]]`.
    pub view: Vec<OpenRule>,
    /// User commands: the F2 menu, in file order.
    pub commands: Vec<UserCommand>,
    /// Saved panelize commands, in file order.
    pub panelize: Vec<PanelizePreset>,
    /// Per-name / per-type colour rules, in file order - the first
    /// matching one wins. MC's filehighlight, as TOML.
    pub highlight: Vec<HighlightRule>,
}

/// One row of the directory hotlist. mc's hotlist is a tree, so this is
/// one too: an entry with a `path` is a place to go, an entry with
/// `entries` is a group to walk into. An entry with neither is an empty
/// group - which is what a group is before anything is put in it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HotEntry {
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<HotEntry>,
}

impl HotEntry {
    pub fn is_group(&self) -> bool {
        self.path.is_empty()
    }

    /// Whether `path` is anywhere in this subtree - what "already in
    /// the hotlist" means once the hotlist has depth.
    pub fn holds(entries: &[HotEntry], path: &str) -> bool {
        entries
            .iter()
            .any(|e| e.path == path || HotEntry::holds(&e.entries, path))
    }
}

/// `[[panelize]]` - a named command whose output becomes a listing.
/// MC keeps these in its own dialog and its own file; rcmd keeps them
/// beside every other list you can name, and the ones you save while
/// running go to the state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelizePreset {
    pub name: String,
    pub run: String,
}

/// `[[open]]` / `[[view]]` - mc.ext's four matchers and a command.
/// What the file is called (`match`, a glob, or `regex`), what it is
/// (`type`, a regex over what `file -b` says of it) and where it is
/// (`directory`, a regex over the panel's path); every matcher given
/// must hold, and a rule with none never matches.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenRule {
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    pub run: String,
}

impl OpenRule {
    pub fn by_glob(pattern: &str, run: &str) -> Self {
        OpenRule {
            pattern: Some(pattern.to_string()),
            run: run.to_string(),
            ..OpenRule::default()
        }
    }

    /// Whether the rule applies to `name` in `dir`. The glob is
    /// case-insensitive, the regexes are what they say (`(?i)` is
    /// there). `file_type` is asked for `file -b`'s line only when a
    /// rule wants it, so the usual rules cost no process.
    pub fn matches(&self, name: &str, dir: &Path, file_type: &mut dyn FnMut() -> String) -> bool {
        if self.pattern.is_none()
            && self.regex.is_none()
            && self.kind.is_none()
            && self.directory.is_none()
        {
            return false;
        }
        if let Some(glob) = &self.pattern
            && !glob_match(&glob.to_lowercase(), &name.to_lowercase())
        {
            return false;
        }
        if let Some(re) = &self.regex
            && !regex_matches(re, name)
        {
            return false;
        }
        if let Some(re) = &self.directory
            && !regex_matches(re, &dir.to_string_lossy())
        {
            return false;
        }
        if let Some(re) = &self.kind
            && !regex_matches(re, &file_type())
        {
            return false;
        }
        true
    }
}

/// A regex that does not compile matches nothing - a rule is not the
/// place to fail loudly, Enter is pressed a hundred times a day.
fn regex_matches(re: &str, text: &str) -> bool {
    regex::Regex::new(re).is_ok_and(|re| re.is_match(text))
}

/// What `file -b` says about a path, for `type =` rules; empty when
/// there is no `file` to ask.
/// The desktop's own opener, if this session has a desktop: `open` on
/// macOS, `xdg-open` under X or Wayland. `None` over a bare ssh, where
/// spawning a browser would happen on the wrong machine, if at all.
pub fn desktop_opener() -> Option<&'static str> {
    let candidate = if cfg!(target_os = "macos") {
        "open"
    } else if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    {
        "xdg-open"
    } else {
        return None;
    };
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(candidate).is_file()));
    on_path.then_some(candidate)
}

pub fn file_type_of(path: &Path) -> String {
    std::process::Command::new("file")
        .args(["-b", "--"])
        .arg(path)
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// `[[highlight]]` - `match = "*.tar.gz"` or `type = "exe"`, plus the
/// colour to draw such entries in (and optionally `bold`). MC keeps the
/// groups in `filehighlight.ini` and their colours in the skin; rcmd
/// puts both in one rule, because splitting them over two files only
/// ever made sense when the skin was shipped separately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HighlightRule {
    /// Glob on the entry name; mutually exclusive with `type`.
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// One of `dir exe link linkdir broken file` - what the entry *is*,
    /// where `match` says what it is called.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub color: String,
    /// `None` keeps whatever the entry kind draws by default, so a rule
    /// that only sets a colour does not quietly un-bold directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
}

/// `[[commands]]` - a named shell template with `%f %d %D %t` macros,
/// shown in the F2 menu; `key` optionally binds it directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// mc's user-menu condition: the entry is only offered when it
    /// holds. See [`rcmd_core::usermenu`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// A submenu. mc's `menu` file is flat; rcmd's TOML is not, because
    /// a menu of thirty entries wants sections and TOML can say so.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<UserCommand>,
}

impl UserCommand {
    pub fn is_submenu(&self) -> bool {
        !self.entries.is_empty()
    }
}

/// `[keys]` split by context, with anything unparsable reported.
#[derive(Debug, Default, Clone)]
pub struct KeyContexts {
    pub panel: BTreeMap<String, String>,
    pub viewer: BTreeMap<String, String>,
    pub editor: BTreeMap<String, String>,
    pub dialog: BTreeMap<String, String>,
}

impl Config {
    /// True when the panels are stacked instead of side by side.
    pub fn horizontal_split(&self) -> bool {
        self.split == "horizontal"
    }

    /// Brief-listing columns, clamped to what fits sensibly.
    pub fn columns(&self) -> u16 {
        self.brief_columns.clamp(1, 6)
    }

    /// The split percentage, clamped to something usable.
    pub fn ratio(&self) -> u16 {
        self.split_ratio.clamp(20, 80)
    }

    /// Sort the raw `[keys]` table into per-context maps. A string value
    /// is a panel binding (the pre-4.0 shape); a sub-table is a context.
    /// Unknown contexts and odd value types become warnings rather than
    /// a refusal to start.
    pub fn key_contexts(&self) -> (KeyContexts, Vec<String>) {
        let mut out = KeyContexts::default();
        let mut warnings = Vec::new();
        for (key, value) in &self.keys {
            match value {
                toml::Value::String(action) => {
                    out.panel.insert(key.clone(), action.clone());
                }
                toml::Value::Table(table) => {
                    let target = match key.as_str() {
                        "panel" => &mut out.panel,
                        "viewer" => &mut out.viewer,
                        "editor" => &mut out.editor,
                        "dialog" => &mut out.dialog,
                        other => {
                            warnings.push(format!(
                                "unknown key context '[keys.{other}]' \
                                 (panel, viewer, editor, dialog)"
                            ));
                            continue;
                        }
                    };
                    for (k, v) in table {
                        match v.as_str() {
                            Some(action) => {
                                target.insert(k.clone(), action.to_string());
                            }
                            None => {
                                warnings.push(format!("[keys.{key}] '{k}' must be an action name"))
                            }
                        }
                    }
                }
                _ => warnings.push(format!("[keys] '{key}' must be an action name or a table")),
            }
        }
        (out, warnings)
    }

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
            listing_format: crate::format::DEFAULT.into(),
            watch: true,
            mouse: true,
            git: false,
            editor: "internal".into(),
            subshell: true,
            brief_columns: 2,
            split: "vertical".into(),
            split_ratio: 50,
            show_menubar: false,
            show_mini_status: false,
            show_free_space: true,
            show_status: true,
            show_cmdline: true,
            show_keybar: true,
            confirm_delete: true,
            confirm_overwrite: true,
            confirm_exit: false,
            confirm_hotlist_delete: true,
            confirm_execute: false,
            esc_timeout_ms: crate::app::ESC_TIMEOUT_MS,
            edit_tab_size: 8,
            edit_fill_tabs: false,
            edit_auto_indent: true,
            edit_backspace_tabs: false,
            edit_wrap_column: 0,
            find_window: true,
            desktop_open: true,
            edit_line_numbers: false,
            edit_backups: false,
            edit_clipboard: true,
            keys: BTreeMap::new(),
            hotlist: Vec::new(),
            open: Vec::new(),
            view: Vec::new(),
            commands: Vec::new(),
            panelize: Vec::new(),
            highlight: Vec::new(),
        }
    }
}

impl Config {
    /// The editor options as the editor core takes them.
    pub fn edit_prefs(&self) -> rcmd_edit::Prefs {
        rcmd_edit::Prefs {
            tab_size: (self.edit_tab_size as usize).clamp(1, 16),
            fill_tabs: self.edit_fill_tabs,
            auto_indent: self.edit_auto_indent,
            backspace_tabs: self.edit_backspace_tabs,
            backup: self.edit_backups,
        }
    }
}

pub fn config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("rcmd/config.toml"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/rcmd/config.toml"))
}

/// The effective configuration: the user's file with [`crate::state`]
/// overlaid on top. Missing file → defaults; unparsable file → defaults
/// plus a warning (never refuse to start over a config typo).
pub fn load() -> (Config, Option<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let text = config_path().and_then(|path| std::fs::read_to_string(&path).ok());
    let mut config = match &text {
        Some(text) => match toml::from_str(text) {
            Ok(config) => config,
            Err(err) => {
                let first = err
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                warnings.push(format!("config: {first}"));
                Config::default()
            }
        },
        None => Config::default(),
    };

    let (state, state_warning) = crate::state::load();
    warnings.extend(state_warning);
    crate::state::apply(&state, &mut config);

    (config, (!warnings.is_empty()).then(|| warnings.join(" · ")))
}

pub fn list_mode_from_name(name: &str) -> ListMode {
    match name {
        "brief" => ListMode::Brief,
        "long" => ListMode::Long,
        "tree" => ListMode::Tree,
        "user" => ListMode::User,
        _ => ListMode::Full,
    }
}

pub fn list_mode_name(mode: ListMode) -> &'static str {
    match mode {
        ListMode::Brief => "brief",
        ListMode::Full => "full",
        ListMode::Long => "long",
        ListMode::Tree => "tree",
        ListMode::User => "user",
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
    #[test]
    fn open_rules_match_by_name_type_and_place() {
        use super::OpenRule;
        use std::path::Path;
        let dir = Path::new("/srv/www");
        let asked = std::cell::Cell::new(0);
        let mut probe = || {
            asked.set(asked.get() + 1);
            "ELF 64-bit LSB executable".to_string()
        };
        let glob = OpenRule::by_glob("*.LOG", "");
        assert!(glob.matches("today.log", dir, &mut probe));
        let re = OpenRule {
            regex: Some("^[a-z]+[0-9]+\\.log$".into()),
            ..OpenRule::default()
        };
        assert!(re.matches("app7.log", dir, &mut probe));
        assert!(!re.matches("APP7.log", dir, &mut probe));
        let place = OpenRule {
            regex: Some("\\.log$".into()),
            directory: Some("^/srv/".into()),
            ..OpenRule::default()
        };
        assert!(place.matches("a.log", dir, &mut probe));
        assert!(!place.matches("a.log", Path::new("/home/x"), &mut probe));
        // nothing so far needed `file`
        assert_eq!(asked.get(), 0);
        let kind = OpenRule {
            kind: Some("^ELF".into()),
            ..OpenRule::default()
        };
        assert!(kind.matches("a.out", dir, &mut probe));
        assert_eq!(asked.get(), 1);
        // a rule that says nothing matches nothing, and a broken regex
        // matches nothing rather than everything
        assert!(!OpenRule::default().matches("x", dir, &mut probe));
        let broken = OpenRule {
            regex: Some("(".into()),
            ..OpenRule::default()
        };
        assert!(!broken.matches("(", dir, &mut probe));
    }

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
        let (contexts, warnings) = config.key_contexts();
        assert!(warnings.is_empty());
        assert_eq!(contexts.panel["ctrl+y"], "swap-panels");
        assert_eq!(contexts.panel["ctrl+\\"], "hotlist");
        assert_eq!(config.hotlist[0].label, "projects");

        let out = toml::to_string_pretty(&config).unwrap();
        let back: Config = toml::from_str(&out).unwrap();
        assert_eq!(back.keys.len(), 2);
        assert_eq!(back.hotlist.len(), 1);
        assert_eq!(back.sort_key, "mtime");
    }

    #[test]
    fn key_contexts_split_by_table() {
        let text = r#"
[keys]
"ctrl+y" = "swap-panels"

[keys.viewer]
"ctrl+w" = "wrap"

[keys.editor]
"ctrl+q" = "quit"

[keys.bogus]
"x" = "y"
"#;
        let config: Config = toml::from_str(text).unwrap();
        let (contexts, warnings) = config.key_contexts();
        assert_eq!(contexts.panel["ctrl+y"], "swap-panels");
        assert_eq!(contexts.viewer["ctrl+w"], "wrap");
        assert_eq!(contexts.editor["ctrl+q"], "quit");
        assert_eq!(warnings.len(), 1, "the unknown context warns: {warnings:?}");
        assert!(warnings[0].contains("bogus"));
    }

    #[test]
    fn key_contexts_warn_on_odd_values() {
        let config: Config = toml::from_str("[keys]\n'ctrl+y' = 42\n").unwrap();
        let (contexts, warnings) = config.key_contexts();
        assert!(contexts.panel.is_empty());
        assert_eq!(warnings.len(), 1);
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
        assert_eq!(config.open[0].pattern.as_deref(), Some("*.pdf"));
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
