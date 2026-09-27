//! Syntax highlighting (feature `syntax`): syntect with parse-state
//! checkpoints every [`CHECKPOINT`] lines, so edits only re-highlight
//! from the edited line down and scrolling never re-parses from the top.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use syntect::highlighting::{
    HighlightState, Highlighter as SynHl, RangedHighlightIterator, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

use crate::LineSource;

const CHECKPOINT: usize = 32;
/// Highlighting is skipped entirely above these limits.
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_LINE_CHARS: usize = 2000;

/// Where a user's own `.sublime-syntax` files are looked for. Set once,
/// before anything highlights - the set is built on first use and then
/// never again.
static USER_SYNTAX: std::sync::RwLock<Option<std::path::PathBuf>> = std::sync::RwLock::new(None);
/// What went wrong loading them, if anything, for the caller to report.
static USER_SYNTAX_WARNING: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Point the highlighter at a directory of user syntax files. They are
/// `.sublime-syntax` definitions, which is what syntect speaks and what
/// is actually downloadable; mc's own syntax format is a different
/// language and is not read.
pub fn set_user_syntax_dir(dir: std::path::PathBuf) {
    *USER_SYNTAX.write().unwrap_or_else(|e| e.into_inner()) = Some(dir);
}

/// A warning from loading them, once the set has been built. None until
/// something has been highlighted.
pub fn user_syntax_warning() -> Option<String> {
    USER_SYNTAX_WARNING
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(|| {
        let defaults = SyntaxSet::load_defaults_newlines();
        let dir = USER_SYNTAX
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(dir) = dir.filter(|dir| dir.is_dir()) else {
            return defaults;
        };
        let mut builder = defaults.into_builder();
        // a broken syntax file costs its own file and a warning, never
        // the highlighting of everything else
        if let Err(err) = builder.add_from_folder(&dir, true) {
            *USER_SYNTAX_WARNING
                .write()
                .unwrap_or_else(|e| e.into_inner()) =
                Some(format!("syntax: {}: {err}", dir.display()));
        }
        builder.build()
    })
}

fn builtin_themes() -> &'static ThemeSet {
    static TS: OnceLock<ThemeSet> = OnceLock::new();
    TS.get_or_init(ThemeSet::load_defaults)
}

/// The colours syntax is drawn in, when not the ones it starts with.
enum Chosen {
    Default,
    Off,
    Theme(Arc<Theme>),
}

static CHOSEN: std::sync::RwLock<Chosen> = std::sync::RwLock::new(Chosen::Default);

/// The theme syntax is coloured by now; `None` when it is not coloured.
fn theme() -> Option<Arc<Theme>> {
    static DEFAULT: OnceLock<Arc<Theme>> = OnceLock::new();
    match &*CHOSEN.read().unwrap_or_else(|e| e.into_inner()) {
        Chosen::Default => Some(
            DEFAULT
                .get_or_init(|| Arc::new(builtin_themes().themes[DEFAULT_THEME].clone()))
                .clone(),
        ),
        Chosen::Off => None,
        Chosen::Theme(theme) => Some(theme.clone()),
    }
}

/// The syntax theme until another is chosen: a dark one, which is what
/// a file manager's blue or black panel is.
const DEFAULT_THEME: &str = "base16-eighties.dark";

/// Colour syntax by the named theme - one syntect has built in (see
/// [`syntax_theme_names`]), a `.tmTheme` file by its path, or one by
/// its stem in the user syntax directory - or not at all, with `None`.
/// A theme that cannot be loaded leaves the one before in place and
/// says why. Highlighters already open take it up on their next frame.
pub fn set_syntax_theme(name: Option<&str>) -> Result<(), String> {
    let chosen = match name {
        None => Chosen::Off,
        Some(name) => Chosen::Theme(Arc::new(load_theme(name)?)),
    };
    *CHOSEN.write().unwrap_or_else(|e| e.into_inner()) = chosen;
    Ok(())
}

fn load_theme(name: &str) -> Result<Theme, String> {
    if let Some(theme) = builtin_themes().themes.get(name) {
        return Ok(theme.clone());
    }
    let path = match name.contains('/') || name.ends_with(".tmTheme") {
        true => std::path::PathBuf::from(name),
        false => {
            let dir = USER_SYNTAX
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            match dir.map(|dir| dir.join(format!("{name}.tmTheme"))) {
                Some(path) if path.is_file() => path,
                _ => {
                    return Err(format!(
                        "unknown syntax theme '{name}' (built in: {})",
                        syntax_theme_names().join(", ")
                    ));
                }
            }
        }
    };
    ThemeSet::get_theme(&path).map_err(|err| format!("{}: {err}", path.display()))
}

/// The syntax themes syntect has built in, by name.
pub fn syntax_theme_names() -> Vec<&'static str> {
    builtin_themes().themes.keys().map(String::as_str).collect()
}

/// Every syntax syntect knows, by name and in order, for a picker.
pub fn syntax_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = syntax_set()
        .syntaxes()
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

pub struct Highlighter {
    syntax: &'static SyntaxReference,
    /// `states[k]` = parser/highlight state *before* line `k * CHECKPOINT`.
    states: Vec<(ParseState, HighlightState)>,
    /// Everything from this line on must be re-derived after an edit.
    dirty_from: usize,
    broken: bool,
    /// The theme `states` were worked out in: another one since means
    /// working them out again.
    theme: Option<Arc<Theme>>,
}

impl Highlighter {
    /// One for a named syntax, whatever the file happens to be called -
    /// which is the point of being asked rather than guessing from the
    /// extension.
    pub fn by_name(name: &str) -> Option<Highlighter> {
        let syntax = syntax_set().find_syntax_by_name(name)?;
        Some(Highlighter {
            syntax,
            states: Vec::new(),
            dirty_from: 0,
            broken: false,
            theme: None,
        })
    }

    /// What it is highlighting as.
    pub fn syntax_name(&self) -> &'static str {
        &self.syntax.name
    }

    /// None when the file is too big, has no known syntax, or is plain
    /// text - callers then render plain, which is also the fast path.
    pub fn new(path: &Path, len_bytes: usize) -> Option<Highlighter> {
        if len_bytes > MAX_BYTES {
            return None;
        }
        let ss = syntax_set();
        let ext = path.extension()?.to_str()?;
        let syntax = ss
            .find_syntax_by_extension(ext)
            .or_else(|| ss.find_syntax_by_extension(path.file_name()?.to_str()?))?;
        if syntax.name == "Plain Text" {
            return None;
        }
        Some(Highlighter {
            syntax,
            states: Vec::new(),
            dirty_from: 0,
            broken: false,
            theme: None,
        })
    }

    pub fn invalidate_from(&mut self, line: usize) {
        self.dirty_from = self.dirty_from.min(line);
    }

    fn initial(&self, hl: &SynHl) -> (ParseState, HighlightState) {
        (
            ParseState::new(self.syntax),
            HighlightState::new(hl, ScopeStack::new()),
        )
    }

    /// Foreground-color spans (as char ranges) for `count` lines starting
    /// at `start` - one call per frame, one state replay per call. An
    /// empty inner vec means "render that line plain".
    pub fn range_spans<S: LineSource>(
        &mut self,
        src: &mut S,
        start: usize,
        count: usize,
    ) -> Vec<Vec<(usize, usize, [u8; 3])>> {
        let plain = vec![Vec::new(); count];
        if self.broken {
            return plain;
        }
        let Some(theme) = theme() else {
            return plain;
        };
        if !self
            .theme
            .as_ref()
            .is_some_and(|had| Arc::ptr_eq(had, &theme))
        {
            self.states.clear();
            self.theme = Some(theme.clone());
        }
        let hl = SynHl::new(&theme);
        // text the theme leaves in its plain colour is drawn in the
        // screen's own: the theme's is picked for its own background,
        // which the screen behind it need not be
        let plain_fg = theme.settings.foreground;
        // drop checkpoints past the first edited line
        let keep = self.dirty_from.div_ceil(CHECKPOINT).min(self.states.len());
        self.states.truncate(keep);
        self.dirty_from = self.states.len() * CHECKPOINT;
        if self.states.is_empty() {
            self.states.push(self.initial(&hl));
            self.dirty_from = 0;
        }

        // advance checkpoints until the one covering `start` exists
        let want = start / CHECKPOINT;
        while self.states.len() <= want {
            let (mut ps, mut hs) = self.states.last().expect("seeded above").clone();
            let from = (self.states.len() - 1) * CHECKPOINT;
            for i in from..from + CHECKPOINT {
                if i >= src.line_count() {
                    break;
                }
                if !self.advance(&mut ps, &mut hs, &hl, &src.line_with_nl(i)) {
                    return plain;
                }
            }
            self.states.push((ps, hs));
        }
        self.dirty_from = self.dirty_from.max(self.states.len() * CHECKPOINT);

        let (mut ps, mut hs) = self.states[want].clone();
        for i in want * CHECKPOINT..start {
            if !self.advance(&mut ps, &mut hs, &hl, &src.line_with_nl(i)) {
                return plain;
            }
        }
        let mut out = Vec::with_capacity(count);
        for line in start..start + count {
            if line >= src.line_count() {
                out.push(Vec::new());
                continue;
            }
            let text = src.line_with_nl(line);
            if text.chars().count() > MAX_LINE_CHARS {
                out.push(Vec::new());
                continue;
            }
            let Ok(ops) = ps.parse_line(&text, syntax_set()) else {
                self.broken = true;
                out.resize(count, Vec::new());
                return out;
            };
            let mut spans = Vec::new();
            let mut col = 0usize;
            for (style, piece, _) in RangedHighlightIterator::new(&mut hs, &ops, &text, &hl) {
                let n = piece.chars().count();
                let fg = style.foreground;
                if Some(fg) != plain_fg {
                    spans.push((col, col + n, [fg.r, fg.g, fg.b]));
                }
                col += n;
            }
            out.push(spans);
        }
        out
    }

    fn advance(
        &mut self,
        ps: &mut ParseState,
        hs: &mut HighlightState,
        hl: &SynHl,
        text: &str,
    ) -> bool {
        if text.chars().count() > MAX_LINE_CHARS {
            return true; // skip styling huge lines but keep going
        }
        match ps.parse_line(text, syntax_set()) {
            Ok(ops) => {
                for _ in RangedHighlightIterator::new(hs, &ops, text, hl) {}
                true
            }
            Err(_) => {
                self.broken = true;
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lines(Vec<&'static str>);

    impl LineSource for Lines {
        fn line_count(&mut self) -> usize {
            self.0.len()
        }
        fn line_with_nl(&mut self, idx: usize) -> String {
            format!("{}\n", self.0[idx])
        }
    }

    /// One test for the one global: a theme switch reaches a highlighter
    /// already open, "none" turns colour off, and text in the theme's
    /// plain colour is left to the screen's.
    #[test]
    fn a_new_theme_reaches_an_open_highlighter() {
        let mut src = Lines(vec!["fn main() {}"]);
        let mut hl = Highlighter::new(Path::new("x.rs"), 12).unwrap();
        let colours = |hl: &mut Highlighter, src: &mut Lines| hl.range_spans(src, 0, 1).remove(0);

        set_syntax_theme(Some("InspiredGitHub")).unwrap();
        let light = colours(&mut hl, &mut src);
        let plain = builtin_themes().themes["InspiredGitHub"]
            .settings
            .foreground
            .map(|c| [c.r, c.g, c.b]);
        assert!(!light.is_empty(), "a keyword is coloured");
        assert!(light.iter().all(|s| Some(s.2) != plain), "{light:?}");

        set_syntax_theme(Some("base16-eighties.dark")).unwrap();
        let dark = colours(&mut hl, &mut src);
        assert_ne!(light, dark, "the open highlighter took the new theme up");

        set_syntax_theme(None).unwrap();
        assert!(colours(&mut hl, &mut src).is_empty());

        assert!(set_syntax_theme(Some("no such theme")).is_err());
        assert!(
            colours(&mut hl, &mut src).is_empty(),
            "a bad name changes nothing"
        );
        set_syntax_theme(Some(DEFAULT_THEME)).unwrap();
    }
}
