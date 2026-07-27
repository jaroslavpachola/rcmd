use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Gauge, Row, Table, TableState};
use rcmd_core::entry::{Entry, EntryKind};
use rcmd_core::panel::{ListMode, Panel};

use crate::app::{
    App, Ask, ConfirmDialog, ConnectAsk, Dialog, EditPrompt, FindDialog, InputDialog, Job, MENUS,
    MenuState, OPTION_ROWS, OptionsDialog, QuickView, menu_label,
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
    "  Alt+N           sort by name (again = reverse); others in F9 > Sort",
    "  Alt+T           cycle listing format: brief / full / long",
    "                  (an active long panel takes the whole width, MC's",
    "                  one-panel view; Tab or cycling back restores the split)",
    "  Alt+Left/Right  walk the panel's directory history (back/forward)",
    "  F9 > Options    panel options form (MC-style checkboxes): hidden",
    "                  files, lynx-like motion (Left/Right = parent/enter),",
    "                  mouse, auto-reload, git, subshell, editor, theme —",
    "                  applied live, saved to the config on exit",
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
    "                  entries — both work on sftp panels too",
    "  Ctrl+X s        create a symlink to the cursor entry",
    "  F9 > View       listing format: brief (names), full, long (ls -l,",
    "                  full-width); the panel footer shows free space",
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
    "  F5              copy",
    "  F6              move / rename",
    "  F7              make directory",
    "  F8              delete to trash",
    "  Shift+F8        delete permanently",
    "  Shift+F4        edit a new file (created on first save)",
    "  Shift+F5/F6     copy / rename the cursor file in place",
    "  F9 > File > Bulk rename   edit the marked names as text: each",
    "                  line is \"number TAB name\" — change names to",
    "                  rename (swaps are fine), delete lines to delete;",
    "                  save, close, and confirm the preview",
    "  Esc             cancel a running operation",
    "  b               send the running operation to the background",
    "  Ctrl+X j        jobs list: Enter foregrounds, c cancels; the",
    "                  status line shows aggregate background progress",
    "  Overwrite prompt hotkeys: o=overwrite a=all s=skip S=skip all",
    "  Error prompt hotkeys:     r=retry s=skip S=skip all",
    "",
    "# Archives",
    "  Enter on zip/tar/tar.{gz,xz,bz2} browses it; F5 copies out,",
    "  F3 views members. Move/delete/mkdir are disabled inside.",
    "  Copy INTO an archive: F5 with the other panel inside it, or a",
    "  destination written as archive.zip://dir — zip appends in place,",
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
    "  %t marked files, %% literal percent — all shell-quoted.",
    "",
    "# Command line",
    "  (type)          compose a command; Enter runs it in the panel dir",
    "  cd PATH         changes the active panel instead",
    "  Alt+Enter       insert the selected filename",
    "  Ctrl+P / Ctrl+N previous / next history entry",
    "  Ctrl+A / Ctrl+E start / end of line",
    "  Esc             clear the command line (Ctrl+U swaps panels, as in MC)",
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
    "                  (a lone Esc acts after 1 s)",
    "  F1              this help",
    "  F4              edit (built-in editor, see above)",
    "  F9              pulldown menu",
    "  F10             quit",
    "  rcmd -P FILE    write last directory to FILE on exit",
    "                  (see README for the rc() shell wrapper)",
    "",
    "# Config  (~/.config/rcmd/config.toml, saved on exit)",
    "  theme = \"mc\" | \"dark\"      keymap = \"mc\" | \"modern\" (= lynx on)",
    "  [keys] section adds custom bindings, e.g. \"ctrl+y\" = \"swap-panels\"",
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
    let [main, status, cmdline, keybar] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // MC's one-panel view: a long listing needs the whole width, so while
    // the ACTIVE side shows one, only that panel is drawn, full-width (the
    // hidden side gets a zero area so mouse hit-testing skips it). Only the
    // active side counts: an off-side long panel renders squeezed in the
    // split rather than invisibly forcing fullscreen — the state stays
    // visible and Alt+T on either side always behaves predictably.
    let qv_side = app.quick_view.as_ref().map(|q| q.side);
    let listing_long = |i: usize| {
        qv_side != Some(i) && app.info != Some(i) && app.panels[i].list_mode == ListMode::Long
    };
    let [left, right] = if listing_long(app.active) {
        let hidden = Rect::new(main.x, main.y, 0, 0);
        if app.active == 0 {
            [main, hidden]
        } else {
            [hidden, main]
        }
    } else {
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(main)
    };

    // 2 border rows + 1 column-header row.
    app.panel_rows = main.height.saturating_sub(3) as usize;
    app.areas = crate::app::Areas {
        screen: frame.area(),
        left,
        right,
        keybar,
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
                app.active == i,
                app.git_info[i].as_ref().map(|(_, s)| s),
                disk,
            );
        }
    }
    draw_status(frame, status, app);
    draw_cmdline(frame, cmdline, app);
    draw_keybar(frame, keybar);

    if let Some(menu) = &app.menu {
        draw_menu(frame, menu);
    }
    if let Some(dialog) = &app.dialog {
        match dialog {
            Dialog::Input(d) => draw_input(frame, d),
            Dialog::Confirm(d) => draw_confirm(frame, d),
            Dialog::Hotlist(selected) => {
                draw_hotlist(frame, &app.config.hotlist, &app.hotlist_recent(), *selected)
            }
            Dialog::UserMenu(selected) => draw_user_menu(frame, &app.config.commands, *selected),
            Dialog::Find(d) => draw_find(frame, d),
            Dialog::Options(d) => draw_options(frame, d),
            Dialog::RenamePreview(d) => draw_rename_preview(frame, d),
            Dialog::Jobs(selected) => draw_jobs(frame, &app.jobs, *selected),
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

fn draw_panel(
    frame: &mut Frame,
    area: Rect,
    panel: &Panel,
    state: &mut TableState,
    active: bool,
    git: Option<&GitStatus>,
    disk: Option<(u64, u64)>,
) {
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
                format!(" {frame_ch} loading — Esc cancels "),
                Style::new().fg(th().mark_fg).add_modifier(Modifier::BOLD),
            ))
            .centered(),
        );
    }

    let (labels, constraints): (&[&str], Vec<Constraint>) = match panel.list_mode {
        ListMode::Brief => (&["Name"], vec![Constraint::Fill(1)]),
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
        .style(Style::new().fg(th().panel_fg).bg(th().panel_bg))
        .block(block);

    state.select(Some(panel.cursor));
    frame.render_stateful_widget(table, area, state);
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
fn entry_row(
    entry: &Entry,
    marked: bool,
    under_cursor: bool,
    git: Option<Option<char>>,
    mode: ListMode,
    remote: bool,
) -> Row<'_> {
    let (marker, base) = match entry.kind {
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
    };
    let style = match (marked, under_cursor) {
        (true, true) => Style::new()
            .fg(th().mark_fg)
            .bg(th().select_bg)
            .add_modifier(Modifier::BOLD),
        (true, false) => Style::new().fg(th().mark_fg).add_modifier(Modifier::BOLD),
        (false, true) => Style::new().fg(th().select_fg).bg(th().select_bg),
        (false, false) => base,
    };

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
        ListMode::Brief => Row::new(vec![name_cell]),
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
            " {} job(s) running — {pct}% — C-x j lists them ",
            app.jobs.len()
        ))
        .style(Style::new().fg(th().select_fg).bg(th().select_bg))
    } else if let Some(prefix) = &app.quick_search {
        Line::from(format!("Search: {prefix}"))
            .style(Style::new().fg(th().select_fg).bg(th().select_bg))
    } else {
        match app.panels[app.active].selected() {
            Some(e) if e.is_parent() => Line::from("UP--DIR"),
            Some(e) => {
                let link = e
                    .link_target
                    .as_ref()
                    .map(|t| format!(" -> {}", t.display()))
                    .unwrap_or_default();
                Line::from(format!(
                    "{} {:>9} {}{}",
                    e.perm_string(),
                    e.size,
                    e.name.to_string_lossy(),
                    link
                ))
            }
            None => Line::default(),
        }
        .style(Style::new().fg(th().panel_fg))
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
        Line::from(format!("{:<width$}", " Help — rcmd")).style(base.add_modifier(Modifier::BOLD)),
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

/// Screen column of character `col` in `text`, with 8-wide tab stops —
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
/// (char-wise lowercase — the same approximation the search itself
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
/// dropdown rect of the open menu — shared by drawing and mouse clicks.
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

/// F9 > Options > Panel options — the MC-style checkbox form.
fn draw_options(frame: &mut Frame, d: &OptionsDialog) {
    let base = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let sel = Style::new().fg(th().select_fg).bg(th().select_bg);
    let area = centered(44, (OPTION_ROWS + 4) as u16, frame.area());
    let inner = popup(frame, area, " Panel options ", base);
    let check = |on: bool| if on { "[x]" } else { "[ ]" };
    let radio = |on: bool| if on { "(*)" } else { "( )" };
    let rows: [String; OPTION_ROWS] = [
        format!(" {} Show hidden files", check(d.show_hidden)),
        format!(" {} Lynx-like motion", check(d.lynx)),
        format!(" {} Mouse support", check(d.mouse)),
        format!(" {} Auto-reload panels", check(d.watch)),
        format!(" {} Git status", check(d.git)),
        format!(" {} Persistent subshell", check(d.subshell)),
        format!(
            " Editor  {} internal  {} external",
            radio(!d.external_editor),
            radio(d.external_editor)
        ),
        format!(
            " Theme   {} mc  {} dark",
            radio(!d.dark_theme),
            radio(d.dark_theme)
        ),
    ];
    for (i, text) in rows.iter().enumerate() {
        let row = Rect {
            x: inner.x + 1,
            y: inner.y + i as u16,
            width: inner.width.saturating_sub(2),
            height: 1,
        };
        let style = if d.cursor == i { sel } else { base };
        let width = row.width as usize;
        frame.render_widget(Line::from(format!("{text:<width$}")).style(style), row);
    }
    let buttons = Rect {
        x: inner.x,
        y: inner.y + OPTION_ROWS as u16 + 1,
        width: inner.width,
        height: 1,
    };
    let selected = if d.cursor == OPTION_ROWS {
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
        Line::from("Tab — switch   Space — toggle   Enter — search   Esc — cancel")
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
            Line::from(" empty — press 'a' to add the current directory ").centered(),
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
/// for, awaiting Yes/No — nothing has happened yet.
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
            " Bulk rename — {} rename(s), {} delete(s) ",
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
                Line::from("Enter — send   Esc — cancel")
                    .centered()
                    .style(style),
                row(3),
            );
        }
    }
}

/// The C-x j jobs list: every running job with its progress; Enter
/// pulls one to the foreground, c cancels it.
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

fn draw_job(frame: &mut Frame, job: &Job) {
    let style = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let area = centered(64, 8, frame.area());
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
    frame.render_widget(Line::from(counts), row(2));
    if job.total_bytes > 0 {
        let ratio = (job.bytes_done as f64 / job.total_bytes as f64).clamp(0.0, 1.0);
        frame.render_widget(
            Gauge::default()
                .ratio(ratio)
                .gauge_style(Style::new().fg(th().panel_bg).bg(th().dialog_bg)),
            row(3),
        );
    }
    frame.render_widget(
        Line::from("Esc — cancel   b — background")
            .centered()
            .style(style),
        row(4),
    );
}

fn draw_ask(frame: &mut Frame, ask: &Ask, button: usize) {
    let style = Style::new().fg(th().error_fg).bg(th().error_bg);
    let sel = Style::new().fg(th().dialog_fg).bg(th().dialog_bg);
    let area = centered(68, 7, frame.area());
    let width = area.width.saturating_sub(4) as usize;

    let (title, lines) = match ask {
        Ask::Overwrite { path } => (
            " File exists ",
            vec![
                tail(&path.display().to_string(), width),
                "Target already exists — overwrite?".to_string(),
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
    for (i, text) in lines.iter().enumerate() {
        let row = Rect {
            x: inner.x + 1,
            y: inner.y + i as u16,
            width: inner.width.saturating_sub(2),
            height: 1,
        };
        frame.render_widget(Line::from(text.as_str()).centered(), row);
    }
    let buttons = Rect {
        x: inner.x + 1,
        y: inner.y + 3,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    frame.render_widget(buttons_line(ask.buttons(), button, style, sel), buttons);
}
