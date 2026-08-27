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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore_other_dir: Option<bool>,
    /// Where the *other* panel was when rcmd last closed. The active
    /// panel starts where the shell is, as mc's does; this is the one
    /// mc keeps in panels.ini, and it is what makes the second panel
    /// worth something on the first keystroke.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other_dir: Option<String>,
    /// Files opened in the viewer or the editor, newest last. Far
    /// keeps this on Alt+F11 and it is the one history you want right
    /// after closing a screen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_history: Vec<String>,
    /// Where rcmd has been, and how often and how recently. The
    /// hotlist's recent half is ranked by this instead of by arrival
    /// order, so the directory you actually work in rises to the top
    /// and is still there next session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visits: Vec<Visit>,
}

/// One directory in the visit log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Visit {
    pub path: String,
    pub count: u32,
    /// Unix seconds of the last visit.
    pub last: i64,
}

/// How many directories the log keeps. Beyond this the lowest-scoring
/// go, which is the point of scoring them.
pub const VISITS: usize = 200;

/// zoxide's frecency, on the same four steps: a visit is worth more the
/// more recently it happened, so ten visits last year lose to three
/// this morning without either being forgotten.
pub fn frecency(visit: &Visit, now: i64) -> f64 {
    let age = (now - visit.last).max(0);
    let weight = if age < 3_600 {
        4.0
    } else if age < 86_400 {
        2.0
    } else if age < 604_800 {
        0.5
    } else {
        0.25
    };
    f64::from(visit.count) * weight
}

/// Fold one visit into a log: same path, one more visit; new path, a
/// new row. Used both while running and when merging this session's
/// log into whatever is on disk, which is why it takes a count.
pub fn merge_visit(log: &mut Vec<Visit>, path: &str, count: u32, last: i64) {
    match log.iter_mut().find(|v| v.path == path) {
        Some(visit) => {
            visit.count = visit.count.saturating_add(count);
            visit.last = visit.last.max(last);
        }
        None => log.push(Visit {
            path: path.to_string(),
            count,
            last,
        }),
    }
}

/// Drop the lowest-scoring rows once the log is over its cap.
pub fn trim_visits(log: &mut Vec<Visit>, now: i64) {
    if log.len() <= VISITS {
        return;
    }
    log.sort_by(|a, b| {
        frecency(b, now)
            .partial_cmp(&frecency(a, now))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    log.truncate(VISITS);
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
        restore_other_dir,
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
    fn frecency_prefers_the_recent_over_the_merely_frequent() {
        let now = 1_000_000i64;
        let today = Visit {
            path: "/today".into(),
            count: 3,
            last: now - 60,
        };
        let last_year = Visit {
            path: "/old".into(),
            count: 40,
            last: now - 400 * 86_400,
        };
        assert!(frecency(&today, now) > frecency(&last_year, now));
        // and neither is forgotten: forty visits still outrank one
        let once_last_year = Visit {
            path: "/once".into(),
            count: 1,
            last: now - 400 * 86_400,
        };
        assert!(frecency(&last_year, now) > frecency(&once_last_year, now));
        // a clock that went backwards is not a negative age
        let future = Visit {
            path: "/future".into(),
            count: 1,
            last: now + 10_000,
        };
        assert_eq!(frecency(&future, now), 4.0);
    }

    #[test]
    fn a_visit_folds_into_the_log_and_the_log_is_capped() {
        let now = 1_000_000i64;
        let mut log = Vec::new();
        merge_visit(&mut log, "/a", 1, now - 10);
        merge_visit(&mut log, "/a", 1, now);
        merge_visit(&mut log, "/b", 1, now);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].count, 2);
        assert_eq!(log[0].last, now, "the newer visit wins the timestamp");

        // over the cap, the lowest-scoring rows go
        for i in 0..VISITS {
            merge_visit(&mut log, &format!("/filler{i}"), 1, now - 400 * 86_400);
        }
        trim_visits(&mut log, now);
        assert_eq!(log.len(), VISITS);
        assert!(log.iter().any(|v| v.path == "/a"), "the busy one stayed");
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
}
