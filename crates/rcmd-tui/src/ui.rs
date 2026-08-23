use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Gauge, Row, Table, TableState};
use rcmd_core::entry::{Entry, EntryKind};
use rcmd_core::fsops::FileFacts;
use rcmd_core::glob::glob_match;
use rcmd_core::panel::{ListMode, Panel};
use rcmd_core::tree::Tree;

use crate::format::{Field, Format, Item};

use crate::app::{
    App, Ask, ConfirmDialog, ConnectAsk, Dialog, EditPrompt, FindDialog, InputDialog, Job, MENUS,
    MenuState, OptionsDialog, QuickView, menu_label,
};
use crate::config::HotEntry;
use crate::git::GitStatus;

/// All colors in one place; selected from config (`theme = "mc" |
/// "dark"`) at startup or from the options form, read through [`th`].
#[derive(Clone, Copy)]
pub struct Theme {
    pub panel_bg: Color,
    pub panel_fg: Color,
    pub dir_fg: Color,
    pub exec_fg: Color,
    pub broken_fg: Color,
    pub header_fg: Color,
    pub mark_fg: Color,
    pub select_bg: Color,
    pub select_fg: Color,
    pub dialog_bg: Color,
    pub dialog_fg: Color,
    pub error_bg: Color,
    pub error_fg: Color,
    pub help_bg: Color,
    pub help_fg: Color,
    pub help_header_fg: Color,
    pub prompt_fg: Color,
    pub key_fg: Color,
    pub key_bg: Color,
    pub label_fg: Color,
    pub label_bg: Color,
}

fn mc_theme() -> Theme {
    Theme {
        panel_bg: Color::Blue,
        panel_fg: Color::Gray,
        dir_fg: Color::White,
        exec_fg: Color::LightGreen,
        broken_fg: Color::LightRed,
        header_fg: Color::Yellow,
        mark_fg: Color::Yellow,
        select_bg: Color::Cyan,
        select_fg: Color::Black,
        dialog_bg: Color::Gray,
        dialog_fg: Color::Black,
        error_bg: Color::Red,
        error_fg: Color::White,
        help_bg: Color::Cyan,
        help_fg: Color::Black,
        help_header_fg: Color::White,
        prompt_fg: Color::LightCyan,
        key_fg: Color::White,
        key_bg: Color::Black,
        label_fg: Color::Black,
        label_bg: Color::Cyan,
    }
}

/// Truecolor dark theme (One Dark-ish).
fn dark_theme() -> Theme {
    Theme {
        panel_bg: Color::Rgb(0x1e, 0x22, 0x2a),
        panel_fg: Color::Rgb(0xc8, 0xcc, 0xd4),
        dir_fg: Color::Rgb(0x61, 0xaf, 0xef),
        exec_fg: Color::Rgb(0x98, 0xc3, 0x79),
        broken_fg: Color::Rgb(0xe0, 0x6c, 0x75),
        header_fg: Color::Rgb(0xe5, 0xc0, 0x7b),
        mark_fg: Color::Rgb(0xe5, 0xc0, 0x7b),
        select_bg: Color::Rgb(0x3e, 0x44, 0x51),
        select_fg: Color::Rgb(0xff, 0xff, 0xff),
        dialog_bg: Color::Rgb(0x2c, 0x31, 0x3a),
        dialog_fg: Color::Rgb(0xc8, 0xcc, 0xd4),
        error_bg: Color::Rgb(0xbe, 0x50, 0x46),
        error_fg: Color::Rgb(0xff, 0xff, 0xff),
        help_bg: Color::Rgb(0x2c, 0x31, 0x3a),
        help_fg: Color::Rgb(0xc8, 0xcc, 0xd4),
        help_header_fg: Color::Rgb(0x61, 0xaf, 0xef),
        prompt_fg: Color::Rgb(0x56, 0xb6, 0xc2),
        key_fg: Color::Rgb(0xff, 0xff, 0xff),
        key_bg: Color::Rgb(0x1e, 0x22, 0x2a),
        label_fg: Color::Rgb(0xc8, 0xcc, 0xd4),
        label_bg: Color::Rgb(0x3e, 0x44, 0x51),
    }
}

/// What a `[[highlight]]` rule looks at: what the entry is called, or
/// what it is.
enum Matcher {
    Glob(String),
    Kind(HighlightKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HighlightKind {
    Dir,
    LinkDir,
    Exe,
    Link,
    Broken,
    File,
}

/// A `[[highlight]]` rule with its glob compiled and its colour parsed.
struct Rule {
    matcher: Matcher,
    color: Color,
    bold: Option<bool>,
}

impl Rule {
    fn matches(&self, entry: &Entry) -> bool {
        match &self.matcher {
            Matcher::Glob(pattern) => glob_match(pattern, &entry.name.to_string_lossy()),
            Matcher::Kind(kind) => *kind == entry_kind(entry),
        }
    }
}

fn entry_kind(entry: &Entry) -> HighlightKind {
    match entry.kind {
        EntryKind::Dir => HighlightKind::Dir,
        EntryKind::SymlinkDir => HighlightKind::LinkDir,
        EntryKind::SymlinkFile => HighlightKind::Link,
        EntryKind::SymlinkBroken => HighlightKind::Broken,
        EntryKind::File if entry.is_executable() => HighlightKind::Exe,
        EntryKind::File => HighlightKind::File,
    }
}

static HIGHLIGHT: std::sync::RwLock<Vec<Rule>> = std::sync::RwLock::new(Vec::new());

/// Compile `[[highlight]]`; returns a warning per rule that could not be
/// understood. A bad rule is dropped, never fatal - a colour typo should
/// cost that one rule, not the listing.
pub fn init_highlight(rules: &[crate::config::HighlightRule]) -> Vec<String> {
    let (compiled, warnings) = compile_highlight(rules);
    *HIGHLIGHT.write().unwrap_or_else(|e| e.into_inner()) = compiled;
    warnings
}

fn compile_highlight(rules: &[crate::config::HighlightRule]) -> (Vec<Rule>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut compiled = Vec::new();
    for rule in rules {
        let matcher = match (&rule.pattern, &rule.kind) {
            (Some(_), Some(_)) => {
                warnings.push("highlight: rule has both match and type".into());
                continue;
            }
            (Some(pattern), None) => Matcher::Glob(pattern.clone()),
            (None, Some(kind)) => match parse_kind(kind) {
                Some(kind) => Matcher::Kind(kind),
                None => {
                    warnings.push(format!("highlight: unknown type '{kind}'"));
                    continue;
                }
            },
            (None, None) => {
                warnings.push("highlight: rule has neither match nor type".into());
                continue;
            }
        };
        match parse_color(&rule.color) {
            Some(color) => compiled.push(Rule {
                matcher,
                color,
                bold: rule.bold,
            }),
            None => warnings.push(format!("highlight: unknown colour '{}'", rule.color)),
        }
    }
    (compiled, warnings)
}

fn parse_kind(name: &str) -> Option<HighlightKind> {
    Some(match name {
        "dir" => HighlightKind::Dir,
        "linkdir" => HighlightKind::LinkDir,
        "exe" => HighlightKind::Exe,
        "link" => HighlightKind::Link,
        "broken" => HighlightKind::Broken,
        "file" => HighlightKind::File,
        _ => return None,
    })
}

/// MC's colour names (its skin files use these), `#rrggbb`, or
/// `default` for the terminal's own foreground.
pub fn parse_color(name: &str) -> Option<Color> {
    if let Some(hex) = name.strip_prefix('#')
        && hex.len() == 6
        && let Ok(rgb) = u32::from_str_radix(hex, 16)
    {
        return Some(Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8));
    }
    // mc spells the bright half "bright*"; "light*" is accepted too,
    // because that is what everything else calls them - except
    // "lightgray", which is a colour of mc's own
    let owned;
    let name = match name.strip_prefix("light") {
        Some("gray" | "grey") => "lightgray",
        Some(rest) => {
            owned = format!("bright{rest}");
            owned.as_str()
        }
        None => name,
    };
    Some(match name {
        "default" => Color::Reset,
        "black" => Color::Black,
        // mc's gray is bright black, and its lightgray is the plain one
        "gray" | "grey" => Color::DarkGray,
        "lightgray" | "lightgrey" => Color::Gray,
        "white" => Color::White,
        "red" => Color::Red,
        "brightred" => Color::LightRed,
        "green" => Color::Green,
        "brightgreen" => Color::LightGreen,
        "brown" => Color::Yellow,
        "yellow" | "brightyellow" => Color::LightYellow,
        "blue" => Color::Blue,
        "brightblue" => Color::LightBlue,
        "magenta" => Color::Magenta,
        "brightmagenta" => Color::LightMagenta,
        "cyan" => Color::Cyan,
        "brightcyan" => Color::LightCyan,
        _ => return None,
    })
}

static THEME: std::sync::RwLock<Option<Theme>> = std::sync::RwLock::new(None);

/// Install the theme; returns a warning for unknown names. Called at
/// startup and again when the options form switches themes.
pub fn init_theme(name: &str) -> Option<String> {
    let (theme, warning) = match name {
        "mc" => (mc_theme(), None),
        "dark" => (dark_theme(), None),
        other => (
            mc_theme(),
            Some(format!("unknown theme '{other}', using mc")),
        ),
    };
    *THEME.write().unwrap_or_else(|e| e.into_inner()) = Some(theme);
    warning
}

fn th() -> Theme {
    THEME
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or_else(mc_theme)
}

/// Help text; lines starting with `#` render as section headers.
const HELP_TEXT: &[&str] = &[
    "",
    "# Panels",
    "  Tab             switch active panel",
    "  Up/Down, PgUp/PgDn, Home/End   move the cursor",
    "  Enter           enter directory or archive (zip, tar, tar.gz)",
    "  Backspace       go to parent directory / leave the archive",
    "  Ctrl+S, Alt+S   quick search (type to jump, Ctrl+S again = next)",
    "  Ctrl+U          swap the two panels",
    "  Ctrl+F          filter shown files by glob ('*' clears)",
    "  Ctrl+\\          directory hotlist (Enter cd, a add, d delete)",
    "  Alt+F7          find file (glob + optional content substring);",
    "                  results stream into the panel, Esc cancels",
    "  Ctrl+X d        compare directories: marks files missing on the",
    "                  other side or differing in size/mtime (F5 syncs)",
    "  F9>Cmd>Panelize command output becomes the panel listing",
    "  (Ctrl+R restores a normal listing after find/panelize)",
    "  Ctrl+Space      directory size (background scan, fills Size column)",
    "  Ctrl+R          reload both panels",
    "  Panels auto-reload when their directory changes on disk",
    "  (watch = false in config disables). Slow directories load in the",
    "  background: old listing + spinner stay up, Esc cancels the load.",
    "  Alt+.           show/hide dotfiles",
    "  Alt+N           sort by name (again = reverse); other orders are in",
    "                  the panel's own F9 menu (Left or Right)",
    "  Alt+T           cycle listing format: brief (names in columns,",
    "                  brief_columns in the config) / full / long",
    "                  (an active long panel takes the whole width, MC's",
    "                  one-panel view; Tab or cycling back restores the split)",
    "  [[highlight]] in the config colours entries: match = \"*.tar.gz\" (a",
    "                  glob) or type = \"exe\" (dir linkdir exe link broken",
    "                  file), color = mc's names / #rrggbb / default, and an",
    "                  optional bold; the first matching rule wins",
    "  F9 > Left/Right   the menu bar is MC's: Left and Right act on that",
    "                  panel whichever one has the focus - listing format,",
    "                  quick view, info, tree, sort order, filter, panelize,",
    "                  rescan, SFTP link. Using one focuses that panel, so",
    "                  the dialogs it opens cannot land on the other.",
    "  F9 > Left/Right > User defined   the panel draws listing_format from the",
    "                  config: a panel size (half/full), an optional repeat",
    "                  count 1-9, then fields - name size bsize type mark",
    "                  mtime atime ctime perm mode nlink ngid nuid owner",
    "                  group inode, plus space and | - each with an optional",
    "                  :width (:width+ grows). MC's own Full listing is",
    "                  \"half type name | size | mtime\"",
    "  F9 > Left/Right > Tree   the panel becomes a directory tree: Up and",
    "                  Down walk it, Left/Right go to parent/child, Enter",
    "                  opens the selection in the *other* panel and the",
    "                  tree stays put,",
    "                  F4 switches dynamic/static navigation, Ctrl+R rescans",
    "  F9 > Command > Directory tree   the same figure in a dialog, where",
    "                  Enter takes *this* panel there and closes; typing",
    "                  jumps to a directory, F2 rescans, F3 forgets a branch",
    "  Alt+Left/Right  walk the panel's directory history (back/forward)",
    "  F9 > Options    one options form, in sections: Layout (split",
    "                  direction and size, which bars are drawn), Panel (hidden",
    "                  files, lynx-like motion, mouse, auto-reload, git),",
    "                  Confirmation (ask before deleting / overwriting /",
    "                  quitting), Shell and editor, Appearance - applied",
    "                  live and saved at once",
    "  In menus the highlighted letter runs the entry (F9 o p = options)",
    "  Alt+Up          directory hotlist (same as Ctrl+\\)",
    "  Ctrl+X q        quick view: the other panel previews the cursor",
    "                  file live (Tab focuses it for scrolling; again = off)",
    "  Ctrl+X i        info panel: the other panel shows the full stat of",
    "                  the cursor file (owner, times, inode...; again = off)",
    "  Alt+I           other panel switches to this panel's directory",
    "  Alt+O           other panel opens the directory under the cursor",
    "  Alt+Y / Alt+U   history back / forward (same as Alt+Left/Right)",
    "  Alt+C           quick cd dialog   Alt+?  find file   Ctrl+L  redraw",
    "  Ctrl+X t / p    paste tagged names / the panel path to the cmdline",
    "  Ctrl+X c / o    chmod (octal) / chown (user[:group]) the marked",
    "                  entries - both work on sftp panels too",
    "  Ctrl+X s        create a symlink to the cursor entry",
    "  F9 > Left/Right   listing format: brief (names), full, long (ls -l,",
    "                  full-width), user defined, tree; the panel footer",
    "                  shows free space",
    "  Inside a git work tree the title shows [branch] and entries get a",
    "  status column: M modified, A added, ? untracked, ! ignored (dim).",
    "",
    "# Mouse  (mouse = false in config disables)",
    "  Click focuses a panel and moves the cursor; double-click enters.",
    "  The wheel scrolls the hovered panel, viewer, editor, or preview.",
    "  The bottom keybar and the F9 menu are clickable. In the editor a",
    "  click places the cursor. Hold Shift to select terminal text.",
    "",
    "# Marking",
    "  Insert, Ctrl+T  toggle mark and advance",
    "  +               select by glob pattern",
    "  - or \\          unselect by glob pattern",
    "  *               invert selection",
    "  (the four keys above work while the command line is empty)",
    "",
    "# File operations  (marked entries, or the cursor entry)",
    "  F5              copy - a form: a source mask, where to, then MC's",
    "                  switches for what a copy means (preserve attributes,",
    "                  follow links, dive into subdirs, stable symlinks),",
    "                  then OK / Background / Cancel. Space flips a box,",
    "                  Up/Down move, Background starts the job detached",
    "  Masks rename as they copy: source *.tar.gz with destination",
    "                  dir/*.tgz makes foo.tar.gz into dir/foo.tgz. The",
    "                  mask's wildcards are numbered left to right - * in",
    "                  the destination is the first, \\1..\\9 any of them,",
    "                  \\0 the whole name - and \\u \\l \\U \\L \\E change case.",
    "                  Files the mask does not match are left where they are.",
    "  F6              move / rename (the same form)",
    "  F7              make directory",
    "  F8              delete to trash",
    "  Shift+F8        delete permanently",
    "  Shift+F4        edit a new file (created on first save)",
    "  Shift+F5/F6     copy / rename the cursor file in place",
    "  F9 > File > Bulk rename   edit the marked names as text: each",
    "                  line is \"number TAB name\" - change names to",
    "                  rename (swaps are fine), delete lines to delete;",
    "                  save, close, and confirm the preview",
    "  Esc             cancel a running operation",
    "  The progress dialog shows the file in hand, how many items are",
    "                  done, the throughput and the time left, a bar for",
    "                  the whole job and a second one for the current file",
    "  b               send the running operation to the background",
    "  Ctrl+X !        panelize a command's output (F9 > Command too)",
    "  Ctrl+X j        jobs list: Enter foregrounds, c cancels; the",
    "                  status line shows aggregate background progress",
    "  Overwrite prompt: both files' size and date, then MC's answers -",
    "                  this file: Overwrite / Append / Reget (resume);",
    "                  all files: All / Update (only where the source is",
    "                  newer) / Size differs / None. Up/Down move between",
    "                  the rows. Append and Reget need a local file on both",
    "                  sides. Hotkeys: o=overwrite a=all s=skip S=skip all",
    "  Error prompt hotkeys:     r=retry s=skip S=skip all",
    "",
    "# Archives",
    "  Enter on zip/tar/tar.{gz,xz,bz2} browses it; F5 copies out,",
    "  F3 views members. Move/delete/mkdir are disabled inside.",
    "  rar and 7z browse too, through an installed 7z (p7zip; rar",
    "  needs its codec) or unrar - read-only, streamed per member.",
    "  Copy INTO an archive: F5 with the other panel inside it, or a",
    "  destination written as archive.zip://dir - zip appends in place,",
    "  tar (plain or .gz/.xz/.bz2) is rewritten with the new entries.",
    "",
    "# SFTP (remote panels)",
    "  cd sftp://[user@]host[:port][/path]   connect (or F9>Cmd>SFTP link)",
    "  Auth: ssh-agent, then ~/.ssh/id_* keys, then password prompts.",
    "  Unknown host keys show a fingerprint dialog; accepted keys are",
    "  saved to ~/.ssh/known_hosts. The panel title shows the URL.",
    "  F5/F6 up/download between panels (progress dialogs as usual),",
    "  F7 mkdir, F8 deletes on the server (no remote trash!), F3 views,",
    "  F4 edits a local scratch copy and uploads it back on save.",
    "  cd PATH stays on the server; plain cd or cd ~ returns local.",
    "  Both panels may share one connection; Ctrl+X d compares",
    "  local vs remote, then F5 syncs the marked differences.",
    "",
    "# Openers & user commands  (config)",
    "  [[open]] rules make Enter open files:",
    "      [[open]]",
    "      match = \"*.pdf\"",
    "      run = \"zathura %f >/dev/null 2>&1 &\"",
    "  First matching glob wins (case-insensitive), local panels only.",
    "  Openers run without a pause; append & for GUI programs.",
    "  With lynx-like motion Right still only enters directories.",
    "  [[view]] rules filter F3 through a command's stdout:",
    "      [[view]]",
    "      match = \"*.pdf\"",
    "      run = \"pdftotext %f -\"",
    "  Shift+F3 always shows the raw bytes (no filter).",
    "  [[commands]] are shell templates in the F2 user menu:",
    "      [[commands]]",
    "      name = \"git status\"",
    "      run = \"git status | less\"",
    "      key = \"ctrl+g\"        # optional direct binding",
    "  Macros: %f cursor file, %d this dir, %D other panel's dir,",
    "  %t marked files, %% literal percent - all shell-quoted.",
    "",
    "# Command line",
    "  (type)          compose a command; Enter runs it in the panel dir",
    "  cd PATH         changes the active panel instead",
    "  Alt+Enter       insert the selected filename",
    "  Ctrl+P / Ctrl+N previous / next history entry (Alt+P / Alt+N too)",
    "  Alt+H           pick from the command history (kept across runs)",
    "  Alt+A           insert this panel's path (same as Ctrl+X p)",
    "  cd -            back to the panel's previous directory; a relative",
    "                  cd that misses here also tries $CDPATH",
    "  %f %d %D %t     expand on the command line, as in MC (%% = percent)",
    "  Ctrl+A / Ctrl+E start / end of line",
    "  Esc             clear the command line - acts at once while typing",
    "  Ctrl+O          open a full shell here; exit returns to rcmd",
    "",
    "# Viewer (F3)",
    "  F2              toggle line wrap",
    "  F4              toggle hex dump",
    "  F7 or /         search (case-insensitive), n = next match;",
    "                  matches are highlighted, the found line marked",
    "  Files with a known syntax (≤2 MB) get syntax colors, like F4",
    "  Left/Right      horizontal scroll",
    "  f               follow mode (tail -f): stick to the growing end",
    "  Shift+F3        raw view (skip any [[view]] filter)",
    "  F3/F10/Esc/q    close the viewer",
    "",
    "# Editor (F4, built-in)",
    "  F2 save (atomic, keeps permissions and CRLF)   F10/Esc quit",
    "  F3 mark (select; Shift+arrows also select)     F8 delete line",
    "  F5 copy the block (no block: duplicate line)   F6 move (cut) it",
    "  Alt+W toggle soft-wrap (long lines fold instead of scrolling)",
    "  Ctrl+C/X/V copy/cut/paste   Ctrl+Z undo   Ctrl+Y redo",
    "  Ctrl+A select all   Ctrl+arrows word hop   Tab inserts a tab",
    "  F7 search (regex, smartcase), Shift+F7 next match",
    "  F4 replace: pattern, replacement, then Replace/Skip/All/Quit",
    "  Enter auto-indents. Syntax colors appear for known file types.",
    "  On sftp panels F4 edits a local copy, uploaded back on quit.",
    "  editor = \"external\" in the config restores $VISUAL/$EDITOR.",
    "",
    "# Other",
    "  Esc KEY         meta prefix, like MC: Esc 1..0 = F1..F10,",
    "                  Esc letter = Alt+letter, Esc Esc = plain Escape",
    "                  (a lone Esc acts after 1 s - at once if you are",
    "                  typing on the command line)",
    "  F1              this help",
    "  F4              edit (built-in editor, see above)",
    "  F9              pulldown menu",
    "  F10             quit",
    "  rcmd -P FILE    write last directory to FILE on exit",
    "                  (see README for the rc() shell wrapper)",
    "",
    "# Config  (~/.config/rcmd/config.toml - yours, never rewritten;",
    "#          rcmd's own state lives in ~/.local/state/rcmd/state.toml)",
    "  theme = \"mc\" | \"dark\"      keymap = \"mc\" | \"modern\" (= lynx on)",
    "  [keys] adds custom bindings, e.g. \"ctrl+y\" = \"swap-panels\";",
    "  [keys.viewer] and [keys.editor] rebind inside the viewer/editor",
];

pub fn help_lines() -> usize {
    HELP_TEXT.len()
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    if app.help.is_some() {
        draw_help(frame, app);
        return;
    }
    if app.editor.is_some() {
        draw_editor(frame, app);
        return;
    }
    if app.viewer.is_some() {
        draw_viewer(frame, app);
        return;
    }
    // MC's Layout dialog: every bar is optional, so the vertical stack
    // is built from whatever is switched on.
    let cfg = &app.config;
    let bar = |on: bool| Constraint::Length(u16::from(on));
    let [menubar, main, status, cmdline, keybar] = Layout::vertical([
        bar(cfg.show_menubar),
        Constraint::Min(3),
        bar(cfg.show_status),
        bar(cfg.show_cmdline),
        bar(cfg.show_keybar),
    ])
    .areas(frame.area());

    // MC's one-panel view: a long listing needs the whole width, so while
    // the ACTIVE side shows one, only that panel is drawn, full-width (the
    // hidden side gets a zero area so mouse hit-testing skips it). Only the
    // active side counts: an off-side long panel renders squeezed in the
    // split rather than invisibly forcing fullscreen - the state stays
    // visible and Alt+T on either side always behaves predictably.
    let qv_side = app.quick_view.as_ref().map(|q| q.side);
    // a user format asks for the full width itself, by starting with
    // `full` instead of `half`
    let full_width = app.listing_format.full;
    let listing_long = |i: usize| {
        qv_side != Some(i)
            && app.info != Some(i)
            && match app.panels[i].list_mode {
                ListMode::Long => true,
                ListMode::User => full_width,
                _ => false,
            }
    };
    let [left, right] = if listing_long(app.active) {
        let hidden = Rect::new(main.x, main.y, 0, 0);
        if app.active == 0 {
            [main, hidden]
        } else {
            [hidden, main]
        }
    } else {
        let split = [
            Constraint::Percentage(app.config.ratio()),
            Constraint::Percentage(100 - app.config.ratio()),
        ];
        if app.config.horizontal_split() {
            Layout::vertical(split).areas(main)
        } else {
            Layout::horizontal(split).areas(main)
        }
    };

    // 2 border rows + 1 column-header row, in the ACTIVE panel: with a
    // horizontal split the two panels can differ in height.
    let active_area = if app.active == 0 { left } else { right };
    let visible_rows = active_area
        .height
        .saturating_sub(3 + u16::from(cfg.show_mini_status)) as usize;
    // listings laid out in columns page by whole screens of names,
    // not by rows
    app.panel_rows = match app.panels[app.active].list_mode {
        ListMode::Brief => visible_rows * cfg.columns() as usize,
        ListMode::User => visible_rows * app.listing_format.repeat.max(1) as usize,
        _ => visible_rows,
    };
    app.areas = crate::app::Areas {
        screen: frame.area(),
        left,
        right,
        keybar,
        menubar,
    };

    for (i, area) in [(0, left), (1, right)] {
        if area.width == 0 {
            continue;
        }
        let disk = app.disk[i]
            .as_ref()
            .filter(|(dir, ..)| dir == &app.panels[i].cwd)
            .and_then(|(_, _, space)| *space);
        if qv_side == Some(i) {
            let qv = app.quick_view.as_mut().expect("side implies quick view");
            draw_quick_view(frame, area, qv, app.active == i);
        } else if app.info == Some(i) {
            let browse = &app.panels[i ^ 1];
            let browse_disk = app.disk[i ^ 1]
                .as_ref()
                .filter(|(dir, ..)| dir == &browse.cwd)
                .and_then(|(_, _, space)| *space);
            draw_info(frame, area, browse, browse_disk, app.active == i);
        } else {
            draw_panel(
                frame,
                area,
                &app.panels[i],
                &mut app.table_states[i],
                Chrome {
                    active: app.active == i,
                    git: app.git_info[i].as_ref().map(|(_, s)| s),
                    disk,
                    mini: app.config.show_mini_status,
                    columns: app.config.columns(),
                    tree: app.trees[i].as_ref(),
                    format: &app.listing_format,
                },
            );
        }
    }
    if menubar.height > 0 {
        draw_menubar(frame, menubar, app.menu.as_ref().map(|m| m.menu));
    }
    if status.height > 0 {
        draw_status(frame, status, app);
    }
    if cmdline.height > 0 {
        draw_cmdline(frame, cmdline, app);
    }
    if keybar.height > 0 {
        draw_keybar(frame, keybar);
    }

    if let Some(menu) = &app.menu {
        draw_menu(frame, menu);
    }
    if let Some(dialog) = &app.dialog {
        match dialog {
            Dialog::Input(d) => draw_input(frame, d),
            Dialog::Confirm(d) => draw_confirm(frame, d),
            Dialog::Tree(tree) => draw_tree_dialog(frame, tree),
            Dialog::Transfer(d) => draw_transfer(frame, d),
            Dialog::Hotlist(selected) => {
                draw_hotlist(frame, &app.config.hotlist, &app.hotlist_recent(), *selected)
            }
            Dialog::UserMenu(selected) => draw_user_menu(frame, &app.config.commands, *selected),
            Dialog::Find(d) => draw_find(frame, d),
            Dialog::Options(d) => draw_options(frame, d),
            Dialog::RenamePreview(d) => draw_rename_preview(frame, d),
            Dialog::Jobs(selected) => draw_jobs(frame, &app.jobs, *selected),
            Dialog::History(selected) => draw_history(frame, app.cmdline.history(), *selected),
        }
    }
    if let Some(job) = app.fg_job() {
        draw_job(frame, job);
        if let Some(ask) = &job.ask {
            draw_ask(frame, ask, job.button);
        }
    }
    if let Some(connect) = &app.connect
        && let Some(ask) = &connect.ask
    {
        draw_connect_ask(frame, ask);
    }
}

/// Everything drawn around a panel's listing: which side is focused,
/// its git status, free space and whether the mini status is on.
struct Chrome<'a> {
    active: bool,
    git: Option<&'a GitStatus>,
    disk: Option<(u64, u64)>,
    mini: bool,
    /// Name columns in a brief listing (MC shows two).
    columns: u16,
    /// The figure to draw instead of the listing, in tree mode.
    tree: Option<&'a Tree>,
    /// The parsed `listing_format`, drawn in user mode.
    format: &'a Format,
}

fn draw_panel(
    frame: &mut Frame,
    area: Rect,
    panel: &Panel,
    state: &mut TableState,
    chrome: Chrome<'_>,
) {
    let Chrome {
        active,
        git,
        disk,
        mini,
        ..
    } = chrome;
    let title_style = if active {
        Style::new().fg(th().select_fg).bg(th().select_bg)
    } else {
        Style::new().fg(th().panel_fg).bg(th().panel_bg)
    };
    let mut block = Block::bordered()
        .style(Style::new().fg(th().panel_fg).bg(th().panel_bg))
        .title(Span::styled(
            format!(" {} ", panel.display_path()),
            title_style,
        ));
    if let Some(branch) = git.map(|g| g.branch.as_str()).filter(|b| !b.is_empty()) {
        block = block.title_top(
            Line::from(Span::styled(
                format!(" [{branch}] "),
                Style::new().fg(th().header_fg).bg(th().panel_bg),
            ))
            .right_aligned(),
        );
    }
    let (marked_count, marked_bytes) = panel.marked_stats();
    if marked_count > 0 {
        block = block.title_bottom(
            Line::from(Span::styled(
                format!(" {marked_bytes} bytes in {marked_count} file(s) "),
                Style::new().fg(th().mark_fg).add_modifier(Modifier::BOLD),
            ))
            .centered(),
        );
    }
    if let Some(filter) = &panel.filter {
        block = block.title_bottom(
            Line::from(Span::styled(
                format!(" filter: {filter} "),
                Style::new().fg(th().header_fg),
            ))
            .right_aligned(),
        );
    } else if let Some((free, total)) = disk {
        block = block.title_bottom(
            Line::from(Span::styled(
                format!(" {} / {} free ", human_size(free), human_size(total)),
                Style::new().fg(th().header_fg),
            ))
            .right_aligned(),
        );
    }
    if let Some(label) = &panel.panelized {
        block = block.title_bottom(Span::styled(
            format!(" {} ", tail(label, 40)),
            Style::new().fg(th().header_fg).add_modifier(Modifier::BOLD),
        ));
    }
    if panel.is_loading() {
        const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
        let tick = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            / 150;
        let frame_ch = FRAMES[(tick % FRAMES.len() as u128) as usize];
        block = block.title_bottom(
            Line::from(Span::styled(
                format!(" {frame_ch} loading - Esc cancels "),
                Style::new().fg(th().mark_fg).add_modifier(Modifier::BOLD),
            ))
            .centered(),
        );
    }

    // Tree mode draws a figure of directories instead of the listing,
    // so it takes the whole inside of the frame.
    if panel.list_mode == ListMode::Tree {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(tree) = chrome.tree else { return };
        let body = Rect {
            height: inner.height.saturating_sub(u16::from(mini)),
            ..inner
        };
        let base = Style::new().fg(th().panel_fg).bg(th().panel_bg);
        let sel = if active {
            Style::new().fg(th().select_fg).bg(th().select_bg)
        } else {
            base
        };
        draw_tree_rows(frame, body, tree, base, sel);
        if mini && inner.height > 0 {
            let row = Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            };
            let text = tree
                .selected()
                .map(|r| abbrev_home(&r.path))
                .unwrap_or_default();
            let style = Style::new().fg(th().header_fg).bg(th().panel_bg);
            frame.render_widget(Line::from(text).style(style), row);
        }
        return;
    }
    if panel.list_mode == ListMode::User {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        draw_user_columns(frame, inner, panel, state, &chrome, git);
        return;
    }
    if panel.list_mode == ListMode::Brief && chrome.columns > 1 {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        draw_brief_columns(frame, inner, panel, state, &chrome, git);
        return;
    }
    let (labels, constraints): (&[&str], Vec<Constraint>) = match panel.list_mode {
        // brief columns, the tree and a user format have renderers
        // of their own; they never reach the table
        ListMode::Brief | ListMode::Tree | ListMode::User => (&["Name"], vec![Constraint::Fill(1)]),
        ListMode::Full => (
            &["Name", "Size", "Modify time"],
            vec![
                Constraint::Fill(1),
                Constraint::Length(7),
                Constraint::Length(12),
            ],
        ),
        ListMode::Long => (
            &["Perms", "Owner", "Group", "Size", "Name"],
            vec![
                Constraint::Length(10),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(7),
                Constraint::Fill(1),
            ],
        ),
    };
    let header = Row::new(labels.iter().map(|l| Cell::from(Line::from(*l).centered())))
        .style(Style::new().fg(th().header_fg));

    let remote = panel.is_remote();
    let rows = panel.entries.iter().enumerate().map(|(i, entry)| {
        let git_mark = git.map(|g| g.marks.get(&entry.name).copied());
        entry_row(
            entry,
            panel.is_marked(entry),
            active && i == panel.cursor,
            git_mark,
            panel.list_mode,
            remote,
        )
    });

    let table = Table::new(rows, constraints)
        .header(header)
        .column_spacing(1)
        .style(Style::new().fg(th().panel_fg).bg(th().panel_bg));

    // The frame is drawn first so the table can be given a smaller area
    // when the mini status claims the last row inside it.
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let listing = Rect {
        height: inner.height.saturating_sub(u16::from(mini)),
        ..inner
    };
    state.select(Some(panel.cursor));
    frame.render_stateful_widget(table, listing, state);

    if mini && inner.height > 0 {
        let row = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        let style = Style::new().fg(th().header_fg).bg(th().panel_bg);
        frame.render_widget(Line::from(entry_summary(panel)).style(style), row);
    }
}

/// One field's text for one entry. `mark` is the panel's tag state and
/// `remote` decides whether uid/gid can be resolved to names.
fn field_text(field: Field, entry: &Entry, marked: bool, remote: bool) -> String {
    let time = |t: Option<std::time::SystemTime>| {
        t.map(|t| DateTime::<Local>::from(t).format("%b %e %H:%M").to_string())
            .unwrap_or_default()
    };
    let number = |n: Option<u64>| n.map(|n| n.to_string()).unwrap_or_default();
    match field {
        Field::Name => entry.name.to_string_lossy().into_owned(),
        Field::Size => format_size(entry.size),
        // mc's bsize: directories say what they are instead of a byte count
        Field::BSize => {
            if entry.is_parent() {
                "UP--DIR".into()
            } else if entry.is_dir() {
                "SUB-DIR".into()
            } else {
                format_size(entry.size)
            }
        }
        Field::Type => entry_style(entry).0.to_string(),
        Field::Mark => if marked { "*" } else { " " }.into(),
        Field::Mtime => time(entry.mtime),
        Field::Atime => time(entry.extra.atime),
        Field::Ctime => time(entry.extra.ctime),
        Field::Perm => entry.perm_string(),
        // mc's mode is the plain octal, no zero padding - so a
        // `mode:3` column shows 755 rather than a clipped 075
        Field::Mode => format!("{:o}", entry.mode & 0o7777),
        Field::Nlink => number(entry.extra.nlink),
        Field::Ngid => number(entry.extra.gid.map(u64::from)),
        Field::Nuid => number(entry.extra.uid.map(u64::from)),
        Field::Owner => owner_label(entry.extra.uid, remote, true),
        Field::Group => owner_label(entry.extra.gid, remote, false),
        Field::Inode => number(entry.extra.inode),
    }
}

/// Clip or pad `text` to exactly `width` columns, on the side the field
/// wants it.
fn fit(text: &str, width: usize, right: bool) -> String {
    let len = text.chars().count();
    if len > width {
        return text.chars().take(width).collect();
    }
    let pad = " ".repeat(width - len);
    if right {
        format!("{pad}{text}")
    } else {
        format!("{text}{pad}")
    }
}

/// MC's user-defined listing: the panel draws whatever `listing_format`
/// asks for. A repeat count lays the field set out several times side by
/// side, filled column by column like the brief listing, so Down is
/// still "the next file".
fn draw_user_columns(
    frame: &mut Frame,
    inner: Rect,
    panel: &Panel,
    state: &mut TableState,
    chrome: &Chrome<'_>,
    git: Option<&GitStatus>,
) {
    let format = chrome.format;
    let sets = format.repeat.max(1);
    let body_height = inner.height.saturating_sub(1 + u16::from(chrome.mini));
    let rows = body_height.max(1) as usize;
    let per_page = rows * sets as usize;
    let set_width = (inner.width / sets).max(1);
    let layout = format.layout(set_width);
    let remote = panel.is_remote();

    // keep the cursor on screen, scrolling a whole column at a time
    let mut start = state.offset();
    if !start.is_multiple_of(rows) {
        start -= start % rows;
    }
    while panel.cursor < start {
        start = start.saturating_sub(rows);
    }
    while panel.cursor >= start + per_page {
        start += rows;
    }
    *state.offset_mut() = start;
    state.select(None); // cells are highlighted, not whole rows

    let header: Vec<Span> = (0..sets)
        .flat_map(|_| {
            layout.iter().map(|(item, width)| {
                let width = *width as usize;
                let text = match item {
                    Item::Field(field, _) => format!("{:^width$}", field.label()),
                    Item::Space => " ".repeat(width),
                    Item::Bar => "│".into(),
                };
                Span::styled(fit(&text, width, false), Style::new().fg(th().header_fg))
            })
        })
        .collect();
    frame.render_widget(
        Line::from(spaced(header, layout.len(), sets)).style(base_style()),
        Rect { height: 1, ..inner },
    );

    for row in 0..rows {
        let mut spans: Vec<Span> = Vec::new();
        for col in 0..sets as usize {
            let index = start + col * rows + row;
            let entry = panel.entries.get(index);
            let marked = entry.is_some_and(|e| panel.is_marked(e));
            let under_cursor = chrome.active && entry.is_some() && index == panel.cursor;
            let style = match entry {
                Some(entry) => cell_style(marked, under_cursor, entry_style(entry).1),
                None => base_style(),
            };
            for (item, width) in &layout {
                let width = *width as usize;
                let text = match (item, entry) {
                    (Item::Space, _) | (_, None) => " ".repeat(width),
                    (Item::Bar, _) => fit("│", width, false),
                    (Item::Field(field, _), Some(entry)) => {
                        let mut text = field_text(*field, entry, marked, remote);
                        // the git column only exists inside a work tree,
                        // and rides on the name like the other listings
                        if *field == Field::Name
                            && let Some(status) = git
                        {
                            let mark = status.marks.get(&entry.name).copied();
                            text = format!("{}{text}", mark.unwrap_or(' '));
                        }
                        fit(&text, width, field.right_aligned())
                    }
                };
                spans.push(Span::styled(text, style));
            }
        }
        let area = Rect {
            x: inner.x,
            y: inner.y + 1 + row as u16,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(
            Line::from(spaced(spans, layout.len(), sets)).style(base_style()),
            area,
        );
    }

    if chrome.mini && inner.height > 1 {
        let row = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(
            Line::from(entry_summary(panel))
                .style(Style::new().fg(th().header_fg).bg(th().panel_bg)),
            row,
        );
    }
}

fn base_style() -> Style {
    Style::new().fg(th().panel_fg).bg(th().panel_bg)
}

/// Put the one-column gap the built-in listings have between fields -
/// between columns of one set, not between the sets themselves.
fn spaced(spans: Vec<Span<'_>>, per_set: usize, sets: u16) -> Vec<Span<'_>> {
    let mut out = Vec::with_capacity(spans.len() * 2);
    for (i, span) in spans.into_iter().enumerate() {
        let last_of_set = per_set > 0 && (i + 1) % per_set == 0;
        let last = i + 1 == per_set * sets as usize;
        out.push(span);
        if !last_of_set && !last {
            out.push(Span::raw(" "));
        }
    }
    out
}

/// MC's brief listing: names only, in several columns. Filled column by
/// column, so Down still lands on the file drawn underneath - the whole
/// point of the layout. The visible window starts at a column boundary,
/// which is what makes paging feel like MC's.
fn draw_brief_columns(
    frame: &mut Frame,
    inner: Rect,
    panel: &Panel,
    state: &mut TableState,
    chrome: &Chrome<'_>,
    git: Option<&GitStatus>,
) {
    let cols = chrome.columns.max(1);
    let body_height = inner.height.saturating_sub(1 + u16::from(chrome.mini));
    let rows = body_height.max(1) as usize;
    let per_page = rows * cols as usize;
    let width = (inner.width / cols).max(1) as usize;

    // keep the cursor on screen, scrolling a whole column at a time
    let mut start = state.offset();
    if !start.is_multiple_of(rows) {
        start -= start % rows;
    }
    while panel.cursor < start {
        start = start.saturating_sub(rows);
    }
    while panel.cursor >= start + per_page {
        start += rows;
    }
    *state.offset_mut() = start;
    state.select(None); // cells are highlighted, not whole rows

    // header: one "Name" per column, like the single-column listing
    let header: Vec<Span> = (0..cols)
        .map(|_| {
            Span::styled(
                format!("{:^width$}", "Name"),
                Style::new().fg(th().header_fg),
            )
        })
        .collect();
    frame.render_widget(
        Line::from(header).style(Style::new().fg(th().panel_fg).bg(th().panel_bg)),
        Rect { height: 1, ..inner },
    );

    for row in 0..rows {
        let mut spans: Vec<Span> = Vec::new();
        for col in 0..cols as usize {
            let index = start + col * rows + row;
            match panel.entries.get(index) {
                None => spans.push(Span::raw(" ".repeat(width))),
                Some(entry) => {
                    let marked = panel.is_marked(entry);
                    let under_cursor = chrome.active && index == panel.cursor;
                    let mark = git.map(|g| g.marks.get(&entry.name).copied());
                    let (marker, base) = entry_style(entry);
                    let style = cell_style(marked, under_cursor, base);
                    // the git column only exists inside a work tree
                    let text = match mark {
                        Some(mark) => format!(
                            "{}{marker}{}",
                            mark.unwrap_or(' '),
                            entry.name.to_string_lossy()
                        ),
                        None => format!("{marker}{}", entry.name.to_string_lossy()),
                    };
                    let text: String = text.chars().take(width).collect();
                    spans.push(Span::styled(format!("{text:<width$}"), style));
                }
            }
        }
        let area = Rect {
            x: inner.x,
            y: inner.y + 1 + row as u16,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(
            Line::from(spans).style(Style::new().fg(th().panel_fg).bg(th().panel_bg)),
            area,
        );
    }

    if chrome.mini && inner.height > 1 {
        let row = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(
            Line::from(entry_summary(panel))
                .style(Style::new().fg(th().header_fg).bg(th().panel_bg)),
            row,
        );
    }
}

/// The one-line description of a panel's cursor entry: what the status
/// line shows for the active panel, and what the mini status shows for
/// each panel of its own.
fn entry_summary(panel: &Panel) -> String {
    match panel.selected() {
        Some(e) if e.is_parent() => "UP--DIR".to_string(),
        Some(e) => {
            let link = e
                .link_target
                .as_ref()
                .map(|t| format!(" -> {}", t.display()))
                .unwrap_or_default();
            format!(
                "{} {:>9} {}{}",
                e.perm_string(),
                e.size,
                e.name.to_string_lossy(),
                link
            )
        }
        None => String::new(),
    }
}

/// The Ctrl+X Q preview pane: renders the head of the file under the
/// other panel's cursor through the viewer's chunked line access.
fn draw_quick_view(frame: &mut Frame, area: Rect, qv: &mut QuickView, active: bool) {
    let title_style = if active {
        Style::new().fg(th().select_fg).bg(th().select_bg)
    } else {
        Style::new().fg(th().panel_fg).bg(th().panel_bg)
    };
    let title = match &qv.view {
        Some((path, _)) => format!(
            " Quick view: {} ",
            tail(
                &path.display().to_string(),
                (area.width as usize).saturating_sub(16),
            )
        ),
        None => " Quick view ".to_string(),
    };
    let block = Block::bordered()
        .style(Style::new().fg(th().panel_fg).bg(th().panel_bg))
        .title(Span::styled(title, title_style));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    qv.rows = inner.height as usize;

    match qv.view.as_mut() {
        Some((_, fv)) if qv.hex => {
            for row in 0..inner.height {
                let offset = (qv.top + row as usize) as u64 * 16;
                if offset >= fv.size {
                    break;
                }
                let bytes = fv.read_at(offset, 16).unwrap_or_default();
                let text: String = hex_row(offset, &bytes)
                    .chars()
                    .take(inner.width as usize)
                    .collect();
                frame.render_widget(
                    Line::from(text),
                    Rect {
                        y: inner.y + row,
                        height: 1,
                        ..inner
                    },
                );
            }
        }
        Some((_, fv)) => {
            for row in 0..inner.height {
                let Ok(Some(line)) = fv.line(qv.top + row as usize) else {
                    break;
                };
                let text: String = expand_line(&line)
                    .chars()
                    .take(inner.width as usize)
                    .collect();
                frame.render_widget(
                    Line::from(text),
                    Rect {
                        y: inner.y + row,
                        height: 1,
                        ..inner
                    },
                );
            }
        }
        None if !qv.note.is_empty() => {
            frame.render_widget(
                Line::from(qv.note.as_str())
                    .style(Style::new().fg(th().header_fg))
                    .centered(),
                Rect {
                    y: inner.y + inner.height / 2,
                    height: 1,
                    ..inner
                },
            );
        }
        None => {}
    }
}

/// `git`: None = no git column at all; Some(mark) = the panel is inside
/// a work tree, render a one-cell status column (mark or blank).
/// The type marker and colour for an entry: `/` a directory, `*`
/// executable, `@` a symlink, and so on.
/// The marker character and colour for an entry: the built-in look for
/// its kind, then whatever `[[highlight]]` says on top - `..` included,
/// since to mc it is a directory like any other. Rules are the
/// exception, so the usual path costs one `is_empty`.
fn entry_style(entry: &Entry) -> (&'static str, Style) {
    let (marker, mut style) = kind_style(entry);
    let rules = HIGHLIGHT.read().unwrap_or_else(|e| e.into_inner());
    if !rules.is_empty()
        && let Some(rule) = rules.iter().find(|rule| rule.matches(entry))
    {
        style = style.fg(rule.color);
        match rule.bold {
            Some(true) => style = style.add_modifier(Modifier::BOLD),
            Some(false) => style = style.remove_modifier(Modifier::BOLD),
            None => {}
        }
    }
    (marker, style)
}

fn kind_style(entry: &Entry) -> (&'static str, Style) {
    match entry.kind {
        EntryKind::Dir => (
            "/",
            Style::new().fg(th().dir_fg).add_modifier(Modifier::BOLD),
        ),
        EntryKind::SymlinkDir => (
            "~",
            Style::new().fg(th().dir_fg).add_modifier(Modifier::BOLD),
        ),
        EntryKind::SymlinkFile => ("@", Style::new().fg(th().panel_fg)),
        EntryKind::SymlinkBroken => ("!", Style::new().fg(th().broken_fg)),
        EntryKind::File if entry.is_executable() => ("*", Style::new().fg(th().exec_fg)),
        EntryKind::File => (" ", Style::new().fg(th().panel_fg)),
    }
}

/// Marked / under the cursor / both, over the entry's own colour.
fn cell_style(marked: bool, under_cursor: bool, base: Style) -> Style {
    match (marked, under_cursor) {
        (true, true) => Style::new()
            .fg(th().mark_fg)
            .bg(th().select_bg)
            .add_modifier(Modifier::BOLD),
        (true, false) => Style::new().fg(th().mark_fg).add_modifier(Modifier::BOLD),
        (false, true) => Style::new().fg(th().select_fg).bg(th().select_bg),
        (false, false) => base,
    }
}

fn entry_row(
    entry: &Entry,
    marked: bool,
    under_cursor: bool,
    git: Option<Option<char>>,
    mode: ListMode,
    remote: bool,
) -> Row<'_> {
    let (marker, base) = entry_style(entry);
    let style = cell_style(marked, under_cursor, base);

    let size = if entry.is_parent() {
        "UP--DIR".to_string()
    } else {
        format_size(entry.size)
    };
    let mtime = entry
        .mtime
        .map(|t| DateTime::<Local>::from(t).format("%b %e %H:%M").to_string())
        .unwrap_or_default();

    let name_text = format!("{marker}{}", entry.name.to_string_lossy());
    let name_cell = match git {
        None => Cell::from(name_text),
        Some(mark) => {
            let mark_style = if under_cursor {
                style
            } else {
                match mark {
                    Some('M') => Style::new().fg(th().mark_fg).add_modifier(Modifier::BOLD),
                    Some('A') => Style::new().fg(th().exec_fg).add_modifier(Modifier::BOLD),
                    Some('?') => Style::new().fg(th().header_fg),
                    _ => style,
                }
            };
            // dim ignored entries so build output fades into the background
            let name_style = if mark == Some('!') && !under_cursor && !marked {
                style.add_modifier(Modifier::DIM)
            } else {
                style
            };
            Cell::from(Line::from(vec![
                Span::styled(mark.unwrap_or(' ').to_string(), mark_style),
                Span::styled(name_text, name_style),
            ]))
        }
    };
    let size_cell = Cell::from(Line::from(size).right_aligned());
    match mode {
        // the tree and the user format have their own renderers; they
        // never build table rows
        ListMode::Brief | ListMode::Tree | ListMode::User => Row::new(vec![name_cell]),
        ListMode::Full => Row::new(vec![name_cell, size_cell, Cell::from(mtime)]),
        ListMode::Long => Row::new(vec![
            Cell::from(entry.perm_string()),
            Cell::from(owner_label(entry.extra.uid, remote, true)),
            Cell::from(owner_label(entry.extra.gid, remote, false)),
            size_cell,
            name_cell,
        ]),
    }
    .style(style)
}

/// Owner/group column text: resolved name locally, the bare id on
/// remote panels (the server's ids mean nothing to our passwd).
fn owner_label(id: Option<u32>, remote: bool, user: bool) -> String {
    match id {
        None => String::new(),
        Some(id) if remote => id.to_string(),
        Some(id) if user => user_name(id),
        Some(id) => group_name(id),
    }
}

/// The Ctrl+X i info pane: full stat of the file under the other
/// panel's cursor, plus the filesystem's free space.
fn draw_info(
    frame: &mut Frame,
    area: Rect,
    browse: &Panel,
    disk: Option<(u64, u64)>,
    active: bool,
) {
    let title_style = if active {
        Style::new().fg(th().select_fg).bg(th().select_bg)
    } else {
        Style::new().fg(th().panel_fg).bg(th().panel_bg)
    };
    let block = Block::bordered()
        .style(Style::new().fg(th().panel_fg).bg(th().panel_bg))
        .title(Span::styled(" Info ", title_style));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let time = |t: &Option<std::time::SystemTime>| {
        t.map(|t| {
            DateTime::<Local>::from(t)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "n/a".into())
    };
    let count = |n: &Option<u64>| n.map(|n| n.to_string()).unwrap_or_else(|| "n/a".into());
    let remote = browse.is_remote();

    let mut lines: Vec<String> = Vec::new();
    match browse.selected() {
        Some(e) if !e.is_parent() => {
            let kind = match e.kind {
                EntryKind::Dir => "directory".to_string(),
                EntryKind::File => "regular file".to_string(),
                EntryKind::SymlinkBroken => "broken symlink".to_string(),
                EntryKind::SymlinkDir | EntryKind::SymlinkFile => format!(
                    "symlink -> {}",
                    e.link_target
                        .as_ref()
                        .map(|t| t.display().to_string())
                        .unwrap_or_default()
                ),
            };
            lines.push(format!("Name:      {}", e.name.to_string_lossy()));
            lines.push(format!("Type:      {kind}"));
            lines.push(format!("Size:      {}  ({})", e.size, human_size(e.size)));
            lines.push(format!("Perms:     {}  ({:o})", e.perm_string(), e.mode));
            let owner = |id: &Option<u32>, user: bool| match id {
                None => "n/a".to_string(),
                Some(id) if remote => id.to_string(),
                Some(id) => format!(
                    "{} ({id})",
                    if user {
                        user_name(*id)
                    } else {
                        group_name(*id)
                    }
                ),
            };
            lines.push(format!("Owner:     {}", owner(&e.extra.uid, true)));
            lines.push(format!("Group:     {}", owner(&e.extra.gid, false)));
            lines.push(format!("Links:     {}", count(&e.extra.nlink)));
            lines.push(format!("Inode:     {}", count(&e.extra.inode)));
            lines.push(String::new());
            lines.push(format!("Modified:  {}", time(&e.mtime)));
            lines.push(format!("Accessed:  {}", time(&e.extra.atime)));
            lines.push(format!("Changed:   {}", time(&e.extra.ctime)));
        }
        _ => lines.push("(parent directory)".into()),
    }
    if let Some((free, total)) = disk {
        let pct = (free * 100).checked_div(total).unwrap_or(0);
        lines.push(String::new());
        lines.push(format!(
            "Space:     {} of {} free ({pct}%)",
            human_size(free),
            human_size(total)
        ));
    }

    for (row, text) in lines.iter().enumerate() {
        if row as u16 >= inner.height {
            break;
        }
        let text: String = text.chars().take(inner.width as usize).collect();
        frame.render_widget(
            Line::from(text),
            Rect {
                y: inner.y + row as u16,
                height: 1,
                ..inner
            },
        );
    }
}

/// "58.2G"-style human size (1024-based), one decimal below 100.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["K", "M", "G", "T"];
    if bytes < 1000 {
        return format!("{bytes}B");
    }
    let mut val = bytes as f64;
    let mut unit = 0;
    val /= 1024.0;
    while val >= 1000.0 && unit + 1 < UNITS.len() {
        val /= 1024.0;
        unit += 1;
    }
    if val >= 100.0 {
        format!("{val:.0}{}", UNITS[unit])
    } else {
        format!("{val:.1}{}", UNITS[unit])
    }
}

fn user_name(uid: u32) -> String {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u32, String>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(&uid) {
        return hit.clone();
    }
    let name = lookup_name(uid, true).unwrap_or_else(|| uid.to_string());
    cache.lock().unwrap().insert(uid, name.clone());
    name
}

fn group_name(gid: u32) -> String {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u32, String>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(&gid) {
        return hit.clone();
    }
    let name = lookup_name(gid, false).unwrap_or_else(|| gid.to_string());
    cache.lock().unwrap().insert(gid, name.clone());
    name
}

/// getpwuid_r / getgrgid_r, tolerating missing entries.
fn lookup_name(id: u32, user: bool) -> Option<String> {
    let mut buf = vec![0u8; 4096];
    unsafe {
        let name_ptr = if user {
            let mut pwd: libc::passwd = std::mem::zeroed();
            let mut out: *mut libc::passwd = std::ptr::null_mut();
            let rc = libc::getpwuid_r(
                id,
                &mut pwd,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut out,
            );
            if rc != 0 || out.is_null() {
                return None;
            }
            pwd.pw_name
        } else {
            let mut grp: libc::group = std::mem::zeroed();
            let mut out: *mut libc::group = std::ptr::null_mut();
            let rc = libc::getgrgid_r(
                id,
                &mut grp,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut out,
            );
            if rc != 0 || out.is_null() {
                return None;
            }
            grp.gr_name
        };
        Some(
            std::ffi::CStr::from_ptr(name_ptr)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Fits in 7 columns: plain bytes below 10M, then K/M/G like MC.
fn format_size(size: u64) -> String {
    if size < 10_000_000 {
        return size.to_string();
    }
    let kb = size / 1024;
    if kb < 10_000_000 {
        return format!("{kb}K");
    }
    let mb = kb / 1024;
    if mb < 10_000_000 {
        return format!("{mb}M");
    }
    format!("{}G", mb / 1024)
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let line = if let Some(msg) = &app.status {
        Line::from(msg.as_str()).style(Style::new().fg(th().error_fg).bg(th().error_bg))
    } else if !app.jobs.is_empty() && app.fg_job().is_none() {
        // background jobs: aggregate progress, C-x j for the list
        let (done, total) = app.jobs.iter().fold((0u64, 0u64), |(d, t), j| {
            (d + j.bytes_done, t + j.total_bytes)
        });
        let pct = (done * 100).checked_div(total).unwrap_or(0);
        Line::from(format!(
            " {} job(s) running - {pct}% - C-x j lists them ",
            app.jobs.len()
        ))
        .style(Style::new().fg(th().select_fg).bg(th().select_bg))
    } else if let Some(prefix) = &app.quick_search {
        Line::from(format!("Search: {prefix}"))
            .style(Style::new().fg(th().select_fg).bg(th().select_bg))
    } else if app.panels[app.active].list_mode == ListMode::Tree {
        // the listing is hidden, so its cursor entry would be a lie -
        // the tree's own selection is what the user is looking at
        let path = app.trees[app.active]
            .as_ref()
            .and_then(|tree| tree.selected())
            .map(|row| abbrev_home(&row.path))
            .unwrap_or_default();
        Line::from(path).style(Style::new().fg(th().panel_fg))
    } else {
        Line::from(entry_summary(&app.panels[app.active])).style(Style::new().fg(th().panel_fg))
    };
    frame.render_widget(line, area);
}

fn draw_cmdline(frame: &mut Frame, area: Rect, app: &App) {
    let prompt = tail(
        &format!("{}$ ", abbrev_home(&app.panels[app.active].local_cwd())),
        (area.width / 2) as usize,
    );
    let prompt_len = prompt.chars().count();
    let field_width = (area.width as usize).saturating_sub(prompt_len).max(1);

    let cl = &app.cmdline;
    let chars: Vec<char> = cl.value.chars().collect();
    let start = cl.cursor.saturating_sub(field_width.saturating_sub(1));
    let visible: String = chars[start..].iter().take(field_width).collect();

    frame.render_widget(
        Line::from(vec![
            Span::styled(prompt, Style::new().fg(th().prompt_fg)),
            Span::raw(visible),
        ]),
        area,
    );
    if app.dialog.is_none() && app.fg_job().is_none() {
        frame.set_cursor_position((area.x + (prompt_len + cl.cursor - start) as u16, area.y));
    }
}

fn abbrev_home(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

const KEYBAR: [(&str, &str); 10] = [
    ("1", "Help"),
    ("2", "Menu"),
    ("3", "View"),
    ("4", "Edit"),
    ("5", "Copy"),
    ("6", "RenMov"),
    ("7", "Mkdir"),
    ("8", "Delete"),
    ("9", "PullDn"),
    ("10", "Quit"),
];

/// MC's permanent menu bar. F9 opens the same menus either way; this is
/// the always-visible row of titles, clickable like MC's.
fn draw_menubar(frame: &mut Frame, area: Rect, open: Option<usize>) {
    use crate::app::MENUS;
    let base = Style::new().fg(th().label_fg).bg(th().label_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    // Same "  title  " spacing as the open menu's own title row, which
    // is what `menu_layout` hit-tests clicks against: drawn any other
    // way, a click on this bar lands on the neighbouring menu.
    let mut spans = Vec::new();
    for (i, (title, _)) in MENUS.iter().enumerate() {
        let style = if open == Some(i) { sel } else { base };
        spans.push(Span::styled("  ", style));
        hot_spans(title, style, &mut spans);
        spans.push(Span::styled("  ", style));
    }
    frame.render_widget(Line::from(spans).style(base), area);
}

fn draw_keybar(frame: &mut Frame, area: Rect) {
    let mut spans = Vec::with_capacity(KEYBAR.len() * 2);
    for (num, label) in KEYBAR {
        spans.push(Span::styled(
            format!("{num:>2}"),
            Style::new().fg(th().key_fg).bg(th().key_bg),
        ));
        spans.push(Span::styled(
            format!("{label:<6}"),
            Style::new().fg(th().label_fg).bg(th().label_bg),
        ));
    }
    frame.render_widget(Line::from(spans), area);
}

fn draw_help(frame: &mut Frame, app: &mut App) {
    let Some(help) = app.help.as_mut() else {
        return;
    };
    let [title_area, content, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    help.rows = content.height as usize;
    help.top = help
        .top
        .min(HELP_TEXT.len().saturating_sub(help.rows.max(1)));

    let base = Style::new().fg(th().help_fg).bg(th().help_bg);
    let width = content.width as usize;
    frame.render_widget(
        Line::from(format!("{:<width$}", " Help - rcmd")).style(base.add_modifier(Modifier::BOLD)),
        title_area,
    );
    frame.render_widget(Block::new().style(base), content);
    for row in 0..content.height {
        let Some(text) = HELP_TEXT.get(help.top + row as usize) else {
            break;
        };
        let row_area = Rect {
            y: content.y + row,
            height: 1,
            ..content
        };
        let (text, style) = match text.strip_prefix("# ") {
            Some(header) => (
                format!(" {header}"),
                base.fg(th().help_header_fg).add_modifier(Modifier::BOLD),
            ),
            None => ((*text).to_string(), base),
        };
        frame.render_widget(Line::from(text).style(style), row_area);
    }
    frame.render_widget(
        Line::from(format!(
            "{:<width$}",
            " Esc/F1/q close   arrows/PgUp/PgDn scroll"
        ))
        .style(base),
        bottom,
    );
}

/// Screen column of character `col` in `text`, with 8-wide tab stops -
/// must match how [`draw_editor`] expands lines.
pub fn screen_col(text: &str, col: usize) -> usize {
    let mut scol = 0usize;
    for c in text.chars().take(col) {
        scol += match c {
            '\t' => 8 - scol % 8,
            _ => 1,
        };
    }
    scol
}

/// Wrapped-segment count of an editor line at `cols` wide (always ≥ 1;
/// an exact-multiple width gets an extra row so the cursor can sit at
/// the line end).
pub fn ed_line_segs(ed: &rcmd_edit::Editor, line: usize, cols: usize) -> usize {
    screen_col(&ed.line(line), ed.line_len(line)) / cols.max(1) + 1
}

/// One editor line as styled spans: syntax colors, selection overlay,
/// tab expansion and horizontal clipping in a single pass.
#[allow(clippy::too_many_arguments)]
fn editor_line(
    text: &str,
    spans: &[(usize, usize, [u8; 3])],
    sel: Option<(usize, usize)>,
    left: usize,
    cols: usize,
    base: Style,
    sel_style: Style,
) -> Line<'static> {
    let mut out: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style = base;
    let mut span_i = 0usize;
    let mut scol = 0usize;
    let flush = |run: &mut String, style: Style, out: &mut Vec<Span>| {
        if !run.is_empty() {
            out.push(Span::styled(std::mem::take(run), style));
        }
    };
    for (idx, c) in text.chars().chain(std::iter::once(' ')).enumerate() {
        // the trailing space stands in for the newline cell so a
        // selection that spans lines shows on the line end
        if scol >= left + cols {
            break;
        }
        while span_i < spans.len() && spans[span_i].1 <= idx {
            span_i += 1;
        }
        let mut style = base;
        if let Some(&(a, b, rgb)) = spans.get(span_i)
            && idx >= a
            && idx < b
        {
            style = base.fg(Color::Rgb(rgb[0], rgb[1], rgb[2]));
        }
        if let Some((a, b)) = sel
            && idx >= a
            && idx < b
        {
            style = sel_style;
        }
        if style != run_style {
            flush(&mut run, run_style, &mut out);
            run_style = style;
        }
        let width = match c {
            '\t' => 8 - scol % 8,
            _ => 1,
        };
        for k in 0..width {
            if scol + k >= left && scol + k < left + cols {
                run.push(match c {
                    '\t' => ' ',
                    c if (c as u32) < 0x20 => '\u{b7}',
                    c => c,
                });
                if c != '\t' && (c as u32) >= 0x20 {
                    break; // normal chars occupy one cell
                }
            }
        }
        scol += width;
    }
    flush(&mut run, run_style, &mut out);
    Line::from(out)
}

fn draw_editor(frame: &mut Frame, app: &mut App) {
    let Some(st) = app.editor.as_mut() else {
        return;
    };
    let [title_area, content, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    st.rows = content.height as usize;
    st.cols = content.width as usize;

    let base = Style::new().fg(th().panel_fg).bg(th().panel_bg);
    let bar = Style::new().fg(th().select_fg).bg(th().select_bg);
    let sel_style = Style::new().fg(th().select_fg).bg(th().select_bg);

    let modified = if st.ed.modified() { " [+]" } else { "" };
    let pos = format!(
        " {}:{}  {} lines ",
        st.ed.cursor.line + 1,
        st.ed.cursor.col + 1,
        st.ed.line_count(),
    );
    let title = format!(" {}{modified}", st.title);
    frame.render_widget(
        Line::from(format!(
            "{title}{pos:>w$}",
            w = (title_area.width as usize).saturating_sub(title.chars().count())
        ))
        .style(bar),
        title_area,
    );

    frame.render_widget(ratatui::widgets::Block::new().style(base), content);
    let rows = st.rows;
    let all_spans = match st.hl.as_mut() {
        Some(hl) => hl.range_spans(&mut st.ed, st.top, rows),
        None => vec![Vec::new(); rows],
    };
    if st.wrap {
        // soft-wrap: walk (line, segment) pairs from the top row; each
        // segment reuses the clipping renderer with its own left edge
        let cols = st.cols.max(1);
        let empty: Vec<(usize, usize, [u8; 3])> = Vec::new();
        let mut line_idx = st.top;
        let mut seg = st.top_seg;
        for row in 0..rows {
            if line_idx >= st.ed.line_count() {
                break;
            }
            let row_area = Rect {
                y: content.y + row as u16,
                height: 1,
                ..content
            };
            let text = st.ed.line(line_idx);
            let spans = all_spans.get(line_idx - st.top).unwrap_or(&empty);
            let line = editor_line(
                &text,
                spans,
                st.ed.sel_on_line(line_idx),
                seg * cols,
                cols,
                base,
                sel_style,
            );
            frame.render_widget(line, row_area);
            if line_idx == st.ed.cursor.line {
                let scol = screen_col(&text, st.ed.cursor.col);
                if scol / cols == seg {
                    frame.set_cursor_position((
                        content.x + (scol % cols) as u16,
                        content.y + row as u16,
                    ));
                }
            }
            seg += 1;
            if seg >= ed_line_segs(&st.ed, line_idx, cols) {
                line_idx += 1;
                seg = 0;
            }
        }
    } else {
        for (row, spans) in all_spans.iter().enumerate().take(rows) {
            let idx = st.top + row;
            if idx >= st.ed.line_count() {
                break;
            }
            let row_area = Rect {
                y: content.y + row as u16,
                height: 1,
                ..content
            };
            let text = st.ed.line(idx);
            let line = editor_line(
                &text,
                spans,
                st.ed.sel_on_line(idx),
                st.left,
                st.cols,
                base,
                sel_style,
            );
            frame.render_widget(line, row_area);
        }
        // hardware cursor on the edit position
        let cur_line = st.ed.line(st.ed.cursor.line);
        let scol = screen_col(&cur_line, st.ed.cursor.col);
        if st.ed.cursor.line >= st.top
            && st.ed.cursor.line < st.top + rows
            && scol >= st.left
            && scol < st.left + st.cols
        {
            frame.set_cursor_position((
                content.x + (scol - st.left) as u16,
                content.y + (st.ed.cursor.line - st.top) as u16,
            ));
        }
    }

    let help =
        " F2 Save  F3 Mark  F4 Replace  F5/F6 CopyMove  F7 Search  F8 DelLine  M-w Wrap  F10 Quit ";
    let note = st.note.clone().unwrap_or_default();
    frame.render_widget(
        Line::from(format!(
            "{help}{note:>w$}",
            w = (bottom.width as usize).saturating_sub(help.chars().count())
        ))
        .style(bar),
        bottom,
    );

    match &st.prompt {
        None => {}
        Some(EditPrompt::Search { value, cursor }) => {
            let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let inner = popup(
                frame,
                centered(50, 5, frame.area()),
                " Search (regex) ",
                style,
            );
            draw_field(frame, inner, value, *cursor);
        }
        Some(EditPrompt::ReplaceFind { value, cursor }) => {
            let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let inner = popup(
                frame,
                centered(50, 5, frame.area()),
                " Replace (regex) ",
                style,
            );
            draw_field(frame, inner, value, *cursor);
        }
        Some(EditPrompt::ReplaceWith { value, cursor, .. }) => {
            let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let inner = popup(
                frame,
                centered(50, 5, frame.area()),
                " Replace with ",
                style,
            );
            draw_field(frame, inner, value, *cursor);
        }
        Some(EditPrompt::ConfirmReplace { count, button, .. }) => {
            let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
            let inner = popup(frame, centered(56, 6, frame.area()), " Replace? ", style);
            let row = |offset: u16| Rect {
                x: inner.x + 1,
                y: inner.y + offset,
                width: inner.width.saturating_sub(2),
                height: 1,
            };
            frame.render_widget(
                Line::from(format!("{count} replaced so far")).centered(),
                row(1),
            );
            frame.render_widget(
                buttons_line(&["Replace", "Skip", "All", "Quit"], *button, style, sel),
                row(3),
            );
        }
        Some(EditPrompt::ConfirmQuit { button }) => {
            let style = Style::new().fg(th().error_fg).bg(th().error_bg);
            let sel = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let inner = popup(
                frame,
                centered(56, 6, frame.area()),
                " Unsaved changes ",
                style,
            );
            let row = |offset: u16| Rect {
                x: inner.x + 1,
                y: inner.y + offset,
                width: inner.width.saturating_sub(2),
                height: 1,
            };
            frame.render_widget(
                Line::from("The file was modified. Save it?").centered(),
                row(1),
            );
            frame.render_widget(
                buttons_line(&["Save", "Discard", "Cancel"], *button, style, sel),
                row(3),
            );
        }
    }
}

fn draw_viewer(frame: &mut Frame, app: &mut App) {
    let Some(v) = app.viewer.as_mut() else { return };
    let [title_area, content, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    v.rows = content.height as usize;
    let width = content.width as usize;
    v.cols = width.max(1);

    let offset = if v.hex {
        v.hex_top * 16
    } else {
        v.file.offset_of_line(v.top).unwrap_or(0)
    };
    let percent = (offset * 100).checked_div(v.file.size).unwrap_or(100);
    let mode = if v.hex {
        "hex"
    } else if v.wrap {
        "wrap"
    } else {
        "text"
    };
    let follow = if v.follow { " [follow]" } else { "" };
    let title = format!(
        " {}  {} bytes  {percent}%  [{mode}]{follow}",
        v.path.display(),
        v.file.size,
    );
    frame.render_widget(
        Line::from(format!("{:<w$}", tail(&title, width), w = width))
            .style(Style::new().fg(th().select_fg).bg(th().select_bg)),
        title_area,
    );

    // syntax spans for the visible line range (empty without a
    // recognized syntax); search matches are overlaid per line below
    let needle = v.search.clone();
    let all_spans = match v.hl.as_mut() {
        Some(hl) => hl.range_spans(&mut FileLines(&mut v.file), v.top, content.height as usize),
        None => Vec::new(),
    };
    let styled =
        |v: &mut crate::app::Viewer, idx: usize, all_spans: &[Vec<(usize, usize, [u8; 3])>]| {
            let text = match v.file.line(idx) {
                Ok(Some(text)) => text,
                _ => return None,
            };
            let (expanded, map) = expand_with_map(&text);
            let clamp = |i: usize| map[i.min(map.len() - 1)];
            let spans: Vec<(usize, usize, [u8; 3])> = all_spans
                .get(idx.saturating_sub(v.top))
                .map(|sp| {
                    sp.iter()
                        .map(|&(a, b, rgb)| (clamp(a), clamp(b), rgb))
                        .collect()
                })
                .unwrap_or_default();
            let matches = if needle.is_empty() {
                Vec::new()
            } else {
                match_ranges(&expanded, &needle)
            };
            let base = if v.found == Some(idx) {
                Style::new().fg(th().mark_fg).add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            Some((expanded, spans, matches, base))
        };
    if v.wrap && !v.hex {
        // soft-wrapped: walk (line, segment) pairs from the top row
        let mut line_idx = v.top;
        let mut seg = v.top_seg;
        for row in 0..content.height {
            let row_area = Rect {
                y: content.y + row,
                height: 1,
                ..content
            };
            let Some((expanded, spans, matches, base)) = styled(v, line_idx, &all_spans) else {
                break;
            };
            let len = expanded.chars().count();
            if seg * width > len {
                seg = 0; // stale segment after a resize
            }
            let start = (seg * width).min(len);
            frame.render_widget(
                viewer_line(&expanded, &spans, &matches, start, width, base),
                row_area,
            );
            if (seg + 1) * width < len.max(1) {
                seg += 1;
            } else {
                line_idx += 1;
                seg = 0;
            }
        }
    } else {
        for row in 0..content.height {
            let row_area = Rect {
                y: content.y + row,
                height: 1,
                ..content
            };
            if v.hex {
                let row_offset = (v.hex_top + row as u64) * 16;
                if row_offset >= v.file.size {
                    break;
                }
                let bytes = v.file.read_at(row_offset, 16).unwrap_or_default();
                frame.render_widget(Line::from(hex_row(row_offset, &bytes)), row_area);
            } else {
                let idx = v.top + row as usize;
                let Some((expanded, spans, matches, base)) = styled(v, idx, &all_spans) else {
                    break;
                };
                frame.render_widget(
                    viewer_line(&expanded, &spans, &matches, v.left, width, base),
                    row_area,
                );
            }
        }
    }

    let help = " F3/q Quit  F2 Wrap  F4 Hex  F7|/ Search  n Next  ←→ Scroll ";
    let note = v.note.clone().unwrap_or_default();
    frame.render_widget(
        Line::from(format!(
            "{help}{:>w$}",
            note,
            w = (bottom.width as usize).saturating_sub(help.chars().count())
        ))
        .style(Style::new().fg(th().select_fg).bg(th().select_bg)),
        bottom,
    );

    if let Some((value, cursor)) = &v.prompt {
        let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
        let area = centered(50, 5, frame.area());
        let inner = popup(frame, area, " Search ", style);
        draw_field(frame, inner, value, *cursor);
    }
}

/// Expand tabs to 8-column stops and hide control characters.
/// Adapter: the viewer's chunked file as a highlighter line source.
/// Only files under the highlighter's 2 MB gate ever get here, so the
/// full index walk behind `total_lines` is cheap.
struct FileLines<'a>(&'a mut rcmd_core::view::FileView);

impl rcmd_edit::LineSource for FileLines<'_> {
    fn line_count(&mut self) -> usize {
        self.0.total_lines().unwrap_or(0)
    }

    fn line_with_nl(&mut self, idx: usize) -> String {
        match self.0.line(idx) {
            Ok(Some(mut s)) => {
                s.push('\n');
                s
            }
            _ => String::new(),
        }
    }
}

/// Like [`expand_line`], also returning raw-char → expanded-char
/// offsets (length = raw chars + 1) so span columns survive tab
/// expansion.
fn expand_with_map(text: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(text.len());
    let mut map = Vec::with_capacity(text.len() + 1);
    let mut col = 0usize;
    for c in text.chars() {
        map.push(col);
        match c {
            '\t' => {
                let pad = 8 - col % 8;
                out.extend(std::iter::repeat_n(' ', pad));
                col += pad;
            }
            c if (c as u32) < 0x20 => {
                out.push('\u{b7}');
                col += 1;
            }
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    map.push(col);
    (out, map)
}

/// Case-insensitive occurrences of `needle` in `text`, as char ranges
/// (char-wise lowercase - the same approximation the search itself
/// uses).
fn match_ranges(text: &str, needle: &str) -> Vec<(usize, usize)> {
    let ned: Vec<char> = needle
        .chars()
        .filter_map(|c| c.to_lowercase().next())
        .collect();
    if ned.is_empty() {
        return Vec::new();
    }
    let hay: Vec<char> = text
        .chars()
        .filter_map(|c| c.to_lowercase().next())
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + ned.len() <= hay.len() {
        if hay[i..i + ned.len()] == ned[..] {
            out.push((i, i + ned.len()));
            i += ned.len();
        } else {
            i += 1;
        }
    }
    out
}

/// One viewer row: syntax colors with the search matches overlaid,
/// windowed to `[start, start+width)` of the expanded text.
fn viewer_line(
    expanded: &str,
    spans: &[(usize, usize, [u8; 3])],
    matches: &[(usize, usize)],
    start: usize,
    width: usize,
    base: Style,
) -> Line<'static> {
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let mut out: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style = base;
    for (i, c) in expanded.chars().enumerate().skip(start).take(width) {
        let mut style = base;
        if let Some(&(_, _, rgb)) = spans.iter().find(|&&(a, b, _)| i >= a && i < b) {
            style = base.fg(Color::Rgb(rgb[0], rgb[1], rgb[2]));
        }
        if matches.iter().any(|&(a, b)| i >= a && i < b) {
            style = sel;
        }
        if style != run_style {
            if !run.is_empty() {
                out.push(Span::styled(std::mem::take(&mut run), run_style));
            }
            run_style = style;
        }
        run.push(c);
    }
    if !run.is_empty() {
        out.push(Span::styled(run, run_style));
    }
    Line::from(out)
}

pub fn expand_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut col = 0usize;
    for c in text.chars() {
        match c {
            '\t' => {
                let pad = 8 - col % 8;
                out.extend(std::iter::repeat_n(' ', pad));
                col += pad;
            }
            c if (c as u32) < 0x20 => {
                out.push('·');
                col += 1;
            }
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

fn hex_row(offset: u64, bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(50);
    for i in 0..16 {
        if i == 8 {
            hex.push(' ');
        }
        match bytes.get(i) {
            Some(b) => hex.push_str(&format!("{b:02X} ")),
            None => hex.push_str("   "),
        }
    }
    let ascii: String = bytes
        .iter()
        .map(|&b| {
            if (0x20..0x7F).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("{offset:08X}  {hex} |{ascii}|")
}

/// Menu-bar geometry: (x, width) of every title in the top bar plus the
/// dropdown rect of the open menu - shared by drawing and mouse clicks.
/// Rendered length of a menu label, without its `&` hotkey marker.
fn label_len(label: &str) -> usize {
    let (pre, hot, post) = menu_label(label);
    pre.chars().count() + usize::from(hot.is_some()) + post.chars().count()
}

/// The label as spans, hotkey letter highlighted MC-style.
fn hot_spans(label: &str, style: Style, spans: &mut Vec<Span<'static>>) {
    let (pre, hot, post) = menu_label(label);
    spans.push(Span::styled(pre.to_string(), style));
    if let Some(c) = hot {
        spans.push(Span::styled(
            c.to_string(),
            style.fg(th().mark_fg).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(post.to_string(), style));
}

pub fn menu_layout(menu: usize, area: Rect) -> (Vec<(u16, u16)>, Rect) {
    let mut titles = Vec::new();
    let mut x = 0u16;
    for (title, _) in MENUS {
        let width = (label_len(title) + 4) as u16;
        titles.push((area.x + x, width));
        x += width;
    }
    let entries = MENUS[menu].1;
    let label_w = entries
        .iter()
        .flatten()
        .map(|(l, ..)| label_len(l))
        .max()
        .unwrap_or(0);
    let keys_w = entries
        .iter()
        .flatten()
        .map(|(_, k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    let width = (label_w + keys_w + 5) as u16;
    let dropdown = Rect {
        x: titles[menu].0.min(area.width.saturating_sub(width)),
        y: area.y + 1,
        width,
        height: (entries.len() as u16 + 2).min(area.height.saturating_sub(1)),
    };
    (titles, dropdown)
}

fn draw_menu(frame: &mut Frame, ms: &MenuState) {
    let area = frame.area();
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let (titles, dropdown) = menu_layout(ms.menu, area);

    let bar = Rect { height: 1, ..area };
    frame.render_widget(Clear, bar);
    let mut spans = Vec::new();
    for (i, (title, _)) in MENUS.iter().enumerate() {
        let style = if i == ms.menu { sel } else { base };
        spans.push(Span::styled("  ", style));
        hot_spans(title, style, &mut spans);
        spans.push(Span::styled("  ", style));
    }
    let used = titles.last().map(|(x, w)| x + w).unwrap_or(0);
    spans.push(Span::styled(
        " ".repeat((bar.width as usize).saturating_sub(used as usize)),
        base,
    ));
    frame.render_widget(Line::from(spans), bar);

    let entries = MENUS[ms.menu].1;
    let label_w = entries
        .iter()
        .flatten()
        .map(|(l, ..)| label_len(l))
        .max()
        .unwrap_or(0);
    let keys_w = entries
        .iter()
        .flatten()
        .map(|(_, k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    frame.render_widget(Clear, dropdown);
    let block = Block::bordered().style(base);
    let inner = block.inner(dropdown);
    frame.render_widget(block, dropdown);
    for (i, entry) in entries.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let row = Rect {
            y: inner.y + i as u16,
            height: 1,
            ..inner
        };
        match entry {
            None => frame.render_widget(
                Line::from("─".repeat(inner.width as usize)).style(base),
                row,
            ),
            Some((label, keys, _)) => {
                let style = if i == ms.item { sel } else { base };
                let mut spans = vec![Span::styled(" ", style)];
                hot_spans(label, style, &mut spans);
                let pad = label_w.saturating_sub(label_len(label));
                spans.push(Span::styled(format!("{:pad$} {keys:>keys_w$} ", ""), style));
                frame.render_widget(Line::from(spans), row);
            }
        }
    }
}

fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn popup(frame: &mut Frame, area: Rect, title: &str, style: Style) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(title).style(style);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Keep the tail of long paths visible; the tail is the interesting part.
fn tail(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        text.to_string()
    } else {
        let cut: String = chars[chars.len() - max.saturating_sub(1)..]
            .iter()
            .collect();
        format!("…{cut}")
    }
}

fn buttons_line(labels: &[&str], selected: usize, base: Style, sel: Style) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        spans.push(Span::styled(
            format!("[ {label} ]"),
            if i == selected { sel } else { base },
        ));
        spans.push(Span::styled(" ", base));
    }
    Line::from(spans).centered()
}

fn draw_input(frame: &mut Frame, d: &InputDialog) {
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let area = centered(64, 5, frame.area());
    let inner = popup(frame, area, &d.title, style);
    draw_field(frame, inner, &d.value, d.cursor);
}

/// Editable text field on the first inner row of a dialog.
fn draw_field(frame: &mut Frame, inner: Rect, value: &str, cursor: usize) {
    let field = Rect {
        x: inner.x + 1,
        y: inner.y + 1,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    field_row(frame, field, value, Some(cursor));
}

/// One editable line; the terminal cursor is placed only when focused.
fn field_row(frame: &mut Frame, field: Rect, value: &str, cursor: Option<usize>) {
    let width = field.width as usize;
    let cur = cursor.unwrap_or(0);
    let chars: Vec<char> = value.chars().collect();
    let start = cur.saturating_sub(width.saturating_sub(1));
    let visible: String = chars[start..].iter().take(width).collect();
    frame.render_widget(
        Line::from(format!("{visible:<width$}"))
            .style(Style::new().fg(th().select_fg).bg(th().select_bg)),
        field,
    );
    if let Some(cur) = cursor {
        frame.set_cursor_position((field.x + (cur - start) as u16, field.y));
    }
}

/// F9 > Options > Panel options - the MC-style checkbox form.
fn draw_options(frame: &mut Frame, d: &OptionsDialog) {
    use crate::app::{OPTION_ROWS, OptRow};
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let head = Style::new().fg(th().header_fg).bg(th().dialog_bg);
    // rows + a blank line + the button row, inside the border
    let area = centered(46, OPTION_ROWS.len() as u16 + 4, frame.area());
    let inner = popup(frame, area, " Options ", base);
    let check = |on: bool| if on { "[x]" } else { "[ ]" };
    let radio = |on: bool| if on { "(*)" } else { "( )" };

    for (i, entry) in OPTION_ROWS.iter().enumerate() {
        let row = Rect {
            x: inner.x + 1,
            y: inner.y + i as u16,
            width: inner.width.saturating_sub(2),
            height: 1,
        };
        let width = row.width as usize;
        let (text, style) = match entry {
            OptRow::Head(title) => (format!(" {title}"), head),
            OptRow::Check(opt, label) => (
                format!(" {} {label}", check(d.get(*opt))),
                if d.cursor == i { sel } else { base },
            ),
            OptRow::Ratio(label) => (
                format!(" {label}  {:>3}%  (Left/Right)", d.ratio),
                if d.cursor == i { sel } else { base },
            ),
            OptRow::Radio(opt, label, off, on) => (
                format!(
                    " {label}  {} {off}  {} {on}",
                    radio(!d.get(*opt)),
                    radio(d.get(*opt))
                ),
                if d.cursor == i { sel } else { base },
            ),
        };
        frame.render_widget(Line::from(format!("{text:<width$}")).style(style), row);
    }

    let buttons = Rect {
        x: inner.x,
        y: inner.y + OPTION_ROWS.len() as u16 + 1,
        width: inner.width,
        height: 1,
    };
    let selected = if d.cursor == OPTION_ROWS.len() {
        usize::from(!d.ok)
    } else {
        usize::MAX // neither highlighted while an option row is focused
    };
    frame.render_widget(
        buttons_line(&["OK", "Cancel"], selected, base, sel),
        buttons,
    );
}

fn draw_find(frame: &mut Frame, d: &FindDialog) {
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let area = centered(64, 9, frame.area());
    let inner = popup(frame, area, " Find file ", style);
    let row = |offset: u16| Rect {
        x: inner.x + 1,
        y: inner.y + offset,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    frame.render_widget(Line::from("Filename glob:"), row(0));
    field_row(
        frame,
        row(1),
        &d.name,
        (d.field == 0).then_some(d.name_cursor),
    );
    frame.render_widget(Line::from("Containing text (optional):"), row(3));
    field_row(
        frame,
        row(4),
        &d.content,
        (d.field == 1).then_some(d.content_cursor),
    );
    let tick = if d.skip_ignored { 'x' } else { ' ' };
    frame.render_widget(
        Line::from(format!("[{tick}] Skip gitignored files")).style(if d.field == 2 {
            sel
        } else {
            style
        }),
        row(5),
    );
    frame.render_widget(
        Line::from("Tab - switch   Space - toggle   Enter - search   Esc - cancel")
            .centered()
            .style(style),
        row(6),
    );
}

/// The F2 user menu: `[[commands]]` from the config, first nine with
/// digit hotkeys.
fn draw_user_menu(frame: &mut Frame, commands: &[crate::config::UserCommand], selected: usize) {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let rows = commands.len().max(1) as u16;
    let area = centered(60, (rows + 2).min(20), frame.area());
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" User menu ")
        .title_bottom(Line::from(" Enter or 1-9 runs ").centered())
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let name_w = commands
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(24);
    for (i, cmd) in commands.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let row = Rect {
            y: inner.y + i as u16,
            height: 1,
            ..inner
        };
        let hotkey = if i < 9 {
            format!("{}", i + 1)
        } else {
            " ".into()
        };
        let text: String = format!(" {hotkey} {:<name_w$}  {}", cmd.name, cmd.run)
            .chars()
            .take(inner.width as usize)
            .collect();
        frame.render_widget(
            Line::from(format!("{text:<w$}", w = inner.width as usize)).style(if i == selected {
                sel
            } else {
                base
            }),
            row,
        );
    }
}

/// One line of the figure: the trunk, drawn from the ancestors' "does
/// this level still continue below" flags, then this row's corner, then
/// the name. The root draws as its bare path.
fn tree_line(row: &rcmd_core::tree::Row) -> String {
    let mut line = String::new();
    for continues in &row.trunk {
        line.push_str(if *continues { "│  " } else { "   " });
    }
    if row.depth > 0 {
        line.push_str(if row.last { "└─ " } else { "├─ " });
    }
    line.push_str(&row.name);
    line
}

/// The figure itself, scrolled to keep the cursor on screen and padded
/// so the selected row highlights across the full width. Shared by the
/// panel's tree mode and the tree dialog.
fn draw_tree_rows(frame: &mut Frame, area: Rect, tree: &Tree, base: Style, selected: Style) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let height = area.height as usize;
    let width = area.width as usize;
    let cursor = tree.cursor();
    let first = tree.first_visible(height);
    for (i, row) in tree.rows().iter().enumerate().skip(first).take(height) {
        let mut text = format!(" {}", tree_line(row));
        let len = text.chars().count();
        if len > width {
            text = text.chars().take(width).collect();
        } else {
            text.push_str(&" ".repeat(width - len));
        }
        let rect = Rect {
            x: area.x,
            y: area.y + (i - first) as u16,
            width: area.width,
            height: 1,
        };
        let style = if i == cursor { selected } else { base };
        frame.render_widget(Line::from(text).style(style), rect);
    }
}

/// F9 > Command > Directory tree. Enter takes the current panel to the
/// selected directory and closes - the panel's own tree mode is the one
/// that stays open and moves the *other* panel.
/// F5/F6: MC's copy/move form. The destination on top, the switches
/// that change what the copy does under it, then OK / Background /
/// Cancel - Background starts the job detached, which is otherwise only
/// reachable by pressing b once it is already running.
fn draw_transfer(frame: &mut Frame, d: &crate::app::TransferDialog) {
    use crate::app::{TRANSFER_DEST_ROW, TRANSFER_OPTS, TRANSFER_ROWS};
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    // mask + destination + one row per option + a blank + the buttons
    let area = centered(64, TRANSFER_OPTS.len() as u16 + 6, frame.area());
    let inner = popup(frame, area, &d.title, base);
    let row_at = |i: u16| Rect {
        x: inner.x + 1,
        y: inner.y + i,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    let width = inner.width.saturating_sub(2) as usize;

    // MC asks for the source mask first, then where it all goes
    let field = |text: &str, label: &str| {
        let room = width.saturating_sub(label.len());
        format!("{label}{:<room$}", tail(text, room))
    };
    frame.render_widget(
        Line::from(field(&d.mask, " mask ")).style(if d.row == 0 { sel } else { base }),
        row_at(0),
    );
    frame.render_widget(
        Line::from(field(&d.dest, " to   ")).style(if d.row == TRANSFER_DEST_ROW {
            sel
        } else {
            base
        }),
        row_at(TRANSFER_DEST_ROW as u16),
    );
    for (i, (label, _)) in TRANSFER_OPTS.iter().enumerate() {
        let mark = if d.checked(i) { "[x]" } else { "[ ]" };
        let text = format!(" {mark} {label}");
        let row = TRANSFER_DEST_ROW + 1 + i;
        frame.render_widget(
            Line::from(format!("{text:<width$}")).style(if d.row == row { sel } else { base }),
            row_at(row as u16),
        );
    }
    let buttons = ["OK", "Background", "Cancel"];
    let selected = if d.row == TRANSFER_ROWS {
        d.button
    } else {
        usize::MAX
    };
    frame.render_widget(
        buttons_line(&buttons, selected, base, sel),
        row_at(TRANSFER_ROWS as u16 + 1),
    );
    // the cursor sits in whichever of the two text rows has the focus
    let text_row = match d.row {
        0 => Some((0u16, d.mask_cursor)),
        TRANSFER_DEST_ROW => Some((TRANSFER_DEST_ROW as u16, d.cursor)),
        _ => None,
    };
    if let Some((row, cursor)) = text_row {
        let x = inner.x + 7 + (cursor.min(width.saturating_sub(8)) as u16);
        frame.set_cursor_position((x, inner.y + row));
    }
}

fn draw_tree_dialog(frame: &mut Frame, tree: &Tree) {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let selected = Style::new().fg(th().select_fg).bg(th().select_bg);
    let area = centered(60, crate::app::TREE_ROWS as u16 + 2, frame.area());
    frame.render_widget(Clear, area);
    let mode = if tree.dynamic() { "dynamic" } else { "static" };
    let hint = if tree.search.is_empty() {
        format!(" Enter cd · F2 rescan · F3 forget · F4 {mode} ")
    } else {
        format!(" search: {} ", tree.search)
    };
    let block = Block::bordered()
        .title(" Directory tree ")
        .title_bottom(Line::from(hint).centered())
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    draw_tree_rows(frame, inner, tree, base, selected);
}

fn draw_hotlist(frame: &mut Frame, entries: &[HotEntry], recent: &[String], selected: usize) {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let title = Style::new().fg(th().header_fg).bg(th().dialog_bg);

    // display rows: pinned entries, then a header + the recent list;
    // `Some(i)` carries the selectable index a row answers to
    let label_w = entries
        .iter()
        .map(|e| e.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(16);
    let mut lines: Vec<(String, Option<usize>)> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let path = abbrev_home(std::path::Path::new(&e.path));
            (format!(" {:<label_w$}  {}", e.label, path), Some(i))
        })
        .collect();
    if !recent.is_empty() {
        lines.push((" Recent:".into(), None));
        lines.extend(recent.iter().enumerate().map(|(i, loc)| {
            let path = abbrev_home(std::path::Path::new(loc));
            (format!("   {path}"), Some(entries.len() + i))
        }));
    }

    let rows = lines.len().max(1) as u16;
    let area = centered(56, (rows + 2).min(20), frame.area());
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Directory hotlist ")
        .title_bottom(Line::from(" Enter cd · a add · d delete ").centered())
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if lines.is_empty() {
        frame.render_widget(
            Line::from(" empty - press 'a' to add the current directory ").centered(),
            inner,
        );
        return;
    }
    // keep the selected row in view when the list outgrows the dialog
    let sel_row = lines
        .iter()
        .position(|(_, s)| *s == Some(selected))
        .unwrap_or(0);
    let first = sel_row.saturating_sub(inner.height.saturating_sub(1) as usize);
    for (i, (text, sel_idx)) in lines.iter().enumerate().skip(first) {
        if (i - first) as u16 >= inner.height {
            break;
        }
        let row = Rect {
            y: inner.y + (i - first) as u16,
            height: 1,
            ..inner
        };
        let text = tail(text, inner.width as usize);
        let style = match sel_idx {
            Some(s) if *s == selected => sel,
            Some(_) => base,
            None => title,
        };
        frame.render_widget(
            Line::from(format!("{text:<w$}", w = inner.width as usize)).style(style),
            row,
        );
    }
}

/// Bulk-rename preview: every rename and delete the edited buffer asks
/// for, awaiting Yes/No - nothing has happened yet.
fn draw_rename_preview(frame: &mut Frame, d: &crate::app::RenamePreview) {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let danger = Style::new().fg(th().error_fg).bg(th().error_bg);

    let mut lines: Vec<(String, bool)> = d
        .renames
        .iter()
        .map(|(old, new)| (format!(" {} → {new}", old.to_string_lossy()), false))
        .chain(d.deletes.iter().map(|name| {
            (
                format!(" delete {} (to trash)", name.to_string_lossy()),
                true,
            )
        }))
        .collect();
    let max_rows = 12usize;
    if lines.len() > max_rows {
        let hidden = lines.len() - (max_rows - 1);
        lines.truncate(max_rows - 1);
        lines.push((format!(" …and {hidden} more"), false));
    }

    let area = centered(64, (lines.len() as u16 + 4).min(20), frame.area());
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(format!(
            " Bulk rename - {} rename(s), {} delete(s) ",
            d.renames.len(),
            d.deletes.len()
        ))
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    for (i, (text, is_delete)) in lines.iter().enumerate() {
        if i as u16 >= inner.height.saturating_sub(2) {
            break;
        }
        let row = Rect {
            y: inner.y + i as u16,
            height: 1,
            ..inner
        };
        frame.render_widget(
            Line::from(tail(text, inner.width as usize)).style(if *is_delete {
                danger
            } else {
                base
            }),
            row,
        );
    }
    let buttons = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    let selected = usize::from(!d.yes);
    frame.render_widget(buttons_line(&["Yes", "No"], selected, base, sel), buttons);
}

fn draw_confirm(frame: &mut Frame, d: &ConfirmDialog) {
    let style = Style::new().fg(th().error_fg).bg(th().error_bg);
    let sel = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let area = centered(52, 6, frame.area());
    let inner = popup(frame, area, &d.title, style);

    let message = Rect {
        y: inner.y + 1,
        height: 1,
        ..inner
    };
    frame.render_widget(
        Line::from(tail(&d.message, inner.width as usize)).centered(),
        message,
    );
    let buttons = Rect {
        y: inner.y + 3,
        height: 1,
        ..inner
    };
    let selected = usize::from(!d.yes);
    frame.render_widget(buttons_line(&["Yes", "No"], selected, style, sel), buttons);
}

/// Host-key confirmation / password prompt during an SFTP connect.
fn draw_connect_ask(frame: &mut Frame, ask: &ConnectAsk) {
    match ask {
        ConnectAsk::HostKey { fingerprint, yes } => {
            let style = Style::new().fg(th().error_fg).bg(th().error_bg);
            let sel = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let area = centered(68, 8, frame.area());
            let inner = popup(frame, area, " Unknown host ", style);
            let row = |offset: u16| Rect {
                x: inner.x + 1,
                y: inner.y + offset,
                width: inner.width.saturating_sub(2),
                height: 1,
            };
            frame.render_widget(
                Line::from("The authenticity of this host can't be established.").centered(),
                row(1),
            );
            frame.render_widget(
                Line::from(tail(fingerprint, inner.width.saturating_sub(2) as usize)).centered(),
                row(2),
            );
            frame.render_widget(
                Line::from("Trust it and save to known_hosts?").centered(),
                row(3),
            );
            let selected = usize::from(!*yes);
            frame.render_widget(buttons_line(&["Yes", "No"], selected, style, sel), row(5));
        }
        ConnectAsk::Password {
            prompt,
            value,
            echo,
        } => {
            let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let area = centered(56, 6, frame.area());
            let inner = popup(frame, area, " SSH authentication ", style);
            let row = |offset: u16| Rect {
                x: inner.x + 1,
                y: inner.y + offset,
                width: inner.width.saturating_sub(2),
                height: 1,
            };
            frame.render_widget(
                Line::from(tail(prompt, inner.width.saturating_sub(2) as usize)),
                row(0),
            );
            let shown = if *echo {
                value.clone()
            } else {
                "*".repeat(value.chars().count())
            };
            field_row(frame, row(1), &shown, Some(shown.chars().count()));
            frame.render_widget(
                Line::from("Enter - send   Esc - cancel")
                    .centered()
                    .style(style),
                row(3),
            );
        }
    }
}

/// The C-x j jobs list: every running job with its progress; Enter
/// pulls one to the foreground, c cancels it.
/// M-h: the command line's history, newest first. Enter puts the
/// selected line back on the command line for editing.
fn draw_history(frame: &mut Frame, history: &[String], selected: usize) {
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let rows = history.len().min(16) as u16;
    let area = centered(64, rows + 2, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Command history ")
        .title_bottom(Line::from(" Enter picks · Esc cancels ").centered())
        .style(style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // keep the selected row in view when the list outgrows the dialog
    let visible = inner.height as usize;
    let first = selected.saturating_sub(visible.saturating_sub(1));
    for (row, (i, cmd)) in history
        .iter()
        .rev()
        .enumerate()
        .skip(first)
        .take(visible)
        .enumerate()
    {
        let line = Rect {
            x: inner.x,
            y: inner.y + row as u16,
            width: inner.width,
            height: 1,
        };
        let text = format!(
            " {:<width$}",
            tail(cmd, inner.width as usize - 1),
            width = inner.width as usize - 1
        );
        frame.render_widget(
            Line::from(text).style(if i == selected { sel } else { style }),
            line,
        );
    }
}

fn draw_jobs(frame: &mut Frame, jobs: &[Job], selected: usize) {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let rows = jobs.len().max(1) as u16;
    let area = centered(70, (rows + 2).min(16), frame.area());
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Jobs ")
        .title_bottom(Line::from(" Enter foreground · c cancel · Esc close ").centered())
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if jobs.is_empty() {
        frame.render_widget(Line::from(" nothing running ").centered(), inner);
        return;
    }
    let selected = selected.min(jobs.len() - 1);
    for (i, job) in jobs.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let row = Rect {
            y: inner.y + i as u16,
            height: 1,
            ..inner
        };
        let pct = (job.bytes_done * 100)
            .checked_div(job.total_bytes)
            .or_else(|| (job.files_done * 100).checked_div(job.total_files))
            .map(|p| format!("{p:>3}%"))
            .unwrap_or_else(|| "  …%".into());
        let counts = format!("{}/{}", job.files_done, job.total_files);
        let text = tail(
            &format!(" {pct} {counts:>9}  {}", job.title.trim()),
            inner.width as usize,
        );
        frame.render_widget(
            Line::from(format!("{text:<w$}", w = inner.width as usize)).style(if i == selected {
                sel
            } else {
                base
            }),
            row,
        );
    }
}

/// "2:05", or "1:02:05" once it runs past an hour. Anything longer than
/// a day is not a number worth printing.
fn human_time(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 || seconds > 86_400.0 {
        return "--:--".into();
    }
    let total = seconds.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Throughput, in the same units the panels use for sizes.
fn human_rate(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second < 1.0 {
        return String::new();
    }
    format!("{}/s", human_size(bytes_per_second as u64))
}

fn draw_job(frame: &mut Frame, job: &Job) {
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let area = centered(64, 9, frame.area());
    let inner = popup(frame, area, &job.title, style);
    let width = inner.width.saturating_sub(2) as usize;

    let row = |offset: u16| Rect {
        x: inner.x + 1,
        y: inner.y + offset,
        width: inner.width.saturating_sub(2),
        height: 1,
    };

    frame.render_widget(
        Line::from(tail(&job.current.display().to_string(), width)),
        row(1),
    );
    let counts = if job.total_files > 0 {
        format!("{}/{} item(s)", job.files_done, job.total_files)
    } else {
        format!("{} item(s)", job.files_done)
    };
    // rate and time left share the counts row, pushed to the right
    let rate = human_rate(job.rate());
    let eta = job
        .eta()
        .map(|left| format!("eta {}", human_time(left)))
        .unwrap_or_default();
    let right = format!("{rate}   {eta}");
    let gap = width.saturating_sub(counts.chars().count() + right.chars().count());
    frame.render_widget(
        Line::from(format!("{counts}{}{right}", " ".repeat(gap))),
        row(2),
    );
    let bar = |ratio: f64| {
        Gauge::default()
            .ratio(ratio.clamp(0.0, 1.0))
            .gauge_style(Style::new().fg(th().panel_bg).bg(th().dialog_bg))
    };
    if job.total_bytes > 0 {
        frame.render_widget(bar(job.bytes_done as f64 / job.total_bytes as f64), row(3));
    }
    // the file in hand gets its own bar - on one big file the total bar
    // barely moves, which looks like a hang
    if job.file_total > 0 {
        frame.render_widget(bar(job.file_done as f64 / job.file_total as f64), row(4));
    }
    frame.render_widget(
        Line::from("Esc - cancel   b - background")
            .centered()
            .style(style),
        row(5),
    );
}

fn draw_ask(frame: &mut Frame, ask: &Ask, button: usize) {
    let style = Style::new().fg(th().error_fg).bg(th().error_bg);
    let sel = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let rows = ask.button_rows();
    // path + the two facts lines (or the message) + a blank + the buttons
    let height = 3 + rows.len() as u16 + 3;
    let area = centered(68, height, frame.area());
    let width = area.width.saturating_sub(4) as usize;

    let facts = |label: &str, f: &FileFacts| {
        let when = f
            .mtime
            .map(|t| DateTime::<Local>::from(t).format("%b %e %H:%M").to_string())
            .unwrap_or_else(|| "unknown".into());
        format!("{label} {:>9}  {when}", human_size(f.size))
    };
    let (title, lines) = match ask {
        // MC puts both files on screen, because "overwrite?" is not a
        // question anyone can answer without knowing which is which
        Ask::Overwrite { path, src, dst, .. } => (
            " File exists ",
            vec![
                tail(&path.display().to_string(), width),
                facts("source", src),
                facts("target", dst),
            ],
        ),
        Ask::Error { path, message } => (
            " Error ",
            vec![
                tail(&path.display().to_string(), width),
                tail(message, width),
            ],
        ),
    };
    let inner = popup(frame, area, title, style);
    let row_at = |offset: u16| Rect {
        x: inner.x + 1,
        y: inner.y + offset,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    for (i, text) in lines.iter().enumerate() {
        frame.render_widget(Line::from(text.as_str()).centered(), row_at(i as u16));
    }
    let mut first = 0;
    for (r, len) in rows.iter().enumerate() {
        let labels = &ask.buttons()[first..first + len];
        // the selected button is only in this row when the index is
        let selected = button.checked_sub(first).filter(|i| *i < *len);
        frame.render_widget(
            buttons_line(labels, selected.unwrap_or(usize::MAX), style, sel),
            row_at(lines.len() as u16 + 1 + r as u16),
        );
        first += len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HighlightRule;

    fn rule(pattern: Option<&str>, kind: Option<&str>, color: &str) -> HighlightRule {
        HighlightRule {
            pattern: pattern.map(str::to_string),
            kind: kind.map(str::to_string),
            color: color.into(),
            bold: None,
        }
    }

    fn entry(name: &str, kind: EntryKind, mode: u32) -> Entry {
        Entry {
            name: name.into(),
            kind,
            size: 0,
            mtime: None,
            mode,
            link_target: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn time_left_reads_as_a_clock() {
        assert_eq!(human_time(0.0), "0:00");
        assert_eq!(human_time(9.4), "0:09");
        assert_eq!(human_time(125.0), "2:05");
        assert_eq!(human_time(3725.0), "1:02:05");
        // nothing useful to say about these
        assert_eq!(human_time(-1.0), "--:--");
        assert_eq!(human_time(f64::INFINITY), "--:--");
        assert_eq!(human_time(90_000.0), "--:--");
    }

    #[test]
    fn throughput_reads_in_the_panels_units() {
        assert!(human_rate(1_500_000.0).ends_with("/s"));
        // too slow to be worth a number, or not a number at all
        assert_eq!(human_rate(0.0), "");
        assert_eq!(human_rate(f64::NAN), "");
    }

    #[test]
    fn colours_come_from_mcs_vocabulary() {
        assert_eq!(parse_color("brightred"), Some(Color::LightRed));
        assert_eq!(parse_color("lightred"), Some(Color::LightRed));
        assert_eq!(parse_color("brown"), Some(Color::Yellow));
        // mc's gray is bright black; its lightgray is the plain one
        assert_eq!(parse_color("gray"), Some(Color::DarkGray));
        assert_eq!(parse_color("lightgray"), Some(Color::Gray));
        assert_eq!(parse_color("default"), Some(Color::Reset));
        assert_eq!(parse_color("#ff8000"), Some(Color::Rgb(0xff, 0x80, 0x00)));
        assert_eq!(parse_color("chartreuse"), None);
        assert_eq!(parse_color("#ff80"), None);
    }

    #[test]
    fn a_glob_rule_matches_by_name_and_a_type_rule_by_kind() {
        let (rules, warnings) = compile_highlight(&[
            rule(Some("*.tar.gz"), None, "brightred"),
            rule(None, Some("exe"), "magenta"),
        ]);
        assert!(warnings.is_empty(), "{warnings:?}");
        let tarball = entry("archive.tar.gz", EntryKind::File, 0o644);
        let script = entry("run.sh", EntryKind::File, 0o755);
        assert!(rules[0].matches(&tarball));
        assert!(!rules[0].matches(&script));
        assert!(rules[1].matches(&script));
        assert!(!rules[1].matches(&tarball));
    }

    #[test]
    fn unusable_rules_are_dropped_with_a_warning() {
        let (rules, warnings) = compile_highlight(&[
            rule(Some("*.a"), Some("dir"), "red"),
            rule(None, Some("sideways"), "red"),
            rule(None, None, "red"),
            rule(Some("*.b"), None, "chartreuse"),
            rule(Some("*.c"), None, "green"),
        ]);
        assert_eq!(rules.len(), 1, "only the last rule is usable");
        assert_eq!(warnings.len(), 4);
        assert!(warnings[0].contains("both match and type"), "{warnings:?}");
        assert!(warnings[1].contains("sideways"), "{warnings:?}");
        assert!(warnings[2].contains("neither"), "{warnings:?}");
        assert!(warnings[3].contains("chartreuse"), "{warnings:?}");
    }

    #[test]
    fn every_kind_has_a_name() {
        for (name, kind, mode) in [
            ("dir", EntryKind::Dir, 0o755),
            ("linkdir", EntryKind::SymlinkDir, 0o777),
            ("link", EntryKind::SymlinkFile, 0o777),
            ("broken", EntryKind::SymlinkBroken, 0o777),
            ("exe", EntryKind::File, 0o755),
            ("file", EntryKind::File, 0o644),
        ] {
            let (rules, warnings) = compile_highlight(&[rule(None, Some(name), "red")]);
            assert!(warnings.is_empty(), "{name}: {warnings:?}");
            assert!(
                rules[0].matches(&entry("x", kind, mode)),
                "{name} did not match its own kind"
            );
        }
    }
}
