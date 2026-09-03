//! The window's own settings: the font and its size.
//!
//! `config.toml` is the user's and read-only, as it is for the terminal
//! build; the window reads a `[window]` table of it that `rcmd_tui`'s
//! `Config` does not know and so ignores. What the Font dialog and the
//! Ctrl+= / Ctrl+- keys change goes to `window.toml` beside rcmd's
//! `state.toml`, sparse and overlaid at load, the same layering as
//! the rest of rcmd: defaults < `config.toml` < the state file <
//! `$RCMD_EGUI_FONT` and `--font-size` for the one session.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Big enough to read on a HiDPI screen, small enough for two panels.
pub const DEFAULT_FONT_SIZE: f32 = 14.0;

/// Every field optional: `None` = "not set here", so the layer below
/// decides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Window {
    /// A family name ("DejaVu Sans Mono") looked up among the system
    /// fonts, or a path to a `.ttf`/`.otf`. Empty means the built-in
    /// search: DejaVu, Liberation, Menlo, Consolas, whichever is there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    /// Point size of the grid font.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
}

impl Window {
    /// This layer with `over` on top: whatever `over` sets wins.
    pub fn overlaid(&self, over: &Window) -> Window {
        Window {
            font: over.font.clone().or_else(|| self.font.clone()),
            font_size: over.font_size.or(self.font_size),
        }
    }

    pub fn size(&self) -> f32 {
        self.font_size.unwrap_or(DEFAULT_FONT_SIZE)
    }
}

/// The `[window]` table of `config.toml`, the rest of the file skipped.
#[derive(Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    window: Window,
}

/// `[window]` from the config file, with the state file's values on
/// top. Neither being there is fine; either being unreadable is a
/// warning, never a refusal to start.
pub fn load() -> (Window, Window, Vec<String>) {
    let mut warnings = Vec::new();
    let config =
        match rcmd_tui::config::config_path().and_then(|path| std::fs::read_to_string(path).ok()) {
            Some(text) => match parse_config(&text) {
                Ok(window) => window,
                Err(err) => {
                    warnings.push(format!("config [window]: {err}"));
                    Window::default()
                }
            },
            None => Window::default(),
        };
    let state = match state_path().and_then(|path| std::fs::read_to_string(path).ok()) {
        Some(text) => match toml::from_str::<Window>(&text) {
            Ok(window) => window,
            Err(err) => {
                let first = err.to_string().lines().next().unwrap_or("").to_string();
                warnings.push(format!("window state: {first}"));
                Window::default()
            }
        },
        None => Window::default(),
    };
    (config, state, warnings)
}

fn parse_config(text: &str) -> Result<Window, String> {
    toml::from_str::<ConfigFile>(text)
        .map(|file| file.window)
        .map_err(|err| err.to_string().lines().next().unwrap_or("").to_string())
}

/// `window.toml` beside rcmd's `state.toml`.
pub fn state_path() -> Option<PathBuf> {
    rcmd_tui::state::state_path().map(|path| path.with_file_name("window.toml"))
}

/// Write the state file: read-modify-write against what is on disk, so
/// a second window's save is not lost under this one's.
pub fn save(f: impl FnOnce(&mut Window)) -> Result<()> {
    let Some(path) = state_path() else {
        return Ok(());
    };
    let mut state: Window = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default();
    f(&mut state);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string_pretty(&state)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_table_is_read_and_the_rest_of_the_file_is_not_its_business() {
        let text = "theme = \"dark\"\nshow_hidden = true\n\n[window]\nfont = \"DejaVu Sans Mono\"\nfont_size = 16\n\n[[hotlist]]\nname = \"x\"\npath = \"/\"\n";
        let window = parse_config(text).unwrap();
        assert_eq!(window.font.as_deref(), Some("DejaVu Sans Mono"));
        assert_eq!(window.size(), 16.0);
        // no table at all is the default, not an error
        assert_eq!(parse_config("theme = \"mc\"\n").unwrap(), Window::default());
        assert!(parse_config("[window]\nfont_size = \"big\"\n").is_err());
    }

    #[test]
    fn the_state_wins_only_where_it_says_something() {
        let config = Window {
            font: Some("Liberation Mono".into()),
            font_size: Some(12.0),
        };
        let state = Window {
            font: None,
            font_size: Some(18.0),
        };
        let effective = config.overlaid(&state);
        assert_eq!(effective.font.as_deref(), Some("Liberation Mono"));
        assert_eq!(effective.size(), 18.0);
        assert_eq!(Window::default().size(), DEFAULT_FONT_SIZE);
    }

    #[test]
    fn the_state_file_is_sparse() {
        let state = Window {
            font: None,
            font_size: Some(15.0),
        };
        let text = toml::to_string_pretty(&state).unwrap();
        assert!(!text.contains("font ="), "{text}");
        assert!(text.contains("font_size = 15"), "{text}");
    }
}
