//! Syntax highlighting (feature `syntax`): syntect with parse-state
//! checkpoints every [`CHECKPOINT`] lines, so edits only re-highlight
//! from the edited line down and scrolling never re-parses from the top.

use std::path::Path;
use std::sync::OnceLock;

use syntect::highlighting::{
    HighlightState, Highlighter as SynHl, RangedHighlightIterator, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

use crate::LineSource;

const CHECKPOINT: usize = 32;
/// Highlighting is skipped entirely above these limits.
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_LINE_CHARS: usize = 2000;

fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static Theme {
    static TS: OnceLock<ThemeSet> = OnceLock::new();
    let themes = TS.get_or_init(ThemeSet::load_defaults);
    &themes.themes["base16-eighties.dark"]
}

pub struct Highlighter {
    syntax: &'static SyntaxReference,
    /// `states[k]` = parser/highlight state *before* line `k * CHECKPOINT`.
    states: Vec<(ParseState, HighlightState)>,
    /// Everything from this line on must be re-derived after an edit.
    dirty_from: usize,
    broken: bool,
}

impl Highlighter {
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
        })
    }

    pub fn invalidate_from(&mut self, line: usize) {
        self.dirty_from = self.dirty_from.min(line);
    }

    fn initial(&self) -> (ParseState, HighlightState) {
        (
            ParseState::new(self.syntax),
            HighlightState::new(&SynHl::new(theme()), ScopeStack::new()),
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
        let hl = SynHl::new(theme());
        // drop checkpoints past the first edited line
        let keep = self.dirty_from.div_ceil(CHECKPOINT).min(self.states.len());
        self.states.truncate(keep);
        self.dirty_from = self.states.len() * CHECKPOINT;
        if self.states.is_empty() {
            self.states.push(self.initial());
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
                spans.push((col, col + n, [fg.r, fg.g, fg.b]));
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
