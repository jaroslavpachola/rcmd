//! Syntax highlighting (feature `syntax`): syntect with parse-state
//! checkpoints every [`CHECKPOINT`] lines, so edits only re-highlight
//! from the edited line down and scrolling never re-parses from the top.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use syntect::highlighting::{
    HighlightState, Highlighter as SynHl, RangedHighlightIterator, Theme, ThemeSet,
};
use syntect::parsing::{
    ParseState, ScopeStack, SyntaxDefinition, SyntaxReference, SyntaxSet, SyntaxSetBuilder,
};

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

/// Syntaxes rcmd carries beyond syntect's own.
const BUNDLED: &[&str] = &[include_str!("syntax/lean4.sublime-syntax")];

fn add_bundled(builder: &mut SyntaxSetBuilder) {
    for text in BUNDLED {
        // one that does not parse is a bug the tests catch; at run time
        // it costs only itself
        if let Ok(def) = SyntaxDefinition::load_from_str(text, true, None) {
            builder.add(def);
        }
    }
}

/// Where syntaxes are looked up. Adding to syntect's own set rebuilds
/// all of it, a fifth of a second before the first file is coloured,
/// so rcmd's syntaxes are a set of their own, searched after syntect's
/// and built only when a lookup gets that far - reading their YAML is
/// itself a cost every first F3 of a `.txt` would pay. They are for
/// languages syntect lacks, so the order changes nothing else. A user
/// with syntax files pays the rebuild anyway and gets one set of
/// everything, theirs added last so that theirs win.
enum Sets {
    Split {
        defaults: SyntaxSet,
        bundled: OnceLock<SyntaxSet>,
    },
    Merged(SyntaxSet),
}

fn syntax_sets() -> &'static Sets {
    static SETS: OnceLock<Sets> = OnceLock::new();
    SETS.get_or_init(|| {
        let dir = USER_SYNTAX
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(dir) = dir.filter(|dir| dir.is_dir()) else {
            return Sets::Split {
                defaults: SyntaxSet::load_defaults_newlines(),
                bundled: OnceLock::new(),
            };
        };
        let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
        add_bundled(&mut builder);
        // a broken syntax file costs its own file and a warning, never
        // the highlighting of everything else
        if let Err(err) = builder.add_from_folder(&dir, true) {
            *USER_SYNTAX_WARNING
                .write()
                .unwrap_or_else(|e| e.into_inner()) =
                Some(format!("syntax: {}: {err}", dir.display()));
        }
        Sets::Merged(builder.build())
    })
}

impl Sets {
    /// Every set, in lookup order; rcmd's own is built when the
    /// iteration reaches it, not before.
    fn each(&'static self) -> impl Iterator<Item = &'static SyntaxSet> {
        let (first, rest): (&SyntaxSet, Option<&OnceLock<SyntaxSet>>) = match self {
            Sets::Split { defaults, bundled } => (defaults, Some(bundled)),
            Sets::Merged(set) => (set, None),
        };
        std::iter::once(first).chain(rest.into_iter().map(|bundled| {
            bundled.get_or_init(|| {
                let mut builder = SyntaxSetBuilder::new();
                add_bundled(&mut builder);
                builder.build()
            })
        }))
    }
}

/// The first syntax `find` turns up, with the set it belongs to.
fn find_syntax(
    find: impl Fn(&'static SyntaxSet) -> Option<&'static SyntaxReference>,
) -> Option<(&'static SyntaxSet, &'static SyntaxReference)> {
    syntax_sets().each().find_map(|set| Some((set, find(set)?)))
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
    let mut names: Vec<&'static str> = syntax_sets()
        .each()
        .flat_map(SyntaxSet::syntaxes)
        .map(|s| s.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

pub struct Highlighter {
    /// The set `syntax` is from, which parsing it needs.
    set: &'static SyntaxSet,
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
        let (set, syntax) = find_syntax(|set| set.find_syntax_by_name(name))?;
        Some(Highlighter {
            set,
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
        let ext = path.extension()?.to_str()?;
        let name = path.file_name()?.to_str()?;
        let (set, syntax) = find_syntax(|set| set.find_syntax_by_extension(ext))
            .or_else(|| find_syntax(|set| set.find_syntax_by_extension(name)))?;
        if syntax.name == "Plain Text" {
            return None;
        }
        Some(Highlighter {
            set,
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
            let Ok(ops) = ps.parse_line(&text, self.set) else {
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
        match ps.parse_line(text, self.set) {
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

    #[test]
    fn the_bundled_syntaxes_load() {
        for text in BUNDLED {
            SyntaxDefinition::load_from_str(text, true, None).unwrap();
        }
    }

    /// The innermost scope each named piece of `text` is parsed into,
    /// by syntect's own parser rather than by a theme's colours.
    fn scopes_of(path: &str, text: &str, pieces: &[&str]) -> Vec<String> {
        use syntect::parsing::ScopeStackOp;
        let ext = Path::new(path).extension().unwrap().to_str().unwrap();
        let (ss, syntax) = find_syntax(|set| set.find_syntax_by_extension(ext)).unwrap();
        let mut ps = ParseState::new(syntax);
        let mut stack = ScopeStack::new();
        // (byte offset in text, innermost scope from there on)
        let mut marks: Vec<(usize, String)> = Vec::new();
        let mut base = 0;
        for line in text.split_inclusive('\n') {
            for (at, op) in ps.parse_line(line, ss).unwrap() {
                stack.apply(&op).unwrap();
                if !matches!(op, ScopeStackOp::Noop) {
                    let top = stack.as_slice().last().map(|s| s.build_string());
                    marks.push((base + at, top.unwrap_or_default()));
                }
            }
            base += line.len();
        }
        pieces
            .iter()
            .map(|piece| {
                let at = text
                    .find(piece)
                    .unwrap_or_else(|| panic!("{piece} in text"));
                marks
                    .iter()
                    .rev()
                    .find(|(pos, _)| *pos <= at)
                    .map(|(_, s)| s.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn lean_is_highlighted() {
        let text = "\
import Mathlib.Data.Nat
/-- doc /- nested -/ still doc -/
@[simp, inline] private theorem add_zero' (n : ℕ) : n + 0 = n := by
  simp [Nat.add_zero] -- done
def «weird name» (x₁ : Nat) : String := s!\"x = {x₁ + 1} ok\"
structure Point where
  x : Float := 3.5e2
#eval 'a'
example : Type* := sorry
def h' := `Nat.succ
";
        let got = scopes_of(
            "a.lean",
            text,
            &[
                "import",
                "doc",
                "still doc",
                "simp,",
                "private",
                "theorem",
                "add_zero'",
                "by",
                "Nat.add_zero",
                "-- done",
                "«weird name»",
                "s!",
                "x = ",
                "x₁ +",
                " ok",
                "Point",
                "3.5e2",
                "#eval",
                "'a'",
                "Type*",
                "sorry",
                "`Nat.succ",
            ],
        );
        let want = [
            "keyword.other.lean4",
            "comment.block.documentation.lean4",
            "comment.block.documentation.lean4",
            "entity.other.attribute-name.lean4",
            "storage.modifier.lean4",
            "keyword.other.declaration.lean4",
            "entity.name.function.lean4",
            "keyword.control.lean4",
            "source.lean4",
            "punctuation.definition.comment.lean4",
            "entity.name.function.lean4",
            "storage.type.string.lean4",
            "string.interpolated.lean4",
            "meta.interpolation.lean4",
            "string.interpolated.lean4",
            "entity.name.type.lean4",
            "constant.numeric.lean4",
            "keyword.other.command.lean4",
            "constant.character.lean4",
            "storage.type.lean4",
            "keyword.other.sorry.lean4",
            "constant.other.symbol.lean4",
        ];
        assert_eq!(got, want);
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
