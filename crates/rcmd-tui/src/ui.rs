use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Gauge, Row, Table, TableState};
use rcmd_core::entry::{Entry, EntryKind};
use rcmd_core::fsops::FileFacts;
use rcmd_core::panel::{ListMode, Panel};
use rcmd_core::tree::Tree;

use crate::format::{Field, Format, Item};

use crate::app::{
    App, Ask, ConfirmDialog, ConnectAsk, Dialog, EditPrompt, FindDialog, FormHit, InputDialog, Job,
    MENUS, MenuState, OptionsDialog, QuickView, VfsDialog, ViewSearch, menu_label,
};
use rcmd_core::view::SearchKind;

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
    /// The menu bar and its dropdowns - mc's `[menu]`, which is its own
    /// surface rather than a dialog: white on cyan in mc's skin.
    pub menu_fg: Color,
    pub menu_bg: Color,
    /// The hotkey letter in a menu entry (mc's `menuhot`).
    pub menu_hot_fg: Color,
    pub menu_sel_fg: Color,
    pub menu_sel_bg: Color,
    /// Section headings and hotkey letters inside a dialog (mc's
    /// `dhotnormal` / `dtitle`): a colour that reads on `dialog_bg`,
    /// which the panel's `header_fg` was never chosen to do.
    pub dialog_hot_fg: Color,
}

/// A dialog's section heading: the "Layout" over the layout rows.
fn head_style() -> Style {
    Style::new()
        .fg(th().dialog_hot_fg)
        .bg(th().dialog_bg)
        .add_modifier(Modifier::BOLD)
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
        menu_fg: Color::White,
        menu_bg: Color::Cyan,
        menu_hot_fg: Color::Yellow,
        menu_sel_fg: Color::White,
        menu_sel_bg: Color::Black,
        dialog_hot_fg: Color::Blue,
    }
}

/// mc's `-b`: no colour at all. Everything is the terminal's own
/// foreground and background, and the things that must stand out do it
/// with reverse video - which is what a monochrome terminal, or an
/// `ssh` into one, has always had. Marks and directories are bold
/// wherever they are drawn, so they survive this too.
fn bw_theme() -> Theme {
    Theme {
        panel_bg: Color::Reset,
        panel_fg: Color::Reset,
        dir_fg: Color::Reset,
        exec_fg: Color::Reset,
        broken_fg: Color::Reset,
        header_fg: Color::Reset,
        mark_fg: Color::Reset,
        select_bg: Color::White,
        select_fg: Color::Black,
        dialog_bg: Color::Reset,
        dialog_fg: Color::Reset,
        error_bg: Color::White,
        error_fg: Color::Black,
        help_bg: Color::Reset,
        help_fg: Color::Reset,
        help_header_fg: Color::Reset,
        prompt_fg: Color::Reset,
        key_fg: Color::Black,
        key_bg: Color::White,
        label_fg: Color::Reset,
        label_bg: Color::Reset,
        menu_fg: Color::Reset,
        menu_bg: Color::Reset,
        menu_hot_fg: Color::Reset,
        menu_sel_fg: Color::Black,
        menu_sel_bg: Color::White,
        dialog_hot_fg: Color::Reset,
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
        menu_fg: Color::Rgb(0xc8, 0xcc, 0xd4),
        menu_bg: Color::Rgb(0x2c, 0x31, 0x3a),
        menu_hot_fg: Color::Rgb(0xe5, 0xc0, 0x7b),
        menu_sel_fg: Color::Rgb(0xff, 0xff, 0xff),
        menu_sel_bg: Color::Rgb(0x3e, 0x44, 0x51),
        dialog_hot_fg: Color::Rgb(0x61, 0xaf, 0xef),
    }
}

/// What a `[[highlight]]` rule looks at: what the entry is called, or
/// what it is.
enum Matcher {
    /// The same mask list the select and filter dialogs take, so
    /// `match = "*.c,*.h|*_test.*"` says here what it says there.
    Masks(rcmd_core::pattern::Masks),
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
    // the glob is matched against the name read as UTF-8 rather than
    // through the panel's codepage: a highlight rule is an extension
    // pattern, and those are ASCII whatever the rest of the name is
    fn matches(&self, entry: &Entry) -> bool {
        match &self.matcher {
            Matcher::Masks(masks) => masks.matches(&entry.name.to_string_lossy()),
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
            (Some(pattern), None) => {
                Matcher::Masks(rcmd_core::pattern::Masks::parse(pattern, false))
            }
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
    // mc's skins also name the 256-colour cube: colorN, rgbRGB (three
    // digits 0-5) and grayN
    if let Some(n) = name.strip_prefix("color")
        && let Ok(n) = n.parse::<u8>()
    {
        return Some(Color::Indexed(n));
    }
    if let Some(digits) = name.strip_prefix("rgb")
        && digits.len() == 3
        && let Some(cube) = digits
            .chars()
            .map(|c| c.to_digit(6))
            .collect::<Option<Vec<_>>>()
    {
        let index = 16 + 36 * cube[0] + 6 * cube[1] + cube[2];
        return Some(Color::Indexed(index as u8));
    }
    if let Some(n) = name.strip_prefix("gray")
        && let Ok(n) = n.parse::<u8>()
        && n < 24
    {
        return Some(Color::Indexed(232 + n));
    }
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
static SPEC: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Install the theme; returns a warning for unknown names. Called at
/// startup and again when the options form switches themes - which is
/// why a `-C` spec is kept and laid over the new theme as well: it was
/// asked for on the command line, and a theme switch is not a retraction.
pub fn init_theme(name: &str) -> Option<String> {
    let (mut theme, warning) = match builtin(name) {
        Some(theme) => (theme, None),
        None => load_theme_file(name),
    };
    let spec = SPEC.read().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(spec) = spec {
        apply_color_spec(&spec, &mut theme);
    }
    *THEME.write().unwrap_or_else(|e| e.into_inner()) = Some(theme);
    warning
}

fn builtin(name: &str) -> Option<Theme> {
    Some(match name {
        "mc" => mc_theme(),
        "dark" => dark_theme(),
        "bw" => bw_theme(),
        _ => return None,
    })
}

/// A theme that is not built in is a file - rcmd's own TOML or an mc
/// skin, whichever [`crate::theme`] finds under the name. A theme that
/// cannot be read is a warning and the mc palette, never a refusal to
/// start: nobody should lose their file manager to a colour typo.
fn load_theme_file(name: &str) -> (Theme, Option<String>) {
    let Some(path) = crate::theme::find(name) else {
        return (
            mc_theme(),
            Some(format!("unknown theme '{name}', using mc")),
        );
    };
    let loaded = match crate::theme::load(&path) {
        Ok(loaded) => loaded,
        Err(err) => return (mc_theme(), Some(format!("theme {name}: {err}"))),
    };
    let mut warnings = loaded.warnings;
    let mut theme = builtin(&loaded.base).unwrap_or_else(|| {
        warnings.push(format!("unknown base '{}'", loaded.base));
        mc_theme()
    });
    for (field, value) in &loaded.fields {
        match parse_color(value) {
            None => warnings.push(format!("unknown colour '{value}'")),
            Some(color) if !set_field(&mut theme, field, color) => {
                warnings.push(format!("no field '{field}'"))
            }
            Some(_) => {}
        }
    }
    let warning = (!warnings.is_empty()).then(|| format!("theme {name}: {}", warnings.join(", ")));
    (theme, warning)
}

/// Set one field by its rcmd name; false = there is no such field.
/// The names are the struct's own, which is what a theme file writes.
fn set_field(theme: &mut Theme, name: &str, color: Color) -> bool {
    match name {
        "panel_bg" => theme.panel_bg = color,
        "panel_fg" => theme.panel_fg = color,
        "dir_fg" => theme.dir_fg = color,
        "exec_fg" => theme.exec_fg = color,
        "broken_fg" => theme.broken_fg = color,
        "header_fg" => theme.header_fg = color,
        "mark_fg" => theme.mark_fg = color,
        "select_bg" => theme.select_bg = color,
        "select_fg" => theme.select_fg = color,
        "dialog_bg" => theme.dialog_bg = color,
        "dialog_fg" => theme.dialog_fg = color,
        "error_bg" => theme.error_bg = color,
        "error_fg" => theme.error_fg = color,
        "help_bg" => theme.help_bg = color,
        "help_fg" => theme.help_fg = color,
        "help_header_fg" => theme.help_header_fg = color,
        "prompt_fg" => theme.prompt_fg = color,
        "key_fg" => theme.key_fg = color,
        "key_bg" => theme.key_bg = color,
        "label_fg" => theme.label_fg = color,
        "label_bg" => theme.label_bg = color,
        "menu_fg" => theme.menu_fg = color,
        "menu_bg" => theme.menu_bg = color,
        "menu_hot_fg" => theme.menu_hot_fg = color,
        "menu_sel_fg" => theme.menu_sel_fg = color,
        "menu_sel_bg" => theme.menu_sel_bg = color,
        "dialog_hot_fg" => theme.dialog_hot_fg = color,
        _ => return false,
    }
    true
}

/// mc's `-C keyword=fg,bg:...`: colours named one at a time on the
/// command line, laid over whatever theme is installed. Keywords mc has
/// and rcmd has nowhere to put are reported together rather than one
/// complaint per word - a spec pasted out of an old `.bashrc` carries a
/// lot of them.
pub fn set_color_spec(spec: &str) -> Vec<String> {
    *SPEC.write().unwrap_or_else(|e| e.into_inner()) = Some(spec.to_string());
    let mut theme = th();
    let warnings = apply_color_spec(spec, &mut theme);
    *THEME.write().unwrap_or_else(|e| e.into_inner()) = Some(theme);
    warnings
}

fn apply_color_spec(spec: &str, theme: &mut Theme) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut unmapped: Vec<&str> = Vec::new();
    for item in spec.split(':').map(str::trim).filter(|s| !s.is_empty()) {
        let Some((keyword, colors)) = item.split_once('=') else {
            warnings.push(format!("colors: '{item}' is not keyword=fg,bg"));
            continue;
        };
        let mut fields = colors.split(',').map(str::trim);
        let mut color = |warnings: &mut Vec<String>| match fields.next() {
            None | Some("") => None,
            Some(name) => match parse_color(name) {
                Some(color) => Some(color),
                None => {
                    warnings.push(format!("colors: unknown colour '{name}'"));
                    None
                }
            },
        };
        let fg = color(&mut warnings);
        let bg = color(&mut warnings);
        // mc's third field is an attribute list (bold, underline); rcmd
        // decides those per element, so it is read and dropped
        let pair = |theme_fg: &mut Color, theme_bg: &mut Color| {
            if let Some(fg) = fg {
                *theme_fg = fg;
            }
            if let Some(bg) = bg {
                *theme_bg = bg;
            }
        };
        let only = |theme_fg: &mut Color| {
            if let Some(fg) = fg {
                *theme_fg = fg;
            }
        };
        match keyword.trim() {
            "normal" => pair(&mut theme.panel_fg, &mut theme.panel_bg),
            "selected" => pair(&mut theme.select_fg, &mut theme.select_bg),
            "errors" => pair(&mut theme.error_fg, &mut theme.error_bg),
            "dnormal" => pair(&mut theme.dialog_fg, &mut theme.dialog_bg),
            "helpnormal" => pair(&mut theme.help_fg, &mut theme.help_bg),
            "marked" => only(&mut theme.mark_fg),
            "directory" => only(&mut theme.dir_fg),
            "executable" => only(&mut theme.exec_fg),
            "stalelink" => only(&mut theme.broken_fg),
            "header" => only(&mut theme.header_fg),
            "input" => only(&mut theme.prompt_fg),
            "helpbold" => only(&mut theme.help_header_fg),
            other => unmapped.push(other),
        }
    }
    if !unmapped.is_empty() {
        warnings.push(format!(
            "colors: no rcmd equivalent for {}",
            unmapped.join(", ")
        ));
    }
    warnings
}

fn th() -> Theme {
    THEME
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or_else(mc_theme)
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    // the whole screen, known before any screen returns early: a
    // viewer's picture in the window is placed from it
    app.areas.screen = frame.area();
    draw_screens(frame, app);
    // M-h's list sits over whatever form or prompt the field is in
    if let Some(popup) = &app.field_popup {
        let rows: Vec<&str> = popup.rows.iter().map(String::as_str).collect();
        draw_pick_list(frame, popup.title, &rows, popup.selected, 0);
    }
}

fn draw_screens(frame: &mut Frame, app: &mut App) {
    if app.help.is_some() {
        draw_help(frame, app);
        return;
    }
    if app.editor().is_some() {
        draw_editor(frame, app);
        draw_screen_list(frame, app);
        return;
    }
    if app.viewer().is_some() {
        draw_viewer(frame, app);
        draw_screen_list(frame, app);
        return;
    }
    if app.diff().is_some() {
        draw_diff(frame, app);
        draw_screen_list(frame, app);
        return;
    }
    // MC's Layout dialog: every bar is optional, so the vertical stack
    // is built from whatever is switched on.
    let cfg = &app.config;
    let bar = |on: bool| Constraint::Length(u16::from(on));
    // the status line is drawn inside the active panel's frame (see
    // `App::panel_mini`), so it takes no row of its own here
    // a front end with a menu bar of its own outside the grid (the
    // window) has no use for a second one inside it
    let [menubar, main, cmdline, keybar] = Layout::vertical([
        bar(cfg.show_menubar && !app.external_menubar()),
        Constraint::Min(3),
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
    // ...and the same shape when a panel is hidden outright (C-F1 /
    // C-F2), which is Volkov Commander's way of giving one panel the
    // screen without changing what it is showing
    let showing_one = app.hidden_panel().map(|at| at ^ 1);
    let [left, right] =
        if let Some(at) = showing_one.or(listing_long(app.active).then_some(app.active)) {
            let hidden = Rect::new(main.x, main.y, 0, 0);
            if at == 0 {
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
        .saturating_sub(3 + MINI_STATUS_ROWS * u16::from(app.panel_mini(app.active)))
        as usize;
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
            .filter(|_| app.config.show_free_space)
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
            let mini = mini_line(app, i);
            draw_panel(
                frame,
                area,
                &app.panels[i],
                &mut app.table_states[i],
                Chrome {
                    active: app.active == i,
                    git: app.git_info[i].as_ref().map(|(_, s)| s),
                    disk,
                    mini,
                    columns: app.config.columns(),
                    tree: app.trees[i].as_ref(),
                    format: &app.listing_format,
                },
            );
            if app.config.scrollbars
                && app.panels[i].list_mode != ListMode::Tree
                && let Some(track) = panel_scroll_track(area, app.panel_mini(i))
            {
                draw_scrollbar(frame, track, &app.panels[i], app.config.columns());
            }
        }
    }
    draw_quick_search(frame, active_area, app);
    // a message too long for the active panel's status row spills
    // across both panels' rows, which sit on one line in a vertical
    // split - a startup warning cut in half is no warning
    if let Some(msg) = &app.status
        && !app.config.horizontal_split()
        && app.panel_mini(app.active)
        && active_area.height > MINI_STATUS_ROWS
        && msg.chars().count() > active_area.width.saturating_sub(2) as usize
    {
        let row = Rect {
            x: main.x + 1,
            y: active_area.y + active_area.height - 2,
            width: main.width.saturating_sub(2),
            height: 1,
        };
        frame.render_widget(
            Line::from(tail(msg, row.width as usize))
                .style(Style::new().fg(th().error_fg).bg(th().error_bg)),
            row,
        );
    }
    if menubar.height > 0 {
        draw_menubar(frame, menubar, app.menu.as_ref().map(|m| m.menu));
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
    // where the open dialog's rows landed, for the mouse; the list
    // dialogs fill it in, everything else leaves it empty
    let mut dialog_rows = None;
    let mut form_hits = Vec::new();
    if let Some(dialog) = &app.dialog {
        match dialog {
            Dialog::Input(d) => draw_input(frame, d),
            Dialog::Confirm(d) => draw_confirm(frame, d, &mut form_hits),
            Dialog::Tree(tree) => draw_tree_dialog(frame, tree),
            Dialog::Transfer(d) => draw_transfer(frame, d, &mut form_hits),
            Dialog::Chmod(d) => draw_chmod(frame, d, &mut form_hits),
            Dialog::Chattr(d) => draw_chattr(frame, d),
            Dialog::Chown(d) => draw_chown(frame, d, &mut form_hits),
            Dialog::Link(d) => draw_link(frame, d, &mut form_hits),
            Dialog::Hotlist(d) => dialog_rows = draw_hotlist(frame, app, d),
            Dialog::UserMenu(d) => dialog_rows = draw_user_menu(frame, d),
            Dialog::Find(d) => draw_find(frame, d, &mut form_hits),
            Dialog::Options(d) => draw_options(frame, d, &mut form_hits),
            Dialog::Pattern(d) => draw_pattern(frame, d, &mut form_hits),
            Dialog::FindResults(d) => draw_find_results(frame, d),
            Dialog::Panelize(d) => draw_panelize(frame, d, &app.config.panelize),
            Dialog::Compare(row) => {
                let rows: Vec<&str> = crate::app::COMPARE_MODES
                    .iter()
                    .map(|(label, _)| *label)
                    .collect();
                // the same question either way; the title says which of
                // the two answers it is being asked for
                let title = match app.comparing_to_sync() {
                    true => " Synchronize: compare how? ",
                    false => " Compare directories ",
                };
                dialog_rows = draw_pick_list(frame, title, &rows, *row, 0)
            }
            Dialog::Sync(d) => dialog_rows = draw_sync(frame, d),
            Dialog::Filters(d) => {
                dialog_rows = draw_filters(frame, &app.config.filter, d);
            }
            Dialog::Charset(row) => {
                dialog_rows =
                    draw_pick_list(frame, " Character set ", &crate::app::CHARSET_ROWS, *row, 0)
            }
            Dialog::Branches(d) => {
                let rows: Vec<&str> = d.rows.iter().map(String::as_str).collect();
                dialog_rows = draw_pick_list(frame, " Switch branch ", &rows, d.row, 0)
            }
            Dialog::Learn(d) => draw_learn(frame, d),
            Dialog::Skin(row) => {
                let names = crate::theme::list();
                let rows: Vec<&str> = names.iter().map(String::as_str).collect();
                dialog_rows = draw_pick_list(frame, " Appearance ", &rows, *row, 0)
            }
            Dialog::RenamePreview(d) => draw_rename_preview(frame, d),
            Dialog::Jobs(selected) => dialog_rows = draw_jobs(frame, &app.jobs, *selected),
            Dialog::Vfs(d) => draw_vfs(frame, d),
            Dialog::Fuzzy(d) => dialog_rows = draw_fuzzy(frame, d),
            Dialog::Palette(d) => draw_palette(frame, d),
            Dialog::Connections(selected) => {
                let saved = app.saved_connections();
                let rows: Vec<String> = match saved.is_empty() {
                    true => vec!["(none yet - Insert adds one)".to_string()],
                    false => saved
                        .iter()
                        .rev()
                        .map(|c| {
                            format!(
                                "{:<14} {}{}{}",
                                c.name,
                                c.url,
                                c.key
                                    .as_deref()
                                    .map(|k| format!("  key {k}"))
                                    .unwrap_or_default(),
                                if c.keyring { "  [keyring]" } else { "" }
                            )
                        })
                        .collect(),
                };
                dialog_rows = draw_history(
                    frame,
                    " Connections - Enter · Ins add · F8 forget · k keyring ",
                    &rows,
                    *selected,
                    None,
                )
            }
            Dialog::RemoteMenu(d) => {
                // the history list draws newest first: hand it the items
                // back to front, and the menu reads top to bottom
                let items: Vec<String> = d.items.iter().rev().cloned().collect();
                dialog_rows =
                    draw_history(frame, &format!(" {} ", d.title), &items, d.selected, None)
            }
            Dialog::History(selected) => {
                dialog_rows = draw_history(
                    frame,
                    " Command history ",
                    app.cmdline.history(),
                    *selected,
                    None,
                )
            }
            Dialog::Undo(selected) => {
                // what a row does is at its start: cut the end, where
                // the history lists keep the end of a command
                let rows: Vec<String> = app
                    .undo_rows()
                    .into_iter()
                    .map(|row| match row.chars().count() > 61 {
                        true => row.chars().take(60).chain(['…']).collect(),
                        false => row,
                    })
                    .collect();
                dialog_rows = draw_history(frame, " Undo ", &rows, *selected, None)
            }
            Dialog::FileHistory(selected) => {
                dialog_rows = draw_history(
                    frame,
                    " Viewed and edited ",
                    app.file_history(),
                    *selected,
                    None,
                )
            }
            Dialog::DirHistory(selected) => {
                let (entries, pos) = app.panels[app.active].history_entries();
                dialog_rows =
                    draw_history(frame, " Directory history ", entries, *selected, Some(pos))
            }
        }
    }
    app.dialog_rows = dialog_rows;
    app.form_hits = form_hits;
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
    draw_screen_list(frame, app);
}

/// mc's screen list (M-`): the panels, then every open editor and
/// viewer. It is drawn over whatever is current, because it is reached
/// from there.
fn draw_screen_list(frame: &mut Frame, app: &App) {
    let Some(row) = app.screen_list else { return };
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let mut rows = vec![" Panels".to_string()];
    rows.extend(app.screens.iter().map(|s| format!(" {}", s.title())));
    let width = rows
        .iter()
        .map(|r| r.chars().count())
        .max()
        .unwrap_or(20)
        .clamp(24, frame.area().width.saturating_sub(4) as usize);
    let area = centered(width as u16 + 2, rows.len() as u16 + 2, frame.area());
    let inner = popup(frame, area, " Screens ", base);
    for (i, text) in rows.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let line = Rect {
            y: inner.y + i as u16,
            height: 1,
            ..inner
        };
        let style = if i == row { sel } else { base };
        frame.render_widget(
            Line::from(format!(
                "{:<w$}",
                tail(text, inner.width as usize),
                w = inner.width as usize
            ))
            .style(style),
            line,
        );
    }
}

/// Everything drawn around a panel's listing: which side is focused,
/// its git status, free space and whether the mini status is on.
struct Chrome<'a> {
    active: bool,
    git: Option<&'a GitStatus>,
    disk: Option<(u64, u64)>,
    /// The framed row above the bottom edge, when this panel has one.
    mini: Option<Line<'static>>,
    /// Name columns in a brief listing (MC shows two).
    columns: u16,
    /// The figure to draw instead of the listing, in tree mode.
    tree: Option<&'a Tree>,
    /// The parsed `listing_format`, drawn in user mode.
    format: &'a Format,
}

/// The column a panel's scrollbar runs down: its right border, beside
/// the listing's rows. `None` for a panel too short to have one.
pub fn panel_scroll_track(area: Rect, mini: bool) -> Option<Rect> {
    let rows = area
        .height
        .saturating_sub(3 + MINI_STATUS_ROWS * u16::from(mini));
    (rows >= 2 && area.width >= 3).then(|| Rect {
        x: area.x + area.width - 1,
        y: area.y + 2,
        width: 1,
        height: rows,
    })
}

/// The bar, only while there is more listing than fits.
fn draw_scrollbar(frame: &mut Frame, track: Rect, panel: &Panel, columns: u16) {
    use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
    let per_screen = track.height as usize
        * match panel.list_mode {
            ListMode::Brief => columns.max(1) as usize,
            _ => 1,
        };
    let len = panel.entries.len();
    if len <= per_screen {
        return;
    }
    let mut state = ScrollbarState::new(len)
        .position(panel.cursor)
        .viewport_content_length(per_screen);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some("│"))
        .thumb_symbol("█")
        .style(Style::new().fg(th().panel_fg).bg(th().panel_bg));
    frame.render_stateful_widget(bar, track, &mut state);
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
        ref mini,
        ..
    } = chrome;
    let title_style = if active {
        Style::new().fg(th().select_fg).bg(th().select_bg)
    } else {
        Style::new().fg(th().panel_fg).bg(th().panel_bg)
    };
    // the codepage rides in the title when there is one: a panel read
    // in the wrong one looks like a directory full of broken names,
    // and nothing else on screen would say why
    let codepage = match panel.charset {
        None => String::new(),
        Some(enc) => format!(" [{}]", rcmd_core::charset::label_of(enc)),
    };
    let mut block = Block::bordered()
        .style(Style::new().fg(th().panel_fg).bg(th().panel_bg))
        .title(Span::styled(
            format!(" {}{codepage} ", panel.display_path()),
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
            height: inner
                .height
                .saturating_sub(MINI_STATUS_ROWS * u16::from(mini.is_some())),
            ..inner
        };
        let base = Style::new().fg(th().panel_fg).bg(th().panel_bg);
        let sel = if active {
            Style::new().fg(th().select_fg).bg(th().select_bg)
        } else {
            base
        };
        draw_tree_rows(frame, body, tree, base, sel);
        if let Some(line) = mini.clone() {
            draw_mini_status(frame, inner, line);
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

    // The frame is drawn first so the table can be given a smaller area
    // when the mini status claims the last row inside it.
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let listing = Rect {
        height: inner
            .height
            .saturating_sub(MINI_STATUS_ROWS * u16::from(mini.is_some())),
        ..inner
    };

    // Only the rows on screen are built: a row formats a date, looks up
    // an owner and matches the highlight rules, and a directory of a
    // hundred thousand entries shows forty of them. The scroll is kept
    // the way the table would keep it - the cursor stays in view, and
    // moves the window only when it would leave it.
    let shown = (listing.height as usize).saturating_sub(1).max(1);
    let len = panel.entries.len();
    let mut offset = state.offset();
    if panel.cursor < offset {
        offset = panel.cursor;
    } else if panel.cursor >= offset + shown {
        offset = panel.cursor + 1 - shown;
    }
    offset = offset.min(len.saturating_sub(shown));
    let remote = panel.is_remote();
    let rows = panel
        .entries
        .iter()
        .enumerate()
        .skip(offset)
        .take(shown)
        .map(|(i, entry)| {
            let git_mark = git.map(|g| g.marks.get(&entry.name).copied());
            entry_row(
                entry,
                panel.is_marked(entry),
                active && i == panel.cursor,
                git_mark,
                panel.list_mode,
                remote,
                panel.charset,
            )
        });

    let table = Table::new(rows, constraints)
        .header(header)
        .column_spacing(1)
        .style(Style::new().fg(th().panel_fg).bg(th().panel_bg));
    let mut window = TableState::default()
        .with_selected(panel.cursor.checked_sub(offset))
        .with_offset(0);
    frame.render_stateful_widget(table, listing, &mut window);
    // what the mouse and the next frame read the scroll from
    *state.offset_mut() = offset;
    state.select(Some(panel.cursor));

    if let Some(line) = mini.clone() {
        draw_mini_status(frame, inner, line);
    }
}

/// One field's text for one entry. `mark` is the panel's tag state and
/// `remote` decides whether uid/gid can be resolved to names.
fn field_text(
    field: Field,
    entry: &Entry,
    marked: bool,
    remote: bool,
    charset: Option<&'static rcmd_core::charset::Encoding>,
) -> String {
    let time = |t: Option<std::time::SystemTime>| t.map(format_time).unwrap_or_default();
    let number = |n: Option<u64>| n.map(|n| n.to_string()).unwrap_or_default();
    match field {
        Field::Name => rcmd_core::charset::decode_name(&entry.name, charset),
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
/// `text` in exactly `width` cells: cut, or padded on the left or the
/// right. Cells, not characters - a CJK name takes two a character,
/// and a column that counted characters would push the next one over.
fn fit(text: &str, width: usize, right: bool) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let len = text.width();
    if len > width {
        let mut out = String::new();
        let mut used = 0;
        for c in text.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > width {
                break;
            }
            used += w;
            out.push(c);
        }
        // a wide character that would straddle the edge leaves a blank
        out.push_str(&" ".repeat(width - used));
        return out;
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
    let body_height = inner
        .height
        .saturating_sub(1 + MINI_STATUS_ROWS * u16::from(chrome.mini.is_some()));
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
                        let mut text = field_text(*field, entry, marked, remote, panel.charset);
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

    if let Some(line) = chrome.mini.clone() {
        draw_mini_status(frame, inner, line);
    }
}

/// The theme's own base colours. A terminal resolves `Color::Reset`
/// against whatever the user's terminal is set to; a window has no such
/// default, so `rcmd-egui` paints these behind the grid instead.
pub fn base_colors() -> (Color, Color) {
    (th().panel_fg, th().panel_bg)
}

fn base_style() -> Style {
    Style::new().fg(th().panel_fg).bg(th().panel_bg)
}

/// How many rows of a panel the mini status takes: its separator line
/// and the row itself, closed off from the listing the way mc does it.
pub const MINI_STATUS_ROWS: u16 = 2;

/// mc's mini status: a `├───┤` rule across the panel, then one line
/// about the cursor entry (or, in the active panel, whatever the status
/// line has to say), in the two rows above the bottom frame.
fn draw_mini_status(frame: &mut Frame, inner: Rect, line: Line<'static>) {
    if inner.height < MINI_STATUS_ROWS + 1 {
        return;
    }
    let rule = Rect {
        x: inner.x.saturating_sub(1),
        y: inner.y + inner.height - 2,
        width: inner.width + 2,
        height: 1,
    };
    frame.render_widget(
        Line::from(format!("├{}┤", "─".repeat(inner.width as usize))).style(base_style()),
        rule,
    );
    let row = Rect {
        x: inner.x,
        y: inner.y + inner.height - 1,
        width: inner.width,
        height: 1,
    };
    frame.render_widget(line, row);
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
    let body_height = inner
        .height
        .saturating_sub(1 + MINI_STATUS_ROWS * u16::from(chrome.mini.is_some()));
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
                    let name = panel.name_of(entry);
                    let text = match mark {
                        Some(mark) => format!("{}{marker}{name}", mark.unwrap_or(' ')),
                        None => format!("{marker}{name}"),
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

    if let Some(line) = chrome.mini.clone() {
        draw_mini_status(frame, inner, line);
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
                .or_else(|| {
                    // the trash says where a thing came from
                    let note = panel.fs.note(&panel.cwd.join(&e.name))?;
                    Some(format!("  {note}"))
                })
                .unwrap_or_default();
            format!(
                "{} {:>9} {}{}",
                e.perm_string(),
                e.size,
                panel.name_of(e),
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

#[allow(clippy::too_many_arguments)]
fn entry_row(
    entry: &Entry,
    marked: bool,
    under_cursor: bool,
    git: Option<Option<char>>,
    mode: ListMode,
    remote: bool,
    charset: Option<&'static rcmd_core::charset::Encoding>,
) -> Row<'static> {
    let (marker, base) = entry_style(entry);
    let style = cell_style(marked, under_cursor, base);

    let size = if entry.is_parent() {
        "UP--DIR".to_string()
    } else {
        format_size(entry.size)
    };
    let mtime = entry.mtime.map(format_time).unwrap_or_default();

    let name_text = format!(
        "{marker}{}",
        rcmd_core::charset::decode_name(&entry.name, charset)
    );
    let name_cell = match git {
        None => Cell::from(name_text),
        Some(mark) => {
            let mark_style = if under_cursor {
                style
            } else {
                match mark {
                    Some('U') => Style::new().fg(th().error_fg).add_modifier(Modifier::BOLD),
                    Some('M') => Style::new().fg(th().mark_fg).add_modifier(Modifier::BOLD),
                    Some('A' | 'S') => Style::new().fg(th().exec_fg).add_modifier(Modifier::BOLD),
                    // untracked and ignored are the common case in a
                    // working tree, so they whisper; a change shouts
                    Some('?') | Some('!') => style.add_modifier(Modifier::DIM),
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
pub fn owner_label(id: Option<u32>, remote: bool, user: bool) -> String {
    match id {
        None => String::new(),
        Some(id) if remote => id.to_string(),
        Some(id) if user => rcmd_core::users::user_name(id),
        Some(id) => rcmd_core::users::group_name(id),
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
            lines.push(format!("Name:      {}", browse.name_of(e)));
            lines.push(format!("Type:      {kind}"));
            lines.push(format!("Size:      {}  ({})", e.size, human_size(e.size)));
            lines.push(format!("Perms:     {}  ({:o})", e.perm_string(), e.mode));
            let owner = |id: &Option<u32>, user: bool| match id {
                None => "n/a".to_string(),
                Some(id) if remote => id.to_string(),
                Some(id) => format!(
                    "{} ({id})",
                    if user {
                        rcmd_core::users::user_name(*id)
                    } else {
                        rcmd_core::users::group_name(*id)
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
            // what only a local file can say: its extended attributes,
            // its chattr flags, and the filesystem it is on
            if browse.is_local() {
                let path = browse.cwd.join(&e.name);
                let names = rcmd_core::attrs::xattr_names(&path);
                let acl = names.iter().any(|n| n.starts_with("system.posix_acl"));
                let xattrs: Vec<&str> = names
                    .iter()
                    .map(String::as_str)
                    .filter(|n| !n.starts_with("system.posix_acl"))
                    .collect();
                lines.push(String::new());
                lines.push(format!(
                    "Xattrs:    {}",
                    if xattrs.is_empty() {
                        "none".to_string()
                    } else {
                        xattrs.join(", ")
                    }
                ));
                lines.push(format!("ACL:       {}", if acl { "yes" } else { "no" }));
                if let Ok(flags) = rcmd_core::attrs::get(&path) {
                    lines.push(format!("Flags:     {}", rcmd_core::attrs::letters(flags)));
                }
                if let Some(fs) = rcmd_core::mounts::facts(&path) {
                    lines.push(String::new());
                    lines.push(format!("Device:    {} ({})", fs.device, fs.source));
                    lines.push(format!("Mounted:   {} ({})", fs.point, fs.fstype));
                    if fs.inodes_total > 0 {
                        lines.push(format!(
                            "Inodes:    {} of {} free",
                            fs.inodes_free, fs.inodes_total
                        ));
                    }
                }
            }
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

static SI_UNITS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TIME_FORMATS: std::sync::RwLock<Option<(String, String)>> = std::sync::RwLock::new(None);

/// The listing's time formats and size base, from the config.
pub fn set_formats(time: &str, time_old: &str, si_units: bool) {
    SI_UNITS.store(si_units, std::sync::atomic::Ordering::Relaxed);
    *TIME_FORMATS.write().unwrap_or_else(|e| e.into_inner()) =
        Some((time.to_string(), time_old.to_string()));
}

/// 1000 with SI units on, 1024 without.
fn size_base() -> u64 {
    match SI_UNITS.load(std::sync::atomic::Ordering::Relaxed) {
        true => 1000,
        false => 1024,
    }
}

/// A time as the listing writes it: the recent format for the last six
/// months, the old one - with the year - for anything before that or
/// in the future, as `ls -l` and mc do.
pub fn format_time(t: std::time::SystemTime) -> String {
    const HALF_YEAR: std::time::Duration = std::time::Duration::from_secs(183 * 24 * 3600);
    let formats = TIME_FORMATS.read().unwrap_or_else(|e| e.into_inner());
    let (recent, old) = match formats.as_ref() {
        Some((recent, old)) => (recent.as_str(), old.as_str()),
        None => ("%b %e %H:%M", "%b %e  %Y"),
    };
    let fresh = std::time::SystemTime::now()
        .duration_since(t)
        .is_ok_and(|age| age < HALF_YEAR);
    let format = if fresh { recent } else { old };
    let mut out = String::new();
    // a format chrono cannot read writes nothing rather than panicking
    if std::fmt::Write::write_fmt(
        &mut out,
        format_args!("{}", DateTime::<Local>::from(t).format(format)),
    )
    .is_err()
    {
        out.clear();
    }
    out
}

/// "58.2G"-style human size, one decimal below 100.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["K", "M", "G", "T"];
    if bytes < 1000 {
        return format!("{bytes}B");
    }
    let base = size_base() as f64;
    let mut val = bytes as f64;
    let mut unit = 0;
    val /= base;
    while val >= 1000.0 && unit + 1 < UNITS.len() {
        val /= base;
        unit += 1;
    }
    if val >= 100.0 {
        format!("{val:.0}{}", UNITS[unit])
    } else {
        format!("{val:.1}{}", UNITS[unit])
    }
}

/// Every user the system will name, id and name, sorted by name.
/// `getpwent` rather than reading /etc/passwd, so whatever NSS knows
/// about (LDAP, SSSD) is in the list too. Capped: a directory service
/// can hand back a great many, and a pick list is not the place to meet
/// them all.
pub fn all_users() -> Vec<(u32, String)> {
    const CAP: usize = 4096;
    let mut out = Vec::new();
    unsafe {
        libc::setpwent();
        loop {
            let entry = libc::getpwent();
            if entry.is_null() || out.len() >= CAP {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*entry).pw_name)
                .to_string_lossy()
                .into_owned();
            out.push(((*entry).pw_uid, name));
        }
        libc::endpwent();
    }
    finish_list(out)
}

/// The same for groups.
pub fn all_groups() -> Vec<(u32, String)> {
    const CAP: usize = 4096;
    let mut out = Vec::new();
    unsafe {
        libc::setgrent();
        loop {
            let entry = libc::getgrent();
            if entry.is_null() || out.len() >= CAP {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*entry).gr_name)
                .to_string_lossy()
                .into_owned();
            out.push(((*entry).gr_gid, name));
        }
        libc::endgrent();
    }
    finish_list(out)
}

/// Sorted by name, one row per name - NSS can hand the same account
/// back twice when two sources carry it.
fn finish_list(mut list: Vec<(u32, String)>) -> Vec<(u32, String)> {
    list.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    list.dedup_by(|a, b| a.1 == b.1 && a.0 == b.0);
    list
}

/// getpwuid_r / getgrgid_r, tolerating missing entries.
/// Fits in 7 columns: plain bytes below 10M, then K/M/G like MC.
fn format_size(size: u64) -> String {
    if size < 10_000_000 {
        return size.to_string();
    }
    let base = size_base();
    let kb = size / base;
    if kb < 10_000_000 {
        return format!("{kb}K");
    }
    let mb = kb / base;
    if mb < 10_000_000 {
        return format!("{mb}M");
    }
    format!("{}G", mb / base)
}

/// mc's quick search field: a box of its own on the active panel's
/// bottom frame, rather than a line of status text that the next
/// message would push aside. Red text is a search that matches nothing:
/// the characters stay where they were typed, so what is on screen is
/// what the search is.
fn draw_quick_search(frame: &mut Frame, panel: Rect, app: &App) {
    let Some(search) = &app.quick_search else {
        return;
    };
    if panel.width < 12 || panel.height < 2 {
        return;
    }
    let label = format!(" Search: {} ", search.text);
    // how to get to the next match is not something the field would
    // otherwise say - the tree walks on C-s alone, the arrows leave it
    let hint = match app.panels[app.active].list_mode {
        ListMode::Tree => " C-s next ",
        _ => " ↓↑ M-s next ",
    };
    let style = match search.miss {
        true => Style::new().fg(th().error_fg).bg(th().error_bg),
        false => Style::new().fg(th().select_fg).bg(th().select_bg),
    };
    let room = panel.width.saturating_sub(4);
    let wanted = (label.chars().count() + hint.chars().count()) as u16;
    // the hint is the first thing to go when the panel is narrow
    let line = match wanted <= room {
        true => Line::from(vec![
            Span::styled(label, style),
            Span::styled(hint, Style::new().fg(th().header_fg).bg(th().panel_bg)),
        ]),
        false => Line::from(label).style(style),
    };
    let area = Rect {
        x: panel.x + 2,
        y: panel.y + panel.height - 1,
        width: (line.width() as u16).min(room),
        height: 1,
    };
    frame.render_widget(Clear, area);
    frame.render_widget(line, area);
}

/// The framed row of panel `side`, if it has one: the active panel's
/// is the status line - a message, background progress, or the cursor
/// entry - and the other panel's describes its own cursor entry.
fn mini_line(app: &App, side: usize) -> Option<Line<'static>> {
    if !app.panel_mini(side) {
        return None;
    }
    let plain = Style::new().fg(th().header_fg).bg(th().panel_bg);
    let tree_path = |side: usize| {
        app.trees[side]
            .as_ref()
            .and_then(|tree| tree.selected())
            .map(|row| abbrev_home(&row.path))
            .unwrap_or_default()
    };
    if side != app.active {
        let text = if app.panels[side].list_mode == ListMode::Tree {
            tree_path(side)
        } else {
            entry_summary(&app.panels[side])
        };
        return Some(Line::from(text).style(plain));
    }
    let line = if let Some(msg) = &app.status {
        Line::from(msg.clone()).style(Style::new().fg(th().error_fg).bg(th().error_bg))
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
    } else if app.panels[side].list_mode == ListMode::Tree {
        // the listing is hidden, so its cursor entry would be a lie -
        // the tree's own selection is what the user is looking at
        Line::from(tree_path(side)).style(plain)
    } else {
        Line::from(entry_summary(&app.panels[side])).style(plain)
    };
    Some(line)
}

fn draw_cmdline(frame: &mut Frame, area: Rect, app: &mut App) {
    use unicode_width::UnicodeWidthStr;
    // the shell's own prompt when it has one to show, mc's way;
    // otherwise where the panel is
    let prompt = app
        .subshell_prompt()
        .unwrap_or_else(|| format!("{}$ ", abbrev_home(&app.panels[app.active].local_cwd())));
    let prompt = tail(&prompt, (area.width / 2) as usize);
    let prompt_len = UnicodeWidthStr::width(prompt.as_str());
    let field_width = (area.width as usize).saturating_sub(prompt_len).max(1);

    let cl = &app.cmdline;
    let (visible, before) = cmdline_window(&cl.value, cl.cursor, field_width);

    frame.render_widget(
        Line::from(vec![
            Span::styled(prompt, Style::new().fg(th().prompt_fg)),
            Span::raw(visible),
        ]),
        area,
    );
    if app.dialog.is_none() && app.fg_job().is_none() {
        frame.set_cursor_position((area.x + (prompt_len + before) as u16, area.y));
    }
}

/// The part of the command line that fits `width` cells with the
/// cursor on screen, and how many cells come before the cursor. Cells,
/// not characters: a CJK character or an emoji takes two, and a cursor
/// placed by count sat left of where typing goes.
fn cmdline_window(value: &str, cursor: usize, width: usize) -> (String, usize) {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    // by grapheme, measured as the terminal draws it: an emoji family
    // is two cells, where its five code points one by one were ten
    let at = crate::field::byte_index(value, cursor);
    let (head, tail) = value.split_at(at);
    let mut start = head.len();
    let mut before = 0;
    for (i, g) in head.grapheme_indices(true).rev() {
        let w = UnicodeWidthStr::width(g);
        if before + w >= width {
            break;
        }
        start = i;
        before += w;
    }
    let mut used = before;
    let mut visible = head[start..].to_string();
    for g in tail.graphemes(true) {
        used += UnicodeWidthStr::width(g);
        if used > width {
            break;
        }
        visible.push_str(g);
    }
    (visible, before)
}

pub(crate) fn abbrev_home(path: &std::path::Path) -> String {
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
    let base = Style::new().fg(th().menu_fg).bg(th().menu_bg);
    let sel = Style::new().fg(th().menu_sel_fg).bg(th().menu_sel_bg);
    // Same "  title  " spacing as the open menu's own title row, which
    // is what `menu_layout` hit-tests clicks against: drawn any other
    // way, a click on this bar lands on the neighbouring menu.
    let mut spans = Vec::new();
    for (i, (title, _)) in MENUS.iter().enumerate() {
        let style = if open == Some(i) { sel } else { base };
        spans.push(Span::styled("  ", style));
        hot_spans(title, style, th().menu_hot_fg, &mut spans);
        spans.push(Span::styled("  ", style));
    }
    frame.render_widget(Line::from(spans).style(base), area);
}

fn draw_keybar(frame: &mut Frame, area: Rect) {
    let labels: [&str; 10] = std::array::from_fn(|i| KEYBAR[i].1);
    draw_keybar_labels(frame, area, &labels);
}

/// mc's button bar, for any screen: ten numbered boxes, a label of up
/// to six letters in each, an empty one where the key does nothing.
/// The viewer and the editor draw the same row as the panels do, so
/// that F3 does not land you in what looks like another program.
fn draw_keybar_labels(frame: &mut Frame, area: Rect, labels: &[&str; 10]) {
    let mut spans = Vec::with_capacity(labels.len() * 2);
    for (i, label) in labels.iter().enumerate() {
        // the ten boxes share the whole width, as mc's do: on a wide
        // terminal the bar grows with it instead of stopping at column 80
        let width = keybar_box(area.width, i + 1) - keybar_box(area.width, i);
        if width < 3 {
            continue;
        }
        spans.push(Span::styled(
            format!("{:>2}", i + 1),
            Style::new().fg(th().key_fg).bg(th().key_bg),
        ));
        let room = width as usize - 2;
        spans.push(Span::styled(
            format!("{:<room$}", tail(label, room)),
            Style::new().fg(th().label_fg).bg(th().label_bg),
        ));
    }
    frame.render_widget(Line::from(spans), area);
}

/// Where box `i` (0..=10) of the key bar starts across `width` cells;
/// the boundaries are proportional, so the boxes differ by at most one.
pub fn keybar_box(width: u16, i: usize) -> u16 {
    (u32::from(width) * i as u32 / 10) as u16
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
    help.top = help.top.min(help.max_top());
    help.pick_visible();
    let page = crate::help::topic(help.topic);

    let base = Style::new().fg(th().help_fg).bg(th().help_bg);
    let width = content.width as usize;
    let title = format!(
        concat!(" Help - rcmd ", env!("CARGO_PKG_VERSION"), " - {}"),
        page.title
    );
    frame.render_widget(
        Line::from(format!("{title:<width$}")).style(base.add_modifier(Modifier::BOLD)),
        title_area,
    );
    frame.render_widget(Block::new().style(base), content);
    // a link is drawn in the heading colour, underlined; the picked one
    // the other way round, as a cursor
    let link_style = base
        .fg(th().help_header_fg)
        .add_modifier(Modifier::UNDERLINED);
    let picked_style = Style::new()
        .fg(th().help_bg)
        .bg(th().help_header_fg)
        .add_modifier(Modifier::BOLD);
    let links = crate::help::links(help.topic);
    help.drawn.clear();
    for row in 0..content.height {
        let at = help.top + row as usize;
        let Some(text) = page.lines.get(at) else {
            break;
        };
        let y = content.y + row;
        let row_area = Rect {
            y,
            height: 1,
            ..content
        };
        if let Some(header) = text.strip_prefix("# ") {
            frame.render_widget(
                Line::from(format!(" {header}"))
                    .style(base.fg(th().help_header_fg).add_modifier(Modifier::BOLD)),
                row_area,
            );
            continue;
        }
        // the searched-for text lit where it occurs
        let hit = !help.query.is_empty()
            && crate::help::plain(text)
                .to_lowercase()
                .contains(&help.query.to_lowercase());
        let style = if hit {
            base.fg(th().mark_fg).add_modifier(Modifier::BOLD)
        } else {
            base
        };
        let first_link = links.iter().position(|l| l.line == at).unwrap_or(0);
        let mut col = 0u16;
        let mut spans = Vec::new();
        for piece in crate::help::pieces(text) {
            match piece {
                crate::help::Piece::Text(text) => {
                    col = col.saturating_add(text.chars().count() as u16);
                    spans.push(Span::styled(text.to_string(), style));
                }
                crate::help::Piece::Link(title, n) => {
                    let link = first_link + n;
                    let wide = title.chars().count() as u16;
                    let x = content.x.saturating_add(col);
                    help.drawn.push((y, x, x.saturating_add(wide), link));
                    col = col.saturating_add(wide);
                    let style = if help.link == Some(link) {
                        picked_style
                    } else {
                        link_style
                    };
                    spans.push(Span::styled(title.to_string(), style));
                }
            }
        }
        frame.render_widget(Line::from(spans).style(base), row_area);
    }
    let bottom_text = match (&help.typing, &help.note) {
        (Some(field), _) => format!(" /{}", field.value),
        (None, Some(note)) => note.clone(),
        (None, None) => {
            " Tab link  Enter follow  ← back  F2 contents  / search  n next  Esc close".into()
        }
    };
    frame.render_widget(
        Line::from(format!("{bottom_text:<width$}")).style(base),
        bottom,
    );
}

/// Screen column of character `col` in `text`, with 8-wide tab stops -
/// must match how [`draw_editor`] expands lines.
pub fn screen_col(text: &str, col: usize) -> usize {
    rcmd_edit::screen_col(text, col, tab_size())
}

/// How wide a tab is in the editor, from the editor options. A global
/// like the theme, for the same reason: the line renderers are free
/// functions and every one of them has to agree.
static TAB_SIZE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(8);

/// Tabs and trailing blanks drawn as `→` and `·`, and the right
/// margin's column (0 = none): editor looks, global as the tab size is.
static SHOW_WHITESPACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static MARGIN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The editor's looks from the options: tab width, whitespace, margin.
pub fn set_editor_look(config: &crate::config::Config) {
    set_tab_size(config.edit_tab_size as usize);
    SHOW_WHITESPACE.store(
        config.edit_show_whitespace,
        std::sync::atomic::Ordering::Relaxed,
    );
    MARGIN.store(
        config.edit_margin as usize,
        std::sync::atomic::Ordering::Relaxed,
    );
}

fn show_whitespace() -> bool {
    SHOW_WHITESPACE.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_tab_size(size: usize) {
    TAB_SIZE.store(size.clamp(1, 16), std::sync::atomic::Ordering::Relaxed);
}

pub fn tab_size() -> usize {
    TAB_SIZE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Wrapped-segment count of an editor line at `cols` wide (always ≥ 1;
/// an exact-multiple width gets an extra row so the cursor can sit at
/// the line end).
pub fn ed_line_segs(ed: &rcmd_edit::Editor, line: usize, cols: usize) -> usize {
    screen_col(&ed.line(line), ed.line_len(line)) / cols.max(1) + 1
}

/// MC's Compare files: the two files side by side, the rows lined up
/// by the diff. A row with one side missing is a line only one of them
/// has, and shows as an empty half rather than as text sliding up.
fn draw_diff(frame: &mut Frame, app: &mut App) {
    let Some(d) = app.diff_mut() else { return };
    let [title_area, content, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    d.height = content.height as usize;
    let bar = Style::new().fg(th().select_fg).bg(th().select_bg);
    let width = content.width as usize;
    let half = width.saturating_sub(1) / 2;

    let changed = Style::new().fg(th().mark_fg);
    // the words that changed inside a changed line
    let word = changed.add_modifier(Modifier::BOLD | Modifier::REVERSED);
    let missing = Style::new().fg(th().header_fg).bg(th().panel_bg);
    let plain = Style::new().fg(th().panel_fg).bg(th().panel_bg);
    // the mark goes at the end: a long title loses its front
    let title = |side: &crate::app::DiffSide| match side.modified {
        true => format!("{} (modified)", side.title),
        false => side.title.clone(),
    };
    // a note (no more differences, ...) and what the options ignore
    // take the end of the right title, which nothing else needs
    let note = match (d.is_pending(), d.note.clone(), d.flags()) {
        (true, _, _) => " comparing… ".to_string(),
        (false, Some(note), flags) if !flags.is_empty() => format!("{note}[{flags}] "),
        (false, Some(note), _) => note,
        (false, None, flags) if !flags.is_empty() => format!(" [{flags}] "),
        (false, None, _) => String::new(),
    };
    let right_title = title(&d.right);
    let right = if note.is_empty() {
        tail(&right_title, half)
    } else {
        let room = half.saturating_sub(note.chars().count() + 1);
        format!("{:<room$} {note}", tail(&right_title, room))
    };
    frame.render_widget(
        Line::from(format!(
            "{:<half$} {:<half$}",
            tail(&title(&d.left), half),
            right
        ))
        .style(bar),
        title_area,
    );
    frame.render_widget(ratatui::widgets::Block::new().style(plain), content);
    let current = d.current.and_then(|at| d.blocks.get(at)).copied();
    for row in 0..content.height as usize {
        let at = d.top + row;
        let Some(entry) = d.rows.get(at).copied() else {
            break;
        };
        let (ltext, rtext) = (d.line(at, false), d.line(at, true));
        // a line changed on both sides says which words changed
        let (lwords, rwords) = match (entry.same, ltext, rtext) {
            (false, Some(l), Some(r)) => rcmd_core::diff::inline_with(l, r, d.opts),
            _ => (Vec::new(), Vec::new()),
        };
        let cell = |text: Option<&str>, words: &[std::ops::Range<usize>]| -> Vec<Span<'static>> {
            let Some(text) = text else {
                // the filler is what says "this line is not here",
                // which is different from "this line is empty"
                return vec![Span::styled("~".repeat(half), missing)];
            };
            let base = if entry.same { plain } else { changed };
            let highlight = |i: usize| match words.iter().any(|w| w.contains(&i)) {
                true => word,
                false => base,
            };
            diff_cells(text, highlight, d.col, half, base)
        };
        let in_current = current.is_some_and(|(start, end)| (start..end).contains(&at));
        let mut spans = cell(ltext, &lwords);
        spans.push(Span::styled(
            if in_current { "┃" } else { "│" },
            if in_current {
                changed.add_modifier(Modifier::BOLD)
            } else {
                plain
            },
        ));
        spans.extend(cell(rtext, &rwords));
        frame.render_widget(
            Line::from(spans),
            Rect {
                y: content.y + row as u16,
                height: 1,
                ..content
            },
        );
    }
    let prompt = match &d.prompt {
        Some(crate::app::DiffPrompt::Search(field)) => {
            Some(("Search: ", &field.value, field.cursor))
        }
        Some(crate::app::DiffPrompt::Goto(value, cursor)) => Some(("Go to line: ", value, *cursor)),
        None => None,
    };
    match prompt {
        Some((label, value, cursor)) => {
            frame.render_widget(Line::from(format!("{label}{value}")).style(bar), bottom);
            let x = bottom.x + (label.len() + cursor) as u16;
            frame.set_cursor_position((x.min(bottom.right().saturating_sub(1)), bottom.y));
        }
        None => draw_keybar_labels(
            frame,
            bottom,
            &[
                "Help", "Save", "Quit", "", "Merge", "", "Search", "Next", "", "Quit",
            ],
        ),
    }
}

/// One side of a diff row as styled cells: tabs expanded the way the
/// viewer expands them, each character keeping the style its place in
/// the raw line has, then the part the horizontal scroll shows,
/// padded to the column.
fn diff_cells(
    text: &str,
    style_at: impl Fn(usize) -> Style,
    skip: usize,
    width: usize,
    pad: Style,
) -> Vec<Span<'static>> {
    let mut cells: Vec<(char, Style)> = Vec::with_capacity(text.len());
    let mut col = 0usize;
    for (i, c) in text.chars().enumerate() {
        let style = style_at(i);
        match c {
            '\t' => {
                let n = 8 - col % 8;
                cells.extend(std::iter::repeat_n((' ', style), n));
                col += n;
            }
            c if (c as u32) < 0x20 => {
                cells.push(('·', style));
                col += 1;
            }
            c => {
                cells.push((c, style));
                col += 1;
            }
        }
    }
    let shown: Vec<(char, Style)> = cells.into_iter().skip(skip).take(width).collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style = pad;
    for (c, style) in &shown {
        if *style != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = *style;
        run.push(*c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    if shown.len() < width {
        spans.push(Span::styled(" ".repeat(width - shown.len()), pad));
    }
    spans
}

/// A pick list in a popup: the rows that fit around the selected one,
/// which is what both the syntax and the codepage pickers are.
/// mc's Learn keys, rcmd's way round: the checklist says which keys
/// arrived, and the line under it names whatever was pressed in the
/// spelling `[keys.panel]` uses - so a key that turns out to be
/// something else can be bound as what it really is.
fn draw_learn(frame: &mut Frame, d: &crate::app::LearnDialog) {
    use crate::app::LEARN_KEYS;
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let ok = Style::new()
        .fg(th().mark_fg)
        .bg(th().dialog_bg)
        .add_modifier(Modifier::BOLD);

    const COLS: usize = 4;
    let cell = 14usize;
    let rows = LEARN_KEYS.len().div_ceil(COLS);
    let width = (COLS * cell + 2) as u16;
    let area = centered(width, rows as u16 + 5, frame.area());
    frame.render_widget(Clear, area);
    let done = d.seen.iter().filter(|seen| **seen).count();
    let block = Block::bordered()
        .title(" Learn keys ")
        .title_bottom(
            Line::from(format!(" {done}/{} seen · Esc closes ", LEARN_KEYS.len())).centered(),
        )
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    frame.render_widget(
        Line::from(" Press each key. A tick means it arrived as itself.").style(base),
        Rect { height: 1, ..inner },
    );
    for (i, key) in LEARN_KEYS.iter().enumerate() {
        let (row, col) = (i / COLS, i % COLS);
        let at = Rect {
            x: inner.x + (col * cell) as u16,
            y: inner.y + 2 + row as u16,
            width: cell as u16,
            height: 1,
        };
        if at.y >= inner.y + inner.height {
            break;
        }
        let mark = if d.seen[i] { "✓" } else { " " };
        let style = match (i == d.row, d.seen[i]) {
            (true, _) => sel,
            (false, true) => ok,
            (false, false) => base,
        };
        frame.render_widget(Line::from(format!(" {mark} {key:<10}")).style(style), at);
    }
    // what the last key really was
    let text = match &d.last {
        Some((name, true)) => format!(" that was {name} - as expected "),
        Some((name, false)) => format!(" rcmd sees: {name} "),
        None => " waiting ".into(),
    };
    frame.render_widget(
        Line::from(text).style(base),
        Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1,
            ..inner
        },
    );
}

fn draw_pick_list(
    frame: &mut Frame,
    title: &str,
    rows: &[&str],
    row: usize,
    top: usize,
) -> Option<crate::app::DialogRows> {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let shown = rows
        .len()
        .min(frame.area().height.saturating_sub(4) as usize)
        .max(1);
    let width = rows
        .iter()
        .map(|r| r.chars().count())
        .max()
        .unwrap_or(20)
        .clamp(20, frame.area().width.saturating_sub(4) as usize);
    let inner = popup(
        frame,
        centered(width as u16 + 4, shown as u16 + 2, frame.area()),
        title,
        base,
    );
    // keep the selected row inside the window whatever `top` says
    let top = top.min(row).max((row + 1).saturating_sub(shown));
    for i in 0..shown {
        let Some(text) = rows.get(top + i) else { break };
        let line = Rect {
            y: inner.y + i as u16,
            height: 1,
            ..inner
        };
        let style = if top + i == row { sel } else { base };
        frame.render_widget(
            Line::from(format!(
                " {text:<w$}",
                w = (inner.width as usize).saturating_sub(1)
            ))
            .style(style),
            line,
        );
    }
    Some(crate::app::DialogRows {
        area: Rect {
            height: shown as u16,
            ..inner
        },
        rows: (0..shown).map(|i| Some(top + i)).collect(),
    })
}

/// One row of the editor's line-number gutter. `None` on a wrapped
/// continuation row, which belongs to the line above and so is not
/// numbered again.
/// The bottom bar of a full-screen view: what the keys do, and the
/// note pushed to the right. The note is what the program just said, so
/// it gets the room it needs and the key list is what gets cut - the
/// other way round the message vanishes exactly when it matters.
fn gutter_line(line: Option<usize>, marked: bool, change: u8, width: usize) -> Line<'static> {
    use crate::app::change::{ADDED, CHANGED, DELETED_BELOW};
    let style = Style::new().fg(th().header_fg).bg(th().panel_bg);
    let mark = Style::new().fg(th().mark_fg).bg(th().panel_bg);
    let num = match line {
        Some(idx) => (idx + 1).to_string(),
        None => String::new(),
    };
    let pad = width.saturating_sub(2);
    // the last column says how the line stands next to the saved text:
    // + new, ~ changed, _ saved lines gone from below it
    let changed = match change {
        c if c & ADDED != 0 => "+",
        c if c & CHANGED != 0 => "~",
        c if c & DELETED_BELOW != 0 => "_",
        _ => " ",
    };
    Line::from(vec![
        Span::styled(format!("{num:>pad$}"), style),
        // the bookmark sits next to its number rather than in the text,
        // where it would move the line sideways
        Span::styled(if marked { "*" } else { " " }, mark),
        Span::styled(changed, mark),
    ])
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
    // where the trailing blanks start, for showing them
    let show_ws = show_whitespace();
    let trailing_from = text.trim_end_matches([' ', '\t']).chars().count();
    let line_chars = text.chars().count();
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
        let width = rcmd_edit::cell_width(c, scol, tab_size());
        let visible = |at: usize| at >= left && at < left + cols;
        match c {
            // a tab shown: an arrow in its first cell
            '\t' if show_ws => run.extend(
                (0..width)
                    .filter(|k| visible(scol + k))
                    .map(|k| if k == 0 { '\u{2192}' } else { ' ' }),
            ),
            ' ' if show_ws && idx >= trailing_from && idx < line_chars => {
                if visible(scol) {
                    run.push('\u{b7}');
                }
            }
            '\t' => run.extend((0..width).filter(|k| visible(scol + k)).map(|_| ' ')),
            c if (c as u32) < 0x20 => {
                if visible(scol) {
                    run.push('\u{b7}');
                }
            }
            // a combining mark rides on the cell before it
            _ if width == 0 => {
                if scol > left && scol <= left + cols {
                    run.push(c);
                }
            }
            // a wide character cut by either edge shows as the half
            // that is on screen: blank cells, so nothing after it moves
            c if (0..width).all(|k| visible(scol + k)) => run.push(c),
            _ => run.extend((0..width).filter(|k| visible(scol + k)).map(|_| ' ')),
        }
        scol += width;
    }
    flush(&mut run, run_style, &mut out);
    Line::from(out)
}

/// The editor's key bar. A const rather than a literal in the middle
/// of the drawing code: it is one line too long to sit there without
/// being wrapped, and a wrapped string literal keeps its indentation.
const EDITOR_KEYBAR: [&str; 10] = [
    "", "Save", "Mark", "Replac", "Copy", "Move", "Search", "DelLn", "PullDn", "Quit",
];

fn draw_editor(frame: &mut Frame, app: &mut App) {
    let Some(st) = app.editor_mut() else {
        return;
    };
    let [title_area, content, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    // mc's line state column: the numbers, and a mark for a bookmarked
    // line so the bookmarks are visible rather than only jumpable
    let gutter_w = if st.line_numbers {
        st.ed.line_count().to_string().chars().count().max(3) + 2
    } else {
        0
    };
    let [gutter, content] =
        Layout::horizontal([Constraint::Length(gutter_w as u16), Constraint::Min(1)])
            .areas(content);
    st.gutter = gutter.width as usize;
    if gutter.width > 0 {
        st.refresh_marks();
    }
    st.rows = content.height as usize;
    st.cols = content.width as usize;

    let base = Style::new().fg(th().panel_fg).bg(th().panel_bg);
    let bar = Style::new().fg(th().select_fg).bg(th().select_bg);
    let sel_style = Style::new().fg(th().select_fg).bg(th().select_bg);

    let modified = if st.ed.modified() { " [+]" } else { "" };
    let charset = match st.ed.charset {
        None => String::new(),
        Some(enc) => format!("  [{}]", rcmd_core::charset::label_of(enc)),
    };
    let pos = format!(
        "{charset} {}:{}  {} lines  {} ",
        st.ed.cursor.line + 1,
        st.ed.cursor.col + 1,
        st.ed.line_count(),
        st.ed.line_ending(),
    );
    // a note (saved, search wrapped, ...) follows the title, where the
    // eye already is; the button bar below is mc's and stays put
    let title = match &st.note {
        Some(note) => format!(" {}{modified}  {}", st.title, note.trim()),
        None => format!(" {}{modified}", st.title),
    };
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
        let cols = st.wrap_width();
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
            if gutter.width > 0 {
                frame.render_widget(
                    gutter_line(
                        (seg == 0).then_some(line_idx),
                        seg == 0 && st.bookmarks.binary_search(&line_idx).is_ok(),
                        match seg {
                            0 => st.marks.1.get(line_idx).copied().unwrap_or(0),
                            _ => 0,
                        },
                        gutter.width as usize,
                    ),
                    Rect {
                        y: content.y + row as u16,
                        height: 1,
                        ..gutter
                    },
                );
            }
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
            if gutter.width > 0 {
                frame.render_widget(
                    gutter_line(
                        Some(idx),
                        st.bookmarks.binary_search(&idx).is_ok(),
                        st.marks.1.get(idx).copied().unwrap_or(0),
                        gutter.width as usize,
                    ),
                    Rect {
                        y: content.y + row as u16,
                        height: 1,
                        ..gutter
                    },
                );
            }
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

    // the right margin: a tinted column at its place, where the text
    // is not wrapped (wrapped text has no one column to mark)
    let margin = MARGIN.load(std::sync::atomic::Ordering::Relaxed);
    if margin > 0 && !st.wrap && margin >= st.left && margin - st.left < content.width as usize {
        let x = content.x + (margin - st.left) as u16;
        let buf = frame.buffer_mut();
        for y in content.y..content.y + content.height {
            buf[(x, y)].set_bg(th().header_fg);
        }
    }

    draw_keybar_labels(frame, bottom, &EDITOR_KEYBAR);

    match &st.prompt {
        None => {}
        Some(EditPrompt::Search(dialog)) => draw_view_search(frame, dialog),
        Some(EditPrompt::ReplaceFind(field)) => {
            let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let inner = popup(
                frame,
                centered(50, 5, frame.area()),
                " Replace (regex) ",
                style,
            );
            draw_field(frame, inner, &field.value, field.cursor);
        }
        Some(EditPrompt::ReplaceWith { field, .. }) => {
            let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let inner = popup(
                frame,
                centered(50, 5, frame.area()),
                " Replace with ",
                style,
            );
            draw_field(frame, inner, &field.value, field.cursor);
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
        Some(EditPrompt::Goto { value, cursor }) => {
            let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let inner = popup(frame, centered(40, 5, frame.area()), " Go to line ", style);
            draw_field(frame, inner, value, *cursor);
        }
        Some(EditPrompt::Syntax { row, top }) => {
            draw_pick_list(frame, " Syntax ", &crate::app::syntax_rows(), *row, *top);
        }
        Some(EditPrompt::Charset(row)) => {
            draw_pick_list(frame, " Codepage ", &crate::app::CHARSET_ROWS, *row, 0);
        }
        Some(EditPrompt::Options(d)) => draw_edit_options(frame, d),
        Some(EditPrompt::SaveAs(field)) => {
            let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let inner = popup(frame, centered(64, 5, frame.area()), " Save as ", style);
            draw_field(frame, inner, &field.value, field.cursor);
        }
        Some(EditPrompt::Clobber { button }) => {
            let style = Style::new().fg(th().error_fg).bg(th().error_bg);
            let sel = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let inner = popup(
                frame,
                centered(60, 7, frame.area()),
                " Changed on disk ",
                style,
            );
            let row = |offset: u16| Rect {
                x: inner.x + 1,
                y: inner.y + offset,
                width: inner.width.saturating_sub(2),
                height: 1,
            };
            frame.render_widget(
                Line::from("The file was changed on disk since it was opened.").centered(),
                row(1),
            );
            frame.render_widget(
                Line::from("Saving now throws that change away.").centered(),
                row(2),
            );
            frame.render_widget(
                buttons_line(&["Overwrite", "Cancel"], *button, style, sel),
                row(4),
            );
        }
        Some(EditPrompt::Complete {
            words, selected, ..
        }) => {
            let rows: Vec<&str> = words.iter().map(String::as_str).collect();
            draw_pick_list(frame, " Complete ", &rows, *selected, 0);
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

    // the menu bar goes over the title row, as it does in mc: it is
    // only there while it is open
    if let Some(ms) = &st.menu {
        draw_menu_of(frame, ms, crate::app::EDIT_MENUS);
    }
}

/// mc's editor options, as a form: two numbers nudged with Left/Right
/// and three switches ticked with Space.
fn draw_edit_options(frame: &mut Frame, d: &crate::app::EditOptions) {
    use crate::app::{EDIT_OPTION_ROWS, EditOpt};
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let rows = EDIT_OPTION_ROWS.len() as u16;
    let inner = popup(
        frame,
        centered(48, rows + 4, frame.area()),
        " Editor options ",
        base,
    );
    let check = |on: bool| if on { "[x]" } else { "[ ]" };
    for (i, (opt, label)) in EDIT_OPTION_ROWS.iter().enumerate() {
        let row = Rect {
            x: inner.x + 1,
            y: inner.y + i as u16,
            width: inner.width.saturating_sub(2),
            height: 1,
        };
        let width = row.width as usize;
        let text = match opt {
            EditOpt::TabSize | EditOpt::WrapColumn => {
                format!(" {label:<26}{}  (Left/Right)", d.value(*opt))
            }
            other => format!(" {} {label}", check(d.get(*other))),
        };
        let style = if d.cursor == i { sel } else { base };
        frame.render_widget(Line::from(format!("{text:<width$}")).style(style), row);
    }
    let buttons = Rect {
        x: inner.x,
        y: inner.y + rows + 1,
        width: inner.width,
        height: 1,
    };
    let selected = if d.cursor == EDIT_OPTION_ROWS.len() {
        usize::from(!d.ok)
    } else {
        usize::MAX
    };
    frame.render_widget(
        buttons_line(&["OK", "Cancel"], selected, base, sel),
        buttons,
    );
}

fn draw_viewer(frame: &mut Frame, app: &mut App) {
    let Some(v) = app.viewer_mut() else { return };
    let ruler_rows = if v.ruler { 1 } else { 0 };
    let [title_area, ruler_area, content, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(ruler_rows),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    // editing hex, the last two rows are the data inspector's
    v.rows = match v.hex && v.hex_edit {
        true => content.height.saturating_sub(2).max(1) as usize,
        false => content.height as usize,
    };
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
    let nroff = if v.nroff { " [format]" } else { "" };
    let charset = match v.file.charset {
        None => String::new(),
        Some(enc) => format!(" [{}]", rcmd_core::charset::label_of(enc)),
    };
    let editing = if v.hex && v.hex_edit {
        format!(
            " [edit @{:08X}{}]",
            v.hex_cursor,
            match v.hex_edits.len() {
                0 => String::new(),
                n => format!(", {n} unwritten"),
            }
        )
    } else {
        String::new()
    };
    let filtered = if v.filtered { " [parsed]" } else { "" };
    let note = match &v.note {
        Some(note) => format!("  {}", note.trim()),
        None => String::new(),
    };
    let title = format!(
        " {}  {} bytes  {percent}%  [{mode}]{editing}{nroff}{charset}{filtered}{follow}{note}",
        v.path.display(),
        v.file.size,
    );
    frame.render_widget(
        Line::from(format!("{:<w$}", tail(&title, width), w = width))
            .style(Style::new().fg(th().select_fg).bg(th().select_bg)),
        title_area,
    );

    if v.ruler {
        frame.render_widget(
            Line::from(ruler_text(v.left, width)).style(Style::new().fg(th().header_fg)),
            ruler_area,
        );
    }

    // syntax spans for the visible line range (empty without a
    // recognized syntax); search matches are overlaid per line below
    let search = v.search.to_search();
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
            // nroff mode: the overstrikes become attributes, so the
            // text everything below works on is the text they spelled
            let (text, nroff) = if v.nroff {
                rcmd_core::view::nroff_line(&text)
            } else {
                (text, Vec::new())
            };
            let (expanded, map) = expand_with_map(&text);
            let clamp = |i: usize| map[i.min(map.len() - 1)];
            let mut attrs = vec![0u8; expanded.chars().count()];
            for (i, &attr) in nroff.iter().enumerate() {
                for col in map[i]..map[i + 1].min(attrs.len()) {
                    attrs[col] = attr;
                }
            }
            let spans: Vec<(usize, usize, [u8; 3])> = all_spans
                .get(idx.saturating_sub(v.top))
                .map(|sp| {
                    sp.iter()
                        .map(|&(a, b, rgb)| (clamp(a), clamp(b), rgb))
                        .collect()
                })
                .unwrap_or_default();
            let matches = if search.pattern.is_empty() {
                Vec::new()
            } else {
                search.ranges(&expanded)
            };
            let base = if v.found == Some(idx) {
                Style::new().fg(th().mark_fg).add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            Some((expanded, spans, matches, base, attrs))
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
            let Some((expanded, spans, matches, base, attrs)) = styled(v, line_idx, &all_spans)
            else {
                break;
            };
            let len = expanded.chars().count();
            if seg * width > len {
                seg = 0; // stale segment after a resize
            }
            let start = (seg * width).min(len);
            frame.render_widget(
                viewer_line(&expanded, &spans, &matches, &attrs, start, width, base),
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
                if v.hex_edit && row as usize >= v.rows {
                    break;
                }
                let bytes = v.file.read_at(row_offset, 16).unwrap_or_default();
                frame.render_widget(hex_line(v, row_offset, &bytes), row_area);
            } else {
                let idx = v.top + row as usize;
                let Some((expanded, spans, matches, base, attrs)) = styled(v, idx, &all_spans)
                else {
                    break;
                };
                frame.render_widget(
                    viewer_line(&expanded, &spans, &matches, &attrs, v.left, width, base),
                    row_area,
                );
            }
        }
    }

    // mc's button bar names what pressing the key does now, which for
    // half of these is the opposite of what it did a moment ago
    let labels: [&str; 10] = if v.hex {
        [
            "",
            if v.hex_edit { "View" } else { "Edit" },
            "Quit",
            "Ascii",
            "Goto",
            "Save",
            "Search",
            "",
            "",
            "Quit",
        ]
    } else {
        [
            "",
            if v.wrap { "Unwrap" } else { "Wrap" },
            "Quit",
            "Hex",
            "Goto",
            if v.filtered { "Raw" } else { "Parse" },
            "Search",
            if v.nroff { "Unform" } else { "Format" },
            "",
            "Quit",
        ]
    };
    draw_keybar_labels(frame, bottom, &labels);

    if v.hex && v.hex_edit && content.height > 2 {
        let bytes: Vec<u8> = (0..8).map_while(|i| v.byte_at(v.hex_cursor + i)).collect();
        let head = Style::new().fg(th().header_fg).bg(th().panel_bg);
        for (i, text) in hex_inspect(&bytes).into_iter().enumerate() {
            frame.render_widget(
                Line::from(format!(
                    " {:<w$}",
                    fit(&text, content.width.saturating_sub(1) as usize, false),
                    w = content.width.saturating_sub(1) as usize
                ))
                .style(head),
                Rect {
                    y: content.y + content.height - 2 + i as u16,
                    height: 1,
                    ..content
                },
            );
        }
    }

    if let Some(dialog) = &v.prompt {
        draw_view_search(frame, dialog);
    }
    if let Some((value, cursor)) = &v.goto {
        let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
        let area = centered(52, 5, frame.area());
        let inner = popup(frame, area, " Goto: line, 0x1f / 31b, or 50% ", style);
        draw_field(frame, inner, value, *cursor);
    }
    if let Some(row) = v.charset_pick {
        draw_pick_list(frame, " Codepage ", &crate::app::CHARSET_ROWS, row, 0);
    }
    if let Some(button) = v.confirm_quit {
        let style = Style::new().fg(th().error_fg).bg(th().error_bg);
        let sel = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
        let inner = popup(
            frame,
            centered(56, 6, frame.area()),
            " Unwritten bytes ",
            style,
        );
        let row = |offset: u16| Rect {
            x: inner.x + 1,
            y: inner.y + offset,
            width: inner.width.saturating_sub(2),
            height: 1,
        };
        frame.render_widget(
            Line::from(format!(
                "{} byte(s) were changed. Write them?",
                v.hex_edits.len()
            ))
            .centered(),
            row(1),
        );
        frame.render_widget(
            buttons_line(&["Save", "Discard", "Cancel"], button, style, sel),
            row(3),
        );
    }
}

/// The data inspector: the bytes at the hex cursor read as each kind of
/// number, little-endian on the first row and big-endian on the second,
/// with a Unix time where four bytes make one this century.
pub fn hex_inspect(bytes: &[u8]) -> [String; 2] {
    let take = |n: usize| (bytes.len() >= n).then(|| &bytes[..n]);
    let row = |little: bool| -> String {
        let int = |n: usize| -> Option<u64> {
            let b = take(n)?;
            let mut v = 0u64;
            for i in 0..n {
                let byte = if little { b[n - 1 - i] } else { b[i] };
                v = (v << 8) | byte as u64;
            }
            Some(v)
        };
        let mut parts = vec![if little { "LE" } else { "BE" }.to_string()];
        if let Some(v) = int(1) {
            parts.push(format!("u8 {v} i8 {}", v as u8 as i8));
        }
        if let Some(v) = int(2) {
            parts.push(format!("u16 {v} i16 {}", v as u16 as i16));
        }
        if let Some(v) = int(4) {
            parts.push(format!(
                "u32 {v} i32 {} f32 {}",
                v as u32 as i32,
                f32::from_bits(v as u32)
            ));
            // a plausible Unix time: 1990 to 2100
            if (631_152_000..4_102_444_800).contains(&v) {
                let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(v);
                parts.push(format!(
                    "time {}",
                    DateTime::<Local>::from(t).format("%Y-%m-%d %H:%M:%S")
                ));
            }
        }
        if let Some(v) = int(8) {
            parts.push(format!(
                "u64 {v} i64 {} f64 {}",
                v as i64,
                f64::from_bits(v)
            ));
        }
        parts.join("  ")
    };
    [row(true), row(false)]
}

/// One row of the hex view: the offset, sixteen bytes and their text,
/// with the pending edits shown where they will land and the cursor on
/// whichever of the two columns has it.
fn hex_line(v: &crate::app::Viewer, offset: u64, bytes: &[u8]) -> Line<'static> {
    let cursor = Style::new().fg(th().select_fg).bg(th().select_bg);
    let changed = Style::new().fg(th().mark_fg).add_modifier(Modifier::BOLD);
    let at_cursor = |i: usize, ascii: bool| {
        v.hex_edit && v.hex_ascii == ascii && offset + i as u64 == v.hex_cursor
    };
    let hit = |i: usize| {
        v.hex_hit
            .is_some_and(|(at, len)| (at..at + len).contains(&(offset + i as u64)))
    };
    let found = Style::new()
        .fg(th().mark_fg)
        .add_modifier(Modifier::BOLD | Modifier::REVERSED);
    let mut spans: Vec<Span> = vec![Span::raw(format!("{offset:08X}  "))];
    for i in 0..16 {
        if i == 8 {
            spans.push(Span::raw(" "));
        }
        let Some(&raw) = bytes.get(i) else {
            spans.push(Span::raw("   "));
            continue;
        };
        let edit = v.hex_edits.get(&(offset + i as u64)).copied();
        let style = match (at_cursor(i, false), edit.is_some()) {
            (true, _) => cursor,
            (false, true) => changed,
            _ if hit(i) => found,
            _ => Style::new(),
        };
        spans.push(Span::styled(format!("{:02X}", edit.unwrap_or(raw)), style));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::raw(" |"));
    for (i, &raw) in bytes.iter().enumerate() {
        let edit = v.hex_edits.get(&(offset + i as u64)).copied();
        let byte = edit.unwrap_or(raw);
        let style = match (at_cursor(i, true), edit.is_some()) {
            (true, _) => cursor,
            (false, true) => changed,
            _ if hit(i) => found,
            _ => Style::new(),
        };
        let shown = if (0x20..0x7F).contains(&byte) {
            byte as char
        } else {
            '.'
        };
        spans.push(Span::styled(shown.to_string(), style));
    }
    spans.push(Span::raw("|"));
    Line::from(spans)
}

/// A column ruler: a tick every ten columns, numbered, counting from
/// the leftmost column actually on screen so it still tells the truth
/// when the view is scrolled sideways.
fn ruler_text(left: usize, width: usize) -> String {
    let mut out = String::with_capacity(width);
    let mut col = left;
    while out.chars().count() < width {
        let at = col + 1;
        if at.is_multiple_of(10) {
            let label = at.to_string();
            out.push_str(&label);
            // a label longer than the gap eats the next columns, which
            // is what a ruler does rather than lying about the width
            col += label.chars().count();
            continue;
        }
        out.push(if at.is_multiple_of(5) { '+' } else { '-' });
        col += 1;
    }
    out.chars().take(width).collect()
}

/// MC's viewer search dialog: the pattern on top, then the four
/// answers that change what it means. The focused row is the one that
/// Space acts on, so it has to be visible - the marker on the left
/// says which.
fn draw_view_search(frame: &mut Frame, dialog: &ViewSearch) {
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let area = centered(56, 9, frame.area());
    let inner = popup(frame, area, " Search ", style);
    let field = Rect { height: 1, ..inner };
    draw_field(frame, field, &dialog.field.value, dialog.field.cursor);

    let kind = match dialog.kind {
        SearchKind::Normal => "Normal",
        SearchKind::Regex => "Regular expression",
        SearchKind::Hex => "Hexadecimal",
    };
    let rows = [
        format!("Pattern is  < {kind} >"),
        format!("[{}] Case sensitive", tick(dialog.case_sensitive)),
        format!("[{}] Whole words", tick(dialog.whole_word)),
        format!("[{}] Backwards", tick(dialog.backwards)),
    ];
    for (i, text) in rows.iter().enumerate() {
        let row = Rect {
            y: inner.y + 2 + i as u16,
            height: 1,
            ..inner
        };
        let focused = dialog.row == i + 1;
        let marker = if focused { ">" } else { " " };
        frame.render_widget(
            Line::from(format!(
                "{marker} {text:<w$}",
                w = (inner.width as usize).saturating_sub(2)
            ))
            .style(if focused { sel } else { style }),
            row,
        );
    }
}

fn tick(on: bool) -> char {
    if on { 'x' } else { ' ' }
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

/// One viewer row: syntax colors with the search matches overlaid,
/// windowed to `[start, start+width)` of the expanded text.
fn viewer_line(
    expanded: &str,
    spans: &[(usize, usize, [u8; 3])],
    matches: &[(usize, usize)],
    attrs: &[u8],
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
        // nroff attributes ride on top of whatever colour won: they say
        // what the character is, not what colour it has
        match attrs.get(i).copied().unwrap_or(0) {
            0 => {}
            attr => {
                if attr & rcmd_core::view::NROFF_BOLD != 0 {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if attr & rcmd_core::view::NROFF_UNDERLINE != 0 {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
            }
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

/// The label as spans, hotkey letter highlighted MC-style: its own
/// colour, and underlined so it survives a theme where that colour is
/// the text's (bw).
fn hot_spans(label: &str, style: Style, hot_fg: Color, spans: &mut Vec<Span<'static>>) {
    let (pre, hot, post) = menu_label(label);
    spans.push(Span::styled(pre.to_string(), style));
    if let Some(c) = hot {
        spans.push(Span::styled(
            c.to_string(),
            style
                .fg(hot_fg)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
    }
    spans.push(Span::styled(post.to_string(), style));
}

pub fn menu_layout(menu: usize, area: Rect) -> (Vec<(u16, u16)>, Rect) {
    menu_layout_of(MENUS, menu, area)
}

/// The same geometry for any menu bar - the panel's and the editor's
/// differ only in what their entries do.
fn menu_layout_of<A>(
    menus: crate::app::MenuBar<A>,
    menu: usize,
    area: Rect,
) -> (Vec<(u16, u16)>, Rect) {
    let mut titles = Vec::new();
    let mut x = 0u16;
    for (title, _) in menus {
        let width = (label_len(title) + 4) as u16;
        titles.push((area.x + x, width));
        x += width;
    }
    let entries = menus[menu].1;
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
    draw_menu_of(frame, ms, MENUS);
}

/// Draw an open menu bar and its dropdown. Generic over what the
/// entries do: the panel's menus run panel actions, the editor's run
/// editor ones, and neither of those is any of drawing's business.
fn draw_menu_of<A>(frame: &mut Frame, ms: &MenuState, menus: crate::app::MenuBar<A>) {
    let area = frame.area();
    let base = Style::new().fg(th().menu_fg).bg(th().menu_bg);
    let sel = Style::new().fg(th().menu_sel_fg).bg(th().menu_sel_bg);
    let (titles, dropdown) = menu_layout_of(menus, ms.menu, area);

    let bar = Rect { height: 1, ..area };
    frame.render_widget(Clear, bar);
    let mut spans = Vec::new();
    for (i, (title, _)) in menus.iter().enumerate() {
        let style = if i == ms.menu { sel } else { base };
        spans.push(Span::styled("  ", style));
        hot_spans(title, style, th().menu_hot_fg, &mut spans);
        spans.push(Span::styled("  ", style));
    }
    let used = titles.last().map(|(x, w)| x + w).unwrap_or(0);
    spans.push(Span::styled(
        " ".repeat((bar.width as usize).saturating_sub(used as usize)),
        base,
    ));
    frame.render_widget(Line::from(spans), bar);

    let entries = menus[ms.menu].1;
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
                hot_spans(label, style, th().menu_hot_fg, &mut spans);
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
/// The end of `text` in at most `max` cells, with an ellipsis where
/// the front was cut.
pub fn tail(text: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if text.width() <= max {
        return text.to_string();
    }
    let room = max.saturating_sub(1);
    let mut used = 0;
    let mut start = text.len();
    for (at, c) in text.char_indices().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > room {
            break;
        }
        used += w;
        start = at;
    }
    format!("…{}", &text[start..])
}

/// A row of dialog buttons, with mc's **underlined hotkey**: the letter
/// after a `&` in the label, or the first one when the label has no
/// marker. Alt and that letter presses the button from anywhere in the
/// dialog, which is what makes it useful when a text field has the
/// focus.
/// Where `buttons_line` puts each of its buttons in `area`: the line is
/// centred, and each button is its label in `[ ]` and one space after.
fn button_rects(labels: &[&str], area: Rect) -> Vec<Rect> {
    let widths: Vec<u16> = labels
        .iter()
        .map(|label| crate::app::button_hotkey(label).0.chars().count() as u16 + 4)
        .collect();
    let total: u16 = widths.iter().map(|w| w + 1).sum();
    let mut x = area.x + area.width.saturating_sub(total) / 2;
    widths
        .into_iter()
        .map(|width| {
            let rect = Rect { x, width, ..area };
            x += width + 1;
            rect
        })
        .collect()
}

fn buttons_line(labels: &[&str], selected: usize, base: Style, sel: Style) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        let style = if i == selected { sel } else { base };
        let (text, hotkey) = crate::app::button_hotkey(label);
        match hotkey {
            Some((at, _)) => {
                let mut chars = text.chars();
                let before: String = chars.by_ref().take(at).collect();
                let letter: String = chars.by_ref().take(1).collect();
                let after: String = chars.collect();
                spans.push(Span::styled(format!("[ {before}"), style));
                spans.push(Span::styled(
                    letter,
                    style.add_modifier(Modifier::UNDERLINED),
                ));
                spans.push(Span::styled(format!("{after} ]"), style));
            }
            None => spans.push(Span::styled(format!("[ {text} ]"), style)),
        }
        spans.push(Span::styled(" ", base));
    }
    Line::from(spans).centered()
}

fn draw_input(frame: &mut Frame, d: &InputDialog) {
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    // the pack form says what it will compress at, and how to change it
    let level = match &d.action {
        crate::app::InputAction::Pack { level, .. } => Some(*level),
        _ => None,
    };
    let area = centered(64, if level.is_some() { 6 } else { 5 }, frame.area());
    let inner = popup(frame, area, &d.title, style);
    if let Some(level) = level {
        let text = match level {
            Some(0) => "level 0 (zip, gz, zst: stored)".to_string(),
            Some(level) => format!("level {level}"),
            None => "level: the format's default".to_string(),
        };
        let row = Rect {
            x: inner.x + 1,
            y: inner.y + 3,
            width: inner.width.saturating_sub(2),
            height: 1,
        };
        frame.render_widget(
            Line::from(format!("{text}  ·  M-0..M-9 set it, M-- default")).style(style),
            row,
        );
    }
    draw_field(frame, inner, &d.field.value, d.field.cursor);
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
    // scrolled just far enough that the cursor's cell is on screen
    let (visible, before) = cmdline_window(value, cursor.unwrap_or(0), width);
    let visible = fit(&visible, width, false);
    frame.render_widget(
        Line::from(visible).style(Style::new().fg(th().select_fg).bg(th().select_bg)),
        field,
    );
    if cursor.is_some() {
        frame.set_cursor_position((field.x + before as u16, field.y));
    }
}

/// F9 > Options > Panel options - the MC-style checkbox form.
fn draw_options(frame: &mut Frame, d: &OptionsDialog, hits: &mut Vec<(Rect, FormHit)>) {
    use crate::app::{OPTION_ROWS, OptRow};
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let head = head_style();
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
        if !matches!(entry, OptRow::Head(_)) {
            hits.push((row, FormHit::Row(i)));
        }
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
    for (i, rect) in button_rects(&["OK", "Cancel"], buttons)
        .into_iter()
        .enumerate()
    {
        hits.push((rect, FormHit::Button(i)));
    }
}

/// MC's external panelize: the saved commands above, the one being
/// typed below.
fn draw_panelize(
    frame: &mut Frame,
    d: &crate::app::PanelizeDialog,
    presets: &[crate::config::PanelizePreset],
) {
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let list_rows = presets.len().clamp(1, 8);
    let inner = popup(
        frame,
        centered(66, list_rows as u16 + 7, frame.area()),
        " Panelize (a command's output as the listing) ",
        style,
    );
    let row = |offset: usize| Rect {
        x: inner.x + 1,
        y: inner.y + offset as u16,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    frame.render_widget(Line::from("Saved:").style(style), row(0));
    if presets.is_empty() {
        frame.render_widget(
            Line::from("  (none yet - Ctrl+S saves what you type)").style(style),
            row(1),
        );
    }
    for (i, preset) in presets.iter().take(list_rows).enumerate() {
        let focused = d.on_list && d.row == i;
        frame.render_widget(
            Line::from(format!(
                " {:<20.20} {}",
                preset.name,
                tail(&preset.run, inner.width as usize / 2)
            ))
            .style(if focused { sel } else { style }),
            row(i + 1),
        );
    }
    let below = list_rows + 2;
    match &d.naming {
        Some((name, cursor)) => {
            frame.render_widget(Line::from("Save as:").style(style), row(below));
            field_row(frame, row(below + 1), name, Some(*cursor));
        }
        None => {
            frame.render_widget(Line::from("Command:").style(style), row(below));
            field_row(
                frame,
                row(below + 1),
                &d.command.value,
                (!d.on_list).then_some(d.command.cursor),
            );
        }
    }
    frame.render_widget(
        Line::from("Tab list/field   Ctrl+S save   F8 drop   Enter run   Esc cancel")
            .centered()
            .style(style),
        row(below + 3),
    );
}

/// MC's select / unselect / filter form: the pattern, then the three
/// answers that change what it means.
fn draw_pattern(frame: &mut Frame, d: &crate::app::PatternDialog, hits: &mut Vec<(Rect, FormHit)>) {
    use crate::app::{PATTERN_FIELDS, PATTERN_ROWS};
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let label = Style::new().fg(th().dialog_hot_fg).bg(th().dialog_bg);
    let inner = popup(frame, centered(56, 13, frame.area()), &d.title, style);
    let row = |offset: u16| Rect {
        x: inner.x + 1,
        y: inner.y + offset,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    // the pattern, the size and the age fields, then the switches
    for (at, which) in [(0, 0), (3, 1), (5, 2), (6, 3), (7, 4), (8, 5)] {
        hits.push((row(at), FormHit::Row(which)));
    }
    let buttons_at = Rect {
        x: inner.x,
        y: inner.y + 10,
        width: inner.width,
        height: 1,
    };
    for (i, rect) in button_rects(&["OK", "Cancel"], buttons_at)
        .into_iter()
        .enumerate()
    {
        hits.push((rect, FormHit::Button(i)));
    }
    field_row(
        frame,
        row(0),
        &d.value.value,
        (d.row == 0).then_some(d.value.cursor),
    );
    // the two questions mc never asks, so they say what they take
    for (i, (text, cursor, hint)) in [
        (&d.size.value, d.size.cursor, "Size    (>1M, <=100k, 1M-2G)"),
        (
            &d.newer.value,
            d.newer.cursor,
            "Newer than  (30m, 24h, 7d, 2w)",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let at = i as u16 * 2 + 2;
        frame.render_widget(Line::from(format!(" {hint}")).style(label), row(at));
        field_row(frame, row(at + 1), text, (d.row == i + 1).then_some(cursor));
    }
    let check = |on: bool| if on { "[x]" } else { "[ ]" };
    for (i, (on, label)) in [
        (d.files_only, "Files only"),
        (d.case_sensitive, "Case sensitive"),
        (d.shell, "Shell patterns (off = regular expression)"),
    ]
    .iter()
    .enumerate()
    {
        let focused = d.row == i + PATTERN_FIELDS;
        frame.render_widget(
            Line::from(format!(" {} {label}", check(*on))).style(if focused { sel } else { style }),
            row(i as u16 + 6),
        );
    }
    frame.render_widget(
        buttons_line(
            &["OK", "Cancel"],
            if d.row == PATTERN_ROWS {
                usize::from(!d.ok)
            } else {
                usize::MAX
            },
            style,
            sel,
        ),
        Rect {
            x: inner.x,
            y: inner.y + 10,
            width: inner.width,
            height: 1,
        },
    );
}

/// How many matches the results window shows at once. A function of
/// the terminal rather than a note drawing leaves behind, so the keys
/// page by exactly what is on screen without having to be told.
pub fn find_list_rows(area: Rect) -> usize {
    let height = area.height.saturating_sub(4).max(8);
    // the list, then a blank line, the count and the buttons
    height.saturating_sub(2 + 3).max(1) as usize
}

/// MC's find results window: the matches as they arrive, and the six
/// things to do with the one under the cursor.
fn draw_find_results(frame: &mut Frame, d: &crate::app::FindResults) {
    use crate::app::FIND_BUTTONS;
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let area = frame.area();
    let width = area.width.saturating_sub(8).max(30);
    let height = area.height.saturating_sub(4).max(8);
    debug_assert_eq!(
        find_list_rows(area),
        height.saturating_sub(2 + 3).max(1) as usize
    );
    let inner = popup(
        frame,
        centered(width, height, area),
        &format!(" {} ", d.label),
        style,
    );
    let list_rows = find_list_rows(area);
    for i in 0..list_rows {
        let Some(at) = d.top.checked_add(i).filter(|at| *at < d.rows.len()) else {
            break;
        };
        let row = Rect {
            x: inner.x + 1,
            y: inner.y + i as u16,
            width: inner.width.saturating_sub(2),
            height: 1,
        };
        // path:line: text, the path kept and the text cut, since the
        // path is what the row is about
        let width = (row.width as usize).saturating_sub(1);
        let path = d.label_of(at);
        let text = match &d.rows[at].hit {
            None => tail(&path, width),
            Some(hit) => {
                let head = format!("{}:{}: ", tail(&path, width / 2), hit.line);
                let room = width.saturating_sub(head.chars().count());
                format!("{head}{}", hit.text.chars().take(room).collect::<String>())
            }
        };
        let marked = Style::new().fg(th().mark_fg).bg(th().dialog_bg);
        let row_style = match (at == d.selected, d.rows[at].marked) {
            (true, _) => sel,
            (false, true) => marked,
            _ => style,
        };
        let mark = if d.rows[at].marked { '*' } else { ' ' };
        frame.render_widget(
            Line::from(format!("{mark}{text:<width$}")).style(row_style),
            row,
        );
    }
    let count = match d.done {
        Some((matches, scanned)) => format!("{matches} match(es), {scanned} entries scanned"),
        None => format!("searching… {} found", d.rows.len()),
    };
    frame.render_widget(
        Line::from(count).centered().style(style),
        Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(2),
            width: inner.width,
            height: 1,
        },
    );
    frame.render_widget(
        buttons_line(FIND_BUTTONS, d.button, style, sel),
        Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        },
    );
}

/// The fuzzy finder: the field on top, the best matches under it with
/// the letters that made each match lit, and how much of the tree has
/// been looked at.
/// Alt+X: the actions matching what is typed, with their menu label and
/// the key each is on.
fn draw_palette(frame: &mut Frame, d: &crate::app::PaletteDialog) {
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let area = frame.area();
    let width = area.width.saturating_sub(8).clamp(30, 96);
    let height = area.height.saturating_sub(6).clamp(8, 24);
    let inner = popup(
        frame,
        centered(width, height, area),
        " Command palette ",
        style,
    );
    let line = |y: u16| Rect {
        x: inner.x + 1,
        y: inner.y + y,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    field_row(frame, line(0), &d.field.value, Some(d.field.cursor));
    let rows = inner.height.saturating_sub(3) as usize;
    let first = d.selected.saturating_sub(rows.saturating_sub(1));
    let w = line(0).width as usize;
    for (i, &at) in d.shown.iter().skip(first).take(rows).enumerate() {
        let row = &d.rows[at];
        let text = format!(
            "{:<22} {:<34} {}",
            row.name,
            fit(&row.label, 34, false),
            row.keys
        );
        let text: String = text.chars().take(w).collect();
        frame.render_widget(
            Line::from(format!("{text:<w$}")).style(if first + i == d.selected {
                sel
            } else {
                style
            }),
            line(1 + i as u16),
        );
    }
    frame.render_widget(
        Line::from(format!(
            "{} of {}   Enter runs it · Esc",
            d.shown.len(),
            d.rows.len()
        ))
        .centered()
        .style(style),
        line(inner.height.saturating_sub(1)),
    );
}

fn draw_fuzzy(frame: &mut Frame, d: &crate::app::FuzzyDialog) -> Option<crate::app::DialogRows> {
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let lit = Style::new()
        .fg(th().mark_fg)
        .bg(th().dialog_bg)
        .add_modifier(Modifier::BOLD);
    let area = frame.area();
    let width = area.width.saturating_sub(8).max(30);
    let height = area.height.saturating_sub(4).max(8);
    let inner = popup(frame, centered(width, height, area), " Fuzzy find ", style);
    let line = |y: u16| Rect {
        x: inner.x + 1,
        y: inner.y + y,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    field_row(frame, line(0), &d.field.value, Some(d.field.cursor));
    let rows = inner.height.saturating_sub(3) as usize;
    let first = d.selected.saturating_sub(rows.saturating_sub(1));
    let list = crate::app::DialogRows {
        area: Rect {
            y: line(1).y,
            height: rows as u16,
            ..line(1)
        },
        rows: (first..first + rows)
            .map(|at| (at < d.shown.len()).then_some(at))
            .collect(),
    };
    for (i, &at) in d.shown.iter().skip(first).take(rows).enumerate() {
        let (path, is_dir) = &d.all[at];
        let shown = if *is_dir {
            format!("{path}/")
        } else {
            path.clone()
        };
        let chosen = first + i == d.selected;
        let lit_at = rcmd_core::fuzzy::fuzzy_match(&d.field.value, path)
            .map(|m| m.positions)
            .unwrap_or_default();
        let width = line(0).width as usize;
        let spans: Vec<Span> = shown
            .chars()
            .take(width)
            .enumerate()
            .map(|(c, ch)| {
                let style = match (chosen, lit_at.contains(&c)) {
                    (true, _) => sel,
                    (false, true) => lit,
                    _ => style,
                };
                Span::styled(ch.to_string(), style)
            })
            .collect();
        let mut row_line = Line::from(spans);
        if chosen {
            row_line = row_line.style(sel);
        }
        frame.render_widget(row_line, line(1 + i as u16));
    }
    let count = match &d.walking {
        Some(_) => format!("{} of {} so far…", d.shown.len(), d.all.len()),
        None => format!("{} of {}", d.shown.len(), d.all.len()),
    };
    frame.render_widget(
        Line::from(format!(
            "{count}   Enter goes there · F3 view · F4 edit · Esc"
        ))
        .centered()
        .style(style),
        line(inner.height.saturating_sub(1)),
    );
    Some(list)
}

fn draw_find(frame: &mut Frame, d: &FindDialog, hits: &mut Vec<(Rect, FormHit)>) {
    use crate::app::{FIND_FIELD_LABELS, FIND_FIELDS, FIND_ROWS, FIND_SWITCHES};
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    // a field a row, the switches two to a row, the buttons
    let switch_rows = FIND_SWITCHES.len().div_ceil(2);
    let height = (FIND_FIELDS + switch_rows + 5) as u16;
    let inner = popup(
        frame,
        centered(72, height, frame.area()),
        " Find file ",
        style,
    );
    let row = |offset: usize| Rect {
        x: inner.x + 1,
        y: inner.y + offset as u16,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    const LABEL: u16 = 13;
    for (i, label) in FIND_FIELD_LABELS.iter().enumerate() {
        let at = row(i);
        hits.push((at, FormHit::Row(i)));
        frame.render_widget(Line::from(*label).style(style), at);
        let Some(field) = d.field_at(i) else { continue };
        field_row(
            frame,
            Rect {
                x: at.x + LABEL,
                width: at.width.saturating_sub(LABEL),
                ..at
            },
            &field.value,
            (d.row == i).then_some(field.cursor),
        );
    }
    let check = |on: bool| if on { "[x]" } else { "[ ]" };
    let half = inner.width.saturating_sub(2) / 2;
    for (i, label) in FIND_SWITCHES.iter().enumerate() {
        let focused = d.row == FIND_FIELDS + i;
        let at = row(FIND_FIELDS + 1 + i / 2);
        let at = Rect {
            x: at.x + (i % 2) as u16 * half,
            width: half,
            ..at
        };
        hits.push((at, FormHit::Row(FIND_FIELDS + i)));
        frame.render_widget(
            Line::from(format!("{} {label}", check(d.switch(i)))).style(if focused {
                sel
            } else {
                style
            }),
            at,
        );
    }
    let buttons_at = Rect {
        x: inner.x,
        y: inner.y + (FIND_FIELDS + switch_rows + 2) as u16,
        width: inner.width,
        height: 1,
    };
    for (i, rect) in button_rects(&["OK", "Cancel"], buttons_at)
        .into_iter()
        .enumerate()
    {
        hits.push((rect, FormHit::Button(i)));
    }
    frame.render_widget(
        buttons_line(
            &["OK", "Cancel"],
            if d.row == FIND_ROWS {
                usize::from(!d.ok)
            } else {
                usize::MAX
            },
            style,
            sel,
        ),
        buttons_at,
    );
}

/// The F2 user menu: `[[commands]]` from the config, first nine with
/// digit hotkeys.
fn draw_user_menu(
    frame: &mut Frame,
    d: &crate::app::UserMenuDialog,
) -> Option<crate::app::DialogRows> {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let commands = d.entries();
    let rows = commands.len().max(1) as u16;
    let area = centered(60, (rows + 2).min(20), frame.area());
    frame.render_widget(Clear, area);
    let title = match d.path.is_empty() {
        true if d.local => " User menu (.mc.menu) ".to_string(),
        true => " User menu ".to_string(),
        false => format!(" {} ", submenu_path(d)),
    };
    let hint = match d.path.is_empty() {
        true => " Enter or 1-9 runs ",
        false => " Enter runs · ← back ",
    };
    let block = Block::bordered()
        .title(title)
        .title_bottom(Line::from(hint).centered())
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
        // a submenu says so where a command would show its command line
        let what = match cmd.is_submenu() {
            true => format!("{} entries...", cmd.entries.len()),
            false => cmd.run.replace('\n', " ; "),
        };
        let text: String = format!(" {hotkey} {:<name_w$}  {what}", cmd.name)
            .chars()
            .take(inner.width as usize)
            .collect();
        let style = if i == d.row { sel } else { base };
        frame.render_widget(
            Line::from(format!("{text:<w$}", w = inner.width as usize)).style(style),
            row,
        );
    }
    Some(crate::app::DialogRows {
        area: Rect {
            height: (commands.len() as u16).min(inner.height),
            ..inner
        },
        rows: (0..commands.len().min(inner.height as usize))
            .map(Some)
            .collect(),
    })
}

/// The names on the way into a submenu, for its title.
fn submenu_path(d: &crate::app::UserMenuDialog) -> String {
    let mut names = Vec::new();
    let mut here = &d.menu[..];
    for &at in &d.path {
        match here.get(at) {
            Some(entry) => {
                names.push(entry.name.as_str());
                here = &entry.entries;
            }
            None => break,
        }
    }
    names.join(" / ")
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
/// C-x l / s / v / C-s: the link form. What to point at on top, what to
/// call it below - except when editing a link, which already has a name.
fn draw_link(frame: &mut Frame, d: &crate::app::LinkDialog, hits: &mut Vec<(Rect, FormHit)>) {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let rows = d.rows();
    let area = centered(64, rows as u16 + 4, frame.area());
    let inner = popup(frame, area, d.title(), base);
    let width = inner.width.saturating_sub(2) as usize;
    let row_at = |i: u16| Rect {
        x: inner.x + 1,
        y: inner.y + i,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    let field = |text: &str, label: &str| {
        let room = width.saturating_sub(label.len());
        format!("{label}{:<room$}", tail(text, room))
    };

    let labels: &[(&str, &str, usize)] = if rows == 1 {
        &[("points at ", "target", 0)]
    } else {
        &[("points at ", "target", 0), ("named     ", "name", 1)]
    };
    for (label, which, row) in labels {
        let text = if *which == "target" {
            &d.target.value
        } else {
            &d.name.value
        };
        frame.render_widget(
            Line::from(field(text, label)).style(if d.row == *row { sel } else { base }),
            row_at(*row as u16),
        );
    }
    let selected = if d.row == rows {
        usize::from(!d.ok)
    } else {
        usize::MAX
    };
    for row in 0..rows {
        hits.push((row_at(row as u16), FormHit::Row(row)));
    }
    for (i, rect) in button_rects(&["OK", "Cancel"], row_at(rows as u16 + 1))
        .into_iter()
        .enumerate()
    {
        hits.push((rect, FormHit::Button(i)));
    }
    frame.render_widget(
        buttons_line(&["OK", "Cancel"], selected, base, sel),
        row_at(rows as u16 + 1),
    );
    if d.row < rows {
        let cursor = if d.row == 0 {
            d.target.cursor
        } else {
            d.name.cursor
        };
        let x = inner.x + 11 + cursor.min(width.saturating_sub(12)) as u16;
        frame.set_cursor_position((x, inner.y + d.row as u16));
    }
}

/// C-x o: MC's chown window - the system's users and groups as two pick
/// lists. Typing an owner is fine when you know the name; picking is
/// what you want when you do not, which is most of the time.
fn draw_chown(frame: &mut Frame, d: &crate::app::ChownDialog, hits: &mut Vec<(Rect, FormHit)>) {
    use crate::app::{CHOWN_BUTTON_COL, CHOWN_BUTTONS, CHOWN_RECURSE_COL, CHOWN_ROWS};
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    // an unfocused list still shows where its cursor is, just quietly
    let idle = Style::new()
        .fg(th().dialog_fg)
        .bg(th().dialog_bg)
        .add_modifier(Modifier::REVERSED);
    let head = head_style();
    let area = centered(62, CHOWN_ROWS as u16 + 5, frame.area());
    let inner = popup(frame, area, " Chown ", base);
    let (col_w, gap) = (16u16, 1u16);
    let row_at = |x: u16, w: u16, i: u16| Rect {
        x: inner.x + 1 + x,
        y: inner.y + i,
        width: w,
        height: 1,
    };

    for (index, (title, list, cursor)) in [
        ("User", &d.users, d.user_row),
        ("Group", &d.groups, d.group_row),
    ]
    .into_iter()
    .enumerate()
    {
        let x = index as u16 * (col_w + gap);
        frame.render_widget(Line::from(title).style(head), row_at(x, col_w, 0));
        // scrolled so the cursor stays in view, mid-window
        let first = cursor
            .saturating_sub(CHOWN_ROWS / 2)
            .min(list.len().saturating_sub(CHOWN_ROWS));
        for i in 0..CHOWN_ROWS {
            let Some((_, name)) = list.get(first + i) else {
                break;
            };
            let style = if first + i != cursor {
                base
            } else if d.column == index {
                sel
            } else {
                idle
            };
            // clipped at the end, not the start: `tail` keeps the tail
            // of a path, which for a name is the half you can spare
            let text: String = name.chars().take(col_w as usize).collect();
            frame.render_widget(
                Line::from(format!("{text:<w$}", w = col_w as usize)).style(style),
                row_at(x, col_w, i as u16 + 1),
            );
            hits.push((
                row_at(x, col_w, i as u16 + 1),
                FormHit::Item(index, first + i),
            ));
        }
    }

    // the File section, past both lists
    let facts_x = 2 * (col_w + gap);
    let facts_w = inner.width.saturating_sub(facts_x + 2);
    for (i, fact) in [
        format!(
            "name  {}",
            tail(&d.name, facts_w.saturating_sub(6) as usize)
        ),
        format!("owner {}", d.owner),
        format!("group {}", d.group),
        format!("{} item(s)", d.paths.len()),
    ]
    .iter()
    .enumerate()
    {
        frame.render_widget(
            Line::from(fact.as_str()).style(base),
            row_at(facts_x, facts_w, i as u16 + 1),
        );
    }

    let recurse = format!(
        " {} recurse into directories",
        if d.recurse { "[x]" } else { "[ ]" }
    );
    let width = inner.width.saturating_sub(2) as usize;
    frame.render_widget(
        Line::from(format!("{recurse:<width$}")).style(if d.column == CHOWN_RECURSE_COL {
            sel
        } else {
            base
        }),
        row_at(0, inner.width.saturating_sub(2), CHOWN_ROWS as u16 + 1),
    );
    let selected = if d.column == CHOWN_BUTTON_COL {
        d.button
    } else {
        usize::MAX
    };
    frame.render_widget(
        buttons_line(CHOWN_BUTTONS, selected, base, sel),
        row_at(0, inner.width.saturating_sub(2), CHOWN_ROWS as u16 + 2),
    );
    hits.push((
        row_at(0, inner.width.saturating_sub(2), CHOWN_ROWS as u16 + 1),
        FormHit::Row(CHOWN_RECURSE_COL),
    ));
    let buttons = row_at(0, inner.width.saturating_sub(2), CHOWN_ROWS as u16 + 2);
    for (i, rect) in button_rects(CHOWN_BUTTONS, buttons).into_iter().enumerate() {
        hits.push((rect, FormHit::Button(i)));
    }
}

/// C-x e: mc's chattr window. The flags as check boxes, each with its
/// `lsattr` letter, and the cursor entry's name and flags beside them.
fn draw_chattr(frame: &mut Frame, d: &crate::app::ChattrDialog) {
    use crate::app::{CHATTR_BUTTONS, CHATTR_ROWS};
    use rcmd_core::attrs::{FLAGS, letters};
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    // a heading row, the flags, a blank, the buttons
    let area = centered(64, CHATTR_ROWS as u16 + 5, frame.area());
    let inner = popup(frame, area, " Chattr ", base);
    let row_at = |i: u16| Rect {
        x: inner.x + 1,
        y: inner.y + i,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    let left = 40usize; // where the File column starts
    frame.render_widget(
        Line::from(format!(" {:<w$}{}", "Flags", "File", w = left - 1)).style(head_style()),
        row_at(0),
    );
    let facts = [
        format!("name  {}", tail(&d.name, 14)),
        format!("was   {}", letters(d.was)),
        format!("now   {}", letters(d.flags)),
    ];
    for (i, (letter, bit, label)) in FLAGS.iter().enumerate() {
        let mark = if d.flags & bit != 0 { "[x]" } else { "[ ]" };
        let row = row_at(i as u16 + 1);
        frame.render_widget(
            Line::from(format!(" {mark} {letter} {label:<w$}", w = left - 9))
                .style(if d.row == i { sel } else { base }),
            Rect {
                width: left as u16,
                ..row
            },
        );
        if let Some(fact) = facts.get(i) {
            frame.render_widget(
                Line::from(fact.as_str()).style(base),
                Rect {
                    x: row.x + left as u16,
                    width: row.width.saturating_sub(left as u16),
                    ..row
                },
            );
        }
    }
    let selected = if d.row == CHATTR_ROWS {
        d.button
    } else {
        usize::MAX
    };
    frame.render_widget(
        buttons_line(CHATTR_BUTTONS, selected, base, sel),
        row_at(CHATTR_ROWS as u16 + 3),
    );
}

/// C-x c: MC's chmod window. The bits on the left as check boxes, what
/// is being changed on the right, and the octal underneath - typing in
/// it moves the boxes, flipping a box rewrites it.
fn draw_chmod(frame: &mut Frame, d: &crate::app::ChmodDialog, hits: &mut Vec<(Rect, FormHit)>) {
    use crate::app::{CHMOD_BITS, CHMOD_BUTTONS, CHMOD_OCTAL_ROW, CHMOD_RECURSE_ROW, CHMOD_ROWS};
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let head = head_style();
    // a heading row, the bits, the octal, recurse, a blank, the buttons
    let area = centered(58, CHMOD_BITS.len() as u16 + 7, frame.area());
    let inner = popup(frame, area, " Chmod ", base);
    let row_at = |i: u16| Rect {
        x: inner.x + 1,
        y: inner.y + i,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    let left = 22usize; // where the File column starts

    frame.render_widget(
        Line::from(format!(" {:<left$}{}", "Permissions", "File")).style(head),
        row_at(0),
    );
    // the File column, beside the first rows of bits
    let facts = [
        format!("name  {}", tail(&d.name, 28)),
        format!("perm  {:04o}", d.mode),
        format!("owner {}", d.owner),
        format!("group {}", d.group),
    ];
    for (i, (label, bit)) in CHMOD_BITS.iter().enumerate() {
        let mark = if d.mode & bit != 0 { "[x]" } else { "[ ]" };
        let text = format!(" {mark} {label}");
        let row = row_at(i as u16 + 1);
        // the bit half highlights on its own; the facts sit past it
        let bit_area = Rect {
            width: left as u16,
            ..row
        };
        frame.render_widget(
            Line::from(format!("{text:<left$}")).style(if d.row == i { sel } else { base }),
            bit_area,
        );
        hits.push((bit_area, FormHit::Row(i)));
        if let Some(fact) = facts.get(i) {
            frame.render_widget(
                Line::from(fact.as_str()).style(base),
                Rect {
                    x: row.x + left as u16,
                    width: row.width.saturating_sub(left as u16),
                    ..row
                },
            );
        }
    }
    let octal_row = CHMOD_OCTAL_ROW as u16 + 1;
    frame.render_widget(
        Line::from(format!(
            " octal {:<w$}",
            d.octal,
            w = left.saturating_sub(7)
        ))
        .style(if d.row == CHMOD_OCTAL_ROW { sel } else { base }),
        Rect {
            width: left as u16,
            ..row_at(octal_row)
        },
    );
    hits.push((
        Rect {
            width: left as u16,
            ..row_at(octal_row)
        },
        FormHit::Row(CHMOD_OCTAL_ROW),
    ));
    hits.push((row_at(octal_row + 1), FormHit::Row(CHMOD_RECURSE_ROW)));
    for (i, rect) in button_rects(CHMOD_BUTTONS, row_at(octal_row + 3))
        .into_iter()
        .enumerate()
    {
        hits.push((rect, FormHit::Button(i)));
    }
    let recurse = format!(
        " {} recurse into directories",
        if d.recurse { "[x]" } else { "[ ]" }
    );
    frame.render_widget(
        Line::from(format!(
            "{recurse:<w$}",
            w = inner.width.saturating_sub(2) as usize
        ))
        .style(if d.row == CHMOD_RECURSE_ROW {
            sel
        } else {
            base
        }),
        row_at(octal_row + 1),
    );
    let selected = if d.row == CHMOD_ROWS {
        d.button
    } else {
        usize::MAX
    };
    frame.render_widget(
        buttons_line(CHMOD_BUTTONS, selected, base, sel),
        row_at(octal_row + 3),
    );
    if d.row == CHMOD_OCTAL_ROW {
        let x = inner.x + 7 + d.octal_cursor.min(8) as u16;
        frame.set_cursor_position((x, inner.y + octal_row));
    }
}

/// F5/F6: MC's copy/move form. The destination on top, the switches
/// that change what the copy does under it, then OK / Background /
/// Cancel - Background starts the job detached, which is otherwise only
/// reachable by pressing b once it is already running.
fn draw_transfer(
    frame: &mut Frame,
    d: &crate::app::TransferDialog,
    hits: &mut Vec<(Rect, FormHit)>,
) {
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
    // both are input fields and both look like one, focused or not -
    // the cursor says which is being typed into, as in every other form
    let field = |text: &str, label: &str| {
        let room = width.saturating_sub(label.len());
        Line::from(vec![
            Span::styled(label.to_string(), base),
            Span::styled(format!("{:<room$}", tail(text, room)), sel),
        ])
    };
    for row in 0..TRANSFER_ROWS {
        hits.push((row_at(row as u16), FormHit::Row(row)));
    }
    for (i, rect) in button_rects(
        crate::app::TRANSFER_BUTTONS,
        row_at(TRANSFER_ROWS as u16 + 1),
    )
    .into_iter()
    .enumerate()
    {
        hits.push((rect, FormHit::Button(i)));
    }
    frame.render_widget(field(&d.mask.value, " mask "), row_at(0));
    frame.render_widget(
        field(&d.dest.value, " to   "),
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
    let buttons = crate::app::TRANSFER_BUTTONS;
    let selected = if d.row == TRANSFER_ROWS {
        d.button
    } else {
        usize::MAX
    };
    frame.render_widget(
        buttons_line(buttons, selected, base, sel),
        row_at(TRANSFER_ROWS as u16 + 1),
    );
    // the cursor sits in whichever of the two text rows has the focus
    let text_row = match d.row {
        0 => Some((0u16, d.mask.cursor)),
        TRANSFER_DEST_ROW => Some((TRANSFER_DEST_ROW as u16, d.dest.cursor)),
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

fn draw_hotlist(
    frame: &mut Frame,
    app: &App,
    d: &crate::app::HotlistDialog,
) -> Option<crate::app::DialogRows> {
    use crate::app::HotRow;
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let head = head_style();

    let group = app.hot_group(&d.group);
    let label_w = group
        .iter()
        .map(|e| e.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(16);
    // one drawn line per row, plus the "Recent:" heading, which is a
    // label rather than something the cursor can land on
    let mut lines: Vec<(String, Option<usize>)> = Vec::new();
    let mut recent_seen = false;
    for (at, row) in app.hotlist_rows(d).into_iter().enumerate() {
        match row {
            HotRow::Up => lines.push((" ..".into(), Some(at))),
            HotRow::Group(i) => {
                let label = group.get(i).map(|e| e.label.as_str()).unwrap_or_default();
                lines.push((format!(" {label:<label_w$}  /..."), Some(at)))
            }
            HotRow::Entry(i) => {
                let entry = match group.get(i) {
                    Some(entry) => entry,
                    None => continue,
                };
                let path = abbrev_home(std::path::Path::new(&entry.path));
                lines.push((format!(" {:<label_w$}  {path}", entry.label), Some(at)))
            }
            HotRow::Recent(loc) => {
                if !recent_seen {
                    recent_seen = true;
                    lines.push((" Recent:".into(), None));
                }
                let path = abbrev_home(std::path::Path::new(&loc));
                lines.push((format!("   {path}"), Some(at)))
            }
        }
    }

    let rows = lines.len().max(1) as u16;
    // wide enough for the key legend along the bottom edge, which is
    // the one line in here that must not be cut short
    let area = centered(62, (rows + 2).min(20), frame.area());
    frame.render_widget(Clear, area);
    // the title says where in the tree this is, and what is in hand
    let where_ = match d.group.is_empty() {
        true => " Directory hotlist ".to_string(),
        false => format!(" Hotlist: {} ", app.hotlist_group_path(d)),
    };
    let hint = match (&d.moving, &d.filter) {
        (Some(entry), _) => format!(" moving \"{}\" - m puts it here ", entry.label),
        // while the filter is open it is what the bottom edge is for:
        // the letters are going into it, not into the commands
        (None, Some(needle)) => format!(" search: {needle}_  (Esc clears) "),
        (None, None) => {
            " Enter go · C-s search · a add · g group · e rename · m move · d drop ".into()
        }
    };
    let block = Block::bordered()
        .title(where_)
        .title_bottom(Line::from(hint).centered())
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if lines.is_empty() {
        frame.render_widget(
            Line::from(" empty - press 'a' to add the current directory ").centered(),
            inner,
        );
        return None;
    }
    // keep the selected row in view when the list outgrows the dialog
    let sel_row = lines
        .iter()
        .position(|(_, s)| *s == Some(d.row))
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
            Some(s) if *s == d.row => sel,
            Some(_) => base,
            None => head,
        };
        frame.render_widget(
            Line::from(format!("{text:<w$}", w = inner.width as usize)).style(style),
            row,
        );
    }
    let shown = lines.len().saturating_sub(first).min(inner.height as usize);
    Some(crate::app::DialogRows {
        area: Rect {
            height: shown as u16,
            ..inner
        },
        // the "Recent:" heading is a line the cursor cannot land on,
        // which is exactly what the None in here means
        rows: lines[first..first + shown]
            .iter()
            .map(|(_, at)| *at)
            .collect(),
    })
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
    frame.render_widget(
        buttons_line(crate::app::YES_NO, selected, base, sel),
        buttons,
    );
}

fn draw_confirm(frame: &mut Frame, d: &ConfirmDialog, hits: &mut Vec<(Rect, FormHit)>) {
    // red is for what cannot be taken back: a permanent delete, an
    // overwrite. The trash and a question are ordinary dialogs.
    let (style, sel) = if d.permanent {
        (
            Style::new().fg(th().error_fg).bg(th().error_bg),
            Style::new().fg(th().dialog_fg).bg(th().dialog_bg),
        )
    } else {
        (
            Style::new().fg(th().dialog_fg).bg(th().dialog_bg),
            Style::new().fg(th().select_fg).bg(th().select_bg),
        )
    };
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
    frame.render_widget(
        buttons_line(crate::app::YES_NO, selected, style, sel),
        buttons,
    );
    for (i, rect) in button_rects(crate::app::YES_NO, buttons)
        .into_iter()
        .enumerate()
    {
        hits.push((rect, FormHit::Button(i)));
    }
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
            frame.render_widget(
                buttons_line(crate::app::YES_NO, selected, style, sel),
                row(5),
            );
        }
        ConnectAsk::Password {
            prompt,
            value,
            cursor,
            echo,
        } => {
            let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
            let area = centered(56, 6, frame.area());
            let title = match prompt.starts_with("[sudo]") {
                true => " sudo ",
                false => " SSH authentication ",
            };
            let inner = popup(frame, area, title, style);
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
            field_row(frame, row(1), &shown, Some(*cursor));
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
/// M-h and M-H: a history, newest first - the command line's, where
/// Enter puts the selected line back for editing, or the panel's
/// directories, where Enter goes there. `current` marks the entry the
/// panel is on now, which the command line has no notion of.
fn draw_history(
    frame: &mut Frame,
    title: &str,
    history: &[String],
    selected: usize,
    current: Option<usize>,
) -> Option<crate::app::DialogRows> {
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let rows = history.len().min(16) as u16;
    let area = centered(64, rows + 2, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(title)
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
        let mark = if current == Some(history.len() - 1 - i) {
            '*'
        } else {
            ' '
        };
        let text = format!(
            "{mark}{:<width$}",
            tail(cmd, inner.width as usize - 1),
            width = inner.width as usize - 1
        );
        frame.render_widget(
            Line::from(text).style(if i == selected { sel } else { style }),
            line,
        );
    }
    let shown = history.len().saturating_sub(first).min(visible);
    Some(crate::app::DialogRows {
        area: Rect {
            height: shown as u16,
            ..inner
        },
        rows: (0..shown).map(|r| Some(first + r)).collect(),
    })
}

/// The active VFS list: what the panels are sitting on that is not the
/// local filesystem. The panel column is what makes the list actionable
/// - it says which side freeing a row will move.
fn draw_vfs(frame: &mut Frame, dialog: &VfsDialog) {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let rows = dialog.rows.len().max(1) as u16;
    let area = centered(72, (rows + 2).min(16), frame.area());
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Active VFS and mounts ")
        .title_bottom(Line::from(" Enter go there · f free · Esc close ").centered())
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if dialog.rows.is_empty() {
        frame.render_widget(Line::from(" nothing open ").centered(), inner);
        return;
    }
    let selected = dialog.selected.min(dialog.rows.len() - 1);
    for (i, row) in dialog.rows.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let area = Rect {
            y: inner.y + i as u16,
            height: 1,
            ..inner
        };
        let used = match row.used_by.as_slice() {
            [] => "idle".to_string(),
            [0] => "left".to_string(),
            [1] => "right".to_string(),
            _ => "both".to_string(),
        };
        let kind = match row.kind {
            crate::app::VfsKind::Remote => "sftp",
            crate::app::VfsKind::Archive => "arch",
            crate::app::VfsKind::Mount => "disk",
        };
        let width = inner.width as usize;
        let label = tail(&row.label, width.saturating_sub(13));
        let text = format!(" {kind} {used:>5}  {label}");
        frame.render_widget(
            Line::from(format!("{text:<width$}")).style(if i == selected { sel } else { base }),
            area,
        );
    }
}

/// The named filter sets, and which the panel is under. Several at
/// once is ordinary: the list is a set of switches, not a menu.
fn draw_filters(
    frame: &mut Frame,
    sets: &[crate::config::FilterSet],
    d: &crate::app::FiltersDialog,
) -> Option<crate::app::DialogRows> {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let shown = sets
        .len()
        .min(frame.area().height.saturating_sub(4) as usize)
        .max(1);
    let inner = popup(
        frame,
        centered(64, shown as u16 + 2, frame.area()),
        match d.panel {
            0 => " Filter sets (left panel) ",
            _ => " Filter sets (right panel) ",
        },
        base,
    );
    let name_width = 20.min((inner.width as usize).saturating_sub(30));
    for (i, set) in sets.iter().take(shown).enumerate() {
        let row = Rect {
            y: inner.y + i as u16,
            height: 1,
            ..inner
        };
        let text = format!(
            " [{}] {}  {}",
            if d.on.get(i).copied().unwrap_or(false) {
                "x"
            } else {
                " "
            },
            fit(&set.name, name_width, false),
            set.mask,
        );
        frame.render_widget(
            Line::from(format!(
                "{text:<w$}",
                w = (inner.width as usize).saturating_sub(1)
            ))
            .style(if i == d.row { sel } else { base }),
            row,
        );
    }
    Some(crate::app::DialogRows {
        area: Rect {
            height: shown as u16,
            ..inner
        },
        rows: (0..shown).map(Some).collect(),
    })
}

/// The synchronize plan: one row per difference, the arrow saying
/// which way it would be copied and the note saying why it is here.
fn draw_sync(frame: &mut Frame, d: &crate::app::SyncDialog) -> Option<crate::app::DialogRows> {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    // a row switched off is still readable, just not shouting
    let off = Style::new()
        .fg(th().dialog_fg)
        .bg(th().dialog_bg)
        .add_modifier(Modifier::DIM);
    let width = frame.area().width.saturating_sub(6).clamp(40, 90);
    let shown = d
        .rows
        .len()
        .min(frame.area().height.saturating_sub(8) as usize)
        .max(1);
    let area = centered(width, shown as u16 + 4, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Synchronize directories ")
        .title_bottom(
            Line::from(
                " Space skip · ←/→ way · m mirror · +/- mask · F3 diff · a all · Enter run ",
            )
            .centered(),
        )
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let on = d.rows.iter().filter(|r| r.on).count();
    // the two directories and how much of the plan is switched on, in
    // the room that is left after the counts
    let mirror = match d.mirror {
        crate::app::Mirror::Off => "",
        crate::app::Mirror::Right => "  mirror -->",
        crate::app::Mirror::Left => "  mirror <--",
    };
    let room = (inner.width as usize).saturating_sub(24 + mirror.len()) / 2;
    let header = match &d.mask {
        Some((field, on)) => format!(
            " {} rows matching: {}",
            if *on { "switch on" } else { "switch off" },
            field.value
        ),
        None => format!(
            " {on} of {} on{mirror}   {} --> <-- {}",
            d.rows.len(),
            fit(&d.left, room, false).trim_end(),
            fit(&d.right, room, false).trim_end(),
        ),
    };
    frame.render_widget(Line::from(header).style(base), Rect { height: 1, ..inner });
    if let Some((field, on)) = &d.mask {
        let label = if *on {
            " switch on rows matching: "
        } else {
            " switch off rows matching: "
        };
        let x = inner.x + (label.chars().count() + field.cursor) as u16;
        frame.set_cursor_position((x.min(inner.right().saturating_sub(1)), inner.y));
    }
    // keep the cursor row inside the window whatever `top` says
    let top = d
        .top
        .min(d.cursor)
        .max((d.cursor + 1).saturating_sub(shown));
    // the notes are the reason for each row: they get the room they
    // need, and the path the rest
    let note_width = d
        .rows
        .iter()
        .map(|r| r.note.len())
        .max()
        .unwrap_or(0)
        .min(44);
    let name_width = (inner.width as usize)
        .saturating_sub(13 + note_width)
        .max(10);
    for i in 0..shown {
        let Some(row) = d.rows.get(top + i) else {
            break;
        };
        let line = Rect {
            y: inner.y + 2 + i as u16,
            height: 1,
            ..inner
        };
        let style = match (top + i == d.cursor, row.on) {
            (true, _) => sel,
            (false, true) => base,
            (false, false) => off,
        };
        let way = match row.step {
            rcmd_core::fsops::SyncStep::ToRight => "-->",
            rcmd_core::fsops::SyncStep::ToLeft => "<--",
            rcmd_core::fsops::SyncStep::DeleteLeft => "del <",
            rcmd_core::fsops::SyncStep::DeleteRight => "> del",
        };
        let dir = if row.left.or(row.right) == Some(true) {
            "/"
        } else {
            ""
        };
        let text = format!(
            " [{}] {} {way:<5} {}",
            if row.on { "x" } else { " " },
            fit(&format!("{}{dir}", row.rel.display()), name_width, false),
            row.note,
        );
        frame.render_widget(
            Line::from(format!(
                "{text:<w$}",
                w = (inner.width as usize).saturating_sub(1)
            ))
            .style(style),
            line,
        );
    }
    Some(crate::app::DialogRows {
        area: Rect {
            y: inner.y + 2,
            height: shown as u16,
            ..inner
        },
        rows: (0..shown).map(|i| Some(top + i)).collect(),
    })
}

fn draw_jobs(frame: &mut Frame, jobs: &[Job], selected: usize) -> Option<crate::app::DialogRows> {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let rows = jobs.len().max(1) as u16;
    let area = centered(70, (rows + 2).min(16), frame.area());
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Jobs ")
        .title_bottom(Line::from(" Enter foreground · p pause · c cancel · Esc close ").centered())
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if jobs.is_empty() {
        frame.render_widget(Line::from(" nothing running ").centered(), inner);
        return None;
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
        let state = if job.handle.is_held() {
            "queued  "
        } else if job.handle.is_paused() {
            "paused  "
        } else {
            ""
        };
        let text = tail(
            &format!(" {pct} {counts:>9}  {state}{}", job.title.trim()),
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
    let shown = jobs.len().min(inner.height as usize);
    Some(crate::app::DialogRows {
        area: Rect {
            height: shown as u16,
            ..inner
        },
        rows: (0..shown).map(Some).collect(),
    })
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
        Line::from(match job.handle.is_paused() {
            true => "paused - p goes on   Esc - cancel",
            false => "Esc - cancel   b - background   p - pause",
        })
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
        let when = f.mtime.map(format_time).unwrap_or_else(|| "unknown".into());
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

    #[test]
    fn the_command_line_cursor_counts_cells() {
        // two wide characters, then the cursor: four cells in, not two
        assert_eq!(cmdline_window("日本", 2, 20), ("日本".to_string(), 4));
        assert_eq!(cmdline_window("ab", 1, 20), ("ab".to_string(), 1));
        // a line wider than the field scrolls so the cursor stays on it
        let (visible, before) = cmdline_window("一二三四五六", 6, 7);
        assert_eq!((visible.as_str(), before), ("四五六", 6));
        assert_eq!(cmdline_window("", 0, 5), (String::new(), 0));
        // an emoji family is two cells wide, as the terminal draws it
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}x";
        assert_eq!(cmdline_window(family, 5, 20).1, 2);
        // an accent takes no cell of its own
        assert_eq!(cmdline_window("e\u{301}", 2, 20).1, 1);
    }

    #[test]
    fn the_inspector_reads_both_ways() {
        let [le, be] = hex_inspect(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f]);
        assert!(le.contains("u16 1 ") && le.contains("u32 1 "), "{le}");
        assert!(le.contains("f64 1"), "{le}");
        assert!(
            be.contains("u16 256 ") && be.contains("u32 16777216 "),
            "{be}"
        );
        // fewer bytes than a width asks for: that width is left out
        let [short, _] = hex_inspect(&[0xff, 0xff]);
        assert!(
            short.contains("i16 -1") && !short.contains("u32"),
            "{short}"
        );
    }

    #[test]
    fn fit_and_tail_count_cells_not_characters() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(fit("日本語", 6, false), "日本語");
        assert_eq!(fit("日本", 6, false).width(), 6);
        // cut through the middle of a wide character: a blank, not half
        assert_eq!(fit("日本語", 5, false), "日本 ");
        assert_eq!(tail("abc日本語", 5), "…本語");
        assert_eq!(tail("日本語", 6), "日本語");
    }

    #[test]
    fn an_old_time_shows_its_year_and_a_new_one_its_time() {
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        assert!(format_time(old).ends_with("2001"), "{}", format_time(old));
        let now = std::time::SystemTime::now();
        assert!(format_time(now).contains(':'), "{}", format_time(now));
    }
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
    fn a_highlight_rule_takes_the_dialogs_mask_list() {
        let (rules, warnings) =
            compile_highlight(&[rule(Some("*.c,*.h|*_test.*"), None, "brightgreen")]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(rules[0].matches(&entry("parser.c", EntryKind::File, 0o644)));
        assert!(rules[0].matches(&entry("parser.h", EntryKind::File, 0o644)));
        assert!(!rules[0].matches(&entry("parser_test.c", EntryKind::File, 0o644)));
        assert!(!rules[0].matches(&entry("parser.rs", EntryKind::File, 0o644)));
        // a rule's mask is case-sensitive, as it was when it was one glob
        assert!(!rules[0].matches(&entry("PARSER.C", EntryKind::File, 0o644)));
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
