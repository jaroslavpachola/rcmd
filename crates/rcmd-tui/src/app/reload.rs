//! `config.toml` read again while rcmd runs: on a save in the editor
//! `edit-config` opened, and on `reload-config` for an edit made
//! anywhere else.
//!
//! Only what the file changed is applied. The running config is the
//! file, the state file on top, and whatever the command line said
//! (`-S dark`, `-d`); a key the edit did not touch keeps the value it
//! has now, so a reload never undoes a flag or a choice made in the
//! UI. What the file changed goes in, and whatever was built from it -
//! the keymaps, the sort groups, the theme - is built again.

use super::*;

impl App {
    /// `config.toml` again: the keys the edit changed go into the
    /// running config and take effect; the answer is what went wrong,
    /// or how much changed.
    pub fn reload_config(&mut self) -> String {
        let (fresh, warning) = config::load();
        let mut warnings: Vec<String> = warning.into_iter().collect();
        // a file that does not parse loads as the defaults; applying
        // those would undo the whole file over one typo
        if warnings.iter().any(|w| w.starts_with("config:")) {
            return warnings.join(" · ");
        }
        // what the edit changed, less what is running that way already
        // (a choice in the options form reaches the state file too)
        let differ = changed_keys(&self.config, &fresh);
        let changed: Vec<String> = changed_keys(&self.loaded, &fresh)
            .into_iter()
            .filter(|key| differ.contains(key))
            .collect();
        self.config = merge_keys(&self.config, &fresh, &changed);
        self.loaded = fresh;
        warnings.extend(self.config_changed(&changed));
        match (warnings.is_empty(), changed.len()) {
            (false, _) => warnings.join(" · "),
            (true, 0) => "config reloaded - nothing changed".into(),
            (true, 1..=3) => format!("config reloaded - {} changed", changed.join(", ")),
            (true, n) => format!("config reloaded - {n} settings changed"),
        }
    }

    /// `config.toml` was saved in the editor: read it again, and say
    /// how that went on the editor's note line.
    pub(super) fn reload_after_save(&mut self) {
        let said = self.reload_config();
        match self.editor_mut() {
            Some(st) if st.ed.path == config::config_path().unwrap_or_default() => {
                st.note = Some(format!(" {said} "));
            }
            // saved on the way out: the panels are what is on screen
            _ => self.status = Some(format!(" {said} ")),
        }
    }

    /// Build again what was built from the config at startup, for
    /// the keys that changed; the warnings the new values brought.
    fn config_changed(&mut self, changed: &[String]) -> Vec<String> {
        let now = self.config.clone();
        let touched = |keys: &[&str]| changed.iter().any(|c| keys.contains(&c.as_str()));
        let mut warnings = Vec::new();
        // the keys: the preset, [keys], the commands' own keys and
        // lynx motion all feed the one map, and the contexts beside it
        if touched(&["keymap", "keys", "lynx", "commands"]) {
            let (keymap, keymap_warnings) = full_keymap(&now);
            warnings.extend(keymap_warnings);
            self.keymap = keymap;
            let (contexts, _) = now.key_contexts();
            let (viewer, w1) = keymap::build_viewer(&contexts.viewer);
            let (editor, w2) = keymap::build_editor(&contexts.editor);
            let (dialog, w3) = keymap::build_dialog(&contexts.dialog);
            warnings.extend(w1.into_iter().chain(w2).chain(w3));
            (self.viewer_keys, self.editor_keys, self.dialog_keys) = (viewer, editor, dialog);
        }
        if touched(&["sort_group"]) {
            let (groups, group_warnings) = config::SortGroupRule::compile(&now.sort_group);
            warnings.extend(group_warnings);
            let groups = (!groups.is_empty()).then(|| Arc::new(groups));
            for panel in &mut self.panels {
                panel.sort_groups = groups.clone();
                panel.resort();
            }
        }
        if touched(&["listing_format"]) {
            let (format, format_warnings) = format::parse(&now.listing_format);
            warnings.extend(format_warnings);
            self.listing_format = format;
        }
        if touched(&["highlight"]) {
            warnings.extend(ui::init_highlight(&now.highlight));
        }
        if touched(&["theme"]) {
            warnings.extend(ui::init_theme(&now.theme));
        }
        if touched(&["syntax_theme"]) {
            warnings.extend(ui::set_syntax_theme(&now.syntax_theme));
        }
        if touched(&["time_format", "time_format_old", "si_units"]) {
            ui::set_formats(&now.time_format, &now.time_format_old, now.si_units);
        }
        if touched(&["edit_tab_size"]) {
            ui::set_tab_size(now.edit_tab_size as usize);
        }
        ui::set_editor_look(&now);
        if touched(&["mouse"]) {
            set_mouse_capture(now.mouse);
        }
        if touched(&["watch"]) {
            self.watch = match now.watch {
                true => {
                    let (watch, warning) = build_watch();
                    warnings.extend(warning);
                    watch
                }
                false => None,
            };
        }
        if touched(&["git"]) {
            self.git_info = [None, None];
            self.git_refresh();
        }
        if touched(&["show_hidden"]) {
            for panel in &mut self.panels {
                if panel.show_hidden != now.show_hidden
                    && let Err(err) = panel.toggle_hidden()
                {
                    warnings.push(err.to_string());
                }
            }
        }
        // the subshell is a process: switched off it goes, switched on
        // it waits for the next start, as the options form has it
        if touched(&["subshell"]) && !now.subshell {
            self.subshell = None;
        }
        self.repaint = true;
        warnings
    }
}

/// The top-level keys whose value differs between two configs, in
/// name order.
fn changed_keys(before: &Config, after: &Config) -> Vec<String> {
    let (Some(before), Some(after)) = (table_of(before), table_of(after)) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = after
        .iter()
        .filter(|(key, value)| before.get(*key) != Some(*value))
        .map(|(key, _)| key.clone())
        .collect();
    // a key the new file leaves out entirely is a change too
    keys.extend(
        before
            .keys()
            .filter(|key| !after.contains_key(*key))
            .cloned(),
    );
    keys
}

/// `current` with the `keys` taken from `fresh`: the one change the
/// edit made, and nothing else.
fn merge_keys(current: &Config, fresh: &Config, keys: &[String]) -> Config {
    let (Some(mut table), Some(new)) = (table_of(current), table_of(fresh)) else {
        return current.clone();
    };
    for key in keys {
        match new.get(key) {
            Some(value) => table.insert(key.clone(), value.clone()),
            None => table.remove(key),
        };
    }
    toml::Value::Table(table)
        .try_into()
        .unwrap_or_else(|_| current.clone())
}

fn table_of(config: &Config) -> Option<toml::Table> {
    match toml::Value::try_from(config).ok()? {
        toml::Value::Table(table) => Some(table),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_the_edit_changed_goes_in() {
        let loaded = Config::default();
        // the file now says a theme and a sort group
        let fresh = Config {
            theme: "dark".into(),
            ..toml::from_str::<Config>("[[sort_group]]\nmatch = \"@pictures\"\n").unwrap()
        };
        // and the running config has a flag of its own (-d: no mouse)
        let running = Config {
            mouse: !Config::default().mouse,
            ..Config::default()
        };
        let changed = changed_keys(&loaded, &fresh);
        assert_eq!(changed, ["sort_group", "theme"]);
        let merged = merge_keys(&running, &fresh, &changed);
        assert_eq!(merged.theme, "dark");
        assert_eq!(merged.sort_group.len(), 1);
        assert_eq!(merged.mouse, running.mouse, "the flag stands");
        // a key the file drops goes back to what the file now says
        let changed = changed_keys(&fresh, &loaded);
        let merged = merge_keys(&merged, &loaded, &changed);
        assert_eq!(merged.theme, Config::default().theme);
        assert!(merged.sort_group.is_empty());
    }
}
