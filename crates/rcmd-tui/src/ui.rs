use chrono::{DateTime, Local};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Gauge, Row, Table, TableState};
use ratatui::Frame;
use rcmd_core::entry::{Entry, EntryKind};
use rcmd_core::panel::Panel;

use crate::app::{App, Ask, ConfirmDialog, Dialog, InputDialog, Job, MenuState, MENUS};

const HELP_BG: Color = Color::Cyan;
const HELP_FG: Color = Color::Black;

/// Help text; lines starting with `#` render as section headers.
const HELP_TEXT: &[&str] = &[
    "",
    "# Panels",
    "  Tab             switch active panel",
    "  Up/Down, PgUp/PgDn, Home/End   move the cursor",
    "  Enter           enter directory (with empty command line)",
    "  Backspace       go to parent directory",
    "  Ctrl+R          reload both panels",
    "  Alt+.           show/hide dotfiles",
    "  Alt+N/E/S/T     sort by name/extension/size/mtime (again = reverse)",
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
    "  Esc             cancel a running operation",
    "  Overwrite prompt hotkeys: o=overwrite a=all s=skip S=skip all",
    "  Error prompt hotkeys:     r=retry s=skip S=skip all",
    "",
    "# Command line",
    "  (type)          compose a command; Enter runs it in the panel dir",
    "  cd PATH         changes the active panel instead",
    "  Alt+Enter       insert the selected filename",
    "  Ctrl+P / Ctrl+N previous / next history entry",
    "  Ctrl+A/E/U      start / end / clear line",
    "  Esc             clear the command line",
    "  Ctrl+O          open a full shell here; exit returns to rcmd",
    "",
    "# Viewer (F3)",
    "  F4              toggle hex dump",
    "  F7 or /         search (case-insensitive), n = next match",
    "  Left/Right      horizontal scroll",
    "  F3/F10/Esc/q    close the viewer",
    "",
    "# Other",
    "  F1              this help",
    "  F4              edit in $VISUAL / $EDITOR",
    "  F9              pulldown menu",
    "  F10             quit",
    "  rcmd -P FILE    write last directory to FILE on exit",
    "                  (see README for the rc() shell wrapper)",
];

pub fn help_lines() -> usize {
    HELP_TEXT.len()
}

const PANEL_BG: Color = Color::Blue;
const PANEL_FG: Color = Color::Gray;
const DIR_FG: Color = Color::White;
const EXEC_FG: Color = Color::LightGreen;
const BROKEN_FG: Color = Color::LightRed;
const HEADER_FG: Color = Color::Yellow;
const MARK_FG: Color = Color::Yellow;
const SELECT_BG: Color = Color::Cyan;
const SELECT_FG: Color = Color::Black;
const DIALOG_BG: Color = Color::Gray;
const DIALOG_FG: Color = Color::Black;
const ERROR_BG: Color = Color::Red;
const ERROR_FG: Color = Color::White;

pub fn draw(frame: &mut Frame, app: &mut App) {
    if app.help.is_some() {
        draw_help(frame, app);
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

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(main);

    // 2 border rows + 1 column-header row.
    app.panel_rows = main.height.saturating_sub(3) as usize;

    draw_panel(
        frame,
        left,
        &app.panels[0],
        &mut app.table_states[0],
        app.active == 0,
    );
    draw_panel(
        frame,
        right,
        &app.panels[1],
        &mut app.table_states[1],
        app.active == 1,
    );
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
        }
    }
    if let Some(job) = &app.job {
        draw_job(frame, job);
        if let Some(ask) = &job.ask {
            draw_ask(frame, ask, job.button);
        }
    }
}

fn draw_panel(frame: &mut Frame, area: Rect, panel: &Panel, state: &mut TableState, active: bool) {
    let title_style = if active {
        Style::new().fg(SELECT_FG).bg(SELECT_BG)
    } else {
        Style::new().fg(PANEL_FG).bg(PANEL_BG)
    };
    let mut block = Block::bordered()
        .style(Style::new().fg(PANEL_FG).bg(PANEL_BG))
        .title(Span::styled(
            format!(" {} ", panel.cwd.display()),
            title_style,
        ));
    let (marked_count, marked_bytes) = panel.marked_stats();
    if marked_count > 0 {
        block = block.title_bottom(
            Line::from(Span::styled(
                format!(" {marked_bytes} bytes in {marked_count} file(s) "),
                Style::new().fg(MARK_FG).add_modifier(Modifier::BOLD),
            ))
            .centered(),
        );
    }

    let header = Row::new([
        Cell::from(Line::from("Name").centered()),
        Cell::from(Line::from("Size").centered()),
        Cell::from(Line::from("Modify time").centered()),
    ])
    .style(Style::new().fg(HEADER_FG));

    let rows =
        panel.entries.iter().enumerate().map(|(i, entry)| {
            entry_row(entry, panel.is_marked(entry), active && i == panel.cursor)
        });

    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(7),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .column_spacing(1)
    .style(Style::new().fg(PANEL_FG).bg(PANEL_BG))
    .block(block);

    state.select(Some(panel.cursor));
    frame.render_stateful_widget(table, area, state);
}

fn entry_row(entry: &Entry, marked: bool, under_cursor: bool) -> Row<'_> {
    let (marker, base) = match entry.kind {
        EntryKind::Dir => ("/", Style::new().fg(DIR_FG).add_modifier(Modifier::BOLD)),
        EntryKind::SymlinkDir => ("~", Style::new().fg(DIR_FG).add_modifier(Modifier::BOLD)),
        EntryKind::SymlinkFile => ("@", Style::new().fg(PANEL_FG)),
        EntryKind::SymlinkBroken => ("!", Style::new().fg(BROKEN_FG)),
        EntryKind::File if entry.is_executable() => ("*", Style::new().fg(EXEC_FG)),
        EntryKind::File => (" ", Style::new().fg(PANEL_FG)),
    };
    let style = match (marked, under_cursor) {
        (true, true) => Style::new()
            .fg(MARK_FG)
            .bg(SELECT_BG)
            .add_modifier(Modifier::BOLD),
        (true, false) => Style::new().fg(MARK_FG).add_modifier(Modifier::BOLD),
        (false, true) => Style::new().fg(SELECT_FG).bg(SELECT_BG),
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

    Row::new([
        Cell::from(format!("{marker}{}", entry.name.to_string_lossy())),
        Cell::from(Line::from(size).right_aligned()),
        Cell::from(mtime),
    ])
    .style(style)
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
        Line::from(msg.as_str()).style(Style::new().fg(ERROR_FG).bg(ERROR_BG))
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
        .style(Style::new().fg(PANEL_FG))
    };
    frame.render_widget(line, area);
}

fn draw_cmdline(frame: &mut Frame, area: Rect, app: &App) {
    let prompt = tail(
        &format!("{}$ ", abbrev_home(&app.panels[app.active].cwd)),
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
            Span::styled(prompt, Style::new().fg(Color::LightCyan)),
            Span::raw(visible),
        ]),
        area,
    );
    if app.dialog.is_none() && app.job.is_none() {
        frame.set_cursor_position((area.x + (prompt_len + cl.cursor - start) as u16, area.y));
    }
}

fn abbrev_home(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rest) = path.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
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
            Style::new().fg(Color::White).bg(Color::Black),
        ));
        spans.push(Span::styled(
            format!("{label:<6}"),
            Style::new().fg(Color::Black).bg(Color::Cyan),
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

    let base = Style::new().fg(HELP_FG).bg(HELP_BG);
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
                base.fg(Color::White).add_modifier(Modifier::BOLD),
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

    let offset = if v.hex {
        v.hex_top * 16
    } else {
        v.file.offset_of_line(v.top).unwrap_or(0)
    };
    let percent = if v.file.size == 0 {
        100
    } else {
        offset * 100 / v.file.size
    };
    let title = format!(
        " {}  {} bytes  {percent}%  [{}]",
        v.path.display(),
        v.file.size,
        if v.hex { "hex" } else { "text" },
    );
    frame.render_widget(
        Line::from(format!("{:<w$}", tail(&title, width), w = width))
            .style(Style::new().fg(SELECT_FG).bg(SELECT_BG)),
        title_area,
    );

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
            match v.file.line(idx) {
                Ok(Some(text)) => {
                    let display: String = expand_line(&text)
                        .chars()
                        .skip(v.left)
                        .take(width)
                        .collect();
                    let style = if v.found == Some(idx) {
                        Style::new().fg(MARK_FG).add_modifier(Modifier::BOLD)
                    } else {
                        Style::new()
                    };
                    frame.render_widget(Line::from(display).style(style), row_area);
                }
                _ => break,
            }
        }
    }

    let help = " F3/q Quit  F4 Hex  F7|/ Search  n Next  ←→ Scroll ";
    let note = v.note.clone().unwrap_or_default();
    frame.render_widget(
        Line::from(format!(
            "{help}{:>w$}",
            note,
            w = (bottom.width as usize).saturating_sub(help.chars().count())
        ))
        .style(Style::new().fg(SELECT_FG).bg(SELECT_BG)),
        bottom,
    );

    if let Some((value, cursor)) = &v.prompt {
        let style = Style::new().fg(DIALOG_FG).bg(DIALOG_BG);
        let area = centered(50, 5, frame.area());
        let inner = popup(frame, area, " Search ", style);
        draw_field(frame, inner, value, *cursor);
    }
}

/// Expand tabs to 8-column stops and hide control characters.
fn expand_line(text: &str) -> String {
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

fn draw_menu(frame: &mut Frame, ms: &MenuState) {
    let area = frame.area();
    let base = Style::new().fg(DIALOG_FG).bg(DIALOG_BG);
    let sel = Style::new().fg(SELECT_FG).bg(SELECT_BG);

    let bar = Rect { height: 1, ..area };
    frame.render_widget(Clear, bar);
    let mut spans = Vec::new();
    let mut x_offsets = Vec::new();
    let mut x = 0u16;
    for (i, (title, _)) in MENUS.iter().enumerate() {
        let text = format!("  {title}  ");
        x_offsets.push(x);
        x += text.chars().count() as u16;
        spans.push(Span::styled(text, if i == ms.menu { sel } else { base }));
    }
    spans.push(Span::styled(
        " ".repeat((bar.width as usize).saturating_sub(x as usize)),
        base,
    ));
    frame.render_widget(Line::from(spans), bar);

    let entries = MENUS[ms.menu].1;
    let label_w = entries
        .iter()
        .flatten()
        .map(|(l, ..)| l.chars().count())
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
        x: (area.x + x_offsets[ms.menu]).min(area.width.saturating_sub(width)),
        y: area.y + 1,
        width,
        height: (entries.len() as u16 + 2).min(area.height.saturating_sub(1)),
    };
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
                let text = format!(" {label:<label_w$} {keys:>keys_w$} ");
                let style = if i == ms.item { sel } else { base };
                frame.render_widget(Line::from(text).style(style), row);
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
    let style = Style::new().fg(DIALOG_FG).bg(DIALOG_BG);
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
    let width = field.width as usize;
    let chars: Vec<char> = value.chars().collect();
    let start = cursor.saturating_sub(width.saturating_sub(1));
    let visible: String = chars[start..].iter().take(width).collect();
    frame.render_widget(
        Line::from(format!("{visible:<width$}")).style(Style::new().fg(SELECT_FG).bg(SELECT_BG)),
        field,
    );
    frame.set_cursor_position((field.x + (cursor - start) as u16, field.y));
}

fn draw_confirm(frame: &mut Frame, d: &ConfirmDialog) {
    let style = Style::new().fg(ERROR_FG).bg(ERROR_BG);
    let sel = Style::new().fg(DIALOG_FG).bg(DIALOG_BG);
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

fn draw_job(frame: &mut Frame, job: &Job) {
    let style = Style::new().fg(DIALOG_FG).bg(DIALOG_BG);
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
                .gauge_style(Style::new().fg(PANEL_BG).bg(DIALOG_BG)),
            row(3),
        );
    }
    frame.render_widget(Line::from("Esc — cancel").centered().style(style), row(4));
}

fn draw_ask(frame: &mut Frame, ask: &Ask, button: usize) {
    let style = Style::new().fg(ERROR_FG).bg(ERROR_BG);
    let sel = Style::new().fg(DIALOG_FG).bg(DIALOG_BG);
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
